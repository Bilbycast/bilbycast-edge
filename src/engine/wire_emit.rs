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

/// Minimum lead time (ns) stamped into a SO_TXTIME launch timestamp on the
/// ETF path. The `etf` qdisc DROPS any packet whose launch time is at or
/// behind "now" when it reaches the qdisc (its `delta` lookahead is 200 µs);
/// the generator legitimately emits "now" targets (late-rebase, PCR
/// discontinuities, input-switch resets, catch-up on bursty/contribution
/// sources), and on a bursty source that dropped ~14 % of packets at the
/// qdisc. Flooring the launch time to `now + ETF_LATE_FLOOR_NS` turns a late
/// packet into "emit ~1 ms from now" instead of a drop. 1 ms is comfortably
/// above the 200 µs etf `delta` + sendmsg→qdisc latency, and trivial added
/// latency. On-time targets (the common case) already exceed the floor and
/// pass through untouched; because both the derived target and the floor are
/// monotonically non-decreasing, the floored stream stays ordered (no etf
/// reorder of HEVC reference frames).
///
/// Sized at 15 ms (not the etf `delta` + a hair): the floor must also cover
/// the gap between stamping `now` here and the etf qdisc's hrtimer actually
/// dequeuing the packet. On a contended box where the SCHED_FIFO wire-emit
/// thread shares cores with the input/media-pacer threads, that gap was
/// observed to exceed 1 ms for ~13 % of packets — etf then dropped them as
/// late (data loss). 15 ms is comfortably past worst-case scheduling slop yet
/// well inside the receiver's T-STD jitter buffer (output PCR carries 80 ms
/// preroll), so a floored (late) packet is delivered intact, just slightly
/// late, instead of dropped. CPU-isolating the wire-emit thread
/// (`BILBYCAST_WIRE_EMIT_CPUS`) is the complementary deployment-side fix.
const ETF_LATE_FLOOR_NS: u64 = 15_000_000;

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

/// Wire-pacing QoS class — selects whether an output may ride the kernel
/// ETF / SO_TXTIME hard-drop path. Orthogonal to [`AnchorSource`] (which
/// only decides how the send target is *derived*); this decides which
/// traffic class the packets land on.
///
/// The ETF qdisc DROPS any packet whose launch time has already passed
/// when it reaches the qdisc. That is correct for ST 2110 uncompressed
/// essence (no receiver re-timing buffer — wire precision is the only
/// timing source) but WRONG for compressed MPEG-TS, where the receiver
/// reconstructs timing from PCR + its jitter buffer (and from SRT/RIST
/// TSBPD when carried over those). A compressed feed forced onto the etf
/// class loses packets as "late" on a contended box — unrecoverable on
/// RTP/UDP (no ARQ). So compressed outputs stay LOSSLESS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WirePacingClass {
    /// Compressed MPEG-TS (UDP / RTP / SMPTE 302M). Paced onto the wire by the
    /// kernel etf qdisc via SO_TXTIME when the operator enabled it
    /// (`BILBYCAST_ENABLE_TXTIME=1`) and an etf qdisc is present — the late-drop
    /// that compressed feeds suffer on etf is prevented by the
    /// `ETF_LATE_FLOOR_NS` (15 ms) launch floor (so a late packet emits ~15 ms
    /// out instead of being dropped). A Lossless output carrying DSCP is pinned
    /// `SO_PRIORITY=0` so it lands on the etf TC0 class (the DSCP wire byte is
    /// unaffected). Falls back to userspace `clock_nanosleep` when no etf qdisc
    /// is present. The receiver still re-times via PCR + jitter buffer.
    Lossless,
    /// ST 2110 essence (-20/-23 video, -30/-31 audio, -40 ANC). No
    /// receiver re-timing buffer, so wire precision matters: eligible for
    /// SO_TXTIME + the ETF qdisc class when the operator enabled it
    /// (`BILBYCAST_ENABLE_TXTIME=1`) and the setsockopt probe succeeds.
    EtfEligible,
}

/// Egress de-jitter / smoothing-buffer policy for the wire emitter.
///
/// Compressed MPEG-TS (`WirePacingClass::Lossless`) must never let a
/// source-rate-vs-wallclock mismatch or burst grow the internal queue
/// unbounded (the diagnosed 47 s latency runaway). When `enabled`, the
/// `clock_nanosleep` drain enforces a hard RESIDENCE CAP: a datagram that
/// has waited longer than `shed_residence_ns` (and the stale backlog behind
/// it) is shed and the pacing anchor re-set to "now", bounding output
/// latency by construction — the downstream receiver re-clocks from the
/// (untouched) PCR via its own T-STD buffer. ST 2110
/// (`WirePacingClass::EtfEligible`) sets `enabled = false`: its strict
/// raster / SO_TXTIME pacing owns timing and has no receiver re-clock.
/// Full rationale + IRD references: `docs/egress-dejitter-design.md`.
#[derive(Clone, Copy, Debug)]
pub struct DejitterConfig {
    pub enabled: bool,
    /// Hard end-to-end residence cap (ns); older datagrams are shed.
    pub shed_residence_ns: u64,
    /// After a shed, drain the wire channel down to this many datagrams
    /// (the de-jitter buffer setpoint floor, ~56 ms @ 6 Mbps for 32).
    pub drain_floor_dgms: usize,
    /// Release-rate servo setpoint: the buffer fill (in ms of content) the
    /// servo holds the queue centred on. The ±authority rate trim pulls the
    /// fill back toward this. 0 disables the servo (shed-only). Default 60 ms.
    pub setpoint_ms: u64,
    /// Servo rate authority in permille (‰) of the recovered source rate.
    /// 50 = ±5 %: enough to absorb any realistic source-vs-wallclock ppm
    /// offset, small enough that the induced PCR_OJ stays inside the
    /// receiver T-STD. Default 50.
    pub authority_permille: u64,
    /// De-jitter buffer mode (OPT-IN): build + hold a `setpoint_ms` cushion so
    /// a bursty/jittery contribution is metered out smoothly. Engaged ONLY when
    /// the operator explicitly sets a per-output `egress_buffer_ms` (or the env
    /// override). When `false` (the default — `egress_buffer_ms` unset) the
    /// servo keeps its legacy emit-on-arrival behaviour byte-for-byte, so the
    /// default deploy carries zero regression risk and adds no latency. Set an
    /// `egress_buffer_ms` to turn the output into a real de-jitter buffer that
    /// absorbs `~setpoint_ms` of arrival jitter (at the cost of that latency).
    pub seed_cushion: bool,
}

impl DejitterConfig {
    /// Default policy for compressed outputs: release-rate servo holding a
    /// 60 ms buffer with ±5 % authority, backed by a 250 ms residence cap
    /// (drain back to ~32 datagrams). Inside the receiver T-STD envelope
    /// (≤ ~0.7 s) and SRT/RIST receive-latency headroom.
    #[cfg(test)] // test-only convenience wrapper; production paths call servo_with
    pub fn servo() -> Self {
        Self::servo_with(None)
    }

    /// As `servo` (the no-arg wrapper) but with an operator-supplied setpoint. Precedence
    /// for the buffer setpoint: explicit per-output `egress_buffer_ms` config
    /// > `BILBYCAST_EGRESS_BUFFER_MS` env > 60 ms default (all clamped to
    /// [20, 2000] ms). The residence cap defaults to `max(4×setpoint, 250)`
    /// ms (overridable via `BILBYCAST_EGRESS_RESIDENCE_MS`) so a bigger
    /// buffer gets proportionally more burst headroom before the hard shed.
    pub fn servo_with(egress_buffer_ms: Option<u32>) -> Self {
        // De-jitter (cushion) is OPT-IN: only when the operator explicitly set
        // a per-output egress_buffer_ms or the env override. Unset → legacy
        // emit-on-arrival servo (no cushion, no added latency, no regression).
        let env_buf = std::env::var("BILBYCAST_EGRESS_BUFFER_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        let seed_cushion = egress_buffer_ms.is_some() || env_buf.is_some();
        let setpoint_ms = egress_buffer_ms
            .map(|m| m as u64)
            .or(env_buf)
            .unwrap_or(60)
            .clamp(20, 2000);
        let cap_ms = std::env::var("BILBYCAST_EGRESS_RESIDENCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| (setpoint_ms.saturating_mul(4)).max(250))
            .clamp(setpoint_ms.saturating_add(40), 5_000);
        Self {
            enabled: true,
            shed_residence_ns: cap_ms.saturating_mul(1_000_000),
            drain_floor_dgms: 32,
            setpoint_ms,
            authority_permille: 50,
            seed_cushion,
        }
    }
    /// No de-jitter (ST 2110 strict pacing / SO_TXTIME owns timing).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            shed_residence_ns: 0,
            drain_floor_dgms: 0,
            setpoint_ms: 0,
            authority_permille: 0,
            seed_cushion: false,
        }
    }
    /// Residence cap in microseconds (for comparison with `recv_time_us`).
    fn shed_residence_us(&self) -> u64 {
        self.shed_residence_ns / 1_000
    }
}

impl Default for DejitterConfig {
    fn default() -> Self {
        Self::disabled()
    }
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
    pacing: WirePacingClass,
    egress_buffer_ms: Option<u32>,
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
    // Egress pacing: BOTH ST 2110 (EtfEligible) AND compressed MPEG-TS
    // (Lossless) ride the kernel etf qdisc via SO_TXTIME — the industry-standard
    // hardware-timestamped egress pacer. De-jitter belongs at INGRESS (SRT
    // TSBPD / RIST / ingress_dejitter_ms), NOT egress; egress only paces. The
    // earlier split that routed Lossless onto a userspace clock_nanosleep +
    // release-rate servo shed ~50 % of a normal compressed feed (servo
    // residence-cap firing) — reverted. The etf "late-drop" that originally
    // motivated the split is prevented by the ETF_LATE_FLOOR_NS (15 ms) launch
    // floor on the SO_TXTIME path: targets are stamped >= now+15 ms, so on a
    // SCHED_FIFO wire-emit thread the kernel never sees a past launch time.
    // Falls back to clock_nanosleep automatically when no etf qdisc is present
    // (try_enable_so_txtime fails).
    let etf_eligible = matches!(
        pacing,
        WirePacingClass::EtfEligible | WirePacingClass::Lossless
    );
    let releaser = if !force_nanosleep
        && enable_so_txtime
        && etf_eligible
        && try_enable_so_txtime(&socket, clockid)
    {
        // A compressed (Lossless) output usually carries a DSCP byte (e.g. 46),
        // which the kernel maps to a non-TC0 priority -> off the etf class. Pin
        // SO_PRIORITY explicitly (default 0 -> TC0/etf) so it lands on the etf
        // class regardless of DSCP. The DSCP byte on the wire is set separately
        // via IP_TOS and is unaffected (QoS marking preserved). ST 2110 has no
        // DSCP -> already prio 0 -> TC0, so this is a harmless no-op there.
        if matches!(pacing, WirePacingClass::Lossless) {
            let etf_prio = std::env::var("BILBYCAST_ETF_SO_PRIORITY")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            match crate::engine::wire_emit_txtime::set_so_priority(&socket, etf_prio) {
                Ok(()) => tracing::info!(
                    "wire-emit '{}': compressed output pinned to SO_PRIORITY={} (etf TC0, hardware-paced)",
                    id,
                    etf_prio
                ),
                Err(e) => tracing::debug!(
                    "wire-emit '{}': set_so_priority({}) failed: {} (no mqprio/etf qdisc — clock_nanosleep fallback)",
                    id,
                    etf_prio,
                    e
                ),
            }
        }
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
    // Surface live queue depth on the output stats so the manager UI can
    // show the egress buffer occupancy (and catch a runaway early).
    stats.set_wire_emit_depth_handle(Arc::clone(&depth));
    let thread_id = id.clone();
    let stats_for_thread = stats.clone();
    // CPU pin index — round-robin across the operator-configured set so
    // multiple wire-emit threads on the same edge spread across the
    // dedicated cores instead of stacking onto one.
    let cpu_index = next_wire_emit_cpu_index();
    // Compressed (Lossless) egress runs a CLOSED-LOOP bounded pacer by default.
    //
    // Open-loop PCR-delta pacing (the prior default) has NO term comparing the
    // internal wire-queue fill or the wallclock drain rate, so any sub-% source-
    // rate-vs-CLOCK_TAI offset, `ffmpeg -re` overrun, SRT jitter-buffer dump,
    // file-loop PCR-handoff drift, or encoder burst integrates into this thread's
    // input channel with nothing correcting it — a MEASURED output-latency
    // runaway to 1–2 s+ on BOTH file (media_player) and live (SRT) inputs
    // (2026-06-04). The receiver's T-STD/VBX buffer then over/underflows →
    // pixelation + pausing on consumer players (VLC) and lighter STBs.
    //
    // The release-rate servo replaces that open-loop integration with a leaky-
    // bucket release trimmed ±authority by the buffer-fill error, holding
    // residence near `setpoint_ms` (~60 ms) — a steady source offset is absorbed
    // as a steady, tiny rate trim with NO accumulation and NO loss. With
    // `egress_buffer_ms` UNSET (the default) `seed_cushion=false`: cold-start
    // emits ASAP (no added startup latency) and there is no hold-release gap —
    // only the gentle rate trim, ~0 at steady state. Setting `egress_buffer_ms`
    // (or BILBYCAST_EGRESS_BUFFER_MS) additionally seeds a jitter-absorption
    // cushion for genuinely bursty INGRESS, at the cost of that latency.
    //
    // ST 2110 / MXL (`EtfEligible`, `St2110Raster` anchor) keep strict
    // isochronous raster + SO_TXTIME pacing — that IS their clock — and NEVER
    // take the servo, by construction (this gate is Lossless-only and the raster
    // anchor bypasses `derive_target` entirely).
    let want_egress_servo = matches!(pacing, WirePacingClass::Lossless);
    let dejitter = if want_egress_servo {
        DejitterConfig::servo_with(egress_buffer_ms)
    } else {
        DejitterConfig::disabled()
    };
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
            run_emitter(thread_id, socket, dest, anchor, releaser, dejitter, stats_for_thread, cancel, rx);
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
        Ok(()) => {
            // The ETF qdisc lives on one mqprio traffic class (TC0 under the
            // standard `0 0 0 0 1 1 1 1 …` prio_tc_map). A DSCP marking on the
            // output socket derives a non-zero sk_priority that routes the
            // packet OFF that class, so SO_TXTIME is silently ignored and the
            // wire runs unpaced. Pin the priority back onto the ETF class so
            // SO_TXTIME actually takes effect. Default 0 (TC0 under the
            // standard map); override via BILBYCAST_ETF_SO_PRIORITY for
            // non-default qdisc layouts.
            let etf_prio = std::env::var("BILBYCAST_ETF_SO_PRIORITY")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            match crate::engine::wire_emit_txtime::set_so_priority(socket, etf_prio) {
                Ok(()) => tracing::info!(
                    "wire-emit: SO_PRIORITY set to {} so SO_TXTIME packets land on the ETF traffic class (overrides DSCP-derived priority)",
                    etf_prio
                ),
                Err(e) => tracing::warn!(
                    "wire-emit: failed to set SO_PRIORITY={} ({}); a DSCP marking may route packets off the ETF class and silently disable SO_TXTIME pacing",
                    etf_prio, e
                ),
            }
            true
        }
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

        // Between PCRs: linearly interpolate each non-PCR datagram's target
        // across the interval implied by the most recently completed PCR
        // window, so datagrams spread evenly instead of all clumping at
        // `wall_anchor` and bursting out in one sendmmsg per PCR interval.
        // The clumping showed up at the receiver as PCR-spacing-equivalent
        // jitter (40–60 ms at 50 fps) — fine for a pro T-STD jitter buffer,
        // but it breaks consumer receivers (VLC) and, when the kernel ETF
        // qdisc is bypassed, hits the wire as a burst.
        //
        // `target = wall_anchor + (bytes_since_anchor / prev_interval_bytes)
        //           × prev_interval_ns`, capped at one interval so a longer
        // current (VBR) window can't push a datagram past where the next PCR
        // re-anchors (the next PCR corrects the baseline regardless). Cold
        // start (no completed interval yet) falls back to emit-at-anchor.
        self.bytes_since_anchor = self.bytes_since_anchor.saturating_add(datagram_bytes as u64);
        if self.prev_interval_bytes > 0 && self.prev_interval_ns > 0 {
            let frac_ns = (self
                .bytes_since_anchor
                .saturating_mul(self.prev_interval_ns)
                / self.prev_interval_bytes)
                .min(self.prev_interval_ns);
            self.wall_anchor_ns.saturating_add(frac_ns).max(now_ns)
        } else {
            self.wall_anchor_ns.max(now_ns)
        }
    }

    /// Pacing dispatch. Compressed/`Lossless` outputs (`cfg.enabled`) use
    /// the closed-loop [`Self::derive_target_servo`]; ST 2110 / everything
    /// else keeps the open-loop PCR-delta [`Self::derive_target`] **byte for
    /// byte unchanged** (its strict raster + SO_TXTIME pacing is the clock).
    fn derive_target_paced(
        &mut self,
        now_ns: u64,
        datagram_pcr: Option<u64>,
        datagram_bytes: usize,
        depth: usize,
        cfg: &DejitterConfig,
    ) -> u64 {
        // Compressed (`Lossless`) outputs run the closed-loop release servo: a
        // smooth UNIFORM-RATE leaky bucket holding buffer residence near
        // `setpoint_ms`, bounding egress latency. Uniform-rate draining avoids
        // the batch CLUMPING that a PCR-delta-baseline drain trim induces —
        // clumping clusters PCR-bearing datagrams into one sendmmsg, spiking
        // PCR_OJ to tens of ms and breaking consumer receivers (see the
        // `derive_target_raw` between-PCR interpolation note). ST 2110 /
        // `disabled` keeps open-loop PCR-delta pacing — the raster anchor owns
        // its clock and never reaches the servo.
        if cfg.enabled {
            self.derive_target_servo(now_ns, datagram_pcr, datagram_bytes, depth, cfg)
        } else {
            self.derive_target(now_ns, datagram_pcr, datagram_bytes)
        }
    }

    /// Closed-loop release-rate servo for compressed (`Lossless`) outputs.
    ///
    /// The open-loop [`Self::derive_target_raw`] paces by **source-PCR
    /// deltas** against a startup wall anchor, with no term comparing the
    /// internal buffer fill or the wallclock drain rate — so any steady
    /// source-rate-vs-`CLOCK_TAI` offset (or burst) integrates into the wire
    /// queue with nothing correcting it (the 47 s latency runaway). This
    /// servo replaces that integration with a **leaky-bucket release** whose
    /// rate is the recovered source rate (`observed_rate_bps`) trimmed
    /// ±`authority` by the buffer-fill error. Holding the buffer at
    /// `setpoint_ms` absorbs a steady offset as a steady rate trim with **no
    /// accumulation and no loss** — there is no integrator for the runaway.
    /// The hard residence-cap shed (in `run_emitter`) stays as the safety net
    /// for bursts beyond the servo's ±authority.
    ///
    /// PCR is used here ONLY to *measure* the nominal rate; pacing is by
    /// byte/rate, not by PCR delta. The PCR field in the bytes is never
    /// rewritten — the receiver re-clocks from it exactly as before.
    ///
    /// OPT-IN only (egress_buffer_ms set). NOTE: a Lossless output CAN take the
    /// SO_TXTIME / etf path now, and the servo + SO_TXTIME can co-run — so this
    /// path applies its OWN `MAX_FUTURE_LOOKAHEAD_NS` cap on the computed target
    /// (mirroring `derive_target`) to bound the launch time handed to the etf
    /// qdisc; without it a source overrun would walk the kernel launch time
    /// arbitrarily far into the future.
    fn derive_target_servo(
        &mut self,
        now_ns: u64,
        datagram_pcr: Option<u64>,
        datagram_bytes: usize,
        depth: usize,
        cfg: &DejitterConfig,
    ) -> u64 {
        // 1. Recover the nominal source rate from inter-PCR observations
        //    (same EMA as the raw path). On a source discontinuity (input
        //    switch / loop seam) re-anchor the rate MEASUREMENT but KEEP
        //    `observed_rate_bps` AND the wallclock release cursor — the leaky
        //    bucket is paced by wallclock, not by source PCR, so a source-PCR
        //    jump must NOT collapse the cushion (the receiver re-clocks from
        //    the new PCR across the discontinuity exactly as before). Earlier
        //    this set `force_asap` → reset the cursor to `now`, which dumped
        //    the de-jitter cushion at every loop seam (~every 64 s) and left
        //    the steady state tracking the bursty arrival.
        let mut discontinuity = false;
        if let Some(pcr) = datagram_pcr {
            match self.pcr_anchor {
                Some(prev) => {
                    let delta_27 = pcr.wrapping_sub(prev) as i64;
                    if (0..=PCR_DISCONTINUITY_27MHZ).contains(&delta_27) {
                        let delta_ns = (delta_27 as u64) * 1000 / 27;
                        if delta_ns >= 1_000_000 && self.bytes_since_anchor > 0 {
                            let observed_bps = self
                                .bytes_since_anchor
                                .saturating_mul(8 * 1_000_000_000)
                                / delta_ns;
                            self.observed_rate_bps = if self.observed_rate_bps == 0 {
                                observed_bps
                            } else {
                                (3 * self.observed_rate_bps + observed_bps) / 4
                            };
                        }
                    } else {
                        // Source discontinuity (loop seam / input switch): skip
                        // the rate EMA (don't measure a rate across the jump).
                        discontinuity = true;
                    }
                    self.pcr_anchor = Some(pcr);
                    self.bytes_since_anchor = 0;
                }
                None => {
                    self.pcr_anchor = Some(pcr);
                }
            }
        }
        self.bytes_since_anchor = self.bytes_since_anchor.saturating_add(datagram_bytes as u64);

        let nominal = self.observed_rate_bps;
        let cushion_ns = (cfg.setpoint_ms.max(1) as u64).saturating_mul(1_000_000);

        if cfg.seed_cushion {
            // ── De-jitter mode (OPT-IN via egress_buffer_ms) ──────────────
            // 2a. Cold start → BUILD the cushion: hold the first datagram one
            //     setpoint into the future so arrival jitter is absorbed. The
            //     legacy path (below) seeds at `now`, so no cushion forms and
            //     the output tracks the bursty arrival — the de-jitter gap.
            //     The kernel ETF qdisc builds this same cushion via its PREROLL
            //     wall-anchor; this is the userspace twin.
            if !self.initialised {
                self.initialised = true;
                self.last_returned_ns = now_ns.saturating_add(cushion_ns);
                return self.last_returned_ns;
            }
            // 2b. No rate yet, OR a source-PCR discontinuity: PRESERVE the
            //     cursor (do NOT collapse to `now`, which dumped the cushion at
            //     every loop seam). The wallclock leaky bucket is immune to a
            //     source-PCR jump; the receiver re-clocks from the new PCR.
            //     `.max(now_ns)` guards a genuine underrun. A discontinuity with
            //     a known rate falls through to the leaky bucket from the
            //     preserved cursor.
            if nominal == 0 {
                let target = self.last_returned_ns.max(now_ns);
                self.last_returned_ns = target;
                return target;
            }
        } else {
            // ── Legacy mode (DEFAULT, egress_buffer_ms unset): UNCHANGED ──
            // Emit ASAP on cold start, source discontinuity, or before a rate
            // is recovered, seeding the cursor at `now`. No cushion, no added
            // latency — byte-for-byte the pre-fix behaviour, so the default
            // deploy carries zero regression risk.
            if discontinuity || !self.initialised || nominal == 0 {
                self.initialised = true;
                self.last_returned_ns = now_ns;
                return now_ns;
            }
        }

        // 4. Buffer-level trim (the feedback the open-loop pacer lacked).
        //    Time domain so it auto-adapts to datagram size + bitrate:
        //    fill_ms = how many ms of content sit in the queue behind this
        //    datagram. err is ‰ of setpoint, clamped ±1000 (±1.0).
        let fill_ms = (depth as u64)
            .saturating_mul(datagram_bytes as u64)
            .saturating_mul(8_000)
            / nominal.max(1);
        let setpoint_ms = cfg.setpoint_ms.max(1);
        let err_permille = (((fill_ms as i64 - setpoint_ms as i64) * 1000)
            / setpoint_ms as i64)
            .clamp(-1000, 1000);
        // release = nominal · (1 + authority·err). authority_permille=50 → ±5%.
        let factor_permille = 1000 + (cfg.authority_permille as i64 * err_permille) / 1000;
        let release_bps = nominal.saturating_mul(factor_permille.max(1) as u64) / 1000;

        // 5. Leaky bucket: schedule this datagram `interval` after the last,
        //    where interval = wire-time of these bytes at the trimmed rate.
        let interval_ns = (datagram_bytes as u64).saturating_mul(8_000_000_000)
            / release_bps.max(1);
        let target = self
            .last_returned_ns
            .saturating_add(interval_ns)
            .max(now_ns) // drained buffer (target in the past) → emit ASAP
            // Bound the lookahead like `derive_target` does: the servo can
            // co-run with SO_TXTIME, so an unbounded target would walk the etf
            // qdisc launch time arbitrarily far into the future on a source
            // overrun. The residence-cap shed is keyed to arrival time and
            // would NOT catch a future-targeted datagram — this is the cap that
            // does. Caps servo latency to the same 200 ms as the legacy path.
            .min(now_ns.saturating_add(MAX_FUTURE_LOOKAHEAD_NS));
        self.last_returned_ns = target;
        target
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

/// Shed the stale wire-queue backlog after the residence cap was exceeded.
///
/// The caller has already pulled the over-residence anchor datagram (counted
/// as the first shed) and decided to drop it. This drains the channel down to
/// `floor` datagrams — dropping the stale backlog that has piled up behind the
/// anchor — and re-anchors the pacing state so the next datagram restarts ASAP
/// rather than chasing the stale future target.
///
/// Terminates: every `try_recv` decrements `rx.depth` (saturating), so the
/// loop exits when depth reaches `floor` or the channel empties (`Err`).
/// Returns the total number of datagrams shed (≥ 1, the anchor included).
///
/// Re-anchor semantics mirror the PCR-discontinuity reset in
/// `derive_target_raw`: clearing `initialised` routes the next datagram through
/// the cold-start path, and resetting `last_returned_ns` stops the outer
/// monotonic guard from dragging that fresh anchor back to the stale future
/// target. `anchor_pid` (same stream) and `observed_rate_bps` (the recovered
/// rate the Phase-2 servo reuses) are intentionally preserved.
fn shed_stale_backlog(
    state: &mut TargetState,
    rx: &WireTxReceiver,
    floor: usize,
    now_ns: u64,
) -> u64 {
    let mut shed: u64 = 1; // the anchor datagram the caller already pulled
    while rx.depth.load(Ordering::Relaxed) > floor {
        match rx.try_recv() {
            Ok(_) => shed += 1,
            Err(_) => break,
        }
    }
    state.initialised = false;
    state.pcr_anchor = None;
    state.bytes_since_anchor = 0;
    state.last_returned_ns = now_ns;
    shed
}

fn run_emitter(
    id: String,
    socket: UdpSocket,
    dest: SocketAddr,
    anchor: AnchorSource,
    releaser: Releaser,
    dejitter: DejitterConfig,
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
                    AnchorSource::Pcr => state.derive_target_paced(
                        now_ns,
                        pcr_27mhz,
                        dg.bytes.len(),
                        rx.depth.load(Ordering::Relaxed),
                        &dejitter,
                    ),
                    AnchorSource::St2110Raster => dg.target_tx_time_ns.unwrap_or(now_ns),
                };
                (dg, target_ns, pcr_27mhz)
            }
        };
        let now_ns = monotonic_now_ns();

        // ── Egress de-jitter: hard residence cap (compressed only) ───────
        //
        // The pacer (`derive_target_raw`) is open-loop on source-PCR deltas
        // against a startup wall anchor — it has NO term comparing the
        // internal wire queue's fill or the wallclock drain rate. So a
        // sub-% source-rate-vs-CLOCK_TAI offset, or a burst, integrates into
        // this thread's input channel with nothing correcting it: the
        // diagnosed 47 s output-latency runaway. (etf masked it as late-drop
        // stutter before the WirePacingClass fix; clock_nanosleep buffers it
        // unbounded.)
        //
        // The safety net: if the anchor datagram has waited longer than the
        // residence cap, the queue has backed up past what any receiver
        // buffer can use — shed this datagram and drain the stale backlog
        // behind it down to the de-jitter floor, then re-anchor pacing at
        // "now". Bounds end-to-end latency by construction (the runaway
        // becomes physically impossible). The PCR field is never touched, so
        // the downstream IRD / SRT-TSBPD / RIST receiver re-clocks across the
        // discontinuity from its own T-STD buffer exactly as it would across
        // any upstream packet loss. Lossless-only (`dejitter.enabled`); ST
        // 2110 keeps strict pacing untouched. The richer release-rate servo
        // (Phase 2) holds occupancy far below this cap so it rarely fires;
        // this is the floor that makes the failure mode bounded regardless.
        // Residence uses the SAME clock on both ends (`util::time::now_us`,
        // the Instant epoch that stamped `recv_time_us`) — NOT the CLOCK_TAI
        // `now_ns` above, which is a different time base.
        if dejitter.enabled && dg.recv_time_us > 0 {
            let residence_us = crate::util::time::now_us().saturating_sub(dg.recv_time_us);
            if residence_us > dejitter.shed_residence_us() {
                let shed =
                    shed_stale_backlog(&mut state, &rx, dejitter.drain_floor_dgms, now_ns);
                stats.egress_shed.fetch_add(shed, Ordering::Relaxed);
                tracing::warn!(
                    "wire-emit '{}': egress residence {}ms exceeded {}ms cap — shed {} stale datagram(s) and re-anchored; receiver re-clocks from PCR",
                    id,
                    residence_us / 1000,
                    dejitter.shed_residence_us() / 1000,
                    shed,
                );
                continue;
            }
        }

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
                        let next_target = state.derive_target_paced(
                            now_ns,
                            next_pcr_27mhz,
                            next_dg.bytes.len(),
                            rx.depth.load(Ordering::Relaxed),
                            &dejitter,
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
                    //
                    // Floor the launch time to `now + ETF_LATE_FLOOR_NS` so a
                    // target that has fallen at/behind real time (late-rebase,
                    // discontinuity, bursty catch-up) is emitted ~1 ms out
                    // rather than DROPPED by the etf qdisc as "too late". The
                    // floor is monotonic with the derived target, so ordering
                    // is preserved; on-time targets are unaffected.
                    let floor = monotonic_now_ns().saturating_add(ETF_LATE_FLOOR_NS);
                    crate::engine::wire_emit_txtime::send_with_txtime(
                        &socket, dest, &dg.bytes, target_ns.max(floor),
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
    fn between_pcrs_interpolate_across_interval() {
        let mut s = TargetState::default();
        // First datagram, PCR=0. wall_anchor = preroll into the future.
        let _ = s.derive_target(0, Some(0), 1316);
        // Second PCR 40 ms later with 50 000 bytes accumulated in between
        // → completes one interval, so prev_interval_{bytes,ns} are seeded.
        s.bytes_since_anchor = 50_000;
        let _ = s.derive_target(0, Some(40 * TICK_PER_MS), 1316);
        assert_eq!(s.observed_rate_bps, 10_000_000); // 50 KB / 40 ms = 10 Mbps (telemetry only)
        assert_eq!(s.prev_interval_bytes, 50_000);
        assert_eq!(s.prev_interval_ns, 40 * 1_000_000);
        let anchor = s.wall_anchor_ns;

        // Non-PCR datagrams now LINEARLY INTERPOLATE across the completed
        // interval instead of all clumping at `wall_anchor` and bursting out
        // together. target = wall_anchor + bytes_since_anchor/prev_interval_bytes × prev_interval_ns.
        // First (1316 bytes): 1316/50000 × 40 ms = 1.0528 ms past the anchor.
        let t1 = s.derive_target(anchor + 1, None, 1316);
        assert_eq!(s.bytes_since_anchor, 1316);
        assert_eq!(t1, anchor + 1316 * 40_000_000 / 50_000);

        // Second (cumulative 2632 bytes): 2632/50000 × 40 ms = 2.1056 ms.
        let t2 = s.derive_target(anchor + 100_000, None, 1316);
        assert_eq!(s.bytes_since_anchor, 2632);
        assert_eq!(t2, anchor + 2632 * 40_000_000 / 50_000);

        // The whole point of the fix: consecutive non-PCR datagrams get
        // strictly-increasing, spread-out targets — NOT the same clumped
        // `wall_anchor` value they had before.
        assert!(t2 > t1, "between-PCR datagrams must spread across the interval, not clump");
        assert!(t1 > anchor, "first non-PCR datagram must be paced past the anchor, not emitted at it");
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
        // Now an interval is complete, so non-PCR datagrams interpolate
        // across it (spread) instead of clumping at wall_anchor:
        // wall_anchor + 1316/50000 × 40 ms.
        let wa = s.wall_anchor_ns;
        let post = s.derive_target(now + 1, None, 1316);
        assert_eq!(post, wa + 1316 * 40_000_000 / 50_000);
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
            WirePacingClass::Lossless,
            None,
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
            WirePacingClass::EtfEligible,
            None,
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

    // ── Egress de-jitter: residence-cap shed (Phase 1) ──────────────────

    /// Build a connected wire channel pre-filled with `n` datagrams so the
    /// shared depth counter reads `n` — mirrors the real spawn-time wiring.
    fn filled_wire_channel(n: usize) -> (WireTxHandle, WireTxReceiver) {
        let (tx_raw, rx_raw) = sync_channel::<WireDatagram>(WIRE_CHANNEL_CAP);
        let depth = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tx = WireTxHandle {
            tx: tx_raw,
            depth: Arc::clone(&depth),
        };
        let rx = WireTxReceiver {
            rx: rx_raw,
            depth,
        };
        for _ in 0..n {
            tx.try_send(WireDatagram {
                bytes: Bytes::from_static(&[0u8; 188]),
                recv_time_us: 0,
                target_tx_time_ns: None,
                ts_offset: 0,
            })
            .expect("channel has capacity for the test fill");
        }
        assert_eq!(tx.depth.load(Ordering::Relaxed), n);
        (tx, rx)
    }

    #[test]
    fn shed_drains_backlog_down_to_floor_and_reanchors() {
        // 100 datagrams queued, drain floor 32: the caller already pulled the
        // anchor (counts as 1), then we drop 100 → 32 = 68 more = 69 total,
        // leaving exactly `floor` in the channel.
        let (tx, rx) = filled_wire_channel(100);
        let mut state = TargetState::default();
        // Simulate a runaway: pacing anchored far in the future.
        state.initialised = true;
        state.pcr_anchor = Some(123_456);
        state.bytes_since_anchor = 9_999;
        state.last_returned_ns = 50_000_000_000;
        let now_ns = 1_000_000_000;

        let shed = shed_stale_backlog(&mut state, &rx, 32, now_ns);

        assert_eq!(shed, 69, "1 anchor + (100-32) drained");
        assert_eq!(
            tx.depth.load(Ordering::Relaxed),
            32,
            "channel drained to the floor, not emptied"
        );
        // Re-anchored exactly like a PCR discontinuity reset.
        assert!(!state.initialised, "next datagram takes the cold-start path");
        assert_eq!(state.pcr_anchor, None);
        assert_eq!(state.bytes_since_anchor, 0);
        assert_eq!(
            state.last_returned_ns, now_ns,
            "last_returned reset so the monotonic guard can't drag the fresh \
             anchor back to the stale future target"
        );
    }

    #[test]
    fn shed_with_floor_zero_empties_channel() {
        let (tx, rx) = filled_wire_channel(50);
        let mut state = TargetState::default();
        let shed = shed_stale_backlog(&mut state, &rx, 0, 7);
        assert_eq!(shed, 51, "1 anchor + all 50 drained");
        assert_eq!(tx.depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shed_below_floor_only_drops_the_anchor() {
        // Depth already at/under the floor: nothing to drain, only the anchor
        // datagram (already pulled by the caller) is shed; the buffer the
        // receiver still needs is left intact.
        let (tx, rx) = filled_wire_channel(10);
        let mut state = TargetState::default();
        let shed = shed_stale_backlog(&mut state, &rx, 32, 7);
        assert_eq!(shed, 1, "floor not reached → only the anchor is shed");
        assert_eq!(tx.depth.load(Ordering::Relaxed), 10, "backlog untouched");
        assert!(!state.initialised, "still re-anchors even with no drain");
    }

    #[test]
    fn dejitter_policy_matches_pacing_class() {
        // The spawn-time derivation: compressed gets the cap, ST 2110 doesn't.
        assert!(DejitterConfig::servo().enabled);
        assert!(!DejitterConfig::disabled().enabled);
        assert!(!DejitterConfig::default().enabled);
        assert_eq!(DejitterConfig::servo().shed_residence_us(), 250_000);
    }

    // ── Egress de-jitter: release-rate servo (Phase 2) ──────────────────

    /// A warmed servo state: rate already recovered, leaky-bucket cursor at
    /// `last_returned_ns`. Mirrors steady-state after a few inter-PCR pairs.
    fn warmed_servo_state(rate_bps: u64, last_returned_ns: u64) -> TargetState {
        let mut s = TargetState::default();
        s.observed_rate_bps = rate_bps;
        s.initialised = true;
        s.pcr_anchor = Some(1_000_000);
        s.last_returned_ns = last_returned_ns;
        s
    }

    #[test]
    fn servo_pacing_responds_to_buffer_fill() {
        // Over setpoint → release faster (shorter interval); under setpoint →
        // slower. This IS the feedback the open-loop pacer lacked.
        let cfg = DejitterConfig::servo();
        let bytes = 1316;
        let now = 1_000_000_000;
        let mut mid = warmed_servo_state(6_000_000, now);
        let int_mid = mid.derive_target_servo(now, None, bytes, 34, &cfg) - now;
        let mut hi = warmed_servo_state(6_000_000, now);
        let int_hi = hi.derive_target_servo(now, None, bytes, 340, &cfg) - now;
        let mut lo = warmed_servo_state(6_000_000, now);
        let int_lo = lo.derive_target_servo(now, None, bytes, 5, &cfg) - now;
        assert!(int_hi < int_mid, "buffer over setpoint must drain faster");
        assert!(int_lo > int_mid, "buffer under setpoint must slow down");
    }

    #[test]
    fn servo_rate_authority_is_bounded() {
        // No matter how full or empty, the trim stays within ±authority
        // (default ±5 %) — the induced PCR_OJ must stay inside the receiver
        // T-STD. A bounded trim is what makes the runaway impossible:
        // sustained overflow can't push release past +5 %, so a burst beyond
        // that is handled by the residence-cap shed, not by unbounded speedup.
        let cfg = DejitterConfig::servo();
        let bytes = 1316;
        // Pathologically full → release capped at nominal · 1.05.
        let mut full = warmed_servo_state(6_000_000, 0);
        let t_full = full.derive_target_servo(0, None, bytes, 1_000_000, &cfg);
        let min_interval = bytes as u64 * 8_000_000_000 / (6_000_000 * 1050 / 1000);
        assert_eq!(t_full, min_interval, "+authority bound (fastest release)");
        // Empty → release floored at nominal · 0.95.
        let mut empty = warmed_servo_state(6_000_000, 2_000_000_000);
        let t_empty = empty.derive_target_servo(2_000_000_000, None, bytes, 0, &cfg);
        let max_interval = bytes as u64 * 8_000_000_000 / (6_000_000 * 950 / 1000);
        assert_eq!(
            t_empty - 2_000_000_000,
            max_interval,
            "-authority bound (slowest release)"
        );
    }

    #[test]
    fn servo_holds_nominal_rate_at_setpoint_no_drift() {
        // The crux: with the buffer held at setpoint, the servo paces at the
        // recovered nominal rate with NO accumulating drift — a steady source
        // offset is absorbed by the trim, not integrated into latency. 100
        // datagrams at depth≈setpoint must land within 1 % of the ideal span.
        let cfg = DejitterConfig::servo();
        let bytes = 1316u64;
        let nominal = 6_000_000u64;
        let mut s = warmed_servo_state(nominal, 0);
        let mut last = 0u64;
        for _ in 0..100 {
            // now=0 so the underflow floor never clamps (targets grow freely).
            last = s.derive_target_servo(0, None, bytes as usize, 34, &cfg);
        }
        let ideal = 100 * bytes * 8_000_000_000 / nominal;
        let diff = (last as i64 - ideal as i64).abs();
        assert!(
            diff < ideal as i64 / 100,
            "paced span {last} drifted from ideal {ideal} by {diff} (> 1%)"
        );
    }

    #[test]
    fn servo_cold_start_seeds_cushion_and_drain_floors() {
        let cfg = DejitterConfig::servo_with(Some(60)); // de-jitter mode, 60 ms
        let cushion_ns = 60_000_000u64;
        // Cold start → BUILD the cushion: hold the first datagram one setpoint
        // into the future so a bursty source's arrival jitter is absorbed.
        // (The old code seeded at `now`, so no cushion ever formed and the
        // output tracked the bursty arrival — the de-jitter gap.)
        let mut cold = TargetState::default();
        let t0 = cold.derive_target_servo(5_000, Some(1000), 1316, 0, &cfg);
        assert_eq!(t0, 5_000 + cushion_ns, "first datagram held one cushion");
        assert_eq!(cold.last_returned_ns, 5_000 + cushion_ns);
        assert!(cold.initialised);
        assert_eq!(cold.pcr_anchor, Some(1000));
        // Genuine underrun (fell a full 4 s behind the cursor) → floor to now,
        // never the past.
        let mut behind = warmed_servo_state(6_000_000, 1_000_000_000);
        let t1 = behind.derive_target_servo(5_000_000_000, None, 1316, 0, &cfg);
        assert_eq!(t1, 5_000_000_000, "target floored to now, not the past");
        assert_eq!(behind.last_returned_ns, 5_000_000_000);
    }

    #[test]
    fn servo_discontinuity_preserves_cushion_and_rate() {
        // Input switch / loop seam: source PCR jumps. The wallclock leaky
        // bucket must KEEP its cushion (cursor stays ahead of now) — it must
        // NOT collapse to `now`, which dumped the de-jitter buffer at every
        // loop seam in the old code (the steady-state jitter the user saw).
        // The rate is preserved; only the rate MEASUREMENT re-anchors.
        let cfg = DejitterConfig::servo_with(Some(60));
        let now = 2_000_000_000u64;
        // Steady state: cursor held one cushion (60 ms) ahead of now.
        let mut s = warmed_servo_state(6_000_000, now + 60_000_000);
        let t = s.derive_target_servo(now, Some(500_000), 1316, 60, &cfg);
        assert!(t > now, "cushion preserved across the seam (not collapsed to now)");
        assert!(t >= now + 60_000_000, "target still ~one cushion ahead");
        assert_eq!(s.observed_rate_bps, 6_000_000, "rate preserved across seam");
        assert_eq!(s.pcr_anchor, Some(500_000), "rate measurement re-anchored");
    }

    #[test]
    fn servo_holds_cushion_across_a_burst() {
        // The crux property the old emit-on-arrival servo lacked: a bursty
        // arrival (many datagrams handed at the same `now`) must be metered
        // out into the future cushion paced to the recovered rate, NOT dumped
        // at `now`. That is what turns ~130 ms input jitter into a smooth
        // output.
        let cfg = DejitterConfig::servo_with(Some(60));
        let bytes = 1316usize;
        let now = 1_000_000_000u64;
        let mut s = TargetState::default();
        s.derive_target_servo(now, Some(0), bytes, 0, &cfg); // cold → seed cushion
        // Second PCR 40 ms later recovers a rate.
        s.derive_target_servo(now, Some(40 * 27_000), bytes, 0, &cfg);
        assert!(s.observed_rate_bps > 0, "rate recovered from the 40 ms PCR gap");
        // Hand 20 datagrams in an instantaneous burst (same `now`).
        let mut prev = 0u64;
        let mut all_future = true;
        for _ in 0..20 {
            let t = s.derive_target_servo(now, None, bytes, 10, &cfg);
            if t <= now {
                all_future = false;
            }
            assert!(t >= prev, "targets monotonic");
            prev = t;
        }
        assert!(all_future, "burst metered into the future cushion, not dumped at now");
        assert!(prev > now, "cursor stays ahead of now (cushion held)");
    }

    #[test]
    fn servo_default_no_egress_buffer_is_legacy_emit_asap() {
        // ZERO-REGRESSION GUARANTEE: with egress_buffer_ms UNSET (plain servo()),
        // seed_cushion is false → cold start AND discontinuity emit ASAP at
        // `now`, byte-for-byte the pre-fix behaviour. The de-jitter cushion is
        // strictly opt-in via egress_buffer_ms — a default deploy adds no
        // latency and changes nothing.
        let cfg = DejitterConfig::servo();
        assert!(!cfg.seed_cushion, "default servo must NOT seed a cushion");
        let mut cold = TargetState::default();
        assert_eq!(
            cold.derive_target_servo(5_000, Some(1000), 1316, 0, &cfg),
            5_000,
            "default cold start emits ASAP (no cushion)"
        );
        let mut s = warmed_servo_state(6_000_000, 1_000_000_000);
        assert_eq!(
            s.derive_target_servo(2_000_000_000, Some(500_000), 1316, 50, &cfg),
            2_000_000_000,
            "default discontinuity emits ASAP at now"
        );
        assert_eq!(s.observed_rate_bps, 6_000_000, "rate preserved across seam");
        // Opt-in path flips the flag.
        assert!(DejitterConfig::servo_with(Some(120)).seed_cushion);
    }

    #[test]
    fn servo_target_is_capped_at_max_future_lookahead() {
        // The servo can co-run with SO_TXTIME (Lossless is etf-eligible now),
        // so its target must be bounded like derive_target — else a source
        // overrun walks the kernel launch time arbitrarily far ahead. Cursor
        // parked 10 s in the future → next target must clamp to now+lookahead.
        let cfg = DejitterConfig::servo_with(Some(60));
        let now = 1_000_000_000u64;
        let mut s = warmed_servo_state(6_000_000, now + 10_000_000_000);
        let t = s.derive_target_servo(now, None, 1316, 34, &cfg);
        assert_eq!(
            t,
            now + MAX_FUTURE_LOOKAHEAD_NS,
            "servo target must clamp to now + MAX_FUTURE_LOOKAHEAD_NS"
        );
        assert_eq!(s.last_returned_ns, t, "cursor stored at the capped value");
    }

    #[test]
    fn paced_dispatch_routes_by_enabled_flag() {
        // disabled → byte-identical to the open-loop derive_target (ST 2110
        // path untouched). enabled → the servo (different value).
        let now = 1_000_000_000;
        let mut a = TargetState::default();
        let mut b = TargetState::default();
        let raw = a.derive_target(now, Some(900_000), 1316);
        let disabled =
            b.derive_target_paced(now, Some(900_000), 1316, 0, &DejitterConfig::disabled());
        assert_eq!(raw, disabled, "disabled dispatch must equal derive_target");
        // Both fresh states warmed identically, servo path diverges from raw.
        let mut c = warmed_servo_state(6_000_000, now);
        let servo = c.derive_target_paced(now, None, 1316, 200, &DejitterConfig::servo());
        assert!(servo >= now, "servo target is sane");
    }
}
