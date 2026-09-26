// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! One continuous media timeline for a CMAF output, whatever the source's
//! timestamps do.
//!
//! The segmenters take a sample's timestamp as its decode time and a sample's
//! duration as the step to the next one, and a fragment's `tfdt` from its
//! first sample. A source whose timestamps jump — a `media_player` loop, an
//! input switch, a restarted encoder, a PES stamped on the wrong clock — put
//! the jump into the published media: at a loop of the 29.97 fps VH1 clip one
//! video sample ran +16 543.6 s and the IDR after it repeated the frame before
//! it, one audio sample ran +58.9 s, and the segment holding the loop ran
//! 3.98 s with two IDRs. A player stalls there. Nothing in the playlist said
//! so, and nothing could: a fragment carrying both sides of a jump cannot be
//! described by `#EXT-X-DISCONTINUITY`, which applies between segments.
//!
//! So the output keeps one timeline of its own: every timestamp is the
//! source's plus an offset, 0 until the source first jumps (a clean source is
//! published exactly as before). A timestamp is taken as it is when it is
//! continuous with its own track — within [`OWN_WINDOW_90K`] of the newest
//! one the track has had — or sits within [`CROSS_WINDOW_90K`] of the other
//! track's newest (a track that paused and came back on the programme's
//! clock is a gap, and stays one, in sync). Otherwise the offsets already in
//! use are tried — the other track may have met the same jump first, or the
//! source may be coming back from a short excursion — and only when none
//! makes it continuous does a new offset place it one step (the track's
//! smallest recent step) after the track's newest timestamp.
//!
//! Sharing offsets keeps audio and video together across a jump: a source
//! that moves to another clock moves both tracks by the same amount, and the
//! second track to see it takes the first one's offset rather than inventing
//! its own a frame or two off. The price of a new offset is a seam: the
//! content on either side is joined end to end, so a real gap in the source
//! across a discontinuity is closed.

/// One lap of the 33-bit PTS clock.
const LAP_90K: u64 = 1 << 33;
const MASK_33: u64 = LAP_90K - 1;

/// How far (90 kHz) a timestamp may sit from its track's newest and still be
/// continuous with it: 1 s either way. Backwards covers a B-frame source's
/// decode-order PTS; forwards a PES carrying up to 700 ms of audio, a
/// dropped frame or two, and a stall shorter than a second.
const OWN_WINDOW_90K: i64 = 90_000;

/// How far (90 kHz) a timestamp may sit from the *other* track's newest and
/// still be on the programme's clock: 3 s, past the video lead of a hardware
/// encoder (the witness source runs ~1.1 s) with room to spare, and three
/// orders of magnitude inside any clip loop or clock change.
const CROSS_WINDOW_90K: i64 = 3 * 90_000;

/// Offsets remembered, newest last. A source coming back from an excursion
/// finds the offset it had; more than a handful of live timelines at once
/// is not a source this can help.
const MAX_OFFSETS: usize = 8;

/// Steps remembered per track for the continuation step.
const STEP_HISTORY: usize = 8;

/// The largest step (90 kHz) taken as one frame's: 500 ms.
const MAX_STEP_90K: u64 = 45_000;

/// Signed distance `a - b` on the 33-bit circle.
fn circ(a: u64, b: u64) -> i64 {
    let d = a.wrapping_sub(b) & MASK_33;
    if d >= LAP_90K / 2 { d as i64 - LAP_90K as i64 } else { d as i64 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Video,
    Audio,
}

#[derive(Debug, Default)]
struct Line {
    /// Added to the source timestamp (mod 2^33).
    offset: u64,
    /// The newest output timestamp.
    newest: Option<u64>,
    /// The previous source timestamp, for the step measurement.
    last_src: Option<u64>,
    /// Recent positive steps between source timestamps.
    steps: [u64; STEP_HISTORY],
    n_steps: usize,
}

impl Line {
    fn note_step(&mut self, src: u64) {
        if let Some(prev) = self.last_src {
            let d = circ(src, prev);
            if d > 0 && d as u64 <= MAX_STEP_90K {
                self.steps[self.n_steps % STEP_HISTORY] = d as u64;
                self.n_steps += 1;
            }
        }
        self.last_src = Some(src);
    }

    /// The step a continuation takes: the smallest recent one (a frame, or a
    /// PES of audio), else `default`.
    fn step(&self, default: u64) -> u64 {
        self.steps[..self.n_steps.min(STEP_HISTORY)].iter().copied().min().unwrap_or(default)
    }
}

/// What mapping one timestamp did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapped {
    /// The output timestamp (90 kHz, 33 bits).
    pub ts: u64,
    /// The source jump a new offset absorbed here (90 kHz, source minus
    /// where the track continued), when this timestamp opened one.
    pub jump: Option<i64>,
}

#[derive(Debug)]
pub struct CmafTimeline {
    video: Line,
    audio: Line,
    offsets: Vec<u64>,
}

impl Default for CmafTimeline {
    fn default() -> Self {
        Self { video: Line::default(), audio: Line::default(), offsets: vec![0] }
    }
}

impl CmafTimeline {
    /// Map a source timestamp (90 kHz) of `track` onto the output timeline.
    pub fn map(&mut self, track: Track, src: u64) -> Mapped {
        let src = src & MASK_33;
        let (me, other) = match track {
            Track::Video => (&mut self.video, &self.audio),
            Track::Audio => (&mut self.audio, &self.video),
        };
        me.note_step(src);
        let Some(newest) = me.newest else {
            // A track's first timestamp takes the newest offset in use: the
            // other track may already have moved off the source's clock.
            me.offset = *self.offsets.last().unwrap_or(&0);
            let ts = (src + me.offset) & MASK_33;
            me.newest = Some(ts);
            return Mapped { ts, jump: None };
        };
        let other_newest = other.newest;
        let continuous = |off: u64| -> Option<u64> {
            let ts = (src + off) & MASK_33;
            let own = circ(ts, newest);
            let own_ok = (-OWN_WINDOW_90K..=OWN_WINDOW_90K).contains(&own);
            let cross_ok = other_newest.is_some_and(|o| circ(ts, o).abs() <= CROSS_WINDOW_90K);
            (own_ok || cross_ok).then_some(ts)
        };
        let own_offset = me.offset;
        let found = std::iter::once(own_offset)
            .chain(self.offsets.iter().rev().copied().filter(|o| *o != own_offset))
            .find_map(|off| continuous(off).map(|ts| (off, ts)));
        let (offset, ts, jump) = match found {
            Some((off, ts)) => (off, ts, None),
            None => {
                let default_step = match track {
                    Track::Video => 3_003,
                    Track::Audio => 1_920,
                };
                let ts = (newest + me.step(default_step)) & MASK_33;
                let off = ts.wrapping_sub(src) & MASK_33;
                self.offsets.push(off);
                if self.offsets.len() > MAX_OFFSETS {
                    self.offsets.remove(0);
                }
                (off, ts, Some(circ((src + own_offset) & MASK_33, ts)))
            }
        };
        me.offset = offset;
        if circ(ts, newest) > 0 {
            me.newest = Some(ts);
        }
        Mapped { ts, jump }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean source is published exactly as it came: offset 0 throughout,
    /// B-frame reordering and a 33-bit wrap included.
    #[test]
    fn a_continuous_source_is_untouched() {
        let mut tl = CmafTimeline::default();
        let mut v = MASK_33 - 30 * 3_003;
        let mut a = v - 1_000;
        for k in 0..200u64 {
            // Decode order I P B B: PTS steps +3, -2, +1 frames.
            let reorder = [0i64, 3, 1, 2][(k % 4) as usize] * 3_003;
            let pts = ((v as i64 + reorder) as u64) & MASK_33;
            assert_eq!(tl.map(Track::Video, pts), Mapped { ts: pts, jump: None });
            assert_eq!(tl.map(Track::Audio, a & MASK_33), Mapped { ts: a & MASK_33, jump: None });
            v += 3_003;
            a += 1_920;
        }
    }

    /// A media_player loop as the flow carried it into the CMAF output (the
    /// p4r-cmaf-vh1-base capture): six audio PES a second of audio stamped
    /// 60 s back, the loop's first picture stamped 16 543 s back, then both
    /// tracks carrying on where they were — the audio returning 1.1 s behind
    /// the audio just published. The output never steps back past a frame,
    /// never jumps forward past one, and comes back to the source's own
    /// clock (offset 0) on both tracks once the source does.
    #[test]
    fn a_loop_excursion_is_bridged_and_left() {
        let mut tl = CmafTimeline::default();
        let mut v = 3_168_500_000u64;
        let mut a = v - 4_000;
        let mut v_out = Vec::new();
        let mut a_out = Vec::new();
        for _ in 0..50 {
            v_out.push(tl.map(Track::Video, v).ts);
            a_out.push(tl.map(Track::Audio, a).ts);
            v += 3_003;
            a += 1_920;
        }
        // The excursion: a second of audio 60 s back...
        let excursion = a - 60 * 90_000;
        for k in 0..48u64 {
            let m = tl.map(Track::Audio, excursion + k * 1_920);
            a_out.push(m.ts);
        }
        // ...one picture 16 543 s back...
        let bad = v - 16_543 * 90_000;
        let m = tl.map(Track::Video, bad);
        assert!(m.jump.is_some());
        v_out.push(m.ts);
        v += 3_003;
        // ...then both carry on, the audio 1.1 s behind what went out.
        for _ in 0..60 {
            let m = tl.map(Track::Video, v);
            assert_eq!(m.ts, v, "video back on the source's clock");
            v_out.push(m.ts);
            v += 3_003;
        }
        let mut back = a + 5_000;
        for _ in 0..120 {
            let m = tl.map(Track::Audio, back);
            assert_eq!(m.ts, back, "audio back on the source's clock");
            a_out.push(m.ts);
            back += 1_920;
        }
        for w in v_out.windows(2) {
            let d = circ(w[1], w[0]);
            assert!((1..=3_003 * 3).contains(&d), "video step {d}");
        }
        // The audio is monotonic up to where its return overlaps the
        // excursion (the output drops those frames: `buffer_audio_frames`),
        // and never jumps.
        for w in a_out.windows(2) {
            let d = circ(w[1], w[0]);
            assert!(d.abs() <= 2 * 90_000, "audio step {d}");
        }
    }

    /// A source that moves to another clock moves both tracks: the second to
    /// see it takes the first one's offset, so audio and video keep exactly
    /// the relation the source gives them.
    #[test]
    fn a_clock_change_keeps_audio_and_video_together() {
        let mut tl = CmafTimeline::default();
        let (mut v, mut a) = (900_000u64, 895_000u64);
        for _ in 0..30 {
            tl.map(Track::Video, v);
            tl.map(Track::Audio, a);
            v += 3_600;
            a += 1_920;
        }
        let jump = 50_000 * 90_000u64;
        let v2 = tl.map(Track::Video, v + jump);
        assert!(v2.jump.is_some());
        assert_eq!(v2.ts, v - 3_600 + 3_600, "one frame on");
        let a2 = tl.map(Track::Audio, a + jump);
        assert_eq!(a2.jump, None, "the audio takes the video's offset");
        assert_eq!(circ(v2.ts, a2.ts), circ(v, a), "the A/V relation is the source's");
    }

    /// A track that paused while the other ran on comes back on the
    /// programme's clock: a gap, kept, in sync.
    #[test]
    fn an_audio_gap_on_the_programmes_clock_stays_a_gap() {
        let mut tl = CmafTimeline::default();
        let (mut v, mut a) = (900_000u64, 900_000u64);
        for _ in 0..30 {
            tl.map(Track::Video, v);
            tl.map(Track::Audio, a);
            v += 3_600;
            a += 1_920;
        }
        for _ in 0..75 {
            tl.map(Track::Video, v);
            v += 3_600;
        }
        let resumed = a + 3 * 90_000;
        assert_eq!(tl.map(Track::Audio, resumed), Mapped { ts: resumed, jump: None });
    }
}
