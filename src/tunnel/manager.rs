// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Tunnel lifecycle manager.
//!
//! Manages the creation, teardown, and status tracking of IP tunnels.
//! Each tunnel runs as a set of tokio tasks coordinated by a CancellationToken.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde::Serialize;
use std::sync::OnceLock;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::config::{TunnelConfig, TunnelDirection, TunnelMode, TunnelProtocol};
use super::relay_client::{self, RelayTunnelParams, RelayTunnelState};
use super::tcp_forwarder::{self, TcpForwarderStats};
use super::udp_forwarder::{self, UdpForwarderStats};
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::throughput::ThroughputEstimator;

/// Runtime state for an active tunnel.
struct TunnelRuntime {
    config: TunnelConfig,
    cancel: CancellationToken,
    state_rx: watch::Receiver<RelayTunnelState>,
    udp_stats: Option<Arc<UdpForwarderStats>>,
    tcp_stats: Option<Arc<TcpForwarderStats>>,
    throughput_in: ThroughputEstimator,
    throughput_out: ThroughputEstimator,
}

/// Serializable tunnel status for API responses.
#[derive(Debug, Clone, Serialize)]
pub struct TunnelStatus {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub mode: String,
    pub direction: String,
    pub local_addr: String,
    pub state: String,
    pub stats: TunnelStatsSnapshot,
}

/// Serializable tunnel stats for API responses.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TunnelStatsSnapshot {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub bitrate_in_bps: u64,
    pub bitrate_out_bps: u64,
    pub send_errors: u64,
    pub connections_total: u64,
    pub connections_active: u64,
}

/// Manages all active tunnels on this edge node.
pub struct TunnelManager {
    tunnels: Arc<DashMap<String, TunnelRuntime>>,
    /// Manager node_id, used to identify this edge to relay nodes.
    /// Set once during manager registration/auth.
    manager_node_id: OnceLock<String>,
    /// Event sender for forwarding operational events to the manager.
    event_sender: EventSender,
}

impl TunnelManager {
    pub fn new(event_sender: EventSender) -> Self {
        Self {
            tunnels: Arc::new(DashMap::new()),
            manager_node_id: OnceLock::new(),
            event_sender,
        }
    }

    /// Get a reference to the event sender.
    pub fn event_sender(&self) -> &EventSender {
        &self.event_sender
    }

    /// Set the manager node_id (called after manager registration/auth).
    /// First call wins — subsequent calls with a different value are ignored
    /// (OnceLock semantics).
    pub fn set_manager_node_id(&self, id: String) {
        let _ = self.manager_node_id.set(id);
    }

    /// Create and start a tunnel from the given configuration.
    ///
    /// Upsert semantics: if a tunnel with the same ID already exists and its
    /// config is identical, this is a no-op.  If the config differs the old
    /// tunnel is destroyed first and a new one is created.
    pub async fn create_tunnel(&self, config: TunnelConfig) -> anyhow::Result<()> {
        if let Some(existing) = self.tunnels.get(&config.id) {
            if existing.config == config {
                tracing::info!(tunnel_id = %config.id, "Tunnel already exists with identical config, skipping");
                return Ok(());
            }
            drop(existing);
            tracing::info!(tunnel_id = %config.id, "Tunnel exists with different config, replacing");
            self.destroy_tunnel(&config.id).await?;
        }

        let tunnel_id = Uuid::parse_str(&config.id)
            .map_err(|e| anyhow::anyhow!("Tunnel '{}': id is not a valid UUID: {e}", config.id))?;

        let cancel = CancellationToken::new();
        let (state_tx, state_rx) = watch::channel(RelayTunnelState::Connecting);

        // Create shared stats objects that are used by both the runtime and the forwarder tasks
        let udp_stats = if config.protocol == TunnelProtocol::Udp {
            Some(Arc::new(UdpForwarderStats::default()))
        } else {
            None
        };
        let tcp_stats = if config.protocol == TunnelProtocol::Tcp {
            Some(Arc::new(TcpForwarderStats::default()))
        } else {
            None
        };

        match config.mode {
            TunnelMode::Relay => {
                self.start_relay_tunnel(
                    &config, tunnel_id, cancel.clone(), state_tx,
                    udp_stats.clone(), tcp_stats.clone(),
                ).await?;
            }
            TunnelMode::Direct => {
                self.start_direct_tunnel(
                    &config, tunnel_id, cancel.clone(), state_tx,
                    udp_stats.clone(), tcp_stats.clone(),
                ).await?;
            }
        }

        let tunnel_name = config.name.clone();
        let tunnel_id_str = config.id.clone();
        self.tunnels.insert(
            config.id.clone(),
            TunnelRuntime {
                config,
                cancel,
                state_rx,
                udp_stats,
                tcp_stats,
                throughput_in: ThroughputEstimator::new(),
                throughput_out: ThroughputEstimator::new(),
            },
        );

        self.event_sender.emit_flow(
            EventSeverity::Info,
            "tunnel",
            format!("Tunnel '{}' started", tunnel_name),
            &tunnel_id_str,
        );

        Ok(())
    }

    async fn start_relay_tunnel(
        &self,
        config: &TunnelConfig,
        tunnel_id: Uuid,
        cancel: CancellationToken,
        state_tx: watch::Sender<RelayTunnelState>,
        udp_stats: Option<Arc<UdpForwarderStats>>,
        tcp_stats: Option<Arc<TcpForwarderStats>>,
    ) -> anyhow::Result<()> {
        let relay_addr: SocketAddr = config
            .relay_addr
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("relay_addr required for relay mode"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid relay_addr: {e}"))?;

        let local_addr: SocketAddr = config
            .local_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid local_addr: {e}"))?;

        let edge_id = self.manager_node_id.get().cloned();
        let params = RelayTunnelParams {
            tunnel_id,
            relay_addr,
            direction: config.direction,
            protocol: config.protocol,
            edge_id,
            tunnel_bind_secret: config.tunnel_bind_secret.clone(),
        };

        // Create cipher from encryption key
        let cipher = if let Some(ref key) = config.tunnel_encryption_key {
            Some(Arc::new(
                super::crypto::TunnelCipher::new(key)
                    .map_err(|e| anyhow::anyhow!("Failed to create tunnel cipher: {e}"))?,
            ))
        } else {
            None
        };

        let direction = config.direction;
        let protocol = config.protocol;
        let config_id = config.id.clone();
        let config_name = config.name.clone();
        let tunnels = self.tunnels.clone();
        let event_sender = self.event_sender.clone();

        // Spawn the tunnel lifecycle task
        tokio::spawn(async move {
            let result = run_relay_tunnel(
                params,
                direction,
                protocol,
                local_addr,
                state_tx,
                cancel.clone(),
                udp_stats,
                tcp_stats,
                cipher,
                event_sender.clone(),
            )
            .await;

            if let Err(e) = result {
                if !cancel.is_cancelled() {
                    tracing::error!(tunnel_id = %config_id, "Relay tunnel failed: {e}");
                    event_sender.emit_flow(
                        EventSeverity::Critical,
                        "tunnel",
                        format!("Tunnel '{}' failed: {e}", config_name),
                        &config_id,
                    );
                    // Remove from DashMap so manager re-push can re-create it
                    tunnels.remove(&config_id);
                }
            }
        });

        Ok(())
    }

    async fn start_direct_tunnel(
        &self,
        config: &TunnelConfig,
        tunnel_id: Uuid,
        cancel: CancellationToken,
        state_tx: watch::Sender<RelayTunnelState>,
        udp_stats: Option<Arc<UdpForwarderStats>>,
        tcp_stats: Option<Arc<TcpForwarderStats>>,
    ) -> anyhow::Result<()> {
        let local_addr: SocketAddr = config
            .local_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid local_addr: {e}"))?;

        let tunnel_psk = config.tunnel_psk.clone()
            .ok_or_else(|| anyhow::anyhow!("tunnel_psk required for direct mode"))?;

        let direction = config.direction;
        let protocol = config.protocol;
        let config_clone = config.clone();
        let config_id = config.id.clone();
        let config_name = config.name.clone();
        let tunnels = self.tunnels.clone();
        let event_sender = self.event_sender.clone();

        tokio::spawn(async move {
            // Retry the full direct-tunnel lifecycle (connect/accept + auth +
            // forwarder) with exponential backoff. Without this, any transient
            // failure — peer not yet reachable, QUIC timeout, forwarder drop —
            // would strand the tunnel permanently until the manager re-pushes
            // it. For standalone edges (no manager) that is never.
            let mut attempt: u32 = 0;
            let max_delay = std::time::Duration::from_secs(30);
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                let result = run_direct_tunnel(
                    config_clone.clone(),
                    tunnel_id,
                    direction,
                    protocol,
                    local_addr,
                    tunnel_psk.clone(),
                    state_tx.clone(),
                    cancel.clone(),
                    udp_stats.clone(),
                    tcp_stats.clone(),
                )
                .await;

                if cancel.is_cancelled() {
                    return;
                }
                match result {
                    Ok(()) => {
                        // Forwarder exited cleanly (peer closed the tunnel);
                        // reset backoff and loop to re-establish.
                        attempt = 0;
                    }
                    Err(e) => {
                        tracing::warn!(
                            tunnel_id = %tunnel_id,
                            "Direct tunnel attempt {} failed: {e}",
                            attempt + 1
                        );
                        event_sender.emit_flow(
                            EventSeverity::Warning,
                            "tunnel",
                            format!("Tunnel '{}' attempt {} failed: {e}", config_name, attempt + 1),
                            &config_id,
                        );
                        attempt = attempt.saturating_add(1);
                    }
                }

                let delay = std::cmp::min(
                    std::time::Duration::from_millis(500 * 2u64.pow(attempt.min(6))),
                    max_delay,
                );
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            // Unreachable: loop exits only via cancel.
            #[allow(unreachable_code)]
            {
                let _ = tunnels;
            }
        });

        Ok(())
    }

    /// Destroy (stop) a tunnel by ID.
    pub async fn destroy_tunnel(&self, id: &str) -> anyhow::Result<()> {
        let (_, runtime) = self
            .tunnels
            .remove(id)
            .ok_or_else(|| anyhow::anyhow!("Tunnel '{id}' not found"))?;

        let tunnel_name = runtime.config.name.clone();
        runtime.cancel.cancel();
        tracing::info!(tunnel_id = %id, "Tunnel destroyed");

        self.event_sender.emit_flow(
            EventSeverity::Info,
            "tunnel",
            format!("Tunnel '{}' stopped", tunnel_name),
            id,
        );

        Ok(())
    }

    /// Get the status of a specific tunnel.
    pub fn tunnel_status(&self, id: &str) -> Option<TunnelStatus> {
        self.tunnels.get(id).map(|entry| build_status(&entry))
    }

    /// List all active tunnels.
    pub fn list_tunnels(&self) -> Vec<TunnelStatus> {
        self.tunnels
            .iter()
            .map(|entry| build_status(&entry))
            .collect()
    }

    /// Stop all tunnels (called during graceful shutdown).
    pub async fn stop_all(&self) {
        for entry in self.tunnels.iter() {
            entry.cancel.cancel();
        }
        self.tunnels.clear();
        tracing::info!("All tunnels stopped");
    }
}

fn build_status(runtime: &TunnelRuntime) -> TunnelStatus {
    let state = *runtime.state_rx.borrow();

    let mut stats = TunnelStatsSnapshot::default();
    if let Some(ref udp) = runtime.udp_stats {
        stats.packets_sent = udp.packets_sent.load(Ordering::Relaxed);
        stats.packets_received = udp.packets_received.load(Ordering::Relaxed);
        stats.bytes_sent = udp.bytes_sent.load(Ordering::Relaxed);
        stats.bytes_received = udp.bytes_received.load(Ordering::Relaxed);
        stats.send_errors = udp.send_errors.load(Ordering::Relaxed);
    }
    if let Some(ref tcp) = runtime.tcp_stats {
        stats.bytes_sent = tcp.bytes_sent.load(Ordering::Relaxed);
        stats.bytes_received = tcp.bytes_received.load(Ordering::Relaxed);
        stats.connections_total = tcp.connections_total.load(Ordering::Relaxed);
        stats.connections_active = tcp.connections_active.load(Ordering::Relaxed);
    }

    // Sample throughput estimators to compute bitrate
    stats.bitrate_in_bps = runtime.throughput_in.sample(stats.bytes_received);
    stats.bitrate_out_bps = runtime.throughput_out.sample(stats.bytes_sent);

    TunnelStatus {
        id: runtime.config.id.clone(),
        name: runtime.config.name.clone(),
        protocol: runtime.config.protocol.to_string(),
        mode: runtime.config.mode.to_string(),
        direction: runtime.config.direction.to_string(),
        local_addr: runtime.config.local_addr.clone(),
        state: state.to_string(),
        stats,
    }
}

/// Main lifecycle for a relay-mode tunnel.
///
/// Wraps the connect+forward cycle in a reconnection loop so the tunnel
/// automatically recovers when the relay restarts or the QUIC connection drops.
async fn run_relay_tunnel(
    params: RelayTunnelParams,
    direction: TunnelDirection,
    protocol: TunnelProtocol,
    local_addr: SocketAddr,
    state_tx: watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    udp_stats: Option<Arc<UdpForwarderStats>>,
    tcp_stats: Option<Arc<TcpForwarderStats>>,
    cipher: Option<Arc<super::crypto::TunnelCipher>>,
    event_sender: EventSender,
) -> anyhow::Result<()> {
    let tunnel_id = params.tunnel_id;
    let tunnel_id_str = tunnel_id.to_string();
    let reconnect_delay = Duration::from_secs(1);

    loop {
        // Connect with retry (handles its own internal backoff for initial connect)
        let conn = relay_client::connect_with_retry(&params, &state_tx, cancel.clone(), event_sender.clone()).await?;

        tracing::info!(
            tunnel_id = %tunnel_id,
            direction = %direction,
            protocol = %protocol,
            encrypted = cipher.is_some(),
            "Relay tunnel connected, starting forwarder"
        );

        // Run the forwarder — blocks until the QUIC connection drops
        let result = run_forwarder(
            tunnel_id, protocol, direction, local_addr, conn, cancel.clone(),
            udp_stats.clone(), tcp_stats.clone(), cipher.clone(),
        ).await;

        if cancel.is_cancelled() {
            return Ok(());
        }

        // Connection lost — log and prepare to reconnect
        match result {
            Err(ref e) => {
                tracing::warn!(
                    tunnel_id = %tunnel_id,
                    "Relay tunnel connection lost: {e}, reconnecting in {reconnect_delay:?}"
                );
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    "tunnel",
                    format!("Tunnel disconnected from relay: {e}"),
                    &tunnel_id_str,
                );
            }
            Ok(()) => {
                tracing::warn!(
                    tunnel_id = %tunnel_id,
                    "Relay tunnel forwarder exited, reconnecting in {reconnect_delay:?}"
                );
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    "tunnel",
                    "Tunnel disconnected from relay",
                    &tunnel_id_str,
                );
            }
        }

        state_tx.send_replace(RelayTunnelState::Connecting);

        tokio::select! {
            _ = tokio::time::sleep(reconnect_delay) => {}
            _ = cancel.cancelled() => return Ok(()),
        }
    }
}

/// Main lifecycle for a direct-mode tunnel.
async fn run_direct_tunnel(
    config: TunnelConfig,
    tunnel_id: Uuid,
    direction: TunnelDirection,
    protocol: TunnelProtocol,
    local_addr: SocketAddr,
    tunnel_psk: String,
    state_tx: watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    udp_stats: Option<Arc<UdpForwarderStats>>,
    tcp_stats: Option<Arc<TcpForwarderStats>>,
) -> anyhow::Result<()> {
    use super::protocol::{ALPN_DIRECT, PeerMessage};
    use super::{auth, quic};

    let conn = match direction {
        // Ingress in direct mode = this edge is the QUIC server (has public IP)
        TunnelDirection::Ingress => {
            let listen_addr: SocketAddr = config
                .direct_listen_addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("direct_listen_addr required for direct ingress"))?
                .parse()?;

            state_tx.send_replace(RelayTunnelState::Connecting);

            let endpoint = quic::make_server_endpoint(
                listen_addr,
                ALPN_DIRECT,
                config.tls_cert_pem.as_deref(),
                config.tls_key_pem.as_deref(),
            )?;

            tracing::info!(
                tunnel_id = %tunnel_id,
                listen_addr = %listen_addr,
                "Direct tunnel: listening for peer"
            );

            state_tx.send_replace(RelayTunnelState::Waiting);

            // Accept incoming connection
            let incoming = tokio::select! {
                result = endpoint.accept() => {
                    result.ok_or_else(|| anyhow::anyhow!("Endpoint closed"))?
                }
                _ = cancel.cancelled() => anyhow::bail!("cancelled"),
            };
            let conn = incoming.await?;

            // Authenticate: accept control stream, verify peer auth
            state_tx.send_replace(RelayTunnelState::Connecting);
            let (mut send, mut recv) = conn.accept_bi().await?;
            let msg: PeerMessage = super::protocol::read_message(&mut recv).await?;

            match msg {
                PeerMessage::PeerAuth { tunnel_id: tid, token } if tid == tunnel_id => {
                    if auth::verify_token(&token, &tunnel_psk).is_some() {
                        super::protocol::write_message(
                            &mut send,
                            &PeerMessage::PeerAuthOk { tunnel_id },
                        )
                        .await?;
                        tracing::info!(tunnel_id = %tunnel_id, "Direct peer authenticated");
                    } else {
                        super::protocol::write_message(
                            &mut send,
                            &PeerMessage::PeerAuthError {
                                reason: "Invalid token".into(),
                            },
                        )
                        .await?;
                        anyhow::bail!("Direct peer auth failed: invalid token");
                    }
                }
                other => {
                    anyhow::bail!("Unexpected peer message during auth: {other:?}");
                }
            }

            conn
        }

        // Egress in direct mode = this edge connects to the public peer
        TunnelDirection::Egress => {
            let peer_addr: SocketAddr = config
                .peer_addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("peer_addr required for direct egress"))?
                .parse()?;

            state_tx.send_replace(RelayTunnelState::Connecting);

            let endpoint = quic::make_client_endpoint(ALPN_DIRECT)?;
            let conn = quic::connect(&endpoint, peer_addr, "bilbycast-edge").await?;

            // Authenticate: open control stream, send auth
            state_tx.send_replace(RelayTunnelState::Connecting);
            let (mut send, mut recv) = conn.open_bi().await?;
            let token = auth::generate_token(&tunnel_id.to_string(), &tunnel_psk);
            super::protocol::write_message(
                &mut send,
                &PeerMessage::PeerAuth { tunnel_id, token },
            )
            .await?;

            let response: PeerMessage = super::protocol::read_message(&mut recv).await?;
            match response {
                PeerMessage::PeerAuthOk { .. } => {
                    tracing::info!(tunnel_id = %tunnel_id, "Direct peer auth OK");
                }
                PeerMessage::PeerAuthError { reason } => {
                    anyhow::bail!("Direct peer auth failed: {reason}");
                }
                other => {
                    anyhow::bail!("Unexpected peer response: {other:?}");
                }
            }

            conn
        }
    };

    state_tx.send_replace(RelayTunnelState::Ready);

    // Create cipher for direct mode (defense-in-depth, optional)
    let cipher = if let Some(ref key) = config.tunnel_encryption_key {
        Some(Arc::new(
            super::crypto::TunnelCipher::new(key)
                .map_err(|e| anyhow::anyhow!("Failed to create tunnel cipher: {e}"))?,
        ))
    } else {
        None
    };

    // Run the appropriate forwarder using the shared stats
    run_forwarder(tunnel_id, protocol, direction, local_addr, conn, cancel, udp_stats, tcp_stats, cipher).await
}

/// Run the appropriate forwarder for the given protocol and direction.
/// Shared between relay and direct tunnel modes.
async fn run_forwarder(
    tunnel_id: Uuid,
    protocol: TunnelProtocol,
    direction: TunnelDirection,
    local_addr: SocketAddr,
    conn: quinn::Connection,
    cancel: CancellationToken,
    udp_stats: Option<Arc<UdpForwarderStats>>,
    tcp_stats: Option<Arc<TcpForwarderStats>>,
    cipher: Option<Arc<super::crypto::TunnelCipher>>,
) -> anyhow::Result<()> {
    match (protocol, direction) {
        (TunnelProtocol::Udp, TunnelDirection::Egress) => {
            let stats = udp_stats.unwrap_or_else(|| Arc::new(UdpForwarderStats::default()));
            udp_forwarder::run_egress(tunnel_id, local_addr, conn, stats, cancel, cipher).await?;
        }
        (TunnelProtocol::Udp, TunnelDirection::Ingress) => {
            let stats = udp_stats.unwrap_or_else(|| Arc::new(UdpForwarderStats::default()));
            udp_forwarder::run_ingress(tunnel_id, local_addr, conn, stats, cancel, cipher).await?;
        }
        (TunnelProtocol::Tcp, TunnelDirection::Egress) => {
            let stats = tcp_stats.unwrap_or_else(|| Arc::new(TcpForwarderStats::default()));
            tcp_forwarder::run_egress(tunnel_id, local_addr, conn, stats, cancel, cipher).await?;
        }
        (TunnelProtocol::Tcp, TunnelDirection::Ingress) => {
            let stats = tcp_stats.unwrap_or_else(|| Arc::new(TcpForwarderStats::default()));
            tcp_forwarder::run_ingress(tunnel_id, local_addr, conn, stats, cancel, cipher).await?;
        }
    }
    Ok(())
}
