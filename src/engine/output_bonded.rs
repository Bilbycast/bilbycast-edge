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
    BondSocket, BondSocketConfig, CapacityAwareScheduler, CongestionConfig, FecParams, PacketHints,
    PathConfig as BondPathTxCfg, PathEvent, PathEventKind, PathPrior,
    PathTransport as BondPathTxTransport, RoundRobinScheduler, WeightedRttScheduler,
};

use crate::config::models::{BondCongestionConfig, BondedOutputConfig, BondSchedulerKind};
use crate::manager::events::{
    BondEventScope, EventSender, EventSeverity, category, run_bond_event_forwarder,
};
use crate::stats::collector::OutputStatsAccumulator;

use super::input_bonded::{parse_sockaddr, translate_tls};
use super::packet::RtpPacket;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;

const RTP_HEADER_MIN_SIZE: usize = 12;

/// Conservative per-datagram wire budget for a bonded leg, bytes.
/// 1400 clears the common WAN MTU floor (PPPoE / LTE carrier tunnels /
/// Starlink) under the 1500-byte ethernet default — a bonded payload
/// above the derived budget fragments at the IP layer, and one lost
/// fragment costs the whole datagram on a lossy cellular leg.
const BOND_WIRE_MTU: usize = 1400;
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

async fn bonded_output_run(
    config: &BondedOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    let socket_cfg = build_sender_cfg(config)?;
    let path_ids: Vec<u8> = socket_cfg.paths.iter().map(|p| p.id).collect();

    // Shared handles to the adaptive scheduler's discovered per-leg
    // capacity (bits/sec) — empty for the non-adaptive policies.
    let mut capacity_handles: Vec<(u8, std::sync::Arc<std::sync::atomic::AtomicU64>)> = Vec::new();

    // Pick scheduler. MediaAware/Adaptive add NAL awareness on top of
    // the inner path-selection policy; see engine::bonded_scheduler.
    let socket = match config.scheduler {
        BondSchedulerKind::RoundRobin => {
            BondSocket::sender(socket_cfg, RoundRobinScheduler::new(path_ids.clone()))
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
        BondSchedulerKind::WeightedRtt => {
            BondSocket::sender(socket_cfg, WeightedRttScheduler::new(path_ids.clone()))
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
        BondSchedulerKind::MediaAware => {
            // MediaAware scheduler only influences the *hints* we pass
            // to send(); the inner scheduler is WeightedRtt so
            // `Critical`-priority packets duplicate across the two
            // lowest-RTT paths.
            BondSocket::sender(socket_cfg, WeightedRttScheduler::new(path_ids.clone()))
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
            let cong = config
                .congestion
                .as_ref()
                .map(build_congestion)
                .unwrap_or_default();
            let sched = CapacityAwareScheduler::with_paths(priors, cong);
            capacity_handles = sched.capacity_handles();
            BondSocket::sender(socket_cfg, sched)
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
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

    // Per-datagram payload budget under the WAN MTU floor: bond header
    // plus (when encryption is on) the AEAD envelope ride on every
    // datagram. Oversize payloads are sent anyway (the bond layer
    // neither drops nor fragments) but flagged once per output —
    // IP-layer fragmentation on a lossy cellular leg multiplies loss.
    let payload_budget = BOND_WIRE_MTU
        - BOND_HEADER_SIZE
        - if config.encryption_key.is_some() {
            BOND_CRYPTO_OVERHEAD
        } else {
            0
        };
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

                    // MTU guard: flag (never drop or fragment) payloads
                    // that will exceed the per-datagram wire budget.
                    // One Warning per output; the running count rides
                    // on `BondLegStats.oversize_payloads`.
                    if remapped.len() > payload_budget {
                        oversize_payloads.fetch_add(1, Ordering::Relaxed);
                        if !mtu_warned.swap(true, Ordering::Relaxed) {
                            events.emit_flow_with_details(
                                EventSeverity::Warning,
                                category::BOND,
                                format!(
                                    "bonded output '{}': payload {} B exceeds the per-datagram \
                                     budget {} B — IP fragmentation likely on WAN legs \
                                     (reported once; see bond stats oversize_payloads)",
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

                    // Decide priority / marker from media-aware
                    // scheduler (or fall back to defaults).
                    let hints = match media_sched.as_mut() {
                        Some(s) => s.hints_for(remapped),
                        None => PacketHints::default(),
                    };

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

fn build_sender_cfg(cfg: &BondedOutputConfig) -> anyhow::Result<BondSocketConfig> {
    let mut out = BondSocketConfig {
        flow_id: cfg.bond_flow_id,
        paths: Vec::with_capacity(cfg.paths.len()),
        ..Default::default()
    };
    if let Some(ms) = cfg.keepalive_ms {
        out.keepalive_interval = std::time::Duration::from_millis(ms as u64);
    }
    if let Some(cap) = cfg.retransmit_capacity {
        out.retransmit_capacity = cap;
    }
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
    for p in &cfg.paths {
        out.paths.push(BondPathTxCfg {
            id: p.id,
            name: p.name.clone(),
            weight_hint: p.weight_hint,
            transport: translate_transport_for_sender(&p.transport)?,
        });
    }
    Ok(out)
}

/// Translate the edge's operator-facing congestion knobs into the
/// library `CongestionConfig`, leaving any unset field at its
/// broadcast-tuned default.
fn build_congestion(c: &BondCongestionConfig) -> CongestionConfig {
    let mut cc = CongestionConfig::default();
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
    cc
}

fn translate_transport_for_sender(
    t: &crate::config::models::BondPathTransportConfig,
) -> anyhow::Result<BondPathTxTransport> {
    use crate::config::models::{BondPathTransportConfig, BondQuicRole, BondRistRole};
    use bonding_transport::{QuicRole as BondQuicRoleTx, RistRole as BondRistRoleTx};
    Ok(match t {
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
            role,
            addr,
            server_name,
            tls,
        } => BondPathTxTransport::Quic {
            role: match role {
                BondQuicRole::Client => BondQuicRoleTx::Client,
                BondQuicRole::Server => BondQuicRoleTx::Server,
            },
            addr: parse_sockaddr(addr)?,
            server_name: server_name.clone(),
            tls: translate_tls(tls)?,
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
        let sock = build_sender_cfg(&cfg).expect("sender cfg builds");
        assert_eq!(sock.flow_id, 6016);
        assert_eq!(sock.paths.len(), 2);
    }
}
