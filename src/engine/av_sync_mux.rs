// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-flow A/V sync pacer — a thin handle on the flow's master clock.
//!
//! [`AvSyncPacer`] carries a clone of the flow's `MasterClockHandle` into
//! the stages that need the master clock: the ingress `ts_pts_rewriter`
//! (muxer-mode anchor, and the `assembler_owned` hand-off for PID-bus
//! flows) and the PID-bus assembler. The TS transcode replacers take none:
//! they stamp source-relative PTS (the `TsAudioReplacer`'s wallclock
//! catch-up, which measured host load rather than lip-sync, is gone).
//!
//! It does **not** generate PCR. The transcode path used to derive PCR
//! from the re-encoded video PTS (`pcr_for_emit`, `pts × 300 − 80 ms`,
//! later floored on a decaying audio lag); that clock ran ~15 500 ppm fast
//! against the PTS it described and could only appear once per frame. The
//! transcode chains now keep the input's own PCR timeline, delayed by one
//! measured allowance — see [`crate::engine::ts_pcr_remux`]. An even earlier
//! design sampled `master.now_27mhz()` at PES emit time; the sampled value
//! had no fixed relationship to the packet's wire time (stdev 176 ms of PCR
//! jitter on a transcoded UDP loopback), which is why
//! [`AvSyncPacer::pcr_27mhz_for_emit`] has no data-path caller.

use std::sync::Arc;

use crate::engine::master_clock::{MasterClockHandle, MasterClockKind};

/// PCR pre-roll behind the master clock in 27 MHz ticks (80 ms ×
/// 27 000 000 / 1000). Mirrors `ts_pts_rewriter::PCR_PREROLL_27MHZ`.
pub const PCR_PREROLL_27MHZ: u64 = 2_160_000;

/// Lightweight pacer carrying a clone of the flow's master-clock handle.
///
/// Cheap to clone (Arc only). Threaded into the input post-process
/// (`ts_pts_rewriter`) and the assembler as an `Option`, so tests and
/// non-mastered code paths work without it.
///
/// `assembler_owned` is a flow-level signal: when set, the per-input
/// `ts_pts_rewriter` skips itself because the assembler will run its
/// own single-anchor muxer-mode rewriter on the assembled output.
/// Avoids per-input anchors interfering with cross-input PES splice
/// arithmetic in PID-bus / Node-Bus flows.
#[derive(Clone)]
pub struct AvSyncPacer {
    master: MasterClockHandle,
    assembler_owned: Arc<std::sync::atomic::AtomicBool>,
}

impl AvSyncPacer {
    pub fn new(master: MasterClockHandle) -> Self {
        Self {
            master,
            assembler_owned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Mark this pacer as owned by an assembled-flow assembler. Per-input
    /// rewriters check this and skip themselves so the assembler can run
    /// a single shared-anchor rewriter on the assembled output. Idempotent.
    pub fn mark_assembler_owned(&self) {
        self.assembler_owned
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether an assembler has claimed this pacer's anchor responsibility.
    /// `ts_pts_rewriter::TsPtsRewriter` checks this in `InputPostProcess::
    /// from_config` to skip per-input rewriting on inputs that feed an
    /// assembled flow.
    pub fn is_assembler_owned(&self) -> bool {
        self.assembler_owned
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Master clock's tagged kind ("source_pcr_pll" / "ptp" / etc.).
    #[allow(dead_code)]
    pub fn kind(&self) -> MasterClockKind {
        self.master.kind()
    }

    /// True when the underlying master clock has converged. Outputs
    /// that need broadcast-grade timing should gate PCR emission on
    /// this; the wallclock fallback always returns `true`.
    #[allow(dead_code)]
    pub fn is_locked(&self) -> bool {
        self.master.is_locked()
    }

    /// Master clock's now() in 27 MHz ticks. Wraps modulo 2^33 × 300.
    #[allow(dead_code)]
    pub fn now_27mhz(&self) -> u64 {
        self.master.now_27mhz()
    }

    /// PCR value to emit *now*: `master_now − PCR_PREROLL_27MHZ`,
    /// modular-aware. Wraps cleanly on the PCR space without producing
    /// a giant garbage value when `master_now < PCR_PREROLL_27MHZ`
    /// (happens briefly at flow start).
    ///
    /// **Not called from the data path** — see the module docs for why
    /// the master clock is not sampled at PES emit time. Retained for tests
    /// and any future caller whose PTS doesn't track source.
    #[allow(dead_code)]
    pub fn pcr_27mhz_for_emit(&self) -> u64 {
        const PCR_MODULUS: u64 = (1u64 << 33) * 300;
        let now = self.master.now_27mhz();
        if now >= PCR_PREROLL_27MHZ {
            now - PCR_PREROLL_27MHZ
        } else {
            // Pre-roll is larger than master_now → wrap around the
            // modulus. Receivers do this maths in the same modular
            // space.
            (PCR_MODULUS + now) - PCR_PREROLL_27MHZ
        }
    }

    /// Operator-set lipsync trim in 90 kHz ticks.
    #[allow(dead_code)]
    pub fn lipsync_offset_90k(&self) -> i64 {
        self.master.lipsync_offset_90k()
    }
}

impl std::fmt::Debug for AvSyncPacer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AvSyncPacer")
            .field("kind", &self.master.kind())
            .field("locked", &self.master.is_locked())
            .finish()
    }
}

/// Convenience builder for tests + standalone flows that want a
/// wallclock pacer without going through `FlowRuntime::start`.
#[allow(dead_code)]
pub fn wallclock_pacer() -> AvSyncPacer {
    AvSyncPacer::new(MasterClockHandle::wallclock())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::master_clock::{MasterClockKind, SourcePcrPllMaster};

    #[test]
    fn wallclock_pacer_is_always_locked() {
        let p = wallclock_pacer();
        assert!(p.is_locked());
        assert_eq!(p.kind(), MasterClockKind::Wallclock);
    }

    #[test]
    fn pcr_emit_trails_master_by_preroll() {
        let p = wallclock_pacer();
        let now = p.now_27mhz();
        let pcr = p.pcr_27mhz_for_emit();
        // Pre-roll exact at the same call-time would be flaky; just
        // assert the algebraic shape holds within a few ticks of jitter.
        let diff = now.wrapping_sub(pcr);
        // Allow up to ~ 100 µs of skew between the two reads (~ 2700
        // ticks at 27 MHz). diff should be ~ PCR_PREROLL_27MHZ.
        let approx = (diff as i64 - PCR_PREROLL_27MHZ as i64).abs();
        assert!(
            approx < 5_000,
            "pcr offset from now is not preroll-aligned: diff={} preroll={}",
            diff,
            PCR_PREROLL_27MHZ
        );
    }

    #[test]
    fn pcr_for_emit_under_preroll_wraps_modular() {
        // Build a pacer whose master_now is below the pre-roll. Easiest
        // way: a SourcePcrPllMaster pre-sample state advances from a
        // process-local epoch — within the first 80 ms after construction
        // master_now < pre-roll, so the wrap path fires.
        let inner = Arc::new(SourcePcrPllMaster::new("test"));
        let h = MasterClockHandle::new(inner.clone(), MasterClockKind::SourcePcrPll);
        let p = AvSyncPacer::new(h);
        let pcr = p.pcr_27mhz_for_emit();
        // Should produce a sensible-sized number, not u64::MAX.
        assert!(pcr < (1u64 << 33) * 300, "pcr exceeded modulus: {pcr}");
    }
}
