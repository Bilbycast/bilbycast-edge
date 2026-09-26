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
//! every segment. A track's first timestamp takes the offset the flow's
//! *programme* is on — the one the output furthest along last moved both
//! its tracks onto (its only track, for a single-track output) — so a
//! restarted output lands where its siblings are, and a jump one output
//! meets first is taken up by the others as theirs. One track's excursion
//! moves no programme: an output started while a sibling's audio sat 60 s
//! back used to take that audio's offset for its first picture and publish
//! the video 60 s off its sibling. A second track whose first timestamp the
//! programme's offset does not put near the first track's (audio stamped
//! 60 s back, met by an output started inside the excursion) takes the
//! offset that does, as its siblings did.
//!
//! **The cross window is directed.** A track may step back within
//! [`OWN_WINDOW_90K`] of its newest timestamp and keep its offset: that is a
//! B-frame source's presentation order (the video is mapped by PTS, in
//! decode order), and on audio an overlap the output drops
//! (`buffer_audio_frames`), keeping the A/V relation. Past its own window a
//! track on its programme's offset takes the cross window only forwards — a
//! track that paused and came back. A track on an offset of its own (a jump
//! it met first, an excursion) takes it only onto the offset the other
//! track is on, in either direction: audio stamped 60 s back comes back to
//! the video's clock behind the audio just published, and one picture
//! stamped 2 s back comes back with the next picture, where it used to stay
//! on the offset it opened, 2 s ahead of the audio for good. "Its own" is a
//! matter of offsets, not of which output opened one: a sibling output that
//! found an excursion's offset rather than opening it comes back from it
//! the same way (it used to open a bogus offset of its own, its audio 1 s
//! off its sibling's). A track on the other's offset that steps back further
//! than its own window is a source jump: both tracks of a switch to a feed
//! 1.5 s behind, a 2 s clip looping from its start, used to pass as "within
//! 3 s of the other track" and publish 1.5 s backwards; the first track to
//! see one now opens an offset and the other takes it. A track goes on from
//! where a move lands it, so a return well past its own window is measured
//! from the return, not from the excursion's newest.

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
    /// The offset the flow's programme is on now: the one the output
    /// furthest along the (common) output timeline last moved its
    /// programme onto — both its tracks, or its only one — and where it
    /// did. An output behind its siblings that meets an old jump moves
    /// too, but at an earlier output time, so it does not take this over;
    /// and one track on an excursion of its own moves no programme.
    current: u64,
    current_at: Option<u64>,
}

impl Default for Offsets {
    fn default() -> Self {
        Self { list: vec![0], current: 0, current_at: None }
    }
}

impl Offsets {
    /// An output's programme moved onto `off` with the sample at output
    /// time `ts`.
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
    /// The offset this output's programme is on: the one both its tracks
    /// were last on together (its only track's, before the other has a
    /// timestamp). A track on another offset is on an excursion — a jump
    /// it met first, or an excursion of its own.
    programme: Option<u64>,
    offsets: SharedOffsets,
}

impl Default for CmafTimeline {
    /// A timeline of its own, shared with nothing (tests).
    fn default() -> Self {
        Self {
            video: Line::default(),
            audio: Line::default(),
            programme: None,
            offsets: Arc::default(),
        }
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
        Self { video: Line::default(), audio: Line::default(), programme: None, offsets }
    }

    /// Map a source timestamp (90 kHz) of `track` onto the output timeline.
    pub fn map(&mut self, track: Track, src: u64) -> Mapped {
        let src = src & MASK_33;
        let (me, other) = match track {
            Track::Video => (&mut self.video, &self.audio),
            Track::Audio => (&mut self.audio, &self.video),
        };
        me.note_step(src);
        let near_other =
            |ts: u64| other.newest.is_some_and(|o| circ(ts, o).abs() <= CROSS_WINDOW_90K);
        let Some(newest) = me.newest else {
            // A track's first timestamp takes the offset its programme is
            // on — the other track's, or on this output's first timestamp
            // the one the flow is on: another output of the flow may
            // already have moved off the source's clock. When that does not
            // put it on the other track's clock it is on an excursion of
            // its own (audio stamped 60 s back, met by an output started
            // inside it), which another output may be on already.
            me.offset = match self.programme {
                Some(p) if other.newest.is_some() && !near_other((src + p) & MASK_33) => {
                    let offsets = lock(&self.offsets);
                    let near = |off: &u64| near_other((src + off) & MASK_33);
                    offsets.list.iter().rev().copied().find(near).unwrap_or(p)
                }
                Some(p) => p,
                None => lock(&self.offsets).current,
            };
            let ts = (src + me.offset) & MASK_33;
            me.newest = Some(ts);
            if other.newest.is_none() || me.offset == other.offset {
                self.programme = Some(me.offset);
            }
            return Mapped { ts, jump: None };
        };
        let own_offset = me.offset;
        let other_offset = other.newest.map(|_| other.offset);
        // On an offset the programme is not on: a jump this track met first,
        // or an excursion of its own.
        let on_excursion = self.programme.is_some_and(|p| p != own_offset);
        let continuous = |off: u64| -> Option<u64> {
            let ts = (src + off) & MASK_33;
            let own = circ(ts, newest);
            if (-OWN_WINDOW_90K..=OWN_WINDOW_90K).contains(&own) {
                return Some(ts);
            }
            let onto_other = other_offset == Some(off);
            // The cross window: onto the other track's clock. Forwards, a
            // track that paused and came back — onto any offset from the
            // programme's, only onto the other track's from an excursion
            // (an offset one picture 2 s back opened would take the video's
            // return 2 s ahead of the audio for good). Backwards, only back
            // onto the other track's offset from another: audio stamped
            // 60 s back returning behind the audio just published. A track
            // on the other's offset stepping back past its own window is a
            // jump — both tracks of a switch to a feed 1.5 s behind.
            let cross_ok = near_other(ts)
                && if own > 0 {
                    onto_other || !on_excursion
                } else {
                    onto_other && Some(own_offset) != other_offset
                };
            cross_ok.then_some(ts)
        };
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
                    Some((off, ts)) => (off, ts, None),
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
                        (off, ts, Some(circ((src + own_offset) & MASK_33, ts)))
                    }
                }
            }
        };
        // A track that stepped back through the cross window (a return from
        // an excursion) goes on from where it landed: the samples after it
        // are measured from there, not from the excursion's newest — a
        // return more than a frame past its own window used to open an
        // offset of its own one sample later. The overlap is the output's to
        // drop (`buffer_audio_frames`). Any other step back keeps the newest,
        // so an offset opened later still continues past everything out.
        let own = circ(ts, newest);
        if own > 0 || (offset != own_offset && own < -OWN_WINDOW_90K) {
            me.newest = Some(ts);
        }
        me.offset = offset;
        // The programme moves when both tracks are on one offset (or the
        // only track moves), and the flow's with it: a track on an excursion
        // of its own moves neither, so an output started meanwhile does not
        // take the excursion for its programme.
        if (other_offset.is_none() || other_offset == Some(offset)) && self.programme != Some(offset) {
            if self.programme.is_some() {
                lock(&self.offsets).moved(offset, ts);
            }
            self.programme = Some(offset);
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

    /// One source step of a programme: video at 25 fps, audio two 20 ms
    /// frames a picture, 4 000 ticks behind the video.
    fn av(k: u64) -> (u64, [u64; 2]) {
        let v = 900_000 + k * 3_600;
        let a = v - 4_000;
        (v, [a, a + 1_800])
    }

    /// A single picture stamped 2 s behind the rest (a PES on the wrong
    /// clock) opens an offset of its own for that picture, and the video
    /// comes straight back to the programme's clock with the next one. It
    /// used to stay on the picture's offset — 2 s ahead of the audio for
    /// good — because the step back onto the source's clock fitted the
    /// forward cross window of an offset only the video was on.
    #[test]
    fn a_single_picture_two_seconds_back_leaves_the_video_on_the_programmes_clock() {
        let mut tl = CmafTimeline::default();
        let mut vo = Vec::new();
        for k in 0..120u64 {
            let (v, a) = av(k);
            let src = if k == 50 { v - 180_000 } else { v };
            let m = tl.map(Track::Video, src);
            vo.push(m.ts);
            if k != 50 {
                assert_eq!(m.ts, v, "picture {k} on the source's clock");
            }
            for a in a {
                assert_eq!(tl.map(Track::Audio, a).ts, a, "audio at {k} untouched");
            }
        }
        assert!(vo.windows(2).all(|w| circ(w[1], w[0]) > 0), "the video never steps back");
    }

    /// The same for about half a second of audio stamped 2 s back: the audio
    /// comes back to the video's clock when the source does, where it used
    /// to publish 2 s ahead of the video from then on.
    #[test]
    fn half_a_second_of_audio_two_seconds_back_leaves_the_audio_on_the_programmes_clock() {
        let mut tl = CmafTimeline::default();
        let mut ao = Vec::new();
        for k in 0..120u64 {
            let (v, a) = av(k);
            assert_eq!(tl.map(Track::Video, v).ts, v, "video at {k} untouched");
            for a in a {
                let stale = (50..63).contains(&k);
                let m = tl.map(Track::Audio, if stale { a - 180_000 } else { a });
                ao.push(m.ts);
                if k >= 63 {
                    assert_eq!(m.ts, a, "audio at {k} back on the source's clock");
                }
            }
        }
        assert!(ao.windows(2).all(|w| circ(w[1], w[0]).abs() <= OWN_WINDOW_90K), "no audio jump");
    }

    /// The media_player loop's audio excursion (1.2 s of audio stamped 60 s
    /// back, then the audio back 1.1 s behind the audio just published, past
    /// its own window) through two CMAF outputs of one flow in lockstep —
    /// a DVR main and its all-intra proxy. Both come back to the video's
    /// clock and publish one timeline. The output that met the excursion
    /// second found its offset rather than opening it, was not taken as on
    /// an excursion, could not step back onto the video's clock, and opened
    /// an offset of its own: its audio 1.03 s off its sibling's and off its
    /// own video, for good.
    #[test]
    fn two_outputs_bring_the_audio_back_from_one_excursion_together() {
        let flow = "timeline-test-audio-excursion";
        let mut a = CmafTimeline::for_flow(flow);
        let mut b = CmafTimeline::for_flow(flow);
        let mut v = 3_168_500_000u64;
        let mut aud = v - 4_000;
        let mut both = |t: Track, src: u64| {
            let (ma, mb) = (a.map(t, src), b.map(t, src));
            assert_eq!(ma.ts, mb.ts, "one timeline for {t:?} at {src}");
            // The output that met a jump second takes it up as the same
            // offset: only the first names it.
            if ma.jump.is_none() {
                assert_eq!(mb.jump, None, "{t:?} at {src}");
            }
            ma
        };
        for _ in 0..50 {
            both(Track::Video, v);
            both(Track::Audio, aud);
            v += 3_003;
            aud += 1_920;
        }
        let excursion = aud - 60 * 90_000;
        for k in 0..56u64 {
            both(Track::Audio, excursion + k * 1_920);
        }
        for _ in 0..5 {
            assert_eq!(both(Track::Video, v).ts, v);
            v += 3_003;
        }
        let back = aud + 5 * 3_003;
        assert_eq!(both(Track::Audio, back), Mapped { ts: back, jump: None }, "back on the source's clock");
        for k in 1..20 {
            assert_eq!(both(Track::Audio, back + k * 1_920).ts, back + k * 1_920);
        }
    }

    /// An output (re)started while a sibling's audio is on a single-track
    /// excursion lands on the programme's timeline — its video where the
    /// sibling's video is, its audio where the sibling's audio is — where
    /// its first picture used to take the excursion's offset (the flow's
    /// `current` followed any one track's move) and publish 60 s off its
    /// sibling: the shared epoch re-anchored on every segment.
    #[test]
    fn an_output_started_during_an_audio_excursion_lands_on_the_programme() {
        let flow = "timeline-test-restart-in-excursion";
        let mut a = CmafTimeline::for_flow(flow);
        let mut v = 3_168_500_000u64;
        let mut aud = v - 4_000;
        for _ in 0..50 {
            a.map(Track::Video, v);
            a.map(Track::Audio, aud);
            v += 3_003;
            aud += 1_920;
        }
        let excursion = aud - 60 * 90_000;
        for k in 0..20u64 {
            a.map(Track::Audio, excursion + k * 1_920);
        }
        // `c` starts here, a third of the way into the excursion.
        let mut c = CmafTimeline::for_flow(flow);
        let (va, vc) = (a.map(Track::Video, v), c.map(Track::Video, v));
        assert_eq!(va, vc, "the restarted output's video is its sibling's");
        assert_eq!(vc.ts, v, "on the programme's clock");
        v += 3_003;
        for k in 20..56u64 {
            let src = excursion + k * 1_920;
            let (ma, mc) = (a.map(Track::Audio, src), c.map(Track::Audio, src));
            assert_eq!(ma.ts, mc.ts, "one audio timeline in the excursion");
        }
        for _ in 0..5 {
            assert_eq!(a.map(Track::Video, v), c.map(Track::Video, v));
            v += 3_003;
        }
        let back = aud + 5 * 3_003;
        for k in 0..20 {
            let src = back + k * 1_920;
            let (ma, mc) = (a.map(Track::Audio, src), c.map(Track::Audio, src));
            assert_eq!(ma, mc, "one audio timeline after it");
            assert_eq!(mc, Mapped { ts: src, jump: None }, "back on the source's clock");
        }
    }

    /// Both tracks meet one excursion (audio, then a few pictures, stamped
    /// 60 s back) and so share its offset — the programme's, for as long
    /// as it lasts. The video comes back first; the audio comes back 1.04 s
    /// behind the audio just published, more than a frame past its own
    /// window, steps back onto the video's clock — the offset it is on is
    /// not the other track's any more — and goes on from there. Measured
    /// from the excursion's newest, the sample after the return opened an
    /// offset of its own.
    #[test]
    fn audio_back_from_an_excursion_both_tracks_took_rejoins_the_video() {
        let mut tl = CmafTimeline::default();
        let mut v = 3_168_500_000u64;
        let mut aud = v - 4_000;
        for _ in 0..50 {
            tl.map(Track::Video, v);
            tl.map(Track::Audio, aud);
            v += 3_003;
            aud += 1_920;
        }
        let excursion = aud - 60 * 90_000;
        for k in 0..56u64 {
            tl.map(Track::Audio, excursion + k * 1_920);
        }
        for k in 0..3u64 {
            let m = tl.map(Track::Video, v - 60 * 90_000 + k * 3_003);
            assert_eq!(m.ts, v + k * 3_003, "on the excursion");
        }
        v += 3 * 3_003;
        for _ in 0..5 {
            assert_eq!(tl.map(Track::Video, v).ts, v, "the video back");
            v += 3_003;
        }
        // 1.04 s behind the audio just published: past its own window.
        let back = aud + 4 * 3_003;
        assert_eq!(tl.map(Track::Audio, back), Mapped { ts: back, jump: None }, "the audio back");
        assert_eq!(tl.map(Track::Audio, back + 1_920).ts, back + 1_920);
    }

    /// An audio gap that ends while the video is on a one-picture excursion
    /// of its own is still a gap on the programme's clock, kept in sync:
    /// the audio's offset is the programme's even though the video, for
    /// that picture, is on another.
    #[test]
    fn an_audio_gap_ending_inside_a_video_excursion_stays_a_gap() {
        let mut tl = CmafTimeline::default();
        let (mut v, mut a) = (900_000u64, 896_000u64);
        for _ in 0..30 {
            tl.map(Track::Video, v);
            tl.map(Track::Audio, a);
            tl.map(Track::Audio, a + 1_800);
            v += 3_600;
            a += 3_600;
        }
        for _ in 0..50 {
            tl.map(Track::Video, v);
            v += 3_600;
            a += 3_600;
        }
        assert!(tl.map(Track::Video, (v + LAP_90K - 16_543 * 90_000) & MASK_33).jump.is_some());
        assert_eq!(tl.map(Track::Audio, a), Mapped { ts: a, jump: None }, "the gap kept");
        v += 3_600;
        assert_eq!(tl.map(Track::Video, v).ts, v, "the video back");
        assert_eq!(tl.map(Track::Audio, a + 1_800).ts, a + 1_800);
    }
}
