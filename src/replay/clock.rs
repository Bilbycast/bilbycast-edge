// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! The one mapping between wall clock and the recording's PTS counter.
//!
//! `accumulated_pts` advances on the **source's** PCR, and a source
//! free-running against this host's clock is the normal case, so the counter
//! does not run at 90 000 ticks per wall-clock second. The writer takes an
//! anchor `(wall, pts)` on the first indexed frame and re-takes a second
//! `recent` sample every minute; between them they give the rate the
//! recording has actually run at.
//!
//! Both sides of the recorder — the writer dating a gap, and the clip
//! exporter placing a mark — have to use the *same* line, or the counter has a
//! kink at every gap: the writer used to resume onto the nominal line while
//! the exporter read the measured one, so a 10 s Stop/Start after a day at
//! 450 ppm jumped the counter by 49 s and put every later mark tens of seconds
//! out. Everything that turns a wall instant into ticks, or ticks into a wall
//! instant, lives here for that reason.

/// The nominal rate: 90 kHz ticks per microsecond, as a fraction.
pub(crate) const NOMINAL_TICKS_PER_US: (i128, i128) = (90_000, 1_000_000);

/// How far the measured rate may differ from nominal before it is disbelieved.
///
/// A source 450 ppm off — the figure this feature's own CMAF half measured on
/// its rig — is 1.00045. Anything outside ±5 % is not a clock offset, it is a
/// corrupt or mis-paired sample, and extrapolating on it would be worse than
/// assuming nominal.
pub(crate) const MAX_RATE_DEVIATION: f64 = 0.05;

/// The shortest span a measured rate is trusted over.
///
/// Below this the arrival jitter of the two sampled frames dominates: 5 ms of
/// sampling error over 30 s is 167 ppm, which is the same order as the offset
/// being measured. Over ten minutes it is 8 ppm and the measurement is worth
/// more than the assumption.
pub(crate) const MIN_RATE_SPAN_US: i128 = 600_000_000;

/// A wall-clock instant as a PTS in the recording.
///
/// Two points, not one, when the recording has been running long enough to
/// have measured a second. `accumulated_pts` advances on the **source's** PCR
/// while a mark is placed from a wall-clock-true date, so the one-point form —
/// `anchor_pts + (wall - anchor_wall) * 90_000 / 1_000_000` — is assuming the
/// source runs at exactly this host's rate. It does not. The CMAF half of this
/// same feature measured its own source at ~450 ppm slow and added a slewing
/// epoch because of it, recording that an epoch pinned once and held drifts
/// 4.1 s across a 2h30m session; the recorder's anchor *is* such a pinned
/// epoch, so the cut drifted by that offset times the age of the **recording**
/// — 1.6 s after an hour, unbounded on a 24/7 DVR flow, and silently, because
/// `find_floor` clamps rather than erroring and hands back a playable clip of
/// the wrong moment.
///
/// So the rate comes from `(recent - anchor)` when that span is long enough to
/// have measured it, and from the nominal 90 kHz otherwise. Both forms are
/// computed in i128: a mark well before the recording began must not wrap a u64
/// into a range near the end of time and hand the exporter something absurd.
/// `None` means "before this recording started", which is a real answer — the
/// media does not exist, and the caller falls back rather than cutting nonsense.
pub(crate) fn pts_for_wall(
    anchor_wall_us: i64,
    anchor_pts: u64,
    recent: Option<(i64, u64)>,
    wall_us: i64,
) -> Option<u64> {
    let (num, den) = measured_rate(anchor_wall_us, anchor_pts, recent)
        .unwrap_or(NOMINAL_TICKS_PER_US);
    let delta_us = (wall_us as i128) - (anchor_wall_us as i128);
    let ticks = anchor_pts as i128 + delta_us * num / den;
    u64::try_from(ticks).ok()
}

/// Ticks per microsecond as measured between the anchor and the rolling sample,
/// or `None` when there is no sample, the span is too short to measure over, or
/// the result is too far from nominal to be a clock offset.
pub(crate) fn measured_rate(
    anchor_wall_us: i64,
    anchor_pts: u64,
    recent: Option<(i64, u64)>,
) -> Option<(i128, i128)> {
    let (rw, rp) = recent?;
    let den = (rw as i128) - (anchor_wall_us as i128);
    let num = (rp as i128) - (anchor_pts as i128);
    if den < MIN_RATE_SPAN_US || num <= 0 {
        return None;
    }
    let nominal = NOMINAL_TICKS_PER_US.0 as f64 / NOMINAL_TICKS_PER_US.1 as f64;
    let measured = num as f64 / den as f64;
    if (measured / nominal - 1.0).abs() > MAX_RATE_DEVIATION {
        return None;
    }
    Some((num, den))
}

/// Where the counter should be at `wall_us`, given where it was.
///
/// The writer's gap arithmetic: `last` is the last tick the counter reached,
/// and the answer is that tick moved forward by the wall time that has passed
/// since it, at the recording's own rate — measured when there is a trusted
/// sample, nominal otherwise. Dating `last` on the same line the counter was
/// built on is what keeps the timeline straight across the gap; dating it on
/// the nominal line re-based every resume onto a line the counter was never
/// on, by the whole of the drift accumulated so far.
///
/// The gap is clamped to `max_gap_us` and never negative, and the result never
/// falls below `last + 1`: an anchor that implies a backwards gap must not be
/// allowed to un-sort the index.
pub(crate) fn advance_to_wall(
    last: u64,
    anchor_wall_us: i64,
    anchor_pts: u64,
    recent: Option<(i64, u64)>,
    wall_us: i64,
    max_gap_us: i128,
) -> u64 {
    let (num, den) = measured_rate(anchor_wall_us, anchor_pts, recent).unwrap_or(NOMINAL_TICKS_PER_US);
    let last_wall_us = (anchor_wall_us as i128)
        + ((last as i128) - (anchor_pts as i128)) * den / num;
    let gap_us = ((wall_us as i128) - last_wall_us).clamp(0, max_gap_us);
    let resumed = (last as i128) + gap_us * num / den;
    u64::try_from(resumed).unwrap_or(last).max(last.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anchor measured on the rig, mapped back.
    ///
    /// Real values from bilby-z440: the recording anchored PTS 158400 to
    /// 1788827202551745 us. A mark one second later must land exactly 90000
    /// ticks on, and the identity case must return the anchor untouched — an
    /// error of one 90 kHz tick here is invisible in review and wrong in every
    /// clip.
    #[test]
    fn a_wall_instant_maps_to_the_pts_the_anchor_implies() {
        let aw = 1_788_827_202_551_745_i64;
        let ap = 158_400_u64;
        assert_eq!(pts_for_wall(aw, ap, None, aw), Some(ap), "the anchor itself must not move");
        assert_eq!(pts_for_wall(aw, ap, None, aw + 1_000_000), Some(ap + 90_000), "one second");
        assert_eq!(pts_for_wall(aw, ap, None, aw + 40_000), Some(ap + 3_600), "one frame at 25fps");
        assert_eq!(
            pts_for_wall(aw, ap, None, aw - 1_000_000),
            Some(ap - 90_000),
            "a second before the anchor is still inside the recording"
        );
    }

    /// A mark from before the recording began has no media behind it.
    ///
    /// The subtraction must not wrap: a u64 underflow would ask the exporter
    /// for a range near the end of time, which reads as a corrupt request
    /// rather than an absent one.
    #[test]
    fn a_mark_before_the_recording_started_is_refused_not_wrapped() {
        let aw = 1_788_827_202_551_745_i64;
        let ap = 158_400_u64; // 1.76s of media before the anchor
        // Two seconds earlier is past the start of the recording.
        assert_eq!(pts_for_wall(aw, ap, None, aw - 2_000_000), None);
        // An hour earlier certainly is.
        assert_eq!(pts_for_wall(aw, ap, None, aw - 3_600_000_000), None);
    }

    /// A source running off this host's clock must not drag the cut with it.
    ///
    /// `accumulated_pts` advances on the source's PCR; a mark comes from a
    /// wall-clock-true date. Assuming 90 000 ticks per wall-clock second puts
    /// the cut out by (offset × the age of the recording) — 450 ppm, the figure
    /// this feature's own CMAF half measured on its rig, is 1.6 s after an hour
    /// and grows without bound on a 24/7 DVR flow. Silently, because the
    /// exporter clamps rather than erroring and returns a playable clip of the
    /// wrong moment.
    #[test]
    fn a_slow_source_is_tracked_rather_than_assumed_nominal() {
        let aw = 1_788_827_202_551_745_i64;
        let ap = 158_400_u64;
        // A source 450 ppm slow: an hour of wall clock carries 3600 s of media
        // less 1.62 s of it.
        let hour_us = 3_600_000_000_i64;
        let recent_wall = aw + hour_us;
        let recent_pts = ap + ((3600.0 - 1.62) * 90_000.0) as u64;
        let recent = Some((recent_wall, recent_pts));

        // A mark at the rolling sample itself must land on it, to the frame.
        let at_sample = pts_for_wall(aw, ap, recent, recent_wall).expect("mappable");
        assert!(
            at_sample.abs_diff(recent_pts) < 3_600,
            "the measured point mapped to {at_sample}, {} ticks off its own PTS",
            at_sample.abs_diff(recent_pts)
        );

        // The nominal mapping is what it is being compared against: it puts the
        // same instant 1.62 s — 40 frames at 25 fps — further on.
        let nominal = pts_for_wall(aw, ap, None, recent_wall).expect("mappable");
        assert!(
            nominal.abs_diff(recent_pts) > 100_000,
            "the nominal mapping was supposed to be wrong here by ~145 800 ticks"
        );
    }

    /// A rate measured over too short a span, or too far from nominal to be a
    /// clock offset, is not believed.
    #[test]
    fn an_unmeasurable_or_implausible_rate_falls_back_to_nominal() {
        let aw = 1_788_827_202_551_745_i64;
        let ap = 158_400_u64;

        // Thirty seconds is dominated by the arrival jitter of two frames.
        let short = Some((aw + 30_000_000, ap + 30 * 90_000));
        assert_eq!(measured_rate(aw, ap, short), None);

        // Half rate is not a clock offset, it is a corrupt or mis-paired
        // sample; extrapolating on it is worse than assuming nominal.
        let wild = Some((aw + 3_600_000_000, ap + 1_800 * 90_000));
        assert_eq!(measured_rate(aw, ap, wild), None);
        assert_eq!(
            pts_for_wall(aw, ap, wild, aw + 1_000_000),
            Some(ap + 90_000),
            "an implausible rate must not reach the mapping"
        );

        // A sample that goes backwards in PTS is not a sample.
        assert_eq!(measured_rate(aw, ap, Some((aw + 3_600_000_000, ap))), None);
    }

    /// A gap is filled at the recording's own rate, from where the counter was.
    ///
    /// After a day at 450 ppm slow the counter sits 39 s below the nominal
    /// line. Dating the last tick on that line and refilling on it — what the
    /// writer used to do — read a 10 s Stop/Start as 49 s and put every later
    /// mark tens of seconds out; the exporter, reading the measured line,
    /// then saw the kink as a source that had sped up.
    #[test]
    fn a_gap_is_filled_on_the_measured_line_not_the_nominal_one() {
        let aw = 1_788_827_202_551_745_i64;
        let ap = 158_400_u64;
        let day_us = 86_400_000_000_i64;
        let ppm = 450.0;
        let ticks_per_us = 0.09 * (1.0 - ppm / 1e6);
        let sample = (aw + day_us, ap + (day_us as f64 * ticks_per_us) as u64);
        let recent = Some(sample);
        let last = sample.1;
        // Ten seconds later, the first frame back.
        let now = aw + day_us + 10_000_000;
        let resumed = advance_to_wall(last, aw, ap, recent, now, i128::MAX);
        let expected = last + (10_000_000.0 * ticks_per_us) as u64;
        assert!(
            resumed.abs_diff(expected) < 3_600,
            "resumed {resumed}, expected within a frame of {expected}"
        );
        // The nominal line is 39 s further on — that is the jump this replaces.
        let nominal = advance_to_wall(last, aw, ap, None, now, i128::MAX);
        assert!(nominal > resumed + 30 * 90_000, "nominal {nominal} vs measured {resumed}");
    }

    /// Never backwards, never below the floor, and clamped to the plausible.
    #[test]
    fn a_gap_never_moves_the_counter_backwards() {
        let aw = 1_000_000_000_i64;
        let ap = 86_400_u64;
        let last = ap + 100 * 90_000;
        // A clock that says the last tick is in the future.
        assert_eq!(advance_to_wall(last, aw, ap, None, aw, i128::MAX), last + 1);
        // An absurd gap is clamped, not believed.
        let capped = advance_to_wall(last, aw, ap, None, i64::MAX / 2, 90_000_000);
        assert_eq!(capped, last + 90 * 90_000);
    }
}
