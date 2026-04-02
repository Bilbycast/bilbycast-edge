// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Relay client — manages the QUIC connection to a bilbycast-relay server.
//!
//! The relay is stateless — no authentication needed. The edge connects and
//! immediately binds tunnels by UUID. All tunnel data is encrypted end-to-end
//! between edge nodes; the relay only sees ciphertext.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use quinn::Connection;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::config::{TunnelDirection, TunnelProtocol};
use super::protocol::{
    self, EdgeMessage, ParsedMessage, RelayDirection, RelayMessage, RelayProtocol, ALPN_RELAY,
    TUNNEL_PROTOCOL_VERSION,
};
use super::quic;
use crate::manager::events::{EventSender, EventSeverity};

/// State of the relay tunnel connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayTunnelState {
    Connecting,
    Waiting,
    Ready,
    Down,
}

impl std::fmt::Display for RelayTunnelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connecting => write!(f, "connecting"),
            Self::Waiting => write!(f, "waiting"),
            Self::Ready => write!(f, "ready"),
            Self::Down => write!(f, "down"),
        }
    }
}

/// Parameters for establishing a relay tunnel.
pub struct RelayTunnelParams {
    pub tunnel_id: Uuid,
    pub relay_addr: SocketAddr,
    pub direction: TunnelDirection,
    pub protocol: TunnelProtocol,
    /// Optional manager node_id to identify this edge to the relay.
    pub edge_id: Option<String>,
    /// Optional bind secret for relay tunnel authentication.
    pub tunnel_bind_secret: Option<String>,
}

/// Establish a QUIC connection to the relay and bind the tunnel.
///
/// No authentication step — the relay is stateless. The edge connects and
/// immediately sends a TunnelBind message.
///
/// Returns the QUIC connection ready for data forwarding once state becomes `Ready`.
pub async fn connect_and_bind(
    params: &RelayTunnelParams,
    state_tx: &watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    event_sender: EventSender,
) -> Result<Connection> {
    // Connect to relay
    state_tx.send_replace(RelayTunnelState::Connecting);
    let endpoint = quic::make_client_endpoint(ALPN_RELAY)?;

    let conn = tokio::select! {
        result = quic::connect(&endpoint, params.relay_addr, "relay") => result?,
        _ = cancel.cancelled() => anyhow::bail!("cancelled during connect"),
    };

    tracing::info!(
        tunnel_id = %params.tunnel_id,
        relay = %params.relay_addr,
        "Connected to relay"
    );

    // Open control stream (bidirectional stream 0)
    let (mut send, mut recv) = conn.open_bi().await?;

    // Send protocol version handshake (new relays respond with HelloAck; old relays ignore it)
    protocol::write_message(
        &mut send,
        &EdgeMessage::Hello {
            protocol_version: TUNNEL_PROTOCOL_VERSION,
            software_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )
    .await?;

    // Identify this edge to the relay (enables topology visualization in the manager)
    if let Some(ref id) = params.edge_id {
        protocol::write_message(&mut send, &EdgeMessage::Identify { edge_id: id.clone() }).await?;
    }

    // Bind tunnel (with optional bind token for relay authentication)
    let relay_direction = match params.direction {
        TunnelDirection::Ingress => RelayDirection::Ingress,
        TunnelDirection::Egress => RelayDirection::Egress,
    };
    let relay_protocol = match params.protocol {
        TunnelProtocol::Tcp => RelayProtocol::Tcp,
        TunnelProtocol::Udp => RelayProtocol::Udp,
    };

    // Compute bind token if bind secret is available
    let bind_token = params.tunnel_bind_secret.as_deref().map(|secret| {
        let direction_str = match params.direction {
            TunnelDirection::Ingress => "ingress",
            TunnelDirection::Egress => "egress",
        };
        super::auth::compute_bind_token(&params.tunnel_id.to_string(), direction_str, secret)
    });

    protocol::write_message(
        &mut send,
        &EdgeMessage::TunnelBind {
            tunnel_id: params.tunnel_id,
            direction: relay_direction,
            protocol: relay_protocol,
            bind_token,
        },
    )
    .await?;

    // Wait for tunnel to become ready.
    // TunnelReady may arrive either on the control bidi stream (if we are the
    // second edge to bind) or via a uni-stream notification (if we are the
    // first edge to bind and the peer binds later).
    loop {
        tokio::select! {
            result = protocol::read_message_resilient::<RelayMessage>(&mut recv) => {
                let msg = match result? {
                    ParsedMessage::Known(msg) => msg,
                    ParsedMessage::Unknown { msg_type } => {
                        tracing::debug!(
                            tunnel_id = %params.tunnel_id,
                            "Unknown relay message type '{msg_type}' while waiting, ignoring"
                        );
                        continue;
                    }
                };
                match msg {
                    RelayMessage::TunnelReady { tunnel_id } if tunnel_id == params.tunnel_id => {
                        state_tx.send_replace(RelayTunnelState::Ready);
                        tracing::info!(
                            tunnel_id = %params.tunnel_id,
                            direction = %params.direction,
                            "Tunnel ready"
                        );
                        event_sender.emit_flow(
                            EventSeverity::Info,
                            "tunnel",
                            "Tunnel connected to relay",
                            &params.tunnel_id.to_string(),
                        );
                        break;
                    }
                    RelayMessage::TunnelWaiting { tunnel_id } if tunnel_id == params.tunnel_id => {
                        state_tx.send_replace(RelayTunnelState::Waiting);
                        tracing::info!(
                            tunnel_id = %params.tunnel_id,
                            "Waiting for peer to bind"
                        );
                    }
                    RelayMessage::HelloAck { protocol_version, software_version } => {
                        tracing::info!(
                            tunnel_id = %params.tunnel_id,
                            "Relay hello_ack: protocol v{protocol_version}, software {software_version}"
                        );
                        if protocol_version != TUNNEL_PROTOCOL_VERSION {
                            tracing::warn!(
                                "Tunnel protocol version mismatch: edge={TUNNEL_PROTOCOL_VERSION}, relay={protocol_version}"
                            );
                        }
                    }
                    RelayMessage::Pong => {}
                    other => {
                        tracing::debug!(
                            tunnel_id = %params.tunnel_id,
                            "Relay message while waiting: {other:?}"
                        );
                    }
                }
            }
            // Accept uni-stream notifications from relay (used when we were
            // the first edge to bind and the peer joins later)
            result = conn.accept_uni() => {
                if let Ok(mut uni_recv) = result {
                    if let Ok(ParsedMessage::Known(msg)) = protocol::read_message_resilient::<RelayMessage>(&mut uni_recv).await {
                        if let RelayMessage::TunnelReady { tunnel_id } = msg {
                            if tunnel_id == params.tunnel_id {
                                state_tx.send_replace(RelayTunnelState::Ready);
                                tracing::info!(
                                    tunnel_id = %params.tunnel_id,
                                    direction = %params.direction,
                                    "Tunnel ready (via notification)"
                                );
                                event_sender.emit_flow(
                                    EventSeverity::Info,
                                    "tunnel",
                                    "Tunnel connected to relay",
                                    &params.tunnel_id.to_string(),
                                );
                                break;
                            }
                        }
                    }
                }
            }
            _ = cancel.cancelled() => anyhow::bail!("cancelled waiting for tunnel ready"),
        }
    }

    // Spawn keepalive task on the control stream
    let cancel_keepalive = cancel.clone();
    let keepalive_send = Arc::new(tokio::sync::Mutex::new(send));
    let ka_send = keepalive_send.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let mut s = ka_send.lock().await;
                    if protocol::write_message(&mut *s, &EdgeMessage::Ping).await.is_err() {
                        break;
                    }
                }
                _ = cancel_keepalive.cancelled() => break,
            }
        }
    });

    // Spawn control stream reader (handles pong, tunnel_down)
    let state_tx_clone = state_tx.clone();
    let tunnel_id = params.tunnel_id;
    let tunnel_id_str = params.tunnel_id.to_string();
    let cancel_reader = cancel.clone();
    let reader_event_sender = event_sender.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = protocol::read_message_resilient::<RelayMessage>(&mut recv) => {
                    match result {
                        Ok(ParsedMessage::Known(RelayMessage::Pong | RelayMessage::HelloAck { .. })) => {}
                        Ok(ParsedMessage::Known(RelayMessage::TunnelDown { tunnel_id: tid, reason })) if tid == tunnel_id => {
                            tracing::warn!(tunnel_id = %tid, reason = %reason, "Tunnel down");
                            reader_event_sender.emit_flow(
                                EventSeverity::Warning,
                                "tunnel",
                                format!("Tunnel peer disconnected: {reason}"),
                                &tunnel_id_str,
                            );
                            state_tx_clone.send_replace(RelayTunnelState::Down);
                            break;
                        }
                        Ok(ParsedMessage::Known(msg)) => {
                            tracing::debug!("Relay control message: {msg:?}");
                        }
                        Ok(ParsedMessage::Unknown { msg_type }) => {
                            tracing::debug!("Unknown relay message type '{msg_type}', ignoring");
                        }
                        Err(e) => {
                            tracing::warn!("Relay control stream error: {e}");
                            reader_event_sender.emit_flow(
                                EventSeverity::Warning,
                                "tunnel",
                                format!("Tunnel disconnected from relay: {e}"),
                                &tunnel_id_str,
                            );
                            state_tx_clone.send_replace(RelayTunnelState::Down);
                            break;
                        }
                    }
                }
                _ = cancel_reader.cancelled() => break,
            }
        }
    });

    Ok(conn)
}

/// Connect to relay with exponential backoff retry.
pub async fn connect_with_retry(
    params: &RelayTunnelParams,
    state_tx: &watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    event_sender: EventSender,
) -> Result<Connection> {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);

    loop {
        match connect_and_bind(params, state_tx, cancel.clone(), event_sender.clone()).await {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                if cancel.is_cancelled() {
                    return Err(e);
                }
                let err_msg = e.to_string();
                // Detect bind rejection
                if err_msg.contains("bind_rejected") || err_msg.contains("BindRejected") {
                    event_sender.emit_flow(
                        EventSeverity::Critical,
                        "tunnel",
                        format!("Tunnel bind rejected by relay: {e}"),
                        &params.tunnel_id.to_string(),
                    );
                } else {
                    event_sender.emit_flow(
                        EventSeverity::Warning,
                        "tunnel",
                        format!("Tunnel connection to relay failed: {e}"),
                        &params.tunnel_id.to_string(),
                    );
                }
                tracing::warn!(
                    tunnel_id = %params.tunnel_id,
                    "Relay connection failed: {e}, retrying in {:?}",
                    backoff
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = cancel.cancelled() => anyhow::bail!("cancelled during retry backoff"),
                }
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}
