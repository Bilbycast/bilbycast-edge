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

use std::time::{Duration, Instant};

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
        if !(1.0..=16.0).contains(&vpf) {
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
    // 10 µs is 500 ppm at 50 Hz (10 000 / 20 000 000) — an earlier comment here
    // said 25 ppm, which is wrong by 20x and made the bin look far tighter than
    // it is. The property that actually makes it safe is the ratio to the
    // measurement noise, not the absolute figure: the endpoint estimator over a
    // 600-flip window resolves ~59 ns (√2 × 50 µs of timestamp jitter spread
    // over ~1200 vblanks), so a bin is ~170σ wide, while the structurally wrong
    // reading it exists to catch — 50.61 Hz against a true 49.998 — is 24 bins
    // away.
    //
    // Note the bin edge is not evenly placed against real panels, and that is
    // load-bearing rather than lucky: at 50 Hz the nominal period lands exactly
    // ON an edge, but a panel sitting there is at 0 ppm and has no drift to
    // absorb, so a rebuild costs it nothing. Panels that DO have drift to
    // absorb — this fleet's sit 12–65 ppm off nominal — are 11–57σ from the
    // nearest edge and never straddle one. The straddle case and the harmful
    // case are anti-correlated, which is why this stays a plain bucket.
    (vblank_period_ns / 10_000, fps_milli)
}

/// How far the live ratio may depart from the one a running scheduler was built
/// with before that scheduler has stopped describing the panel in front of it.
///
/// Sized between two measured populations rather than picked round:
///
///   * **Legitimate spread, which must never trip this.** The panel is
///     re-measured endpoint-style (~2 ppm), the source EMA truncates to whole
///     microseconds (≤ 480 ppm at 59.94), and after a modeset the candidate is
///     computed from the mode's *advertised* refresh while the running
///     scheduler was built from a *measured* one — on an NTSC mode advertising
///     integer 60 Hz that gap is 1000 ppm, the largest term by far. Budget the
///     sum at 0.1 %.
///   * **Departures worth catching.** A 60 → 50 Hz auto-match is 16.7 %,
///     50 → 60 is 20 %, and a 25 → 50 fps source change is 100 %.
///
/// 2.5 % sits 25x above the first and 6.7x below the second.
///
/// Deliberately **coarser** than [`rebuild_key`]'s 500 ppm bin, because the two
/// answer different questions. The bin asks "is a better scheduler available?"
/// and wants to notice a 1.2 % misreading. This asks "is the running one now
/// unsafe?", and a scheduler that is merely improvable must keep running until
/// its replacement can be built.
pub const CADENCE_STALE_TOLERANCE: f64 = 0.025;

/// Whether a running scheduler still matches the two clocks in front of it.
///
/// The display loop cannot rely on [`rebuild_key`] for this. A key change says
/// a rebuild is *wanted*; it cannot say the running scheduler is still safe,
/// because the rebuild it gates is rate-limited and — after a modeset — blocked
/// outright until the vblank clock has been re-measured over a fresh 600-flip
/// window. Through that window the old scheduler would otherwise go on
/// allocating vblanks at the old panel's rate.
///
/// Unknown resolves to **stale**: an unmeasurable panel or an unseeded source
/// means the honest answer is "stop and fall back to wall-clock pacing", never
/// "carry on with the last ratio".
pub fn cadence_is_stale(
    built_vblanks_per_frame: f64,
    panel_period_ns: Option<u64>,
    source_period_us: u64,
) -> bool {
    let Some(panel_ns) = panel_period_ns.filter(|ns| *ns > 0) else {
        return true;
    };
    // `is_finite` first so a NaN ratio takes the stale path rather than falling
    // through a comparison that is false for every operand.
    if source_period_us == 0
        || !built_vblanks_per_frame.is_finite()
        || built_vblanks_per_frame <= 0.0
    {
        return true;
    }
    // panel_hz / source_fps, rearranged so both measurements stay as the raw
    // periods they were taken as: (1/panel_s) / (1/source_s) = source_s / panel_s.
    let candidate = 1_000.0 * source_period_us as f64 / panel_ns as f64;
    if !candidate.is_finite() {
        return true;
    }
    (candidate - built_vblanks_per_frame).abs()
        > built_vblanks_per_frame * CADENCE_STALE_TOLERANCE
}

/// How long a stale reading must persist before the scheduler is dropped.
///
/// A rate estimate can depart from the built ratio for two very different
/// reasons, and the tolerance alone cannot tell them apart:
///
///   * **A transient.** `upstream_frame_period_us` is reset by a PTS
///     discontinuity and then re-converges through its EMA. Measured on this
///     dev host against a 15 s file on loop: at each loop boundary the estimate
///     stepped to 42 191 µs (5.5 % off a true 40 000) and was back inside 2.5 %
///     **within 200 ms**. The rates either side of the loop were identical, so
///     dropping the scheduler there achieved nothing but losing the cadence for
///     the several seconds it takes to re-engage — measured as a disengage on
///     every single loop.
///   * **A real change.** A mode change or a source rate change moves the ratio
///     and it stays moved.
///
/// One second separates them with margin either way: 5x longer than the
/// measured transient, and 5x shorter than the ~4.9 s it takes an over-holding
/// stale cadence to fill the 24-slot decode queue and start shedding.
pub const CADENCE_STALE_DWELL: Duration = Duration::from_secs(1);

/// Debounce for [`cadence_is_stale`].
///
/// Split out from the display loop so the edge-timing can be tested with
/// synthetic instants — the loop itself has no test harness, and an
/// off-by-one-edge here either flaps the cadence off on every source hiccup or
/// leaves a stale scheduler running.
#[derive(Debug, Default, Clone)]
pub struct StaleDwell {
    since: Option<Instant>,
}

impl StaleDwell {
    /// Feed one observation. Returns `true` exactly once per episode, on the
    /// first observation at or past [`CADENCE_STALE_DWELL`] — the caller acts on
    /// that and the episode is then closed, so a caller that keeps reporting
    /// `stale` is not told to disengage over and over.
    pub fn observe(&mut self, stale: bool, now: Instant) -> bool {
        if !stale {
            self.since = None;
            return false;
        }
        match self.since {
            None => {
                self.since = Some(now);
                false
            }
            Some(t) => {
                if now.duration_since(t) >= CADENCE_STALE_DWELL {
                    self.since = None;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Forget any episode in progress — used when the scheduler goes away for
    /// an unrelated reason, so the next one starts clean.
    pub fn clear(&mut self) {
        self.since = None;
    }
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

    fn fps_to_period_us(fps: f64) -> u64 {
        (1_000_000.0 / fps).round() as u64
    }

    #[test]
    fn a_transient_stale_reading_never_disengages() {
        // The measured loop-boundary excursion: out of tolerance, back inside
        // within 200 ms. Sampled at the 25 fps loop rate that is 5 observations.
        let mut d = StaleDwell::default();
        let t0 = Instant::now();
        for i in 0..5 {
            assert!(
                !d.observe(true, t0 + Duration::from_millis(40 * i)),
                "a 200 ms excursion must not disengage"
            );
        }
        // Recovered — the episode is forgotten.
        assert!(!d.observe(false, t0 + Duration::from_millis(240)));
        // ...and a later, equally short excursion must not inherit its age.
        assert!(!d.observe(true, t0 + Duration::from_secs(30)));
        assert!(!d.observe(
            true,
            t0 + Duration::from_secs(30) + Duration::from_millis(200)
        ));
    }

    #[test]
    fn a_persistent_stale_reading_disengages_once() {
        let mut d = StaleDwell::default();
        let t0 = Instant::now();
        assert!(!d.observe(true, t0));
        assert!(!d.observe(true, t0 + CADENCE_STALE_DWELL - Duration::from_millis(1)));
        assert!(
            d.observe(true, t0 + CADENCE_STALE_DWELL),
            "must fire at the dwell boundary"
        );
        // Exactly once: the caller has acted, and a still-true reading must not
        // re-fire on the very next frame.
        assert!(!d.observe(true, t0 + CADENCE_STALE_DWELL + Duration::from_millis(40)));
    }

    #[test]
    fn the_dwell_is_shorter_than_the_damage_and_longer_than_the_transient() {
        // Pin both margins. 200 ms is the measured EMA recovery after a PTS
        // discontinuity; ~4.9 s is the simulated time for an over-holding stale
        // cadence to fill the 24-slot decode queue and begin shedding.
        assert!(CADENCE_STALE_DWELL >= Duration::from_millis(600));
        assert!(CADENCE_STALE_DWELL <= Duration::from_millis(2500));
    }

    #[test]
    fn clearing_the_dwell_abandons_the_episode() {
        let mut d = StaleDwell::default();
        let t0 = Instant::now();
        assert!(!d.observe(true, t0));
        d.clear();
        // Age is gone, so the dwell restarts from here.
        assert!(!d.observe(true, t0 + CADENCE_STALE_DWELL));
        assert!(d.observe(true, t0 + CADENCE_STALE_DWELL + CADENCE_STALE_DWELL));
    }

    #[test]
    fn a_refresh_change_makes_the_running_scheduler_stale() {
        // The harmful direction, and the one the auto-match actually performs:
        // built for 60 Hz (2.4 vblanks/frame at 25 fps), panel is now 50 Hz, so
        // every hold is 20 % too long and the surplus has to go somewhere.
        assert!(cadence_is_stale(
            60.0 / 25.0,
            Some(hz_to_period_ns(50.0)),
            fps_to_period_us(25.0)
        ));
        // And the benign direction still counts as stale — the ratio is wrong
        // either way, and re-deriving it is cheap.
        assert!(cadence_is_stale(
            50.0 / 25.0,
            Some(hz_to_period_ns(60.0)),
            fps_to_period_us(25.0)
        ));
    }

    #[test]
    fn a_resolution_only_modeset_leaves_the_scheduler_valid() {
        // The modal modeset: 1080p -> 720p at an unchanged 50 Hz. The scheduler
        // was built from a MEASURED 49.998 Hz; the candidate is computed from
        // the new mode's ADVERTISED 50 Hz because the measurement was just
        // discarded. Tearing the scheduler down here would reseed `accum` and
        // lose the drift absorption for nothing.
        assert!(!cadence_is_stale(
            49.998 / 25.0,
            Some(hz_to_period_ns(50.0)),
            fps_to_period_us(25.0)
        ));
    }

    #[test]
    fn an_ntsc_mode_advertising_integer_hz_is_not_stale() {
        // Largest legitimate gap in the whole budget: built from a measured
        // 59.94 Hz, candidate from the mode's advertised 60. 0.1 %, which must
        // stay well inside the tolerance or every NTSC panel disengages on
        // every modeset forever.
        assert!(!cadence_is_stale(
            59.94 / 29.97,
            Some(hz_to_period_ns(60.0)),
            fps_to_period_us(29.97)
        ));
    }

    #[test]
    fn a_source_rate_change_makes_the_running_scheduler_stale() {
        // No modeset at all — the panel is untouched and only the source
        // changed. A fix keyed on the vblank-clock trust bit would miss this
        // case completely.
        assert!(cadence_is_stale(
            50.0 / 25.0,
            Some(hz_to_period_ns(50.0)),
            fps_to_period_us(50.0)
        ));
    }

    #[test]
    fn ordinary_crystal_drift_never_makes_the_scheduler_stale() {
        // The no-churn requirement, over both axes at once — something the
        // two-part rebuild key could not express. +/-100 ppm is beyond this
        // fleet's measured 12-65 ppm spread.
        for panel_ppm in [-100.0, -33.0, 0.0, 33.0, 100.0] {
            for source_ppm in [-100.0, -33.0, 0.0, 33.0, 100.0] {
                for (panel_hz, fps) in [(50.0, 25.0), (50.0, 50.0), (60.0, 24.0), (60.0, 20.0)] {
                    let panel = panel_hz * (1.0 + panel_ppm / 1e6);
                    let source = fps * (1.0 + source_ppm / 1e6);
                    assert!(
                        !cadence_is_stale(
                            panel_hz / fps,
                            Some(hz_to_period_ns(panel)),
                            fps_to_period_us(source)
                        ),
                        "{panel_ppm} ppm panel / {source_ppm} ppm source at \
                         {panel_hz}Hz/{fps}fps must not disengage"
                    );
                }
            }
        }
    }

    #[test]
    fn an_unmeasurable_clock_makes_the_scheduler_stale() {
        // Unknown must resolve to the benign side: disengage and let the
        // wall-clock pacer have it, rather than keep holding on a guess.
        assert!(cadence_is_stale(2.0, None, fps_to_period_us(25.0)));
        assert!(cadence_is_stale(2.0, Some(0), fps_to_period_us(25.0)));
        // `upstream_frame_period_us` is reset to 0 by a PTS discontinuity.
        assert!(cadence_is_stale(2.0, Some(hz_to_period_ns(50.0)), 0));
        // A scheduler that never built cannot be matched against.
        assert!(cadence_is_stale(0.0, Some(hz_to_period_ns(50.0)), fps_to_period_us(25.0)));
    }

    #[test]
    fn the_stale_tolerance_sits_between_drift_and_a_mode_change() {
        // Pin the constant against both populations it was sized from, driven
        // through the real function so the property is what is tested rather
        // than the literal. Departures are applied to the PANEL period, which
        // is where a mode change lands.
        let built = 2.0;
        let fps_us = fps_to_period_us(25.0);
        let panel_at = |departure: f64| {
            // built = 1000 * fps_us / panel_ns, so a ratio departure of `d`
            // means a panel period scaled by 1/(1+d).
            Some((1_000.0 * fps_us as f64 / (built * (1.0 + departure))).round() as u64)
        };
        // 0.1 % — the largest legitimate gap, advertised-vs-measured on an NTSC
        // mode. Must NOT disengage, in either direction.
        assert!(!cadence_is_stale(built, panel_at(0.001), fps_us));
        assert!(!cadence_is_stale(built, panel_at(-0.001), fps_us));
        // 16.7 % — the smallest real departure, a 60 -> 50 Hz auto-match. Must
        // disengage, in either direction.
        assert!(cadence_is_stale(built, panel_at(0.167), fps_us));
        assert!(cadence_is_stale(built, panel_at(-0.167), fps_us));
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
