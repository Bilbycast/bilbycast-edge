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
//! track's newest where the offsets and the other track allow it (below; a
//! track that paused while the other ran on and came back on the
//! programme's clock is a gap, and stays one, in sync). Otherwise the
//! offsets already in use are tried — the other track may have met the same
//! jump first, or the source may be coming back from a short excursion —
//! and only when none makes it continuous does a new offset place it one
//! step (the track's smallest recent step) after the track's newest
//! timestamp.
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
//! the video 60 s off its sibling. A track whose sibling has gone quiet —
//! no timestamp while this one came a cross window, the audio of a switch
//! to a video-only backup — moves the programme alone: the flow's offset
//! used to stay where both tracks last were, so an output started after the
//! video met a jump on its own published it hours off its sibling. A second
//! track whose first timestamp the programme's offset does not put near the
//! first track's (audio stamped 60 s back, met by an output started inside
//! the excursion) takes the offset that does, as its siblings did.
//!
//! **The cross window is directed.** A track may step back within
//! [`OWN_WINDOW_90K`] of its newest timestamp and keep its offset: that is a
//! B-frame source's presentation order (the video is mapped by PTS, in
//! decode order), and on audio an overlap the output drops
//! (`buffer_audio_frames`), keeping the A/V relation. Past its own window:
//!
//! - Onto the offset the other track is on, from another — the other met
//!   this jump first, or this track is coming back from an excursion — a
//!   track may step forwards (a gap in it), and the audio backwards too:
//!   audio stamped 60 s back comes back to the video's clock behind the
//!   audio just published. The other track's offset is tried before a step
//!   of the track's own: of both tracks jumping 2 s forward with the video
//!   ahead in the mux, the video's step past the cross window opened an
//!   offset and the audio's own step, still near the video, used to keep
//!   its gap — A/V 2 s apart for good. "Its own" is a matter of offsets, not
//!   of which output opened one: a sibling output that found an excursion's
//!   offset comes back from it the same way.
//! - The video never steps back past its own window: nothing drops a
//!   picture, and one behind a picture already published is a zero-length
//!   sample and an overlapping `tfdt`. An input switch the audio met first,
//!   from a feed whose video ran 1.1 s ahead of its audio to a level one,
//!   stepped the video back 1.04 s onto the audio's new offset. The video
//!   takes a new offset instead, and with both tracks off the programme on
//!   different offsets, the one behind joins the other: the new feed is
//!   published in its own A/V relation.
//! - On the programme's offset, a step forwards within the cross window is a
//!   gap when the other track ran through it (a pause of this track) or
//!   leapt too (a jump both meet). Otherwise it is in doubt (`Doubt`): one
//!   picture or half a second of audio stamped 2 s ahead, and a jump of the
//!   source's that the other track has yet to meet, look alike until the
//!   track's next timestamps. The video places the picture one step on, and
//!   goes on with the gap only when its next picture lands on as that one
//!   did; the audio takes the gap, and steps back to where it was when the
//!   source does, in its old relation to the video. A stray timestamp 2 s
//!   ahead used to be taken as a gap, and its track published 2 s off the
//!   other for good.
//! - A track on the other's offset that steps back further than its own
//!   window is a source jump: both tracks of a switch to a feed 1.5 s
//!   behind, a 2 s clip looping from its start, used to pass as "within 3 s
//!   of the other track" and publish 1.5 s backwards; the first track to see
//!   one now opens an offset and the other track takes it.
//!
//! A track goes on from where a move lands it, so a return well past its
//! own window is measured from the return, not from the excursion's newest.

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
    /// The furthest output timestamp the track has reached, and how far
    /// (90 kHz) it has come in all: its progress, which audio stepping back
    /// over audio it already sent does not count twice.
    reach: Option<u64>,
    advance: u64,
    /// The other track's `advance` when this track last had a timestamp: how
    /// far the other has come since is what a step of this track's past its
    /// own window is measured against (a pause the other ran through), and
    /// how far this track has come since the other last had one says whether
    /// the other has gone quiet.
    other_at_last: u64,
    /// How many times the source has stepped this track forward further than
    /// any frame, and the other track's count when this one last had a
    /// timestamp: the other leaping meanwhile shows a step of this track's
    /// past its own window to be a jump of the source's, which both meet.
    leaps: u64,
    other_leaps: u64,
    /// A step past this track's own window that nothing yet shows to be the
    /// source's (see [`Doubt`]).
    doubt: Option<Doubt>,
}

/// A step of one track forward past its own window, on the programme's
/// offset and near the other track, that the other track neither ran
/// through (as it would a pause of this one) nor met (it has not leapt).
/// It is a jump of the source's that the other track is about to meet — an
/// outage of both, a switch to a feed on the same clock with another video
/// lead — or a stray timestamp — one picture, or half a second of audio,
/// stamped 2 s ahead — and only the track's next timestamps tell which.
/// Taken as a jump the first leaves the track off the other by as much as
/// the tracks' steps differ; taken as a gap the second leaves it 2 s ahead
/// of the other for good, as it used to.
///
/// The video, which cannot step back, places the picture one step on (an
/// offset of its own) and goes on to the programme's offset with the gap only
/// when its next picture lands on as the first did, with the audio still
/// there; a single picture stamped ahead is then passed without a trace. The
/// audio, whose overlap the output drops (`buffer_audio_frames`), takes the
/// gap, and steps back to where it was when the source comes back in the
/// relation to the video it had before.
#[derive(Debug, Clone, Copy)]
struct Doubt {
    /// The offset the step was on, and where on it the timestamp landed.
    offset: u64,
    landed: u64,
    /// The track's newest less the other's before the step: the relation a
    /// return of the audio finds again.
    relation: i64,
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

    /// The track's output reached `ts`: its progress goes on by however far
    /// that is past anything it reached before.
    fn reached(&mut self, ts: u64) {
        match self.reach {
            Some(r) if circ(ts, r) <= 0 => {}
            Some(r) => {
                self.advance = self.advance.saturating_add(circ(ts, r) as u64);
                self.reach = Some(ts);
            }
            None => self.reach = Some(ts),
        }
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
        let programme = self.programme;
        let (me, other) = match track {
            Track::Video => (&mut self.video, &mut self.audio),
            Track::Audio => (&mut self.audio, &mut self.video),
        };
        let leapt = me.last_src.is_some_and(|prev| circ(src, prev) > MAX_STEP_90K as i64);
        let other_leapt = other.leaps > me.other_leaps;
        me.note_step(src);
        // How far the other track has come since this one last had a
        // timestamp; and whether it has gone quiet — this track has come a
        // cross window since the other last had one (the audio of a switch
        // to a video-only backup).
        let other_ran = other.advance.saturating_sub(me.other_at_last);
        let other_quiet = other.newest.is_some()
            && me.advance.saturating_sub(other.other_at_last) > CROSS_WINDOW_90K as u64;
        let other_newest = other.newest;
        let near_other = |ts: u64| other_newest.is_some_and(|o| circ(ts, o).abs() <= CROSS_WINDOW_90K);
        let Some(newest) = me.newest else {
            // A track's first timestamp takes the offset its programme is
            // on — the other track's, or on this output's first timestamp
            // the one the flow is on: another output of the flow may
            // already have moved off the source's clock. When that does not
            // put it on the other track's clock it is on an excursion of
            // its own (audio stamped 60 s back, met by an output started
            // inside it), which another output may be on already.
            me.offset = match programme {
                Some(p) if other_newest.is_some() && !near_other((src + p) & MASK_33) => {
                    let offsets = lock(&self.offsets);
                    let near = |off: &u64| near_other((src + off) & MASK_33);
                    offsets.list.iter().rev().copied().find(near).unwrap_or(p)
                }
                Some(p) => p,
                None => lock(&self.offsets).current,
            };
            let ts = (src + me.offset) & MASK_33;
            me.newest = Some(ts);
            me.reached(ts);
            me.other_at_last = other.advance;
            me.other_leaps = other.leaps;
            if other_newest.is_none() || me.offset == other.offset {
                self.programme = Some(me.offset);
            }
            return Mapped { ts, jump: None };
        };
        let own_offset = me.offset;
        let other_offset = other_newest.map(|_| other.offset);
        // The other track's offset, unless it is there only in doubt (a
        // picture placed one step on until its step shows real).
        let other_settled = other_offset.filter(|_| other.doubt.is_none());
        let on_programme = programme == Some(own_offset);
        let doubt = me.doubt.take();
        let in_own = |own: i64| (-OWN_WINDOW_90K..=OWN_WINDOW_90K).contains(&own);
        // Past its own window, onto the offset the other track is on, from
        // another: the other met this jump first, or this track is coming
        // back from an excursion. Forwards, a gap in this track. Backwards
        // only the audio — audio stamped 60 s back returning behind the
        // audio just published, an overlap the output drops
        // (`buffer_audio_frames`). Nothing drops a picture: behind one
        // already published it is a zero-length sample and an overlapping
        // `tfdt`, so the video takes a new offset instead.
        let joins = |off: u64, ts: u64, own: i64| {
            near_other(ts)
                && other_offset == Some(off)
                && other_offset != Some(own_offset)
                && (own > 0 || track == Track::Audio)
        };
        let fits = |off: u64| -> Option<u64> {
            let ts = (src + off) & MASK_33;
            let own = circ(ts, newest);
            (in_own(own) || joins(off, ts, own)).then_some(ts)
        };
        // Among the offsets in use, also forwards from the programme's onto
        // one neither track is on: the source back on a clock it left, with
        // a gap.
        let fits_listed = |off: u64| -> Option<u64> {
            let ts = (src + off) & MASK_33;
            let own = circ(ts, newest);
            fits(off).or((own > 0 && on_programme && near_other(ts)).then_some(ts))
        };
        let mut new_doubt = None;
        let mut placing_in_doubt = false;
        let mut returned = false;
        let chosen: Option<(u64, u64)> = 'choose: {
            match doubt {
                // The picture after one placed in doubt: landing on as that
                // one did, with the audio still on the offset, the step was
                // the source's, and the video goes on there with the gap.
                Some(d) if track == Track::Video => {
                    let ts = (src + d.offset) & MASK_33;
                    let from_landed = circ(ts, d.landed).abs();
                    if other_offset == Some(d.offset)
                        && from_landed <= OWN_WINDOW_90K
                        && circ(ts, newest) > 0
                    {
                        break 'choose Some((d.offset, ts));
                    }
                }
                // The audio after a step taken in doubt: the source back
                // where the audio was, and in the relation to the video it
                // had then, the audio steps back there. A switch back to
                // where the audio was is no return when the video meets it
                // too: the video, which cannot step back with it, moved on.
                Some(d) if track == Track::Audio && d.offset == own_offset => {
                    let ts = (src + own_offset) & MASK_33;
                    let related =
                        other_newest.is_some_and(|o| (circ(ts, o) - d.relation).abs() <= OWN_WINDOW_90K);
                    if circ(ts, newest) < -OWN_WINDOW_90K && related {
                        returned = true;
                        break 'choose Some((own_offset, ts));
                    }
                    new_doubt = Some(d);
                }
                _ => {}
            }
            // Both tracks off the programme, on different offsets: they met
            // one jump and came out of it apart — the audio met an input
            // switch first and opened an offset the video could only have
            // taken by stepping back, so the video opened its own. The track
            // behind joins the other (a gap in it), and the programme forms
            // again on one offset, in the new feed's A/V relation.
            if let (Some(p), Some(o)) = (programme, other_settled)
                && !other_quiet
                && o != own_offset
                && o != p
                && own_offset != p
                && let Some(ts) = fits(o)
            {
                break 'choose Some((o, ts));
            }
            let ts = (src + own_offset) & MASK_33;
            let own = circ(ts, newest);
            if in_own(own) {
                break 'choose Some((own_offset, ts));
            }
            // The other track may have met this jump first: onto its offset
            // before any step of this track's own past its window. With the
            // video ahead of the audio in the mux, both tracks jumping
            // forward 2 s put the video on a new offset, and the audio's own
            // step, 2 s but near the video, used to keep its gap: A/V apart
            // by the jump for good.
            if let Some(o) = other_settled.filter(|o| *o != own_offset)
                && let Some(ts) = fits(o)
            {
                break 'choose Some((o, ts));
            }
            // Forwards past its own window on the programme's offset: a pause
            // of this track that the other ran through, or a jump the other
            // met too, is a gap. Anything else is in doubt (see `Doubt`).
            if own > OWN_WINDOW_90K && on_programme && near_other(ts) {
                let ran = i64::try_from(other_ran).unwrap_or(i64::MAX).saturating_add(OWN_WINDOW_90K);
                if other_leapt || own <= ran {
                    break 'choose Some((own_offset, ts));
                }
                let relation = other_newest.map_or(0, |o| circ(newest, o));
                new_doubt = Some(Doubt { offset: own_offset, landed: ts, relation });
                if track == Track::Audio {
                    break 'choose Some((own_offset, ts));
                }
                placing_in_doubt = true;
            }
            fits(own_offset).map(|ts| (own_offset, ts))
        };
        let (offset, ts, jump) = match chosen {
            Some((off, ts)) => (off, ts, None),
            None => {
                let mut offsets = lock(&self.offsets);
                // Not for a picture placed in doubt: its step is forward on
                // the programme's clock, and an old offset that happens to
                // take it would put it back on a clock the source has left.
                let found = offsets
                    .list
                    .iter()
                    .rev()
                    .copied()
                    .filter(|o| *o != own_offset && !placing_in_doubt)
                    .find_map(|off| fits_listed(off).map(|ts| (off, ts)));
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
        // an excursion, or from a step taken in doubt) goes on from where it
        // landed: the samples after it are measured from there, not from the
        // excursion's newest — a return more than a frame past its own
        // window used to open an offset of its own one sample later. The
        // overlap is the output's to drop (`buffer_audio_frames`). Any other
        // step back keeps the newest, so an offset opened later still
        // continues past everything out.
        let own = circ(ts, newest);
        if own > 0 || (offset != own_offset && own < -OWN_WINDOW_90K) || returned {
            me.newest = Some(ts);
        }
        me.reached(ts);
        me.offset = offset;
        me.other_at_last = other.advance;
        me.leaps += u64::from(leapt);
        me.other_leaps = other.leaps;
        // A doubt lasts while its track stays where it was taken: the
        // video's for its next picture, the audio's while it is on the
        // step's offset.
        me.doubt = new_doubt.filter(|d| track == Track::Video || d.offset == offset);
        // The programme moves when both tracks are on one offset, or the
        // track moves with no other beside it — the output has only this
        // one, or the other has gone quiet — and the flow's with it. A track
        // on an excursion of its own moves neither, so an output started
        // meanwhile does not take the excursion for its programme; but an
        // output whose audio has stopped follows its video onto a new feed,
        // or an output started then would publish the video on the old one.
        let alone = other_offset.is_none() || other_quiet;
        if (alone || other_offset == Some(offset)) && self.programme != Some(offset) {
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

    /// A feed on the wire: `frames` pictures at 25 fps and the audio beside
    /// them in 1 920-tick frames, stamped `base` plus the arrival time, the
    /// video `lead` ticks ahead of the audio in the mux.
    #[derive(Clone, Copy)]
    struct Feed {
        frames: u64,
        base: i64,
        lead: i64,
    }

    /// Every timestamp of `feeds`, one feed after another, as it arrives —
    /// the audio first at a tie when `audio_first` — with its feed.
    fn wire(feeds: &[Feed], audio_first: bool) -> Vec<(Track, u64, usize)> {
        let mut ev = Vec::new();
        let mut t0 = 0i64;
        for (i, f) in feeds.iter().enumerate() {
            let len = f.frames as i64 * 3_600;
            for t in (0..len).step_by(3_600) {
                ev.push((t0 + t, Track::Video, (f.base + t0 + t + f.lead) as u64 & MASK_33, i));
            }
            for t in (0..len).step_by(1_920) {
                ev.push((t0 + t, Track::Audio, (f.base + t0 + t) as u64 & MASK_33, i));
            }
            t0 += len;
        }
        ev.sort_by_key(|&(t, track, _, _)| (t, (track == Track::Video) == audio_first));
        ev.into_iter().map(|(_, track, src, feed)| (track, src, feed)).collect()
    }

    /// `wire` through a timeline: (track, source, output, feed).
    fn play(tl: &mut CmafTimeline, feeds: &[Feed], audio_first: bool) -> Vec<(Track, u64, u64, usize)> {
        wire(feeds, audio_first)
            .into_iter()
            .map(|(track, src, feed)| (track, src, tl.map(track, src).ts, feed))
            .collect()
    }

    /// Every picture after the one before it.
    fn video_forward(out: &[(Track, u64, u64, usize)]) -> bool {
        let v: Vec<u64> = out.iter().filter(|o| o.0 == Track::Video).map(|o| o.2).collect();
        v.windows(2).all(|w| circ(w[1], w[0]) > 0)
    }

    /// The last picture's and the last audio's offsets from their source
    /// (equal: the last feed's A/V relation is published as it came).
    fn last_offsets(out: &[(Track, u64, u64, usize)]) -> (i64, i64) {
        let last = |track| out.iter().rev().find(|o| o.0 == track).map(|o| circ(o.2, o.1)).unwrap();
        (last(Track::Video), last(Track::Audio))
    }

    /// An input switch the audio meets first, from a feed whose video runs
    /// 1.1 s ahead of its audio in the mux (the witness encoder) to one on
    /// another clock whose tracks are level. The audio opens an offset
    /// continuing its track; on it the video would land 1.04 s behind the
    /// picture before, and it used to take it — the published video stepping
    /// back 1.04 s, a zero-length sample and an overlapping `tfdt`, with
    /// nothing to drop the overlap as the audio's is. The video continues
    /// its own track instead, and the audio, behind, joins it: the new feed
    /// is published in its own A/V relation. (A lead under 1 s lands the
    /// video within its own window, where a step back cannot be told from
    /// B-frame reordering, and is taken as before.)
    #[test]
    fn a_switch_the_audio_meets_first_never_steps_the_video_back() {
        for lead in [99_000i64, 180_000] {
            let mut tl = CmafTimeline::default();
            let feeds = [
                Feed { frames: 150, base: 900_000, lead },
                Feed { frames: 200, base: 5_000_000_000, lead: 0 },
            ];
            let out = play(&mut tl, &feeds, true);
            assert!(video_forward(&out), "{lead}: the video never steps back");
            let (v, a) = last_offsets(&out);
            assert_eq!(v, a, "{lead}: the new feed's A/V relation");
        }
    }

    /// An output whose audio stopped (a switch to a video-only backup) and
    /// whose video then met a jump on its own: once the audio has been quiet
    /// a cross window, the programme — and the flow's offset — follow the
    /// video. They used to stay on the old offset for good, so an output
    /// started then (a bitrate edit, a DVR proxy provisioned beside a main)
    /// published the same video 5 h off its sibling.
    #[test]
    fn an_output_started_after_its_siblings_audio_stopped_lands_on_its_video() {
        let flow = "timeline-test-quiet-audio";
        let mut a = CmafTimeline::for_flow(flow);
        let (mut v, mut aud) = (900_000u64, 896_000u64);
        for _ in 0..100 {
            a.map(Track::Video, v);
            a.map(Track::Audio, aud);
            a.map(Track::Audio, aud + 1_800);
            v += 3_600;
            aud += 3_600;
        }
        let mut v2 = v + 5 * 3_600 * 90_000;
        for _ in 0..200 {
            a.map(Track::Video, v2);
            v2 += 3_600;
        }
        let mut c = CmafTimeline::for_flow(flow);
        assert_eq!(a.map(Track::Video, v2).ts, c.map(Track::Video, v2).ts, "one video timeline");
    }

    /// A single picture stamped 2 s ahead (a PES on the wrong clock; the
    /// ingress rewriter maps PES timestamps as they come) is placed one step
    /// on, and the video goes on on the programme's clock with the next
    /// picture. It used to be taken as a gap, and the next picture, 1.96 s
    /// behind it and on the audio's offset, opened another: the video
    /// published 2 s off the audio for good.
    #[test]
    fn a_single_picture_two_seconds_ahead_leaves_the_video_on_the_programmes_clock() {
        let mut tl = CmafTimeline::default();
        let mut vo = Vec::new();
        for k in 0..120u64 {
            let (v, a) = av(k);
            let m = tl.map(Track::Video, if k == 50 { v + 180_000 } else { v });
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

    /// Half a second of audio stamped 2 s ahead: the audio takes the step
    /// and steps back to the video's clock when the source does (the
    /// overlap is the output's to drop), where it used to stay 2 s ahead of
    /// the video for good.
    #[test]
    fn half_a_second_of_audio_two_seconds_ahead_comes_back_to_the_programmes_clock() {
        let mut tl = CmafTimeline::default();
        for k in 0..120u64 {
            let (v, a) = av(k);
            assert_eq!(tl.map(Track::Video, v).ts, v, "video at {k} untouched");
            for a in a {
                let m = tl.map(Track::Audio, if (50..63).contains(&k) { a + 180_000 } else { a });
                if k >= 63 {
                    assert_eq!(m.ts, a, "audio at {k} back on the source's clock");
                }
            }
        }
    }

    /// Both tracks jumping forward (an outage losing 2–4 s of both, or a
    /// switch to a feed that far ahead) with the video ahead of the audio in
    /// the mux: the video meets it first and, past the cross window, opens
    /// an offset; the audio takes that offset. Its own step, still within
    /// 3 s of the video, used to be taken as a gap: A/V apart by the jump
    /// for good (−2.0 to −4.0 s at a 1.1 s lead).
    #[test]
    fn both_tracks_jumping_forward_keep_their_relation_with_the_video_ahead() {
        for lead in [9_000i64, 45_000, 99_000] {
            for jump in [180_000i64, 225_000, 247_500, 270_000, 315_000, 360_000] {
                let mut tl = CmafTimeline::default();
                let feeds = [
                    Feed { frames: 100, base: 900_000, lead },
                    Feed { frames: 300, base: 900_000 + jump, lead },
                ];
                let out = play(&mut tl, &feeds, false);
                assert!(video_forward(&out), "lead {lead} jump {jump}: the video never steps back");
                let (v, a) = last_offsets(&out);
                assert_eq!(v, a, "lead {lead} jump {jump}: the A/V relation");
            }
        }
    }

    /// An outage of 2 s of both tracks, the video (0.5 s ahead in the mux)
    /// back first: its first picture is placed in doubt, the next shows the
    /// step real, and both tracks go on on the source's clock with the gap —
    /// in sync, the gap kept as it was before this rule.
    #[test]
    fn an_outage_of_both_tracks_is_a_gap_in_both() {
        let mut tl = CmafTimeline::default();
        let feeds = [
            Feed { frames: 150, base: 900_000, lead: 45_000 },
            Feed { frames: 200, base: 900_000 + 180_000, lead: 45_000 },
        ];
        let out = play(&mut tl, &feeds, false);
        assert!(video_forward(&out), "the video never steps back");
        assert_eq!(last_offsets(&out), (0, 0), "the gap kept, on the source's clock");
    }

    /// A switch to a feed on the same clock whose video leads its audio by
    /// 0.6 s where the old feed's trailed by 0.5 s: the video steps 1.1 s,
    /// the audio 0.5 s. The video's step is placed in doubt and shown real
    /// by the next picture; the new feed's relation is published.
    #[test]
    fn a_switch_moving_the_video_further_than_the_audio_takes_the_new_relation() {
        let mut tl = CmafTimeline::default();
        let feeds = [
            Feed { frames: 150, base: 900_000, lead: -45_000 },
            Feed { frames: 200, base: 945_000, lead: 9_000 },
        ];
        let out = play(&mut tl, &feeds, false);
        assert!(video_forward(&out));
        let (v, a) = last_offsets(&out);
        assert_eq!(v, a, "the new feed's A/V relation");
    }

    /// The video pausing 2 s while the audio runs on: the picture after the
    /// pause lands where the source put it, the gap before it, not one step
    /// on (the audio ran through the pause, so the step is not in doubt).
    #[test]
    fn the_picture_after_a_pause_of_the_video_is_on_the_source_clock() {
        let mut tl = CmafTimeline::default();
        for k in 0..200u64 {
            let (v, a) = av(k);
            if !(100..150).contains(&k) {
                assert_eq!(tl.map(Track::Video, v).ts, v, "picture {k}");
            }
            for a in a {
                assert_eq!(tl.map(Track::Audio, a).ts, a, "audio at {k}");
            }
        }
    }

    /// A switch to a feed 3 s behind, and 0.4 s later to one 2 s ahead of
    /// that: the picture in doubt at the second step is placed one step on,
    /// not taken onto the offset the source left, which an old offset in the
    /// list happened to fit 0.96 s behind the picture before.
    #[test]
    fn a_step_in_doubt_takes_no_old_offset() {
        let mut tl = CmafTimeline::default();
        let feeds = [
            Feed { frames: 150, base: 900_000, lead: 0 },
            Feed { frames: 10, base: 630_000, lead: 0 },
            Feed { frames: 200, base: 810_000, lead: 0 },
        ];
        let out = play(&mut tl, &feeds, false);
        assert!(video_forward(&out), "the video never steps back");
        let (v, a) = last_offsets(&out);
        assert_eq!(v, a);
    }

    /// A switch 3 s ahead, and back 1.5 s later to the clock the source
    /// left: both tracks go back onto that clock's offset with a gap.
    #[test]
    fn the_source_back_on_a_clock_it_left_goes_back_on_its_offset() {
        let mut tl = CmafTimeline::default();
        let feeds = [
            Feed { frames: 150, base: 900_000, lead: 0 },
            Feed { frames: 10, base: 1_170_000, lead: 45_000 },
            Feed { frames: 200, base: 1_035_000, lead: 0 },
        ];
        let out = play(&mut tl, &feeds, true);
        assert!(video_forward(&out), "the video never steps back");
        let (v, a) = last_offsets(&out);
        assert_eq!(v, a);
    }

    /// The video leaping 0.94 s just before the audio steps 2 s (a switch
    /// that changes the video lead by 1.1 s): the audio's step is the
    /// source's, not a doubt, so a switch back 0.4 s later does not take the
    /// audio back without the video.
    #[test]
    fn an_audio_step_the_video_leapt_before_is_no_doubt() {
        let mut tl = CmafTimeline::default();
        let feeds = [
            Feed { frames: 150, base: 900_000, lead: 99_000 },
            Feed { frames: 10, base: 1_080_000, lead: 0 },
            Feed { frames: 200, base: 810_000, lead: 99_000 },
        ];
        let out = play(&mut tl, &feeds, false);
        let (v, a) = last_offsets(&out);
        assert_eq!(v, a, "the A/V relation");
    }

    /// Both tracks stepping 1.5 s forward (the audio first, in doubt), then
    /// 1.5 s later 3 s back — to where the audio was. That is no return of
    /// the audio's: the video, 2.96 s off where it stood against the audio
    /// before, meets the switch too and cannot step back with it.
    #[test]
    fn a_switch_back_to_where_the_audio_was_is_no_return_of_the_audios() {
        let mut tl = CmafTimeline::default();
        let feeds = [
            Feed { frames: 150, base: 900_000, lead: 0 },
            Feed { frames: 37, base: 1_035_000, lead: 0 },
            Feed { frames: 200, base: 765_000, lead: 0 },
        ];
        let out = play(&mut tl, &feeds, true);
        let (v, a) = last_offsets(&out);
        assert_eq!(v, a, "the A/V relation");
    }

    /// The audio does not join the offset a picture sits on in doubt: both
    /// tracks stepping 1.5 s forward, the video first and in doubt, then 2 s
    /// back — the audio would have taken the picture's placement and the
    /// video, shown real, gone on without it.
    #[test]
    fn a_picture_in_doubt_is_no_offset_to_join() {
        let mut tl = CmafTimeline::default();
        let feeds = [
            Feed { frames: 150, base: 900_000, lead: 0 },
            Feed { frames: 10, base: 1_035_000, lead: 0 },
            Feed { frames: 200, base: 855_000, lead: 0 },
        ];
        let out = play(&mut tl, &feeds, false);
        assert!(video_forward(&out), "the video never steps back");
    }

    /// Two outputs of a flow, in lockstep and one twelve timestamps behind,
    /// through three pictures stamped 1.1 s ahead: the step in doubt is
    /// placed where each output's own history puts it, the same place, so
    /// the renditions publish one timeline.
    #[test]
    fn two_outputs_place_a_step_in_doubt_alike() {
        let mut ev = wire(&[Feed { frames: 400, base: 900_000, lead: 0 }], false);
        let mut n = 0;
        for e in ev.iter_mut().filter(|e| e.0 == Track::Video) {
            n += 1;
            if (151..154).contains(&n) {
                e.1 += 100_000;
            }
        }
        for lag in [0usize, 12] {
            let flow = format!("timeline-test-doubt-{lag}");
            let (mut a, mut b) = (CmafTimeline::for_flow(&flow), CmafTimeline::for_flow(&flow));
            let (mut oa, mut ob) = (Vec::new(), Vec::new());
            for i in 0..ev.len() + lag {
                if let Some(&(track, src, _)) = ev.get(i) {
                    oa.push(a.map(track, src).ts);
                }
                if let Some(&(track, src, _)) = i.checked_sub(lag).and_then(|j| ev.get(j)) {
                    ob.push(b.map(track, src).ts);
                }
            }
            assert_eq!(oa, ob, "lag {lag}: one timeline");
        }
    }

    /// Twelve `media_player` loops, each with 1.2 s of audio stamped a
    /// different way back: every excursion opens an offset, and after eight
    /// the source's own offset had left the list, so the audio's return
    /// found nothing and opened an offset of its own, off the video for
    /// good. The return looks at the video's offset first.
    #[test]
    fn the_audio_comes_back_after_ever_more_excursions() {
        let mut tl = CmafTimeline::default();
        let mut v = 3_168_500_000u64;
        for lap in 0..12u64 {
            for _ in 0..50 {
                assert_eq!(tl.map(Track::Video, v).ts, v, "lap {lap}");
                assert_eq!(tl.map(Track::Audio, v - 4_000).ts, v - 4_000, "lap {lap}");
                v += 3_003;
            }
            let excursion = v - 4_000 - (60 + 7 * lap) * 90_000;
            for k in 0..37u64 {
                tl.map(Track::Audio, excursion + k * 3_003);
            }
            for _ in 0..5 {
                assert_eq!(tl.map(Track::Video, v).ts, v, "lap {lap}");
                v += 3_003;
            }
        }
    }

    /// A B-frame source (decode order I P B B) with one picture stamped 2 s
    /// back and, later, one 2 s ahead: the video is back on the source's
    /// clock after each, the pictures around them in their own order. (A
    /// rule that a picture moving to another offset lands past the newest
    /// took a B-frame landing exactly on the stray picture's placement for a
    /// step back and opened an offset, 2 frames off for good.)
    #[test]
    fn a_b_frame_source_passes_a_stray_picture_either_way() {
        let mut tl = CmafTimeline::default();
        let mut a = 900_000u64;
        for k in 0..400u64 {
            let reorder = [0u64, 3, 1, 2][(k % 4) as usize] * 3_600;
            let v = 900_000 + k * 3_600 + reorder;
            let stray = match k {
                149 => v - 180_000,
                251 => v + 180_000,
                _ => v,
            };
            let m = tl.map(Track::Video, stray);
            if !(149..=153).contains(&k) && !(251..=255).contains(&k) {
                assert_eq!(m.ts, v, "picture {k}");
            }
            while a < 900_000 + (k + 1) * 3_600 {
                assert_eq!(tl.map(Track::Audio, a).ts, a);
                a += 1_920;
            }
        }
    }
}
