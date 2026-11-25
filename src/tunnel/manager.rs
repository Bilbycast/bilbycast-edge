// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::carrier::{self, Carrier};
use super::config::{TunnelConfig, TunnelDirection, TunnelMode, TunnelProtocol, TunnelTransport};
use super::relay_client::{self, RelayTunnelParams, RelayTunnelState};
use super::tcp_forwarder::{self, TcpForwarderStats};
use super::udp_forwarder::{self, UdpForwarderStats};
use super::udp_relay_client::{self, NativeRelayParams};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::throughput::ThroughputEstimator;

/// Runtime state for an active tunnel.
struct TunnelRuntime {
    config: TunnelConfig,
    cancel: CancellationToken,
    state_rx: watch::Receiver<RelayTunnelState>,
    /// Index of the currently-active relay within `config.relay_addrs`
    /// (only meaningful for relay-mode tunnels).
    active_relay_idx_rx: Option<watch::Receiver<usize>>,
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
    /// Configured relay addresses in priority order (relay mode only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relay_addrs: Vec<String>,
    /// Index of the currently-active relay (0 = primary). Only present for
    /// running relay-mode tunnels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_relay_idx: Option<usize>,
    /// Convenience copy of `relay_addrs[active_relay_idx]`, suitable for the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_relay_addr: Option<String>,
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
    /// Inbound AEAD decrypt failures (native-UDP carrier). Sustained growth
    /// with flat `packets_received` ⇒ `tunnel_encryption_key` mismatch.
    pub decrypt_errors: u64,
    /// Datagrams the kernel dropped on the native-UDP carrier socket before
    /// the edge read them (receive-buffer overrun). Only the native-UDP
    /// carrier samples it; 0 on QUIC and where the platform has no counter.
    pub kernel_rx_drops: u64,
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

        let active_relay_idx_rx = match config.transport {
            // Native plain-UDP carrier (no QUIC): SRT/RIST over relay / direct.
            TunnelTransport::Udp => match config.mode {
                TunnelMode::Relay => {
                    let rx = self
                        .start_native_relay_tunnel(
                            &config, tunnel_id, cancel.clone(), state_tx, udp_stats.clone(),
                        )
                        .await?;
                    Some(rx)
                }
                TunnelMode::Direct => {
                    self.start_native_direct_tunnel(
                        &config, tunnel_id, cancel.clone(), state_tx, udp_stats.clone(),
                    )
                    .await?;
                    None
                }
            },
            // Legacy QUIC carrier (unchanged).
            TunnelTransport::Quic => match config.mode {
                TunnelMode::Relay => {
                    let rx = self
                        .start_relay_tunnel(
                            &config, tunnel_id, cancel.clone(), state_tx,
                            udp_stats.clone(), tcp_stats.clone(),
                        )
                        .await?;
                    Some(rx)
                }
                TunnelMode::Direct => {
                    self.start_direct_tunnel(
                        &config, tunnel_id, cancel.clone(), state_tx,
                        udp_stats.clone(), tcp_stats.clone(),
                    ).await?;
                    None
                }
            },
        };

        let tunnel_name = config.name.clone();
        let tunnel_id_str = config.id.clone();
        // Clones for the decrypt-skew watchdog, captured before `cancel` /
        // `udp_stats` are moved into the runtime below.
        let watchdog_cancel = cancel.clone();
        let watchdog_stats = udp_stats.clone();
        self.tunnels.insert(
            config.id.clone(),
            TunnelRuntime {
                config,
                cancel,
                state_rx,
                active_relay_idx_rx,
                udp_stats,
                tcp_stats,
                throughput_in: ThroughputEstimator::new(),
                throughput_out: ThroughputEstimator::new(),
            },
        );

        self.event_sender.emit_flow_with_details(
            EventSeverity::Info,
            category::TUNNEL,
            format!("Tunnel '{}' started", tunnel_name),
            &tunnel_id_str,
            serde_json::json!({ "tunnel_name": tunnel_name.clone() }),
        );

        // Phase 1 loud-failure: on the native-UDP carrier (SRT/RIST/bond legs
        // over relay or direct), spawn a per-tunnel watchdog that turns a silent
        // key-mismatch (every packet drops on AEAD decrypt) into a Critical
        // event. Cancelled with the tunnel.
        if let Some(stats) = watchdog_stats {
            tokio::spawn(kernel_drop_watchdog(
                tunnel_id_str.clone(),
                tunnel_name.clone(),
                stats.clone(),
                self.event_sender.clone(),
                watchdog_cancel.clone(),
            ));
            tokio::spawn(decrypt_skew_watchdog(
                tunnel_id_str,
                tunnel_name,
                stats,
                self.event_sender.clone(),
                watchdog_cancel,
            ));
        }

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
    ) -> anyhow::Result<watch::Receiver<usize>> {
        // Resolve ordered relay list (primary + optional backup). Validation has
        // already enforced length, format, and uniqueness. We keep these as
        // strings — DNS names resolve at connect time inside
        // `RelayLink::dial`, so an IP change behind a hostname is observed on
        // the next retry without a config edit.
        let relay_addrs: Vec<String> = config
            .effective_relays()
            .into_iter()
            .map(str::to_string)
            .collect();
        if relay_addrs.is_empty() {
            anyhow::bail!("at least one relay address required for relay mode");
        }

        let local_addr: SocketAddr = config
            .local_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid local_addr: {e}"))?;

        let edge_id = self.manager_node_id.get().cloned();
        let params = RelayTunnelParams {
            tunnel_id,
            relay_addrs,
            direction: config.direction,
            protocol: config.protocol,
            edge_id,
            tunnel_bind_secret: config.tunnel_bind_secret.clone(),
            failback_rtt_gate_ms: config.failback_rtt_gate_ms(),
            timing: Default::default(),
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

        // Channel for publishing the active relay index. Created here (not
        // inside the spawned task) so the TunnelRuntime can hold the receiver
        // for status snapshots without race-on-spawn.
        let (active_idx_tx, active_idx_rx) = watch::channel(0usize);

        // Spawn the tunnel lifecycle task
        tokio::spawn(async move {
            let result = run_relay_tunnel(
                params,
                config_name.clone(),
                direction,
                protocol,
                local_addr,
                state_tx,
                active_idx_tx,
                cancel.clone(),
                udp_stats,
                tcp_stats,
                cipher,
                event_sender.clone(),
            )
            .await;

            if let Err(e) = result
                && !cancel.is_cancelled() {
                    tracing::error!(tunnel_id = %config_id, "Relay tunnel failed: {e}");
                    event_sender.emit_flow_with_details(
                        EventSeverity::Critical,
                        category::TUNNEL,
                        format!("Tunnel '{}' failed: {e}", config_name),
                        &config_id,
                        serde_json::json!({ "tunnel_name": config_name }),
                    );
                    // Remove from DashMap so manager re-push can re-create it
                    tunnels.remove(&config_id);
                }
        });

        Ok(active_idx_rx)
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
                        // Port conflicts are permanent — don't retry
                        if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                            tracing::error!(
                                tunnel_id = %tunnel_id,
                                "Direct tunnel local port conflict, stopping: {e}"
                            );
                            event_sender.emit_flow_with_details(
                                EventSeverity::Critical,
                                category::PORT_CONFLICT,
                                format!("Tunnel '{}' stopped: {e}", config_name),
                                &config_id,
                                serde_json::json!({
                                    "error_code": "port_conflict",
                                    "component": format!("Tunnel '{}'", config_name),
                                    "tunnel_name": config_name,
                                    "error": e.to_string(),
                                }),
                            );
                            tunnels.remove(&config_id);
                            return;
                        }

                        tracing::warn!(
                            tunnel_id = %tunnel_id,
                            "Direct tunnel attempt {} failed: {e}",
                            attempt + 1
                        );
                        event_sender.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::TUNNEL,
                            format!("Tunnel '{}' attempt {} failed: {e}", config_name, attempt + 1),
                            &config_id,
                            serde_json::json!({ "tunnel_name": config_name, "attempt": attempt + 1 }),
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

    /// Start a native plain-UDP **relay** tunnel (no QUIC) — SRT/RIST over relay.
    async fn start_native_relay_tunnel(
        &self,
        config: &TunnelConfig,
        tunnel_id: Uuid,
        cancel: CancellationToken,
        state_tx: watch::Sender<RelayTunnelState>,
        udp_stats: Option<Arc<UdpForwarderStats>>,
    ) -> anyhow::Result<watch::Receiver<usize>> {
        let relay_addrs: Vec<String> = config
            .effective_relays()
            .into_iter()
            .map(str::to_string)
            .collect();
        if relay_addrs.is_empty() {
            anyhow::bail!("at least one relay address required for native-UDP relay mode");
        }
        let local_addr: SocketAddr = config
            .local_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid local_addr: {e}"))?;

        // Native relay carries no QUIC TLS — the AEAD is the only confidentiality
        // layer, so the encryption key is mandatory (enforced by validation).
        let key = config.tunnel_encryption_key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("tunnel_encryption_key is required for native-UDP relay mode")
        })?;
        let cipher = Some(Arc::new(
            super::crypto::TunnelCipher::new(key)
                .map_err(|e| anyhow::anyhow!("Failed to create tunnel cipher: {e}"))?,
        ));

        let params = NativeRelayParams {
            tunnel_id,
            relay_addrs,
            direction: config.direction,
            tunnel_bind_secret: config.tunnel_bind_secret.clone(),
            interface: config.interface.clone(),
            source: config.source.clone(),
            gateway: config.gateway.clone(),
        };
        let stats = udp_stats.unwrap_or_else(|| Arc::new(UdpForwarderStats::default()));
        let (active_idx_tx, active_idx_rx) = watch::channel(0usize);

        let config_id = config.id.clone();
        let config_name = config.name.clone();
        let tunnels = self.tunnels.clone();
        let event_sender = self.event_sender.clone();

        tokio::spawn(async move {
            let result = udp_relay_client::run_native_relay_tunnel(
                params,
                local_addr,
                state_tx,
                active_idx_tx,
                cancel.clone(),
                stats,
                cipher,
                event_sender.clone(),
            )
            .await;

            if let Err(e) = result
                && !cancel.is_cancelled() {
                    tracing::error!(tunnel_id = %config_id, "Native-UDP relay tunnel failed: {e}");
                    event_sender.emit_flow_with_details(
                        EventSeverity::Critical,
                        category::TUNNEL,
                        format!("Tunnel '{}' failed: {e}", config_name),
                        &config_id,
                        serde_json::json!({ "tunnel_name": config_name, "transport": "udp" }),
                    );
                    tunnels.remove(&config_id);
                }
        });

        Ok(active_idx_rx)
    }

    /// Start a native plain-UDP **direct** tunnel (no QUIC) — peer-to-peer.
    async fn start_native_direct_tunnel(
        &self,
        config: &TunnelConfig,
        tunnel_id: Uuid,
        cancel: CancellationToken,
        state_tx: watch::Sender<RelayTunnelState>,
        udp_stats: Option<Arc<UdpForwarderStats>>,
    ) -> anyhow::Result<()> {
        let local_addr: SocketAddr = config
            .local_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid local_addr: {e}"))?;
        let tunnel_psk = config
            .tunnel_psk
            .clone()
            .ok_or_else(|| anyhow::anyhow!("tunnel_psk required for direct mode"))?;
        // Direct mode: encryption optional (defense-in-depth) — but recommended.
        let cipher = match config.tunnel_encryption_key.as_deref() {
            Some(key) => Some(Arc::new(
                super::crypto::TunnelCipher::new(key)
                    .map_err(|e| anyhow::anyhow!("Failed to create tunnel cipher: {e}"))?,
            )),
            None => None,
        };

        let direction = config.direction;
        let peer_addr = config.peer_addr.clone();
        let direct_listen_addr = config.direct_listen_addr.clone();
        let interface = config.interface.clone();
        let source = config.source.clone();
        let gateway = config.gateway.clone();
        let stats = udp_stats.unwrap_or_else(|| Arc::new(UdpForwarderStats::default()));
        let config_id = config.id.clone();
        let config_name = config.name.clone();
        let tunnels = self.tunnels.clone();
        let event_sender = self.event_sender.clone();

        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            let max_delay = std::time::Duration::from_secs(30);
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                let result = udp_relay_client::run_native_direct_tunnel(
                    tunnel_id,
                    direction,
                    local_addr,
                    peer_addr.clone(),
                    direct_listen_addr.clone(),
                    interface.clone(),
                    source.clone(),
                    gateway.clone(),
                    tunnel_psk.clone(),
                    state_tx.clone(),
                    cancel.clone(),
                    stats.clone(),
                    cipher.clone(),
                    event_sender.clone(),
                )
                .await;

                if cancel.is_cancelled() {
                    return;
                }
                match result {
                    Ok(()) => attempt = 0,
                    Err(e) => {
                        if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                            tracing::error!(tunnel_id = %tunnel_id, "Native-UDP direct tunnel port conflict: {e}");
                            event_sender.emit_flow_with_details(
                                EventSeverity::Critical,
                                category::PORT_CONFLICT,
                                format!("Tunnel '{}' stopped: {e}", config_name),
                                &config_id,
                                serde_json::json!({
                                    "error_code": "port_conflict",
                                    "component": format!("Tunnel '{}'", config_name),
                                    "tunnel_name": config_name,
                                    "transport": "udp",
                                    "error": e.to_string(),
                                }),
                            );
                            tunnels.remove(&config_id);
                            return;
                        }
                        tracing::warn!(tunnel_id = %tunnel_id, "Native-UDP direct attempt {} failed: {e}", attempt + 1);
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
        });

        Ok(())
    }

    /// Destroy (stop) a tunnel by ID.
    ///
    /// Returns `Ok(true)` when a live runtime tunnel was found and cancelled,
    /// or `Ok(false)` when no such tunnel was in the registry. A tunnel can
    /// self-evict from the registry on a connect failure (relay unreachable,
    /// port-bind conflict), so "already gone" is the *expected* end state of a
    /// delete, not an error — callers reconcile `config.json` regardless.
    /// Keeping this idempotent lets the surgical `delete_tunnel` paths clean an
    /// orphaned config entry instead of failing with "Tunnel not found" (and
    /// then resurrecting it on the next restart).
    pub async fn destroy_tunnel(&self, id: &str) -> anyhow::Result<bool> {
        let Some((_, runtime)) = self.tunnels.remove(id) else {
            tracing::debug!(
                tunnel_id = %id,
                "destroy_tunnel: no live tunnel in registry (already stopped / self-evicted)"
            );
            return Ok(false);
        };

        let tunnel_name = runtime.config.name.clone();
        runtime.cancel.cancel();
        tracing::info!(tunnel_id = %id, "Tunnel destroyed");

        self.event_sender.emit_flow_with_details(
            EventSeverity::Info,
            category::TUNNEL,
            format!("Tunnel '{}' stopped", tunnel_name),
            id,
            serde_json::json!({ "tunnel_name": tunnel_name }),
        );

        Ok(true)
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
        stats.decrypt_errors = udp.decrypt_errors.load(Ordering::Relaxed);
        stats.kernel_rx_drops = udp.kernel_rx_drops.load(Ordering::Relaxed);
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

    let relay_addrs: Vec<String> = runtime
        .config
        .effective_relays()
        .into_iter()
        .map(str::to_string)
        .collect();
    let active_relay_idx = runtime.active_relay_idx_rx.as_ref().map(|rx| *rx.borrow());
    let active_relay_addr = active_relay_idx
        .and_then(|idx| relay_addrs.get(idx).cloned());

    TunnelStatus {
        id: runtime.config.id.clone(),
        name: runtime.config.name.clone(),
        protocol: runtime.config.protocol.to_string(),
        mode: runtime.config.mode.to_string(),
        direction: runtime.config.direction.to_string(),
        local_addr: runtime.config.local_addr.clone(),
        state: state.to_string(),
        relay_addrs,
        active_relay_idx,
        active_relay_addr,
        stats,
    }
}

/// Decision core of the decrypt-skew watchdog, factored out so it can be
/// unit-tested without driving a real timer. Skew = errors at/above the
/// threshold AND nothing delivered in the window. Any delivery clears it (a
/// lossy link still delivers some packets — only a key mismatch delivers none).
fn is_decrypt_skew(d_decrypt: u64, d_received: u64, threshold: u64) -> bool {
    d_received == 0 && d_decrypt >= threshold
}

/// Per-tunnel inbound-decrypt skew watchdog (Phase 1 loud-failure).
///
/// Samples the native-UDP forwarder's atomics on a fixed cadence — strictly off
/// the data path — and raises one Critical `tunnel_decrypt_failure` event when
/// AEAD decrypt errors climb while zero packets are delivered: the unambiguous
/// signature of a `tunnel_encryption_key` mismatch between the two edges (every
/// packet dropped, no media, no other error). Sporadic link loss never trips it
/// because a lossy link still delivers *some* packets (delta_received > 0). The
/// alarm rearms once healthy traffic resumes, so a transient never latches.
async fn decrypt_skew_watchdog(
    tunnel_id: String,
    tunnel_name: String,
    stats: Arc<UdpForwarderStats>,
    event_sender: EventSender,
    cancel: CancellationToken,
) {
    const WINDOW: Duration = Duration::from_secs(10);
    // A lossy/duplicated link produces a trickle of decrypt errors; key skew
    // produces a flood with zero delivery. 50 per window with no delivered
    // packet separates the two cleanly.
    const SKEW_THRESHOLD: u64 = 50;

    let mut last_decrypt = stats.decrypt_errors.load(Ordering::Relaxed);
    let mut last_received = stats.packets_received.load(Ordering::Relaxed);
    let mut fired = false;
    let mut tick = tokio::time::interval(WINDOW);
    tick.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {
                let cur_decrypt = stats.decrypt_errors.load(Ordering::Relaxed);
                let cur_received = stats.packets_received.load(Ordering::Relaxed);
                let d_decrypt = cur_decrypt.saturating_sub(last_decrypt);
                let d_received = cur_received.saturating_sub(last_received);
                last_decrypt = cur_decrypt;
                last_received = cur_received;

                if d_received > 0 {
                    // Healthy traffic is flowing — rearm so a future episode fires.
                    fired = false;
                } else if is_decrypt_skew(d_decrypt, d_received, SKEW_THRESHOLD) && !fired {
                    fired = true;
                    event_sender.emit_flow_with_details(
                        EventSeverity::Critical,
                        category::TUNNEL,
                        format!(
                            "Tunnel '{tunnel_name}': {d_decrypt} decrypt failures and zero packets \
                             delivered in {}s — likely tunnel_encryption_key mismatch between edges",
                            WINDOW.as_secs()
                        ),
                        &tunnel_id,
                        serde_json::json!({
                            "error_code": "tunnel_decrypt_failure",
                            "tunnel_name": tunnel_name,
                            "decrypt_errors_window": d_decrypt,
                            "packets_delivered_window": d_received,
                            "window_secs": WINDOW.as_secs(),
                        }),
                    );
                }
            }
        }
    }
}

/// Per-tunnel kernel-drop watchdog (edge#165).
///
/// The kernel drops datagrams on a carrier socket whose receive buffer
/// overflows, before the edge reads them, so no counter of the tunnel's own
/// moves — and SRT retransmits the loss until its latency budget runs out, so
/// nothing downstream moves either. `kernel_rx_drops` is the only sign of it;
/// this raises one Warning `tunnel_kernel_rx_drops` when it moves, and again
/// only after a minute without drops. The message is the same every time and
/// the numbers ride `details`, so the manager folds repeats into one row.
async fn kernel_drop_watchdog(
    tunnel_id: String,
    tunnel_name: String,
    stats: Arc<UdpForwarderStats>,
    event_sender: EventSender,
    cancel: CancellationToken,
) {
    const WINDOW: Duration = Duration::from_secs(10);
    const CLEAN_WINDOWS_TO_REARM: u32 = 6;

    let mut last = stats.kernel_rx_drops.load(Ordering::Relaxed);
    let mut armed = true;
    let mut clean_windows = 0u32;
    let mut tick = tokio::time::interval(WINDOW);
    tick.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {
                let now = stats.kernel_rx_drops.load(Ordering::Relaxed);
                let dropped = now.saturating_sub(last);
                last = now;
                if dropped == 0 {
                    clean_windows = clean_windows.saturating_add(1);
                    if clean_windows >= CLEAN_WINDOWS_TO_REARM {
                        armed = true;
                    }
                    continue;
                }
                clean_windows = 0;
                if !armed {
                    continue;
                }
                armed = false;
                event_sender.emit_flow_with_details(
                    EventSeverity::Warning,
                    category::TUNNEL,
                    format!(
                        "Tunnel '{tunnel_name}': the kernel is dropping datagrams on the carrier \
                         socket before the edge reads them (receive buffer overrun)"
                    ),
                    &tunnel_id,
                    serde_json::json!({
                        "error_code": "tunnel_kernel_rx_drops",
                        "tunnel_name": tunnel_name,
                        "dropped_window": dropped,
                        "dropped_total": now,
                        "window_secs": WINDOW.as_secs(),
                    }),
                );
            }
        }
    }
}

/// Main lifecycle for a relay-mode tunnel.
///
/// Wraps the connect+forward cycle in a reconnection loop so the tunnel
/// automatically recovers when the relay restarts or the QUIC connection drops.
/// A tunnel with two relays may move to the other one while it forwards
/// ([`relay_client::other_relay`]); the forwarder goes on through that.
#[allow(clippy::too_many_arguments)]
async fn run_relay_tunnel(
    params: RelayTunnelParams,
    tunnel_name: String,
    direction: TunnelDirection,
    protocol: TunnelProtocol,
    local_addr: SocketAddr,
    state_tx: watch::Sender<RelayTunnelState>,
    active_idx_tx: watch::Sender<usize>,
    cancel: CancellationToken,
    udp_stats: Option<Arc<UdpForwarderStats>>,
    tcp_stats: Option<Arc<TcpForwarderStats>>,
    cipher: Option<Arc<super::crypto::TunnelCipher>>,
    event_sender: EventSender,
) -> anyhow::Result<()> {
    let tunnel_id = params.tunnel_id;
    let tunnel_id_str = tunnel_id.to_string();
    let reconnect_delay = Duration::from_secs(1);

    // Active relay index — persists across reconnects so the tunnel stays
    // sticky on the same relay unless it fails to reconnect.
    let mut idx = 0;

    loop {
        // Run the failover walker, until both sides are on a relay.
        let mut link = relay_client::connect_with_retry(
            &params,
            &mut idx,
            &active_idx_tx,
            &state_tx,
            cancel.clone(),
            event_sender.clone(),
        )
        .await?;

        tracing::info!(
            tunnel_id = %tunnel_id,
            direction = %direction,
            protocol = %protocol,
            encrypted = cipher.is_some(),
            relay_idx = idx,
            "Relay tunnel connected, starting forwarder"
        );

        // A task of its own: what is waited for beside it is not looked at for
        // every datagram. Its token ends the TCP connections that go through it,
        // whichever relay they are on.
        let forwarder_cancel = cancel.child_token();
        let (moves, moves_rx) = mpsc::channel(1);
        let mut forwarder = tokio::spawn(run_forwarder(
            tunnel_id, protocol, direction, local_addr,
            Carrier::new(link.conn().clone(), moves_rx), forwarder_cancel.clone(),
            udp_stats.clone(), tcp_stats.clone(), cipher.clone(),
        ));

        // Until the QUIC connection drops or the tunnel is stopped.
        let ended = loop {
            let conn = link.conn().clone();
            tokio::select! {
                ended = &mut forwarder => break ended,
                _ = link.hold(&params.relay_addrs[idx], &state_tx, &event_sender) => break (&mut forwarder).await,
                (to, next) = relay_client::other_relay(
                    &params, idx, &conn, state_tx.subscribe(), &event_sender,
                ) => {
                    // The peer is told at the same time, and what it sends until
                    // it has moved too comes by the relay we leave.
                    let was_up = *state_tx.borrow() == RelayTunnelState::Ready;
                    let drain = if was_up { carrier::DRAIN } else { Duration::ZERO };
                    if to == 0 {
                        report_failback(&params, &conn, next.conn(), &event_sender);
                        relay_client::report_up(&params, to, to, &event_sender);
                    } else {
                        relay_client::report_up(&params, idx, to, &event_sender);
                    }
                    // It is not taken if the forwarder has just ended.
                    let _ = moves.send(next.conn().clone()).await;
                    let left = std::mem::replace(&mut link, next);
                    tokio::spawn(left.leave(drain));
                    idx = to;
                    active_idx_tx.send_replace(idx);
                    state_tx.send_replace(RelayTunnelState::Ready);
                }
            }
        };
        forwarder_cancel.cancel();

        if cancel.is_cancelled() {
            return Ok(());
        }

        // Connection lost — log and prepare to reconnect
        let e = match ended {
            Ok(Err(e)) => e,
            Ok(Ok(())) => anyhow::anyhow!("forwarder exited"),
            Err(e) => anyhow::anyhow!("forwarder: {e}"),
        };

        // Port conflicts are permanent — don't retry, escalate immediately
        if crate::util::port_error::anyhow_is_addr_in_use(&e) {
            tracing::error!(
                tunnel_id = %tunnel_id,
                "Tunnel local port conflict, stopping tunnel: {e}"
            );
            event_sender.emit_flow_with_details(
                EventSeverity::Critical,
                category::PORT_CONFLICT,
                format!("Tunnel '{}' stopped: {e}", tunnel_name),
                &tunnel_id_str,
                serde_json::json!({
                    "error_code": "port_conflict",
                    "component": format!("Tunnel '{}'", tunnel_name),
                    "tunnel_name": tunnel_name,
                    "error": e.to_string(),
                }),
            );
            return Err(anyhow::anyhow!("{e}"));
        }

        tracing::warn!(
            tunnel_id = %tunnel_id,
            "Relay tunnel connection lost: {e}, reconnecting in {reconnect_delay:?}"
        );
        event_sender.emit_flow_with_details(
            EventSeverity::Warning,
            category::TUNNEL,
            format!("Tunnel disconnected from relay: {e}"),
            &tunnel_id_str,
            serde_json::json!({ "tunnel_name": tunnel_name, "reason": e.to_string() }),
        );

        state_tx.send_replace(RelayTunnelState::Connecting);

        tokio::select! {
            _ = tokio::time::sleep(reconnect_delay) => {}
            _ = cancel.cancelled() => return Ok(()),
        }
    }
}

/// Report that the tunnel goes back to its primary relay.
fn report_failback(
    params: &RelayTunnelParams,
    active: &quinn::Connection,
    primary: &quinn::Connection,
    event_sender: &EventSender,
) {
    let primary_relay = &params.relay_addrs[0];
    event_sender.emit_flow_with_details(
        EventSeverity::Info,
        category::TUNNEL,
        format!("Tunnel failing back to primary relay {primary_relay}"),
        &params.tunnel_id.to_string(),
        serde_json::json!({
            "primary_relay_addr": primary_relay,
            "primary_rtt_ms": primary.rtt().as_millis(),
            "active_rtt_ms": active.rtt().as_millis(),
            "gate_ms": params.failback_rtt_gate_ms,
        }),
    );
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
    // There is no other connection to move to.
    let carrier = Carrier::new(conn, mpsc::channel(1).1);
    run_forwarder(tunnel_id, protocol, direction, local_addr, carrier, cancel, udp_stats, tcp_stats, cipher).await
}

/// Run the appropriate forwarder for the given protocol and direction.
/// Shared between relay and direct tunnel modes.
async fn run_forwarder(
    tunnel_id: Uuid,
    protocol: TunnelProtocol,
    direction: TunnelDirection,
    local_addr: SocketAddr,
    carrier: Carrier,
    cancel: CancellationToken,
    udp_stats: Option<Arc<UdpForwarderStats>>,
    tcp_stats: Option<Arc<TcpForwarderStats>>,
    cipher: Option<Arc<super::crypto::TunnelCipher>>,
) -> anyhow::Result<()> {
    match (protocol, direction) {
        (TunnelProtocol::Udp, TunnelDirection::Egress) => {
            let stats = udp_stats.unwrap_or_else(|| Arc::new(UdpForwarderStats::default()));
            // Wrap the QUIC carrier as a DatagramLink; the native-UDP path
            // (transport=udp) uses a PlainUdpLink instead — see udp_relay_client.
            udp_forwarder::run_egress(
                tunnel_id,
                local_addr,
                udp_forwarder::QuicLink(carrier),
                stats,
                cancel,
                cipher,
            )
            .await?;
        }
        (TunnelProtocol::Udp, TunnelDirection::Ingress) => {
            let stats = udp_stats.unwrap_or_else(|| Arc::new(UdpForwarderStats::default()));
            udp_forwarder::run_ingress(
                tunnel_id,
                local_addr,
                udp_forwarder::QuicLink(carrier),
                stats,
                cancel,
                cipher,
            )
            .await?;
        }
        (TunnelProtocol::Tcp, TunnelDirection::Egress) => {
            let stats = tcp_stats.unwrap_or_else(|| Arc::new(TcpForwarderStats::default()));
            tcp_forwarder::run_egress(tunnel_id, local_addr, carrier, stats, cancel, cipher).await?;
        }
        (TunnelProtocol::Tcp, TunnelDirection::Ingress) => {
            let stats = tcp_stats.unwrap_or_else(|| Arc::new(TcpForwarderStats::default()));
            tcp_forwarder::run_ingress(tunnel_id, local_addr, carrier, stats, cancel, cipher).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod decrypt_skew_tests {
    use super::is_decrypt_skew;

    const T: u64 = 50;

    #[test]
    fn key_mismatch_floods_errors_with_zero_delivery() {
        // Every packet failing AEAD, nothing delivered → skew.
        assert!(is_decrypt_skew(50, 0, T));
        assert!(is_decrypt_skew(100_000, 0, T));
    }

    #[test]
    fn lossy_link_still_delivers_so_never_skew() {
        // A lossy/duplicated link produces some decrypt errors but still
        // delivers packets — must NOT trip the key-mismatch alarm.
        assert!(!is_decrypt_skew(50, 1, T));
        assert!(!is_decrypt_skew(10_000, 100, T));
    }

    #[test]
    fn quiet_or_sub_threshold_is_not_skew() {
        assert!(!is_decrypt_skew(0, 0, T)); // idle tunnel
        assert!(!is_decrypt_skew(49, 0, T)); // a few stray errors, below floor
    }
}

#[cfg(test)]
mod destroy_tunnel_tests {
    use super::TunnelManager;
    use crate::manager::events::event_channel;

    /// Deleting a tunnel that is not in the runtime registry — because it was
    /// never started, or self-evicted on a connect failure — must report
    /// `Ok(false)` rather than erroring. The surgical `delete_tunnel` handlers
    /// rely on this to still reconcile `config.json` (and not resurrect the
    /// orphan on restart) instead of bailing with "Tunnel not found".
    #[tokio::test]
    async fn destroy_absent_tunnel_is_ok_false_not_error() {
        let (tx, _rx) = event_channel();
        let mgr = TunnelManager::new(tx);
        let r = mgr
            .destroy_tunnel("bbbb0000-0000-4000-8000-000000000000")
            .await;
        assert!(
            matches!(r, Ok(false)),
            "deleting an absent tunnel should be Ok(false), got {r:?}"
        );
    }
}

#[cfg(test)]
mod kernel_drop_watchdog_tests {
    //! edge#165: kernel drops on the carrier socket are the one loss nothing
    //! else on the tunnel can see, so they must reach the operator — once,
    //! not once every ten seconds.
    use super::*;
    use crate::manager::events::event_channel;

    async fn windows(n: u32) {
        for _ in 0..n {
            tokio::time::advance(Duration::from_secs(10)).await;
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drops_raise_one_warning_and_rearm_after_a_clean_minute() {
        let (tx, mut rx) = event_channel();
        let stats = Arc::new(UdpForwarderStats::default());
        let cancel = CancellationToken::new();
        tokio::spawn(kernel_drop_watchdog(
            "t1".into(),
            "contribution".into(),
            stats.clone(),
            tx,
            cancel.clone(),
        ));
        tokio::task::yield_now().await;

        // A clean window raises nothing.
        windows(1).await;
        assert!(rx.try_recv().is_err());

        // Drops in a window raise one Warning carrying the numbers.
        stats.kernel_rx_drops.fetch_add(1_700, Ordering::Relaxed);
        windows(1).await;
        let ev = rx.try_recv().expect("a Warning for the drops");
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(ev.category, category::TUNNEL);
        assert_eq!(ev.flow_id.as_deref(), Some("t1"));
        let d = ev.details.expect("details");
        assert_eq!(d["error_code"], "tunnel_kernel_rx_drops");
        assert_eq!(d["dropped_window"], 1_700);
        assert_eq!(d["dropped_total"], 1_700);

        // Still dropping: no repeat.
        stats.kernel_rx_drops.fetch_add(50, Ordering::Relaxed);
        windows(1).await;
        assert!(rx.try_recv().is_err(), "a continuing episode is one event");

        // Five clean windows are not enough to rearm…
        windows(5).await;
        stats.kernel_rx_drops.fetch_add(5, Ordering::Relaxed);
        windows(1).await;
        assert!(rx.try_recv().is_err(), "rearmed before a clean minute");

        // …six are.
        windows(6).await;
        stats.kernel_rx_drops.fetch_add(3, Ordering::Relaxed);
        windows(1).await;
        let ev = rx.try_recv().expect("a new episode after a clean minute");
        assert_eq!(ev.details.unwrap()["dropped_window"], 3);
        cancel.cancel();
    }
}

#[cfg(test)]
mod relay_notice_tests {
    //! What a relay says of a tunnel that is up, and what becomes of the tunnel
    //! and of what it carries.
    use super::*;
    use crate::manager::events::{Event, event_channel};
    use crate::tunnel::protocol::{self, RelayMessage};
    use crate::tunnel::relay_client::stand_in::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};
    use tokio::sync::mpsc;

    struct Edge {
        id: Uuid,
        relay: StandIn,
        manager: TunnelManager,
        events: mpsc::Receiver<Event>,
    }

    impl Edge {
        async fn start(protocol: &str, direction: &str, local_addr: SocketAddr) -> Self {
            let relay = StandIn::start();
            let id = Uuid::new_v4();
            let (event_tx, events) = event_channel();
            let manager = TunnelManager::new(event_tx);
            let config = serde_json::json!({
                "id": id, "name": "t", "protocol": protocol, "mode": "relay", "direction": direction,
                "local_addr": local_addr, "relay_addrs": [relay.addr],
            });
            manager.create_tunnel(serde_json::from_value(config).unwrap()).await.unwrap();
            let mut edge = Self { id, relay, manager, events };
            edge.raises(&["Tunnel 't' started"]).await;
            edge
        }

        /// Bind, and the peer with it.
        async fn up(&mut self) -> Session {
            let (mut session, _bind) = self.relay.binding().await;
            session.answer(ready(self.id)).await;
            self.becomes("ready").await;
            self.raises(&["Tunnel connected to relay"]).await;
            session
        }

        async fn becomes(&self, state: &str) {
            let is = || self.manager.tunnel_status(&self.id.to_string()).map(|s| s.state);
            soon(state, async {
                while is().as_deref() != Some(state) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
        }

        /// The next events raised, by their messages.
        async fn raises(&mut self, messages: &[&str]) {
            for message in messages {
                let event = soon(message, self.events.recv()).await.unwrap();
                assert_eq!(event.message, *message);
            }
        }
    }

    pub(super) async fn passes(
        session: &Session,
        id: Uuid,
        app: &UdpSocket,
        from_edge: &mut Option<SocketAddr>,
    ) {
        let payload = Uuid::new_v4().to_string();
        let datagram = protocol::encode_udp_datagram(&id, payload.as_bytes());
        session.conn.send_datagram(datagram.into()).unwrap();
        let mut buf = [0; 64];
        let (n, edge) = soon("a datagram", app.recv_from(&mut buf)).await.unwrap();
        assert_eq!(&buf[..n], payload.as_bytes());
        assert_eq!(*from_edge.get_or_insert(edge), edge, "another forwarder");
        app.send_to(b"back", edge).await.unwrap();
        let back = soon("its answer", session.conn.read_datagram()).await.unwrap();
        assert_eq!(protocol::decode_udp_datagram(&back).unwrap(), (id, &b"back"[..]));
    }

    /// The peer goes and is back, and our side is removed and bound again: one
    /// connection and one forwarder throughout, on one socket.
    #[tokio::test]
    async fn a_udp_tunnel_goes_on_through_what_the_relay_says() {
        let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut edge = Edge::start("udp", "ingress", app.local_addr().unwrap()).await;
        let mut session = edge.up().await;
        let id = edge.id;
        let mut from = None;
        passes(&session, id, &app, &mut from).await;

        session.notify(down(id, "peer unbound")).await;
        edge.becomes("waiting").await;
        passes(&session, id, &app, &mut from).await;
        session.notify(ready(id)).await;
        edge.becomes("ready").await;
        passes(&session, id, &app, &mut from).await;
        edge.raises(&["Tunnel peer disconnected: peer unbound", "Tunnel ready again"]).await;

        session.notify(down(id, "tunnel closed by manager")).await;
        edge.becomes("down").await;
        passes(&session, id, &app, &mut from).await;
        session.next().await;
        session.answer(ready(id)).await;
        edge.becomes("ready").await;
        passes(&session, id, &app, &mut from).await;
        edge.raises(&["Tunnel removed on relay: tunnel closed by manager", "Tunnel ready again"]).await;
        assert!(!edge.relay.is_dialled(Duration::from_millis(300)).await);
    }

    /// A TCP connection through the tunnel is not cut by it either.
    #[tokio::test]
    async fn a_tcp_connection_goes_on_through_what_the_relay_says() {
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut edge = Edge::start("tcp", "ingress", app.local_addr().unwrap()).await;
        let mut session = edge.up().await;
        let id = edge.id;
        let (mut send, mut recv) = session.conn.open_bi().await.unwrap();
        let header = protocol::StreamHeader { tunnel_id: id, stream_type: protocol::StreamType::Tcp };
        protocol::write_stream_header(&mut send, &header).await.unwrap();
        let (mut tcp, _) = soon("a connection", app.accept()).await.unwrap();
        for n in 0..3u8 {
            send.write_all(&[n]).await.unwrap();
            assert_eq!(soon("its data", tcp.read_u8()).await.unwrap(), n);
            tcp.write_all(&[n + 100]).await.unwrap();
            let mut back = [0];
            soon("its answer", recv.read_exact(&mut back)).await.unwrap();
            assert_eq!(back, [n + 100]);
            match n {
                0 => {
                    session.notify(down(id, "peer disconnected")).await;
                    edge.becomes("waiting").await;
                }
                _ => {
                    session.notify(down(id, "tunnel deleted via relay REST API")).await;
                    edge.becomes("down").await;
                    session.next().await;
                    session.answer(ready(id)).await;
                    edge.becomes("ready").await;
                }
            }
        }
    }

    /// Bound again and refused: the edge connects anew, is refused there too,
    /// and goes on trying, more and more seldom, until it is not. It is an
    /// alarm once, and again when a tunnel that was up since is refused.
    #[tokio::test]
    async fn a_tunnel_that_was_revoked_is_tried_again_and_again() {
        let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut edge = Edge::start("udp", "ingress", app.local_addr().unwrap()).await;
        let mut session = edge.up().await;
        let id = edge.id;
        passes(&session, id, &app, &mut None).await;
        session.notify(down(id, "tunnel deleted via relay REST API")).await;
        session.next().await;
        session.answer(down(id, "bind authentication failed")).await;
        edge.raises(&["Tunnel removed on relay: tunnel deleted via relay REST API"]).await;
        edge.raises(&["Tunnel disconnected from relay: quic datagram recv: closed"]).await;
        let mut since = tokio::time::Instant::now();
        let rejected = ["Tunnel bind rejected by relay: bind authentication failed"];
        for wait in [1, 1, 2] {
            let (mut session, _bind) = edge.relay.binding().await;
            assert!(since.elapsed() >= Duration::from_millis(wait * 1000 - 100), "{:?}", since.elapsed());
            edge.becomes("connecting").await;
            session.answer(down(id, "bind authentication failed")).await;
            session.closed().await;
            since = tokio::time::Instant::now();
        }
        edge.raises(&rejected).await;
        let mut session = edge.up().await;
        passes(&session, id, &app, &mut None).await;

        session.send.finish().unwrap();
        edge.raises(&["Tunnel disconnected from relay: quic datagram recv: closed"]).await;
        let (mut session, _bind) = edge.relay.binding().await;
        session.answer(down(id, "bind authentication failed")).await;
        edge.raises(&rejected).await;
        edge.up().await;
    }

    /// The relay's control stream ends: the edge connects anew.
    #[tokio::test]
    async fn a_tunnel_whose_control_stream_ends_connects_anew() {
        let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut edge = Edge::start("udp", "ingress", app.local_addr().unwrap()).await;
        let mut session = edge.up().await;
        session.send.finish().unwrap();
        edge.raises(&["Tunnel disconnected from relay: quic datagram recv: closed"]).await;
        let session = edge.up().await;
        passes(&session, edge.id, &app, &mut None).await;
    }

    /// Deleted while it waits to bind its side again: it does not, and leaves.
    #[tokio::test]
    async fn a_tunnel_that_is_deleted_is_not_bound_again() {
        let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut edge = Edge::start("udp", "ingress", app.local_addr().unwrap()).await;
        let mut session = edge.up().await;
        session.notify(down(edge.id, "tunnel closed by manager")).await;
        edge.becomes("down").await;
        edge.manager.destroy_tunnel(&edge.id.to_string()).await.unwrap();
        session.closed().await;
        while let Ok(msg) = protocol::read_message(&mut session.recv).await {
            assert!(matches!(msg, protocol::EdgeMessage::Ping), "{msg:?}");
        }
        assert!(!edge.relay.is_dialled(Duration::from_millis(1500)).await);
        edge.raises(&["Tunnel removed on relay: tunnel closed by manager", "Tunnel 't' stopped"]).await;
    }

    /// A tunnel that cannot listen locally leaves the relay: it does not keep
    /// its side bound there, which would be neither forwarded to nor free.
    #[tokio::test]
    async fn a_tunnel_that_cannot_listen_lets_go_of_its_bind() {
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        for (protocol, taken) in
            [("udp", udp.local_addr().unwrap()), ("tcp", tcp.local_addr().unwrap())]
        {
            let edge = Edge::start(protocol, "egress", taken).await;
            let (mut session, _bind) = edge.relay.binding().await;
            session.answer(RelayMessage::TunnelReady { tunnel_id: edge.id }).await;
            session.closed().await;
            soon("its end", async {
                while edge.manager.tunnel_status(&edge.id.to_string()).is_some() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
        }
    }
}

#[cfg(test)]
mod two_relay_tests {
    //! A tunnel with two relays: where it looks for its peer, when it moves, and
    //! what becomes of what it carries.
    use super::relay_notice_tests::passes;
    use super::*;
    use crate::manager::events::{Event, event_channel};
    use crate::tunnel::protocol::{self, EdgeMessage};
    use crate::tunnel::relay_client::{Timing, stand_in::*};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream, UdpSocket};
    use tokio::task::JoinHandle;
    use tokio::time::{Instant, timeout};

    const A_MOMENT: Duration = Duration::from_millis(300);
    const GONE: &str = "peer disconnected";
    const REFUSED: &str = "bind authentication failed";

    /// An edge with one tunnel, its two relays, and what it is for.
    struct Edge {
        id: Uuid,
        primary: StandIn,
        backup: StandIn,
        states: mpsc::UnboundedReceiver<RelayTunnelState>,
        state: watch::Receiver<RelayTunnelState>,
        active: watch::Receiver<usize>,
        events: mpsc::Receiver<Event>,
        cancel: CancellationToken,
        app: UdpSocket,
        /// Where the forwarder sends to the application from.
        from: Option<SocketAddr>,
        task: JoinHandle<anyhow::Result<()>>,
    }

    impl Edge {
        async fn start_with(adjust: impl FnOnce(&mut RelayTunnelParams)) -> Self {
            Self::between(StandIn::start(), StandIn::start(), adjust).await
        }

        async fn between(
            primary: StandIn,
            backup: StandIn,
            adjust: impl FnOnce(&mut RelayTunnelParams),
        ) -> Self {
            let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let local_addr = app.local_addr().unwrap();
            let udp_in = (TunnelProtocol::Udp, TunnelDirection::Ingress);
            Self::of(udp_in, local_addr, app, (primary, backup), adjust)
        }

        async fn tcp(
            direction: TunnelDirection,
            local_addr: SocketAddr,
            adjust: impl FnOnce(&mut RelayTunnelParams),
        ) -> Self {
            let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let relays = (StandIn::start(), StandIn::start());
            Self::of((TunnelProtocol::Tcp, direction), local_addr, app, relays, adjust)
        }

        /// What is not adjusted is waited for longer than any test takes, but
        /// for the settling, which is over in no time.
        fn of(
            (protocol, direction): (TunnelProtocol, TunnelDirection),
            local_addr: SocketAddr,
            app: UdpSocket,
            (primary, backup): (StandIn, StandIn),
            adjust: impl FnOnce(&mut RelayTunnelParams),
        ) -> Self {
            let mut params = RelayTunnelParams {
                tunnel_id: Uuid::new_v4(),
                relay_addrs: vec![primary.addr.clone(), backup.addr.clone()],
                direction,
                protocol,
                edge_id: None,
                tunnel_bind_secret: Some("ab".repeat(32)),
                failback_rtt_gate_ms: 5000,
                timing: Timing {
                    connect: Duration::from_secs(5),
                    waiting: Duration::from_secs(600),
                    settle: Duration::from_millis(1),
                    failback: Duration::from_secs(600),
                },
            };
            adjust(&mut params);
            let id = params.tunnel_id;
            let (state_tx, state) = watch::channel(RelayTunnelState::Connecting);
            let (active_tx, active) = watch::channel(0);
            let (event_tx, events) = event_channel();
            let cancel = CancellationToken::new();
            let (seen_tx, states) = mpsc::unbounded_channel();
            let mut seen = state.clone();
            tokio::spawn(async move {
                while seen.changed().await.is_ok() {
                    let _ = seen_tx.send(*seen.borrow_and_update());
                }
            });
            let task = tokio::spawn(run_relay_tunnel(
                params, "t".into(), direction, protocol, local_addr, state_tx, active_tx,
                cancel.clone(), None, None, None, event_tx,
            ));
            Self { id, primary, backup, states, state, active, events, cancel, app, from: None, task }
        }

        async fn becomes(&mut self, want: RelayTunnelState, on: usize) {
            soon(&format!("{want} on {on}"), async {
                while *self.state.borrow_and_update() != want {
                    self.state.changed().await.unwrap();
                }
                while *self.active.borrow_and_update() != on {
                    self.active.changed().await.unwrap();
                }
            })
            .await;
        }

        /// The messages of the next events.
        async fn raises(&mut self, messages: &[String]) {
            for message in messages {
                let event = soon(message, self.events.recv()).await.unwrap();
                assert_eq!(event.message, *message);
            }
        }

        async fn is_quiet(&mut self, time: Duration) {
            if let Ok(event) = timeout(time, self.events.recv()).await {
                panic!("{event:?}");
            }
        }

        /// No state but `want` was seen since this was last asked.
        fn was_only(&mut self, want: RelayTunnelState) {
            while let Ok(state) = self.states.try_recv() {
                assert_eq!(state, want);
            }
        }

        fn failed_over(&self) -> String {
            format!("Tunnel failed over to backup relay {}", self.backup.addr)
        }

        fn fails_back(&self) -> String {
            format!("Tunnel failing back to primary relay {}", self.primary.addr)
        }

        /// The datagrams of the tunnel pass on this session, through the
        /// forwarder there has been all along.
        async fn passes_on(&mut self, session: &Session) {
            passes(session, self.id, &self.app, &mut self.from).await;
        }

        async fn up_on_primary(&mut self) -> Session {
            let (mut session, _bind) = self.primary.binding().await;
            session.answer(ready(self.id)).await;
            self.becomes(RelayTunnelState::Ready, 0).await;
            self.raises(&[connected()]).await;
            self.passes_on(&session).await;
            session
        }

        /// The primary is not there at first.
        async fn up_on_backup(&mut self) -> Session {
            self.primary.turns_away().await;
            let (mut session, _bind) = self.backup.binding().await;
            session.answer(ready(self.id)).await;
            self.becomes(RelayTunnelState::Ready, 1).await;
            let failed = soon("an event", self.events.recv()).await.unwrap().message;
            assert!(failed.starts_with("Tunnel connection to relay failed: "), "{failed}");
            self.raises(&[connected(), self.failed_over()]).await;
            self.passes_on(&session).await;
            while self.states.try_recv().is_ok() {}
            session
        }

        async fn loses_its_peer(&mut self, session: &Session) {
            session.notify(down(self.id, GONE)).await;
            let on = *self.active.borrow();
            self.becomes(RelayTunnelState::Waiting, on).await;
            self.raises(&[gone()]).await;
        }

        /// Neither relay hears anything, and nothing is reported.
        async fn rests(&mut self, sessions: [&mut Session; 2]) {
            let [one, other] = sessions;
            let (one, other, primary, backup) = tokio::join!(
                one.is_silent(A_MOMENT),
                other.is_silent(A_MOMENT),
                self.primary.is_dialled(A_MOMENT),
                self.backup.is_dialled(A_MOMENT),
            );
            assert_eq!((one, other, primary, backup), (true, true, false, false));
            self.is_quiet(Duration::ZERO).await;
        }
    }

    fn connected() -> String {
        "Tunnel connected to relay".into()
    }

    fn gone() -> String {
        format!("Tunnel peer disconnected: {GONE}")
    }

    fn short(timing: &mut Duration) -> Duration {
        *timing = Duration::from_millis(400);
        *timing - Duration::from_millis(20)
    }

    /// Both binds are of the same side, with the same token.
    fn assert_same(one: &EdgeMessage, other: &EdgeMessage) {
        assert!(matches!(one, EdgeMessage::TunnelBind { bind_token: Some(_), .. }), "{one:?}");
        assert_eq!(serde_json::to_value(one).unwrap(), serde_json::to_value(other).unwrap());
    }

    /// Nobody is there. The edge binds its primary, and after a while its backup
    /// as well, and stays on both. The peer comes to either.
    #[tokio::test]
    async fn an_edge_that_connects_waits_on_both_relays() {
        for comes_to_backup in [false, true] {
            let mut wait = Duration::ZERO;
            let mut edge = Edge::start_with(|params| wait = short(&mut params.timing.waiting)).await;
            let id = edge.id;
            let (mut primary, bind) = edge.primary.binding().await;
            let since = Instant::now();
            primary.answer(waiting(id)).await;
            let (mut backup, other_bind) = edge.backup.binding().await;
            assert!(since.elapsed() >= wait, "{:?}", since.elapsed());
            assert_same(&bind, &other_bind);
            backup.answer(waiting(id)).await;
            edge.becomes(RelayTunnelState::Waiting, 0).await;
            // That it is said again, or that a peer came and went, changes nothing.
            primary.answer(waiting(id)).await;
            primary.notify(down(id, GONE)).await;
            backup.notify(down(id, GONE)).await;
            edge.rests([&mut primary, &mut backup]).await;
            edge.becomes(RelayTunnelState::Waiting, 0).await;

            let (on, stays, leaves) = match comes_to_backup {
                false => (0, &mut primary, &mut backup),
                true => (1, &mut backup, &mut primary),
            };
            stays.notify(ready(id)).await;
            edge.becomes(RelayTunnelState::Ready, on).await;
            edge.raises(&[connected()]).await;
            if comes_to_backup {
                let event = soon("an event", edge.events.recv()).await.unwrap();
                assert_eq!((event.severity, event.message), (EventSeverity::Warning, edge.failed_over()));
                assert_eq!(
                    event.details.unwrap(),
                    serde_json::json!({
                        "from_relay_addr": edge.primary.addr, "to_relay_addr": edge.backup.addr,
                        "from_idx": 0, "to_idx": 1,
                    })
                );
            }
            leaves.is_left().await;
            edge.passes_on(stays).await;
            assert!(stays.is_silent(A_MOMENT).await);
            edge.is_quiet(Duration::ZERO).await;
            // It connects anew where it was.
            stays.conn.close(0u32.into(), b"");
            [&edge.primary, &edge.backup][on].binding().await;
        }
    }

    /// The primary is out of reach at first, and the peer comes to it when it is
    /// back. The tunnel has never been up elsewhere.
    #[tokio::test]
    async fn an_edge_that_waits_on_its_backup_comes_to_its_primary() {
        let mut wait = Duration::ZERO;
        let mut edge = Edge::start_with(|params| {
            wait = short(&mut params.timing.waiting);
            params.timing.settle = Duration::from_secs(600);
        })
        .await;
        let id = edge.id;
        edge.primary.turns_away().await;
        let (mut backup, _bind) = edge.backup.binding().await;
        let since = Instant::now();
        backup.answer(waiting(id)).await;
        edge.becomes(RelayTunnelState::Waiting, 1).await;
        let (mut primary, _bind) = edge.primary.binding().await;
        assert!(since.elapsed() >= wait, "{:?}", since.elapsed());
        primary.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        let failed = soon("an event", edge.events.recv()).await.unwrap().message;
        assert!(failed.starts_with("Tunnel connection to relay failed: "), "{failed}");
        edge.raises(&[connected()]).await;
        backup.is_left().await;
        edge.passes_on(&primary).await;
        edge.is_quiet(A_MOMENT).await;
    }

    /// The relay an edge waits on goes while it waits on both: it tries the next
    /// one, and has let go of the other.
    #[tokio::test]
    async fn an_edge_that_loses_the_relay_it_waits_on_goes_on() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(waiting(id)).await;
        let (mut second, _bind) = edge.backup.binding().await;
        second.answer(waiting(id)).await;
        primary.conn.close(0u32.into(), b"");
        second.is_left().await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 1).await;
    }

    /// The peer of a tunnel that was up has lost its path to the primary, and is
    /// on the backup. The tunnel follows it, and the forwarder goes on.
    #[tokio::test]
    async fn a_tunnel_follows_its_peer_to_the_backup() {
        let mut wait = Duration::ZERO;
        let mut edge = Edge::start_with(|params| wait = short(&mut params.timing.waiting)).await;
        let id = edge.id;
        let mut primary = edge.up_on_primary().await;
        let since = Instant::now();
        edge.loses_its_peer(&primary).await;
        assert!(primary.is_silent(Duration::from_millis(100)).await);
        let (mut backup, _bind) = edge.backup.binding().await;
        assert!(since.elapsed() >= wait, "{:?}", since.elapsed());
        assert_eq!(*edge.state.borrow(), RelayTunnelState::Waiting);
        backup.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 1).await;
        let since = Instant::now();
        edge.raises(&[connected(), edge.failed_over()]).await;
        // There is nothing left for it to take there.
        assert!(matches!(primary.next().await, EdgeMessage::TunnelUnbind { .. }));
        assert!(since.elapsed() < carrier::DRAIN, "{:?}", since.elapsed());
        primary.closed().await;
        edge.passes_on(&backup).await;
        assert!(!edge.primary.is_dialled(A_MOMENT).await, "that is for later");
        edge.is_quiet(Duration::ZERO).await;
    }

    /// The peer comes back to the relay it has left, while the edge waits for it
    /// on both. What the edge has on the other is let go of.
    #[tokio::test]
    async fn a_peer_that_comes_back_finds_the_edge_where_it_was() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let mut primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(waiting(id)).await;
        edge.rests([&mut primary, &mut backup]).await;
        edge.becomes(RelayTunnelState::Waiting, 0).await;

        primary.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        edge.raises(&["Tunnel ready again".into()]).await;
        backup.is_left().await;
        edge.passes_on(&primary).await;
        assert!(primary.is_silent(A_MOMENT).await);

        // It is the same again when it goes again.
        edge.loses_its_peer(&primary).await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(waiting(id)).await;
        primary.notify(ready(id)).await;
        backup.is_left().await;
    }

    /// What the edge has on the other relay is let go of, though the way there is
    /// cut for a moment.
    #[tokio::test]
    async fn a_relay_is_told_that_it_is_left() {
        let mut backup = StandIn::start();
        let way = Far::to(&mut backup, 10).await;
        let mut edge = Edge::between(StandIn::start(), backup, |params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let mut primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(waiting(id)).await;
        edge.rests([&mut primary, &mut backup]).await;
        way.delay.store(CUT, Ordering::Relaxed);
        primary.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        tokio::time::sleep(A_MOMENT).await;
        way.delay.store(10, Ordering::Relaxed);
        backup.is_left().await;
    }

    /// Both edges may be looking for each other, each on the other's relay. The
    /// one on its primary stays if the peer comes there, and then only.
    #[tokio::test]
    async fn an_edge_on_its_primary_gives_the_peer_time_to_come_there() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
            params.timing.settle = Duration::from_secs(2);
        })
        .await;
        let id = edge.id;
        let mut primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        assert!(primary.is_silent(A_MOMENT).await);
        assert_eq!((*edge.state.borrow(), *edge.active.borrow()), (RelayTunnelState::Waiting, 0));
        primary.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        edge.raises(&["Tunnel ready again".into()]).await;
        backup.is_left().await;

        // One that was on the backup for a moment is not followed there. One
        // that is there again is, a whole while later.
        edge.loses_its_peer(&primary).await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        tokio::time::sleep(A_MOMENT).await;
        backup.notify(down(id, GONE)).await;
        assert!(primary.is_silent(Duration::from_secs(2)).await);
        assert_eq!((*edge.state.borrow(), *edge.active.borrow()), (RelayTunnelState::Waiting, 0));
        let since = Instant::now();
        backup.notify(ready(id)).await;
        primary.is_left().await;
        assert!(since.elapsed() >= Duration::from_millis(1950), "{:?}", since.elapsed());
        edge.becomes(RelayTunnelState::Ready, 1).await;
    }

    /// The tunnel goes back to its primary when the peer can come too. Nothing
    /// is interrupted: not its state, and no datagram, whichever way it came.
    #[tokio::test]
    async fn a_tunnel_fails_back_with_its_peer() {
        let mut wait = Duration::ZERO;
        let mut edge = Edge::start_with(|params| wait = short(&mut params.timing.failback)).await;
        let id = edge.id;
        let since = Instant::now();
        let mut backup = edge.up_on_backup().await;
        let (mut primary, _bind) = edge.primary.binding().await;
        assert!(since.elapsed() >= wait, "{:?}", since.elapsed());
        primary.answer(waiting(id)).await;
        edge.rests([&mut primary, &mut backup]).await;
        edge.passes_on(&backup).await;

        let since = Instant::now();
        primary.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        let event = soon("an event", edge.events.recv()).await.unwrap();
        assert_eq!((event.severity, event.message), (EventSeverity::Info, edge.fails_back()));
        let details = event.details.unwrap();
        assert_eq!((&details["primary_relay_addr"], &details["gate_ms"]), (&edge.primary.addr[..].into(), &5000.into()));
        assert!(details["primary_rtt_ms"].is_u64() && details["active_rtt_ms"].is_u64());
        edge.raises(&[connected()]).await;
        edge.passes_on(&primary).await;
        // What the peer sent before it moved.
        let late = protocol::encode_udp_datagram(&id, b"late");
        backup.conn.send_datagram(late.into()).unwrap();
        let mut buf = [0; 8];
        let (n, _) = soon("what came late", edge.app.recv_from(&mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"late");

        // The peer is not told that we leave before it has moved itself.
        backup.notify(down(id, "peer unbound")).await;
        assert!(matches!(backup.next().await, EdgeMessage::TunnelUnbind { .. }));
        assert!(since.elapsed() >= carrier::DRAIN, "{:?}", since.elapsed());
        backup.closed().await;
        edge.passes_on(&primary).await;
        edge.was_only(RelayTunnelState::Ready);
        assert!(primary.is_silent(A_MOMENT).await);
        assert!(!edge.backup.is_dialled(A_MOMENT).await);
        edge.is_quiet(Duration::ZERO).await;
    }

    /// Not on the spot: the peer may have left the primary, and the edge that is
    /// told so stays where it is.
    #[tokio::test]
    async fn a_tunnel_does_not_fail_back_to_where_its_peer_has_been() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.failback);
            params.timing.settle = Duration::from_secs(2);
        })
        .await;
        let id = edge.id;
        let mut backup = edge.up_on_backup().await;
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(ready(id)).await;
        tokio::time::sleep(A_MOMENT).await;
        primary.notify(down(id, GONE)).await;
        assert!(backup.is_silent(Duration::from_secs(2)).await);
        edge.rests([&mut primary, &mut backup]).await;
        assert_eq!(*edge.active.borrow(), 1);
        let since = Instant::now();
        primary.notify(ready(id)).await;
        // That it is said again does not start the wait again.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        primary.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        let waited = since.elapsed();
        assert!((1_950..3_400).contains(&waited.as_millis()), "{waited:?}");
        edge.raises(&[edge.fails_back(), connected()]).await;
        edge.was_only(RelayTunnelState::Ready);
    }

    /// A tunnel whose peer leaves the backup finds it on the primary, however
    /// far that is.
    #[tokio::test]
    async fn a_tunnel_follows_its_peer_to_the_primary() {
        let mut wait = Duration::ZERO;
        let mut edge = Edge::start_with(|params| {
            wait = short(&mut params.timing.waiting);
            params.timing.settle = Duration::from_secs(600);
            params.failback_rtt_gate_ms = 0;
        })
        .await;
        let id = edge.id;
        let mut backup = edge.up_on_backup().await;
        let since = Instant::now();
        edge.loses_its_peer(&backup).await;
        let (mut primary, _bind) = edge.primary.binding().await;
        assert!(since.elapsed() >= wait, "{:?}", since.elapsed());
        primary.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        edge.raises(&[edge.fails_back(), connected()]).await;
        backup.is_left().await;
        edge.passes_on(&primary).await;
    }

    /// A primary that is too far is not bound: the peer would be told to come.
    /// It is asked again later, and bound when it is near.
    #[tokio::test]
    async fn a_primary_that_is_too_far_is_not_gone_back_to() {
        let mut primary = StandIn::start();
        let far = Far::to(&mut primary, 150).await;
        let mut edge = Edge::between(primary, StandIn::start(), |params| {
            params.timing.failback = Duration::from_secs(1);
            params.failback_rtt_gate_ms = 50;
        })
        .await;
        let id = edge.id;
        let mut backup = edge.up_on_backup().await;
        for _ in 0..2 {
            let since = Instant::now();
            let mut session = edge.primary.session().await;
            session.closed().await;
            assert!(since.elapsed() >= Duration::from_millis(700), "{:?}", since.elapsed());
            while let Ok(msg) = protocol::read_message(&mut session.recv).await {
                assert!(matches!(msg, EdgeMessage::Hello { .. }), "{msg:?}");
            }
        }
        far.delay.store(0, Ordering::Relaxed);
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(waiting(id)).await;
        edge.rests([&mut primary, &mut backup]).await;
        edge.was_only(RelayTunnelState::Ready);
    }

    /// One that becomes too far while the edge waits there is let go of again.
    #[tokio::test]
    async fn a_primary_that_becomes_too_far_is_let_go_of() {
        let (mut primary, mut backup) = (StandIn::start(), StandIn::start());
        let _far = Far::to(&mut primary, 100).await;
        let near = Far::to(&mut backup, 100).await;
        let mut edge = Edge::between(primary, backup, |params| {
            short(&mut params.timing.failback);
            params.failback_rtt_gate_ms = 100;
        })
        .await;
        let id = edge.id;
        let backup = edge.up_on_backup().await;
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(waiting(id)).await;
        // Now the backup is nearer than it was, which the edge learns from what
        // it sends there.
        near.delay.store(0, Ordering::Relaxed);
        let to = edge.from.unwrap();
        let flood = async {
            loop {
                edge.app.send_to(b"x", to).await.unwrap();
                let _ = timeout(Duration::from_millis(10), backup.conn.read_datagram()).await;
            }
        };
        tokio::select! {
            _ = flood => {}
            _ = primary.is_left() => {}
        }
        assert_eq!((*edge.state.borrow(), *edge.active.borrow()), (RelayTunnelState::Ready, 1));
    }

    /// Nor is one that the edge has bound while it waited, but not on the spot:
    /// the peer may be on the backup for a moment only, and on its way there.
    #[tokio::test]
    async fn a_primary_that_is_too_far_is_let_go_of_once_the_peer_stays() {
        let mut primary = StandIn::start();
        let _far = Far::to(&mut primary, 100).await;
        let mut edge = Edge::between(primary, StandIn::start(), |params| {
            short(&mut params.timing.waiting);
            params.timing.settle = Duration::from_millis(1500);
            params.failback_rtt_gate_ms = 50;
        })
        .await;
        let id = edge.id;
        let backup = edge.up_on_backup().await;
        edge.loses_its_peer(&backup).await;
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(waiting(id)).await;
        let since = Instant::now();
        backup.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 1).await;
        edge.raises(&["Tunnel ready again".into()]).await;
        primary.is_left().await;
        assert!(since.elapsed() >= Duration::from_millis(1500), "{:?}", since.elapsed());
        assert_eq!((*edge.state.borrow(), *edge.active.borrow()), (RelayTunnelState::Ready, 1));
    }

    /// A relay that refuses the second bind is an alarm once. The tunnel goes on
    /// where it is, and asks again.
    #[tokio::test]
    async fn an_other_relay_that_refuses_is_reported_once() {
        let mut wait = Duration::ZERO;
        let mut edge = Edge::start_with(|params| wait = short(&mut params.timing.failback)).await;
        let id = edge.id;
        let backup = edge.up_on_backup().await;
        let mut since = Instant::now();
        for attempt in 0..3 {
            let (mut primary, _bind) = edge.primary.binding().await;
            assert!(attempt == 0 || since.elapsed() >= wait, "{:?}", since.elapsed());
            since = Instant::now();
            primary.answer(down(id, REFUSED)).await;
            if attempt == 0 {
                let event = soon("an event", edge.events.recv()).await.unwrap();
                let rejected = format!("Tunnel bind rejected by relay: {REFUSED}");
                assert_eq!((event.severity, event.message), (EventSeverity::Critical, rejected));
                assert_eq!(event.details.unwrap()["relay_addr"], edge.primary.addr);
            }
            primary.closed().await;
            // It has nothing to let go of.
            assert!(protocol::read_message::<EdgeMessage>(&mut primary.recv).await.is_err());
        }
        edge.passes_on(&backup).await;
        edge.was_only(RelayTunnelState::Ready);
        edge.is_quiet(Duration::ZERO).await;
    }

    /// The same of an edge that connects: one alarm for a relay, however it was
    /// tried.
    #[tokio::test]
    async fn a_relay_that_refuses_the_first_bind_and_the_second_is_reported_once() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(down(id, REFUSED)).await;
        edge.raises(&[format!("Tunnel bind rejected by relay: {REFUSED}")]).await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(waiting(id)).await;
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(down(id, REFUSED)).await;
        primary.closed().await;
        edge.is_quiet(A_MOMENT).await;
    }

    /// The connection to the other relay ends. It is made again.
    #[tokio::test]
    async fn an_other_relay_that_is_lost_is_bound_again() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let mut primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let mut since = Instant::now();
        for wait in [0, 1] {
            let (mut backup, _bind) = edge.backup.binding().await;
            assert!(since.elapsed() >= Duration::from_secs(wait), "{:?}", since.elapsed());
            backup.answer(waiting(id)).await;
            since = Instant::now();
            backup.conn.close(0u32.into(), b"");
        }
        assert!(primary.is_silent(A_MOMENT).await);
        edge.is_quiet(Duration::ZERO).await;
    }

    /// The peer comes back while the edge dials the other relay. It is not bound
    /// there.
    #[tokio::test]
    async fn a_peer_that_comes_back_in_time_stops_the_bind() {
        let mut backup = StandIn::start();
        let _far = Far::to(&mut backup, 600).await;
        let mut edge = Edge::between(StandIn::start(), backup, |params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        // 400 ms of waiting, and then 1200 ms for the handshake.
        tokio::time::sleep(Duration::from_millis(600)).await;
        primary.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        let mut session = edge.backup.session().await;
        session.closed().await;
        while let Ok(msg) = protocol::read_message(&mut session.recv).await {
            assert!(matches!(msg, EdgeMessage::Hello { .. }), "{msg:?}");
        }
    }

    /// With one relay there is nowhere else to look.
    #[tokio::test]
    async fn a_tunnel_with_one_relay_stays_where_it_is() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
            params.relay_addrs.truncate(1);
        })
        .await;
        let id = edge.id;
        let mut primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let (said, dialled, other) = tokio::join!(
            primary.is_silent(Duration::from_secs(1)),
            edge.primary.is_dialled(Duration::from_secs(1)),
            edge.backup.is_dialled(Duration::from_secs(1)),
        );
        assert_eq!((said, dialled, other), (true, false, false));
        primary.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
    }

    /// Our side is removed on the other relay. It is bound there again later,
    /// and what the tunnel is on is not touched.
    #[tokio::test]
    async fn a_side_removed_on_the_other_relay_is_bound_again() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let mut primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let mut since = Instant::now();
        for wait in [0, 1, 2] {
            let (mut backup, _bind) = edge.backup.binding().await;
            let waited = since.elapsed().as_secs_f64();
            assert!(waited >= f64::from(wait) - 0.02, "{waited} for {wait}");
            backup.answer(waiting(id)).await;
            since = Instant::now();
            backup.notify(down(id, "tunnel closed by manager")).await;
            backup.closed().await;
        }
        assert!(primary.is_silent(A_MOMENT).await);
        edge.is_quiet(Duration::ZERO).await;
        // The peer is back and goes again: the wait starts over.
        primary.notify(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        edge.raises(&["Tunnel ready again".into()]).await;
        edge.loses_its_peer(&primary).await;
        edge.backup.binding().await;
        assert!(since.elapsed() < Duration::from_secs(4), "{:?}", since.elapsed());
    }

    /// Another relay that cannot be reached is tried more and more seldom, and
    /// nothing is raised for it.
    #[tokio::test]
    async fn an_other_relay_that_is_not_there_is_tried_seldom() {
        let nobody = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut edge = Edge::start_with(|params| {
            params.timing.connect = Duration::from_millis(500);
            short(&mut params.timing.waiting);
            params.relay_addrs[1] = nobody.local_addr().unwrap().to_string();
        })
        .await;
        let primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let mut dials: Vec<(SocketAddr, Instant)> = Vec::new();
        let mut buf = [0; 2048];
        while dials.len() < 3 {
            let (_, from) = soon("a dial", nobody.recv_from(&mut buf)).await.unwrap();
            if dials.last().is_none_or(|(last, _)| *last != from) {
                dials.push((from, Instant::now()));
            }
        }
        // Half a second for each try, and then 1 s and 2 s.
        for (n, wait) in [(0, 1.2), (1, 2.2)] {
            let waited = (dials[n + 1].1 - dials[n].1).as_secs_f64();
            assert!(waited >= wait, "{n}: {waited}");
        }
        edge.is_quiet(Duration::ZERO).await;
    }

    /// A side that is removed on the relay the tunnel is on is bound there
    /// again. It is not a peer that is away.
    #[tokio::test]
    async fn a_removed_side_is_not_looked_for_elsewhere() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let mut primary = edge.up_on_primary().await;
        primary.notify(down(id, "tunnel closed by manager")).await;
        edge.becomes(RelayTunnelState::Down, 0).await;
        let (_bind, dialled) = tokio::join!(primary.next(), edge.backup.is_dialled(Duration::from_secs(1)));
        assert!(!dialled);
        primary.answer(waiting(id)).await;
        edge.becomes(RelayTunnelState::Waiting, 0).await;
        edge.backup.binding().await;
    }

    /// Stopped, the edge lets go of both relays.
    #[tokio::test]
    async fn a_tunnel_that_is_stopped_lets_go_of_both_relays() {
        for up in [false, true] {
            let mut edge = Edge::start_with(|params| {
                short(&mut params.timing.waiting);
            })
            .await;
            let id = edge.id;
            let primary = if up {
                let primary = edge.up_on_primary().await;
                edge.loses_its_peer(&primary).await;
                primary
            } else {
                let (mut primary, _bind) = edge.primary.binding().await;
                primary.answer(waiting(id)).await;
                primary
            };
            let (mut backup, _bind) = edge.backup.binding().await;
            backup.answer(waiting(id)).await;
            edge.becomes(RelayTunnelState::Waiting, 0).await;
            edge.cancel.cancel();
            primary.closed().await;
            backup.is_left().await;
            let _ = soon("its end", &mut edge.task).await.unwrap();
        }
    }

    /// The connection the tunnel has moved to ends: the edge connects anew,
    /// where it was.
    #[tokio::test]
    async fn a_tunnel_that_has_moved_reconnects_where_it_is() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        edge.raises(&[connected(), edge.failed_over()]).await;
        edge.passes_on(&backup).await;
        // The one it has left is nothing to it any more.
        primary.conn.close(0u32.into(), b"");
        edge.passes_on(&backup).await;

        backup.conn.close(0u32.into(), b"");
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 1).await;
        let lost = soon("an event", edge.events.recv()).await.unwrap().message;
        assert!(lost.starts_with("Tunnel disconnected from relay: "), "{lost}");
        edge.raises(&[connected()]).await;
        edge.is_quiet(A_MOMENT).await;
    }

    /// A moved tunnel is held where it is: what the relay says there counts.
    #[tokio::test]
    async fn a_tunnel_that_has_moved_is_held_there() {
        let mut edge = Edge::start_with(|params| {
            short(&mut params.timing.waiting);
        })
        .await;
        let id = edge.id;
        let primary = edge.up_on_primary().await;
        edge.loses_its_peer(&primary).await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        edge.raises(&[connected(), edge.failed_over()]).await;
        backup.notify(down(id, "tunnel closed by manager")).await;
        edge.becomes(RelayTunnelState::Down, 1).await;
        let event = soon("an event", edge.events.recv()).await.unwrap();
        assert_eq!(event.message, "Tunnel removed on relay: tunnel closed by manager");
        assert_eq!(event.details.unwrap()["relay_addr"], edge.backup.addr);
        backup.next().await;
        backup.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 1).await;
        edge.passes_on(&backup).await;
    }

    async fn stream_on(session: &Session, id: Uuid) -> (quinn::SendStream, quinn::RecvStream) {
        let (mut send, recv) = session.conn.open_bi().await.unwrap();
        let header = protocol::StreamHeader { tunnel_id: id, stream_type: protocol::StreamType::Tcp };
        protocol::write_stream_header(&mut send, &header).await.unwrap();
        (send, recv)
    }

    async fn echoes(
        (send, recv): &mut (quinn::SendStream, quinn::RecvStream),
        tcp: &mut TcpStream,
        n: u8,
    ) {
        send.write_all(&[n]).await.unwrap();
        assert_eq!(soon("its data", tcp.read_u8()).await.unwrap(), n);
        tcp.write_all(&[n + 100]).await.unwrap();
        let mut back = [0];
        soon("its answer", recv.read_exact(&mut back)).await.unwrap();
        assert_eq!(back, [n + 100]);
    }

    /// The TCP connections that are open when the tunnel moves go on where they
    /// are, whichever edge opened them. Those opened after are on the new relay.
    #[tokio::test]
    async fn a_tcp_connection_outlives_a_move() {
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut edge = Edge::tcp(TunnelDirection::Ingress, app.local_addr().unwrap(), |params| {
            short(&mut params.timing.failback);
        })
        .await;
        let id = edge.id;
        edge.primary.turns_away().await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        let mut stream = stream_on(&backup, id).await;
        let (mut tcp, _) = soon("a connection", app.accept()).await.unwrap();
        echoes(&mut stream, &mut tcp, 1).await;

        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        echoes(&mut stream, &mut tcp, 2).await;
        let mut on_primary = stream_on(&primary, id).await;
        let (mut tcp_there, _) = soon("a connection", app.accept()).await.unwrap();
        echoes(&mut on_primary, &mut tcp_there, 3).await;
        // One the peer opens before it has moved.
        let mut late = stream_on(&backup, id).await;
        let (mut tcp_late, _) = soon("a connection", app.accept()).await.unwrap();
        echoes(&mut late, &mut tcp_late, 4).await;

        assert!(matches!(backup.next().await, EdgeMessage::TunnelUnbind { .. }));
        tokio::time::sleep(A_MOMENT).await;
        echoes(&mut stream, &mut tcp, 5).await;
        echoes(&mut late, &mut tcp_late, 6).await;
        assert!(backup.conn.close_reason().is_none());
        drop((stream, late));
        backup.closed().await;
        echoes(&mut on_primary, &mut tcp_there, 7).await;
    }

    /// They end with the forwarder they go through, like those on the relay the
    /// tunnel is on.
    #[tokio::test]
    async fn a_tcp_connection_on_the_relay_left_ends_with_the_tunnels_connection() {
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut edge = Edge::tcp(TunnelDirection::Ingress, app.local_addr().unwrap(), |params| {
            short(&mut params.timing.failback);
        })
        .await;
        let id = edge.id;
        edge.primary.turns_away().await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        let mut stream = stream_on(&backup, id).await;
        let (mut tcp, _) = soon("a connection", app.accept()).await.unwrap();
        echoes(&mut stream, &mut tcp, 1).await;
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        assert!(matches!(backup.next().await, EdgeMessage::TunnelUnbind { .. }));
        echoes(&mut stream, &mut tcp, 2).await;
        primary.conn.close(0u32.into(), b"");
        assert_eq!(soon("its end", tcp.read(&mut [0])).await.unwrap_or(0), 0);
    }

    #[tokio::test]
    async fn a_tcp_connection_is_opened_on_the_relay_the_tunnel_has_moved_to() {
        let local_addr = {
            let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
            free.local_addr().unwrap()
        };
        let mut edge = Edge::tcp(TunnelDirection::Egress, local_addr, |params| {
            short(&mut params.timing.failback);
        })
        .await;
        let id = edge.id;
        edge.primary.turns_away().await;
        let (mut backup, _bind) = edge.backup.binding().await;
        backup.answer(ready(id)).await;
        let connect = async || {
            soon("the listener", async {
                loop {
                    if let Ok(tcp) = TcpStream::connect(local_addr).await {
                        return tcp;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
        };
        let opened = async |session: &Session| {
            let (send, mut recv) = soon("a stream", session.conn.accept_bi()).await.unwrap();
            assert_eq!(protocol::read_stream_header(&mut recv).await.unwrap().tunnel_id, id);
            (send, recv)
        };
        let mut tcp = connect().await;
        // A stream is there for its other end once something is on it.
        let mut stream = opened(&backup).await;
        echoes(&mut stream, &mut tcp, 1).await;
        let (mut primary, _bind) = edge.primary.binding().await;
        primary.answer(ready(id)).await;
        edge.becomes(RelayTunnelState::Ready, 0).await;
        let mut tcp_there = connect().await;
        let mut on_primary = opened(&primary).await;
        echoes(&mut on_primary, &mut tcp_there, 2).await;
        echoes(&mut stream, &mut tcp, 3).await;
    }
}
