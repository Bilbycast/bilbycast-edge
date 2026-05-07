// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-flow A/V sync mux — master-clock-driven PCR pacing helper.
//!
//! The mux's job is to make output PCR generation deterministic against
//! a single per-flow clock so every output of a flow emits identical
//! PCR sequences regardless of internal pipeline depth, and so multiple
//! edges slaved to the same source produce coherent output.
//!
//! The architecture brief considered a heavier per-flow mux task that
//! re-orders frames in PTS order, owns audio buffering, and re-emits
//! onto the existing broadcast channel. That design conflated three
//! concerns:
//!
//! 1. **PTS preservation** — already handled by the `src_pts_queue` in
//!    `ts_audio_replace.rs` and `ts_video_replace.rs`. Output PES PTS
//!    values come from the source.
//! 2. **PCR generation** — was *derived* from PTS (`pts × 300 −
//!    preroll`), which means PCR jitter mirrors the encoder's pipeline
//!    delay. This is what the mux fixes.
//! 3. **Per-output emission timing** — the wire-level pacing of TS
//!    packets onto the socket. Already paced by libsrt / RIST / RTP
//!    senders; not the mux's concern.
//!
//! Per the chosen scope (operator preference: divergent outputs keep
//! their own per-output replacers), the mux is a pacer, not a per-flow
//! producer task. Each replacer holds an [`AvSyncPacer`] and consults
//! it for PCR generation. PTS still flows through `src_pts_queue`
//! unchanged.
//!
//! ## Why master-clock PCR works
//!
//! Output PCR's purpose in the receiver is to clock the demux's STC. If
//! the source is N ms ahead/behind in any individual PES due to encoder
//! reorder, you don't want PCR to mirror that — PCR should advance at a
//! steady rate the receiver can lock onto.
//!
//! Master-clock PCR achieves this:
//!
//! - **SourcePcrPll**: PCR advances at the source's recovered 27 MHz —
//!   end-to-end clock-locked, no buffer underrun / overflow at the
//!   downstream receiver.
//! - **Ptp**: PCR advances at the plant grandmaster's rate — multiple
//!   edges feeding the same downstream chain stay coherent.
//! - **Wallclock fallback**: PCR advances at the local CPU clock, with
//!   a random per-process offset so cross-edge wallclock fallback flows
//!   never claim accidental coherence.
//!
//! In every case, the receiver's STC has the [`PCR_PREROLL_27MHZ`]
//! buffer it needs to absorb network jitter + encoder CPB peak, exactly
//! as ISO/IEC 13818-1 Annex L (T-STD model) requires.

use std::sync::Arc;

use crate::engine::master_clock::{MasterClockHandle, MasterClockKind};

/// PCR pre-roll behind the master clock in 27 MHz ticks (80 ms ×
/// 27 000 000 / 1000). Mirrors `ts_video_replace.rs::PCR_PREROLL_27MHZ`
/// — kept here so non-replacer call sites can use the same anchor.
pub const PCR_PREROLL_27MHZ: u64 = 2_160_000;

/// Lightweight pacer carrying a clone of the flow's master-clock handle.
///
/// Cheap to clone (Arc only). Threaded into `TsVideoReplacer` /
/// `TsAudioReplacer` / output emit code via a setter that defaults to
/// `None` so existing tests + non-mastered code paths keep working
/// without changes.
#[derive(Clone)]
pub struct AvSyncPacer {
    master: MasterClockHandle,
}

impl AvSyncPacer {
    pub fn new(master: MasterClockHandle) -> Self {
        Self { master }
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

/// Pacer-aware PCR generation: prefer the master clock when one is
/// attached, otherwise fall back to the legacy `pts × 300 − preroll`
/// derivation. Caller passes `pts_for_pes` so the legacy path is
/// preserved without forcing every replacer to plumb a pacer.
pub fn pcr_for_emit(pacer: Option<&Arc<AvSyncPacer>>, pts_for_pes_90k: u64) -> u64 {
    match pacer {
        Some(p) => p.pcr_27mhz_for_emit(),
        None => pts_for_pes_90k
            .saturating_mul(300)
            .saturating_sub(PCR_PREROLL_27MHZ),
    }
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
    fn pcr_for_emit_uses_master_when_pacer_set() {
        let p = Some(Arc::new(wallclock_pacer()));
        // pts_for_pes is intentionally tiny — if the pacer was ignored
        // we'd return ~0; with master clock attached we get the
        // wallclock-derived value, much larger.
        let pcr = pcr_for_emit(p.as_ref(), 1);
        assert!(pcr > 1_000_000, "pacer ignored: pcr={pcr}");
    }

    #[test]
    fn pcr_for_emit_legacy_path_uses_pts() {
        let pts: u64 = 90_000; // 1 s in 90 kHz
        let pcr = pcr_for_emit(None, pts);
        // 1 s × 300 = 27_000_000 ticks; minus PCR_PREROLL_27MHZ = 2_160_000.
        assert_eq!(pcr, 27_000_000 - PCR_PREROLL_27MHZ);
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

    #[test]
    fn legacy_pcr_path_saturates_under_preroll() {
        // The legacy path uses saturating_sub which clamps to 0 instead
        // of wrapping — ensures PCR never goes negative on early
        // start-up when source PTS hasn't crossed the pre-roll yet.
        let pcr = pcr_for_emit(None, 0);
        assert_eq!(pcr, 0);
    }
}
