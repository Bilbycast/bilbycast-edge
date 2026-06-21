// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Active per-leg capacity / reachability probe (Phase 2 of bond leg
//! testing) — measures real Mbps / loss / RTT over a single bond leg by
//! driving synthetic traffic to a cooperating peer **responder** and
//! reading back the delivered-rate it confirms.
//!
//! ## Zero-impact guarantees (the load-bearing requirement)
//!
//! Unlike the Phase 1 usability check (`engine::bond_leg_test`, zero
//! packets), this *does* generate bounded synthetic traffic. It must
//! still never affect live media, so:
//!
//! 1. **Idle-gated** — [`busy_keys`] reports the interfaces / source IPs /
//!    gateways currently held by *running* bonded flows (registered via
//!    [`register_active_legs`]). [`capacity_conflict`] refuses the probe
//!    if the leg under test shares any of them. Saturating a shared
//!    physical link is the one thing that would affect a feed, so it is
//!    never done while that link carries traffic.
//! 2. **Dedicated socket + probe port** — the probe binds its own
//!    ephemeral socket (honouring the leg's interface/source binding so it
//!    egresses the *same* physical leg) and targets a dedicated responder
//!    port, never the production `BondSocket` or a live media port.
//! 3. **Bounded + capped** — hard total-duration cap and a `max_bps`
//!    ceiling (honour a metered cellular leg's `max_bitrate_bps`).
//! 4. **Off the hot path** — ordinary Tokio tasks, not the SCHED_FIFO
//!    wire-emit threads.
//!
//! Wire format: a dedicated `BPRB` magic, distinct from the bond data
//! (`0xBC`) and control (`0xBE`) magics, so a probe datagram can never be
//! mistaken for — or land in — a live input's accounting.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::config::models::BondPathConfig;
use crate::engine::bond_leg_test::{bind_leg_socket, ip_only, normalize, LegNet};
use crate::util::network_interfaces;

/// Probe wire magic — deliberately distinct from `0xBC` (bond data) and
/// `0xBE` (bond control).
const MAGIC: &[u8; 4] = b"BPRB";
const PKT_PROBE: u8 = 1;
const PKT_ECHO: u8 = 2;
/// Probe header bytes: magic(4) + type(1) + seq(4) + send_us(8).
const PROBE_HDR: usize = 17;
/// Echo bytes: header(17) + recv_count(8) + recv_bytes(8).
const ECHO_LEN: usize = 33;

/// Process-monotonic microsecond clock for embedding into probe packets
/// (so an echo round-trips the send instant for RTT).
fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

// ── Responder ───────────────────────────────────────────────────────────────

fn responders() -> &'static StdMutex<HashMap<u16, CancellationToken>> {
    static R: OnceLock<StdMutex<HashMap<u16, CancellationToken>>> = OnceLock::new();
    R.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Bind a stateless echo responder on `bind` for `duration`, then return
/// the bound port. Each received probe is echoed back carrying the
/// responder's cumulative `(recv_count, recv_bytes)` so the sender can
/// compute delivered-rate + loss even if some echoes are lost on the
/// return path. Auto-expires after `duration`; cancellable via
/// [`stop_responder`]. Isolated from every live socket.
pub async fn start_responder(bind: SocketAddr, duration: Duration) -> Result<u16, String> {
    let sock = UdpSocket::bind(bind)
        .await
        .map_err(|e| format!("probe responder bind {bind}: {e}"))?;
    let port = sock.local_addr().map_err(|e| e.to_string())?.port();
    let token = CancellationToken::new();
    responders().lock().unwrap().insert(port, token.clone());

    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut recv_count: u64 = 0;
        let mut recv_bytes: u64 = 0;
        let sleep = tokio::time::sleep(duration);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = &mut sleep => break,
                r = sock.recv_from(&mut buf) => {
                    let (n, from) = match r { Ok(v) => v, Err(_) => continue };
                    if n < PROBE_HDR || &buf[0..4] != MAGIC || buf[4] != PKT_PROBE {
                        continue;
                    }
                    recv_count += 1;
                    recv_bytes += n as u64;
                    let mut echo = [0u8; ECHO_LEN];
                    echo[0..4].copy_from_slice(MAGIC);
                    echo[4] = PKT_ECHO;
                    echo[5..9].copy_from_slice(&buf[5..9]);   // seq
                    echo[9..17].copy_from_slice(&buf[9..17]); // send_us
                    echo[17..25].copy_from_slice(&recv_count.to_be_bytes());
                    echo[25..33].copy_from_slice(&recv_bytes.to_be_bytes());
                    let _ = sock.send_to(&echo, from).await;
                }
            }
        }
        responders().lock().unwrap().remove(&port);
    });

    Ok(port)
}

/// Cancel a running responder by port. Returns whether one was found.
pub fn stop_responder(port: u16) -> bool {
    if let Some(tok) = responders().lock().unwrap().remove(&port) {
        tok.cancel();
        true
    } else {
        false
    }
}

// ── Active-leg registry (authoritative idle-safety gate) ──────────────────────

/// The network identity of one active bond leg, used to detect whether a
/// capacity probe would share a physical link with live traffic.
#[derive(Clone, Debug)]
pub struct LegKey {
    pub interface: Option<String>,
    pub source_ip: Option<IpAddr>,
    pub gateway_ip: Option<IpAddr>,
}

/// Snapshot of every interface / source IP / gateway in use by running
/// bonded flows.
#[derive(Default, Debug)]
pub struct BusyKeys {
    pub interfaces: HashSet<String>,
    pub sources: HashSet<IpAddr>,
    pub gateways: HashSet<IpAddr>,
}

impl BusyKeys {
    fn is_empty(&self) -> bool {
        self.interfaces.is_empty() && self.sources.is_empty() && self.gateways.is_empty()
    }
}

fn active_legs() -> &'static StdMutex<Vec<(String, LegKey)>> {
    static A: OnceLock<StdMutex<Vec<(String, LegKey)>>> = OnceLock::new();
    A.get_or_init(|| StdMutex::new(Vec::new()))
}

/// Derive the leg keys for a bonded output/input's path set.
pub fn leg_keys_for_paths(paths: &[BondPathConfig]) -> Vec<LegKey> {
    paths
        .iter()
        .map(|p| {
            let net = normalize(&p.transport);
            LegKey {
                interface: net.interface.clone(),
                source_ip: net.source.as_deref().and_then(ip_only),
                gateway_ip: net.gateway.as_deref().and_then(ip_only),
            }
        })
        .collect()
}

/// RAII registration of a running bonded flow's legs. Dropping it (on
/// flow stop / teardown) removes them from the busy set.
pub struct ActiveLegGuard {
    flow_id: String,
}

impl Drop for ActiveLegGuard {
    fn drop(&mut self) {
        active_legs().lock().unwrap().retain(|(fid, _)| fid != &self.flow_id);
    }
}

/// Register a running flow's bonded legs as busy. Hold the returned guard
/// for the flow's lifetime.
pub fn register_active_legs(flow_id: &str, keys: Vec<LegKey>) -> ActiveLegGuard {
    let mut legs = active_legs().lock().unwrap();
    legs.retain(|(fid, _)| fid != flow_id); // idempotent re-register
    for k in keys {
        legs.push((flow_id.to_string(), k));
    }
    ActiveLegGuard { flow_id: flow_id.to_string() }
}

/// Snapshot the interfaces / sources / gateways currently in use.
pub fn busy_keys() -> BusyKeys {
    let legs = active_legs().lock().unwrap();
    let mut b = BusyKeys::default();
    for (_, k) in legs.iter() {
        if let Some(i) = &k.interface {
            b.interfaces.insert(i.clone());
        }
        if let Some(s) = k.source_ip {
            b.sources.insert(s);
        }
        if let Some(g) = k.gateway_ip {
            b.gateways.insert(g);
        }
    }
    b
}

/// Refuse a capacity probe that would share a physical link with live
/// traffic. `None` = safe to probe.
pub(crate) fn capacity_conflict(net: &LegNet, busy: &BusyKeys) -> Option<String> {
    if let Some(i) = &net.interface {
        if busy.interfaces.contains(i) {
            return Some(format!("interface '{i}' is in use by a running bonded flow"));
        }
    }
    if let Some(ip) = net.source.as_deref().and_then(ip_only) {
        if busy.sources.contains(&ip) {
            return Some(format!("source {ip} is in use by a running bonded flow"));
        }
    }
    if let Some(ip) = net.gateway.as_deref().and_then(ip_only) {
        if busy.gateways.contains(&ip) {
            return Some(format!("gateway {ip} is in use by a running bonded flow"));
        }
    }
    if net.interface.is_none() && net.source.is_none() && !busy.is_empty() {
        return Some(
            "this leg has no NIC/source pin (default route); refusing the capacity probe while bonded flows are running — it would share the default uplink".to_string(),
        );
    }
    None
}

// ── Client (sender) ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeMode {
    Reachability,
    Capacity,
}

/// Tuning for the capacity ramp.
#[derive(Clone, Copy, Debug)]
pub struct CapacityOpts {
    pub duration: Duration,
    pub max_bps: Option<u64>,
    pub start_bps: u64,
    pub packet_bytes: usize,
}

impl Default for CapacityOpts {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(10),
            max_bps: None,
            start_bps: 2_000_000,
            packet_bytes: 1200,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct ProbeSample {
    pub round: u32,
    pub target_bps: u64,
    pub delivered_bps: u64,
    pub loss_pct: f64,
    pub rtt_ms: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct ProbeReport {
    pub ok: bool,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Best clean delivered rate observed (capacity mode). 0 for pure
    /// reachability.
    pub measured_bps: u64,
    /// Rate at which loss / RTT-inflation first appeared, if a knee was
    /// found before the duration / ceiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knee_bps: Option<u64>,
    pub loss_pct: f64,
    pub rtt_ms: f64,
    pub rtt_ms_min: f64,
    pub samples: Vec<ProbeSample>,
    pub note: String,
}

impl ProbeReport {
    fn err(mode: ProbeMode, msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            mode: mode_str(mode).to_string(),
            error: Some(msg.into()),
            measured_bps: 0,
            knee_bps: None,
            loss_pct: 0.0,
            rtt_ms: 0.0,
            rtt_ms_min: 0.0,
            samples: Vec::new(),
            note: String::new(),
        }
    }
}

fn mode_str(m: ProbeMode) -> &'static str {
    match m {
        ProbeMode::Reachability => "reachability",
        ProbeMode::Capacity => "capacity",
    }
}

#[derive(Default)]
struct RecvState {
    recv_count: u64,
    recv_bytes: u64,
    echoes: u64,
    rtt_us_min: u64,
    rtt_us_last: u64,
    rtts: Vec<u64>,
}

/// Spawn a task draining echoes into shared state until cancelled.
fn spawn_receiver(sock: Arc<UdpSocket>, state: Arc<StdMutex<RecvState>>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                r = sock.recv(&mut buf) => {
                    let n = match r { Ok(n) => n, Err(_) => continue };
                    if n < ECHO_LEN || &buf[0..4] != MAGIC || buf[4] != PKT_ECHO {
                        continue;
                    }
                    let send_us = be_u64(&buf[9..17]);
                    let rc = be_u64(&buf[17..25]);
                    let rb = be_u64(&buf[25..33]);
                    let rtt = now_us().saturating_sub(send_us);
                    let mut s = state.lock().unwrap();
                    s.recv_count = s.recv_count.max(rc);
                    s.recv_bytes = s.recv_bytes.max(rb);
                    s.echoes += 1;
                    s.rtt_us_last = rtt;
                    s.rtt_us_min = if s.rtt_us_min == 0 { rtt } else { s.rtt_us_min.min(rtt) };
                    if s.rtts.len() < 256 {
                        s.rtts.push(rtt);
                    }
                }
            }
        }
    });
}

fn make_probe(seq: u32, len: usize) -> Vec<u8> {
    let mut p = vec![0u8; len.max(PROBE_HDR)];
    p[0..4].copy_from_slice(MAGIC);
    p[4] = PKT_PROBE;
    p[5..9].copy_from_slice(&seq.to_be_bytes());
    p[9..17].copy_from_slice(&now_us().to_be_bytes());
    p
}

/// Light reachability probe: a handful of small datagrams; report RTT +
/// loss. Safe even adjacent to live traffic (negligible, rate-capped).
pub async fn probe_reachability(sock: Arc<UdpSocket>) -> ProbeReport {
    const COUNT: u32 = 20;
    let state = Arc::new(StdMutex::new(RecvState::default()));
    let cancel = CancellationToken::new();
    spawn_receiver(sock.clone(), state.clone(), cancel.clone());

    for seq in 0..COUNT {
        let _ = sock.send(&make_probe(seq, 64)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Drain window for in-flight echoes.
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();

    let s = state.lock().unwrap();
    let mut rtts = s.rtts.clone();
    rtts.sort_unstable();
    let median_ms = if rtts.is_empty() {
        0.0
    } else {
        rtts[rtts.len() / 2] as f64 / 1000.0
    };
    let loss = if COUNT > 0 {
        (1.0 - (s.echoes as f64 / COUNT as f64)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ok = s.echoes > 0;
    ProbeReport {
        ok,
        mode: "reachability".to_string(),
        error: if ok { None } else { Some("no echoes — responder unreachable on this leg".to_string()) },
        measured_bps: 0,
        knee_bps: None,
        loss_pct: loss * 100.0,
        rtt_ms: median_ms,
        rtt_ms_min: s.rtt_us_min as f64 / 1000.0,
        samples: Vec::new(),
        note: if ok {
            format!("Peer reachable over this leg ({} / {COUNT} echoes).", s.echoes)
        } else {
            "Peer did not answer — check the leg wiring (run Test leg) and that the responder is up.".to_string()
        },
    }
}

/// Capacity ramp: increase the offered rate each round until loss or
/// RTT-inflation appears (the knee), reporting the best clean delivered
/// rate. Bounded by `opts.duration` and `opts.max_bps`.
pub async fn probe_capacity(sock: Arc<UdpSocket>, opts: CapacityOpts) -> ProbeReport {
    const ROUND_MS: u64 = 100;
    const SETTLE_MS: u64 = 40;
    const LOSS_HIGH: f64 = 0.02;
    const RTT_INFLATION_US: u64 = 50_000; // 50 ms
    const GROWTH: f64 = 1.4;
    const HARD_MAX_BPS: u64 = 1_000_000_000;
    const MAX_PKTS_PER_ROUND: u64 = 6000;

    let ceiling = opts.max_bps.unwrap_or(HARD_MAX_BPS).min(HARD_MAX_BPS);
    let pkt_bits = (opts.packet_bytes.max(PROBE_HDR) as u64) * 8;

    let state = Arc::new(StdMutex::new(RecvState::default()));
    let cancel = CancellationToken::new();
    spawn_receiver(sock.clone(), state.clone(), cancel.clone());

    let started = Instant::now();
    let mut seq: u32 = 0;
    let mut target = opts.start_bps.clamp(100_000, ceiling);
    let mut best_clean: u64 = 0;
    let mut knee: Option<u64> = None;
    let mut samples: Vec<ProbeSample> = Vec::new();
    let mut empty_rounds = 0u32;
    let mut round = 0u32;

    while started.elapsed() < opts.duration {
        round += 1;
        let want_pkts = (((target as f64) * (ROUND_MS as f64 / 1000.0)) / pkt_bits as f64)
            .ceil()
            .max(1.0) as u64;
        let want_pkts = want_pkts.min(MAX_PKTS_PER_ROUND);

        let (rc0, rb0) = {
            let s = state.lock().unwrap();
            (s.recv_count, s.recv_bytes)
        };

        // Send the round's packets in bursts so we don't issue thousands
        // of sub-ms sleeps. ~20 pacing points per round.
        let burst = (want_pkts / 20).max(1);
        let pace = Duration::from_micros((ROUND_MS * 1000) / 20);
        let round_start = Instant::now();
        let mut sent: u64 = 0;
        while sent < want_pkts && round_start.elapsed() < Duration::from_millis(ROUND_MS) {
            for _ in 0..burst {
                if sent >= want_pkts {
                    break;
                }
                let _ = sock.send(&make_probe(seq, opts.packet_bytes)).await;
                seq = seq.wrapping_add(1);
                sent += 1;
            }
            tokio::time::sleep(pace).await;
        }

        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;

        let (rc1, rb1, rtt_min, rtt_last, echoes) = {
            let s = state.lock().unwrap();
            (s.recv_count, s.recv_bytes, s.rtt_us_min, s.rtt_us_last, s.echoes)
        };

        if echoes == 0 {
            empty_rounds += 1;
            if empty_rounds >= 3 {
                cancel.cancel();
                return ProbeReport::err(
                    ProbeMode::Capacity,
                    "no echoes from responder — is it running on the probe port, and does the leg reach it? (run Test leg / reachability first)",
                );
            }
            continue;
        }
        empty_rounds = 0;

        let elapsed_secs = (round_start.elapsed().as_micros() as f64 / 1_000_000.0).max(0.001);
        let delivered_bps = (((rb1.saturating_sub(rb0)) as f64) * 8.0 / elapsed_secs) as u64;
        let loss = if sent > 0 {
            (1.0 - (rc1.saturating_sub(rc0) as f64 / sent as f64)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        samples.push(ProbeSample {
            round,
            target_bps: target,
            delivered_bps,
            loss_pct: loss * 100.0,
            rtt_ms: rtt_last as f64 / 1000.0,
        });

        let inflated = rtt_min > 0 && rtt_last > rtt_min + RTT_INFLATION_US;
        if loss > LOSS_HIGH || inflated {
            knee = Some(target);
            break;
        }
        best_clean = best_clean.max(delivered_bps);

        if target >= ceiling {
            break; // hit the configured ceiling cleanly
        }
        target = ((target as f64 * GROWTH) as u64).min(ceiling);
    }

    cancel.cancel();
    let (rtt_min, rtt_last, total_echoes) = {
        let s = state.lock().unwrap();
        (s.rtt_us_min, s.rtt_us_last, s.echoes)
    };
    let last_loss = samples.last().map(|s| s.loss_pct).unwrap_or(0.0);

    let note = if knee.is_some() {
        format!(
            "Usable ≈ {:.1} Mbps (knee at {:.1} Mbps where loss/latency rose). Approximate — userspace pacing + point-in-time; cellular/satellite capacity varies.",
            best_clean as f64 / 1e6,
            knee.unwrap() as f64 / 1e6
        )
    } else {
        format!(
            "Carried ≈ {:.1} Mbps cleanly up to the test ceiling (no loss/latency knee hit). Approximate — raise the ceiling to find the real limit.",
            best_clean as f64 / 1e6
        )
    };

    ProbeReport {
        ok: total_echoes > 0,
        mode: "capacity".to_string(),
        error: None,
        measured_bps: best_clean,
        knee_bps: knee,
        loss_pct: last_loss,
        rtt_ms: rtt_last as f64 / 1000.0,
        rtt_ms_min: rtt_min as f64 / 1000.0,
        samples,
        note,
    }
}

// ── Orchestration ─────────────────────────────────────────────────────────────

/// Run a probe over one leg: bind the leg-pinned socket, program a
/// temporary gateway policy route if needed (idle-gated upstream), probe,
/// then tear the route down. The caller must have already cleared
/// [`capacity_conflict`] for capacity mode.
pub(crate) async fn run_leg_probe(
    net: LegNet,
    responder: SocketAddr,
    mode: ProbeMode,
    opts: CapacityOpts,
) -> ProbeReport {
    let nics = network_interfaces::enumerate();

    // Gateway-mode: program a temporary policy route so the leg actually
    // egresses its gateway (the route a real flow gets at start time).
    let mut programmed: Option<(String, u8)> = None;
    if let (Some(gw_ip), Some(src)) = (net.gateway.as_deref().and_then(ip_only), net.source.as_deref())
    {
        match crate::engine::bond_routing::SourceNet::parse(src) {
            Ok(src_net) => match crate::engine::bond_routing::BondRouteManager::global().await {
                Ok(mgr) => {
                    let iface = net.interface.as_deref().unwrap_or_default();
                    let flow_id = "__bond_probe__";
                    let pid = 250u8;
                    if let Err(e) = mgr.program(flow_id, pid, iface, src_net, gw_ip).await {
                        return ProbeReport::err(mode, format!("gateway route program failed: {e}"));
                    }
                    programmed = Some((flow_id.to_string(), pid));
                }
                Err(e) => {
                    return ProbeReport::err(
                        mode,
                        format!("gateway routing unavailable (needs CAP_NET_ADMIN): {e}"),
                    );
                }
            },
            Err(e) => return ProbeReport::err(mode, format!("invalid source '{src}': {e}")),
        }
    }

    let result = run_probe_inner(&net, &nics, responder, mode, opts).await;

    if let Some((fid, pid)) = programmed {
        if let Ok(mgr) = crate::engine::bond_routing::BondRouteManager::global().await {
            mgr.teardown(&fid, pid).await;
        }
    }

    match result {
        Ok(r) => r,
        Err(e) => ProbeReport::err(mode, e),
    }
}

async fn run_probe_inner(
    net: &LegNet,
    nics: &[network_interfaces::NetworkInterfaceInfo],
    responder: SocketAddr,
    mode: ProbeMode,
    opts: CapacityOpts,
) -> Result<ProbeReport, String> {
    let (sock2, _strict) =
        bind_leg_socket(net, nics, responder.is_ipv4()).map_err(|e| e.to_string())?;
    sock2.set_nonblocking(true).ok();
    let std_sock: std::net::UdpSocket = sock2.into();
    let sock = UdpSocket::from_std(std_sock).map_err(|e| e.to_string())?;
    sock.connect(responder)
        .await
        .map_err(|e| format!("connect responder {responder}: {e}"))?;
    let sock = Arc::new(sock);
    Ok(match mode {
        ProbeMode::Reachability => probe_reachability(sock).await,
        ProbeMode::Capacity => probe_capacity(sock, opts).await,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::{BondPathTransportConfig, BondQuicRole};

    fn udp_default_route_leg() -> LegNet {
        normalize(&BondPathTransportConfig::Udp {
            bind: None,
            remote: Some("203.0.113.9:7400".into()),
            interface: None,
            gateway: None,
            source: None,
        })
    }

    #[test]
    fn capacity_conflict_flags_busy_interface() {
        let net = normalize(&BondPathTransportConfig::Udp {
            bind: None,
            remote: Some("203.0.113.9:7400".into()),
            interface: Some("eno4".into()),
            gateway: None,
            source: None,
        });
        let mut busy = BusyKeys::default();
        busy.interfaces.insert("eno4".into());
        assert!(capacity_conflict(&net, &busy).is_some());
        assert!(capacity_conflict(&net, &BusyKeys::default()).is_none());
    }

    #[test]
    fn capacity_conflict_default_route_refused_when_busy() {
        let net = udp_default_route_leg();
        assert!(capacity_conflict(&net, &BusyKeys::default()).is_none());
        let mut busy = BusyKeys::default();
        busy.interfaces.insert("wwan0".into());
        assert!(capacity_conflict(&net, &busy).is_some());
    }

    #[test]
    fn register_then_drop_clears_busy() {
        let keys = vec![LegKey {
            interface: Some("test-leg-nic-xyz".into()),
            source_ip: None,
            gateway_ip: None,
        }];
        {
            let _g = register_active_legs("flow-probe-test", keys);
            assert!(busy_keys().interfaces.contains("test-leg-nic-xyz"));
        }
        assert!(!busy_keys().interfaces.contains("test-leg-nic-xyz"));
    }

    #[test]
    fn leg_keys_from_quic_client() {
        let paths = vec![BondPathConfig {
            id: 0,
            name: "q".into(),
            weight_hint: 1,
            max_bitrate_bps: None,
            transport: BondPathTransportConfig::Quic {
                role: BondQuicRole::Client,
                addr: "203.0.113.9:7400".into(),
                server_name: String::new(),
                tls: crate::config::models::BondQuicTls::SelfSigned,
                bind: Some("10.0.0.2:0".into()),
                interface: Some("eno4".into()),
            },
        }];
        let keys = leg_keys_for_paths(&paths);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].interface.as_deref(), Some("eno4"));
        assert_eq!(keys[0].source_ip, "10.0.0.2".parse().ok());
    }

    #[tokio::test]
    async fn responder_echoes_and_capacity_measures_on_loopback() {
        // End-to-end on loopback: responder + client, no shaping → should
        // carry a healthy rate cleanly and report echoes.
        let port = start_responder("127.0.0.1:0".parse().unwrap(), Duration::from_secs(5))
            .await
            .expect("responder starts");
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let sock = Arc::new(sock);

        let reach = probe_reachability(sock.clone()).await;
        assert!(reach.ok, "loopback reachability should succeed: {reach:?}");

        let cap = probe_capacity(
            sock,
            CapacityOpts {
                duration: Duration::from_millis(800),
                max_bps: Some(50_000_000),
                start_bps: 2_000_000,
                packet_bytes: 1200,
            },
        )
        .await;
        assert!(cap.ok, "capacity probe should see echoes: {cap:?}");
        assert!(cap.measured_bps > 0, "should measure some delivered rate: {cap:?}");
        stop_responder(port);
    }

    #[tokio::test]
    async fn stop_responder_unknown_port_is_false() {
        assert!(!stop_responder(1));
    }
}
