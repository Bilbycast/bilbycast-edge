// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Explicit rational frame-rate conversion cadence (media-player normalisation,
//! Phase 6).
//!
//! The vendored FFmpeg build sets `--disable-avfilter` (see the plan §4.1), so
//! there is no `fps`/`framerate` avfilter graph to delegate constant-frame-rate
//! conversion to — it has to be computed explicitly. This module is that pure
//! primitive: given a source and target rational frame rate it decides, for
//! each output slot, which decoded source frame should be presented. A source
//! frame that maps to several output slots is repeated (pull-up, e.g. 24→30); a
//! source frame that no output slot lands on is dropped (pull-down, e.g. 30→24).
//!
//! It is deliberately decoupled from any codec, decoder, encoder, or hardware
//! session so it is exhaustively unit-testable on its own (plan §17.1
//! "rational frame-rate comparison").
//!
//! Two halves with different maturity. The **rate comparison** half
//! ([`FrameRateConverter::new`] + [`FrameRateConverter::is_identity`]) is
//! live: the Phase 2 playlist planner classifies every boundary's
//! frame-rate change through it, so the planner's notion of "no conversion
//! needed" and this module's notion of "the converter is the identity" cannot
//! drift apart. The **cadence** half (`source_index_for_output`,
//! `output_repeat_for_source`, `output_len_for_source_len`) has no consumer
//! yet — it is driven by the normalisation playout pipeline (decode →
//! scale/convert → encode, Phase 6, feature-gated on a video encoder) when
//! that lands, and is marked `#[allow(dead_code)]` individually rather than
//! module-wide so genuinely new dead code here still warns.
//!
//! **Nearest-preceding model.** Output frame `k` presents at wall time
//! `k · dst_den / dst_num`. The source frame showing at that instant is the one
//! whose own presentation time is the greatest not exceeding it, i.e.
//! `floor(output_time · src_num / src_den)`. This "hold the last source frame"
//! rule is the standard constant-rate resample: it never invents motion
//! (no blending), is monotonic, and yields the classic 3:2-style repeat/drop
//! cadence for the broadcast rates (23.976/24/25/29.97/30/50/59.94). All
//! arithmetic is done in `u128` so the rate products never overflow and the
//! mapping is exact — no floating-point drift accumulates across a long loop.

/// A constant-rate frame-rate converter between two rational rates.
///
/// Rates are `num/den` in frames per second (e.g. `30000/1001` for 29.97).
/// Both must be non-zero; [`new`](Self::new) returns `None` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRateConverter {
    src_num: u64,
    src_den: u64,
    dst_num: u64,
    dst_den: u64,
}

impl FrameRateConverter {
    /// Build a converter from source `src_num/src_den` fps to target
    /// `dst_num/dst_den` fps. Returns `None` if any component is zero.
    pub fn new(src_num: u64, src_den: u64, dst_num: u64, dst_den: u64) -> Option<Self> {
        if src_num == 0 || src_den == 0 || dst_num == 0 || dst_den == 0 {
            return None;
        }
        Some(Self { src_num, src_den, dst_num, dst_den })
    }

    /// Whether source and target describe the same rate (cross-multiplied so
    /// `30000/1001` equals `60000/2002`). When true, conversion is the identity
    /// and the pipeline can skip the repeat/drop bookkeeping entirely.
    pub fn is_identity(&self) -> bool {
        (self.src_num as u128) * (self.dst_den as u128)
            == (self.dst_num as u128) * (self.src_den as u128)
    }

    /// The 0-based source frame index that should be presented at output slot
    /// `output_index`. Monotonic non-decreasing in `output_index`.
    ///
    /// `floor(output_index · dst_den · src_num / (dst_num · src_den))`.
    #[allow(dead_code)] // Phase 6 normalisation pipeline (see module doc)
    pub fn source_index_for_output(&self, output_index: u64) -> u64 {
        let numer = (output_index as u128) * (self.dst_den as u128) * (self.src_num as u128);
        let denom = (self.dst_num as u128) * (self.src_den as u128);
        (numer / denom) as u64
    }

    /// How many output slots a given source frame is presented in, over an
    /// output run of `output_len` slots. `0` means the frame is dropped (its
    /// time falls between two output slots). Useful for a producer that walks
    /// source frames and asks "emit this decoded frame N times, or skip it".
    #[allow(dead_code)] // Phase 6 normalisation pipeline (see module doc)
    pub fn output_repeat_for_source(&self, source_index: u64, output_len: u64) -> u64 {
        // Count outputs k in [0, output_len) with source_index_for_output(k) == source_index.
        // The mapping is monotonic, so the matching k form a contiguous run; find its bounds.
        let first = self.first_output_for_source(source_index, output_len);
        match first {
            None => 0,
            Some(start) => {
                let mut end = start;
                while end < output_len && self.source_index_for_output(end) == source_index {
                    end += 1;
                }
                end - start
            }
        }
    }

    /// The first output slot (< `output_len`) that presents `source_index`, or
    /// `None` if the frame is dropped or lies beyond the run.
    fn first_output_for_source(&self, source_index: u64, output_len: u64) -> Option<u64> {
        // Smallest k with floor(k · dst_den · src_num / (dst_num · src_den)) >= source_index,
        // then confirm it maps exactly to source_index (else the frame is skipped).
        // k >= source_index · dst_num · src_den / (dst_den · src_num), take the ceiling.
        let numer = (source_index as u128) * (self.dst_num as u128) * (self.src_den as u128);
        let denom = (self.dst_den as u128) * (self.src_num as u128);
        let k = numer.div_ceil(denom) as u64;
        if k >= output_len {
            return None;
        }
        if self.source_index_for_output(k) == source_index {
            Some(k)
        } else {
            None
        }
    }

    /// Number of output frames spanning `source_len` source frames — i.e. how
    /// many target-rate slots the source's own duration covers. Used to size a
    /// normalised segment's output timeline.
    #[allow(dead_code)] // Phase 6 normalisation pipeline (see module doc)
    pub fn output_len_for_source_len(&self, source_len: u64) -> u64 {
        // source duration = source_len · src_den / src_num seconds
        // output frames   = ceil(duration · dst_num / dst_den)
        let numer = (source_len as u128) * (self.src_den as u128) * (self.dst_num as u128);
        let denom = (self.src_num as u128) * (self.dst_den as u128);
        numer.div_ceil(denom) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_components_rejected() {
        assert!(FrameRateConverter::new(0, 1, 30, 1).is_none());
        assert!(FrameRateConverter::new(30, 0, 30, 1).is_none());
        assert!(FrameRateConverter::new(30, 1, 0, 1).is_none());
        assert!(FrameRateConverter::new(30, 1, 30, 0).is_none());
        assert!(FrameRateConverter::new(24, 1, 30, 1).is_some());
    }

    #[test]
    fn identity_is_detected_across_equivalent_ratios() {
        assert!(FrameRateConverter::new(30, 1, 30, 1).unwrap().is_identity());
        assert!(FrameRateConverter::new(30000, 1001, 60000, 2002).unwrap().is_identity());
        assert!(!FrameRateConverter::new(24, 1, 25, 1).unwrap().is_identity());
    }

    #[test]
    fn same_rate_is_one_to_one() {
        let c = FrameRateConverter::new(25, 1, 25, 1).unwrap();
        for k in 0..100 {
            assert_eq!(c.source_index_for_output(k), k);
        }
    }

    #[test]
    fn output_index_mapping_is_monotonic_non_decreasing() {
        for &(sn, sd, dn, dd) in &[(24u64, 1u64, 30u64, 1u64), (30, 1, 24, 1), (30000, 1001, 25, 1)] {
            let c = FrameRateConverter::new(sn, sd, dn, dd).unwrap();
            let mut prev = 0;
            for k in 0..600 {
                let s = c.source_index_for_output(k);
                assert!(s >= prev, "mapping must not go backwards at k={k}");
                prev = s;
            }
        }
    }

    #[test]
    fn pull_up_24_to_30_repeats_in_a_5_to_6_cadence() {
        // 24→30 over one second: 30 output slots must draw from source 0..=23,
        // repeating in the classic 5:6 pattern (every 5 source frames become 6).
        let c = FrameRateConverter::new(24, 1, 30, 1).unwrap();
        let mapped: Vec<u64> = (0..30).map(|k| c.source_index_for_output(k)).collect();
        assert_eq!(mapped[0], 0);
        assert_eq!(*mapped.last().unwrap(), 23, "last output slot shows source frame 23");
        // Exactly 6 source frames are shown twice (30 slots, 24 distinct sources).
        let repeats = (0..24).filter(|&s| c.output_repeat_for_source(s, 30) == 2).count();
        let singles = (0..24).filter(|&s| c.output_repeat_for_source(s, 30) == 1).count();
        assert_eq!(repeats, 6);
        assert_eq!(singles, 18);
        assert_eq!(repeats * 2 + singles, 30);
    }

    #[test]
    fn pull_down_30_to_24_drops_one_in_five() {
        // 30→24 over one second: 6 of the 30 source frames are dropped.
        let c = FrameRateConverter::new(30, 1, 24, 1).unwrap();
        let dropped = (0..30).filter(|&s| c.output_repeat_for_source(s, 24) == 0).count();
        let shown = (0..30).filter(|&s| c.output_repeat_for_source(s, 24) == 1).count();
        assert_eq!(dropped, 6, "6 source frames dropped");
        assert_eq!(shown, 24, "24 source frames shown once");
    }

    #[test]
    fn ntsc_2997_to_pal_25_is_a_pull_down() {
        // 29.97→25: fewer output frames than source ⇒ net drops, monotonic.
        let c = FrameRateConverter::new(30000, 1001, 25, 1).unwrap();
        // Over ~1s (25 output slots) the last source index is ~29.
        let last = c.source_index_for_output(24);
        assert!((28..=30).contains(&last), "≈29.97 source frames elapse in 25 output slots, got {last}");
        assert!(!c.is_identity());
    }

    #[test]
    fn output_len_for_source_len_matches_rate_ratio() {
        // 24 source frames at 24fps = 1s ⇒ 30 output frames at 30fps.
        let c = FrameRateConverter::new(24, 1, 30, 1).unwrap();
        assert_eq!(c.output_len_for_source_len(24), 30);
        // 30 source frames at 30fps = 1s ⇒ 24 output frames at 24fps.
        let d = FrameRateConverter::new(30, 1, 24, 1).unwrap();
        assert_eq!(d.output_len_for_source_len(30), 24);
        // Identity keeps the count.
        let e = FrameRateConverter::new(25, 1, 25, 1).unwrap();
        assert_eq!(e.output_len_for_source_len(50), 50);
    }

    #[test]
    fn repeat_run_covers_every_output_slot_exactly_once() {
        // For any conversion, summing each source frame's output-repeat over the
        // run must equal the run length — no slot double-counted or missed.
        for &(sn, sd, dn, dd) in &[(24u64, 1u64, 30u64, 1u64), (30, 1, 24, 1), (50, 1, 60, 1)] {
            let c = FrameRateConverter::new(sn, sd, dn, dd).unwrap();
            let out_len = 240u64;
            let max_src = c.source_index_for_output(out_len - 1) + 1;
            let total: u64 = (0..max_src).map(|s| c.output_repeat_for_source(s, out_len)).sum();
            assert_eq!(total, out_len, "{sn}/{sd}->{dn}/{dd}: repeats must tile the output run");
        }
    }
}
