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
//!
//! The offsets are shared by every CMAF output of a flow
//! ([`CmafTimeline::for_flow`]), not kept per output. The flow's renditions
//! (a DVR session's main and all-intra proxy, any two CMAF outputs) share
//! one wall-clock epoch keyed by flow (`FlowClock`), so they must publish
//! one media timeline: an output restarted after a jump — a config edit to
//! its bitrate restarts that output alone — started over at offset 0 and
//! published the source's raw timestamps while its sibling kept the offset
//! it had absorbed, hours apart; each then re-anchored the other's epoch on
//! every segment. A track's first timestamp takes the offset the flow is on
//! (the one the output furthest along last moved onto), so a restarted
//! output lands where its siblings are, and a jump one output meets first is
//! taken up by the others as theirs.
//!
//! **Backward steps.** A track may step back within [`OWN_WINDOW_90K`] of its
//! newest timestamp and keep its offset: that is a B-frame source's
//! presentation order (the video is mapped by PTS, in decode order), and on
//! audio an overlap the output drops (`buffer_audio_frames`), keeping the
//! A/V relation. The cross window, though, takes a *backward* step only from
//! a track on an excursion it opened itself — audio stamped 60 s back that
//! comes back to the video's clock behind the audio just published. A track
//! on the programme's clock that steps back further than its own window is
//! a source jump: both tracks of a switch to a feed 1.5 s behind, a 2 s clip
//! looping from its start, used to pass as "within 3 s of the other track"
//! and publish 1.5 s backwards; the first track to see one now opens an
//! offset and the other takes it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

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
    /// The track is on an excursion it opened itself (it took a new
    /// offset), and has not yet come back to the offset the other track is
    /// on: only such a track may step back onto the other's clock through
    /// the cross window.
    excursion: bool,
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

/// The offsets a flow's CMAF outputs share.
#[derive(Debug)]
struct Offsets {
    /// Every offset in use, newest last.
    list: Vec<u64>,
    /// The offset the flow is on now: the one the output furthest along
    /// the (common) output timeline last moved onto, and where it did.
    /// An output behind its siblings that meets an old jump moves too, but
    /// at an earlier output time, so it does not take this over.
    current: u64,
    current_at: Option<u64>,
}

impl Default for Offsets {
    fn default() -> Self {
        Self { list: vec![0], current: 0, current_at: None }
    }
}

impl Offsets {
    /// A track moved onto `off` with its sample at output time `ts`.
    fn moved(&mut self, off: u64, ts: u64) {
        if self.current_at.is_none_or(|at| circ(ts, at) >= 0) {
            self.current = off;
            self.current_at = Some(ts);
        }
    }
}

type SharedOffsets = Arc<Mutex<Offsets>>;

/// Every flow's shared offsets, held weakly: the flow's outputs own them, so
/// a flow whose CMAF outputs have all stopped starts over at offset 0.
static FLOW_OFFSETS: OnceLock<Mutex<HashMap<String, Weak<Mutex<Offsets>>>>> = OnceLock::new();

/// Lock, recovering from poisoning: what sits behind these locks is a list
/// of numbers every one of which is a valid offset.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

#[derive(Debug)]
pub struct CmafTimeline {
    video: Line,
    audio: Line,
    offsets: SharedOffsets,
}

impl Default for CmafTimeline {
    /// A timeline of its own, shared with nothing (tests).
    fn default() -> Self {
        Self { video: Line::default(), audio: Line::default(), offsets: Arc::default() }
    }
}

impl CmafTimeline {
    /// A timeline for one CMAF output of `flow_id`, sharing its offsets with
    /// every other CMAF output of that flow (see the module doc).
    pub fn for_flow(flow_id: &str) -> Self {
        let mut flows = lock(FLOW_OFFSETS.get_or_init(Default::default));
        flows.retain(|_, w| w.strong_count() > 0);
        let offsets = match flows.get(flow_id).and_then(Weak::upgrade) {
            Some(o) => o,
            None => {
                let o: SharedOffsets = Arc::default();
                flows.insert(flow_id.to_string(), Arc::downgrade(&o));
                o
            }
        };
        Self { video: Line::default(), audio: Line::default(), offsets }
    }

    /// Map a source timestamp (90 kHz) of `track` onto the output timeline.
    pub fn map(&mut self, track: Track, src: u64) -> Mapped {
        let src = src & MASK_33;
        let (me, other) = match track {
            Track::Video => (&mut self.video, &self.audio),
            Track::Audio => (&mut self.audio, &self.video),
        };
        me.note_step(src);
        let Some(newest) = me.newest else {
            // A track's first timestamp takes the offset the flow is on: the
            // other track, or another output of the flow, may already have
            // moved off the source's clock.
            me.offset = lock(&self.offsets).current;
            let ts = (src + me.offset) & MASK_33;
            me.newest = Some(ts);
            return Mapped { ts, jump: None };
        };
        let other_newest = other.newest;
        let excursion = me.excursion;
        let continuous = |off: u64| -> Option<u64> {
            let ts = (src + off) & MASK_33;
            let own = circ(ts, newest);
            let own_ok = (-OWN_WINDOW_90K..=OWN_WINDOW_90K).contains(&own);
            // Onto the other track's clock: forwards (a track that paused
            // and came back), or back from an excursion this track opened.
            let cross_ok = (own > 0 || excursion)
                && other_newest.is_some_and(|o| circ(ts, o).abs() <= CROSS_WINDOW_90K);
            (own_ok || cross_ok).then_some(ts)
        };
        let own_offset = me.offset;
        let (offset, ts, jump) = match continuous(own_offset) {
            Some(ts) => (own_offset, ts, None),
            None => {
                let mut offsets = lock(&self.offsets);
                let found = offsets
                    .list
                    .iter()
                    .rev()
                    .copied()
                    .filter(|o| *o != own_offset)
                    .find_map(|off| continuous(off).map(|ts| (off, ts)));
                match found {
                    Some((off, ts)) => {
                        offsets.moved(off, ts);
                        (off, ts, None)
                    }
                    None => {
                        let default_step = match track {
                            Track::Video => 3_003,
                            Track::Audio => 1_920,
                        };
                        let ts = (newest + me.step(default_step)) & MASK_33;
                        let off = ts.wrapping_sub(src) & MASK_33;
                        offsets.list.retain(|o| *o != off);
                        offsets.list.push(off);
                        if offsets.list.len() > MAX_OFFSETS {
                            offsets.list.remove(0);
                        }
                        offsets.moved(off, ts);
                        me.excursion = true;
                        (off, ts, Some(circ((src + own_offset) & MASK_33, ts)))
                    }
                }
            }
        };
        me.offset = offset;
        if me.excursion && jump.is_none() && other.newest.is_some() && offset == other.offset {
            // Back on the offset the other track is on.
            me.excursion = false;
        }
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

    /// Two CMAF outputs of one flow publish one timeline, one of them
    /// started after the source jumped (an output restarted by a config
    /// edit to its bitrate): it lands on the offset its sibling absorbed,
    /// where it used to publish the source's raw timestamps, hours off, and
    /// the two re-anchored the flow's shared epoch on every segment.
    #[test]
    fn an_output_started_after_a_jump_publishes_its_siblings_timeline() {
        let flow = "timeline-test-restart";
        let mut a = CmafTimeline::for_flow(flow);
        let mut b = CmafTimeline::for_flow(flow);
        let (mut v, mut aud) = (900_000u64, 896_000u64);
        for _ in 0..30 {
            for t in [&mut a, &mut b] {
                t.map(Track::Video, v);
                t.map(Track::Audio, aud);
            }
            v += 3_600;
            aud += 1_920;
        }
        // An input switch to a feed on an unrelated clock.
        let jump = 20_000 * 90_000u64;
        let (mut v2, mut a2) = (v + jump, aud + jump);
        for _ in 0..30 {
            for t in [&mut a, &mut b] {
                t.map(Track::Video, v2);
                t.map(Track::Audio, a2);
            }
            v2 += 3_600;
            a2 += 1_920;
        }
        // `b` restarts.
        drop(b);
        let mut b = CmafTimeline::for_flow(flow);
        for _ in 0..30 {
            let (va, vb) = (a.map(Track::Video, v2), b.map(Track::Video, v2));
            let (aa, ab) = (a.map(Track::Audio, a2), b.map(Track::Audio, a2));
            assert_eq!(va, vb, "one video timeline");
            assert_eq!(aa, ab, "one audio timeline");
            assert!(circ(va.ts, v2) != 0, "on the absorbed offset, not the source's clock");
            v2 += 3_600;
            a2 += 1_920;
        }
        // A flow whose outputs have all stopped starts over.
        drop((a, b));
        let mut c = CmafTimeline::for_flow(flow);
        assert_eq!(c.map(Track::Video, v2).ts, v2);
        // Another flow is its own.
        let mut d = CmafTimeline::for_flow("timeline-test-other");
        assert_eq!(d.map(Track::Video, 123_456).ts, 123_456);
    }

    /// A jump one output meets first is taken up by the other as the same
    /// offset, even behind it by frames; and an output started while a
    /// sibling behind the others is still meeting an old excursion takes
    /// the offset the flow is on now, not the one that sibling just moved
    /// onto.
    #[test]
    fn two_outputs_take_one_offset_for_one_jump() {
        let flow = "timeline-test-lag";
        let mut a = CmafTimeline::for_flow(flow);
        let mut b = CmafTimeline::for_flow(flow);
        // Five frames stamped on another clock, then back.
        let src: Vec<u64> = (0..80u64)
            .map(|k| if (30..35).contains(&k) { 5_000_000_000 + k * 3_600 } else { 900_000 + k * 3_600 })
            .collect();
        let (mut out_a, mut out_b, mut out_c) = (Vec::new(), Vec::new(), Vec::new());
        let mut c: Option<CmafTimeline> = None;
        for i in 0..src.len() + 10 {
            if i < src.len() {
                out_a.push(a.map(Track::Video, src[i]).ts);
            }
            // `b` runs ten frames behind.
            if i >= 10 {
                out_b.push(b.map(Track::Video, src[i - 10]).ts);
            }
            // `c` starts live at frame 42, while `b` is inside the excursion.
            if (42..src.len()).contains(&i) {
                let c = c.get_or_insert_with(|| CmafTimeline::for_flow(flow));
                out_c.push(c.map(Track::Video, src[i]).ts);
            }
        }
        assert_eq!(out_a, out_b);
        assert!(out_a.windows(2).all(|w| circ(w[1], w[0]) == 3_600), "continuous across the excursion");
        assert_eq!(out_c[..], out_a[42..], "the output started late lands on the flow's offset");
    }

    /// Both tracks jumping 1.5 s back together (a switch to a feed that far
    /// behind) is a jump, not a step back onto the other track's clock:
    /// neither track publishes backwards, and the A/V relation is kept. It
    /// used to pass the cross window ("within 3 s of the other track") and
    /// publish 1.5 s back on both tracks.
    #[test]
    fn a_synchronised_backward_jump_does_not_go_backwards() {
        for back in [135_000u64, 180_000, 250_000] {
            let mut tl = CmafTimeline::default();
            let (mut v, mut a) = (900_000u64, 896_000u64);
            let mut vo = Vec::new();
            let mut ao = Vec::new();
            for k in 0..150u64 {
                if k == 75 {
                    v -= back;
                    a -= back;
                }
                vo.push(tl.map(Track::Video, v).ts);
                ao.push(tl.map(Track::Audio, a).ts);
                v += 3_600;
                a += 3_600;
            }
            assert!(vo.windows(2).all(|w| circ(w[1], w[0]) > 0), "{back}: video forward");
            assert!(ao.windows(2).all(|w| circ(w[1], w[0]) > 0), "{back}: audio forward");
            assert!(
                vo.iter().zip(&ao).all(|(v, a)| circ(*v, *a) == 4_000),
                "{back}: the source's A/V relation throughout"
            );
        }
    }

    /// A 2 s clip looping from its own start, over and over.
    #[test]
    fn a_short_clip_looping_from_its_start_runs_on() {
        let mut tl = CmafTimeline::default();
        let mut vo = Vec::new();
        let mut ao = Vec::new();
        for lap in 0..6u64 {
            for k in 0..50u64 {
                vo.push(tl.map(Track::Video, 900_000 + k * 3_600).ts);
                ao.push(tl.map(Track::Audio, 900_000 + k * 3_600 + 1_000).ts);
            }
            let _ = lap;
        }
        assert!(vo.windows(2).all(|w| circ(w[1], w[0]) == 3_600), "video one frame a step");
        assert!(ao.windows(2).all(|w| circ(w[1], w[0]) == 3_600), "audio one step a step");
        assert!(vo.iter().zip(&ao).all(|(v, a)| circ(*a, *v) == 1_000));
    }

    /// The loop excursion above with the audio coming back 1.1 s behind the
    /// audio just published — past its own window, back through the cross
    /// window onto the video's clock (the excursion was its own).
    #[test]
    fn audio_back_from_its_own_excursion_rejoins_the_video_past_its_window() {
        let mut tl = CmafTimeline::default();
        let mut v = 3_168_500_000u64;
        let mut a = v - 4_000;
        for _ in 0..50 {
            tl.map(Track::Video, v);
            tl.map(Track::Audio, a);
            v += 3_003;
            a += 1_920;
        }
        // 1.2 s of audio stamped 60 s back.
        let excursion = a - 60 * 90_000;
        for k in 0..56u64 {
            tl.map(Track::Audio, excursion + k * 1_920);
        }
        for _ in 0..5 {
            assert_eq!(tl.map(Track::Video, v).ts, v);
            v += 3_003;
        }
        let back = a + 5 * 3_003;
        let m = tl.map(Track::Audio, back);
        assert_eq!(m, Mapped { ts: back, jump: None }, "back on the source's clock");
        assert_eq!(tl.map(Track::Audio, back + 1_920).ts, back + 1_920);
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
