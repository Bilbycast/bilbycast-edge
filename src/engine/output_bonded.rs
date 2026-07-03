// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Bonded output.
//!
//! Subscribes to the flow's broadcast channel, frames each outbound
//! payload into the bond protocol, and transmits across N paths via
//! [`bonding_transport::BondSocket::sender`]. The scheduler decides
//! which path (or paths, for duplication) carries each packet.
//!
//! ## Scheduler selection
//!
//! - [`BondSchedulerKind::RoundRobin`] — built-in
//!   `RoundRobinScheduler`.
//! - [`BondSchedulerKind::WeightedRtt`] — built-in
//!   `WeightedRttScheduler`.
//! - [`BondSchedulerKind::MediaAware`] — edge-owned
//!   [`super::bonded_scheduler::MediaAwareScheduler`]. Walks the TS
//!   stream, finds H.264 / HEVC NAL boundaries, and tags SPS / PPS /
//!   IDR packets as `Priority::Critical` so the inner scheduler
//!   duplicates them across the two lowest-RTT paths.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use bonding_transport::{
    AttachedChannels, BondScheduler, BondSocket, BondSocketConfig, CapacityAwareScheduler,
    CongestionConfig, FecParams, PacketHints, PathConfig as BondPathTxCfg, PathEvent,
    PathEventKind, PathPrior, PathTransport as BondPathTxTransport, Priority, RedundancyMode,
    RedundancyPolicy, RoundRobinScheduler, WeightedRttScheduler,
};

use crate::config::models::{
    BondCongestionConfig, BondedOutputConfig, BondRedundancyConfig, BondRedundancyMode,
    BondRedundancyPriority, BondSchedulerKind,
};
use crate::manager::events::{
    BondEventScope, EventSender, EventSeverity, category, run_bond_event_forwarder,
};
use crate::stats::collector::OutputStatsAccumulator;

use super::input_bonded::{
    parse_sockaddr, prepare_relay_leg, spawn_relay_leg_bridges, translate_tls, BondBuild,
    RelayLegBridge,
};
use crate::tunnel::config::TunnelDirection;
use super::packet::RtpPacket;
use super::ts_parse::TS_PACKET_SIZE;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;

const RTP_HEADER_MIN_SIZE: usize = 12;

/// Default per-leg IP path MTU assumed when `path_mtu` is unset: standard
/// 1500-byte ethernet. The derived TS re-chunk quantum under every
/// header / crypto combination is 7 × 188 = 1316 B — the classic SRT /
/// assembler datagram that internet paths carry unfragmented.
const DEFAULT_PATH_MTU: usize = 1500;
/// IPv4 (20) + UDP (8) header bytes per datagram.
const IPV4_UDP_OVERHEAD: usize = 28;
/// IPv6 (40) + UDP (8) header bytes per datagram.
const IPV6_UDP_OVERHEAD: usize = 48;
/// Relay-leg carrier framing: the 16-byte `tunnel_id` prefix the in-process
/// bridge prepends to every datagram (`tunnel::protocol::encode_udp_datagram`).
const RELAY_TUNNEL_ID_OVERHEAD: usize = 16;
/// Relay-leg tunnel AEAD (12 nonce + 16 tag) — applied by the bridge only
/// when the bond itself is unkeyed (the fail-closed single-encryption-layer
/// rule; bond-keyed relay legs carry the 29 B `0xBD` envelope instead).
const RELAY_TUNNEL_CRYPTO_OVERHEAD: usize = 28;
/// RIST-leg carrier framing: RIST Simple Profile wraps each bond frame in
/// its own 12-byte RTP header (`bonding-transport` `path/rist.rs`).
const RIST_RTP_OVERHEAD: usize = 12;
/// Bond wire header size (`bonding-protocol` `packet.rs`).
const BOND_HEADER_SIZE: usize = 12;
/// ChaCha20-Poly1305 envelope overhead when `encryption_key` is set:
/// 1 (0xBD magic) + 12 (nonce) + 16 (tag).
const BOND_CRYPTO_OVERHEAD: usize = 29;

/// Spawn a bonded output task.
pub fn spawn_bonded_output(
    config: BondedOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();
    tokio::spawn(async move {
        if let Err(e) =
            bonded_output_loop(&config, &mut rx, output_stats, cancel, &event_sender, &flow_id).await
        {
            tracing::error!("bonded output '{}' exited with error: {e}", config.id);
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::MEDIA,
                format!("bonded output '{}' exited with error: {e}", config.id),
                &flow_id,
            );
        }
    })
}

async fn bonded_output_loop(
    config: &BondedOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    // Gateway-mode legs: program each leg's policy route (ensure
    // source address + `from <source>` rule + `default via <gateway>`
    // table) BEFORE the socket binds to that source. Teardown runs on
    // every exit path so a stopped flow leaves no orphan routes.
    let programmed = program_gateway_paths(config, flow_id, events).await?;
    let result = bonded_output_run(config, rx, stats, cancel, events, flow_id).await;
    if !programmed.is_empty() {
        teardown_gateway_paths(flow_id, &programmed).await;
    }
    result
}

/// Program the policy route for every gateway-mode leg. Returns the
/// list of path ids that were programmed (for teardown). Fails loudly
/// — a gateway path that can't be routed must NOT silently fall
/// through to the default route (that collapses every leg onto one
/// router).
async fn program_gateway_paths(
    config: &BondedOutputConfig,
    flow_id: &str,
    events: &EventSender,
) -> anyhow::Result<Vec<u8>> {
    use crate::config::models::BondPathTransportConfig;
    let mut programmed = Vec::new();
    for p in &config.paths {
        if let BondPathTransportConfig::Udp {
            interface,
            gateway: Some(gw),
            source: Some(src),
            ..
        } = &p.transport
        {
            let gw_ip: std::net::IpAddr = match gw.parse() {
                Ok(ip) => ip,
                Err(_) => continue, // validation already rejected this
            };
            let src_net = super::bond_routing::SourceNet::parse(src)?;
            let iface = interface.as_deref().unwrap_or_default();
            let mgr = match super::bond_routing::BondRouteManager::global().await {
                Ok(m) => m,
                Err(e) => {
                    // No netlink / no CAP_NET_ADMIN — refuse, don't collapse.
                    teardown_gateway_paths(flow_id, &programmed).await;
                    events.emit_flow(
                        EventSeverity::Critical,
                        category::MEDIA,
                        format!(
                            "bonded output '{}': gateway routing unavailable ({e}). \
                             Needs CAP_NET_ADMIN; path '{}' not started.",
                            config.id, p.name
                        ),
                        flow_id,
                    );
                    return Err(anyhow::anyhow!("bond gateway routing init: {e}"));
                }
            };
            if let Err(e) = mgr.program(flow_id, p.id, iface, src_net, gw_ip).await {
                teardown_gateway_paths(flow_id, &programmed).await;
                events.emit_flow(
                    EventSeverity::Critical,
                    category::MEDIA,
                    format!(
                        "bonded output '{}': failed to program gateway route for path '{}' \
                         (via {gw}): {e}",
                        config.id, p.name
                    ),
                    flow_id,
                );
                return Err(anyhow::anyhow!("bond gateway route program: {e}"));
            }
            programmed.push(p.id);
        }
    }
    Ok(programmed)
}

/// Best-effort teardown of all programmed gateway routes for a flow.
async fn teardown_gateway_paths(flow_id: &str, path_ids: &[u8]) {
    if let Ok(mgr) = super::bond_routing::BondRouteManager::global().await {
        for id in path_ids {
            mgr.teardown(flow_id, *id).await;
        }
    }
}

/// Routing facts for one gateway-mode leg, captured at output start so
/// the rebuild handler can re-program without re-parsing config.
struct GatewayLeg {
    path_id: u8,
    name: String,
    interface: String,
    source: super::bond_routing::SourceNet,
    gateway: std::net::IpAddr,
}

/// Extract the gateway-mode legs from a bonded output config. Parse
/// failures are skipped — validation already rejected them, and
/// `program_gateway_paths` applies the same rule.
fn gateway_legs(config: &BondedOutputConfig) -> Vec<GatewayLeg> {
    use crate::config::models::BondPathTransportConfig;
    config
        .paths
        .iter()
        .filter_map(|p| {
            let BondPathTransportConfig::Udp {
                interface,
                gateway: Some(gw),
                source: Some(src),
                ..
            } = &p.transport
            else {
                return None;
            };
            Some(GatewayLeg {
                path_id: p.id,
                name: p.name.clone(),
                interface: interface.clone().unwrap_or_default(),
                source: super::bond_routing::SourceNet::parse(src).ok()?,
                gateway: gw.parse().ok()?,
            })
        })
        .collect()
}

/// How often the re-program task verifies each gateway leg's policy
/// route is still in effect (`BondRouteManager::is_intact`). Covers the
/// kernel-silent failures: link-down flushes the private table without
/// touching the address or ifindex (no watcher signal, sends still
/// succeed via the main default route — cosmetic bond), and a failed
/// re-program retries here until the device returns.
const GATEWAY_ROUTE_REASSERT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Re-program a gateway-mode leg's policy route when the bond watcher
/// rebuilds its socket (`PathRebuilt`) or loses its watch target
/// (`InterfaceLost`), plus a periodic integrity re-assert. Failure is
/// loud: a leg whose route can't be restored silently falls through to
/// the default route and collapses onto another leg's router.
///
/// `InterfaceLost` matters for the circularity it breaks: the leg's
/// source address is added by `program()` itself, so after a USB
/// modem re-plug drops it, the watcher polls `NoChange` forever (it
/// only rebuilds when the target RETURNS) and `PathRebuilt` can never
/// fire. Re-running `program()` (idempotent) re-adds the address; the
/// watcher's next tick then sees it restored and rebuilds the socket.
async fn run_gateway_route_reprogram(
    mut rx: broadcast::Receiver<PathEvent>,
    legs: Vec<GatewayLeg>,
    output_id: String,
    flow_id: String,
    events: EventSender,
    cancel: CancellationToken,
) {
    // Per-leg "re-assert already failing" latch so the periodic tick
    // emits one Critical on entry into the failing state, not one per
    // 5 s retry.
    let mut reassert_failed = vec![false; legs.len()];
    let mut reassert = tokio::time::interval(GATEWAY_ROUTE_REASSERT_INTERVAL);
    reassert.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reassert.tick().await; // swallow the immediate t=0 tick
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = reassert.tick() => {
                let Ok(mgr) = super::bond_routing::BondRouteManager::global().await else {
                    continue;
                };
                for (i, leg) in legs.iter().enumerate() {
                    if mgr.is_intact(&flow_id, leg.path_id).await {
                        reassert_failed[i] = false;
                        continue;
                    }
                    match mgr
                        .program(&flow_id, leg.path_id, &leg.interface, leg.source, leg.gateway)
                        .await
                    {
                        Ok(()) => {
                            reassert_failed[i] = false;
                            tracing::warn!(
                                "bonded output '{output_id}': gateway route for path '{}' \
                                 (#{}) was missing (link flap / device re-plug / route \
                                 flush) — re-programmed",
                                leg.name,
                                leg.path_id
                            );
                        }
                        Err(e) => {
                            if !reassert_failed[i] {
                                reassert_failed[i] = true;
                                events.emit_flow_with_details(
                                    EventSeverity::Critical,
                                    category::MEDIA,
                                    format!(
                                        "bonded output '{output_id}': gateway route for path \
                                         '{}' (via {}) is gone and re-programming failed: {e}. \
                                         Retrying every {}s; until it succeeds this leg rides \
                                         the default route (cosmetic bond).",
                                        leg.name,
                                        leg.gateway,
                                        GATEWAY_ROUTE_REASSERT_INTERVAL.as_secs()
                                    ),
                                    &flow_id,
                                    serde_json::json!({
                                        "error_code": "bond_gateway_route_lost",
                                        "output_id": output_id,
                                        "path_id": leg.path_id,
                                        "path_name": leg.name,
                                        "gateway": leg.gateway.to_string(),
                                    }),
                                );
                            }
                        }
                    }
                }
            }
            res = rx.recv() => match res {
                Ok(ev) => {
                    let rebuilt = match ev.kind {
                        PathEventKind::PathRebuilt { .. } => true,
                        // Source address vanished — re-add it so the
                        // watcher can rebuild (see fn doc).
                        PathEventKind::InterfaceLost => false,
                        _ => continue,
                    };
                    let Some(leg) = legs.iter().find(|l| l.path_id == ev.path_id) else {
                        continue;
                    };
                    let cause = if rebuilt { "socket rebuild" } else { "interface lost" };
                    let result = match super::bond_routing::BondRouteManager::global().await {
                        Ok(mgr) => {
                            mgr.program(
                                &flow_id,
                                leg.path_id,
                                &leg.interface,
                                leg.source,
                                leg.gateway,
                            )
                            .await
                        }
                        Err(e) => Err(e),
                    };
                    match result {
                        Ok(()) => tracing::info!(
                            "bonded output '{output_id}': re-programmed gateway route for \
                             path '{}' (#{}) after {cause}",
                            leg.name,
                            leg.path_id
                        ),
                        // InterfaceLost commonly means the device is
                        // still gone (USB re-enumeration takes seconds)
                        // — the periodic re-assert above retries and
                        // owns the Critical-on-persistent-failure
                        // latch, so only a rebuild-triggered failure
                        // is loud here.
                        Err(e) if rebuilt => events.emit_flow(
                            EventSeverity::Critical,
                            category::MEDIA,
                            format!(
                                "bonded output '{output_id}': failed to re-program gateway \
                                 route for path '{}' (via {}) after socket rebuild: {e}",
                                leg.name, leg.gateway
                            ),
                            &flow_id,
                        ),
                        Err(e) => tracing::warn!(
                            "bonded output '{output_id}': gateway route re-program for \
                             path '{}' after {cause} failed ({e}); periodic re-assert \
                             will retry",
                            leg.name
                        ),
                    }
                }
                // Lagged: rebuild events are sparse; the next one re-syncs.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

/// Bond data-header version that carries the equalization send-stamp. A leg
/// whose far receiver advertises a version below this while we are stamping is
/// a media-blackout risk (it can't parse our v2 headers).
const BOND_PROTOCOL_V2: u64 = 2;

/// Periodic per-leg bond protocol-version alarm. When this output is
/// equalization-stamping v2 but a leg's far receiver has advertised an older
/// version (it has equalization off, or is a genuinely-old build), it cannot
/// parse our 16-byte headers and media on that leg can blackhole. Fires one
/// Warning per leg (advisory — the negotiation still keeps that leg on v1, so
/// this is a "you configured the two ends differently" heads-up, not a fault).
async fn run_bond_version_alarm(
    legs: Vec<(u8, String, Arc<bonding_protocol::stats::PathStats>)>,
    measures: bool,
    output_id: String,
    flow_id: String,
    events: EventSender,
    cancel: CancellationToken,
) {
    // Nothing to alarm about if we never stamp (equalization off) or no legs.
    if !measures || legs.is_empty() {
        return;
    }
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    tick.tick().await; // swallow the immediate t=0 tick
    let mut alarmed: std::collections::HashSet<u8> = std::collections::HashSet::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {
                for (id, name, ps) in &legs {
                    let v = ps.peer_protocol_version.load(Ordering::Relaxed);
                    // 0 = no keepalive-ack yet (don't alarm on the sentinel);
                    // >= v2 = fine; in between = old/equalization-off peer while
                    // we intend to stamp. One-shot per leg.
                    if v != 0 && v < BOND_PROTOCOL_V2 && alarmed.insert(*id) {
                        tracing::warn!(
                            "bonded output '{output_id}' leg '{name}': peer advertises bond \
                             protocol v{v} but this output equalization-stamps v2 — peer can't \
                             parse v2; that leg stays on v1 (no equalization). Align both ends."
                        );
                        events.emit_output_with_details(
                            EventSeverity::Warning,
                            category::BOND,
                            format!(
                                "bonded output '{output_id}' leg '{name}': bond protocol version \
                                 mismatch — peer reports v{v}, this end equalization-stamps v2. \
                                 Equalization won't engage on this leg until the matching bonded \
                                 input also has equalization enabled (auto/on)."
                            ),
                            &output_id,
                            serde_json::json!({
                                "error_code": "bond_protocol_version_mismatch",
                                "component": "bonded_output",
                                "output_id": output_id,
                                "path_id": id,
                                "path_name": name,
                                "peer_version": v,
                                "local_version": BOND_PROTOCOL_V2,
                                "flow_id": flow_id,
                            }),
                        );
                    }
                }
            }
        }
    }
}

/// Auto-diagnose a leg the instant it goes down / starts re-dialing, and emit
/// the plain-English verdict (`bond_leg_test::test_leg`) as an operator event
/// — so the operator is told WHY a leg failed without clicking "Test leg".
/// Zero-impact (the probe puts nothing on the wire); debounced per leg.
async fn run_bond_leg_autodiagnose(
    mut rx: broadcast::Receiver<PathEvent>,
    legs: Vec<(u8, crate::config::models::BondPathTransportConfig)>,
    output_id: String,
    flow_id: String,
    events: EventSender,
    cancel: CancellationToken,
) {
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(30);
    let mut last: std::collections::HashMap<u8, std::time::Instant> =
        std::collections::HashMap::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            res = rx.recv() => match res {
                Ok(ev) => {
                    if !matches!(
                        ev.kind,
                        PathEventKind::PathDead { .. } | PathEventKind::PathReconnecting { .. }
                    ) {
                        continue;
                    }
                    let Some((_, transport)) = legs.iter().find(|(id, _)| *id == ev.path_id)
                    else {
                        continue;
                    };
                    let now = std::time::Instant::now();
                    if let Some(t) = last.get(&ev.path_id) {
                        if now.duration_since(*t) < DEBOUNCE {
                            continue;
                        }
                    }
                    last.insert(ev.path_id, now);
                    let report = crate::engine::bond_leg_test::test_leg(transport, false);
                    let sev = if report.ok {
                        EventSeverity::Info
                    } else {
                        EventSeverity::Warning
                    };
                    let message = format!(
                        "bonded output '{output_id}' leg '{}' (#{}) diagnosis: {}",
                        ev.path_name, ev.path_id, report.headline
                    );
                    events.emit_output_with_details(
                        sev,
                        category::BOND,
                        message,
                        &output_id,
                        serde_json::json!({
                            "error_code": "bond_leg_diagnosis",
                            "path_id": ev.path_id,
                            "path_name": ev.path_name,
                            "headline": report.headline,
                            "suggested_fix": report.suggested_fix,
                            "checks": report.checks,
                            "chosen_egress_interface": report.chosen_egress_interface,
                            "flow_id": flow_id,
                        }),
                    );
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

async fn bonded_output_run(
    config: &BondedOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    // Register this output's legs as "in use" so a per-leg capacity probe
    // refuses to saturate a physical link this flow is using (zero-impact
    // guarantee). Dropped on every return path → leg freed.
    let _active_legs = super::bond_leg_probe::register_active_legs(
        flow_id,
        super::bond_leg_probe::leg_keys_for_paths(&config.paths),
    );

    let BondBuild {
        cfg: socket_cfg,
        attachments,
        bridges,
    } = build_sender_cfg(config)?;
    let path_ids: Vec<u8> = socket_cfg.paths.iter().map(|p| p.id).collect();

    // Spawn one in-process relay bridge per relay leg BEFORE the socket binds —
    // each owns its relay socket (Register/keepalive + failover) and pumps the
    // attached leg's channels with no loopback hop. The bridge tasks live under
    // the output's cancel token; a relay rotation re-Registers on a fresh socket
    // but the leg channels persist (the bond leg is never torn down).
    spawn_relay_leg_bridges(bridges, &cancel);

    // Shared handles to the adaptive scheduler's discovered per-leg
    // capacity (bits/sec) — empty for the non-adaptive policies.
    let mut capacity_handles: Vec<(u8, std::sync::Arc<std::sync::atomic::AtomicU64>)> = Vec::new();
    // Shared handles the shared-leg capacity broker writes each flow's
    // fair-share ceiling into (bits/sec) — empty for non-adaptive policies.
    let mut ceiling_handles: Vec<(u8, std::sync::Arc<std::sync::atomic::AtomicU64>)> = Vec::new();

    // Operator redundancy policy (replicate across N best legs). Off →
    // default no-op; each scheduler still does its own Critical/IDR dup.
    let redundancy = build_redundancy(&config.redundancy);

    // Pick scheduler. MediaAware/Adaptive add NAL awareness on top of
    // the inner path-selection policy; see engine::bonded_scheduler.
    let socket = match config.scheduler {
        BondSchedulerKind::RoundRobin => {
            let mut sched = RoundRobinScheduler::new(path_ids.clone());
            sched.set_redundancy(redundancy);
            BondSocket::sender_attached(socket_cfg, sched, attachments)
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
        BondSchedulerKind::WeightedRtt => {
            let mut sched = WeightedRttScheduler::new(path_ids.clone());
            sched.set_redundancy(redundancy);
            BondSocket::sender_attached(socket_cfg, sched, attachments)
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
        BondSchedulerKind::MediaAware => {
            // MediaAware scheduler only influences the *hints* we pass
            // to send(); the inner scheduler is WeightedRtt so
            // `Critical`-priority packets duplicate across the two
            // lowest-RTT paths.
            let mut sched = WeightedRttScheduler::new(path_ids.clone());
            sched.set_redundancy(redundancy);
            BondSocket::sender_attached(socket_cfg, sched, attachments)
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
        BondSchedulerKind::Adaptive => {
            // Congestion-controlled, capacity-aware inner scheduler:
            // each leg discovers its usable bandwidth and is filled to
            // (not past) it, with smooth quality deweighting and spill.
            let priors: Vec<PathPrior> = config
                .paths
                .iter()
                .map(|p| PathPrior {
                    id: p.id,
                    weight_hint: p.weight_hint,
                    ceiling_bps: p.max_bitrate_bps,
                })
                .collect();
            let mut cong = config
                .congestion
                .as_ref()
                .map(build_congestion)
                .unwrap_or_else(edge_default_congestion);
            // Equalization latency budget (top-level output knob, not a
            // congestion sub-field): the sender-side un-equalizable demote.
            if let Some(ms) = config.max_bonding_latency_ms {
                cong.max_bonding_latency_us = ms as u64 * 1000;
            }
            // Surface the effective sender demote budget so a cross-end mismatch
            // with the bonded input's budget is visible (they should be equal).
            if super::input_bonded::map_equalization_mode(config.equalization).measures() {
                tracing::info!(
                    "bonded output '{}': equalization demote budget {} ms — set the matching \
                     bonded input's max_bonding_latency_ms to the same value",
                    config.id,
                    cong.max_bonding_latency_us / 1000
                );
            }
            let mut sched = CapacityAwareScheduler::with_paths(priors, cong);
            capacity_handles = sched.capacity_handles();
            ceiling_handles = sched.ceiling_handles();
            sched.set_redundancy(redundancy);
            BondSocket::sender_attached(socket_cfg, sched, attachments)
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
    };

    // Shared-leg capacity broker: register this flow's legs so a host-level
    // fair-share arbiter divides each shared physical uplink across all
    // bonded flows on it (writing each leg's ceiling atomic every ~100 ms).
    // No-op unless the broker is enabled (bond_uplinks configured). The guard
    // deregisters on teardown. Only the Adaptive scheduler exposes ceilings.
    let _broker_reg = {
        let b = super::bond_leg_broker::broker();
        if b.enabled() && !ceiling_handles.is_empty() {
            let leg_keys = super::bond_leg_probe::leg_keys_for_paths(&config.paths);
            let mut regs = Vec::new();
            for (i, p) in config.paths.iter().enumerate() {
                let leg_key = match leg_keys
                    .get(i)
                    .and_then(|k| super::bond_leg_broker::leg_key_from_probe(k))
                {
                    Some(k) => k,
                    None => continue,
                };
                let demand = capacity_handles
                    .iter()
                    .find(|(id, _)| *id == p.id)
                    .map(|(_, a)| a.clone());
                let ceiling = ceiling_handles
                    .iter()
                    .find(|(id, _)| *id == p.id)
                    .map(|(_, a)| a.clone());
                if let (Some(demand), Some(ceiling)) = (demand, ceiling) {
                    regs.push(super::bond_leg_broker::MemberReg {
                        leg_key,
                        flow_id: config.bond_flow_id,
                        path_id: p.id,
                        weight_hint: p.weight_hint,
                        max_bitrate_bps: p.max_bitrate_bps,
                        priority: config.priority.unwrap_or_default(),
                        demand,
                        ceiling,
                        path_stats: socket.path_stats(p.id),
                    });
                }
            }
            // P4 admission check (advisory by default; refuses only under
            // strict mode). Surfaces when this flow lands on a shared uplink
            // that can't give every co-resident its min-viable rate.
            if let Some(leg) = b.admission_conflict(&regs) {
                events.emit_with_details(
                    crate::manager::events::EventSeverity::Warning,
                    "bonding",
                    format!(
                        "bonded flow admitted onto over-subscribed shared uplink '{leg}' \
                         — combined min-viable rates exceed its capacity"
                    ),
                    Some(flow_id),
                    serde_json::json!({
                        "error_code": "bond_leg_admission_pressure",
                        "leg": leg,
                    }),
                );
            }
            Some(b.register(regs))
        } else {
            None
        }
    };

    // Register per-path + aggregate bond stats on the accumulator so
    // `OutputStats.bond_stats` carries path-level RTT, loss,
    // retransmit, keepalive state at snapshot time.
    let scheduler_label = match config.scheduler {
        BondSchedulerKind::RoundRobin => "round_robin",
        BondSchedulerKind::WeightedRtt => "weighted_rtt",
        BondSchedulerKind::MediaAware => "media_aware",
        BondSchedulerKind::Adaptive => "adaptive",
    }
    .to_string();
    let mut path_handles: Vec<crate::stats::collector::BondPathStatsHandle> =
        Vec::with_capacity(config.paths.len());
    for p in &config.paths {
        if let Some(ps) = socket.path_stats(p.id) {
            let gateway_mode = matches!(
                &p.transport,
                crate::config::models::BondPathTransportConfig::Udp { gateway: Some(_), .. }
            );
            let cap_est = capacity_handles
                .iter()
                .find(|(id, _)| *id == p.id)
                .map(|(_, a)| a.clone());
            path_handles.push(
                crate::stats::collector::BondPathStatsHandle::new(
                    p.id,
                    p.name.clone(),
                    super::input_bonded::bond_transport_label(&p.transport),
                    ps,
                )
                .with_gateway_mode(gateway_mode)
                .with_interface(super::input_bonded::bond_path_interface(&p.transport))
                .with_tunnel_id(super::input_bonded::bond_path_tunnel_id(&p.transport))
                .with_capacity_est(cap_est),
            );
        }
    }
    // Counts payloads that exceeded the per-datagram MTU budget (sent
    // anyway — see the budget computation below). Shared with the bond
    // stats handle so the snapshot surfaces it as
    // `BondLegStats.oversize_payloads`.
    let oversize_payloads = Arc::new(AtomicU64::new(0));
    stats.set_bond_stats(crate::stats::collector::BondStatsHandle {
        flow_id: config.bond_flow_id,
        role: crate::stats::collector::BondStatsRole::Sender,
        scheduler: scheduler_label,
        conn_stats: socket.stats(),
        paths: path_handles,
        oversize_payloads: oversize_payloads.clone(),
    });

    // Security visibility: encryption is OPT-IN and OFF by default. A bond
    // carrying plaintext UDP legs with no `encryption_key` ships media in
    // the clear. Warn once at bring-up so the unsafe default is visible.
    // `encryption_key` ChaCha20-encrypts every UDP leg; QUIC legs are
    // always TLS (so a pure-QUIC bond is fine without a key).
    if config.encryption_key.is_none() {
        let udp_legs = config
            .paths
            .iter()
            .filter(|p| {
                matches!(
                    p.transport,
                    crate::config::models::BondPathTransportConfig::Udp { .. }
                )
            })
            .count();
        if udp_legs > 0 {
            tracing::warn!(
                "bonded output '{}': {} UDP leg(s) are UNENCRYPTED — no encryption_key set, \
                 media transits in the clear. Set a 64-hex-char encryption_key (same value on \
                 the bonded input) to ChaCha20-encrypt UDP legs; QUIC legs are always TLS.",
                config.id,
                udp_legs,
            );
            events.emit_flow_with_details(
                EventSeverity::Warning,
                category::MEDIA,
                format!(
                    "bonded output '{}': {} UDP leg(s) unencrypted — set encryption_key for confidentiality",
                    config.id, udp_legs
                ),
                flow_id,
                serde_json::json!({
                    "error_code": "bond_legs_unencrypted",
                    "component": "bonded_output",
                    "output_id": config.id,
                    "udp_legs": udp_legs,
                }),
            );
        }
    }

    tracing::info!(
        "bonded output '{}' up: {} path(s), scheduler={:?}",
        config.id,
        path_ids.len(),
        config.scheduler
    );
    events.emit_flow(
        EventSeverity::Info,
        category::MEDIA,
        format!(
            "bonded output '{}' up — {} path(s)",
            config.id,
            path_ids.len()
        ),
        flow_id,
    );

    // Forward bonding lifecycle events (per-path alive/dead +
    // bond-aggregate transitions) to the manager event feed.
    tokio::spawn(run_bond_event_forwarder(
        socket.subscribe_events(),
        events.clone(),
        flow_id.to_string(),
        BondEventScope::Output,
        config.id.clone(),
        cancel.clone(),
    ));

    // Auto-diagnose a leg the instant it goes down / starts re-dialing and
    // emit the plain-English verdict as an operator event — so the operator
    // is told WHY a leg failed without having to click "Test leg". The probe
    // is zero-impact (no packets on the wire) and debounced per leg.
    tokio::spawn(run_bond_leg_autodiagnose(
        socket.subscribe_events(),
        config.paths.iter().map(|p| (p.id, p.transport.clone())).collect(),
        config.id.clone(),
        flow_id.to_string(),
        events.clone(),
        cancel.clone(),
    ));

    // Gateway-mode legs lose their source address + policy route when
    // the underlying device bounces (the kernel flushes both on
    // link-down), so a watcher-driven socket rebuild alone would leave
    // the leg riding the default route — collapsing it onto another
    // leg's router. Re-program the leg's route on every PathRebuilt
    // AND on InterfaceLost (the address is edge-programmed, so nothing
    // else ever re-adds it and the watcher's rebuild is gated on its
    // return), plus a periodic is_intact re-assert for kernel-silent
    // route flushes. `program()` is idempotent per (flow_id, path_id)
    // and re-ensures the source address on the NIC.
    let gw_legs = gateway_legs(config);
    if !gw_legs.is_empty() {
        tokio::spawn(run_gateway_route_reprogram(
            socket.subscribe_events(),
            gw_legs,
            config.id.clone(),
            flow_id.to_string(),
            events.clone(),
            cancel.clone(),
        ));
    }

    // Per-leg bond protocol-version alarm: warn (once per leg) if we stamp v2
    // for equalization but a leg's far receiver advertises an older version.
    let version_legs: Vec<(u8, String, Arc<bonding_protocol::stats::PathStats>)> = config
        .paths
        .iter()
        .filter_map(|p| socket.path_stats(p.id).map(|ps| (p.id, p.name.clone(), ps)))
        .collect();
    tokio::spawn(run_bond_version_alarm(
        version_legs,
        super::input_bonded::map_equalization_mode(config.equalization).measures(),
        config.id.clone(),
        flow_id.to_string(),
        events.clone(),
        cancel.clone(),
    ));

    // Optional media-aware NAL-priority tagging — used by both the
    // legacy MediaAware policy and the new Adaptive policy.
    let mut media_sched = match config.scheduler {
        BondSchedulerKind::MediaAware | BondSchedulerKind::Adaptive => {
            Some(super::bonded_scheduler::MediaAwareScheduler::new())
        }
        _ => None,
    };

    let mut program_filter = config.program_number.map(TsProgramFilter::new);
    let mut filter_scratch: Vec<u8> = Vec::new();

    let mut pid_remapper = config.pid_map.as_ref().and_then(|m| {
        let r = TsPidRemapper::new(m);
        if r.is_active() {
            tracing::info!(
                "bonded output '{}': pid_map active ({} entries)",
                config.id,
                m.len()
            );
            Some(r)
        } else {
            None
        }
    });
    let mut remap_scratch: Vec<u8> = Vec::new();

    // Per-datagram sizing: from the configured (or default 1500) path MTU,
    // resolve the payload budget and the N×188 re-chunk quantum. Oversized
    // TS payloads are re-chunked in the send loop below so nothing this
    // output emits fragments at the IP layer on the narrowest leg.
    let (payload_budget, chunk_bytes) = resolve_datagram_budget(config);
    tracing::info!(
        "bonded output '{}': path MTU {} → per-datagram payload budget {} B → \
         {} × 188 B TS packet(s) ({} B) per datagram",
        config.id,
        config.path_mtu.unwrap_or(DEFAULT_PATH_MTU as u32),
        payload_budget,
        chunk_bytes / TS_PACKET_SIZE,
        chunk_bytes
    );
    let mtu_warned = AtomicBool::new(false);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("bonded output '{}' stopping (cancelled)", config.id);
                socket.close();
                return Ok(());
            }
            result = rx.recv() => match result {
                Ok(packet) => {
                    let ts_bytes: &[u8] = if packet.is_raw_ts {
                        &packet.data[..]
                    } else if packet.data.len() > RTP_HEADER_MIN_SIZE {
                        &packet.data[RTP_HEADER_MIN_SIZE..]
                    } else {
                        continue;
                    };

                    let filtered: &[u8] = if let Some(ref mut pf) = program_filter {
                        filter_scratch.clear();
                        pf.filter_into(ts_bytes, &mut filter_scratch);
                        if filter_scratch.is_empty() {
                            continue;
                        }
                        &filter_scratch
                    } else {
                        ts_bytes
                    };

                    let remapped: &[u8] = if let Some(ref mut remapper) = pid_remapper {
                        remap_scratch.clear();
                        remapper.process(filtered, &mut remap_scratch);
                        &remap_scratch
                    } else {
                        filtered
                    };

                    // Decide priority / marker once per source payload from
                    // the media-aware scheduler (or fall back to defaults).
                    // When the payload is re-chunked below, every chunk
                    // shares this whole-payload hint, so an IDR-bearing
                    // payload duplicates ALL of its chunks across the two
                    // best legs — keyframe protection covers the whole
                    // frame. (The bond sender re-stamps `hints.size` per
                    // datagram itself.)
                    let hints = match media_sched.as_mut() {
                        Some(s) => s.hints_for(remapped),
                        None => PacketHints::default(),
                    };

                    if remapped.len() > chunk_bytes
                        && remapped.len() % TS_PACKET_SIZE == 0
                    {
                        // Re-chunk oversized TS payloads at TS-packet
                        // boundaries so every bond datagram — header, AEAD
                        // and carrier framing included — fits the smallest
                        // leg's path MTU. An IP-fragmented datagram is lost
                        // wholesale on cellular CGNAT paths (fragments
                        // dropped, PMTU discovery black-holed), so fitting
                        // the MTU beats fragment-and-recover. The receiver
                        // drains in bond_seq order, so the TS byte stream
                        // reassembles identically with no receiver change.
                        let payload = bytes::Bytes::copy_from_slice(remapped);
                        let mut off = 0;
                        let mut sent_all = true;
                        while off < payload.len() {
                            let end = (off + chunk_bytes).min(payload.len());
                            if let Err(e) =
                                socket.send(payload.slice(off..end), hints).await
                            {
                                tracing::warn!(
                                    "bonded output '{}' send error: {e}",
                                    config.id
                                );
                                sent_all = false;
                                break;
                            }
                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                            stats
                                .bytes_sent
                                .fetch_add((end - off) as u64, Ordering::Relaxed);
                            off = end;
                        }
                        if sent_all {
                            stats.record_latency(packet.recv_time_us);
                        }
                        continue;
                    }

                    // Not re-chunkable: either the payload already fits, or
                    // it is not 188-byte-aligned TS (non-TS essence) and
                    // slicing it would corrupt downstream framing. A payload
                    // over budget here is sent whole (never dropped) and
                    // flagged — one Warning per output, running count on
                    // `BondLegStats.oversize_payloads` — because it will
                    // IP-fragment on a leg whose MTU it exceeds.
                    if remapped.len() > payload_budget {
                        oversize_payloads.fetch_add(1, Ordering::Relaxed);
                        if !mtu_warned.swap(true, Ordering::Relaxed) {
                            events.emit_flow_with_details(
                                EventSeverity::Warning,
                                category::BOND,
                                format!(
                                    "bonded output '{}': payload {} B exceeds the per-datagram \
                                     budget {} B and is not 188-byte-aligned TS, so it cannot \
                                     be re-chunked — sent whole; IP fragmentation likely on \
                                     WAN legs (reported once; see bond stats oversize_payloads)",
                                    config.id,
                                    remapped.len(),
                                    payload_budget
                                ),
                                flow_id,
                                serde_json::json!({
                                    "error_code": "bond_payload_exceeds_mtu",
                                    "output_id": config.id,
                                    "payload_bytes": remapped.len(),
                                    "budget_bytes": payload_budget,
                                }),
                            );
                        }
                    }

                    // Emit via bond socket. We clone into a fresh Bytes
                    // so the bond layer owns its own refcount; if the
                    // caller holds `packet` elsewhere the copy is
                    // amortised with the scheduler's own send.
                    let payload = bytes::Bytes::copy_from_slice(remapped);
                    if let Err(e) = socket.send(payload, hints).await {
                        tracing::warn!("bonded output '{}' send error: {e}", config.id);
                    } else {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats
                            .bytes_sent
                            .fetch_add(remapped.len() as u64, Ordering::Relaxed);
                        stats.record_latency(packet.recv_time_us);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    tracing::warn!("bonded output '{}' lagged, dropped {n} packets", config.id);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("bonded output '{}' broadcast channel closed", config.id);
                    return Ok(());
                }
            },
        }
    }
}

/// Resolve the per-datagram sizing for a bonded output from its configured
/// (or default 1500) `path_mtu`: `(payload_budget, chunk_bytes)`.
///
/// `payload_budget` is the largest bond payload whose full wire datagram —
/// IP/UDP + carrier framing + bond header + AEAD envelope — fits the
/// smallest leg's path MTU. `chunk_bytes` is the largest whole-TS-packet
/// (N × 188) size within that budget: the re-chunk quantum for oversized TS
/// payloads in the send loop.
///
/// Sized to the WORST leg: the scheduler picks a leg only after `send()`
/// (and a retransmit may ride any leg), so every datagram must fit all of
/// them. Per-datagram overheads:
/// - IP/UDP: 28 B (IPv4), or 48 B when any leg dials an IPv6 literal OR a
///   hostname — `parse_sockaddr` prefers A records but falls back to any
///   family, so an AAAA-only hostname genuinely dials IPv6; the
///   conservative deduction costs at most one TS packet at boundary MTUs
///   and nothing at all at the common ones (1500 and ~1000 both derive the
///   same N either way).
/// - Relay legs: 16 B `tunnel_id` prefix, plus the 28 B tunnel AEAD when
///   the bond itself is unkeyed (fail-closed single-layer rule — a
///   bond-keyed relay leg carries the 29 B `0xBD` envelope counted below
///   instead).
/// - RIST legs: 12 B RIST/RTP framing around each bond frame.
/// - Bond header: 16 B v2 when equalization measures (the default), else
///   12 B v1 — budget against v2 so a boundary-sized chunk doesn't silently
///   fragment once send-stamping activates.
/// - Bond AEAD envelope: 29 B when `encryption_key` is set. QUIC legs skip
///   the envelope but carry ~30 B of their own QUIC framing, so the same
///   deduction is approximately right there; quinn additionally hard-fails
///   (never fragments) anything over its discovered datagram MTU.
/// - FEC repair headroom: a repair datagram carries framing ON TOP of the
///   largest media chunk it protects (combined XOR `FecRepair`: +14 B;
///   per-leg XOR `PerLegRepair`: +10 + 4·rows B seq list; per-leg RS
///   `PerLegRsRepair`: +12 + 4·columns B) — reserve it so repairs fit the
///   MTU too, else FEC silently fragments (and dies) on exactly the
///   constrained path it is meant to protect.
///
/// The 1500 default derives 7 × 188 = 1316 B chunks under every header /
/// crypto combination (without FEC) — today's assembler/SRT datagram, so
/// normal-MTU bonds are unchanged.
fn resolve_datagram_budget(cfg: &BondedOutputConfig) -> (usize, usize) {
    use crate::config::models::{BondFecAlgorithm, BondPathTransportConfig as T};
    let path_mtu = cfg.path_mtu.unwrap_or(DEFAULT_PATH_MTU as u32) as usize;

    // IPv4 literal → 28; IPv6 literal or unresolved hostname → 48
    // (conservative: the dial path falls back to AAAA when A is absent).
    let addr_needs_v6 = |addr: &str| {
        match addr.parse::<std::net::SocketAddr>() {
            Ok(a) => a.is_ipv6(),
            Err(_) => true, // hostname — may resolve to IPv6
        }
    };
    let mut any_v6 = false;
    let mut relay_overhead = 0usize;
    let mut rist_overhead = 0usize;
    for p in &cfg.paths {
        match &p.transport {
            T::Udp { remote, .. } => {
                any_v6 |= remote.as_deref().map(addr_needs_v6).unwrap_or(false);
            }
            T::Quic { addr, .. } => {
                any_v6 |= addr_needs_v6(addr);
            }
            T::Rist { remote, .. } => {
                any_v6 |= remote.as_deref().map(addr_needs_v6).unwrap_or(false);
                rist_overhead = RIST_RTP_OVERHEAD;
            }
            T::Relay { relay_addrs, .. } => {
                any_v6 |= relay_addrs.iter().any(|a| addr_needs_v6(a));
                relay_overhead = RELAY_TUNNEL_ID_OVERHEAD
                    + if cfg.encryption_key.is_some() {
                        0
                    } else {
                        RELAY_TUNNEL_CRYPTO_OVERHEAD
                    };
            }
        }
    }
    let mut fec_headroom = 0usize;
    if cfg.fec.is_some() {
        fec_headroom = 14;
    }
    for p in &cfg.paths {
        if let Some(f) = &p.fec {
            fec_headroom = fec_headroom.max(match f.algorithm {
                Some(BondFecAlgorithm::ReedSolomon) => 12 + 4 * f.columns as usize,
                _ => 10 + 4 * f.rows as usize,
            });
        }
    }
    let ip_udp = if any_v6 {
        IPV6_UDP_OVERHEAD
    } else {
        IPV4_UDP_OVERHEAD
    };
    let bond_header = if super::input_bonded::map_equalization_mode(cfg.equalization).measures() {
        BOND_HEADER_SIZE + 4
    } else {
        BOND_HEADER_SIZE
    };
    let crypto = if cfg.encryption_key.is_some() {
        BOND_CRYPTO_OVERHEAD
    } else {
        0
    };
    // Floor at one TS packet: validation keeps path_mtu ≥ 576 so this only
    // guards a hand-authored pathological config.
    let payload_budget = path_mtu
        .saturating_sub(ip_udp)
        .saturating_sub(relay_overhead.max(rist_overhead))
        .saturating_sub(bond_header)
        .saturating_sub(crypto)
        .saturating_sub(fec_headroom)
        .max(TS_PACKET_SIZE);
    let chunk_bytes = (payload_budget / TS_PACKET_SIZE) * TS_PACKET_SIZE;
    (payload_budget, chunk_bytes)
}

/// Derive the sender retransmit-buffer capacity (in packets) from the bonding
/// latency budget and the aggregate per-leg send ceiling, per the library's
/// own sizing guidance (send rate × longest acceptable NACK round-trip).
/// Floored at the library default so it never regresses below the old fixed
/// 8192-packet behaviour, and capped so a fat/long-budget bond can't request an
/// unbounded ring.
fn derive_retransmit_capacity(cfg: &BondedOutputConfig) -> usize {
    const DEFAULT_CAPACITY: usize = 8192;
    const MAX_CAPACITY: usize = 262_144; // ~345 MB ring ceiling
    const ASSUMED_LEG_BPS: u64 = 20_000_000; // when a leg declares no ceiling
    // The ring is counted in packets, so size against the typical datagram:
    // a low path_mtu re-chunks everything smaller (more packets in the same
    // NACK window → deeper ring), but a jumbo path_mtu does NOT make
    // datagrams bigger — inputs still emit ≤ 1316 B bundles and small
    // payloads are never coalesced up — so cap the divisor at the classic
    // 7 × 188 bundle or the ring would cover only a fraction of the window.
    let (_, chunk_bytes) = resolve_datagram_budget(cfg);
    let typical_datagram = chunk_bytes.min(7 * TS_PACKET_SIZE);
    let budget_ms = cfg.max_bonding_latency_ms.unwrap_or(1000) as u64;
    let aggregate_bps: u64 = cfg
        .paths
        .iter()
        .map(|p| p.max_bitrate_bps.unwrap_or(ASSUMED_LEG_BPS))
        .sum();
    let bits_in_flight = aggregate_bps.saturating_mul(budget_ms) / 1000;
    let derived = (bits_in_flight / (typical_datagram as u64 * 8)) as usize;
    derived.clamp(DEFAULT_CAPACITY, MAX_CAPACITY)
}

fn build_sender_cfg(cfg: &BondedOutputConfig) -> anyhow::Result<BondBuild> {
    let mut out = BondSocketConfig {
        flow_id: cfg.bond_flow_id,
        paths: Vec::with_capacity(cfg.paths.len()),
        ..Default::default()
    };
    // Fail-closed conditional-AEAD decision (must match the bonded INPUT leg):
    // bond-keyed ⇒ the attached path seals 0xBD and the bridge cipher is None;
    // bond-unkeyed ⇒ the leg's tunnel_encryption_key is the single layer.
    let bond_has_key = cfg.encryption_key.is_some();
    let mut attachments: std::collections::HashMap<u8, AttachedChannels> =
        std::collections::HashMap::new();
    let mut bridges: Vec<RelayLegBridge> = Vec::new();
    if let Some(ms) = cfg.keepalive_ms {
        out.keepalive_interval = std::time::Duration::from_millis(ms as u64);
    }
    // Retransmit-buffer capacity: an explicit operator value wins; otherwise
    // derive it the way the library asks the caller to — send rate × the
    // longest acceptable NACK round-trip (the bonding latency budget). We size
    // it from the aggregate per-leg ceiling (max_bitrate_bps, or a generous
    // 20 Mbps/leg assumption when a leg has no cap) over the budget, floored at
    // the library default so we never go below today's behaviour.
    out.retransmit_capacity = cfg
        .retransmit_capacity
        .unwrap_or_else(|| derive_retransmit_capacity(cfg));
    // Per-leg equalization (sender side): in auto/on, stamp data packets so the
    // receiver can measure relative one-way delay and time-align the legs.
    out.equalization = super::input_bonded::map_equalization_mode(cfg.equalization);
    // Ride-fastest (duplicate-all redundancy) → signal the receiver to suppress
    // alignment: holding the fast copy to align a slow duplicate the receiver
    // would deliver immediately defeats the point. Threshold redundancy keeps
    // alignment on (only the duplicated minority pays a small intended hold).
    out.align_suppress = matches!(
        cfg.redundancy.as_ref().map(|r| r.mode),
        Some(BondRedundancyMode::All)
    );
    if let Some(hex_key) = &cfg.encryption_key {
        out.encryption_key = Some(
            super::input_bonded::decode_bond_key(hex_key)
                .map_err(|e| anyhow::anyhow!("bonded output '{}': {e}", cfg.id))?,
        );
    }
    if let Some(f) = &cfg.fec {
        out.fec = Some(FecParams {
            columns: f.columns,
            rows: f.rows,
        });
    }
    // Per-leg FEC: each path that carries its own `fec` runs an independent
    // encoder over only that leg's packets, emitting repairs on the same leg
    // (validation rejects mixing this with the combined `fec` above). A
    // non-empty map selects per-leg mode on the sender.
    for p in &cfg.paths {
        if let Some(f) = &p.fec {
            out.per_path_fec
                .insert(p.id, super::input_bonded::build_per_leg_fec(f));
        }
    }
    for p in &cfg.paths {
        // A bonded OUTPUT relay leg registers `egress` with the relay (it sends
        // media; the matching bonded input leg registers `ingress`). Relay legs
        // need in-process bridge channels, so they go through `prepare_relay_leg`
        // rather than the plain transport translation.
        let transport = if matches!(
            &p.transport,
            crate::config::models::BondPathTransportConfig::Relay { .. }
        ) {
            let (att, bridge, t) = prepare_relay_leg(p, bond_has_key, TunnelDirection::Egress)?;
            attachments.insert(p.id, att);
            bridges.push(bridge);
            t
        } else {
            translate_transport_for_sender(&p.transport)?
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

/// Translate the edge's operator-facing congestion knobs into the
/// library `CongestionConfig`, leaving any unset field at its
/// broadcast-tuned default.
/// Map the edge's bonded-output `redundancy` config to the bonding layer's
/// [`RedundancyPolicy`]. Absent / `off` → no extra replication.
fn build_redundancy(c: &Option<BondRedundancyConfig>) -> RedundancyPolicy {
    let Some(c) = c else {
        return RedundancyPolicy::default();
    };
    let mode = match c.mode {
        BondRedundancyMode::Off => RedundancyMode::Off,
        BondRedundancyMode::All => RedundancyMode::All,
        BondRedundancyMode::Threshold => {
            let p = match c.min_priority.unwrap_or(BondRedundancyPriority::High) {
                BondRedundancyPriority::Normal => Priority::Normal,
                BondRedundancyPriority::High => Priority::High,
                BondRedundancyPriority::Critical => Priority::Critical,
            };
            RedundancyMode::AtOrAbove(p)
        }
    };
    RedundancyPolicy {
        mode,
        replicas: c.replicas.unwrap_or(2).max(2) as usize,
    }
}

/// The edge's baseline congestion tuning. Differs from the library default
/// in ONE field: `delay_inflation_auto` is ON. The edge's headline bond is a
/// heterogeneous cellular + satellite contribution bond, where a leg's RTT
/// baseline (5G ~100 ms, Starlink ~600 ms) is far above the fixed 40 ms
/// `delay_inflation` floor; auto-deriving the queue-building threshold from
/// each leg's own windowed baseline stops normal radio jitter from reading as
/// congestion (a terrestrial low-RTT leg still keeps the tight 40 ms floor).
/// An operator can still force it off with `delay_inflation_auto: false`.
fn edge_default_congestion() -> CongestionConfig {
    CongestionConfig {
        delay_inflation_auto: true,
        ..CongestionConfig::default()
    }
}

fn build_congestion(c: &BondCongestionConfig) -> CongestionConfig {
    let mut cc = edge_default_congestion();
    if let Some(v) = c.min_rate_kbps {
        cc.min_rate_bps = v as u64 * 1000;
    }
    if let Some(v) = c.start_rate_kbps {
        cc.start_rate_bps = v as u64 * 1000;
    }
    if let Some(v) = c.loss_low_pct {
        cc.loss_low = v / 100.0;
    }
    if let Some(v) = c.loss_high_pct {
        cc.loss_high = v / 100.0;
    }
    if let Some(v) = c.delay_inflation_ms {
        cc.delay_inflation = std::time::Duration::from_millis(v as u64);
    }
    if let Some(v) = c.burst_ms {
        cc.burst_secs = v as f32 / 1000.0;
    }
    if let Some(v) = c.probe_cap_mult {
        cc.probe_cap_mult = v;
    }
    if let Some(v) = c.rtt_min_window_ms {
        cc.rtt_min_window = std::time::Duration::from_millis(v);
    }
    if let Some(v) = c.delay_inflation_auto {
        cc.delay_inflation_auto = v;
    }
    if let Some(v) = c.jitter_demote_ms {
        cc.jitter_demote_us = v as u64 * 1000;
    }
    cc
}

fn translate_transport_for_sender(
    t: &crate::config::models::BondPathTransportConfig,
) -> anyhow::Result<BondPathTxTransport> {
    use crate::config::models::{BondPathTransportConfig, BondRistRole};
    use bonding_transport::{QuicRole as BondQuicRoleTx, RistRole as BondRistRoleTx};
    Ok(match t {
        // Relay legs carry in-process bridge channels and are translated via
        // `prepare_relay_leg` (which builds the `Attached` transport), never here.
        BondPathTransportConfig::Relay { .. } => {
            anyhow::bail!("internal: relay bond leg must be prepared via prepare_relay_leg")
        }
        BondPathTransportConfig::Udp {
            bind,
            remote,
            interface,
            gateway,
            source,
        } => {
            if gateway.is_some() {
                // Gateway mode: bind to the source IP so the policy
                // rule (programmed by engine::bond_routing) matches
                // and steers this leg via its router. No
                // SO_BINDTODEVICE pin (that needs CAP_NET_RAW); the
                // route does the steering.
                let src = source.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("gateway-mode bonded path requires `source`")
                })?;
                let src_net = super::bond_routing::SourceNet::parse(src)?;
                BondPathTxTransport::Udp {
                    bind: Some(std::net::SocketAddr::new(src_net.addr, 0)),
                    remote: remote.as_deref().map(parse_sockaddr).transpose()?,
                    interface: None,
                }
            } else {
                BondPathTxTransport::Udp {
                    bind: bind.as_deref().map(parse_sockaddr).transpose()?,
                    remote: remote.as_deref().map(parse_sockaddr).transpose()?,
                    interface: interface.clone(),
                }
            }
        }
        BondPathTransportConfig::Rist {
            role,
            remote,
            local_bind,
            buffer_ms,
        } => BondPathTxTransport::Rist {
            role: match role {
                BondRistRole::Sender => BondRistRoleTx::Sender,
                BondRistRole::Receiver => BondRistRoleTx::Receiver,
            },
            remote: remote.as_deref().map(parse_sockaddr).transpose()?,
            local_bind: local_bind.as_deref().map(parse_sockaddr).transpose()?,
            buffer_ms: *buffer_ms,
        },
        BondPathTransportConfig::Quic {
            role: _,
            addr,
            server_name,
            tls,
            bind,
            interface,
        } => BondPathTxTransport::Quic {
            // Role is auto-derived from the side: a bonded output (sender) leg is
            // always the QUIC client. Any value carried in the config is ignored.
            role: BondQuicRoleTx::Client,
            addr: parse_sockaddr(addr)?,
            server_name: server_name.clone(),
            tls: translate_tls(tls)?,
            bind: bind.as_deref().map(parse_sockaddr).transpose()?,
            interface: interface.clone(),
        },
    })
}

#[cfg(test)]
mod bonded_output_tests {
    use super::*;

    #[test]
    fn build_congestion_maps_operator_knobs_to_library_units() {
        let c = crate::config::models::BondCongestionConfig {
            min_rate_kbps: Some(1200),
            delay_inflation_ms: Some(250),
            loss_high_pct: Some(6.0),
            ..Default::default()
        };
        let cc = build_congestion(&c);
        assert_eq!(cc.min_rate_bps, 1_200_000, "kbps → bps ×1000");
        assert_eq!(cc.delay_inflation, std::time::Duration::from_millis(250));
        assert!((cc.loss_high - 0.06).abs() < 1e-6, "pct → fraction");
        // Edge default: auto delay threshold ON (heterogeneous cellular+sat
        // bond) unless the operator explicitly turns it off.
        assert!(cc.delay_inflation_auto, "edge default enables delay_inflation_auto");
        assert!(
            edge_default_congestion().delay_inflation_auto,
            "edge_default_congestion() base enables delay_inflation_auto"
        );
        let off = build_congestion(&crate::config::models::BondCongestionConfig {
            delay_inflation_auto: Some(false),
            ..Default::default()
        });
        assert!(!off.delay_inflation_auto, "operator can still force it off");
    }

    #[test]
    fn build_sender_cfg_maps_every_path_and_flow_id() {
        let cfg: crate::config::models::BondedOutputConfig = serde_json::from_str(
            r#"{ "id": "o1", "name": "bond", "bond_flow_id": 6016, "keepalive_ms": 200,
                 "paths": [
                   { "id": 0, "name": "a", "transport": { "type": "udp", "remote": "1.2.3.4:7400" } },
                   { "id": 1, "name": "b", "transport": { "type": "udp", "remote": "1.2.3.4:7401" } }
                 ] }"#,
        )
        .unwrap();
        let sock = build_sender_cfg(&cfg).expect("sender cfg builds").cfg;
        assert_eq!(sock.flow_id, 6016);
        assert_eq!(sock.paths.len(), 2);
    }

    fn budget_cfg(json: &str) -> crate::config::models::BondedOutputConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn datagram_budget_default_mtu_derives_1316_chunks() {
        // Encrypted + default (auto) equalization — the wizard's common shape.
        // 1500 − 28 (IPv4/UDP) − 16 (v2 header) − 29 (AEAD) = 1427 → 7 × 188.
        let cfg = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[{"id":0,"name":"a","transport":{"type":"udp","remote":"1.2.3.4:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&cfg), (1427, 1316));

        // Cleartext + equalization off (v1 12 B header): 1500 − 28 − 12 = 1460
        // → still 7 packets. Every default-MTU combination lands on 1316.
        let cfg = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1, "equalization":"off",
                 "paths":[{"id":0,"name":"a","transport":{"type":"udp","remote":"1.2.3.4:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&cfg), (1460, 1316));
    }

    #[test]
    fn datagram_budget_cellular_1000_mtu() {
        // The live odt500 case: measured path MTU ≈ 1000, encrypted, v2
        // header. 1000 − 28 − 16 − 29 = 927 → 4 × 188 = 752 B datagrams
        // (825 B on the wire — no fragmentation).
        let cfg = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1, "path_mtu":1000,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[{"id":0,"name":"a","transport":{"type":"udp","remote":"1.2.3.4:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&cfg), (927, 752));
    }

    #[test]
    fn datagram_budget_relay_leg_overheads() {
        // Bond-keyed relay leg: +16 tunnel_id only (bond envelope is the one
        // crypto layer). 1500 − 28 − 16 − 16 − 29 = 1411 → 1316.
        let keyed = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[{"id":0,"name":"r","transport":{"type":"relay",
                   "tunnel_id":"9f8b3c1e-0000-4000-8000-000000000001",
                   "relay_addrs":["203.0.113.9:4434"]}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&keyed), (1411, 1316));

        // Bond-unkeyed relay leg: +16 tunnel_id +28 leg AEAD, no bond
        // envelope. 1500 − 28 − 44 − 16 = 1412 → 1316.
        let unkeyed = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1,
                 "paths":[{"id":0,"name":"r","transport":{"type":"relay",
                   "tunnel_id":"9f8b3c1e-0000-4000-8000-000000000001",
                   "relay_addrs":["203.0.113.9:4434"],
                   "tunnel_encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&unkeyed), (1412, 1316));

        // Mixed direct + relay at cellular MTU sizes to the worst (relay) leg:
        // 1000 − 28 − 16 − 16 − 29 = 911 → 4 × 188 = 752.
        let mixed = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1, "path_mtu":1000,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[
                   {"id":0,"name":"d","transport":{"type":"udp","remote":"1.2.3.4:7400"}},
                   {"id":1,"name":"r","transport":{"type":"relay",
                     "tunnel_id":"9f8b3c1e-0000-4000-8000-000000000001",
                     "relay_addrs":["203.0.113.9:4434"]}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&mixed), (911, 752));
    }

    #[test]
    fn datagram_budget_ipv6_literal_leg() {
        // An IPv6-literal leg costs 20 B more of IP header. At a boundary MTU
        // this drops one TS packet: 1024 − 28 − 16 − 29 = 951 → 5 packets on
        // v4, but 1024 − 48 − 16 − 29 = 931 → 4 packets on v6.
        let v4 = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1, "path_mtu":1024,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[{"id":0,"name":"a","transport":{"type":"udp","remote":"1.2.3.4:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&v4), (951, 940));
        let v6 = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1, "path_mtu":1024,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[{"id":0,"name":"a","transport":{"type":"udp","remote":"[2001:db8::1]:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&v6), (931, 752));
    }

    #[test]
    fn datagram_budget_hostname_leg_assumes_ipv6() {
        // A hostname remote may resolve AAAA-only (parse_sockaddr falls back
        // to any family), so the budget must deduct the IPv6 header size.
        // At the same 1024 boundary as the literal test: 1024−48−16−29 = 931
        // → 4 packets (vs 5 for a v4 literal).
        let cfg = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1, "path_mtu":1024,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[{"id":0,"name":"a","transport":{"type":"udp","remote":"relay.example.com:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&cfg), (931, 752));
    }

    #[test]
    fn datagram_budget_reserves_fec_repair_headroom() {
        // Combined XOR FEC repairs carry chunk+14 of bond payload; without
        // headroom a zero-slack budget (1013−28−16−29 = 940 = 5×188 exactly)
        // would emit repairs that fragment on exactly the constrained path.
        // With the 14 B reserve: 940−14 = 926 → 4 × 188 = 752.
        let combined = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1, "path_mtu":1013,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "fec": { "columns": 10, "rows": 10 },
                 "paths":[{"id":0,"name":"a","transport":{"type":"udp","remote":"1.2.3.4:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&combined), (926, 752));

        // Per-leg XOR repairs additionally carry the 4·rows seq list:
        // headroom 10 + 4×5 = 30 → 1500−28−16−29−30 = 1397 → 7 × 188 still.
        let per_leg = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[{"id":0,"name":"a",
                   "fec": { "columns": 5, "rows": 5 },
                   "transport":{"type":"udp","remote":"1.2.3.4:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&per_leg), (1397, 1316));
    }

    #[test]
    fn datagram_budget_rist_leg_rtp_framing() {
        // A RIST leg wraps each bond frame in its own 12 B RTP header.
        // 1000−28−12−16−29 = 915 → 4 × 188 = 752 (budget drops by 12 vs the
        // plain-UDP 927).
        let cfg = budget_cfg(
            r#"{ "id":"o","name":"b","bond_flow_id":1, "path_mtu":1000,
                 "encryption_key":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                 "paths":[{"id":0,"name":"r","transport":{"type":"rist","role":"sender","remote":"1.2.3.4:7400"}}] }"#,
        );
        assert_eq!(resolve_datagram_budget(&cfg), (915, 752));
    }

    #[test]
    fn retransmit_capacity_jumbo_mtu_does_not_shrink_ring() {
        // Inputs still emit ≤1316 B bundles under a jumbo path_mtu — the
        // NACK ring must be sized against the typical datagram, not the
        // (never-reached) jumbo chunk ceiling.
        let mk = |mtu: &str| {
            budget_cfg(&format!(
                r#"{{ "id":"o","name":"b","bond_flow_id":1, {mtu}
                     "max_bonding_latency_ms": 2000,
                     "paths":[{{"id":0,"name":"a","max_bitrate_bps":100000000,
                       "transport":{{"type":"udp","remote":"1.2.3.4:7400"}}}}] }}"#
            ))
        };
        assert_eq!(
            derive_retransmit_capacity(&mk(r#""path_mtu":9000,"#)),
            derive_retransmit_capacity(&mk("")),
            "jumbo path_mtu must not undersize the retransmit ring"
        );
    }

    #[test]
    fn retransmit_capacity_grows_when_datagrams_shrink() {
        // Same bitrate + latency budget: a 1000 B path MTU halves-ish the
        // datagram size, so the packet-counted NACK ring must grow to cover
        // the same wallclock window.
        let mk = |mtu: &str| {
            budget_cfg(&format!(
                r#"{{ "id":"o","name":"b","bond_flow_id":1, {mtu}
                     "max_bonding_latency_ms": 2000,
                     "paths":[{{"id":0,"name":"a","max_bitrate_bps":100000000,
                       "transport":{{"type":"udp","remote":"1.2.3.4:7400"}}}}] }}"#
            ))
        };
        let default_ring = derive_retransmit_capacity(&mk(""));
        let cellular_ring = derive_retransmit_capacity(&mk(r#""path_mtu":1000,"#));
        assert!(
            cellular_ring > default_ring,
            "smaller datagrams need a deeper packet-counted ring \
             ({cellular_ring} vs {default_ring})"
        );
    }

    #[test]
    fn sender_align_suppress_and_equalization_mode() {
        use bonding_transport::EqualizationMode as Tx;
        let legs = r#""paths":[{"id":0,"name":"a","transport":{"type":"udp","remote":"1.2.3.4:7400"}}]"#;
        let build = |json: &str| {
            let cfg: crate::config::models::BondedOutputConfig =
                serde_json::from_str(json).unwrap();
            build_sender_cfg(&cfg).expect("sender cfg builds").cfg
        };

        // Default (no equalization, no redundancy) → Auto, no suppression.
        let s = build(&format!(r#"{{ "id":"o","name":"b","bond_flow_id":1, {legs} }}"#));
        assert_eq!(s.equalization, Tx::Auto);
        assert!(!s.align_suppress);

        // Duplicate-all redundancy → align_suppress set (ride-fastest).
        let s = build(&format!(
            r#"{{ "id":"o","name":"b","bond_flow_id":1, "redundancy":{{"mode":"all"}}, {legs} }}"#
        ));
        assert!(s.align_suppress, "duplicate-all must suppress alignment");

        // Threshold redundancy → NOT suppressed (the bulk still aggregates).
        let s = build(&format!(
            r#"{{ "id":"o","name":"b","bond_flow_id":1, "redundancy":{{"mode":"threshold","min_priority":"high"}}, {legs} }}"#
        ));
        assert!(!s.align_suppress, "threshold redundancy keeps alignment on");

        // `off` mode + legacy `false` map through.
        let s = build(&format!(r#"{{ "id":"o","name":"b","bond_flow_id":1, "equalization":"off", {legs} }}"#));
        assert_eq!(s.equalization, Tx::Off);
        let s = build(&format!(r#"{{ "id":"o","name":"b","bond_flow_id":1, "equalization":false, {legs} }}"#));
        assert_eq!(s.equalization, Tx::Off);
    }
}
