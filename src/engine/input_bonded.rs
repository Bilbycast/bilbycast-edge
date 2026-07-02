// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Bonded input.
//!
//! Binds N bond paths (UDP / QUIC / RIST) via
//! [`bonding_transport::BondSocket::receiver`], reassembles the
//! bond-sequenced payloads in order, and publishes each delivered
//! datagram as `RtpPacket { is_raw_ts: true, .. }` onto the flow's
//! per-input broadcast channel. Bonding-layer NACK recovery, keepalive
//! liveness, and per-path stats are handled inside the `BondSocket`.
//!
//! Downstream consumers see a single ordered stream of payloads —
//! exactly like the UDP and RIST inputs. No per-packet overhead
//! beyond the existing broadcast-channel hop.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use bonding_transport::{
    BondSocket, BondSocketConfig, FecParams, PathConfig as BondPathTxCfg,
    PathTransport as BondPathTxTransport, PerLegFecKind, QuicRole as BondQuicRoleTx,
    QuicTlsMode as BondQuicTlsTx, RistRole as BondRistRoleTx,
};

use crate::config::models::{
    BondEqualizationMode, BondFecAlgorithm, BondFecConfig, BondPathConfig, BondPathTransportConfig,
    BondQuicTls, BondRistRole, BondedInputConfig,
};
use crate::tunnel::config::TunnelDirection;
use crate::tunnel::crypto::TunnelCipher;
use crate::tunnel::udp_forwarder::UdpForwarderStats;
use crate::tunnel::udp_relay_client::NativeRelayParams;

use bonding_transport::{AttachedBridgeEnds, AttachedChannels};

/// Synthetic placeholder peer for an in-process **attached** (relay) bond leg.
/// The edge bridge owns the single real relay peer, so `AttachedPath::send_to`
/// ignores its `to` arg — but `primary_peer` must be `Some(_)` or the bond
/// drops the leg from aggregation (see `bonding-transport` `sender.rs`). The
/// value is never put on a wire; `127.0.0.1:9` (discard) is a stable sentinel.
pub(crate) const SYNTHETIC_ATTACHED_PEER: &str = "127.0.0.1:9";

/// Output of [`build_receiver_cfg`] / [`build_sender_cfg`]: the bond socket
/// config plus, for any relay legs, the in-process bridge ends + the path-side
/// attachment channels keyed by `PathId`. The caller spawns one
/// `run_native_relay_leg_inproc` per [`RelayLegBridge`] and hands `attachments`
/// to `BondSocket::{receiver,sender}_attached`.
pub(crate) struct BondBuild {
    pub cfg: BondSocketConfig,
    pub attachments: std::collections::HashMap<u8, AttachedChannels>,
    pub bridges: Vec<RelayLegBridge>,
}

/// One relay leg's bridge setup — everything `run_native_relay_leg_inproc`
/// needs, paired with the leg name for logging.
pub(crate) struct RelayLegBridge {
    pub params: NativeRelayParams,
    /// Conditional tunnel AEAD: `Some` only when the bond is UNKEYED (the leg's
    /// `tunnel_encryption_key` is the single layer); `None` when the bond's own
    /// `0xBD` key is the single layer (the attached path seals it).
    pub cipher: Option<Arc<TunnelCipher>>,
    pub stats: Arc<UdpForwarderStats>,
    pub bridge: AttachedBridgeEnds,
    pub leg_name: String,
}

/// Build the bridge + attachment channels for one relay leg, and the
/// `Attached` path-transport that goes into the bond socket config.
///
/// `bond_has_key` is whether the bond carries its own `encryption_key`. This is
/// the **fail-closed conditional-AEAD decision** — and it MUST be identical on
/// the output (sender) and input (receiver) legs (validation enforces the
/// single-layer rule on both ends together): bond-keyed ⇒ the bridge cipher is
/// `None` and the attached path seals `0xBD`; bond-unkeyed ⇒ the bridge cipher
/// is the leg's `TunnelCipher` and the attached path is plaintext.
pub(crate) fn prepare_relay_leg(
    p: &BondPathConfig,
    bond_has_key: bool,
    direction: TunnelDirection,
) -> anyhow::Result<(AttachedChannels, RelayLegBridge, BondPathTxTransport)> {
    let BondPathTransportConfig::Relay {
        tunnel_id,
        relay_addrs,
        tunnel_bind_secret,
        tunnel_encryption_key,
        interface,
        source,
        gateway,
    } = &p.transport
    else {
        anyhow::bail!("internal: prepare_relay_leg called on a non-relay leg");
    };

    let tunnel_uuid = uuid::Uuid::parse_str(tunnel_id.trim())
        .map_err(|e| anyhow::anyhow!("relay bond leg '{}': invalid tunnel_id: {e}", p.name))?;
    if relay_addrs.is_empty() {
        anyhow::bail!("relay bond leg '{}': at least one relay address required", p.name);
    }

    // Conditional cipher — fail-closed single-layer (see fn docs). Validation
    // already rejected "neither" and "both"; this mirrors that decision.
    let cipher = if bond_has_key {
        None
    } else {
        let key = tunnel_encryption_key.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "relay bond leg '{}': bond is unencrypted and the leg has no \
                 tunnel_encryption_key (would blackhole — validation should have caught this)",
                p.name
            )
        })?;
        Some(Arc::new(TunnelCipher::new(key.trim()).map_err(|e| {
            anyhow::anyhow!("relay bond leg '{}': invalid tunnel_encryption_key: {e}", p.name)
        })?))
    };

    let (att, ends) = AttachedChannels::new();
    let params = NativeRelayParams {
        tunnel_id: tunnel_uuid,
        relay_addrs: relay_addrs.clone(),
        direction,
        tunnel_bind_secret: tunnel_bind_secret.clone(),
        interface: interface.clone(),
        source: source.clone(),
        gateway: gateway.clone(),
    };
    let bridge = RelayLegBridge {
        params,
        cipher,
        stats: Arc::new(UdpForwarderStats::default()),
        bridge: ends,
        leg_name: p.name.clone(),
    };
    let synthetic = parse_sockaddr(SYNTHETIC_ATTACHED_PEER)?;
    Ok((att, bridge, BondPathTxTransport::Attached { primary_peer: synthetic }))
}

/// Spawn one in-process relay bridge per relay leg under a child of `cancel`.
/// Shared by the bonded input (ingress legs) and output (egress legs). Each
/// bridge drains `from_bond` → relay and pumps relay → `to_bond`, drop-on-full
/// both directions; it self-redials the relay on failover without tearing down
/// the leg channels.
pub(crate) fn spawn_relay_leg_bridges(bridges: Vec<RelayLegBridge>, cancel: &CancellationToken) {
    for b in bridges {
        let RelayLegBridge {
            params,
            cipher,
            stats,
            bridge,
            leg_name,
        } = b;
        let AttachedBridgeEnds { to_bond, from_bond } = bridge;
        let leg_cancel = cancel.child_token();
        tokio::spawn(async move {
            if let Err(e) = crate::tunnel::udp_relay_client::run_native_relay_leg_inproc(
                params, leg_cancel, stats, cipher, to_bond, from_bond,
            )
            .await
            {
                tracing::warn!("relay bond leg '{leg_name}' bridge exited: {e}");
            }
        });
    }
}

/// Equalization latency budget (= the bonding-latency knob) applied when a
/// bonded input/output enables equalization but leaves both
/// `max_bonding_latency_ms` and `hold_max_ms` unset. Generous on purpose: it
/// must cover the worst-case inter-leg one-way-delay skew the equalizer aligns
/// out (a Starlink or slow-5G leg vs. a fast cellular leg). Matches the bonding
/// layer's default `max_bonding_latency_us` (1 s) so the receiver's alignment
/// budget and the sender's demote budget agree by default.
pub(crate) const DEFAULT_EQUALIZATION_BUDGET_MS: u64 = 1000;

/// Map the edge's tri-state [`BondEqualizationMode`] (absent → `auto`) to the
/// bonding layer's [`bonding_transport::EqualizationMode`]. Shared by the
/// bonded input (receiver) and output (sender) builders.
pub(crate) fn map_equalization_mode(
    m: Option<BondEqualizationMode>,
) -> bonding_transport::EqualizationMode {
    use bonding_transport::EqualizationMode as Tx;
    match m.unwrap_or_default() {
        BondEqualizationMode::Auto => Tx::Auto,
        BondEqualizationMode::Off => Tx::Off,
        BondEqualizationMode::On => Tx::On,
    }
}

/// Map an edge per-leg [`BondFecConfig`] to the bonding layer's
/// [`PerLegFecKind`] — shared by the bonded input + output builders.
pub(crate) fn build_per_leg_fec(f: &BondFecConfig) -> PerLegFecKind {
    match f.algorithm.unwrap_or_default() {
        BondFecAlgorithm::Xor => PerLegFecKind::Xor(FecParams {
            columns: f.columns,
            rows: f.rows,
        }),
        BondFecAlgorithm::ReedSolomon => PerLegFecKind::ReedSolomon {
            data: f.columns,
            parity: f.rows,
            // parity_max defaults to the parity floor (= fixed).
            parity_max: f.parity_max.unwrap_or(f.rows).max(f.rows),
        },
    }
}
use crate::manager::events::{
    BondEventScope, EventSender, EventSeverity, category, run_bond_event_forwarder,
};
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::time::now_us;

use super::packet::RtpPacket;

const TS_PACKET_SIZE: usize = 188;

/// Spawn a bonded input task. Produces `RtpPacket { is_raw_ts: true, .. }`.
pub fn spawn_bonded_input(
    config: BondedInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Register this input's legs as "in use" (held for the task's
        // life) so a per-leg capacity probe won't saturate a link this
        // flow is receiving on. See engine::bond_leg_probe.
        let _active_legs = super::bond_leg_probe::register_active_legs(
            &flow_id,
            super::bond_leg_probe::leg_keys_for_paths(&config.paths),
        );

        let build = match build_receiver_cfg(&config) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("bonded input '{}': config translation failed: {}", input_id, e);
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::MEDIA,
                    format!("bonded input config invalid: {e}"),
                    &flow_id,
                );
                return;
            }
        };
        let BondBuild {
            cfg: socket_cfg,
            attachments,
            bridges,
        } = build;

        // Spawn one in-process relay bridge per relay leg BEFORE the socket
        // binds — each owns its relay socket (Register/keepalive + failover)
        // and pumps the attached leg's channels with no loopback hop. The
        // bridge task lives under the input's cancel token; relay rotation
        // re-Registers on a fresh socket but the leg channels persist.
        spawn_relay_leg_bridges(bridges, &cancel);

        let socket = match BondSocket::receiver_attached(socket_cfg, attachments).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("bonded input '{}': bind failed: {}", input_id, e);
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::MEDIA,
                    format!("bonded input bind failed: {e}"),
                    &flow_id,
                );
                return;
            }
        };

        // Collect per-path stats handles so the accumulator snapshot
        // path can surface `BondLegStats.paths[…].rtt_ms`, loss,
        // keepalive liveness, etc. The operator-facing `name` and
        // `transport` labels come from the config so the UI doesn't
        // need to re-read it.
        let mut path_handles: Vec<crate::stats::collector::BondPathStatsHandle> =
            Vec::with_capacity(config.paths.len());
        for p in &config.paths {
            if let Some(ps) = socket.path_stats(p.id) {
                // Receiver legs bind a listening port (inbound only) — there is
                // no meaningful egress interface, so leave it `None` (the
                // cellular signal join is a sender-side concern). See the
                // BondPathStatsHandle.interface contract.
                path_handles.push(
                    crate::stats::collector::BondPathStatsHandle::new(
                        p.id,
                        p.name.clone(),
                        bond_transport_label(&p.transport),
                        ps,
                    )
                    .with_tunnel_id(bond_path_tunnel_id(&p.transport)),
                );
            }
        }
        stats.set_input_bond_stats(crate::stats::collector::BondStatsHandle {
            flow_id: config.bond_flow_id,
            role: crate::stats::collector::BondStatsRole::Receiver,
            scheduler: String::new(), // receivers don't schedule
            conn_stats: socket.stats(),
            paths: path_handles,
            // Sender-side counter; stays 0 on the receiver.
            oversize_payloads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });

        tracing::info!(
            "bonded input '{}' up — {} path(s), bond_flow_id={}",
            input_id,
            config.paths.len(),
            config.bond_flow_id
        );
        event_sender.emit_flow(
            EventSeverity::Info,
            category::MEDIA,
            format!(
                "bonded input listening: {} path(s), bond_flow_id={}",
                config.paths.len(),
                config.bond_flow_id
            ),
            &flow_id,
        );

        // Forward bonding lifecycle events (per-path alive/dead +
        // bond-aggregate transitions) to the manager event feed.
        tokio::spawn(run_bond_event_forwarder(
            socket.subscribe_events(),
            event_sender.clone(),
            flow_id.clone(),
            BondEventScope::Input,
            input_id.clone(),
            cancel.clone(),
        ));

        // A bonded input is deliberately NOT routed through the rate-paced
        // ingress de-jitter servo. The bond reassembler releases in HOL-gated
        // bursts (a straggler on a slow leg stalls the head, then a clump
        // flushes up to the hold budget — potentially seconds). That servo is
        // sized for small UDP PDV with only ±5% rate authority, so it cannot
        // drain a multi-second clump: the buffer backs up and the residence-cap
        // *sheds* 15–30% of the bond's media (measured `ingress_dejitter_shed`
        // in the thousands; delivery down to ~64–82%). The bond already delivers
        // in order, and output pacing is handled losslessly downstream — the
        // egress wire-emit servo (UDP/RTP) and the display A/V clock (HDMI) both
        // re-time the bursty stream. So we pass it through directly. If an
        // operator set `ingress_dejitter_ms` on a bonded input, it is ignored
        // (it would only shed) — warn and point them at hold/latency knobs.
        if config.ingress_dejitter_ms.filter(|v| *v > 0).is_some() {
            tracing::warn!(
                "bonded input '{}': ingress_dejitter_ms is IGNORED on a bonded input — the \
                 rate-paced de-jitter servo would shed the bond's bursty delivery (15–30% loss); \
                 the bond reassembly + egress/display pacing already smooth the stream. Use \
                 hold_ms / max_bonding_latency_ms to control bond latency.",
                input_id
            );
            event_sender.emit_flow_with_details(
                EventSeverity::Warning,
                category::BOND,
                format!(
                    "ingress_dejitter_ms ignored on bonded input '{input_id}' (would shed bond bursts)"
                ),
                &flow_id,
                serde_json::json!({
                    "error_code": "bond_ingress_dejitter_ignored",
                    "input_id": input_id.clone(),
                }),
            );
        }
        // Direct passthrough — no rate-paced de-jitter on a bonded input (see above).
        let publisher = crate::engine::ingress_publisher::IngressPublisher::new(
            None,
            None,
            broadcast_tx,
            &input_id,
            cancel.clone(),
            stats.clone(),
        );

        let mut seq_counter: u16 = 0;
        let mut ts_counter: u32 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("bonded input '{}' stopping (cancelled)", input_id);
                    socket.close();
                    return;
                }
                recv = socket.recv() => match recv {
                    Some(payload) => {
                        publish(
                            &payload,
                            &publisher,
                            &stats,
                            &mut seq_counter,
                            &mut ts_counter,
                        );
                    }
                    None => {
                        tracing::info!("bonded input '{}' recv channel closed", input_id);
                        return;
                    }
                },
            }
        }
    })
}

fn publish(
    data: &Bytes,
    publisher: &crate::engine::ingress_publisher::IngressPublisher,
    stats: &Arc<FlowStatsAccumulator>,
    seq_counter: &mut u16,
    ts_counter: &mut u32,
) {
    let seq = *seq_counter;
    *seq_counter = seq_counter.wrapping_add(1);
    let ts_pkts = (data.len() / TS_PACKET_SIZE) as u32;
    *ts_counter = ts_counter.wrapping_add(ts_pkts.max(1) * 188 * 8);

    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let packet = RtpPacket {
        data: data.clone(),
        sequence_number: seq,
        rtp_timestamp: *ts_counter,
        recv_time_us: now_us(),
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
    };
    if !publisher.send(packet) {
        // De-jitter / delay drainer mpsc full — drop rather than block the
        // bond receive loop (bounded-buffer overflow, surfaced via the
        // de-jitter stage's own shed telemetry).
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
    }
}

/// Translate an edge `BondedInputConfig` into a
/// `bonding_transport::BondSocketConfig` for the receiver side.
pub(crate) fn build_receiver_cfg(cfg: &BondedInputConfig) -> anyhow::Result<BondBuild> {
    let mut out = BondSocketConfig {
        flow_id: cfg.bond_flow_id,
        paths: Vec::with_capacity(cfg.paths.len()),
        ..Default::default()
    };
    let bond_has_key = cfg.encryption_key.is_some();
    let mut attachments: std::collections::HashMap<u8, AttachedChannels> =
        std::collections::HashMap::new();
    let mut bridges: Vec<RelayLegBridge> = Vec::new();
    if let Some(ms) = cfg.hold_ms {
        out.hold_time = Duration::from_millis(ms as u64);
    }
    if let Some(ms) = cfg.hold_max_ms {
        out.hold_max = Some(Duration::from_millis(ms as u64));
    }
    if let Some(ms) = cfg.keepalive_ms {
        out.keepalive_interval = Duration::from_millis(ms as u64);
    }
    if let Some(ms) = cfg.nack_delay_ms {
        out.nack_delay = Duration::from_millis(ms as u64);
    }
    if let Some(n) = cfg.max_nack_retries {
        out.max_nack_retries = n;
    }
    // Per-leg equalization mode (receiver side). `auto` is the self-configuring
    // default: measure always, align only when the inter-leg skew is worth the
    // latency, within budget.
    let eq_mode = map_equalization_mode(cfg.equalization);
    out.equalization = eq_mode;
    // The single bonding-latency budget = max_bonding_latency_ms (the one knob),
    // falling back to the legacy hold_max_ms, else the default. It is the
    // receiver's alignment ceiling + loss-recovery deadline (`hold_max`). The
    // jitter floor (`hold_time`) stays separate, from `hold_ms`. Only injected
    // when equalization measures — `off` keeps the legacy servo-ceiling
    // semantics of hold_max_ms.
    if eq_mode.measures() {
        let budget_ms = cfg
            .max_bonding_latency_ms
            .map(|m| m as u64)
            .or(cfg.hold_max_ms.map(|m| m as u64))
            .unwrap_or(DEFAULT_EQUALIZATION_BUDGET_MS);
        out.hold_max = Some(Duration::from_millis(budget_ms));
        // Log the effective budget so a cross-end mismatch (the matching bonded
        // output's max_bonding_latency_ms set to a different value on the other
        // node) is visible — the two should be equal for the single-knob model.
        tracing::info!(
            "bonded input (flow {}): equalization={:?}, latency budget {} ms — \
             set the matching bonded output's max_bonding_latency_ms to the same value",
            cfg.bond_flow_id,
            eq_mode,
            budget_ms
        );
    }
    if let Some(hex_key) = &cfg.encryption_key {
        out.encryption_key = Some(decode_bond_key(hex_key)?);
    }
    if let Some(f) = &cfg.fec {
        out.fec = Some(FecParams {
            columns: f.columns,
            rows: f.rows,
        });
    }
    // Per-leg FEC: each path that carries its own `fec` runs an independent
    // decoder over only that leg's packets (validation rejects mixing this
    // with the combined `fec` above). A non-empty map selects per-leg mode.
    for p in &cfg.paths {
        if let Some(f) = &p.fec {
            out.per_path_fec.insert(p.id, build_per_leg_fec(f));
        }
    }
    for p in &cfg.paths {
        // A bonded INPUT relay leg registers `ingress` with the relay (it
        // receives media; the matching bonded output leg registers `egress`).
        let transport = if matches!(&p.transport, BondPathTransportConfig::Relay { .. }) {
            let (att, bridge, t) =
                prepare_relay_leg(p, bond_has_key, TunnelDirection::Ingress)?;
            attachments.insert(p.id, att);
            bridges.push(bridge);
            t
        } else {
            translate_transport_for_receiver(&p.transport)?
        };
        out.paths.push(BondPathTxCfg {
            id: p.id,
            name: p.name.clone(),
            weight_hint: p.weight_hint,
            transport,
        });
    }
    Ok(BondBuild {
        cfg: out,
        attachments,
        bridges,
    })
}

fn translate_transport_for_receiver(
    t: &BondPathTransportConfig,
) -> anyhow::Result<BondPathTxTransport> {
    Ok(match t {
        // Relay legs are translated via `prepare_relay_leg` (they need bridge
        // channels), never here.
        BondPathTransportConfig::Relay { .. } => {
            anyhow::bail!("internal: relay bond leg must be prepared via prepare_relay_leg")
        }
        BondPathTransportConfig::Udp {
            bind,
            remote,
            interface,
            // gateway/source are sender-side only (validation rejects
            // them on a receiver); ignore on the receive path.
            gateway: _,
            source: _,
        } => BondPathTxTransport::Udp {
            bind: bind.as_deref().map(parse_sockaddr).transpose()?,
            remote: remote.as_deref().map(parse_sockaddr).transpose()?,
            interface: interface.clone(),
        },
        BondPathTransportConfig::Rist {
            role,
            remote,
            local_bind,
            buffer_ms,
        } => {
            let role = match role {
                BondRistRole::Sender => BondRistRoleTx::Sender,
                BondRistRole::Receiver => BondRistRoleTx::Receiver,
            };
            BondPathTxTransport::Rist {
                role,
                remote: remote.as_deref().map(parse_sockaddr).transpose()?,
                local_bind: local_bind.as_deref().map(parse_sockaddr).transpose()?,
                buffer_ms: *buffer_ms,
            }
        }
        BondPathTransportConfig::Quic {
            role: _,
            addr,
            server_name,
            tls,
            bind,
            interface,
        } => BondPathTxTransport::Quic {
            // Role is auto-derived from the side: a bonded input (receiver) leg is
            // always the QUIC server. Any value carried in the config is ignored.
            role: BondQuicRoleTx::Server,
            addr: parse_sockaddr(addr)?,
            server_name: server_name.clone(),
            tls: translate_tls(tls)?,
            bind: bind.as_deref().map(parse_sockaddr).transpose()?,
            interface: interface.clone(),
        },
    })
}

pub(crate) fn translate_tls(tls: &BondQuicTls) -> anyhow::Result<BondQuicTlsTx> {
    Ok(match tls {
        BondQuicTls::SelfSigned => BondQuicTlsTx::SelfSigned,
        BondQuicTls::Pem {
            cert_chain_path,
            private_key_path,
            client_trust_root_path,
        } => {
            let cert_chain = std::fs::read(cert_chain_path)
                .map_err(|e| anyhow::anyhow!("read cert chain '{cert_chain_path}': {e}"))?;
            let private_key = std::fs::read(private_key_path)
                .map_err(|e| anyhow::anyhow!("read private key '{private_key_path}': {e}"))?;
            let client_trust_root = match client_trust_root_path {
                Some(p) => Some(
                    std::fs::read(p).map_err(|e| anyhow::anyhow!("read trust root '{p}': {e}"))?,
                ),
                None => None,
            };
            BondQuicTlsTx::Pem {
                cert_chain,
                private_key,
                client_trust_root,
            }
        }
    })
}

pub(crate) fn parse_sockaddr(s: &str) -> anyhow::Result<SocketAddr> {
    // Fast path: a literal `ip:port`.
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(addr);
    }
    // Fall back to DNS for a `host:port` remote so an operator can point a
    // bond leg at a hostname (e.g. `home.example.com:7400`) instead of a
    // bare IP literal that has to be hand-edited every time the address
    // changes. Resolved once here at flow-build time — an IP change after
    // that needs a flow restart, same as any resolve-at-connect transport.
    // This runs at flow start, never on the data path. Prefer an IPv4
    // record (bond cellular / CGNAT legs are v4) and fall back to the
    // first record of any family.
    use std::net::ToSocketAddrs;
    let resolved: Vec<SocketAddr> = s
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("invalid or unresolvable socket address '{s}': {e}"))?
        .collect();
    resolved
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| resolved.first())
        .copied()
        .ok_or_else(|| anyhow::anyhow!("socket address '{s}' resolved to no records"))
}

/// Decode a 64-hex-char bond AEAD key into its 32 raw bytes. Shared by
/// the bonded input (receiver) and output (sender) so both ends derive
/// the same key bytes from the operator's hex string.
pub(crate) fn decode_bond_key(hex: &str) -> anyhow::Result<Vec<u8>> {
    let hex = hex.trim();
    if hex.len() != 64 {
        anyhow::bail!(
            "bond encryption_key must be 64 hex chars (32 bytes), got {}",
            hex.len()
        );
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("invalid hex in bond encryption_key at {i}: {e}"))
        })
        .collect()
}

/// String label for the path transport, used in stats + Prometheus.
pub(crate) fn bond_transport_label(t: &BondPathTransportConfig) -> String {
    match t {
        BondPathTransportConfig::Udp { .. } => "udp".to_string(),
        BondPathTransportConfig::Relay { .. } => "relay".to_string(),
        BondPathTransportConfig::Rist { .. } => "rist".to_string(),
        BondPathTransportConfig::Quic { .. } => "quic".to_string(),
    }
}

/// The kernel netdev an interface-mode UDP leg egresses on, for the bond-leg
/// stats join (cellular signal strip). `None` for gateway-mode legs (the
/// gateway IP + policy route do the steering, not a NIC pin) and for
/// QUIC / RIST legs.
pub(crate) fn bond_path_interface(t: &BondPathTransportConfig) -> Option<String> {
    match t {
        BondPathTransportConfig::Udp {
            interface, gateway, ..
        } if gateway.is_none() => interface.clone(),
        // A relay leg's bridge egresses the configured NIC (interface-mode);
        // surface it for the cellular signal-strip join. Gateway-mode legs
        // steer via the policy route, not a NIC pin, so report None there.
        BondPathTransportConfig::Relay {
            interface, gateway, ..
        } if gateway.is_none() => interface.clone(),
        _ => None,
    }
}

/// The relay tunnel id (UUID) for a `Relay` leg — surfaced on
/// `BondPathLegStats.tunnel_id` so the manager can join this live leg to the
/// relay `udp_session` forwarding it (per-leg edge-send vs relay-forward
/// correlation). `None` for direct UDP / QUIC / RIST legs.
pub(crate) fn bond_path_tunnel_id(t: &BondPathTransportConfig) -> Option<String> {
    match t {
        BondPathTransportConfig::Relay { tunnel_id, .. } => Some(tunnel_id.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod bonded_input_tests {
    use super::*;

    #[test]
    fn bond_transport_label_names_each_transport() {
        let udp: crate::config::models::BondPathTransportConfig =
            serde_json::from_str(r#"{ "type": "udp", "bind": "0.0.0.0:7400" }"#).unwrap();
        assert_eq!(bond_transport_label(&udp).as_str(), "udp");
    }

    #[test]
    fn bond_path_tunnel_id_only_for_relay_legs() {
        // A relay leg surfaces its tunnel id (the manager joins it to the
        // relay's udp_session by this UUID).
        let relay: crate::config::models::BondPathTransportConfig = serde_json::from_str(
            r#"{ "type": "relay", "tunnel_id": "11111111-2222-3333-4444-555555555555",
                 "relay_addrs": ["relay.example.com:4434"] }"#,
        )
        .unwrap();
        assert_eq!(bond_transport_label(&relay).as_str(), "relay");
        assert_eq!(
            bond_path_tunnel_id(&relay).as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );

        // A direct UDP leg carries no relay tunnel id.
        let udp: crate::config::models::BondPathTransportConfig =
            serde_json::from_str(r#"{ "type": "udp", "remote": "203.0.113.7:7400" }"#).unwrap();
        assert_eq!(bond_path_tunnel_id(&udp), None);
    }

    #[test]
    fn parse_sockaddr_takes_literal_then_resolves_hostnames() {
        // Literal ip:port — the fast path, byte-exact.
        let lit = parse_sockaddr("203.0.113.7:7400").unwrap();
        assert_eq!(lit, "203.0.113.7:7400".parse::<SocketAddr>().unwrap());
        // Hostname:port resolves (localhost is in /etc/hosts → no network),
        // preferring the IPv4 record.
        let host = parse_sockaddr("localhost:7400").unwrap();
        assert_eq!(host.port(), 7400);
        assert!(host.ip().is_loopback());
        // A bare host with no port is rejected.
        assert!(parse_sockaddr("localhost").is_err());
    }

    #[test]
    fn build_receiver_cfg_maps_paths_and_flow_id() {
        let cfg: crate::config::models::BondedInputConfig = serde_json::from_str(
            r#"{ "bond_flow_id": 6016,
                 "paths": [
                   { "id": 0, "name": "a", "transport": { "type": "udp", "bind": "0.0.0.0:7400" } },
                   { "id": 1, "name": "b", "transport": { "type": "udp", "bind": "0.0.0.0:7401" } }
                 ] }"#,
        )
        .unwrap();
        let sock = build_receiver_cfg(&cfg).expect("receiver cfg builds").cfg;
        assert_eq!(sock.flow_id, 6016);
        assert_eq!(sock.paths.len(), 2);
    }

    #[test]
    fn equalization_mode_maps_and_derives_the_single_budget() {
        use bonding_transport::EqualizationMode as Tx;
        let leg = r#""paths":[{"id":0,"name":"a","transport":{"type":"udp","bind":"0.0.0.0:7400"}}]"#;
        let build = |json: &str| {
            let cfg: crate::config::models::BondedInputConfig =
                serde_json::from_str(json).unwrap();
            build_receiver_cfg(&cfg).expect("receiver cfg builds").cfg
        };

        // Legacy `true` → Auto; no budget set → defaults to 1000.
        let s = build(&format!(r#"{{ "bond_flow_id":7, "equalization":true, {leg} }}"#));
        assert_eq!(s.equalization, Tx::Auto);
        assert_eq!(s.hold_max, Some(Duration::from_millis(DEFAULT_EQUALIZATION_BUDGET_MS)));

        // Absent equalization → Auto (the self-configuring default).
        let s = build(&format!(r#"{{ "bond_flow_id":7, {leg} }}"#));
        assert_eq!(s.equalization, Tx::Auto);
        assert_eq!(s.hold_max, Some(Duration::from_millis(1000)));

        // The single knob max_bonding_latency_ms takes precedence over hold_max_ms.
        let s = build(&format!(
            r#"{{ "bond_flow_id":7, "equalization":"auto", "max_bonding_latency_ms":800, "hold_max_ms":1200, {leg} }}"#
        ));
        assert_eq!(s.hold_max, Some(Duration::from_millis(800)));

        // No max_bonding_latency_ms → falls back to the legacy hold_max_ms (the
        // edge6 case: hold_max_ms:800 keeps its 800 ms budget).
        let s = build(&format!(
            r#"{{ "bond_flow_id":7, "equalization":true, "hold_max_ms":800, {leg} }}"#
        ));
        assert_eq!(s.hold_max, Some(Duration::from_millis(800)));

        // `off` → never measures, no budget injected (legacy aggregate-only).
        let s = build(&format!(r#"{{ "bond_flow_id":7, "equalization":"off", {leg} }}"#));
        assert_eq!(s.equalization, Tx::Off);
        assert_eq!(s.hold_max, None);
        // legacy `false` also maps to Off.
        let s = build(&format!(r#"{{ "bond_flow_id":7, "equalization":false, {leg} }}"#));
        assert_eq!(s.equalization, Tx::Off);

        // `on` → force-engage mode.
        let s = build(&format!(r#"{{ "bond_flow_id":7, "equalization":"on", {leg} }}"#));
        assert_eq!(s.equalization, Tx::On);
    }
}
