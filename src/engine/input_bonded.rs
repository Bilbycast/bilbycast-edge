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
    BondEqualizationMode, BondFecAlgorithm, BondFecConfig, BondPathTransportConfig, BondQuicTls,
    BondRistRole, BondedInputConfig,
};

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

        let socket_cfg = match build_receiver_cfg(&config) {
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

        let socket = match BondSocket::receiver(socket_cfg).await {
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
                path_handles.push(crate::stats::collector::BondPathStatsHandle::new(
                    p.id,
                    p.name.clone(),
                    bond_transport_label(&p.transport),
                    ps,
                ));
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
pub(crate) fn build_receiver_cfg(cfg: &BondedInputConfig) -> anyhow::Result<BondSocketConfig> {
    let mut out = BondSocketConfig {
        flow_id: cfg.bond_flow_id,
        paths: Vec::with_capacity(cfg.paths.len()),
        ..Default::default()
    };
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
        out.paths.push(BondPathTxCfg {
            id: p.id,
            name: p.name.clone(),
            weight_hint: p.weight_hint,
            transport: translate_transport_for_receiver(&p.transport)?,
        });
    }
    Ok(out)
}

fn translate_transport_for_receiver(
    t: &BondPathTransportConfig,
) -> anyhow::Result<BondPathTxTransport> {
    Ok(match t {
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
        let sock = build_receiver_cfg(&cfg).expect("receiver cfg builds");
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
            build_receiver_cfg(&cfg).expect("receiver cfg builds")
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
