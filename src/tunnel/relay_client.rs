// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Relay client — manages the QUIC connection to a bilbycast-relay server.
//!
//! The relay is stateless — no authentication needed. The edge connects and
//! immediately binds tunnels by UUID. All tunnel data is encrypted end-to-end
//! between edge nodes; the relay only sees ciphertext.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::manager::events::{EventSender, EventSeverity, category};

/// How long we tolerate a `Waiting` state (peer not yet bound) before stepping
/// to the next relay. This is the convergence mechanism when each edge has
/// landed on a different relay.
const WAITING_TIMEOUT: Duration = Duration::from_secs(10);

/// How long we wait for the QUIC handshake to a relay to complete before
/// stepping to the next relay. Bounded so a dead primary cannot stall failover
/// behind Quinn's default idle timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// Marker substring used to distinguish a waiting-timeout failure from a real
/// connect/bind error. The failover loop checks for this to avoid logging a
/// hard error when we're simply trying to converge on the same relay as the peer.
const WAITING_TIMEOUT_MSG: &str = "waiting-timeout-stepping-forward";

/// How often the failback probe task attempts to measure the primary relay.
pub const FAILBACK_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Timeout for a single RTT probe against a relay address.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
///
/// `relay_addrs` is an ordered list (primary first, optional backup second).
/// The failover loop in [`connect_with_retry`] walks this list and publishes
/// the index of the currently-active relay on `active_idx_tx`.
pub struct RelayTunnelParams {
    pub tunnel_id: Uuid,
    /// Ordered list of relay addresses. Must contain at least one entry.
    /// A second entry enables automatic primary↔backup failover.
    pub relay_addrs: Vec<SocketAddr>,
    pub direction: TunnelDirection,
    pub protocol: TunnelProtocol,
    /// Optional manager node_id to identify this edge to the relay.
    pub edge_id: Option<String>,
    /// Optional bind secret for relay tunnel authentication.
    pub tunnel_bind_secret: Option<String>,
    /// Maximum RTT increase (ms) tolerated when failing back from backup to
    /// primary. Consumed by the failback probe task.
    pub failback_rtt_gate_ms: u32,
}

/// Establish a QUIC connection to the specified relay address and bind the
/// tunnel.
///
/// `relay_addr` is the address currently being attempted — chosen by the
/// caller from `params.relay_addrs`. If the peer has bound a different relay
/// and we get stuck in `Waiting` for more than [`WAITING_TIMEOUT`], this
/// function bails with an error containing [`WAITING_TIMEOUT_MSG`] so the
/// failover loop can step to the next relay without treating it as a hard
/// failure.
///
/// Returns the QUIC connection ready for data forwarding once state becomes `Ready`.
pub async fn connect_and_bind(
    params: &RelayTunnelParams,
    relay_addr: SocketAddr,
    state_tx: &watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    event_sender: EventSender,
) -> Result<Connection> {
    // Connect to relay
    state_tx.send_replace(RelayTunnelState::Connecting);
    let endpoint = quic::make_client_endpoint(ALPN_RELAY)?;

    let conn = tokio::select! {
        result = tokio::time::timeout(
            CONNECT_TIMEOUT,
            quic::connect(&endpoint, relay_addr, "relay"),
        ) => result
            .map_err(|_| anyhow::anyhow!("connect to relay {relay_addr} timed out after {CONNECT_TIMEOUT:?}"))??,
        _ = cancel.cancelled() => anyhow::bail!("cancelled during connect"),
    };

    tracing::info!(
        tunnel_id = %params.tunnel_id,
        relay = %relay_addr,
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
    //
    // While in the `Waiting` state (peer not yet bound), we start a deadline
    // timer. If we remain in `Waiting` for longer than `WAITING_TIMEOUT`, we
    // bail so the caller can step to the next relay — this is how two edges
    // that independently landed on different relays converge.
    let mut waiting_since: Option<Instant> = None;
    loop {
        // Compute how long until the waiting deadline fires (if any). Default
        // to a long idle interval so the select! future is well-formed.
        let timeout = match waiting_since {
            Some(since) => WAITING_TIMEOUT
                .checked_sub(since.elapsed())
                .unwrap_or(Duration::from_millis(0)),
            None => Duration::from_secs(3600),
        };

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
                        event_sender.emit_flow_with_details(
                            EventSeverity::Info,
                            category::TUNNEL,
                            "Tunnel connected to relay",
                            &params.tunnel_id.to_string(),
                            serde_json::json!({ "relay_addr": relay_addr.to_string() }),
                        );
                        break;
                    }
                    RelayMessage::TunnelWaiting { tunnel_id } if tunnel_id == params.tunnel_id => {
                        state_tx.send_replace(RelayTunnelState::Waiting);
                        if waiting_since.is_none() {
                            waiting_since = Some(Instant::now());
                        }
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
                if let Ok(mut uni_recv) = result
                    && let Ok(ParsedMessage::Known(RelayMessage::TunnelReady { tunnel_id })) =
                        protocol::read_message_resilient::<RelayMessage>(&mut uni_recv).await
                    && tunnel_id == params.tunnel_id
                {
                    state_tx.send_replace(RelayTunnelState::Ready);
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        direction = %params.direction,
                        "Tunnel ready (via notification)"
                    );
                    event_sender.emit_flow_with_details(
                        EventSeverity::Info,
                        category::TUNNEL,
                        "Tunnel connected to relay",
                        &params.tunnel_id.to_string(),
                        serde_json::json!({ "relay_addr": relay_addr.to_string() }),
                    );
                    break;
                }
            }
            _ = tokio::time::sleep(timeout), if waiting_since.is_some() => {
                // Peer hasn't bound on this relay within the window. Unbind
                // politely so the relay drops its pending state, then bail.
                let _ = protocol::write_message(
                    &mut send,
                    &EdgeMessage::TunnelUnbind { tunnel_id: params.tunnel_id },
                ).await;
                tracing::info!(
                    tunnel_id = %params.tunnel_id,
                    relay = %relay_addr,
                    "Peer hasn't bound on this relay within {WAITING_TIMEOUT:?}, stepping to next relay"
                );
                anyhow::bail!("{WAITING_TIMEOUT_MSG}: peer not bound on {relay_addr}");
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
                            reader_event_sender.emit_flow_with_details(
                                EventSeverity::Warning,
                                category::TUNNEL,
                                format!("Tunnel peer disconnected: {reason}"),
                                &tunnel_id_str,
                                serde_json::json!({ "reason": reason }),
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
                            reader_event_sender.emit_flow_with_details(
                                EventSeverity::Warning,
                                category::TUNNEL,
                                format!("Tunnel disconnected from relay: {e}"),
                                &tunnel_id_str,
                                serde_json::json!({ "reason": e.to_string() }),
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

/// Connect to a relay with ordered failover + exponential backoff.
///
/// Walks `params.relay_addrs` starting from `*active_idx`, stepping forward on
/// each failure. After trying every relay in the list once (a full cycle), it
/// sleeps with exponential backoff before retrying.
///
/// `active_idx` is shared mutable state owned by the caller so that reconnect
/// cycles are sticky: when the forwarder drops and we reconnect, we resume on
/// the same relay we were using (and only step forward on sustained failure).
///
/// `active_idx_tx` publishes the currently-active relay index to observers
/// (stats snapshot, failback probe).
pub async fn connect_with_retry(
    params: &RelayTunnelParams,
    active_idx: &mut usize,
    active_idx_tx: &watch::Sender<usize>,
    state_tx: &watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    event_sender: EventSender,
) -> Result<Connection> {
    if params.relay_addrs.is_empty() {
        anyhow::bail!("relay_addrs is empty — at least one relay is required");
    }

    let n = params.relay_addrs.len();
    if *active_idx >= n {
        *active_idx = 0;
    }
    let initial_idx = *active_idx;

    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);

    loop {
        let relay_addr = params.relay_addrs[*active_idx];
        match connect_and_bind(params, relay_addr, state_tx, cancel.clone(), event_sender.clone())
            .await
        {
            Ok(conn) => {
                active_idx_tx.send_replace(*active_idx);
                if *active_idx != initial_idx {
                    let from = params.relay_addrs[initial_idx];
                    tracing::warn!(
                        tunnel_id = %params.tunnel_id,
                        "Tunnel failed over from {from} to {relay_addr} (idx {initial_idx} → {})",
                        *active_idx
                    );
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning,
                        category::TUNNEL,
                        format!("Tunnel failed over to backup relay {relay_addr}"),
                        &params.tunnel_id.to_string(),
                        serde_json::json!({
                            "from_relay_addr": from.to_string(),
                            "to_relay_addr": relay_addr.to_string(),
                            "from_idx": initial_idx,
                            "to_idx": *active_idx,
                        }),
                    );
                }
                return Ok(conn);
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    return Err(e);
                }
                let err_msg = e.to_string();
                let is_waiting_timeout = err_msg.contains(WAITING_TIMEOUT_MSG);
                let is_bind_rejected =
                    err_msg.contains("bind_rejected") || err_msg.contains("BindRejected");

                if is_waiting_timeout {
                    // Expected convergence step, not a hard failure.
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        relay = %relay_addr,
                        "Stepping to next relay (waiting timeout)"
                    );
                } else if is_bind_rejected {
                    event_sender.emit_flow_with_details(
                        EventSeverity::Critical,
                        category::TUNNEL,
                        format!("Tunnel bind rejected by relay: {e}"),
                        &params.tunnel_id.to_string(),
                        serde_json::json!({ "relay_addr": relay_addr.to_string() }),
                    );
                } else {
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning,
                        category::TUNNEL,
                        format!("Tunnel connection to relay failed: {e}"),
                        &params.tunnel_id.to_string(),
                        serde_json::json!({ "relay_addr": relay_addr.to_string() }),
                    );
                    tracing::warn!(
                        tunnel_id = %params.tunnel_id,
                        relay = %relay_addr,
                        "Relay connection failed: {e}"
                    );
                }

                // Step to the next relay in the list. If that brings us back
                // to `initial_idx` we've tried every relay once this cycle —
                // sleep with exponential backoff before the next round.
                let next = (*active_idx + 1) % n;
                *active_idx = next;
                active_idx_tx.send_replace(*active_idx);

                if next == initial_idx {
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        "All {n} relays failed this cycle, sleeping {backoff:?} before retry"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => anyhow::bail!("cancelled during retry backoff"),
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }
}

/// Measure the smoothed QUIC RTT to `relay_addr` by opening a short-lived
/// probe connection and performing the Hello / HelloAck handshake.
///
/// Used by the failback probe to decide whether the primary relay is healthy
/// enough (RTT-wise) to resume traffic on. The connection is dropped at the
/// end of the call.
pub async fn measure_relay_rtt(
    relay_addr: SocketAddr,
    cancel: CancellationToken,
) -> Result<Duration> {
    let work = async move {
        let endpoint = quic::make_client_endpoint(ALPN_RELAY)?;
        let conn = tokio::select! {
            result = quic::connect(&endpoint, relay_addr, "relay") => result?,
            _ = cancel.cancelled() => anyhow::bail!("cancelled during probe connect"),
        };
        let (mut send, mut recv) = conn.open_bi().await?;
        protocol::write_message(
            &mut send,
            &EdgeMessage::Hello {
                protocol_version: TUNNEL_PROTOCOL_VERSION,
                software_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await?;
        // Wait for HelloAck so we know the relay is responsive at the
        // application layer (not just the QUIC handshake).
        let _ = protocol::read_message_resilient::<RelayMessage>(&mut recv).await?;
        Ok::<Duration, anyhow::Error>(conn.rtt())
    };

    match tokio::time::timeout(PROBE_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("probe to {relay_addr} timed out after {PROBE_TIMEOUT:?}"),
    }
}
