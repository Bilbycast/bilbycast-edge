// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-output PCR-anchored wire emission.
//!
//! One thread per output, decoupled from the Tokio runtime so wire timing
//! does not depend on the timer wheel. Userspace sleeps — `tokio::time::
//! sleep`, `std::thread::sleep`, and even `clock_nanosleep` on a
//! `SCHED_FIFO` thread — are unreliable at sub-1 ms targets. Two release
//! paths are available, picked per output at spawn time based on a
//! one-shot capability probe:
//!
//! - **SO_TXTIME** (preferred, Linux ≥ 4.19, requires the
//!   `wire_emit_txtime` setsockopt to succeed): each datagram carries a
//!   `SCM_TXTIME` CMSG with a target nanosecond timestamp; the kernel
//!   ETF qdisc (and on supported NICs, hardware tx scheduling) honours
//!   the timestamp without further userspace involvement. Sub-µs jitter
//!   with ETF + NIC HW offload; ~1–10 µs with software ETF; same as
//!   no-pacing if ETF qdisc isn't installed (kernel ignores the CMSG
//!   silently — degrades gracefully).
//! - **`clock_nanosleep` fallback** (Linux without setsockopt
//!   permission, kernels < 4.19, non-Linux): the thread sleeps to the
//!   target via `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)` and
//!   then issues a blocking `send_to`. Realistic envelope: ~50–500 µs at
//!   SCHED_FIFO under typical desktop load, ~1–5 ms at SCHED_OTHER.
//!   Acceptable for ≤ 6 Mbps TS; degraded above that. At ST 2110
//!   regimes (250 k+ pps) userspace sleep cannot pace at all and the
//!   emitter logs a Critical event + emits unpaced.
//!
//! Both paths share the same target-derivation math.
//!
//! ## Anchors
//!
//! Two anchor strategies, selected by [`AnchorSource`]:
//!
//! - [`AnchorSource::Pcr`] (TS regime): the emitter parses each datagram
//!   for an MPEG-TS PCR sample. PCR-bearing datagrams re-anchor the
//!   wallclock target. Non-PCR datagrams pace by `bytes_since_anchor /
//!   observed_rate_bps`. The rate comes exclusively from inter-PCR
//!   observations (EMA, ~10 PCRs to 95 % convergence). There is no
//!   "declared bitrate" parameter — open-loop pacing on a configured rate
//!   drifts when the encoder runs above (or below) the configured target,
//!   and that drift is what reverted the prior wire_emit integration on
//!   2026-05-09. Closed-loop on observed rate is the universal fix
//!   regardless of codec, rate-control mode, or encoder backend.
//! - [`AnchorSource::St2110Raster`] (uncompressed video regime): the
//!   caller computes a per-packet `target_tx_time_ns` from the active
//!   ST 2110-21 frame raster and passes it on the [`WireDatagram`]. The
//!   emitter just delivers at the requested time.
//!
//! ## First-datagram preroll
//!
//! Anchor on the first datagram: `wall_anchor_ns = monotonic_now() +
//! PREROLL_NS` (50 ms). Emit there. Subsequent PCRs re-anchor.
//!
//! ## Late-rebase
//!
//! If a PCR-driven target lands more than 5 ms behind real time (e.g.
//! after an encoder pipeline burst), `wall_anchor` snaps to `now`. One
//! one-time jitter step instead of a steady-state degradation.
//!
//! ## Discontinuity
//!
//! A PCR jump > 500 ms forward or any backwards step resets the anchor
//! to `now` and resumes from the new PCR. Mirrors `engine::pcr_pll` and
//! `stats::pcr_trust`.
//!
//! ## Cancellation
//!
//! `recv_timeout(50 ms)` polls the cancellation token; the emitter exits
//! within ~50 ms of cancel. Channel disconnect (encoder dropped) also
//! exits cleanly.
//!
//! ## Wrap is the caller's job
//!
//! [`WireDatagram::bytes`] is on-the-wire-final. RTP wrap, FEC packet
//! framing, ST 2110 RFC 4175 packetisation — all handled upstream. This
//! lets multiple emitters share a wrap state (2022-7 dual-leg with
//! byte-identical RTP headers on Red + Blue) and lets one emitter carry
//! mixed traffic on one socket (RTP media + RTP FEC).
//!
//! ## Stats
//!
//! `packets_sent`, `bytes_sent` on every successful send.
//! `record_latency(recv_time_us)` on every successful send.
//! `record_pcr_egress(pcr, now_us)` on every PCR-bearing send.
//! `packets_dropped` is incremented by the caller on `try_send::Full`.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::stats::collector::OutputStatsAccumulator;

/// 50 ms preroll on the first emit. Avoids the immediate send-cliff
/// when the first datagram arrives at the wire thread before the
/// encoder has settled.
const PREROLL_NS: u64 = 50_000_000;

/// If a PCR-driven target lands in the past by more than this, rebase
/// `wall_anchor` to `now_ns` so we don't accumulate wallclock lag every
/// burst. Causes a one-time jitter step at the rebase moment; without
/// it, a single late burst pins the anchor permanently behind real time
/// and subsequent PCRs all emit ASAP.
const LATE_REBASE_THRESHOLD_NS: u64 = 5_000_000;

/// Hard upper bound on how far into the future a single derive_target
/// return may sit relative to `now_ns`. The closed-loop pacer can drift
/// `wall_anchor` ahead of wallclock during catch-up bursts (e.g. SRT
/// jitter-buffer dump on connect, or input-switch backlog drain) — once
/// past the bound, the natural-paced math keeps stretching the target
/// further out, the userspace mpsc fills, and `try_send` from the
/// producer collapses to drop-on-full at single-digit packet rates.
/// Capping at 200 ms is harmless on the SO_TXTIME path (the kernel ETF
/// qdisc has plenty of margin) and breaks the death spiral on the
/// clock_nanosleep fallback where the userspace queue is the bottleneck.
const MAX_FUTURE_LOOKAHEAD_NS: u64 = 200_000_000;

/// 500 ms in 27 MHz ticks. PCR jumps beyond this trigger an anchor reset.
const PCR_DISCONTINUITY_27MHZ: i64 = 13_500_000;

/// If no PCR is seen on anchor_pid for this long, reset the anchor so
/// the emitter can re-lock on a different PID (input switch). 200 ms
/// covers 5× the typical 40 ms PCR cadence — long enough to ride out
/// a single lost PCR packet, short enough to re-lock within one GOP.
const PCR_ANCHOR_STALE_NS: u64 = 200_000_000;

/// `recv_timeout` polling cadence. Bounds cancel latency.
const RECV_POLL: Duration = Duration::from_millis(50);

/// Drain MSG_ERRQUEUE every Nth packet on the SO_TXTIME path. ~1024
/// packets ≈ 1.7 s at 6 Mbps TS ≈ 4 ms at ST 2110 1080p50. Cheap (one
/// non-blocking `recvmsg` call returning EAGAIN when empty).
const SO_TXTIME_ERRQUEUE_DRAIN_EVERY: u64 = 1024;

/// Channel capacity. Bumped from 1024 → 8192 to absorb startup
/// bursts that previously caused drop-on-full at the `wire_tx`
/// boundary:
///   - SRT input jitter-buffer dumps 1+ s of buffered media on
///     connect (at 50 Mbps, ~4800 packets in one burst);
///   - PCR PLL pre-lock, first-emit preroll, and 2022-7 dual-leg
///     merge transients all push more than 1024 datagrams through
///     before the wire pacer stabilises;
///   - ST 2110-20 narrow-profile frame bursts approach ~5700
///     packets per frame at 1080p50 (one frame in <1 ms followed
///     by ~20 ms of pacing), and the higher cap covers a full
///     frame plus the next being prepared.
///
/// 8192 datagrams ≈ 14 s in-flight at 6 Mbps TS / ~3.5 s at 25 Mbps
/// / 30 ms at 3 Gbps ST 2110. Memory cost is ~400 KB of slot space
/// per output socket (8192 × ~48 B); the heap behind each
/// `Bytes::clone` is refcounted and only held while a slot is
/// occupied. At ST 2110 rates with SO_TXTIME, the kernel takes
/// packets at line rate so the queue stays nearly empty in steady
/// state regardless — depth only matters during transients.
pub const WIRE_CHANNEL_CAP: usize = 8192;

// `MIN_BITRATE_BPS` was a clamp on the natural-paced
// `wall + bytes/observed_rate` interpolation. Removed alongside the
// natural pacing itself — between PCRs we now emit ASAP, and PCR-to-PCR
// targets come from the source's PCR clock difference (no rate divisor
// involved). See `derive_target_raw` for the rationale.

// ── Wire-tx depth tracking for codec backpressure ──────────────────────
//
// The codec thread upstream (engine::transcode_chain::run_chain) needs
// to know how deep the wire pacer's queue is so it can pause before
// over-producing during transient encoder bursts. Without this, a VBR
// encoder that briefly overshoots its target rate during a complex
// scene fills the wire_tx channel, hits the cap, and drops. cell24 x264
// surfaces this on every long run at t=270 s when the source content
// drives the encoder into a sustained overshoot window.
//
// The depth is shared via a single `Arc<AtomicUsize>` updated on every
// successful `try_send` (incr) and `recv_timeout` (decr). The codec
// thread polls it once per frame; if it's above
// `BACKPRESSURE_THRESHOLD` (75% of cap) the codec sleeps one frame
// interval before encoding more. This naturally rate-matches the
// encoder to the wire pacer's drain rate without any explicit rate
// limit — once the queue drains below the threshold the codec resumes
// at full speed.

/// Codec backpressure kicks in at 75% of WIRE_CHANNEL_CAP. Below this
/// the codec runs at full speed; above, it sleeps one frame interval
/// before each encode to let the wire pacer drain.
pub const WIRE_CHANNEL_BACKPRESSURE_THRESHOLD: usize = (WIRE_CHANNEL_CAP * 3) / 4;

/// Wraps a `SyncSender<WireDatagram>` with a shared depth counter so
/// upstream codec threads can apply backpressure when the wire pacer
/// falls behind. Cloneable; all clones share the same depth.
#[derive(Clone)]
pub struct WireTxHandle {
    tx: SyncSender<WireDatagram>,
    depth: Arc<std::sync::atomic::AtomicUsize>,
}

impl WireTxHandle {
    /// Try to send a datagram. Returns `Ok(())` on success (depth
    /// incremented), `Err(WireDatagram)` if the channel was full (depth
    /// unchanged, caller decides whether to count the drop).
    pub fn try_send(&self, dg: WireDatagram) -> Result<(), WireDatagram> {
        match self.tx.try_send(dg) {
            Ok(()) => {
                self.depth.fetch_add(1, Ordering::Release);
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Full(dg)) => Err(dg),
            Err(std::sync::mpsc::TrySendError::Disconnected(dg)) => Err(dg),
        }
    }

    /// Snapshot of the depth Arc, for passing into upstream codec
    /// threads that need to poll it independently of the sender.
    pub fn depth_handle(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.depth)
    }
}

/// Wraps a `Receiver<WireDatagram>` so the depth counter decrements on
/// every successful recv. The wire-emit thread owns this; no other
/// code path should construct or hold a `WireTxReceiver`.
struct WireTxReceiver {
    rx: Receiver<WireDatagram>,
    depth: Arc<std::sync::atomic::AtomicUsize>,
}

impl WireTxReceiver {
    fn recv_timeout(&self, timeout: Duration) -> Result<WireDatagram, RecvTimeoutError> {
        let r = self.rx.recv_timeout(timeout)?;
        // Saturate-on-underflow guard: depth is incremented on send and
        // decremented here. Under normal flow they balance, but on
        // shutdown the receiver may drain after the sender has been
        // dropped — keep the count non-negative.
        self.depth.fetch_update(Ordering::Release, Ordering::Acquire, |d| {
            Some(d.saturating_sub(1))
        }).ok();
        Ok(r)
    }

    fn try_recv(&self) -> Result<WireDatagram, TryRecvError> {
        let r = self.rx.try_recv()?;
        self.depth.fetch_update(Ordering::Release, Ordering::Acquire, |d| {
            Some(d.saturating_sub(1))
        }).ok();
        Ok(r)
    }
}

/// Anchor strategy chosen at spawn time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorSource {
    /// PCR-anchored (MPEG-TS). The emitter parses each datagram for a
    /// PCR sample; cold-start emits ASAP; observed inter-PCR rate
    /// disciplines between-PCR pacing. `WireDatagram::target_tx_time_ns`
    /// is ignored.
    Pcr,
    /// Caller-supplied target (ST 2110-20/-23 raster pacing).
    /// `WireDatagram::target_tx_time_ns` MUST be set; the emitter
    /// delivers at the requested wallclock time without parsing the
    /// payload.
    St2110Raster,
}

/// One datagram on its way to the wire. `bytes` is on-the-wire-final
/// (caller has already wrapped RTP / FEC / RFC 4175 / etc.).
///
/// `bytes::Bytes` so callers feeding from a `BytesMut` staging buffer
/// can `.freeze()` (zero-copy) instead of `.to_vec()` (per-datagram
/// heap copy on the data path). For 2022-7 dual-leg, `Bytes::clone()`
/// is a refcount bump — no copy.
#[derive(Clone, Debug)]
pub struct WireDatagram {
    pub bytes: Bytes,
    pub recv_time_us: u64,
    /// Caller-supplied target wallclock (CLOCK_MONOTONIC ns) for the
    /// `St2110Raster` anchor. Ignored under `Pcr` anchor. `None` is
    /// "emit ASAP" under either anchor.
    pub target_tx_time_ns: Option<u64>,
    /// Byte offset within `bytes` where the MPEG-TS payload starts.
    /// Set to 0 for raw-TS callers (UDP); set to 12 for RTP-wrapped
    /// callers so the `Pcr` anchor's PCR scan finds the `0x47` sync
    /// bytes at correct strides. Without this offset, RTP datagrams
    /// look like "between PCRs" forever and the wire pacer collapses
    /// to producer cadence (no PCR-anchored re-pacing). For non-`Pcr`
    /// anchors (`St2110Raster`) the field is ignored.
    pub ts_offset: usize,
}

/// Internal: which release path the spawned thread will use.
#[derive(Clone, Copy, Debug)]
enum Releaser {
    /// Linux SO_TXTIME on a setsockopt-enabled UDP socket. Per-packet
    /// `sendmsg(SCM_TXTIME)` hands the kernel the target tx time; the
    /// thread does no userspace sleep.
    SoTxtime,
    /// `clock_nanosleep(TIMER_ABSTIME)` to the target, then blocking
    /// `send_to`. Universal fallback.
    ClockNanosleep,
}

/// The release-path tier as logged + reported via stats.
fn tier_label(releaser: Releaser, sched_fifo_granted: bool) -> &'static str {
    match (releaser, sched_fifo_granted) {
        (Releaser::SoTxtime, _) => "so_txtime",
        (Releaser::ClockNanosleep, true) => "clock_nanosleep_fifo",
        (Releaser::ClockNanosleep, false) => "clock_nanosleep",
    }
}

/// Spawn a dedicated wire-emission thread.
///
/// Returns the encoder-side `SyncSender`. `try_send` is non-blocking;
/// overflow is the caller's responsibility (count under
/// `stats.packets_dropped`).
///
/// `socket` must be a blocking `std::net::UdpSocket`. Callers converting
/// from `tokio::net::UdpSocket` should call `into_std()` then
/// `set_nonblocking(false)`.
///
/// At spawn time the function probes SO_TXTIME on `socket`. On success
/// the thread emits via `wire_emit_txtime::send_with_txtime`; on
/// failure it falls back to `clock_nanosleep` + blocking `send_to`. The
/// active tier is logged at info level and registered on
/// `stats.set_wire_pacing_tier`.
pub fn spawn_wire_emitter(
    id: String,
    socket: UdpSocket,
    dest: SocketAddr,
    anchor: AnchorSource,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> WireTxHandle {
    // Always use CLOCK_TAI for SO_TXTIME's clockid. The kernel etf
    // qdisc on Intel ice / igc drivers (and most others) only accepts
    // CLOCK_TAI; setsockopt with CLOCK_MONOTONIC silently degrades to
    // "kernel ignores the cmsg" — looks like the SO_TXTIME path is
    // active but in practice every datagram sends ASAP, giving the
    // same precision as no qdisc at all. ST 2110 raster has always
    // used CLOCK_TAI (PTP grandmaster reference); for PCR-anchored
    // we're switching to TAI too — `derive_target`'s math is purely
    // ns-relative so the absolute clock domain doesn't matter, and
    // `clock_nanosleep`/`clock_gettime` both accept CLOCK_TAI on
    // Linux ≥ 4.6. ST 2110 raster targets that arrive in TAI from
    // upstream `St2110_21Pacer` work unchanged.
    let clockid = crate::engine::wire_emit_txtime::CLOCK_TAI;

    // Releaser selection policy. **Default = clock_nanosleep**, *not* SO_TXTIME.
    //
    // Rationale (operator directive `feedback_no_etf_qdisc.md`): on every host
    // that doesn't ship with an ETF qdisc + PTP grandmaster + HW-PTP NIC —
    // i.e. essentially every production deployment today — the kernel
    // accepts `SO_TXTIME(setsockopt)` but silently emits each packet *as
    // soon as `sendmsg` is called* because there's no etf qdisc to honour
    // the `SCM_TXTIME` cmsg. The wire-emit thread then runs in a tight
    // producer-paced loop, propagating any burstiness from upstream
    // (media-pacer, encoder, broadcast subscriber lag) straight through to
    // the wire. Result: PCR_AC degrades to the source's burst envelope,
    // even though `OutputStats.wire_pacing_tier` reports `so_txtime` and
    // gives operators a false sense of broadcast-grade pacing.
    //
    // The `clock_nanosleep(CLOCK_TAI, TIMER_ABSTIME)` path actually paces in
    // userspace using the closed-loop observed inter-PCR rate. On modern
    // CPUs at SCHED_FIFO it consistently delivers ~50–500 µs p99 jitter,
    // which sits comfortably in the broadcast tier-2 envelope and is
    // sufficient for every real-world receiver short of T-STD-strict
    // contribution decoders.
    //
    // SO_TXTIME stays available as an opt-in for the small minority of hosts
    // that *do* have a properly-configured ETF qdisc (see
    // `packaging/setup-etf-qdisc.sh` and the boot-time systemd template
    // `packaging/bilbycast-etf-qdisc@.service`) **and** a PTP discipline
    // stack (`ptp4l` + `phc2sys`) running. Set `BILBYCAST_ENABLE_TXTIME=1`
    // (or the more verbose alias `BILBYCAST_ENABLE_SO_TXTIME=1`, both
    // accepted) to take that path on those hosts. The testbed start
    // SO_TXTIME is opt-in via BILBYCAST_ENABLE_TXTIME=1 because the
    // setsockopt probe succeeding does NOT guarantee an ETF qdisc is
    // configured on the output NIC. Without ETF, the kernel ignores the
    // per-packet timestamp and sends immediately — losing all PCR
    // pacing. The operator must set up the ETF qdisc first (see
    // packaging/setup-etf-qdisc.sh), then enable this flag.
    // BILBYCAST_FORCE_NANOSLEEP=1 forces the fallback for diagnostics.
    let enable_so_txtime = std::env::var("BILBYCAST_ENABLE_TXTIME")
        .map(|v| v == "1")
        .unwrap_or(false)
        || std::env::var("BILBYCAST_ENABLE_SO_TXTIME")
            .map(|v| v == "1")
            .unwrap_or(false);
    let force_nanosleep = std::env::var("BILBYCAST_FORCE_NANOSLEEP")
        .map(|v| v == "1")
        .unwrap_or(false);
    let releaser = if !force_nanosleep && enable_so_txtime && try_enable_so_txtime(&socket, clockid) {
        Releaser::SoTxtime
    } else {
        Releaser::ClockNanosleep
    };

    let (tx_raw, rx_raw) = sync_channel::<WireDatagram>(WIRE_CHANNEL_CAP);
    let depth = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tx = WireTxHandle {
        tx: tx_raw,
        depth: Arc::clone(&depth),
    };
    let rx = WireTxReceiver {
        rx: rx_raw,
        depth: Arc::clone(&depth),
    };
    let thread_id = id.clone();
    let stats_for_thread = stats.clone();
    // CPU pin index — round-robin across the operator-configured set so
    // multiple wire-emit threads on the same edge spread across the
    // dedicated cores instead of stacking onto one.
    let cpu_index = next_wire_emit_cpu_index();
    std::thread::Builder::new()
        .name(format!("wire-emit-{}", id))
        .spawn(move || {
            let who = format!("wire-emit '{}'", thread_id);
            let sched_fifo = crate::util::runtime_diag::apply_sched_fifo(&who, 50);
            let pinned_to = crate::util::runtime_diag::apply_cpu_pinning(&who, cpu_index);
            stats_for_thread.set_wire_pacing_tier(tier_label(releaser, sched_fifo));
            stats_for_thread.set_wire_pacing_pinned_cpu(pinned_to);
            tracing::info!(
                "wire-emit '{}': starting (anchor={:?}, tier={}, sched_fifo={}, pinned_cpu={:?})",
                thread_id,
                anchor,
                tier_label(releaser, sched_fifo),
                sched_fifo,
                pinned_to,
            );
            run_emitter(thread_id, socket, dest, anchor, releaser, stats_for_thread, cancel, rx);
        })
        .expect("wire-emit thread spawn");
    tx
}

/// Round-robin next CPU from `BILBYCAST_WIRE_EMIT_CPUS`. Returns `None`
/// when the env var is unset or empty, which leaves the wire-emit
/// thread on the kernel's default placement.
fn next_wire_emit_cpu_index() -> Option<usize> {
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicUsize;
    static SET: OnceLock<Vec<usize>> = OnceLock::new();
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let set =
        SET.get_or_init(|| crate::util::runtime_diag::parse_cpu_set_env("BILBYCAST_WIRE_EMIT_CPUS"));
    if set.is_empty() {
        return None;
    }
    let i = COUNTER.fetch_add(1, Ordering::Relaxed) % set.len();
    Some(set[i])
}

/// Best-effort SO_TXTIME enablement. Returns `true` only when the
/// setsockopt actually succeeded — matches the existing
/// `wire_emit_txtime::probe()` semantics but operates on the
/// already-opened output socket.
///
/// Logs the EXACT errno on failure. The kernel ≥ 6.x in mainline (and
/// every recent Ubuntu kernel that backports the same patch) rejects
/// `SO_TXTIME` with non-`CLOCK_MONOTONIC` clockids unless the calling
/// process has `CAP_NET_ADMIN` — `setsockopt` returns `EPERM` and the
/// emitter silently falls back to the `clock_nanosleep` tier. Without
/// this log line the only externally-visible symptom is the tier
/// label on `OutputStats.wire_pacing_tier`; with it, the operator
/// sees the cause and the remediation (grant the cap via systemd
/// `AmbientCapabilities=CAP_NET_ADMIN`, `setcap cap_net_admin+ep`, or
/// run as root) the moment the edge starts.
fn try_enable_so_txtime(socket: &UdpSocket, clockid: i32) -> bool {
    match crate::engine::wire_emit_txtime::enable_so_txtime(socket, clockid) {
        Ok(()) => true,
        Err(e) => {
            // Distinguish "no permission" from "kernel doesn't know
            // this clockid / option" — both surface as setsockopt
            // failures but call for different operator action.
            let hint = match e.raw_os_error() {
                Some(libc::EPERM) => {
                    " (kernel requires CAP_NET_ADMIN for non-CLOCK_MONOTONIC SO_TXTIME on this kernel; \
                     grant via systemd `AmbientCapabilities=CAP_NET_ADMIN` or `setcap cap_net_admin+ep <binary>`)"
                }
                Some(libc::EINVAL) => {
                    " (unknown clockid or invalid flags — check kernel ≥ 4.19 and iproute2)"
                }
                Some(libc::ENOPROTOOPT) => " (kernel built without SO_TXTIME — needs CONFIG_NET_SCH_ETF=y/m)",
                _ => "",
            };
            tracing::warn!(
                "wire-emit: SO_TXTIME(clockid={}) setsockopt failed: {}{} — falling back to clock_nanosleep tier",
                clockid,
                e,
                hint
            );
            false
        }
    }
}

// ── Target derivation state ─────────────────────────────────────────────

/// PCR-anchored target derivation state. Closed-loop on observed
/// inter-PCR rate; no declared/configured rate parameter.
#[derive(Default)]
struct TargetState {
    /// PID of the first PCR-bearing packet observed. Locks subsequent PCR
    /// extraction to this single PID — critical for MPTS streams where
    /// every program has its own independent 27 MHz clock and naive
    /// "first PCR in datagram" returns values from unrelated clocks. See
    /// `engine::ts_parse::first_pcr_in_ts_buffer_pid` for the rationale.
    /// `None` until the first PCR is seen on this stream.
    anchor_pid: Option<u16>,
    /// Most recent PCR sample (27 MHz ticks). `None` until first PCR.
    pcr_anchor: Option<u64>,
    /// Wallclock instant at which the most recent anchor datagram emits
    /// (CLOCK_MONOTONIC ns). Set to `now + PREROLL_NS` on the first
    /// datagram regardless of PCR presence.
    wall_anchor_ns: u64,
    /// Bytes accumulated past `wall_anchor` by datagrams since the most
    /// recent re-anchor. Reset to 0 when we re-anchor on a PCR.
    bytes_since_anchor: u64,
    /// EMA of inter-PCR observed bitrate (telemetry only).
    observed_rate_bps: u64,
    /// Byte count of the most recently completed PCR interval. Used for
    /// between-PCR linear interpolation: `target = wall_anchor +
    /// (bytes_since_anchor / prev_interval_bytes) × prev_interval_ns`.
    /// Per-interval local — tracks VBR instantly without EMA lag.
    prev_interval_bytes: u64,
    /// Duration (ns) of the most recently completed PCR interval.
    prev_interval_ns: u64,
    /// Set after the very first datagram so we don't preroll again.
    initialised: bool,
    /// Wallclock (ns) of the last datagram where we found a PCR on
    /// `anchor_pid`. When this exceeds `PCR_ANCHOR_STALE_NS` without
    /// a new PCR, `anchor_pid` is reset to `None` so the emitter can
    /// re-lock on a different PID after an input switch. Without this,
    /// `anchor_pid` stays pinned to the old input's PCR PID forever
    /// and the emitter runs unpaced (pcr=0) on every subsequent input.
    last_pcr_seen_ns: u64,
    /// Last `tx-time` value returned by `derive_target` — used to
    /// guarantee the kernel ETF qdisc never sees a tx-time that goes
    /// backwards. Without this, an input switch (PCR discontinuity →
    /// wall_anchor reset to "now") would hand the kernel a packet with
    /// `target = now` while the qdisc still holds residue from before
    /// the switch with `target = now + small_delta`. ETF sorts by
    /// target time and releases earliest-first, so the new packet would
    /// jump ahead of legitimately-queued residue, breaking HEVC
    /// reference-frame integrity at the receiver. Guarding the return
    /// with `>= last_returned_ns + EPSILON` emulates the strict-FIFO
    /// behaviour of the clock_nanosleep tier on the SO_TXTIME path,
    /// trading at most one wire-queue's worth of latency on the
    /// discontinuity packet for spec-compliant ordering.
    last_returned_ns: u64,
}

/// Minimum gap (ns) between consecutive `derive_target` returns when
/// the natural computation would have produced a non-monotonic step.
/// 1 µs is well below the wire-time of any real datagram (a 1316-byte
/// datagram at 1 Gbps takes ~10 µs; at 10 Mbps takes ~1 ms), so the
/// guard is invisible to a healthy stream and only kicks in on
/// pathological re-anchor steps.
const MONOTONIC_TARGET_EPSILON_NS: u64 = 1_000;

impl TargetState {
    /// Compute the target wall instant (CLOCK_MONOTONIC ns) at which
    /// the just-handed datagram should hit the wire. Mutates internal
    /// anchors so the next call sees consistent state.
    ///
    /// The return is guaranteed monotonically non-decreasing across
    /// calls (with a `MONOTONIC_TARGET_EPSILON_NS` minimum step) — see
    /// `last_returned_ns` for why. When the guard kicks in (typically
    /// only at a PCR discontinuity / re-anchor) `wall_anchor_ns` is
    /// also advanced so subsequent natural-paced calls compute targets
    /// from the guarded point — without that follow-through, every
    /// post-discontinuity datagram would clamp to `last_returned + N·ε`
    /// and the kernel ETF qdisc would release them as a tight burst at
    /// `last_returned`. With the follow-through, natural pacing
    /// resumes seamlessly from the guarded anchor.
    fn derive_target(&mut self, now_ns: u64, datagram_pcr: Option<u64>, datagram_bytes: usize) -> u64 {
        let raw = self.derive_target_raw(now_ns, datagram_pcr, datagram_bytes);
        let guarded = if raw < self.last_returned_ns {
            // Strict backwards step — kernel ETF would reorder. Push
            // forward to last_returned + epsilon and fast-forward the
            // pacing anchor so subsequent natural-paced calls compute
            // from the corrected baseline rather than re-clamping each
            // packet.
            let floor = self.last_returned_ns.saturating_add(MONOTONIC_TARGET_EPSILON_NS);
            self.wall_anchor_ns = floor;
            floor
        } else {
            // raw is monotonic with the previous return (equal is fine —
            // multiple cold-start preroll datagrams legitimately target
            // the same wall_anchor and queue FIFO at the kernel).
            raw
        };
        // Cap `wall_anchor` at `now + MAX_FUTURE_LOOKAHEAD_NS`. The closed-
        // loop pacer drifts `wall_anchor` ahead of wallclock during catch-
        // up bursts (initial SRT jitter-buffer dump, input-switch backlog
        // drain, encoder-bursts faster than declared rate). Once the
        // anchor races past wallclock by more than `MAX_FUTURE_LOOKAHEAD`,
        // the natural-paced math (`wall + bytes/observed_rate`) keeps
        // pushing subsequent targets further into the future, the wire-
        // emit mpsc fills (1024 cap), and the producer's try_send
        // collapses into drop-on-full at single-digit packet rates —
        // exactly the documented `wire pacer collapses to ~0.5 Mbps after
        // 4-5 input switches` regression. Snapping back to `now + MAX`
        // when the lookahead is exceeded breaks the spiral. Reset
        // `wall_anchor` and `last_returned_ns` to the cap so subsequent
        // natural-paced calls compute from the bounded baseline rather
        // than re-clamping each packet.
        let max_future = now_ns.saturating_add(MAX_FUTURE_LOOKAHEAD_NS);
        let final_target = if guarded > max_future {
            self.wall_anchor_ns = max_future;
            max_future
        } else {
            guarded
        };
        self.last_returned_ns = final_target;
        final_target
    }

    /// Inner derivation; may produce non-monotonic targets at PCR
    /// discontinuities or under late-rebase. The outer `derive_target`
    /// applies the monotonic guard before handing the value to the
    /// caller.
    fn derive_target_raw(&mut self, now_ns: u64, datagram_pcr: Option<u64>, datagram_bytes: usize) -> u64 {
        if !self.initialised {
            self.wall_anchor_ns = now_ns.saturating_add(PREROLL_NS);
            self.bytes_since_anchor = 0;
            self.initialised = true;
            if let Some(pcr) = datagram_pcr {
                self.pcr_anchor = Some(pcr);
            }
            // First datagram emits exactly at wall_anchor — no
            // accumulated bytes past the anchor yet.
            return self.wall_anchor_ns;
        }

        if let Some(pcr) = datagram_pcr {
            if let Some(prev) = self.pcr_anchor {
                let delta_27 = pcr.wrapping_sub(prev) as i64;
                // Discontinuity: backwards by any meaningful amount, or
                // forward jump > 500 ms. Reset anchor to "now". Also
                // reset `last_returned_ns` here so the outer monotonic
                // guard (`derive_target`) doesn't fast-forward
                // `wall_anchor` back to the previous in-flight target —
                // that's the documented `wire-pacer collapse after 4-5
                // input switches` regression: when the previous source
                // was paced into the future and a new source's first
                // PCR comes in, the guard would lift `wall_anchor` from
                // `now` back to `last_returned + ε` (= future), every
                // subsequent natural-paced packet would target further
                // out, and the wire-emit channel would drown in drops.
                // Resetting both makes the discontinuity reset complete
                // — pacing genuinely restarts from `now`.
                if !(0..=PCR_DISCONTINUITY_27MHZ).contains(&delta_27) {
                    self.pcr_anchor = Some(pcr);
                    self.wall_anchor_ns = now_ns;
                    self.bytes_since_anchor = 0;
                    self.observed_rate_bps = 0;
                    self.last_returned_ns = now_ns;
                    return now_ns;
                }
                let delta_ns = (delta_27 as u64) * 1000 / 27;

                // Update inter-PCR observed rate (EMA, 1/4 step toward
                // each new sample). Skip ultra-short inter-PCR windows
                // (< 1 ms) to avoid noise from clock-quantization at
                // tiny deltas.
                if delta_ns >= 1_000_000 && self.bytes_since_anchor > 0 {
                    let observed_bps = self
                        .bytes_since_anchor
                        .saturating_mul(8 * 1_000_000_000)
                        / delta_ns;
                    if self.observed_rate_bps == 0 {
                        self.observed_rate_bps = observed_bps;
                    } else {
                        self.observed_rate_bps = (3 * self.observed_rate_bps + observed_bps) / 4;
                    }
                    // Save the completed interval for between-PCR
                    // linear interpolation.
                    self.prev_interval_bytes = self.bytes_since_anchor;
                    self.prev_interval_ns = delta_ns;
                }

                let mut target = self.wall_anchor_ns.saturating_add(delta_ns);

                // Late-rebase: if encoder bursts pushed the ideal target
                // into the past, snap forward so subsequent PCRs are
                // paced from real time. Otherwise wall_anchor stays
                // behind forever and the burst pattern shows up at the
                // receiver as PCR-spacing-equivalent jitter. When this
                // fires, also reset `last_returned_ns` for the same
                // reason as the discontinuity branch — without it, the
                // outer monotonic guard would lift the snapped-to-now
                // target back to the previously-queued future value.
                if target + LATE_REBASE_THRESHOLD_NS < now_ns {
                    target = now_ns;
                    self.last_returned_ns = now_ns;
                }

                self.pcr_anchor = Some(pcr);
                self.wall_anchor_ns = target;
                self.bytes_since_anchor = 0;
                return target;
            }
            // First PCR after a no-PCR run: anchor on it. Target the
            // wall_anchor itself so the datagram queues behind the
            // initial preroll-anchored emit instead of jumping ahead
            // of it.
            self.pcr_anchor = Some(pcr);
            self.bytes_since_anchor = self.bytes_since_anchor.saturating_add(datagram_bytes as u64);
            return self.wall_anchor_ns.max(now_ns);
        }

        // Between PCRs: emit ASAP. PCR-bearing packets are the only
        // pacing-critical events (they set wall_anchor at the correct
        // wallclock instant via the closed-loop). Non-PCR datagrams
        // target max(wall_anchor, now) so they queue behind the last
        // PCR-paced emit and burst out in one sendmmsg batch.
        //
        // This creates bursty delivery (one batch per PCR interval).
        // Consumer receivers (VLC, ffplay) may need a larger UDP receive
        // buffer — `sysctl net.core.rmem_default=8388608` on the
        // receiving host, or VLC `--network-caching=3000`.
        // Professional receivers (Appear X, Cobalt, Cisco) handle the
        // burst natively via their T-STD jitter buffers.
        self.bytes_since_anchor = self.bytes_since_anchor.saturating_add(datagram_bytes as u64);
        self.wall_anchor_ns.max(now_ns)
    }
}

// ── Emitter main loop ───────────────────────────────────────────────────

/// Greedy collect cap for the `ClockNanosleep`-tier batch send. The
/// batch naturally bounds at the next PCR-bearing datagram with a
/// future target (one full inter-PCR window of bytes), so this cap is
/// only the absolute safety ceiling. Sized to absorb a worst-case
/// uncompressed-class I-frame burst — 2 MB raw video per 33 ms PCR
/// cycle (≈ 480 Mbps peak) is ~1600 datagrams. The cycle's `try_recv`
/// + `derive_target` overhead at 2 048 datagrams is ~3 ms (1.5 µs ×
/// 2 048) plus one `sendmmsg` syscall (~50 µs at 1 500 dgms on a
/// loopback socket) — well under the 33 ms cycle budget.
///
/// Shipping the full inter-PCR window in **one** `sendmmsg(2)` call is
/// what keeps the wire_tx channel from accumulating during I-frame
/// bursts: a 200 KB I-frame on a 5 Mbps stream is 152 datagrams within
/// a single 33 ms PCR cadence; capping the batch at e.g. 64 would
/// spread that single I-frame across 3 PCR cycles (99 ms wallclock vs
/// the 33 ms of content it represents), accumulating 66 ms of latency
/// per I-frame and ultimately filling the channel on long runs.
const NANOSLEEP_BATCH_MAX: usize = 2048;

/// One entry in the `ClockNanosleep`-tier batch: the datagram + the
/// PCR sample we parsed out of it (for `record_pcr_egress`).
struct BatchEntry {
    dg: WireDatagram,
    pcr_27mhz: Option<u64>,
}

/// A datagram whose `derive_target` was already evaluated in a prior
/// iteration's batch-collection step. Carries the resolved target so
/// the next iteration's main path skips a second `derive_target` call
/// (which would double-advance `state`).
struct PendingDatagram {
    dg: WireDatagram,
    target_ns: u64,
    pcr_27mhz: Option<u64>,
}

fn run_emitter(
    id: String,
    socket: UdpSocket,
    dest: SocketAddr,
    anchor: AnchorSource,
    releaser: Releaser,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    rx: WireTxReceiver,
) {
    let mut state = TargetState::default();
    let mut packets_since_drain: u64 = 0;
    let mut pending: Option<PendingDatagram> = None;
    // Initial capacity covers a typical TS inter-PCR window (~20 dgms)
    // plus comfortable headroom for I-frame bursts. The Vec grows on
    // demand up to NANOSLEEP_BATCH_MAX; the initial size just avoids
    // reallocation on the first few cycles.
    let mut batch: Vec<BatchEntry> = Vec::with_capacity(256);
    // Diagnostic — log average batch size every 5 seconds so operators
    // can confirm the batching path is firing on bursty TS outputs.
    let mut batch_count: u64 = 0;
    let mut batch_dgms_total: u64 = 0;
    let mut batch_log_next_ns: u64 = monotonic_now_ns().saturating_add(5_000_000_000);
    // Diagnostic: per-5s throughput + PCR event counters
    let mut diag_dgms: u64 = 0;
    let mut diag_pcr_events: u64 = 0;
    let mut diag_sleep_ns: u64 = 0;
    let mut diag_last_ns: u64 = monotonic_now_ns();

    loop {
        if cancel.is_cancelled() {
            return;
        }
        // The "anchor" datagram for this iteration is either the one we
        // deferred from the previous iteration (its target was in the
        // future so we couldn't batch it — but `derive_target` already
        // ran for it, so we MUST NOT re-derive here) or a fresh blocking
        // recv.
        let (dg, target_ns, pcr_27mhz) = match pending.take() {
            Some(p) => (p.dg, p.target_ns, p.pcr_27mhz),
            None => {
                let dg = match rx.recv_timeout(RECV_POLL) {
                    Ok(d) => d,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                };

                let now_ns = monotonic_now_ns();
                let pcr_with_pid = match anchor {
                    AnchorSource::Pcr => {
                        // RTP-wrapped callers prepend a 12-byte RTP header;
                        // raw-TS callers set `ts_offset = 0`. Without
                        // skipping the header, the 188-byte-stride sync
                        // scan never lands on `0x47` and the pacer treats
                        // every datagram as "between PCRs", collapsing to
                        // producer cadence on RTP outputs.
                        let off = dg.ts_offset.min(dg.bytes.len());
                        crate::engine::ts_parse::first_pcr_in_ts_buffer_pid(
                            &dg.bytes[off..],
                            state.anchor_pid,
                        )
                    }
                    AnchorSource::St2110Raster => None,
                };
                // Lock onto the first PCR-bearing PID we ever see. From
                // this point on, the ts_parse helper filters to this PID —
                // PCRs from other programs in an MPTS are ignored (each
                // program has its own 27 MHz clock; mixing them produces
                // wild apparent discontinuities that collapse the closed-
                // loop pacer). One PID anchored, every other datagram
                // looks like "between PCRs" → emit ASAP. PCR_AC for the
                // anchored program tracks the same as an SPTS would.
                if let Some((_, pid)) = pcr_with_pid {
                    if state.anchor_pid.is_none() {
                        state.anchor_pid = Some(pid);
                    }
                    state.last_pcr_seen_ns = now_ns;
                } else if state.anchor_pid.is_some()
                    && state.last_pcr_seen_ns > 0
                    && now_ns.saturating_sub(state.last_pcr_seen_ns) > PCR_ANCHOR_STALE_NS
                {
                    // No PCR on anchor_pid for > 200 ms — the active
                    // input likely switched to a stream with a different
                    // PCR PID. Reset so the next PCR-bearing datagram
                    // re-locks the anchor.
                    tracing::debug!(
                        "wire-emit '{}': anchor_pid {:?} stale ({}ms), resetting",
                        id,
                        state.anchor_pid,
                        now_ns.saturating_sub(state.last_pcr_seen_ns) / 1_000_000,
                    );
                    state.anchor_pid = None;
                    state.pcr_anchor = None;
                    state.initialised = false;
                }
                let pcr_27mhz = pcr_with_pid.map(|(pcr, _)| pcr);

                let target_ns = match anchor {
                    AnchorSource::Pcr => {
                        state.derive_target(now_ns, pcr_27mhz, dg.bytes.len())
                    }
                    AnchorSource::St2110Raster => dg.target_tx_time_ns.unwrap_or(now_ns),
                };
                (dg, target_ns, pcr_27mhz)
            }
        };
        let now_ns = monotonic_now_ns();

        // PCR is left exactly as packetized. An earlier attempt to
        // re-stamp PCR to `target_ns - preroll` here was reverted —
        // it broke passthrough because PTS values in the PES headers
        // come from the source while the rewritten PCR was a
        // wallclock-derived value, so the PTS-PCR offset that
        // receivers use to time frame rendering became nonsensical
        // and decoders dropped every frame as "too late". Wire pacing
        // accuracy comes from `target_ns` driving SO_TXTIME (sub-µs)
        // or `clock_nanosleep` (~100 µs); we trust the kernel to
        // place the packet on the wire at the right instant rather
        // than rewriting the PCR field.
        if let Releaser::ClockNanosleep = releaser {
            if target_ns > now_ns {
                let before = monotonic_now_ns();
                sleep_until_monotonic_ns(target_ns);
                diag_sleep_ns = diag_sleep_ns.saturating_add(monotonic_now_ns().saturating_sub(before));
            }
        }
        if pcr_27mhz.is_some() { diag_pcr_events += 1; }

        // ── ClockNanosleep tier: batch send via sendmmsg ────────────
        //
        // Per-packet `socket.send_to` is the bottleneck under SCHED_OTHER:
        // ~30 µs syscall cost × 20 datagrams between two PCR-bearing
        // datagrams = 600 µs out of the 33 ms pacing budget for a 30 fps
        // 6 Mbps TS. That margin lost per cycle accumulates into the
        // `wire_tx` channel; a long run grows the channel to the cap →
        // drop-on-full. The fix: after sleeping for the anchor's target,
        // greedily drain the channel for follow-on datagrams that are
        // also ready NOW, and ship them all in one `sendmmsg`.
        //
        // Only meaningful for `Pcr` + `ClockNanosleep`. ST 2110 raster
        // anchor has per-packet target tx times and the existing v1
        // per-packet `send_with_txtime` path is fine at the rates we
        // currently drive it. SO_TXTIME path leaves kernel pacing in
        // place; batching would need `send_batch_with_txtime` (already
        // implemented but separate wire-up).
        let use_batch =
            matches!(releaser, Releaser::ClockNanosleep) && matches!(anchor, AnchorSource::Pcr);
        if use_batch {
            batch.clear();
            batch.push(BatchEntry { dg, pcr_27mhz });
            // Greedily pull more datagrams that are already ready to
            // send (target ≤ now). Stop when:
            //   - channel is empty (most common — burst already drained)
            //   - next datagram has a future target (defer to next loop)
            //   - we hit NANOSLEEP_BATCH_MAX (cap collection latency)
            //   - channel disconnected (return)
            // Track Disconnected separately — we must still ship the
            // accumulated batch before exiting, otherwise teardown drops
            // up to NANOSLEEP_BATCH_MAX in-flight datagrams on the floor.
            let mut channel_disconnected = false;
            while batch.len() < NANOSLEEP_BATCH_MAX {
                match rx.try_recv() {
                    Ok(next_dg) => {
                        let now_ns = monotonic_now_ns();
                        let off = next_dg.ts_offset.min(next_dg.bytes.len());
                        let next_pcr_with_pid =
                            crate::engine::ts_parse::first_pcr_in_ts_buffer_pid(
                                &next_dg.bytes[off..],
                                state.anchor_pid,
                            );
                        if let Some((_, pid)) = next_pcr_with_pid {
                            if state.anchor_pid.is_none() {
                                state.anchor_pid = Some(pid);
                            }
                        }
                        let next_pcr_27mhz = next_pcr_with_pid.map(|(pcr, _)| pcr);
                        let next_target = state.derive_target(
                            now_ns,
                            next_pcr_27mhz,
                            next_dg.bytes.len(),
                        );
                        if next_target <= now_ns {
                            batch.push(BatchEntry {
                                dg: next_dg,
                                pcr_27mhz: next_pcr_27mhz,
                            });
                        } else {
                            // Future target — defer to next iteration.
                            // We've already advanced `state` via
                            // `derive_target`, so the next iteration MUST
                            // skip the derive call. Stash the resolved
                            // target_ns and pcr_27mhz alongside the dg so
                            // the next loop's main path picks them up via
                            // the `pending.take()` arm and skips the
                            // re-derive that would double-advance state.
                            pending = Some(PendingDatagram {
                                dg: next_dg,
                                target_ns: next_target,
                                pcr_27mhz: next_pcr_27mhz,
                            });
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        channel_disconnected = true;
                        break;
                    }
                }
            }

            // Send the batch in a single sendmmsg syscall (or per-packet
            // on non-Linux, or if the batch is size 1).
            let bufs: Vec<&[u8]> = batch.iter().map(|e| e.dg.bytes.as_ref()).collect();
            let send_result =
                crate::engine::wire_emit_txtime::send_batch_simple(&socket, dest, &bufs);
            let now_us = crate::util::time::now_us();
            match send_result {
                Ok(n_sent) => {
                    let mut bytes_total: u64 = 0;
                    for entry in batch.iter().take(n_sent) {
                        bytes_total =
                            bytes_total.saturating_add(entry.dg.bytes.len() as u64);
                        stats.record_latency(entry.dg.recv_time_us);
                        if let Some(pcr) = entry.pcr_27mhz {
                            stats.record_pcr_egress(pcr, now_us);
                        }
                        // A/V sync drift: feed every TS packet.
                        let ts_start = entry.dg.ts_offset.min(entry.dg.bytes.len());
                        let ts_data = &entry.dg.bytes[ts_start..];
                        let mut off = 0;
                        while off + 188 <= ts_data.len() {
                            stats.observe_av_sync_packet(&ts_data[off..off + 188]);
                            off += 188;
                        }
                    }
                    stats
                        .packets_sent
                        .fetch_add(n_sent as u64, Ordering::Relaxed);
                    stats.bytes_sent.fetch_add(bytes_total, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!(
                        "wire-emit '{}' batch send error ({} dgms): {}",
                        id,
                        batch.len(),
                        e
                    );
                }
            }
            // Diagnostic accounting — running average batch size over a
            // 5 s window, emitted at DEBUG level so operators can confirm
            // the sendmmsg optimisation is firing on bursty TS outputs
            // (`RUST_LOG=bilbycast_edge::engine::wire_emit=debug`). Silent
            // at default log level.
            diag_dgms = diag_dgms.saturating_add(batch.len() as u64);
            batch_count = batch_count.saturating_add(1);
            batch_dgms_total = batch_dgms_total.saturating_add(batch.len() as u64);
            let now = monotonic_now_ns();
            if now >= batch_log_next_ns && batch_count > 0 {
                let avg = batch_dgms_total as f64 / batch_count as f64;
                let dt_s = (now.saturating_sub(diag_last_ns)) as f64 / 1e9;
                let dgms_per_s = diag_dgms as f64 / dt_s.max(0.001);
                let mbps = dgms_per_s * 1316.0 * 8.0 / 1e6;
                let sleep_pct = diag_sleep_ns as f64 / (now.saturating_sub(diag_last_ns)) as f64 * 100.0;
                let depth = rx.depth.load(std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    "wire-emit '{}' diag: {:.0} dgm/s ({:.2} Mbps)  pcr={}/5s  sleep={:.1}%  batch_avg={:.1}  depth={}",
                    id, dgms_per_s, mbps, diag_pcr_events, sleep_pct, avg, depth
                );
                batch_count = 0;
                batch_dgms_total = 0;
                diag_dgms = 0;
                diag_pcr_events = 0;
                diag_sleep_ns = 0;
                diag_last_ns = now;
                batch_log_next_ns = now.saturating_add(5_000_000_000);
            }
            // Honor a Disconnect that happened during batch collection
            // — exit AFTER shipping the batch above.
            if channel_disconnected {
                return;
            }
        } else {
            // Non-batched path: ST 2110 raster anchor or SO_TXTIME releaser.
            let send_result = match releaser {
                Releaser::ClockNanosleep => socket.send_to(&dg.bytes, dest),
                Releaser::SoTxtime => {
                    // Kernel handles the wallclock target via ETF qdisc.
                    // No userspace sleep — the thread loops at producer
                    // rate with bounded recvmsg + sendmsg cost.
                    crate::engine::wire_emit_txtime::send_with_txtime(
                        &socket, dest, &dg.bytes, target_ns,
                    )
                }
            };
            match send_result {
                Ok(sent) => {
                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                    stats.record_latency(dg.recv_time_us);
                    if let Some(pcr) = pcr_27mhz {
                        stats.record_pcr_egress(pcr, crate::util::time::now_us());
                    }
                    // A/V sync drift: feed every TS packet.
                    let ts_start = dg.ts_offset.min(dg.bytes.len());
                    let ts_data = &dg.bytes[ts_start..];
                    let mut off = 0;
                    while off + 188 <= ts_data.len() {
                        stats.observe_av_sync_packet(&ts_data[off..off + 188]);
                        off += 188;
                    }
                }
                Err(e) => {
                    tracing::warn!("wire-emit '{}' send error: {}", id, e);
                }
            }
        }

        // Periodic errqueue drain on the SO_TXTIME path. Cheap when
        // empty (one non-blocking recvmsg returning EAGAIN).
        if let Releaser::SoTxtime = releaser {
            packets_since_drain = packets_since_drain.wrapping_add(1);
            if packets_since_drain >= SO_TXTIME_ERRQUEUE_DRAIN_EVERY {
                let late = crate::engine::wire_emit_txtime::drain_errqueue_late_count(&socket);
                if late > 0 {
                    stats.wire_pacing_late.fetch_add(late, Ordering::Relaxed);
                }
                packets_since_drain = 0;
            }
        }
    }
}

// ── Platform: clock_nanosleep + SCHED_FIFO ──────────────────────────────

/// Wire-emit's reference clock. CLOCK_TAI on Linux: PTP-disciplined when
/// `ptp4l` + `phc2sys` are running (broadcast-grade, sub-µs PCR_AC against
/// a grandmaster); equivalent to system clock + leap seconds otherwise
/// (still ms-jittery but at least the kernel etf qdisc accepts it). The
/// kernel etf qdisc on Intel ice/igc drivers REJECTS CLOCK_MONOTONIC, so
/// SO_TXTIME with MONOTONIC silently degrades to "kernel ignores cmsg" —
/// using TAI keeps the path live and gains free PTP-discipline when the
/// operator wires up ptp4l. Non-Linux falls back to a generic monotonic
/// clock — production paths are Linux-only.
#[cfg(target_os = "linux")]
pub(super) fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe {
        libc::clock_gettime(libc::CLOCK_TAI, &mut ts);
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64)
}

#[cfg(target_os = "linux")]
pub(super) fn sleep_until_monotonic_ns(target_ns: u64) {
    let ts = libc::timespec {
        tv_sec: (target_ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (target_ns % 1_000_000_000) as i64,
    };
    // EINTR returns: re-arm with the same absolute deadline.
    loop {
        let rc = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_TAI,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        };
        if rc != libc::EINTR {
            break;
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn monotonic_now_ns() -> u64 {
    use std::time::Instant;
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_nanos() as u64
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sleep_until_monotonic_ns(target_ns: u64) {
    let now = monotonic_now_ns();
    if target_ns > now {
        std::thread::sleep(Duration::from_nanos(target_ns - now));
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 27 MHz ticks per millisecond.
    const TICK_PER_MS: u64 = 27_000;

    #[test]
    fn first_datagram_anchors_at_now_plus_preroll() {
        let mut s = TargetState::default();
        let target = s.derive_target(1_000_000_000, Some(900_000), 1316);
        assert_eq!(target, 1_000_000_000 + PREROLL_NS);
        assert!(s.initialised);
        assert_eq!(s.pcr_anchor, Some(900_000));
    }

    #[test]
    fn pcr_to_pcr_delta_drives_target() {
        let mut s = TargetState::default();
        // Init with first datagram at t=0, PCR=27 000 (1 ms).
        let _ = s.derive_target(0, Some(TICK_PER_MS), 1316);
        let anchor_wall = s.wall_anchor_ns;

        // Next PCR datagram, 40 ms later in PCR space.
        let target = s.derive_target(
            anchor_wall + 100_000, // wall isn't used for PCR-driven targets
            Some(TICK_PER_MS + 40 * TICK_PER_MS),
            1316,
        );
        // Expected: anchor_wall + 40 ms in ns.
        assert_eq!(target, anchor_wall + 40_000_000);
        // Re-anchor: bytes_since_anchor reset, wall advanced.
        assert_eq!(s.bytes_since_anchor, 0);
        assert_eq!(s.wall_anchor_ns, target);
    }

    #[test]
    fn between_pcrs_emit_asap_after_anchor() {
        let mut s = TargetState::default();
        // First datagram, PCR=0. wall_anchor = preroll into the future.
        let _ = s.derive_target(0, Some(0), 1316);
        // Seed observed rate via a second PCR 40 ms later carrying
        // 50 000 bytes accumulated in between (the test fakes this by
        // setting bytes_since_anchor directly so the observed-rate
        // EMA fires).
        s.bytes_since_anchor = 50_000;
        let _ = s.derive_target(0, Some(40 * TICK_PER_MS), 1316);
        assert_eq!(s.observed_rate_bps, 10_000_000); // 50 KB / 40 ms = 10 Mbps (telemetry only)
        let anchor = s.wall_anchor_ns;

        // Non-PCR datagram between PCRs: target = max(wall_anchor, now).
        let target = s.derive_target(anchor + 1, None, 1316);
        assert_eq!(target, anchor + 1);
        assert_eq!(s.bytes_since_anchor, 1316);

        let target2 = s.derive_target(anchor + 100_000, None, 1316);
        assert_eq!(target2, anchor + 100_000);
        assert_eq!(s.bytes_since_anchor, 2632);
    }

    #[test]
    fn discontinuity_backwards_resets_anchor() {
        let mut s = TargetState::default();
        let _ = s.derive_target(0, Some(100 * TICK_PER_MS), 1316);
        let now = 5_000_000_000u64;
        // Backwards PCR: should reset to "now".
        let target = s.derive_target(now, Some(50 * TICK_PER_MS), 1316);
        assert_eq!(target, now);
        assert_eq!(s.wall_anchor_ns, now);
        assert_eq!(s.pcr_anchor, Some(50 * TICK_PER_MS));
    }

    #[test]
    fn discontinuity_forward_jump_resets_anchor() {
        let mut s = TargetState::default();
        let _ = s.derive_target(0, Some(TICK_PER_MS), 1316);
        let now = 5_000_000_000u64;
        // 600 ms forward jump > 500 ms threshold.
        let target = s.derive_target(now, Some(601 * TICK_PER_MS), 1316);
        assert_eq!(target, now);
        assert_eq!(s.pcr_anchor, Some(601 * TICK_PER_MS));
    }

    /// A PCR discontinuity must reset pacing to "now" — both the
    /// `wall_anchor_ns` AND the `last_returned_ns` (the outer monotonic
    /// guard's reference). The discontinuity branch already sets
    /// `wall_anchor = now`; if we don't ALSO reset `last_returned`,
    /// the outer guard fast-forwards the returned target back to
    /// `last_returned + ε` (= the previously-queued future value),
    /// which is the documented "wire pacer collapses to ~0.5 Mbps after
    /// 4-5 input switches" regression — the new input then paces from
    /// that future anchor, every subsequent natural-paced packet
    /// targets further out, and the wire-emit channel drowns in drops.
    #[test]
    fn discontinuity_resets_pacing_to_now() {
        let mut s = TargetState::default();
        // Prime the state so `last_returned_ns` is far in the future.
        let _ = s.derive_target(1_000_000_000, Some(0), 1316);
        s.bytes_since_anchor = 100_000;
        let _ = s.derive_target(1_000_000_000, Some(100 * TICK_PER_MS), 1316);
        let last_before = s.last_returned_ns;
        // Now a discontinuity. now_ns is BEFORE last_returned (residue
        // queue effect from the previously paced-ahead state).
        let now = last_before.saturating_sub(10_000_000); // 10 ms in the past
        let target = s.derive_target(now, Some(10 * TICK_PER_MS), 1316);
        // After my fix: discontinuity resets last_returned to now, so
        // the outer guard's `raw < last_returned` check sees raw=now,
        // last_returned=now, and uses raw — no fast-forward.
        assert_eq!(target, now);
        assert_eq!(s.wall_anchor_ns, now);
        assert_eq!(s.last_returned_ns, now);
        assert_eq!(s.observed_rate_bps, 0, "discontinuity must clear stale observed rate");
    }

    #[test]
    fn small_forward_jump_under_threshold_keeps_anchor() {
        let mut s = TargetState::default();
        let _ = s.derive_target(0, Some(TICK_PER_MS), 1316);
        let anchor = s.wall_anchor_ns;
        // 400 ms forward — well under the 500 ms discontinuity bound.
        // Use a `now_ns` far enough in the future so the 200 ms
        // lookahead cap doesn't trip (the cap is only meant to catch
        // pacer-runaway scenarios where `wall_anchor` outraces
        // wallclock; here we're emulating the real case where
        // wallclock has caught up to ~PCR time).
        let now = anchor + 350_000_000; // 350 ms past anchor
        let target = s.derive_target(now, Some(401 * TICK_PER_MS), 1316);
        assert_eq!(target, anchor + 400_000_000);
    }

    #[test]
    fn observed_rate_emas_from_inter_pcr_observations() {
        let mut s = TargetState::default();
        // First datagram, PCR.
        let _ = s.derive_target(0, Some(0), 1316);
        // Set bytes_since_anchor directly so the EMA fires on the next
        // PCR (the API has no way to add bytes without arrival, which
        // is the exact reason between-PCR pacing only kicks in after
        // an observation).
        s.bytes_since_anchor = 50_000;
        // Second PCR 40 ms later. Observed = 50 000 B / 40 ms = 10 Mbps.
        let _ = s.derive_target(0, Some(40 * TICK_PER_MS), 1316);
        assert_eq!(s.observed_rate_bps, 10_000_000);

        // Third PCR 40 ms later, observed = 60 000 B / 40 ms = 12 Mbps.
        // EMA: (3*10 + 12) / 4 = 10.5 Mbps.
        s.bytes_since_anchor = 60_000;
        let _ = s.derive_target(0, Some(80 * TICK_PER_MS), 1316);
        assert_eq!(s.observed_rate_bps, 10_500_000);
    }

    #[test]
    fn cold_start_targets_wall_anchor_until_first_inter_pcr_pair() {
        let mut s = TargetState::default();
        // First datagram has PCR — anchors at now+preroll.
        let _ = s.derive_target(0, Some(0), 1316);
        let anchor = s.wall_anchor_ns;
        // Subsequent non-PCR datagrams arrive before the second PCR
        // lands. With no observed rate yet (= 0), they fall back to
        // wall_anchor.max(now) — preserves FIFO order through preroll.
        let now = anchor - 1_000_000; // pretend we're inside preroll
        let target = s.derive_target(now, None, 1316);
        assert_eq!(target, anchor, "cold-start non-PCR must target wall_anchor during preroll");
        let target2 = s.derive_target(now, None, 1316);
        assert_eq!(target2, anchor);

        // After two PCRs land, the EMA seeds and interpolation activates.
        s.bytes_since_anchor = 50_000;
        let _ = s.derive_target(now, Some(40 * TICK_PER_MS), 1316);
        assert!(s.observed_rate_bps > 0);
        // Non-PCR datagrams emit ASAP: target = max(wall_anchor, now).
        let post = s.derive_target(now + 1, None, 1316);
        assert_eq!(post, s.wall_anchor_ns.max(now + 1));
    }

    #[test]
    fn cold_start_after_preroll_targets_now() {
        // Same as above but `now_ns` is past `wall_anchor`. The fallback
        // `wall_anchor.max(now_ns)` should return now to keep emit
        // current with real time once preroll is over.
        let mut s = TargetState::default();
        let _ = s.derive_target(0, Some(0), 1316);
        let after_preroll = s.wall_anchor_ns + 1_000_000_000;
        let target = s.derive_target(after_preroll, None, 1316);
        assert_eq!(target, after_preroll);
    }

    #[test]
    fn monotonic_now_advances() {
        let a = monotonic_now_ns();
        std::thread::sleep(Duration::from_millis(2));
        let b = monotonic_now_ns();
        assert!(b > a);
        assert!(b - a >= 1_000_000); // ≥ 1 ms
    }

    #[test]
    fn sleep_until_monotonic_returns_at_or_after_target() {
        let target = monotonic_now_ns() + 2_000_000; // +2 ms
        sleep_until_monotonic_ns(target);
        let after = monotonic_now_ns();
        assert!(after >= target, "after={} target={}", after, target);
    }

    /// End-to-end smoke: spawn the emitter, push three TS datagrams,
    /// observe monotonic emit pacing. UDP send target is loopback :0
    /// (kernel allocated port that no one's listening on — kernel
    /// still drops the datagram cleanly without erroring on
    /// Linux/macOS).
    #[test]
    fn emitter_paces_and_sends_pcr_anchor() {
        use std::net::Ipv4Addr;
        let bind = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let dest = bind.local_addr().unwrap();
        let send = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        send.set_nonblocking(false).unwrap();

        let stats = Arc::new(OutputStatsAccumulator::new(
            "test".to_string(),
            "test".to_string(),
            "udp".to_string(),
        ));
        let cancel = CancellationToken::new();
        let tx = spawn_wire_emitter(
            "test".to_string(),
            send,
            dest,
            AnchorSource::Pcr,
            stats.clone(),
            cancel.clone(),
        );

        let mut bytes = vec![0u8; 1316];
        // Build a TS packet with PCR so first_pcr_in_ts_buffer finds it.
        bytes[0] = 0x47; // sync
        bytes[3] = 0x20; // adaptation-field only, no payload
        bytes[4] = 7; // af_len
        bytes[5] = 0x10; // PCR flag
        // PCR base = 0 → emit at first wall_anchor.
        for _ in 0..3 {
            tx.try_send(WireDatagram {
                bytes: Bytes::from(bytes.clone()),
                recv_time_us: 0,
                target_tx_time_ns: None,
                ts_offset: 0,
            })
            .unwrap();
        }
        drop(tx);

        // Wait for emitter to drain past PREROLL_NS + a margin.
        std::thread::sleep(Duration::from_millis(200));
        cancel.cancel();

        let sent = stats.packets_sent.load(Ordering::Relaxed);
        assert!(sent >= 1, "expected at least one packet sent, got {}", sent);
    }

    /// St2110Raster anchor: caller-supplied target_tx_time_ns drives
    /// emission. PCR parsing is skipped.
    #[test]
    fn emitter_honours_st2110_raster_target() {
        use std::net::Ipv4Addr;
        let bind = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let dest = bind.local_addr().unwrap();
        let send = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        send.set_nonblocking(false).unwrap();

        let stats = Arc::new(OutputStatsAccumulator::new(
            "test-2110".to_string(),
            "test-2110".to_string(),
            "st2110_20".to_string(),
        ));
        let cancel = CancellationToken::new();
        let tx = spawn_wire_emitter(
            "test-2110".to_string(),
            send,
            dest,
            AnchorSource::St2110Raster,
            stats.clone(),
            cancel.clone(),
        );

        let now = monotonic_now_ns();
        for i in 0..3 {
            tx.try_send(WireDatagram {
                bytes: Bytes::from_static(b"st2110-raster-test"),
                recv_time_us: 0,
                target_tx_time_ns: Some(now + i * 100_000),
                ts_offset: 0,
            })
            .unwrap();
        }
        drop(tx);

        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
        let sent = stats.packets_sent.load(Ordering::Relaxed);
        assert_eq!(sent, 3);
    }

    /// Regression — RTP outputs prepend a 12-byte header before the TS
    /// stream. Before the `ts_offset` field landed, `wire_emit`'s PCR
    /// scan walked the buffer from byte 0 and never found a `0x47`
    /// sync at any 188-byte boundary; PCR-pacing therefore never fired
    /// and RTP outputs collapsed to producer cadence. This test pins
    /// the contract: scanning at `ts_offset` recovers the same PCR an
    /// equivalent raw-TS datagram would have produced.
    #[test]
    fn pcr_extracted_from_rtp_wrapped_datagram() {
        let pcr_27mhz: u64 = 1_234_567 * 300; // arbitrary 27 MHz value
        let mut ts = vec![0u8; 188];
        ts[0] = 0x47;
        ts[1] = 0x01; // PID upper 5 bits = 0, then 0x100
        ts[2] = 0x00;
        ts[3] = 0x20; // adaptation-field only
        ts[4] = 7;    // af_len
        ts[5] = 0x10; // PCR flag set
        let base = pcr_27mhz / 300;
        let ext = pcr_27mhz % 300;
        ts[6]  = ((base >> 25) & 0xFF) as u8;
        ts[7]  = ((base >> 17) & 0xFF) as u8;
        ts[8]  = ((base >> 9) & 0xFF) as u8;
        ts[9]  = ((base >> 1) & 0xFF) as u8;
        ts[10] = (((base & 0x01) << 7) as u8) | 0x7E | (((ext >> 8) & 0x01) as u8);
        ts[11] = (ext & 0xFF) as u8;
        // Sanity: extract_pcr on the raw 188 should round-trip.
        assert_eq!(
            crate::engine::ts_parse::extract_pcr(&ts),
            Some(pcr_27mhz),
            "fixture extract_pcr round-trip failed"
        );

        // Build the exact buffer shape output_rtp.rs hands to wire_emit.
        let mut buf = vec![0u8; 12]; // RTP header bytes (content irrelevant)
        buf[0] = 0x80; // RTP V=2, P=0, X=0, CC=0
        buf[1] = 33;   // PT = MP2T
        buf.extend_from_slice(&ts);

        let dg = WireDatagram {
            bytes: Bytes::from(buf),
            recv_time_us: 0,
            target_tx_time_ns: None,
            ts_offset: 12,
        };

        // Mirror the exact slice expression used by run_emitter.
        let off = dg.ts_offset.min(dg.bytes.len());
        let found = crate::engine::ts_parse::first_pcr_in_ts_buffer_pid(
            &dg.bytes[off..],
            None,
        );
        assert_eq!(
            found.map(|(p, _pid)| p),
            Some(pcr_27mhz),
            "PCR scan must recover the source PCR through the RTP-header offset; \
             the previous code scanned from byte 0 and silently returned None, \
             collapsing every RTP output to producer cadence"
        );

        // And confirm the broken pre-fix path: scanning the whole buffer
        // (offset 0) misses the PCR — that's the regression we just
        // fixed. Document it here so a future change that restores
        // offset-0 scanning is caught immediately.
        let broken = crate::engine::ts_parse::first_pcr_in_ts_buffer_pid(
            &dg.bytes[..],
            None,
        );
        assert!(
            broken.is_none(),
            "control: scanning the RTP-wrapped buffer from byte 0 must \
             miss the PCR (this is what justified the ts_offset field); \
             if this assertion ever fails the fixture has changed"
        );
    }
}
