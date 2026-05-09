// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! ST 2110-21 raster pacer for uncompressed video output.
//!
//! Computes per-packet `target_tx_time_ns` in `CLOCK_TAI` ns for an
//! ST 2110-20 / -22 sender. Hands its outputs to
//! [`engine::wire_emit_txtime::send_batch_with_txtime`] so the kernel
//! / NIC schedules transmission against the operator-disciplined PTP
//! clock.
//!
//! # Profile
//!
//! ST 2110-21 defines three sender profiles:
//!
//! | Profile         | Pacing                                                                                     |
//! |-----------------|--------------------------------------------------------------------------------------------|
//! | Narrow (gapped) | Burst packets across each active video line, idle during HBI/VBI. Tightest VRX bounds.     |
//! | Narrow Linear   | Even pacing across the full frame period. Slightly larger VRX, but uniform inter-packet.   |
//! | Wide            | Relaxed Cmax / VRX. Bursts up to a single line of packets allowed.                         |
//!
//! **v1 ships Narrow Linear for every profile selection** — even pacing
//! across the frame period (`frame_period_ns / packets_per_frame`).
//! This is correct for receivers tolerant of narrow_linear (most modern
//! ST 2110 gear: Imagine, Lawo, EVS Xeebra, GV LDX) and an order of
//! magnitude better than the today's "send as fast as possible" loop in
//! `engine::st2110_video_io`. A future revision adds gapped narrow when
//! a real receiver requires it; the wire format and the
//! `target_for_packet` contract don't change.
//!
//! # PTP anchoring
//!
//! `frame_epoch_tai_ns` is the `CLOCK_TAI` time at which frame 0 starts.
//! Subsequent frames advance by `frame_period_ns`. On startup the
//! anchor is `clock_gettime(CLOCK_TAI)` rounded up to the next frame
//! boundary. [`anchor_to_ptp`] re-snaps the anchor whenever the
//! system PTP daemon reports a non-trivial step, which prevents the
//! pacer from running ahead/behind the grandmaster across long sessions.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Sender profile per ST 2110-21 §6.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum St2110_21Profile {
    /// Tightest VRX bounds; v1 of this pacer treats this identically to
    /// `NarrowLinear` (even pacing across the frame period). Classic
    /// gapped narrow is a follow-up.
    Narrow,
    /// Even pacing across the full frame period.
    NarrowLinear,
    /// Relaxed bounds. v1 treats this identically to `NarrowLinear`.
    Wide,
}

/// ST 2110-21 raster pacer state.
pub struct St2110_21Pacer {
    /// Inverse refresh rate × 1e9. e.g. 1080p50 → 20_000_000.
    frame_period_ns: u64,
    /// Total RFC 4175 packets per frame (1080p50 4:2:2 10-bit ≈ 4320,
    /// derived once at construction time from raster geometry).
    packets_per_frame: u32,
    /// Reserved for future gapped-narrow expansion. v1 ignores; behaves
    /// like `NarrowLinear` for all variants.
    #[allow(dead_code)]
    profile: St2110_21Profile,

    /// `CLOCK_TAI` ns at which frame 0 starts. Updated by
    /// [`anchor_to_ptp`] whenever PTP reports a fresh sample, and at
    /// construction (monotonic fallback if PTP is not yet available).
    frame_epoch_tai_ns: AtomicU64,
    /// `true` once `anchor_to_ptp` has run successfully at least once.
    /// `false` while we're on the monotonic fallback path.
    ptp_anchored: AtomicBool,
}

impl St2110_21Pacer {
    /// Construct a pacer. `frame_period_ns` and `packets_per_frame` are
    /// derived by the caller from the format (rate_num/rate_den +
    /// `Pgroup::bytes_for_pixels`). The initial anchor is set from
    /// `clock_gettime(CLOCK_TAI)` rounded up to the next frame boundary.
    /// On non-Linux platforms the anchor is monotonic-now without the
    /// CLOCK_TAI snap.
    pub fn new(
        frame_period_ns: u64,
        packets_per_frame: u32,
        profile: St2110_21Profile,
    ) -> Self {
        let packets = packets_per_frame.max(1);
        let period = frame_period_ns.max(1);
        let initial_anchor = align_up(current_tai_ns(), period);
        Self {
            frame_period_ns: period,
            packets_per_frame: packets,
            profile,
            frame_epoch_tai_ns: AtomicU64::new(initial_anchor),
            ptp_anchored: AtomicBool::new(false),
        }
    }

    /// Compute the target tx time for one packet, in `CLOCK_TAI` ns.
    pub fn target_for_packet(&self, frame_index: u64, packet_in_frame: u32) -> u64 {
        let anchor = self.frame_epoch_tai_ns.load(Ordering::Relaxed);
        let frame_start = anchor.saturating_add(frame_index.saturating_mul(self.frame_period_ns));
        // Even pacing across the frame: target = frame_start +
        // packet_in_frame × frame_period / packets_per_frame.
        // Use u128 in the middle to avoid overflow at very high
        // packets_per_frame (e.g. 4K60 ~17k packets/frame).
        let pkt = (packet_in_frame as u128) * (self.frame_period_ns as u128)
            / (self.packets_per_frame as u128);
        frame_start.saturating_add(pkt as u64)
    }

    /// Snap the frame epoch to the next frame boundary at or after
    /// `current_tai_ns`. Idempotent; safe to call from a PTP reporter
    /// task on every successful sample. Sets `ptp_anchored = true`.
    pub fn anchor_to_ptp(&self, current_tai_ns: u64) {
        // Round up to the next frame boundary so the pacer never tries
        // to emit at a target in the past.
        let snapped = align_up(current_tai_ns, self.frame_period_ns);
        self.frame_epoch_tai_ns.store(snapped, Ordering::Relaxed);
        self.ptp_anchored.store(true, Ordering::Relaxed);
    }

    /// `true` once a PTP sample has been used to anchor the frame
    /// epoch. `false` while running on the monotonic fallback path
    /// (operator should be warned at startup).
    pub fn is_ptp_anchored(&self) -> bool {
        self.ptp_anchored.load(Ordering::Relaxed)
    }

    /// Frame period in ns. Useful to expose for downstream
    /// frame-boundary scheduling.
    pub fn frame_period_ns(&self) -> u64 {
        self.frame_period_ns
    }

    /// Packets per frame.
    pub fn packets_per_frame(&self) -> u32 {
        self.packets_per_frame
    }
}

/// Round `value` up to the next multiple of `align`. Saturates on
/// overflow.
fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 {
        return value;
    }
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value.saturating_add(align - rem)
    }
}

/// Read `CLOCK_TAI` on Linux. Falls back to wall time on other
/// platforms (close enough for unit-test seeding; production ST 2110
/// is Linux-only anyway).
#[cfg(target_os = "linux")]
fn current_tai_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_TAI, &mut ts) };
    if rc != 0 {
        // CLOCK_TAI unavailable — fall back to CLOCK_REALTIME. Will be
        // off by the leap-second offset (currently 37 s) but is always
        // forward-progressing and good enough for the monotonic anchor
        // until ptp4l brings the system clock into TAI alignment.
        unsafe {
            libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
        }
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64)
}

#[cfg(not(target_os = "linux"))]
fn current_tai_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 1080p50 at 4320 packets/frame: every packet target lies on a
    /// `(20_000_000 / 4320)` ns boundary past the frame start.
    #[test]
    fn target_for_packet_narrow_1080p50() {
        let p = St2110_21Pacer::new(20_000_000, 4320, St2110_21Profile::Narrow);
        let f0 = p.target_for_packet(0, 0);
        let anchor = p.frame_epoch_tai_ns.load(Ordering::Relaxed);
        assert_eq!(f0, anchor);
        // Packet 1 of frame 0: 20_000_000 / 4320 ≈ 4630 ns.
        let f0_p1 = p.target_for_packet(0, 1);
        assert!(f0_p1 > f0, "packet 1 must be after packet 0");
        let delta = f0_p1 - f0;
        assert!(delta >= 4_000 && delta <= 5_000, "delta={delta}");
        // Packet 4319 (last in frame): nearly the next frame boundary.
        let f0_last = p.target_for_packet(0, 4319);
        let next_frame = p.target_for_packet(1, 0);
        assert!(f0_last < next_frame, "last packet of frame must precede frame 1");
        let gap = next_frame - f0_last;
        // Should be ~ (frame_period / packets) gap.
        assert!(gap >= 4_000 && gap <= 5_000, "gap={gap}");
    }

    /// 4K60 at ~17280 packets/frame: math doesn't overflow at large
    /// frame indices. Computes target for the 1 millionth frame
    /// (~4.6 hours of video).
    #[test]
    fn target_for_packet_4k60_no_overflow() {
        let p = St2110_21Pacer::new(16_666_667, 17_280, St2110_21Profile::Narrow);
        let target = p.target_for_packet(1_000_000, 17_279);
        // No overflow, no panic, monotonic.
        let next_frame = p.target_for_packet(1_000_001, 0);
        assert!(next_frame > target);
    }

    /// PTP anchor snaps the frame epoch and flips `is_ptp_anchored`.
    /// Two calls with monotonic TAI samples must both leave the
    /// anchor on a frame boundary.
    #[test]
    fn ptp_anchor_alignment() {
        let p = St2110_21Pacer::new(20_000_000, 4320, St2110_21Profile::Narrow);
        assert!(!p.is_ptp_anchored());
        let sample_ns = 1_700_000_000_000_000_000u64; // some absolute TAI ns
        p.anchor_to_ptp(sample_ns);
        assert!(p.is_ptp_anchored());
        let anchor = p.frame_epoch_tai_ns.load(Ordering::Relaxed);
        assert_eq!(anchor % 20_000_000, 0, "anchor must be on frame boundary");
        assert!(anchor >= sample_ns, "anchor >= sample (next boundary)");
        // A second call jumps further along; monotonic.
        let sample2 = sample_ns + 5_000_000_000;
        p.anchor_to_ptp(sample2);
        let anchor2 = p.frame_epoch_tai_ns.load(Ordering::Relaxed);
        assert!(anchor2 > anchor);
        assert_eq!(anchor2 % 20_000_000, 0);
    }

    /// Even pacing means consecutive packets within a frame are
    /// equally spaced (within ±1 ns rounding).
    #[test]
    fn even_pacing_consecutive_packets() {
        let p = St2110_21Pacer::new(20_000_000, 4320, St2110_21Profile::NarrowLinear);
        let mut deltas = Vec::new();
        for i in 0..100 {
            let a = p.target_for_packet(0, i);
            let b = p.target_for_packet(0, i + 1);
            deltas.push(b - a);
        }
        let min = *deltas.iter().min().unwrap();
        let max = *deltas.iter().max().unwrap();
        assert!(max - min <= 1, "uneven pacing: min={min} max={max}");
    }

    /// `align_up` rounds correctly and is idempotent on aligned input.
    #[test]
    fn align_up_rounds_correctly() {
        assert_eq!(align_up(0, 1000), 0);
        assert_eq!(align_up(1, 1000), 1000);
        assert_eq!(align_up(999, 1000), 1000);
        assert_eq!(align_up(1000, 1000), 1000);
        assert_eq!(align_up(1001, 1000), 2000);
    }
}
