// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Edge-added A/V skew — the lip-sync error THIS EDGE introduces.
//!
//! At a TS tap, true lip-sync is not directly observable: a compliant
//! receiver pairs audio and video by PTS no matter how they are
//! interleaved (that interleave is the separate `stats::av_interleave`
//! metric). What IS exactly knowable is how much the edge's own
//! PTS-touching stages SHIFTED the audio↔video PTS relationship relative
//! to the source — and every such stage already holds that number in its
//! anchor state:
//!
//! - [`engine::ts_pts_rewriter`] uses ONE shared `ClockAnchor` for all
//!   PIDs, so it preserves A/V by construction; its only contribution is
//!   the deliberate operator lipsync trim (audio-only, ±200 ms).
//! - [`engine::ts_audio_replace`] re-encodes audio on a free-running
//!   sample clock; its `(assigned output PTS − source PTS)` delta is the
//!   exact audio-path shift (this is where the historical Sky-Witness
//!   −27 ms/min loop drift lived — with this reporter the dashboard
//!   would have shown it directly).
//! - [`engine::ts_video_replace`] re-stamps encoded frames with PTS
//!   dequeued from its `src_pts_queue`, so its video delta is 0 by
//!   design; it reports anyway so a future regression is visible.
//!
//! Combined: `skew = lipsync_trim + audio_delta − video_delta`, positive
//! ⇒ audio presented LATER than video vs the source's own alignment.
//! When NO stage is active the path is passthrough and skew is 0 by
//! construction (`mode = "passthrough"`).
//!
//! One reporter per INPUT (each input owns its own stage chain; passive
//! switcher inputs keep processing and must not cross-talk). The flow
//! snapshot publishes the ACTIVE input's reporter. Output-level
//! transcode stages get their own per-output reporter.
//!
//! Lock-free: stages store their current delta with relaxed atomics on
//! PES cadence (~40-75 Hz); snapshot reads are wait-free.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crate::stats::models::AvSkewStats;

/// Sentinel "stage active" flags + current per-stage deltas, 90 kHz ticks.
#[derive(Default)]
pub struct AvSkewReporter {
    /// Lipsync trim currently applied by the PTS rewriter (audio PIDs
    /// only). 0 when the rewriter is inactive or trim unset.
    rewriter_trim_90k: AtomicI64,
    rewriter_active: AtomicBool,
    /// Audio transcode stage: (assigned output PTS − source PTS) for the
    /// most recent source PES.
    audio_delta_90k: AtomicI64,
    audio_active: AtomicBool,
    /// Video transcode stage: same definition (0 by design today).
    video_delta_90k: AtomicI64,
    video_active: AtomicBool,
    /// Worst |skew| seen since construction / reset, in 90 kHz ticks.
    worst_abs_90k: AtomicI64,
}

impl AvSkewReporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called by `TsPtsRewriter` when its anchor is established or the
    /// lipsync trim changes. `active == false` on MPTS passthrough latch
    /// or pre-anchor.
    pub fn set_rewriter(&self, trim_90k: i64, active: bool) {
        self.rewriter_trim_90k.store(trim_90k, Ordering::Relaxed);
        self.rewriter_active.store(active, Ordering::Relaxed);
        self.fold_worst();
    }

    /// Called by `TsAudioReplacer` at each source PES: the delta between
    /// the output PTS its content will carry and the source PTS it had.
    pub fn set_audio_delta(&self, delta_90k: i64) {
        self.audio_delta_90k.store(delta_90k, Ordering::Relaxed);
        self.audio_active.store(true, Ordering::Relaxed);
        self.fold_worst();
    }

    /// Called by `TsVideoReplacer` on emit (delta is 0 by design today).
    pub fn set_video_delta(&self, delta_90k: i64) {
        self.video_delta_90k.store(delta_90k, Ordering::Relaxed);
        self.video_active.store(true, Ordering::Relaxed);
        self.fold_worst();
    }

    /// Current combined skew in 90 kHz ticks (positive ⇒ audio late).
    fn skew_90k(&self) -> (i64, bool) {
        let mut skew: i64 = 0;
        let mut measured = false;
        if self.rewriter_active.load(Ordering::Relaxed) {
            skew += self.rewriter_trim_90k.load(Ordering::Relaxed);
            measured = true;
        }
        if self.audio_active.load(Ordering::Relaxed) {
            skew += self.audio_delta_90k.load(Ordering::Relaxed);
            measured = true;
        }
        if self.video_active.load(Ordering::Relaxed) {
            skew -= self.video_delta_90k.load(Ordering::Relaxed);
            measured = true;
        }
        (skew, measured)
    }

    fn fold_worst(&self) {
        let (skew, _) = self.skew_90k();
        self.worst_abs_90k.fetch_max(skew.abs(), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> AvSkewStats {
        let (skew_90k, measured) = self.skew_90k();
        AvSkewStats {
            skew_ms: skew_90k / 90,
            worst_abs_ms: self.worst_abs_90k.load(Ordering::Relaxed) / 90,
            lipsync_trim_ms: if self.rewriter_active.load(Ordering::Relaxed) {
                self.rewriter_trim_90k.load(Ordering::Relaxed) / 90
            } else {
                0
            },
            mode: if measured {
                "measured".to_string()
            } else {
                "passthrough".to_string()
            },
        }
    }

    /// Reset the worst-case tracker (input switch / flow restart).
    pub fn reset_worst(&self) {
        self.worst_abs_90k.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_no_stage_active() {
        let r = AvSkewReporter::new();
        let s = r.snapshot();
        assert_eq!(s.mode, "passthrough");
        assert_eq!(s.skew_ms, 0);
        assert_eq!(s.worst_abs_ms, 0);
        assert_eq!(s.lipsync_trim_ms, 0);
    }

    #[test]
    fn rewriter_trim_only() {
        let r = AvSkewReporter::new();
        r.set_rewriter(900, true); // +10 ms operator trim
        let s = r.snapshot();
        assert_eq!(s.mode, "measured");
        assert_eq!(s.skew_ms, 10);
        assert_eq!(s.lipsync_trim_ms, 10);
        assert_eq!(s.worst_abs_ms, 10);
    }

    #[test]
    fn audio_drift_combines_with_trim_and_tracks_worst() {
        let r = AvSkewReporter::new();
        r.set_rewriter(0, true);
        r.set_audio_delta(-6885); // the historical -76.5 ms loop step
        let s = r.snapshot();
        assert_eq!(s.mode, "measured");
        assert_eq!(s.skew_ms, -76);
        assert_eq!(s.worst_abs_ms, 76);
        // Drift recovers — worst stays latched.
        r.set_audio_delta(0);
        let s = r.snapshot();
        assert_eq!(s.skew_ms, 0);
        assert_eq!(s.worst_abs_ms, 76);
        r.reset_worst();
        assert_eq!(r.snapshot().worst_abs_ms, 0);
    }

    #[test]
    fn video_delta_subtracts() {
        let r = AvSkewReporter::new();
        r.set_audio_delta(900); // audio +10 ms
        r.set_video_delta(900); // video +10 ms — both shifted equally
        let s = r.snapshot();
        assert_eq!(s.skew_ms, 0, "equal shifts on both paths cancel");
        assert_eq!(s.mode, "measured");
    }

    #[test]
    fn mpts_latch_deactivates_rewriter_contribution() {
        let r = AvSkewReporter::new();
        r.set_rewriter(900, true);
        assert_eq!(r.snapshot().skew_ms, 10);
        r.set_rewriter(900, false); // MPTS passthrough latch
        let s = r.snapshot();
        assert_eq!(s.mode, "passthrough");
        assert_eq!(s.skew_ms, 0);
        assert_eq!(s.lipsync_trim_ms, 0);
    }
}
