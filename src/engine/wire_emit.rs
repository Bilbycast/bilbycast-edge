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
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
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

/// `recv_timeout` polling cadence. Bounds cancel latency.
const RECV_POLL: Duration = Duration::from_millis(50);

/// Drain MSG_ERRQUEUE every Nth packet on the SO_TXTIME path. ~1024
/// packets ≈ 1.7 s at 6 Mbps TS ≈ 4 ms at ST 2110 1080p50. Cheap (one
/// non-blocking `recvmsg` call returning EAGAIN when empty).
const SO_TXTIME_ERRQUEUE_DRAIN_EVERY: u64 = 1024;

/// Channel capacity. 1024 datagrams ≈ 1.7 s of in-flight bytes at
/// 6 Mbps. With closed-loop rate the channel should rarely fill — it
/// was the symptom of the open-loop drift bug, not the root cause.
/// At ST 2110 rates with SO_TXTIME the kernel takes packets at line
/// rate, so userspace queue depth is irrelevant.
pub const WIRE_CHANNEL_CAP: usize = 1024;

// `MIN_BITRATE_BPS` was a clamp on the natural-paced
// `wall + bytes/observed_rate` interpolation. Removed alongside the
// natural pacing itself — between PCRs we now emit ASAP, and PCR-to-PCR
// targets come from the source's PCR clock difference (no rate divisor
// involved). See `derive_target_raw` for the rationale.

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
#[derive(Clone)]
pub struct WireDatagram {
    pub bytes: Bytes,
    pub recv_time_us: u64,
    /// Caller-supplied target wallclock (CLOCK_MONOTONIC ns) for the
    /// `St2110Raster` anchor. Ignored under `Pcr` anchor. `None` is
    /// "emit ASAP" under either anchor.
    pub target_tx_time_ns: Option<u64>,
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
) -> SyncSender<WireDatagram> {
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

    // Operator escape hatch: when `BILBYCAST_FORCE_NANOSLEEP=1` is set, skip
    // the SO_TXTIME probe entirely and fall back to the clock_nanosleep tier.
    // Useful for diagnosing SO_TXTIME / ETF qdisc reordering during input
    // switches (see /home/ms02/.claude/plans/bright-skipping-whisper.md
    // step 1) and as a workaround on kernels where ETF behaves badly under
    // PCR-discontinuity load.
    let force_nanosleep = std::env::var("BILBYCAST_FORCE_NANOSLEEP")
        .map(|v| v == "1")
        .unwrap_or(false);
    let releaser = if !force_nanosleep && try_enable_so_txtime(&socket, clockid) {
        Releaser::SoTxtime
    } else {
        Releaser::ClockNanosleep
    };

    let (tx, rx) = sync_channel::<WireDatagram>(WIRE_CHANNEL_CAP);
    let thread_id = id.clone();
    let stats_for_thread = stats.clone();
    std::thread::Builder::new()
        .name(format!("wire-emit-{}", id))
        .spawn(move || {
            let sched_fifo = apply_realtime_priority(&thread_id);
            let tier = tier_label(releaser, sched_fifo);
            stats_for_thread.set_wire_pacing_tier(tier);
            tracing::info!(
                "wire-emit '{}': starting (anchor={:?}, tier={})",
                thread_id,
                anchor,
                tier
            );
            run_emitter(thread_id, socket, dest, anchor, releaser, stats_for_thread, cancel, rx);
        })
        .expect("wire-emit thread spawn");
    tx
}

/// Best-effort SO_TXTIME enablement. Returns `true` only when the
/// setsockopt actually succeeded — matches the existing
/// `wire_emit_txtime::probe()` semantics but operates on the
/// already-opened output socket.
fn try_enable_so_txtime(socket: &UdpSocket, clockid: i32) -> bool {
    crate::engine::wire_emit_txtime::enable_so_txtime(socket, clockid).is_ok()
}

// ── Target derivation state ─────────────────────────────────────────────

/// PCR-anchored target derivation state. Closed-loop on observed
/// inter-PCR rate; no declared/configured rate parameter.
#[derive(Default)]
struct TargetState {
    /// Most recent PCR sample (27 MHz ticks). `None` until first PCR.
    pcr_anchor: Option<u64>,
    /// Wallclock instant at which the most recent anchor datagram emits
    /// (CLOCK_MONOTONIC ns). Set to `now + PREROLL_NS` on the first
    /// datagram regardless of PCR presence.
    wall_anchor_ns: u64,
    /// Bytes accumulated past `wall_anchor` by datagrams since the most
    /// recent re-anchor. Reset to 0 when we re-anchor on a PCR.
    bytes_since_anchor: u64,
    /// EMA of inter-PCR observed bitrate. Drives between-PCR
    /// interpolation. 0 until the first inter-PCR observation seeds it.
    observed_rate_bps: u64,
    /// Set after the very first datagram so we don't preroll again.
    initialised: bool,
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

        // No PCR between PCRs: emit ASAP (target = max(wall_anchor, now)).
        //
        // The previous closed-loop interpolation (`wall + bytes /
        // observed_rate`) was fragile under upstream packet loss —
        // observed_rate measures bytes that *reached* wire-emit, so once
        // the wire_tx mpsc fills and the producer's try_send drops new
        // datagrams, observed_rate collapses to egress-rate-not-source-
        // rate, the natural-paced target stretches further out, more
        // drops, more collapse. Documented as the "wire pacer collapses
        // to ~0.5 Mbps" regression in
        // .claude-memory/monorepo/project_wire_pacer_switch_regression.md.
        //
        // Inter-PCR cadence is still enforced at PCR boundaries (the
        // PCR-with-prev branch above advances `wall_anchor` by `delta_ns`
        // per PCR — that's the only timing constraint receivers care
        // about for broadcast PCR_AC). Between PCRs, datagrams emit ASAP;
        // the receiver's jitter buffer absorbs the burst exactly the
        // same way it would absorb a paced stream. On the SO_TXTIME path,
        // ETF qdisc still sorts on tx-time and releases at the right
        // instant — between-PCR datagrams all carry the previous PCR's
        // wall-anchor as tx-time, so the kernel queues them as a tight
        // burst at that instant rather than spread over the inter-PCR
        // window. PCR_AC at the receiver is unchanged because PCR-bearing
        // packets still hit the wire at `wall_anchor + delta_pcr`.
        self.bytes_since_anchor = self.bytes_since_anchor.saturating_add(datagram_bytes as u64);
        self.wall_anchor_ns.max(now_ns)
    }

    // `target_with_observed_rate` was removed — the natural-paced
    // interpolation between PCRs (`wall + bytes/observed_rate`) collapsed
    // under upstream packet loss because `observed_rate` measures bytes
    // that *reached* wire-emit, not bytes the source generated. Once the
    // wire_tx mpsc fills and producer try_send drops, observed_rate
    // tracks egress instead of source rate, the natural-paced target
    // stretches further out, more drops, more collapse. Inter-PCR
    // cadence is now enforced only at PCR boundaries (PCR-with-prev
    // branch advances wall_anchor by delta_ns); between PCRs is ASAP.
    // `observed_rate_bps` is still computed for telemetry but does not
    // drive pacing.
}

// ── Emitter main loop ───────────────────────────────────────────────────

fn run_emitter(
    id: String,
    socket: UdpSocket,
    dest: SocketAddr,
    anchor: AnchorSource,
    releaser: Releaser,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    rx: Receiver<WireDatagram>,
) {
    let mut state = TargetState::default();
    let mut packets_since_drain: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            return;
        }
        let dg = match rx.recv_timeout(RECV_POLL) {
            Ok(d) => d,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        let now_ns = monotonic_now_ns();
        let pcr_27mhz = match anchor {
            AnchorSource::Pcr => crate::engine::ts_parse::first_pcr_in_ts_buffer(&dg.bytes),
            AnchorSource::St2110Raster => None,
        };

        let target_ns = match anchor {
            AnchorSource::Pcr => state.derive_target(now_ns, pcr_27mhz, dg.bytes.len()),
            AnchorSource::St2110Raster => dg.target_tx_time_ns.unwrap_or(now_ns),
        };

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
        let send_result = match releaser {
            Releaser::ClockNanosleep => {
                if target_ns > now_ns {
                    sleep_until_monotonic_ns(target_ns);
                }
                socket.send_to(&dg.bytes, dest)
            }
            Releaser::SoTxtime => {
                // Kernel handles the wallclock target via ETF qdisc.
                // No userspace sleep — the thread loops at producer
                // rate with bounded recvmsg + sendmsg cost.
                crate::engine::wire_emit_txtime::send_with_txtime(
                    &socket,
                    dest,
                    &dg.bytes,
                    target_ns,
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
            }
            Err(e) => {
                tracing::warn!("wire-emit '{}' send error: {}", id, e);
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
fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe {
        libc::clock_gettime(libc::CLOCK_TAI, &mut ts);
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64)
}

#[cfg(target_os = "linux")]
fn sleep_until_monotonic_ns(target_ns: u64) {
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

#[cfg(target_os = "linux")]
fn apply_realtime_priority(id: &str) -> bool {
    let mut sp: libc::sched_param = unsafe { std::mem::zeroed() };
    sp.sched_priority = 50;
    let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &sp) };
    if rc == 0 {
        tracing::debug!("wire-emit '{}': SCHED_FIFO priority 50 acquired", id);
        true
    } else {
        tracing::debug!(
            "wire-emit '{}': SCHED_FIFO unavailable (rc={}); running at default priority. \
             Grant CAP_SYS_NICE for lower jitter.",
            id,
            rc
        );
        false
    }
}

#[cfg(not(target_os = "linux"))]
fn monotonic_now_ns() -> u64 {
    use std::time::Instant;
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_nanos() as u64
}

#[cfg(not(target_os = "linux"))]
fn sleep_until_monotonic_ns(target_ns: u64) {
    let now = monotonic_now_ns();
    if target_ns > now {
        std::thread::sleep(Duration::from_nanos(target_ns - now));
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_realtime_priority(_id: &str) -> bool {
    // No realtime path on non-Linux. clock_nanosleep + SCHED_FIFO is
    // Linux-specific in this codebase.
    false
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

        // Non-PCR datagram between PCRs. Now (anchor + 1) is past the
        // anchor — emit ASAP from the perspective of receivers (the
        // anchor is the PCR-bearing pacing instant; between-PCR
        // datagrams ride the previous anchor and burst out).
        let target = s.derive_target(anchor + 1, None, 1316);
        // wall_anchor.max(now). now=anchor+1, so target=anchor+1.
        assert_eq!(target, anchor + 1);
        assert_eq!(s.bytes_since_anchor, 1316);

        // Second non-PCR datagram, slightly later in wallclock. Target
        // should still pin to now (since wall_anchor < now).
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
        // lands. With no observed rate yet, they MUST target the
        // wall_anchor (preserves FIFO order through preroll instead
        // of jumping ahead of the queued first datagram).
        let now = anchor - 1_000_000; // pretend we're inside preroll
        let target = s.derive_target(now, None, 1316);
        assert_eq!(target, anchor, "cold-start non-PCR must target wall_anchor during preroll");
        let target2 = s.derive_target(now, None, 1316);
        assert_eq!(target2, anchor);

        // After two PCRs land, the EMA seeds (telemetry only — the
        // pacer no longer interpolates between PCRs).
        s.bytes_since_anchor = 50_000;
        let _ = s.derive_target(now, Some(40 * TICK_PER_MS), 1316);
        assert!(s.observed_rate_bps > 0);
        // Non-PCR datagrams between PCRs always emit ASAP from
        // wall_anchor.max(now). No interpolation. The PCR-with-prev
        // branch has already advanced wall_anchor for the next PCR.
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
            })
            .unwrap();
        }
        drop(tx);

        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
        let sent = stats.packets_sent.load(Ordering::Relaxed);
        assert_eq!(sent, 3);
    }
}
