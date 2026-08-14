// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later
//
//! Per-vblank frame scheduling for the local display output (edge#115).
//!
//! # Why this exists
//!
//! The display loop historically computed a wall-clock instant for each frame,
//! slept until it, and committed — leaving the vblank as something it aimed
//! *near*. Source and panel are independent crystals (measured 12–65 ppm apart
//! across this fleet), so that target walks against the scanout raster and
//! periodically lands on a vblank boundary, where sub-millisecond scheduling
//! noise decides which of two adjacent vblanks catches the flip. The panel
//! shows one frame for one vblank and the next for three. Nothing is dropped,
//! so every loss counter reads clean (#112, #104).
//!
//! Rounding that target to the nearest real vblank helps — it took on-target
//! presents from 88.1% to 99.7% on RK3588 and visibly removed the stutter —
//! but it only adjusts *when* a flip is requested. It never decides *which*
//! frame belongs on *which* vblank, so it is inert wherever there is no slack
//! to round within (measured: 100% no-sleep at 50p on a 50 Hz panel, where the
//! code never executes at all).
//!
//! This module is the other half: frames are assigned to vblank *counts*
//! rather than to instants.
//!
//! # The model
//!
//! One accumulator. Each frame is held for a whole number of vblanks, and the
//! fractional remainder carries:
//!
//! ```text
//! accum += vblanks_per_frame        // panel_hz / source_fps
//! hold   = floor(accum)
//! accum -= hold
//! ```
//!
//! At 25p on 50 Hz that emits 2, 2, 2, … At 24p on 60 Hz it emits 2, 3, 2, 3 —
//! correct 2:3 pulldown, *generated* rather than hoped for. Crystal drift lands
//! in `accum` and surfaces as one extra held vblank per beat period, which is
//! the same "absorb, don't correct" principle validated by the rounding
//! prototype, applied where it can actually act.
//!
//! Deliberately **not** a servo. Two servo-shaped attempts on this fault were
//! measured *worse* than doing nothing: a mid-vblank phase-lock (withdrawn,
//! #112) and a `div_ceil` rounding variant that introduced twice-per-beat
//! bursts of late frames. An accumulator has no gain to mistune.

/// How a frame's vblank allocation was decided — reported so a mistimed
/// present can be told apart from a correctly-held one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldKind {
    /// The cadence called for this hold. Normal operation.
    Scheduled,
    /// One extra vblank absorbed because the accumulator crossed an integer —
    /// the crystal-drift beat surfacing. Expected, and periodic at the beat
    /// rate; not an error.
    DriftAbsorb,
}

/// A frame's vblank allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    /// Whole vblanks this frame occupies. Always >= 1: a frame that would be
    /// allotted zero would never be seen, and the source is entitled to have
    /// every frame displayed at least once when the panel is faster than it.
    pub vblanks: u32,
    pub kind: HoldKind,
}

/// Frame-to-vblank scheduler.
///
/// Holds no clock and does no I/O so it can be exercised without a panel — the
/// rounding prototype's arithmetic originally took a `&KmsDisplay` and could
/// not be unit-tested at all, which is how a bug that collapsed the target
/// onto an already-past instant survived review.
#[derive(Debug, Clone)]
pub struct CadenceScheduler {
    vblanks_per_frame: f64,
    /// The hold an *undrifting* clock pair would produce every frame, present
    /// only when the cadence is near-integer.
    ///
    /// `None` for genuinely fractional cadences: at 2.5 the holds alternate 2
    /// and 3 and **neither is drift** — comparing against a single nominal
    /// would label half of a correct 2:3 pulldown as an anomaly. Deliberately
    /// `round()` and not `floor()`: a panel 33 ppm slow gives 1.999934, and
    /// flooring that yields 1, which would misreport every ordinary 2-vblank
    /// hold as drift absorption.
    nominal_hold: Option<u32>,
    accum: f64,
}

impl CadenceScheduler {
    /// `panel_hz` and `source_fps` should both be **measured**, not advertised.
    /// Panels on this fleet sit 12–65 ppm off nominal, and
    /// `ingress_summary.video_fps` is known to go stale across a source rate
    /// change — it reported 50.0 for minutes while the stream was 24 fps.
    ///
    /// Returns `None` for rates that cannot produce a sane cadence, so callers
    /// keep their existing timing path rather than divide by a guess.
    pub fn new(panel_hz: f64, source_fps: f64) -> Option<Self> {
        if !(panel_hz.is_finite() && source_fps.is_finite()) {
            return None;
        }
        if panel_hz <= 0.0 || source_fps <= 0.0 {
            return None;
        }
        let vpf = panel_hz / source_fps;
        // Below 1.0 the source is faster than the panel and frames must be
        // dropped rather than held. That is a different algorithm with
        // different perceptual trade-offs (which frame to discard), and it is
        // not implemented here — 60p on a 50 Hz panel keeps the old path.
        if vpf < 1.0 || vpf > 16.0 {
            return None;
        }
        // "Near-integer" spans real crystal error (tens of ppm) with room to
        // spare, while staying far from a true fractional cadence like 2.5.
        const INTEGER_EPS: f64 = 0.01;
        let rounded = vpf.round();
        let nominal_hold = if (vpf - rounded).abs() < INTEGER_EPS && rounded >= 1.0 {
            Some(rounded as u32)
        } else {
            None
        };
        Some(Self {
            vblanks_per_frame: vpf,
            nominal_hold,
            // Start half a vblank in so the first crossing is not biased by
            // the seed: at an exact integer cadence this keeps every hold on
            // the nominal value indefinitely instead of alternating.
            accum: 0.5,
        })
    }

    /// Vblanks per frame this scheduler was built with.
    pub fn vblanks_per_frame(&self) -> f64 {
        self.vblanks_per_frame
    }

    /// Allocate the next frame.
    pub fn next_hold(&mut self) -> Hold {
        self.accum += self.vblanks_per_frame;
        let mut vblanks = self.accum.floor();
        // Guard the degenerate case rather than trusting the arithmetic: a
        // frame held for zero vblanks is never displayed.
        if vblanks < 1.0 {
            vblanks = 1.0;
        }
        self.accum -= vblanks;
        let vblanks = vblanks as u32;
        let kind = match self.nominal_hold {
            // Only a near-integer cadence has a single "correct" hold, so only
            // there does a departure from it mean the crystal difference has
            // accumulated past a whole vblank.
            Some(nominal) if vblanks != nominal => HoldKind::DriftAbsorb,
            _ => HoldKind::Scheduled,
        };
        Hold { vblanks, kind }
    }

    /// Re-seed after a discontinuity — a source switch, a decoder re-open, or a
    /// PTS jump. Carrying `accum` across one would apply the old stream's
    /// fractional debt to the new one.
    pub fn reset(&mut self) {
        self.accum = 0.5;
    }
}

/// The identity of the rate pair a scheduler was built for.
///
/// The display loop rebuilds its scheduler when this changes, so the
/// quantisation decides what counts as "the same panel". It must be fine
/// enough that a *wrong* measurement is a different key from the right one,
/// and coarse enough that ordinary jitter is not.
///
/// The panel half was originally the advertised integer Hz, which failed the
/// first requirement completely: a measurement of 50.61 Hz and a corrected
/// one of 49.998 Hz both truncate to 50, so the scheduler never rebuilt and
/// kept a cadence of 2.024 vblanks/frame against a true 2.000 — inserting one
/// extra vblank every 41 frames on a panel that needed none.
pub fn rebuild_key(vblank_period_ns: u64, fps_milli: u32) -> (u64, u32) {
    // 10 µs is ~25 ppm at 50 Hz. Crystal error is tens of ppm and measurement
    // jitter over a 600-flip window is well under that; a structurally wrong
    // reading is orders of magnitude larger.
    (vblank_period_ns / 10_000, fps_milli)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holds(panel_hz: f64, fps: f64, n: usize) -> Vec<u32> {
        let mut s = CadenceScheduler::new(panel_hz, fps).expect("valid cadence");
        (0..n).map(|_| s.next_hold().vblanks).collect()
    }

    #[test]
    fn integer_cadence_is_stable_forever() {
        // 25p on 50 Hz must be a flat 2,2,2 — any alternation here is visible
        // as judder on every frame, which is the fault being fixed.
        assert_eq!(holds(50.0, 25.0, 8), vec![2; 8]);
        // 30p on 60 Hz, same shape at a different rate.
        assert_eq!(holds(60.0, 30.0, 8), vec![2; 8]);
        // 1:1 — degenerate but must still emit one vblank per frame.
        assert_eq!(holds(50.0, 50.0, 8), vec![1; 8]);
    }

    #[test]
    fn twenty_four_p_on_sixty_hz_is_two_three_pulldown() {
        // The classic film cadence. 2.5 vblanks/frame must alternate, and must
        // not drift into 2,2,3 or 3,3,2 — an uneven pattern is the judder.
        assert_eq!(holds(60.0, 24.0, 8), vec![3, 2, 3, 2, 3, 2, 3, 2]);
    }

    #[test]
    fn holds_never_reach_zero_even_at_the_boundary() {
        for fps in [24.0, 25.0, 30.0, 50.0, 59.94] {
            for hz in [50.0, 59.94, 60.0] {
                if let Some(mut s) = CadenceScheduler::new(hz, fps) {
                    for _ in 0..500 {
                        assert!(
                            s.next_hold().vblanks >= 1,
                            "hz={hz} fps={fps} produced a zero-vblank hold"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn crystal_drift_is_absorbed_at_the_predicted_beat_rate() {
        // 50 Hz panel running 33 ppm slow against a 25.000 fps source — the
        // measured difference on this fleet. Presentation phase walks a whole
        // vblank every ~10 min, which must surface as exactly one extra held
        // vblank per beat, not as continuous error.
        let panel = 50.0 * (1.0 - 33e-6);
        let mut s = CadenceScheduler::new(panel, 25.0).unwrap();
        let frames = 25 * 60 * 20; // 20 minutes
        let mut total = 0u64;
        let mut absorbs = 0u32;
        for _ in 0..frames {
            let h = s.next_hold();
            total += u64::from(h.vblanks);
            if h.kind == HoldKind::DriftAbsorb {
                absorbs += 1;
            }
        }
        // A slow panel delivers *fewer* vblanks than 2 per frame, so the
        // accumulator sheds a hold roughly once per beat — a 1-vblank hold
        // among 2s. Over ~20 min at a ~10 min beat that is a couple of events,
        // and crucially not a continuous stream.
        assert!(
            (1..=4).contains(&absorbs),
            "expected a couple of shed holds over 20 min, got {absorbs}"
        );
        // Total vblanks must track the panel, within one.
        let expected = (frames as f64 * (panel / 25.0)).round() as u64;
        assert!(
            total.abs_diff(expected) <= 1,
            "vblank total {total} drifted from {expected}"
        );
    }

    #[test]
    fn a_fast_panel_absorbs_drift_as_discrete_extra_holds() {
        // Mirror of the above: panel 33 ppm fast. The extra vblanks have to go
        // somewhere, and they must appear as whole extra holds rather than
        // being silently lost.
        let panel = 50.0 * (1.0 + 33e-6);
        let mut s = CadenceScheduler::new(panel, 25.0).unwrap();
        let frames = 25 * 60 * 20;
        let mut absorbs = 0u32;
        for _ in 0..frames {
            if s.next_hold().kind == HoldKind::DriftAbsorb {
                absorbs += 1;
            }
        }
        // ~10 min beat over a 20 min window: expect roughly two, and certainly
        // neither zero nor a continuous stream.
        assert!(
            (1..=4).contains(&absorbs),
            "expected a couple of drift absorptions over 20 min, got {absorbs}"
        );
    }

    #[test]
    fn rejects_cadences_it_cannot_schedule() {
        // Source faster than the panel needs frame *dropping*, a different
        // algorithm; the caller must keep its existing path.
        assert!(CadenceScheduler::new(50.0, 60.0).is_none());
        // Nonsense inputs must not produce a scheduler.
        assert!(CadenceScheduler::new(0.0, 25.0).is_none());
        assert!(CadenceScheduler::new(50.0, 0.0).is_none());
        assert!(CadenceScheduler::new(f64::NAN, 25.0).is_none());
        assert!(CadenceScheduler::new(50.0, f64::INFINITY).is_none());
        // Absurdly slow source: beyond this the hold is so long that the
        // frame-rate conversion is the operator's problem, not ours.
        assert!(CadenceScheduler::new(60.0, 1.0).is_none());
    }

    #[test]
    fn reset_clears_fractional_debt() {
        let mut s = CadenceScheduler::new(60.0, 24.0).unwrap();
        let first = s.next_hold().vblanks;
        for _ in 0..7 {
            s.next_hold();
        }
        s.reset();
        assert_eq!(
            s.next_hold().vblanks,
            first,
            "reset must reproduce the seed state, not carry the old stream's debt"
        );
    }

    fn hz_to_period_ns(hz: f64) -> u64 {
        (1_000_000_000.0 / hz).round() as u64
    }

    #[test]
    fn rebuild_key_separates_a_wrong_measurement_from_the_right_one() {
        // The exact pair observed on bilby-pir6s: a window that straddled a
        // 60 -> 50 Hz modeset measured 50.61 Hz for a panel independently
        // measured at 49.998 Hz. Keying on advertised integer Hz made these
        // the same key, so the bad reading was never replaced.
        let bad = rebuild_key(hz_to_period_ns(50.60964376883944), 25_000);
        let good = rebuild_key(hz_to_period_ns(49.998), 25_000);
        assert_ne!(
            bad, good,
            "a 1.2% panel-rate error must force a scheduler rebuild"
        );
    }

    #[test]
    fn rebuild_key_is_stable_across_ordinary_measurement_jitter() {
        // Same panel, successive windows, differing by a few ppm. These must
        // NOT rebuild -- a scheduler rebuilt every window would reset its
        // fractional debt every window and never absorb drift at all.
        let a = rebuild_key(hz_to_period_ns(49.998), 25_000);
        let b = rebuild_key(hz_to_period_ns(49.9985), 25_000);
        let c = rebuild_key(hz_to_period_ns(49.9975), 25_000);
        assert_eq!(a, b, "sub-ppm drift must not rebuild the scheduler");
        assert_eq!(a, c, "sub-ppm drift must not rebuild the scheduler");
    }

    #[test]
    fn a_wrong_panel_rate_manufactures_judder() {
        // Why the key matters at all: state the cost of the stale reading in
        // terms of what reaches the panel. The true cadence is exactly 2, so
        // a correct scheduler emits nothing but 2s.
        let mut good = CadenceScheduler::new(49.998, 25.0).expect("valid");
        let holds: Vec<u32> = (0..500).map(|_| good.next_hold().vblanks).collect();
        assert!(
            holds.iter().all(|v| *v == 2),
            "an exact 2:1 cadence must never vary the hold"
        );

        // The stale reading does not, and every departure is a visible hitch.
        let mut bad = CadenceScheduler::new(50.60964376883944, 25.0).expect("valid");
        let hitches = (0..500).filter(|_| bad.next_hold().vblanks != 2).count();
        assert!(
            hitches > 5,
            "the 50.61 Hz misreading should inject repeated hitches, saw {hitches}"
        );
    }
}
