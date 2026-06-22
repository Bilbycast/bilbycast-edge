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
    /// EWMA of RTT — smooths single-sample jitter so the inflation knee
    /// reacts to sustained bufferbloat, not a lone late echo (critical on
    /// cellular, where per-sample RTT jitter is routinely ±100 ms).
    rtt_us_smooth: u64,
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
                    s.rtt_us_smooth = if s.rtt_us_smooth == 0 { rtt } else { (s.rtt_us_smooth * 7 + rtt) / 8 };
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
/// RTT-inflation knee floor (low-latency links). The real budget scales with
/// the link's baseline RTT in [`rtt_inflated`].
const RTT_INFLATION_FLOOR_US: u64 = 50_000; // 50 ms

/// Cellular-aware RTT-inflation knee. Trips only when the smoothed RTT rises
/// well above the link's *own* baseline — the inflation budget scales with
/// `rtt_min` (floored for low-latency wired links), so a high-RTT cellular
/// leg (~360 ms with ±100 ms jitter) doesn't false-trip on jitter, only on
/// genuine bufferbloat (the queue building under overload). The old absolute
/// 50 ms threshold made the probe knee on normal cellular jitter and badly
/// under-report 5G capacity.
fn rtt_inflated(rtt_min_us: u64, rtt_smooth_us: u64) -> bool {
    if rtt_min_us == 0 {
        return false;
    }
    let budget = RTT_INFLATION_FLOOR_US.max(rtt_min_us);
    rtt_smooth_us > rtt_min_us.saturating_add(budget)
}

pub async fn probe_capacity(sock: Arc<UdpSocket>, opts: CapacityOpts) -> ProbeReport {
    const ROUND_MS: u64 = 100;
    const SETTLE_MS: u64 = 40;
    // Loss knee. Cellular routinely loses a few % of raw UDP that the bond
    // recovers via ARQ/FEC, so a 2% knee under-reported badly — the bond's
    // own congestion controller tolerates ~6%. 5% gives a representative
    // usable figure without declaring capacity at absurd loss.
    const LOSS_HIGH: f64 = 0.05;
    const GROWTH: f64 = 1.4;
    const HARD_MAX_BPS: u64 = 1_000_000_000;
    const MAX_PKTS_PER_ROUND: u64 = 6000;

    let ceiling = opts.max_bps.unwrap_or(HARD_MAX_BPS).min(HARD_MAX_BPS);
    let pkt_bits = (opts.packet_bytes.max(PROBE_HDR) as u64) * 8;

    let state = Arc::new(StdMutex::new(RecvState::default()));
    let cancel = CancellationToken::new();
    spawn_receiver(sock.clone(), state.clone(), cancel.clone());

    // ── RTT bootstrap ── learn the baseline RTT with a few light pings before
    // ramping, so every ramp round can settle for ≥ RTT. Without this, a
    // high-RTT cellular link (RTT ≫ the round time) measures each round's
    // loss/delivered against echoes that haven't returned yet → phantom loss
    // and a collapsed result. Polls for the first echo, so a low-RTT link
    // bootstraps in milliseconds and a high-RTT one waits at most ~1.2 s.
    let mut seq: u32 = 0;
    for _ in 0..8u32 {
        let _ = sock.send(&make_probe(seq, opts.packet_bytes)).await;
        seq = seq.wrapping_add(1);
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    {
        let boot = Instant::now();
        while boot.elapsed() < Duration::from_millis(1200) {
            if state.lock().unwrap().rtt_us_min > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // Settle each round long enough for THIS round's echoes to come back
    // (RTT + 80 ms margin, capped), so per-round loss/delivered is real rather
    // than lagging one or more rounds behind the offered rate.
    let settle = {
        let rtt0 = state.lock().unwrap().rtt_us_min;
        if rtt0 > 0 {
            Duration::from_micros((rtt0 + 80_000).min(2_000_000))
        } else {
            Duration::from_millis(SETTLE_MS)
        }
    };

    let started = Instant::now();
    let mut target = opts.start_bps.clamp(100_000, ceiling);
    let mut best_clean: u64 = 0;
    let mut last_clean_target: u64 = 0;
    let mut knee: Option<u64> = None;
    let mut samples: Vec<ProbeSample> = Vec::new();
    let mut sustain_samples: Vec<u64> = Vec::new();
    let mut empty_rounds = 0u32;
    let mut round = 0u32;
    // Two phases in one loop. RAMP grows the offered rate ×1.4 per round to
    // find the operating point (loss/RTT knee, or the ceiling). SUSTAIN then
    // holds the last clean rate for the REST of the requested duration. The
    // sustain phase is what makes the operator's "Seconds" matter: without it
    // the probe returned the instant it found the knee (one ~100 ms round on a
    // lossy cellular link), so "Seconds" was ignored and the result was a
    // single blip. Sustain runs the full window and reports the median held
    // throughput — robust, and a proof the rate actually holds.
    let mut sustaining = false;

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

        // Send the whole round in ~10 small bursts spread over ~ROUND_MS —
        // paced enough for cellular (never one big burst), but reliably
        // OFFERS the target rate. Per-packet userspace sleeps were a mistake:
        // they overshoot badly below ~10 ms and throttled the probe's own send
        // rate (a clean link then read ~1 Mbps). We send ALL want_pkts (no
        // early time-cut) so the offered rate actually matches `target`.
        let ticks = 10u64;
        let burst = want_pkts.div_ceil(ticks).max(1);
        let pace = Duration::from_micros((ROUND_MS * 1000) / ticks);
        let round_start = Instant::now();
        let mut sent: u64 = 0;
        while sent < want_pkts {
            for _ in 0..burst {
                if sent >= want_pkts {
                    break;
                }
                let _ = sock.send(&make_probe(seq, opts.packet_bytes)).await;
                seq = seq.wrapping_add(1);
                sent += 1;
            }
            if sent < want_pkts {
                tokio::time::sleep(pace).await;
            }
        }
        // The offered-rate window is the SEND duration only (not the settle).
        // delivered_bps divides by this, so it reflects the rate we offered.
        let send_secs = (round_start.elapsed().as_micros() as f64 / 1_000_000.0).max(0.001);

        // Settle ≥ RTT so this round's echoes return before we read the
        // counters (the core fix for high-RTT cellular phantom loss).
        tokio::time::sleep(settle).await;

        let (rc1, rb1, rtt_min, rtt_last, rtt_smooth, echoes) = {
            let s = state.lock().unwrap();
            (s.recv_count, s.recv_bytes, s.rtt_us_min, s.rtt_us_last, s.rtt_us_smooth, s.echoes)
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

        let delivered_bps = (((rb1.saturating_sub(rb0)) as f64) * 8.0 / send_secs) as u64;
        let loss = if sent > 0 {
            (1.0 - (rc1.saturating_sub(rc0) as f64 / sent as f64)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if samples.len() < 240 {
            samples.push(ProbeSample {
                round,
                target_bps: target,
                delivered_bps,
                loss_pct: loss * 100.0,
                rtt_ms: rtt_last as f64 / 1000.0,
            });
        }
        // `delivered_bps` is what actually got through this round — a valid
        // usable-capacity figure even at the knee. Record it unconditionally
        // (a knee on round 1 must not report 0).
        best_clean = best_clean.max(delivered_bps);

        // ── Sustain/hold phase ── already holding the operating rate: keep
        // sampling for the rest of the window so the result is the sustained
        // throughput over the operator's requested duration, not one round.
        if sustaining {
            sustain_samples.push(delivered_bps);
            continue;
        }

        // ── Ramp phase ──
        let inflated = rtt_inflated(rtt_min, rtt_smooth);
        if loss > LOSS_HIGH || inflated {
            // Knee found. Back off to the last clean rate (or one growth step
            // below the knee if even the first round lost) and SUSTAIN it for
            // the remainder of the requested duration.
            knee = Some(target);
            target = if last_clean_target > 0 {
                last_clean_target
            } else {
                ((target as f64 / GROWTH) as u64).clamp(100_000, ceiling)
            };
            sustaining = true;
            continue;
        }

        // Clean round — remember it, then keep ramping.
        last_clean_target = target;
        if target >= ceiling {
            // Carried the ceiling cleanly; hold it for the rest of the window.
            sustaining = true;
            continue;
        }
        target = ((target as f64 * GROWTH) as u64).min(ceiling);
    }

    cancel.cancel();
    let (rtt_min, rtt_last, total_echoes) = {
        let s = state.lock().unwrap();
        (s.rtt_us_min, s.rtt_us_last, s.echoes)
    };
    let last_loss = samples.last().map(|s| s.loss_pct).unwrap_or(0.0);

    // Report the MEDIAN sustained delivered rate — robust over the whole
    // requested window. Fall back to the ramp best only when the window was
    // too short to enter the sustain phase (duration ran out mid-ramp).
    let measured_bps = if sustain_samples.is_empty() {
        best_clean
    } else {
        sustain_samples.sort_unstable();
        sustain_samples[sustain_samples.len() / 2]
    };
    let held_secs = sustain_samples.len() as f64 * ((ROUND_MS + SETTLE_MS) as f64 / 1000.0);

    let note = if sustain_samples.is_empty() {
        format!(
            "Reached ≈ {:.1} Mbps when the {:.0}s window elapsed mid-ramp — raise Seconds (or set Max Mbps) to let it find and hold the knee. Approximate.",
            measured_bps as f64 / 1e6,
            opts.duration.as_secs_f64()
        )
    } else if knee.is_some() {
        format!(
            "Usable ≈ {:.1} Mbps held for ~{:.0}s (knee at {:.1} Mbps where loss/latency rose). Approximate — userspace pacing; cellular/satellite capacity varies.",
            measured_bps as f64 / 1e6,
            held_secs,
            knee.unwrap() as f64 / 1e6
        )
    } else {
        format!(
            "Carried ≈ {:.1} Mbps held for ~{:.0}s up to the test ceiling (no loss/latency knee). Approximate — raise Max Mbps to find the real limit.",
            measured_bps as f64 / 1e6,
            held_secs
        )
    };

    ProbeReport {
        ok: total_echoes > 0,
        mode: "capacity".to_string(),
        error: None,
        measured_bps,
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

    #[test]
    fn rtt_knee_is_relative_to_baseline() {
        // Low-latency wired: the ~50 ms floor keeps it sensitive.
        assert!(!rtt_inflated(10_000, 55_000), "55ms < 10+50 floor");
        assert!(rtt_inflated(10_000, 70_000), "70ms > 60ms");
        // High-latency cellular: budget scales with rtt_min, so normal jitter
        // (the old absolute 50 ms knee's false-trip cause) is ignored.
        assert!(!rtt_inflated(360_000, 450_000), "+90ms jitter is not congestion");
        assert!(!rtt_inflated(360_000, 700_000), "still under the doubled baseline");
        assert!(rtt_inflated(360_000, 760_000), ">2x baseline = real bufferbloat");
        // Unknown baseline never trips.
        assert!(!rtt_inflated(0, 999_000));
    }

    /// Echo responder that silently ignores every other probe — simulates a
    /// lossy link (cellular/satellite). The sender sees loss ≈ 50 %, so the
    /// capacity ramp trips its knee on the very first round.
    async fn lossy_responder() -> (u16, CancellationToken) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let token = CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (mut recv_count, mut recv_bytes, mut seen): (u64, u64, u64) = (0, 0, 0);
            loop {
                tokio::select! {
                    _ = t2.cancelled() => break,
                    r = sock.recv_from(&mut buf) => {
                        let (n, from) = match r { Ok(v) => v, Err(_) => continue };
                        if n < PROBE_HDR || &buf[0..4] != MAGIC || buf[4] != PKT_PROBE { continue; }
                        seen += 1;
                        if seen % 2 == 0 { continue; } // drop: don't count, don't echo
                        recv_count += 1;
                        recv_bytes += n as u64;
                        let mut echo = [0u8; ECHO_LEN];
                        echo[0..4].copy_from_slice(MAGIC);
                        echo[4] = PKT_ECHO;
                        echo[5..9].copy_from_slice(&buf[5..9]);
                        echo[9..17].copy_from_slice(&buf[9..17]);
                        echo[17..25].copy_from_slice(&recv_count.to_be_bytes());
                        echo[25..33].copy_from_slice(&recv_bytes.to_be_bytes());
                        let _ = sock.send_to(&echo, from).await;
                    }
                }
            }
        });
        (port, token)
    }

    #[tokio::test]
    async fn capacity_reports_delivered_when_knee_hits_first_round() {
        // Regression for the live-cellular "✓ 0.0 Mbps usable" bug: a lossy
        // (but reachable) path trips the loss knee on round 1. Echoes return
        // (ok=true), so the probe MUST report the delivered rate, not 0.
        let (port, token) = lossy_responder().await;
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let sock = Arc::new(sock);

        let cap = probe_capacity(
            sock,
            CapacityOpts {
                duration: Duration::from_millis(600),
                max_bps: Some(50_000_000),
                start_bps: 2_000_000,
                packet_bytes: 1200,
            },
        )
        .await;
        token.cancel();

        assert!(cap.ok, "echoes should return on a lossy-but-reachable path: {cap:?}");
        assert!(cap.knee_bps.is_some(), "≈50% loss should trip the knee: {cap:?}");
        assert!(
            cap.measured_bps > 0,
            "must report the delivered rate at the knee, not 0: {cap:?}"
        );
    }

    /// Echo responder that delays every echo by `rtt_ms` (simulates a
    /// high-latency cellular link where RTT >> the probe's round time) and
    /// optionally drops 1-in-`drop_every` probes.
    async fn delayed_responder(rtt_ms: u64, drop_every: u64) -> (u16, CancellationToken) {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let port = sock.local_addr().unwrap().port();
        let token = CancellationToken::new();
        let t2 = token.clone();
        let s2 = sock.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (mut recv_count, mut recv_bytes, mut seen): (u64, u64, u64) = (0, 0, 0);
            loop {
                tokio::select! {
                    _ = t2.cancelled() => break,
                    r = s2.recv_from(&mut buf) => {
                        let (n, from) = match r { Ok(v) => v, Err(_) => continue };
                        if n < PROBE_HDR || &buf[0..4] != MAGIC || buf[4] != PKT_PROBE { continue; }
                        seen += 1;
                        if drop_every > 0 && seen % drop_every == 0 { continue; }
                        recv_count += 1;
                        recv_bytes += n as u64;
                        let mut echo = [0u8; ECHO_LEN];
                        echo[0..4].copy_from_slice(MAGIC);
                        echo[4] = PKT_ECHO;
                        echo[5..9].copy_from_slice(&buf[5..9]);
                        echo[9..17].copy_from_slice(&buf[9..17]);
                        echo[17..25].copy_from_slice(&recv_count.to_be_bytes());
                        echo[25..33].copy_from_slice(&recv_bytes.to_be_bytes());
                        let s3 = s2.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(rtt_ms)).await;
                            let _ = s3.send_to(&echo, from).await;
                        });
                    }
                }
            }
        });
        (port, token)
    }

    #[tokio::test]
    async fn capacity_probe_high_rtt_clean_link() {
        // A clean but high-latency link (≈300ms RTT, no loss) — the cellular
        // regime where RTT ≫ the round time. The probe must NOT collapse: with
        // a 10 Mbps ceiling and zero real loss it should ramp to ~ceiling, not
        // report ~1 Mbps (the per-packet-pacing throttle + RTT-phantom-loss
        // bug that made the live test read "even lower than before").
        let (port, token) = delayed_responder(300, 0).await;
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let sock = Arc::new(sock);
        let cap = probe_capacity(
            sock,
            CapacityOpts {
                duration: Duration::from_secs(5),
                max_bps: Some(10_000_000),
                start_bps: 2_000_000,
                packet_bytes: 1200,
            },
        )
        .await;
        token.cancel();
        assert!(cap.ok, "should get echoes despite RTT: {cap:?}");
        assert!(
            cap.measured_bps > 4_000_000,
            "clean 300ms-RTT link (10M ceiling, 0 loss) must not collapse — got {:.2}M: {cap:?}",
            cap.measured_bps as f64 / 1e6
        );
    }

    #[tokio::test]
    async fn capacity_probe_high_rtt_with_loss() {
        // High RTT (≈300ms) AND real loss (1-in-3 dropped ≈ 33%). The probe
        // should knee on the genuine loss and still report a sane delivered
        // rate — neither collapsing to 0 nor reporting the full offered rate.
        let (port, token) = delayed_responder(300, 3).await;
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let sock = Arc::new(sock);
        let cap = probe_capacity(
            sock,
            CapacityOpts {
                duration: Duration::from_secs(4),
                max_bps: Some(10_000_000),
                start_bps: 2_000_000,
                packet_bytes: 1200,
            },
        )
        .await;
        token.cancel();
        assert!(cap.ok, "echoes return despite RTT + loss: {cap:?}");
        assert!(cap.measured_bps > 0, "must report a delivered rate, not 0: {cap:?}");
        assert!(
            cap.measured_bps < 9_000_000,
            "a 33%-loss link must not report ~the full offered rate: {cap:?}"
        );
    }

    #[tokio::test]
    async fn capacity_probe_honors_requested_duration() {
        // The operator's "Seconds" must govern the test window. Even on a
        // lossy link that trips the knee on round 1, the probe holds the
        // operating rate (sustain phase) for the rest of the duration instead
        // of returning after ~100 ms. Regression for "seems like a very quick
        // test" — pre-sustain this returned in ~140 ms regardless of Seconds.
        let (port, token) = lossy_responder().await;
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let sock = Arc::new(sock);

        let t0 = std::time::Instant::now();
        let cap = probe_capacity(
            sock,
            CapacityOpts {
                duration: Duration::from_millis(1500),
                max_bps: Some(50_000_000),
                start_bps: 2_000_000,
                packet_bytes: 1200,
            },
        )
        .await;
        let elapsed = t0.elapsed();
        token.cancel();

        assert!(
            elapsed >= Duration::from_millis(1200),
            "probe must run ~the full requested duration (sustain phase), ran {elapsed:?}: {cap:?}"
        );
        assert!(cap.measured_bps > 0, "should report a sustained rate: {cap:?}");
        // Multiple sustain rounds → multiple samples, not a single blip.
        assert!(cap.samples.len() >= 3, "should sample across the window: {cap:?}");
    }

    /// Live WAN test over a real firewall hairpin. Inert unless
    /// `BOND_WAN_TARGET` is set (e.g. `111.118.193.172:7400`). The
    /// responder binds the DNAT target locally; the probe egresses to the
    /// public IP and the firewall hairpins it back to the responder, so
    /// the round-trip traverses the real WAN path.
    #[tokio::test]
    #[ignore = "requires a live WAN hairpin; set BOND_WAN_TARGET (+ BOND_WAN_RESP_BIND)"]
    async fn wan_hairpin_capacity_probe() {
        let target = match std::env::var("BOND_WAN_TARGET") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("skip: set BOND_WAN_TARGET=ip:port");
                return;
            }
        };
        let resp_bind = std::env::var("BOND_WAN_RESP_BIND").unwrap_or_else(|_| "0.0.0.0:7400".into());
        let port = start_responder(resp_bind.parse().unwrap(), Duration::from_secs(25))
            .await
            .expect("responder starts");
        eprintln!("responder bound, local port {port} ({resp_bind})");
        let responder: SocketAddr = target.parse().expect("BOND_WAN_TARGET ip:port");
        // Optional source bind to pin the probe onto a specific uplink
        // (e.g. a 5G modem with a pre-existing policy route).
        let src = std::env::var("BOND_WAN_SRC").unwrap_or_else(|_| "0.0.0.0:0".into());
        let bind_addr = if src.contains(':') { src } else { format!("{src}:0") };
        eprintln!("probe source bind: {bind_addr}");
        let sock = UdpSocket::bind(&bind_addr).await.unwrap();
        sock.connect(responder).await.unwrap();
        let sock = Arc::new(sock);

        let reach = probe_reachability(sock.clone()).await;
        eprintln!(
            "REACHABILITY: ok={} loss={:.1}% rtt={:.2}ms (min {:.2}ms) — {}",
            reach.ok, reach.loss_pct, reach.rtt_ms, reach.rtt_ms_min, reach.note
        );

        let cap = probe_capacity(
            sock,
            CapacityOpts {
                duration: Duration::from_secs(6),
                max_bps: Some(80_000_000),
                start_bps: 2_000_000,
                packet_bytes: 1200,
            },
        )
        .await;
        eprintln!(
            "CAPACITY: ok={} measured={:.1} Mbps knee={:?} loss={:.2}% rtt={:.2}ms (min {:.2}ms)\n  note: {}",
            cap.ok,
            cap.measured_bps as f64 / 1e6,
            cap.knee_bps.map(|k| (k as f64 / 1e6 * 10.0).round() / 10.0),
            cap.loss_pct,
            cap.rtt_ms,
            cap.rtt_ms_min,
            cap.note
        );
        for s in &cap.samples {
            eprintln!(
                "  round {:>2}: target {:>5.1} Mbps  delivered {:>5.1} Mbps  loss {:>4.1}%  rtt {:>5.1}ms",
                s.round,
                s.target_bps as f64 / 1e6,
                s.delivered_bps as f64 / 1e6,
                s.loss_pct,
                s.rtt_ms
            );
        }
        stop_responder(port);
        assert!(reach.ok, "WAN hairpin reachability should succeed");
    }
}
