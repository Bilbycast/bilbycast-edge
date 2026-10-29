// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CMAF segmenter state machine.
//!
//! Phase 1: H.264 video passthrough only.
//! Phase 2: HEVC video passthrough; DASH MPD generator alongside HLS.
//! Phase 3: AAC audio passthrough/encode + H.264 video re-encode.
//!
//! # Coordination model
//!
//! Video drives segment boundaries. Each IDR (or HEVC RAP) at or after
//! `last_cut_dts + target_duration_90k` opens a new segment. Audio
//! frames are buffered into the *current* video segment as they arrive
//! and flushed atomically when the video boundary fires. This keeps
//! audio and video PTS aligned to the segment without imposing a
//! second cut decision on the audio path.
//!
//! All codec work (audio decode/encode, video decode/encode) runs in
//! `tokio::task::spawn_blocking` workers fed by bounded mpsc channels.
//! Drop-on-full prevents the broadcast subscriber from being blocked.

use std::collections::VecDeque;

use super::fmp4::{AudioTrack, Sample, VideoCodec, VideoTrack};
use super::nalu::{filter_frame_nalus_h264, filter_frame_nalus_h265, to_length_prefixed};

/// Incremental 33-bit PTS unwrapper → 64-bit monotonic clock.
pub struct PtsUnwrap {
    last: Option<u64>,
    base_unwrapped: u64,
}

impl PtsUnwrap {
    pub fn new() -> Self {
        Self {
            last: None,
            base_unwrapped: 0,
        }
    }

    /// MPEG-TS PTS is 33 bits (mod 2^33). Unwrap to a monotonic 64-bit
    /// clock by detecting wraps and adding 2^33 each time.
    pub fn unwrap(&mut self, pts33: u64) -> u64 {
        const MASK: u64 = (1u64 << 33) - 1;
        const HALF: u64 = 1u64 << 32;
        let pts = pts33 & MASK;
        match self.last {
            None => {
                self.last = Some(pts);
                self.base_unwrapped = 0;
                pts
            }
            Some(prev) => {
                if prev > HALF && pts < (prev - HALF) {
                    self.base_unwrapped += 1u64 << 33;
                }
                self.last = Some(pts);
                self.base_unwrapped + pts
            }
        }
    }
}

impl Default for PtsUnwrap {
    fn default() -> Self {
        Self::new()
    }
}

/// The 33-bit clock unwrapped to the lap nearest the last value — for video
/// presentation times.
///
/// [`PtsUnwrap`] counts a wrap only on a step *forward* across it, which is
/// right for a clock that only moves forward. Video presentation times do
/// not: they swing back by the reorder depth, and a B-picture presenting just
/// before the wrap, decoded after a P-picture presenting just past it, was
/// taken a whole lap (26.5 hours) ahead. This takes either direction across
/// the wrap. It unwraps the presentation time rather than the decode time so
/// that video starts on the lap the audio's [`PtsUnwrap`] starts on: a first
/// picture presenting just past the wrap decodes just before it, and its
/// decode time, unwrapped first, put the whole video track a lap ahead of the
/// audio in every muxed fragment.
pub struct NearestLapUnwrap {
    last: Option<u64>,
}

impl NearestLapUnwrap {
    pub fn new() -> Self {
        Self { last: None }
    }

    pub fn unwrap(&mut self, ts33: u64) -> u64 {
        const LAP: i64 = 1 << 33;
        let ts = ts33 & (LAP as u64 - 1);
        let out = match self.last {
            None => ts,
            Some(last) => {
                // The step from the last value's place on the lap, taken the
                // short way round: within ±half a lap.
                let mut step = ts as i64 - (last as i64).rem_euclid(LAP);
                if step >= LAP / 2 {
                    step -= LAP;
                } else if step < -LAP / 2 {
                    step += LAP;
                }
                match u64::try_from(last as i64 + step) {
                    Ok(v) => v,
                    // Back past zero, with no lap before it to land on. A
                    // reorder's reach — a leading picture presenting before
                    // the first one — is held at zero; anything further is a
                    // jump, taken on the first lap as `PtsUnwrap` takes it.
                    Err(_) if -step <= REORDER_SLACK_90K as i64 => 0,
                    Err(_) => ts,
                }
            }
        };
        self.last = Some(out);
        out
    }
}

impl Default for NearestLapUnwrap {
    fn default() -> Self {
        Self::new()
    }
}

/// The largest composition offset (PTS − DTS, 90 kHz) a video sample may
/// carry: two seconds, far past any reorder an encoder uses (a few frames)
/// and well short of anything a broken or wrapped DTS would produce. The
/// demuxer's caller takes no larger offset from a source; [`VideoSegmenter::push`]
/// clamps to it besides, and sizes its reorder tolerance from it.
pub const MAX_COMPOSITION_OFFSET_90K: u64 = 2 * 90_000;

/// How far a timestamp may step back within one stream before it is taken
/// for a restart: the largest presentation swing a reorder can cause, plus a
/// second for a broken stamp.
const REORDER_SLACK_90K: u64 = MAX_COMPOSITION_OFFSET_90K + 90_000;

/// The frame step learned from the source's own stamps is believed between
/// these (200 fps to 5 fps); outside them it is a gap, a splice or a restart,
/// and an unstamped picture placed by it would land seconds away.
const PLAUSIBLE_FRAME_STEP_90K: std::ops::RangeInclusive<u64> = 450..=18_000;

/// Where an unstamped picture goes before the source has shown a frame step:
/// one frame at 29.97 fps.
const NOMINAL_FRAME_STEP_90K: u64 = 3_003;

/// One completed media segment to upload.
pub struct CompletedSegment {
    pub sequence_number: u64,
    pub duration_90k: u64,
    pub bytes: Vec<u8>,
    pub kind: SegmentKind,
    /// 90 kHz DTS of the first sample in the segment.
    pub base_dts_90k: u64,
    /// 90 kHz PTS of the first sample in the segment — how the flow clock
    /// recognises the segment across renditions. The date itself is taken
    /// from `base_dts_90k`, where a player anchors it (a fragment's `tfdt`);
    /// but with B-frames the first sample decodes a frame or two before it
    /// presents, so the decode time is not the number the all-intra proxy
    /// has for the same picture, and the presentation time is.
    pub base_pts_90k: u64,
    /// Which init this segment's samples decode against — see
    /// [`VideoSegmenter::generation`]. Stamped by the segmenter at the cut,
    /// not read off the output's state by the caller, because the segment
    /// that closes *at* a parameter-set change is the last of the old
    /// generation while the state, by the time the row is written, may
    /// already describe the new one.
    pub generation: u32,
    /// True for the first segment cut after the parameter sets changed. It
    /// is a decode discontinuity whatever the clock says, and the row that
    /// lists it has to say so.
    pub first_of_generation: bool,
    /// The codec the samples were filtered and must be encrypted under —
    /// the track's at the cut, which a rotation applied by the same push
    /// has already replaced by the time the caller looks.
    pub codec: VideoCodec,
    /// 90 kHz DTS of the first sample still queued at the cut. The segment
    /// base on the plain path; on the low-latency path, where chunks have
    /// been drained as they filled, the start of the tail that closes the
    /// segment.
    pub first_pending_dts_90k: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Video,
    Audio,
    /// One fragment carrying both the video and the audio track, which is
    /// what a browser wants: a single MSE SourceBuffer, one timeline, no
    /// second playlist to keep aligned. Named like a video segment because
    /// it *is* the media segment the playlist lists.
    Muxed,
}

// ────────────────────────────────────────────────────────────────────────
//  Video segmenter (passthrough)
// ────────────────────────────────────────────────────────────────────────

pub struct VideoSegmenter {
    pub track: VideoTrack,
    target_duration_90k: u64,
    pts_unwrap: NearestLapUnwrap,
    samples: Vec<PendingVideoSample>,
    segment_base_dts: Option<u64>,
    /// The presentation time of the open segment's first sample.
    segment_base_pts: Option<u64>,
    next_seq: u64,
    /// How many times the parameter sets have changed under this stream,
    /// counting from whatever the restored window said. Zero is the ordinary
    /// life of a stream: one `init.mp4`, never renamed.
    generation: u32,
    /// A track to swap in at the next IDR. The source's SPS/PPS changed, so
    /// everything from that IDR on decodes against different parameter sets;
    /// the swap waits for the IDR because that is where the new sets take
    /// effect, and the segment open at that moment is cut first so no segment
    /// ever carries samples from both sides of the change.
    pending_track: Option<VideoTrack>,
    /// Set at the swap, carried onto the next segment to close.
    open_first_of_generation: bool,
    /// The newest presentation timestamp pushed, whether or not a segment is
    /// open — where the picture is (see [`Self::newest_pts_90k`]).
    newest_pts: Option<u64>,
    /// The decode time of the last sample pushed, and whether it was the
    /// source's own (stamped, and not held): where a picture whose PES
    /// carried no timestamp goes, and whether the next step can teach the
    /// frame step.
    last_decode: Option<u64>,
    last_decode_stamped: bool,
    /// One frame, learned from consecutive stamped decode times — never
    /// from a placed or held one, which would teach it what it placed.
    frame_step: Option<u64>,
}

struct PendingVideoSample {
    dts: u64,
    pts: u64,
    nalus: Vec<Vec<u8>>,
    is_sync: bool,
}

/// What changed when a frame was pushed.
pub struct PushOutcome {
    /// Set if a previously-open video segment closed because of this
    /// frame's IDR (and the previous segment had reached the duration
    /// target, or a track rotation forced the cut). The bytes inside are
    /// the *video-only* fMP4 segment; callers that want a muxed segment
    /// should drain the audio buffer concurrently.
    pub completed_video: Option<CompletedSegment>,
    /// True iff this frame opened a new segment. Callers use this to
    /// signal the audio segmenter to flush its buffered frames into
    /// the same segment number.
    pub new_segment_started: bool,
    /// CENC: the raw `Sample` list that built `completed_video`. The
    /// caller can re-mux it with encryption applied. `None` when no
    /// segment closed.
    pub completed_video_samples: Option<(u64, u64, Vec<Sample>)>,
}

impl VideoSegmenter {
    /// Continue an existing stream's numbering.
    ///
    /// A restart appends to an origin that already holds this stream's
    /// segments. Starting again at zero overwrites them from `seg-00000`
    /// onward, which is how a restart used to destroy the DVR history it was
    /// publishing into.
    pub fn new_from_seq(track: VideoTrack, target_duration_secs: f64, next_seq: u64) -> Self {
        let mut s = Self::new(track, target_duration_secs);
        s.next_seq = next_seq;
        s
    }

    pub fn new(track: VideoTrack, target_duration_secs: f64) -> Self {
        let target_duration_90k = (target_duration_secs * 90_000.0) as u64;
        Self {
            track,
            target_duration_90k,
            pts_unwrap: NearestLapUnwrap::new(),
            samples: Vec::new(),
            segment_base_dts: None,
            segment_base_pts: None,
            next_seq: 0,
            generation: 0,
            pending_track: None,
            open_first_of_generation: false,
            newest_pts: None,
            last_decode: None,
            last_decode_stamped: false,
            frame_step: None,
        }
    }

    /// The init generation the segments being cut now belong to.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Continue an existing stream's generation, as `new_from_seq` continues
    /// its numbering: a restart that finds rows naming `init-3.mp4` must not
    /// publish `init.mp4` over the object those rows decode against.
    pub fn set_generation(&mut self, generation: u32) {
        self.generation = generation;
    }

    /// Open a new generation without a track change. The restart path uses
    /// it when this run's init turns out not to describe the window it
    /// restored: rather than overwrite the previous run's init under rows
    /// that decode against it, this run publishes its own under a new name.
    ///
    /// The open segment is not marked first-of-generation: the join this
    /// bump makes is between the restored rows and this run's first row,
    /// which the restore's own discontinuity already tags, and the segment
    /// open now is continuous with whatever own rows precede it.
    pub fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    /// 90 kHz presentation timestamp of the newest sample pushed, if any —
    /// where the picture is, for a track that has to keep up with it.
    /// Monotonic.
    pub fn newest_pts_90k(&self) -> Option<u64> {
        self.newest_pts
    }

    /// The source's parameter sets changed. Swap the track in at the next
    /// IDR, cutting whatever segment is open first.
    ///
    /// Nothing changes until that IDR: the samples already queued were coded
    /// against the track in force and stay with it.
    pub fn rotate_track(&mut self, track: VideoTrack) {
        self.pending_track = Some(track);
    }

    /// True while a rotation is waiting for its IDR.
    pub fn rotation_pending(&self) -> bool {
        self.pending_track.is_some()
    }

    /// Whether the segment open now is the first of its generation — the
    /// low-latency path advertises a segment before it closes, so it needs
    /// the answer `CompletedSegment` will carry before that exists.
    pub fn open_segment_first_of_generation(&self) -> bool {
        self.open_first_of_generation
    }

    /// Push one access unit, in decode order. Returns `PushOutcome`
    /// describing any segment that closed and whether a new segment started.
    ///
    /// `pts90k` is the picture's presentation time on the 33-bit clock and
    /// `composition_offset_90k` how far it presents after it decodes
    /// (PTS − DTS: 0 without B-frames). Samples are written at their decode
    /// time with that offset, which is what a source with B-frames needs: at
    /// DTS = PTS, as before edge#156, its samples stepped back in decode
    /// order and every reordered picture decoded as an error. A picture whose
    /// PES carried no timestamp (`None`) is placed one frame after the last,
    /// presenting as it decodes; before any timestamp has been seen it is
    /// dropped, there being nowhere to put it.
    pub fn push(
        &mut self,
        nalus: &[Vec<u8>],
        pts90k: Option<u64>,
        composition_offset_90k: u64,
        is_keyframe: bool,
    ) -> PushOutcome {
        // The presentation time is unwrapped to its nearest lap — see
        // `NearestLapUnwrap` — and the decode time is the offset before it,
        // on the same lap by construction. One that would fall before zero
        // (the stream's first pictures presenting within their offset of it)
        // is taken at zero, and the hold below keeps the next ones moving.
        let offset = composition_offset_90k.min(MAX_COMPOSITION_OFFSET_90K);
        let (unwrapped, dts) = match pts90k {
            Some(p) => {
                let pts = self.pts_unwrap.unwrap(p);
                (pts, pts.saturating_sub(offset))
            }
            None => match self.last_decode {
                Some(last) => {
                    let dts = last + self.frame_step.unwrap_or(NOMINAL_FRAME_STEP_90K);
                    (dts, dts)
                }
                None => {
                    return PushOutcome {
                        completed_video: None,
                        new_segment_started: false,
                        completed_video_samples: None,
                    };
                }
            },
        };
        // Decode times only move forward within a stream; one that does not
        // is a source restarting below its old timeline (followed — the
        // segment cut that follows starts a fresh base) or a broken stamp
        // within a reorder's reach of the last, which is held one tick on so
        // no sample ends before it starts.
        let held = matches!(
            self.last_decode,
            Some(last) if dts <= last && dts + REORDER_SLACK_90K > last
        );
        let (unwrapped, dts) = match self.last_decode {
            Some(last) if held => (unwrapped.max(last + 1), last + 1),
            _ => (unwrapped, dts),
        };
        let stamped = pts90k.is_some() && !held;
        if let Some(last) = self.last_decode
            && stamped
            && self.last_decode_stamped
            && PLAUSIBLE_FRAME_STEP_90K.contains(&dts.wrapping_sub(last))
        {
            self.frame_step = Some(dts - last);
        }
        self.last_decode = Some(dts);
        self.last_decode_stamped = stamped;
        // The newest, not the last: presentation timestamps swing by the
        // reorder depth, and a track measured against them would swing with
        // them. A step back further than any reorder — a source restarting
        // below its old timeline, short of a wrap — is followed, or the
        // picture's position would stay stale until it climbed past the old
        // maximum.
        self.newest_pts = Some(match self.newest_pts {
            Some(d) if unwrapped + REORDER_SLACK_90K >= d => d.max(unwrapped),
            _ => unwrapped,
        });

        let mut completed = None;
        let mut completed_samples: Option<(u64, u64, Vec<Sample>)> = None;
        let mut new_segment_started = false;
        if is_keyframe {
            // A pending rotation cuts here whatever the elapsed time says:
            // the samples queued so far decode against the old parameter
            // sets and this IDR is the first that does not, so they cannot
            // share a segment. A short segment is legal; a segment whose
            // second half needs a different SPS is not decodable.
            let rotating = self.pending_track.is_some();
            if let Some(base) = self.segment_base_dts {
                let elapsed = dts.saturating_sub(base);
                // Within a frame of the target counts as at it: an IDR less
                // than one frame short closes the segment. A source whose
                // timeline lost part of a frame — a loop splice 20 ms short
                // of frame-contiguous — put the re-encode's tiling IDR just
                // under the target, and the segment ran on to the next one:
                // 3.98 s at a 29.97 fps loop, two GOPs in one segment.
                let frame = self.frame_step_90k();
                let reached = elapsed + frame > self.target_duration_90k;
                if (reached || rotating) && !self.samples.is_empty() {
                    let first_pending_dts_90k = self.samples[0].dts;
                    let base_pts_90k = self.segment_base_pts.unwrap_or(base);
                    let (seq, sb, samples) = self.snapshot_samples(dts);
                    completed = Some(CompletedSegment {
                        sequence_number: seq,
                        duration_90k: dts.saturating_sub(sb),
                        bytes: super::fmp4::build_video_segment(seq as u32, sb, &samples),
                        kind: SegmentKind::Video,
                        base_dts_90k: sb,
                        base_pts_90k,
                        generation: self.generation,
                        first_of_generation: std::mem::take(&mut self.open_first_of_generation),
                        codec: self.track.codec,
                        first_pending_dts_90k,
                    });
                    completed_samples = Some((seq, sb, samples));
                    self.samples.clear();
                    self.segment_base_dts = Some(dts);
                    self.segment_base_pts = Some(unwrapped);
                    new_segment_started = true;
                }
            }
            if self.segment_base_dts.is_none() {
                self.segment_base_dts = Some(dts);
                self.segment_base_pts = Some(unwrapped);
                new_segment_started = true;
            }
            if let Some(track) = self.pending_track.take() {
                self.track = track;
                self.generation = self.generation.saturating_add(1);
                self.open_first_of_generation = true;
            }
        }

        if self.segment_base_dts.is_none() {
            return PushOutcome {
                completed_video: None,
                new_segment_started: false,
                completed_video_samples: None,
            };
        }

        let filtered = match self.track.codec {
            VideoCodec::H264 => filter_frame_nalus_h264(nalus),
            VideoCodec::H265 => filter_frame_nalus_h265(nalus),
        };
        self.samples.push(PendingVideoSample {
            dts,
            pts: unwrapped,
            nalus: filtered,
            is_sync: is_keyframe,
        });

        PushOutcome {
            completed_video: completed,
            new_segment_started,
            completed_video_samples: completed_samples,
        }
    }

    /// One frame of the open segment (90 kHz): the median step between its
    /// samples' timestamps — robust to the one short step a splice leaves —
    /// 0 before it has two.
    fn frame_step_90k(&self) -> u64 {
        let mut steps: Vec<u64> = self
            .samples
            .windows(2)
            .map(|w| w[1].dts.saturating_sub(w[0].dts))
            .filter(|d| *d > 0)
            .collect();
        if steps.is_empty() {
            return 0;
        }
        let mid = steps.len() / 2;
        *steps.select_nth_unstable(mid).1
    }

    /// Build the `Sample` vector for the current pending samples
    /// without flushing the segmenter state. The caller owns the
    /// snapshot for CENC re-muxing.
    fn snapshot_samples(&mut self, next_seg_start_dts: u64) -> (u64, u64, Vec<Sample>) {
        let base = self.segment_base_dts.unwrap_or(0);
        let samples_out: Vec<Sample> = self
            .samples
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let next_dts = self
                    .samples
                    .get(i + 1)
                    .map(|n| n.dts)
                    .unwrap_or(next_seg_start_dts);
                Sample {
                    duration: next_dts.saturating_sub(s.dts) as u32,
                    data: to_length_prefixed(&s.nalus),
                    composition_time_offset: s.pts.saturating_sub(s.dts) as i32,
                    is_sync: s.is_sync,
                }
            })
            .collect();
        let seq = self.next_seq;
        self.next_seq += 1;
        (seq, base, samples_out)
    }

    pub fn next_segment_number(&self) -> u64 {
        self.next_seq
    }

    /// 90 kHz DTS the currently-open segment starts at, if one is open.
    ///
    /// The low-latency path advertises a segment while it is still being
    /// written, so it needs the segment's position on the media timeline
    /// before `CompletedSegment` exists to carry it.
    pub fn open_segment_base_dts_90k(&self) -> Option<u64> {
        self.segment_base_dts
    }

    /// 90 kHz PTS of the currently-open segment's first sample — how the
    /// segment is recognised across renditions (see
    /// [`CompletedSegment::base_pts_90k`]).
    pub fn open_segment_base_pts_90k(&self) -> Option<u64> {
        self.segment_base_pts
    }

    /// LL-CMAF: take *a subset* of the pending samples that span at
    /// least `chunk_duration_90k` ticks from the current chunk anchor,
    /// and rebuild them as a standalone moof+mdat chunk. Returns the
    /// byte blob or `None` if not enough samples have accumulated.
    ///
    /// Chunks are numbered within a segment starting at 0. The first
    /// chunk of a segment is always a sync chunk (starts with IDR).
    pub fn take_pending_chunk(
        &mut self,
        sequence_number: u32,
        chunk_duration_90k: u64,
        chunk_index_within_seg: u32,
    ) -> Option<Vec<u8>> {
        if self.samples.is_empty() {
            return None;
        }
        let first_dts = self.samples.first()?.dts;
        // Accumulate samples until we've met the chunk duration target.
        let mut included = 0usize;
        for (i, s) in self.samples.iter().enumerate() {
            if s.dts.saturating_sub(first_dts) >= chunk_duration_90k && i > 0 {
                included = i;
                break;
            }
        }
        if included == 0 {
            // Not enough data yet.
            return None;
        }
        // Estimate the next sample's DTS for the last-sample duration.
        let next_dts = self.samples.get(included)
            .map(|n| n.dts)
            .unwrap_or(first_dts + chunk_duration_90k);

        let chunk_samples: Vec<Sample> = self.samples[..included]
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let nd = if i + 1 < included {
                    self.samples[i + 1].dts
                } else {
                    next_dts
                };
                Sample {
                    duration: nd.saturating_sub(s.dts) as u32,
                    data: to_length_prefixed(&s.nalus),
                    composition_time_offset: s.pts.saturating_sub(s.dts) as i32,
                    is_sync: s.is_sync,
                }
            })
            .collect();

        // Drain consumed samples from the pending buffer.
        self.samples.drain(..included);

        // Build a standalone moof+mdat chunk (no styp on subsequent
        // chunks — styp opens the segment on the first chunk only).
        let include_styp = chunk_index_within_seg == 0;
        Some(super::fmp4::build_segment_chunk(
            super::fmp4::VIDEO_TRACK_ID,
            sequence_number,
            first_dts,
            &chunk_samples,
            include_styp,
        ))
    }
}


// ────────────────────────────────────────────────────────────────────────
//  Audio segmenter — frame buffer that flushes on external signal
// ────────────────────────────────────────────────────────────────────────

pub struct AudioSegmenter {
    pub track: AudioTrack,
    /// Target duration in the track's timescale (== sample_rate).
    target_duration_ts: u64,
    pts_unwrap: PtsUnwrap,
    samples: VecDeque<PendingAudioSample>,
    segment_base_dts: Option<u64>,
    next_seq: u64,
}

struct PendingAudioSample {
    dts_ts: u64,
    data: Vec<u8>,
}

/// How much audio may accumulate before frames are shed, as a multiple of the
/// segment target.
///
/// The muxed fragment drains the buffer every time a video segment closes, so
/// running this far past the target means video has stopped closing segments —
/// no IDR, or the video track gone. Nothing is being published in that state,
/// so the oldest audio is dead weight; shedding it is what keeps a stalled
/// video track from growing the buffer without bound. It is reported, not
/// silent: `push` hands the shed audio back to the caller.
const MAX_PENDING_AUDIO_MULTIPLE: u64 = 4;

/// The last-resort stride, in track ticks, for a frame with nothing after
/// it: a fragment drained with nothing queued behind it, or the shed path.
///
/// An AAC-LC frame is 1024 samples and an HE-AAC (SBR) frame 2048 at the
/// output rate, which is why the observed spacing is preferred whenever two
/// frames have been seen — this is only for the first frame of a stream.
const AAC_FRAME_SAMPLES: u64 = 1024;

impl AudioSegmenter {
    /// Continue an existing stream's numbering — see
    /// [`VideoSegmenter::new_from_seq`].
    pub fn new_from_seq(track: AudioTrack, target_duration_secs: f64, next_seq: u64) -> Self {
        let mut s = Self::new(track, target_duration_secs);
        s.next_seq = next_seq;
        s
    }

    pub fn new(track: AudioTrack, target_duration_secs: f64) -> Self {
        let target_duration_ts = (target_duration_secs * track.sample_rate as f64) as u64;
        Self {
            track,
            target_duration_ts,
            pts_unwrap: PtsUnwrap::new(),
            samples: VecDeque::new(),
            segment_base_dts: None,
            next_seq: 0,
        }
    }

    /// Append one AAC frame. PTS is the source 90 kHz value; we
    /// convert into the audio track timescale.
    ///
    /// Returns a segment only when audio has been **shed** — see
    /// [`MAX_PENDING_AUDIO_MULTIPLE`]. In normal operation this is `None` and
    /// the buffer is drained by [`Self::take_pending_samples`] when the video
    /// segment closes.
    pub fn push(&mut self, frame: &[u8], pts90k: u64) -> Option<CompletedSegment> {
        let unwrapped = self.pts_unwrap.unwrap(pts90k);
        let dts_ts = unwrapped * self.track.sample_rate as u64 / 90_000;

        self.segment_base_dts.get_or_insert(dts_ts);
        self.samples.push_back(PendingAudioSample {
            dts_ts,
            data: frame.to_vec(),
        });

        // Deliberately NOT cut at the segment target. The muxed fragment is
        // built from these frames when the *video* segment closes, and video
        // closes on the first IDR at or after its own target — which is at or
        // after this one, and usually after. Cutting here handed the caller a
        // segment it discards while draining the buffer the muxed fragment
        // was about to read: measured at 3 of 98 frames surviving, i.e. 65 ms
        // of audio carried per 2 s segment, silently.
        //
        // The buffer still has to be bounded, because a video track that has
        // stopped producing IDRs never drains it.
        if let Some(base) = self.segment_base_dts {
            let elapsed = dts_ts.saturating_sub(base);
            if elapsed >= self.target_duration_ts * MAX_PENDING_AUDIO_MULTIPLE {
                let stride = Self::frame_stride(self.samples.make_contiguous());
                return self.flush_current_segment(dts_ts + stride);
            }
        }
        None
    }

    /// Take the audio frames belonging to a segment that ends at
    /// `boundary_dts_ts`, as a `Sample` vector for a muxed fragment.
    /// Returns `(sequence_number, base_dts_ts, samples)`.
    ///
    /// Frames at or past the boundary stay queued for the next fragment. They
    /// are routinely present: a TS audio PES carries several ADTS frames at
    /// once and a video PES is only released when the next PUSI arrives, so at
    /// the moment a video segment closes the buffer normally holds audio from
    /// after the cut.
    ///
    /// `base_dts_ts` is the first taken frame's own DTS, not the boundary.
    /// The video traf's `base_media_decode_time` is likewise its first
    /// sample's exact DTS, and the two trafs share one moof: anchoring audio
    /// on the boundary instead declares it starting earlier than it does, by
    /// however far the buffer ran past the previous cut — a fixed A/V offset
    /// for the life of the flow, plus a zero-duration sample at each junction.
    pub fn take_pending_samples(
        &mut self,
        boundary_dts_ts: u64,
    ) -> Option<(u64, u64, Vec<Sample>)> {
        let taken = self
            .samples
            .iter()
            .take_while(|f| f.dts_ts < boundary_dts_ts)
            .count();
        if taken == 0 {
            return None;
        }
        let frames: Vec<PendingAudioSample> = self.samples.drain(..taken).collect();
        let base = frames[0].dts_ts;

        // Where the LAST frame of this fragment ends.
        //
        // It must be the successor's own DTS, never the video boundary. The
        // next fragment anchors on the successor (see the doc comment above),
        // so ending this one at the boundary leaves a hole between the two of
        // however far the audio grid overshot the video cut — up to one frame.
        //
        // That hole is not cosmetic. Chrome's MSE declares a discontinuity
        // when the next decode timestamp jumps more than **twice the previous
        // frame's duration**, and truncating makes the previous frame short
        // exactly when the following gap is large, so the test trips on
        // roughly every other segment. A discontinuity sets "need random
        // access point" on every track buffer sharing the source buffer — the
        // VIDEO track included — and Chrome then discards frames until the
        // next IDR. On a 2 s GOP that is the entire next segment of picture,
        // while the audio plays on untouched: a still image and continuing
        // sound, every four seconds or so. An all-intra rendition loses a
        // single frame to the same fault, which is why the proxy looked clean
        // and only the full-resolution feed appeared broken.
        //
        // Audio may therefore end up to one frame past the video boundary in
        // a fragment. That is allowed and is what ffmpeg's own muxer does;
        // its seams measure zero, and this one now does too.
        let successor = self.samples.front().map(|f| f.dts_ts);
        // `taken > 0`, so `frames` is not empty.
        let last_end = successor.unwrap_or(frames[taken - 1].dts_ts + Self::frame_stride(&frames));

        let samples_out: Vec<Sample> = frames
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let next = frames.get(i + 1).map(|n| n.dts_ts).unwrap_or(last_end);
                Sample {
                    duration: next.saturating_sub(f.dts_ts) as u32,
                    data: f.data.clone(),
                    composition_time_offset: 0,
                    is_sync: true,
                }
            })
            .collect();
        let seq = self.next_seq;
        self.next_seq += 1;
        // Anchor the shed-cap clock on what is actually still buffered. Left
        // as `None` it would be re-anchored by `push` on the next *new* frame,
        // which is later than the frames already queued.
        self.segment_base_dts = self.samples.front().map(|f| f.dts_ts);
        Some((seq, base, samples_out))
    }

    /// The stride one frame runs when nothing follows it to say.
    ///
    /// The smallest spacing seen between the frames given, not the last: a
    /// source gap in the final pair — a dropped PES, a splice — would
    /// otherwise be copied onto the last frame as its duration, extending it
    /// past where the audio actually ends. The smallest is a real frame's
    /// length or one tick short of it from the 90 kHz → sample-rate
    /// rounding, never a gap.
    fn frame_stride(frames: &[PendingAudioSample]) -> u64 {
        frames
            .windows(2)
            .map(|w| w[1].dts_ts.saturating_sub(w[0].dts_ts))
            .filter(|d| *d > 0)
            .min()
            .unwrap_or(AAC_FRAME_SAMPLES)
    }

    fn flush_current_segment(&mut self, next_seg_start_dts: u64) -> Option<CompletedSegment> {
        if self.samples.is_empty() {
            return None;
        }
        let base = self.segment_base_dts?;
        let frames: Vec<PendingAudioSample> = self.samples.drain(..).collect();
        let samples_out: Vec<Sample> = frames
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let next = frames
                    .get(i + 1)
                    .map(|n| n.dts_ts)
                    .unwrap_or(next_seg_start_dts);
                Sample {
                    duration: next.saturating_sub(f.dts_ts) as u32,
                    data: f.data.clone(),
                    composition_time_offset: 0,
                    is_sync: true,
                }
            })
            .collect();
        let seq = self.next_seq;
        self.next_seq += 1;
        let bytes = super::fmp4::build_audio_segment(seq as u32, base, &samples_out);
        let duration_ts = next_seg_start_dts.saturating_sub(base);

        self.segment_base_dts = Some(next_seg_start_dts);

        Some(CompletedSegment {
            sequence_number: seq,
            duration_90k: duration_ts * 90_000 / self.track.sample_rate as u64,
            bytes,
            kind: SegmentKind::Audio,
            base_dts_90k: base * 90_000 / self.track.sample_rate as u64,
            base_pts_90k: base * 90_000 / self.track.sample_rate as u64,
            // Audio-only segments are cut on the video's signal and never
            // rotate: the audio track is committed at the first init and the
            // audio-only segment path has no init generation of its own.
            generation: 0,
            first_of_generation: false,
            // Audio is never encrypted and never chunked, so neither field
            // is read for this kind; the codec is a placeholder.
            codec: VideoCodec::H264,
            first_pending_dts_90k: base * 90_000 / self.track.sample_rate as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pts_unwrap_handles_33bit_wrap() {
        let mut u = PtsUnwrap::new();
        let top = (1u64 << 33) - 1_000;
        assert_eq!(u.unwrap(top), top);
        let wrapped = 1_000u64;
        let expected = (1u64 << 33) + 1_000;
        assert_eq!(u.unwrap(wrapped), expected);
    }

    #[test]
    fn video_segmenter_drops_until_first_idr() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        let p_frame = vec![vec![0x41, 0x00]];
        let outcome = s.push(&p_frame, Some(0), 0, false);
        assert!(outcome.completed_video.is_none());
        assert!(!outcome.new_segment_started);
        assert!(s.samples.is_empty());
        let idr = vec![vec![0x65, 0xB8]];
        let outcome = s.push(&idr, Some(3000), 0, true);
        assert!(outcome.completed_video.is_none());
        assert!(outcome.new_segment_started);
        assert_eq!(s.samples.len(), 1);
    }

    /// edge#156: a source with B-frames is written in decode order. Each
    /// sample sits at its DTS — one frame after the last — and carries its
    /// presentation offset. At DTS = PTS, as before, the reordered pictures
    /// stepped back and every B-picture decoded as an error.
    #[test]
    fn a_b_frame_gop_is_written_in_decode_order_with_its_offsets() {
        const F: u64 = 3_600;
        let base = 900_000u64;
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 0.2);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        // Decode order I0 P3 B1 B2 P6 B4 B5, then the next IDR: display index
        // d presents at base + (d + 1)·F, decode position k at base + k·F.
        let gop: [(u64, bool); 8] =
            [(0, true), (3, false), (1, false), (2, false), (6, false), (4, false), (5, false), (7, true)];
        let mut cut = None;
        for (k, (d, key)) in gop.into_iter().enumerate() {
            let pts = base + (d + 1) * F;
            let dts = base + k as u64 * F;
            let out = s.push(if key { &idr } else { &p }, Some(pts), pts - dts, key);
            if let Some(samples) = out.completed_video_samples {
                cut = Some(samples);
            }
        }
        let (_seq, seg_base, samples) = cut.expect("the second IDR closes the segment");
        assert_eq!(seg_base, base, "the segment starts at the IDR's decode time");
        assert!(samples.iter().all(|x| x.duration == F as u32), "one frame each, in decode order");
        let offsets: Vec<i32> = samples.iter().map(|x| x.composition_time_offset).collect();
        assert_eq!(offsets, [F as i32, 3 * F as i32, 0, 0, 3 * F as i32, 0, 0]);
        assert_eq!(s.newest_pts_90k(), Some(base + 8 * F), "where the picture is: the newest PTS");
    }

    /// Timestamps unwrap to the nearest lap, in either direction: a step
    /// back across the wrap stays on its lap rather than jumping a whole one
    /// ahead. Back past zero there is no lap: a reorder's reach is held at
    /// zero, a jump is taken on the first lap.
    #[test]
    fn nearest_lap_unwrap_takes_the_nearest_lap() {
        const LAP: u64 = 1 << 33;
        let mut u = NearestLapUnwrap::new();
        assert_eq!(u.unwrap(LAP - 3_600), LAP - 3_600);
        assert_eq!(u.unwrap(0), LAP, "forward across the wrap");
        assert_eq!(u.unwrap(LAP - 1_800), LAP - 1_800, "back across it, on its lap");
        assert_eq!(u.unwrap(7_200), LAP + 7_200);
        let mut u = NearestLapUnwrap::new();
        assert_eq!(u.unwrap(1_000), 1_000);
        assert_eq!(u.unwrap(LAP - 500), 0, "a leading picture before zero: held at zero");
        let mut u = NearestLapUnwrap::new();
        assert_eq!(u.unwrap(1_000), 1_000);
        assert_eq!(u.unwrap(LAP - 900_000), LAP - 900_000, "ten seconds back: a jump, first lap");
    }

    /// A stream whose first picture presents just past the wrap decodes just
    /// before it. Its video must start on the lap its audio starts on —
    /// unwrapping the decode time first put the video a whole lap ahead.
    #[test]
    fn a_first_picture_just_past_the_wrap_starts_on_the_first_lap() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 60.0);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        // PTS 1000, DTS 2^33 − 2003.
        s.push(&idr, Some(1_000), 3_003, true);
        s.push(&p, Some(1_000 + 3 * 3_003), 3 * 3_003, false);
        s.push(&p, Some(1_000 + 3_003), 0, false);
        assert_eq!(s.newest_pts_90k(), Some(1_000 + 3 * 3_003));
        let d: Vec<u64> = s.samples.iter().map(|x| x.dts).collect();
        assert!(d.iter().all(|t| *t < 90_000), "first lap: {d:?}");
        assert!(d.windows(2).all(|w| w[1] > w[0]), "decode times move forward: {d:?}");
        assert!(s.samples.iter().all(|x| x.pts >= x.dts));
    }

    /// edge#156 at the 33-bit wrap: a B-frame GOP straddling it, at every
    /// position in the GOP, on a stream that began a GOP earlier.
    /// Presentation times swing back by the reorder, and a forward-only
    /// unwrap of them took a B-picture presenting just before the wrap,
    /// after a P-picture just past it, for a whole lap ahead. Every sample
    /// stays one frame on, its offset intact.
    #[test]
    fn a_b_frame_gop_straddling_the_wrap_stays_continuous() {
        const F: u64 = 3_600;
        const MASK: u64 = (1 << 33) - 1;
        // Display index of each decode position: I0 P3 B1 B2 P6 B4 B5.
        const ORDER: [u64; 7] = [0, 3, 1, 2, 6, 4, 5];
        for at in 0..7u64 {
            // Decode position `at` of the second GOP is the first past the wrap.
            let start = (1u64 << 33) - (7 + at) * F;
            let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
            let mut s = VideoSegmenter::new(v, 0.1);
            let idr = vec![vec![0x65, 0xB8]];
            let p = vec![vec![0x41, 0x00]];
            let mut cuts = Vec::new();
            let mut newest = 0;
            for n in 0..15u64 {
                let (gop, k) = (n / 7, n % 7);
                let d = if n == 14 { 0 } else { ORDER[k as usize] };
                let key = k == 0;
                let dts = start + n * F;
                let pts = start + (gop * 7 + d + 1) * F;
                let out = s.push(if key { &idr } else { &p }, Some(pts & MASK), pts - dts, key);
                if let Some(c) = out.completed_video_samples {
                    cuts.push(c);
                }
                let now = s.newest_pts_90k().expect("pushed");
                assert!(now >= newest && now <= start + 15 * F, "wrap at {at}: newest {now}");
                newest = now;
            }
            let (_, seg_base, samples) = cuts.pop().unwrap_or_else(|| panic!("wrap at {at}: no cut"));
            assert_eq!(seg_base, start + 7 * F, "wrap at {at}");
            assert!(samples.iter().all(|x| x.duration == F as u32), "wrap at {at}");
            let offsets: Vec<i32> = samples.iter().map(|x| x.composition_time_offset).collect();
            assert_eq!(offsets, [F as i32, 3 * F as i32, 0, 0, 3 * F as i32, 0, 0], "wrap at {at}");
        }
    }

    /// A segment carries its first sample's presentation time beside its
    /// decode time: the flow clock recognises a segment across renditions by
    /// it, and with B-frames the IDR decodes a frame before it presents, so
    /// the decode time differs from the all-intra proxy's.
    #[test]
    fn a_segment_carries_its_first_presentation_time() {
        const F: u64 = 3_600;
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 0.2);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        let base = 900_000u64;
        // I0 P3 B1 B2 P6 B4 B5, then the next IDR.
        let gop: [(u64, bool); 8] =
            [(0, true), (3, false), (1, false), (2, false), (6, false), (4, false), (5, false), (7, true)];
        let mut done = None;
        for (k, (d, key)) in gop.into_iter().enumerate() {
            let pts = base + (d + 1) * F;
            let dts = base + k as u64 * F;
            s.push(if key { &idr } else { &p }, Some(pts), pts - dts, key);
            if k == 0 {
                assert_eq!(s.open_segment_base_dts_90k(), Some(base));
                assert_eq!(s.open_segment_base_pts_90k(), Some(base + F));
            }
            if key && k > 0 {
                done = s.open_segment_base_pts_90k();
            }
        }
        assert_eq!(done, Some(base + 8 * F), "the next segment opens at its IDR's presentation");
        // And the closed one carries both.
        let mut s2 = VideoSegmenter::new(
            VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]),
            0.2,
        );
        let mut seg = None;
        for (k, (d, key)) in gop.into_iter().enumerate() {
            let pts = base + (d + 1) * F;
            let dts = base + k as u64 * F;
            if let Some(c) = s2.push(if key { &idr } else { &p }, Some(pts), pts - dts, key).completed_video {
                seg = Some(c);
            }
        }
        let seg = seg.expect("cut");
        assert_eq!(seg.base_dts_90k, base);
        assert_eq!(seg.base_pts_90k, base + F);
    }

    /// The step an unstamped picture is placed by is learned only from two
    /// consecutive stamped decode times a frame apart: not from a held
    /// sample's one tick, not from a jump of seconds, and not from a
    /// picture it placed itself.
    #[test]
    fn an_unstamped_picture_steps_by_a_learned_frame_only() {
        const F: u64 = 3_600;
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 60.0);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        s.push(&idr, Some(90_000), 0, true);
        // Nothing learned yet: one nominal frame.
        s.push(&p, None, 0, false);
        assert_eq!(s.samples.last().unwrap().dts, 90_000 + NOMINAL_FRAME_STEP_90K);
        s.push(&p, Some(200_000), 0, false);
        s.push(&p, Some(200_000 + F), 0, false);
        // A held stamp (one tick on) and then a jump of ten seconds teach
        // nothing: the step stays one frame.
        s.push(&p, Some(200_000), 0, false);
        s.push(&p, Some(200_000 + 10 * 90_000), 0, false);
        s.push(&p, None, 0, false);
        let d: Vec<u64> = s.samples.iter().rev().take(2).map(|x| x.dts).collect();
        assert_eq!(d[0] - d[1], F, "{d:?}");
        // Placed pictures place the next by the same step.
        s.push(&p, None, 0, false);
        let d: Vec<u64> = s.samples.iter().rev().take(2).map(|x| x.dts).collect();
        assert_eq!(d[0] - d[1], F);
    }

    /// An offset past two seconds is clamped, so a sample never presents
    /// further from its decode time than the reorder tolerance allows for.
    #[test]
    fn a_composition_offset_is_clamped() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 60.0);
        s.push(&[vec![0x65, 0xB8]], Some(9_000_000), 10 * 90_000, true);
        let x = s.samples.last().unwrap();
        assert_eq!(x.pts - x.dts, MAX_COMPOSITION_OFFSET_90K);
        assert_eq!(x.pts, 9_000_000);
    }

    /// A picture whose PES carried no timestamp is placed a frame after the
    /// last decode time; fed its 0 it used to be a sample stepping back to
    /// the epoch. Before any timestamp there is nowhere to put it.
    #[test]
    fn an_unstamped_picture_follows_the_last_one() {
        const F: u64 = 3_600;
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 0.1);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        assert!(!s.push(&idr, None, 0, true).new_segment_started, "nowhere to put it yet");
        s.push(&idr, Some(90_000), 0, true);
        s.push(&p, Some(90_000 + F), 0, false);
        s.push(&p, None, 0, false);
        s.push(&p, None, 0, false);
        let out = s.push(&idr, Some(90_000 + 4 * F), 0, true);
        let (_, base, samples) = out.completed_video_samples.expect("the IDR cuts");
        assert_eq!(base, 90_000);
        assert_eq!(samples.len(), 4);
        assert!(samples.iter().all(|x| x.duration == F as u32), "{:?}", samples.iter().map(|x| x.duration).collect::<Vec<_>>());
    }

    /// A decode time that steps back within a reorder's reach — a broken
    /// stamp — is held one tick past the last, so no sample ends before it
    /// starts; a step back past that is a restart and is followed.
    #[test]
    fn decode_times_never_step_back_within_a_stream() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 10.0);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        s.push(&idr, Some(9_000_000), 0, true);
        s.push(&p, Some(9_003_600), 0, false);
        // DTS 9_000_000 again: held to 9_003_601.
        s.push(&p, Some(9_007_200), 7_200, false);
        let dts: Vec<u64> = s.samples.iter().map(|x| x.dts).collect();
        assert_eq!(dts, [9_000_000, 9_003_600, 9_003_601]);
        // A step back of a minute is a restart, followed.
        s.push(&p, Some(9_000_000 - 5_400_000), 0, false);
        assert_eq!(s.samples.last().unwrap().dts, 9_000_000 - 5_400_000);
    }

    #[test]
    fn video_segmenter_cuts_on_next_idr_after_target() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        s.push(&idr, Some(0), 0, true);
        for i in 1..60 {
            let pts = (i * 3000) as u64;
            assert!(s.push(&p, Some(pts), 0, false).completed_video.is_none(), "frame {i}");
        }
        let boundary_pts = 2 * 90_000;
        let outcome = s.push(&idr, Some(boundary_pts), 0, true);
        let seg = outcome.completed_video.expect("segment should cut here");
        assert_eq!(seg.sequence_number, 0);
        assert!(seg.duration_90k > 0);
        assert!(outcome.new_segment_started);
        assert_eq!(seg.kind, SegmentKind::Video);
        assert_eq!(s.samples.len(), 1);
    }

    /// An IDR less than a frame short of the target closes the segment: a
    /// 29.97 fps re-encode whose timeline lost 20 ms at a loop splice put its
    /// 60-frame tiling IDR 1 809 ticks under 2 s, and the segment ran on to
    /// the next — 3.98 s. An IDR a whole frame or more short still does not
    /// cut (25 fps: the 49th frame of a 2 s segment).
    #[test]
    fn an_idr_within_a_frame_of_the_target_closes_the_segment() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        let mut s = VideoSegmenter::new(v, 2.0);
        s.push(&idr, Some(0), 0, true);
        for i in 1..60u64 {
            // Frame 20 came 1 809 ticks early: the splice.
            let pts = i * 3_003 - if i >= 20 { 1_809 } else { 0 };
            assert!(s.push(&p, Some(pts), 0, false).completed_video.is_none(), "frame {i}");
        }
        let seg = s.push(&idr, Some(60 * 3_003 - 1_809), 0, true).completed_video.expect("cut at the tiling IDR");
        assert_eq!(seg.duration_90k, 178_371);

        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        s.push(&idr, Some(0), 0, true);
        for i in 1..49u64 {
            s.push(&p, Some(i * 3_600), 0, false);
        }
        assert!(s.push(&idr, Some(49 * 3_600), 0, true).completed_video.is_none(), "a frame short: no cut");
    }

    /// A track rotation cuts the open segment at the IDR that carries the new
    /// parameter sets, before that IDR is queued — so the segment that closes
    /// is all old-generation samples, stamped with the old generation, and
    /// the segment that opens is the first of the new one.
    ///
    /// Without the forced cut the samples either side of the change shared a
    /// segment, and the row that listed it named whichever init the output
    /// had adopted by the time it was written — the new one, so a player
    /// re-initialised its decoder from the new SPS and was fed the old
    /// slices. Reproduced the very fault the rotation exists to fix, on
    /// every rotation.
    #[test]
    fn a_rotation_cuts_at_its_idr_and_stamps_both_sides() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        s.push(&idr, Some(0), 0, true);
        for i in 1..10 {
            s.push(&p, Some((i * 3000) as u64), 0, false);
        }
        assert_eq!(s.generation(), 0);
        assert!(!s.open_segment_first_of_generation());

        // The change is noticed on a P-frame's worth of lookahead; nothing
        // happens until an IDR.
        s.rotate_track(VideoTrack::from_h264(vec![0x67, 0x64, 0x00, 0x1F], vec![0x68, 0xEE]));
        assert!(s.rotation_pending());
        let out = s.push(&p, Some(30_000), 0, false);
        assert!(out.completed_video.is_none());
        assert_eq!(s.generation(), 0);

        // Well short of the 2 s target, the IDR still cuts.
        let out = s.push(&idr, Some(33_000), 0, true);
        let seg = out.completed_video.expect("the rotation forces a cut");
        assert_eq!(seg.generation, 0, "the closed segment is the old generation");
        assert!(!seg.first_of_generation);
        assert_eq!(seg.duration_90k, 33_000);
        assert!(out.new_segment_started);
        assert!(!s.rotation_pending());
        assert_eq!(s.generation(), 1);
        assert_eq!(s.track.sps(), &[0x67, 0x64, 0x00, 0x1F]);
        assert!(s.open_segment_first_of_generation(), "the open segment is the first of the new one");
        assert_eq!(s.samples.len(), 1, "the IDR itself opens the new segment");

        // The next ordinary cut carries the mark, and only that one does.
        for i in 1..61 {
            s.push(&p, Some(33_000 + (i * 3000) as u64), 0, false);
        }
        let out = s.push(&idr, Some(33_000 + 2 * 90_000), 0, true);
        let seg = out.completed_video.expect("an ordinary cut");
        assert_eq!(seg.generation, 1);
        assert!(seg.first_of_generation);
        assert!(!s.open_segment_first_of_generation());
    }

    /// A restart's generation bump marks nothing: the join to the restored
    /// rows is the restore discontinuity's to tag, and the segment open at
    /// the bump continues whatever own rows precede it.
    #[test]
    fn a_restart_bump_does_not_mark_the_open_segment() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        s.push(&idr, Some(0), 0, true);
        // 59 frames after each IDR, so the next IDR is the 60th: one more
        // would share the IDR's timestamp.
        for i in 1..60 {
            s.push(&p, Some((i * 3000) as u64), 0, false);
        }
        let a = s.push(&idr, Some(2 * 90_000), 0, true).completed_video.expect("A");
        assert_eq!(a.generation, 0);
        s.bump_generation();
        assert!(!s.open_segment_first_of_generation());
        for i in 1..60 {
            s.push(&p, Some(2 * 90_000 + (i * 3000) as u64), 0, false);
        }
        let b = s.push(&idr, Some(4 * 90_000), 0, true).completed_video.expect("B");
        assert_eq!(b.generation, 1);
        assert!(!b.first_of_generation);
        assert_eq!(b.codec, VideoCodec::H264);
        assert_eq!(b.first_pending_dts_90k, 2 * 90_000, "nothing was drained: the tail is the segment");
        assert_eq!(s.newest_pts_90k(), Some(4 * 90_000));
    }

    /// On the low-latency path chunks are drained as they fill, so what the
    /// cut carries is the tail — and its own first DTS, which is where the
    /// final chunk's `tfdt` has to point.
    #[test]
    fn the_cut_carries_the_tails_own_first_dts() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        s.push(&idr, Some(0), 0, true);
        for i in 1..50 {
            s.push(&p, Some((i * 3600) as u64), 0, false); // 25 fps
        }
        // 500 ms chunks: three go out, at frames 0..13, 13..26, 26..39.
        let mut drained = 0;
        while s.take_pending_chunk(0, 45_000, drained).is_some() {
            drained += 1;
        }
        assert_eq!(drained, 3);
        let tail_first = s.samples[0].dts;
        assert_eq!(tail_first, 39 * 3600);
        let seg = s.push(&idr, Some(50 * 3600), 0, true).completed_video.expect("the cut");
        assert_eq!(seg.first_pending_dts_90k, tail_first);
        assert_eq!(seg.base_dts_90k, 0, "the segment still starts where it started");
        let tail: u64 = seg.duration_90k;
        assert_eq!(tail, 50 * 3600);
    }

    /// A rotation requested before any segment is open has nothing to cut:
    /// the first IDR opens generation 1 directly.
    #[test]
    fn a_rotation_before_the_first_idr_opens_the_new_generation() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        s.rotate_track(VideoTrack::from_h264(vec![0x67, 0x64, 0x00, 0x1F], vec![0x68, 0xEE]));
        let out = s.push(&[vec![0x65, 0xB8]], Some(0), 0, true);
        assert!(out.completed_video.is_none());
        assert!(out.new_segment_started);
        assert_eq!(s.generation(), 1);
    }

    /// The segment that just closed ends exactly where the open one begins.
    ///
    /// This identity is what dates a closed low-latency row: the row is built
    /// after `push()` has already moved the segmenter on, so the length it
    /// advertises — and the length the flow clock is steered with — is
    /// `open_segment_base_dts_90k()` minus the closed segment's own base. It
    /// had no test caller anywhere, on either side of the subtraction, while
    /// the whole of `closed_ll_entry` rested on it.
    ///
    /// A 1.5 s GOP against a 2 s target is the case that matters: the
    /// segmenter cuts on the first IDR at or *past* the target, so the segment
    /// runs 3 s and the configured 2 s is not its length. Passing the nominal
    /// figure instead dated every row a second early, permanently.
    #[test]
    fn the_open_segment_begins_where_the_closed_one_ended() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        let idr = vec![vec![0x65, 0xB8]];

        // Nothing is open before the first IDR, and the caller must be able to
        // tell that apart from a segment that starts at zero.
        assert_eq!(s.open_segment_base_dts_90k(), None);

        let gop = 90_000 + 45_000; // 1.5 s
        s.push(&idr, Some(0), 0, true);
        assert_eq!(s.open_segment_base_dts_90k(), Some(0));
        // 1.5 s in: past a GOP, short of the 2 s target, so no cut.
        assert!(s.push(&idr, Some(gop), 0, true).completed_video.is_none());
        assert_eq!(s.open_segment_base_dts_90k(), Some(0));

        // 3 s in: the first IDR at or past the target closes the segment.
        let seg = s
            .push(&idr, Some(2 * gop), 0, true)
            .completed_video
            .expect("the second IDR past the target closes the segment");
        assert_eq!(seg.base_dts_90k, 0);
        assert_eq!(seg.duration_90k, 2 * gop, "a 1.5 s GOP gives 3 s segments");
        assert_eq!(
            s.open_segment_base_dts_90k(),
            Some(seg.base_dts_90k + seg.duration_90k),
            "the open segment does not begin where the closed one ended, so a \
             row dated by the difference is dated by a fiction"
        );

        // And it holds for the second segment too, where the base is not zero
        // and an off-by-one in either direction would still land somewhere
        // plausible.
        let seg = s
            .push(&idr, Some(4 * gop), 0, true)
            .completed_video
            .expect("segment two closes on the same rule");
        assert_eq!(seg.base_dts_90k, 2 * gop);
        assert_eq!(
            s.open_segment_base_dts_90k(),
            Some(seg.base_dts_90k + seg.duration_90k)
        );
    }

    /// Audio is shed only when it has run far past the segment target, which
    /// means the video track has stopped closing segments. At the target
    /// itself it must be retained — the muxed fragment is built from it.
    #[test]
    fn audio_segmenter_sheds_only_after_the_buffer_cap() {
        let a = AudioTrack::aac([0x11, 0x90], 48000, 2, 128_000);
        let mut s = AudioSegmenter::new(a, 2.0);
        let data = vec![0xFF; 200];
        let mut seg = None;
        let mut shed_at = None;
        // 2 s of audio is 94 frames, so 4x the target is around 375.
        for i in 0..500 {
            let pts = (i * 1920) as u64;
            if let Some(s) = s.push(&data, pts) {
                seg = Some(s);
                shed_at = Some(i);
                break;
            }
        }
        let seg = seg.expect("audio should be shed once the cap is passed");
        assert_eq!(seg.kind, SegmentKind::Audio);
        assert!(seg.duration_90k > 0);
        let shed_at = shed_at.expect("shed index");
        assert!(
            (350..400).contains(&shed_at),
            "shed at frame {shed_at}, expected around 4x the 94-frame target"
        );
    }

    /// A full segment's worth of audio must still be there when the video
    /// segment closes and the muxed fragment drains it.
    #[test]
    fn audio_survives_until_the_video_boundary_drains_it() {
        let a = AudioTrack::aac([0x11, 0x90], 48000, 2, 128_000);
        let mut s = AudioSegmenter::new(a, 2.0);
        // 2 s of AAC at 48 kHz: 1024 samples per frame, 1920 ticks of 90 kHz.
        // 98 frames is 2.09 s — the video segment closes on the first IDR at
        // or after 2 s, so audio for a little past the target is normal and
        // the exact overshoot depends on where the source's IDRs fall.
        const FRAMES: u64 = 98;
        for i in 0..FRAMES {
            s.push(&[0xFF; 200], i * 1920);
        }
        // The video segment closes on its IDR and drains the audio buffer.
        let (_seq, _base, samples) = s
            .take_pending_samples(FRAMES * 1024)
            .expect("a segment of audio should be waiting");
        assert_eq!(
            samples.len(),
            FRAMES as usize,
            "the muxed fragment must carry every audio frame of the segment"
        );
    }

    /// The audio traf must be anchored on its own first sample, and frames
    /// from after the video cut must stay for the next fragment.
    ///
    /// Anchoring on the boundary instead declares the audio starting earlier
    /// than it does — a fixed A/V offset for the life of the flow — and
    /// draining past the boundary gives the last sample a zero duration.
    #[test]
    fn audio_split_anchors_on_its_own_first_sample() {
        let a = AudioTrack::aac([0x11, 0x90], 48000, 2, 128_000);
        let mut s = AudioSegmenter::new(a, 2.0);
        // 10 frames on the 1024-sample grid: DTS 0, 1024, ... 9216.
        for i in 0..10u64 {
            s.push(&[0xFF; 200], i * 1920);
        }
        // The video segment closes between frame 4 (4096) and frame 5 (5120).
        let (_seq, base, samples) = s.take_pending_samples(5000).expect("audio waiting");
        assert_eq!(base, 0, "first fragment anchors on its first sample");
        assert_eq!(samples.len(), 5, "only frames before the cut belong here");
        assert!(
            samples.iter().all(|s| s.duration > 0),
            "a sample drained past the boundary would have zero duration"
        );
        // To its successor's DTS (5120), NOT to the 5000 video boundary. See
        // `take_pending_samples`: truncating here opens a hole before the next
        // fragment, and Chrome answers a hole by dropping video to the next
        // IDR — two seconds of frozen picture over playing audio.
        assert_eq!(
            samples[4].duration,
            (5120 - 4096) as u32,
            "the last frame must reach the frame that follows it, not the video cut"
        );

        // The next fragment starts at the first frame that was left queued —
        // 5120, not the 5000 boundary.
        let (_seq, base, samples) = s.take_pending_samples(20_000).expect("audio waiting");
        assert_eq!(base, 5120, "second fragment anchors on its own first sample");
        assert_eq!(samples.len(), 5);
    }

    /// Consecutive fragments must tile the audio timeline exactly.
    ///
    /// This is the guard for the frozen-picture bug. A seam of even a few
    /// milliseconds is read by Chrome's MSE as a discontinuity when the frame
    /// before it was truncated — and a discontinuity on the audio track sets
    /// "need random access point" on the VIDEO track that shares the source
    /// buffer, so the picture stops until the next IDR while the sound plays
    /// on. It took a frame-presentation trace to see it: every counter the
    /// player and the browser expose read healthy throughout.
    ///
    /// Video cuts land wherever an IDR falls, so the boundaries here are
    /// deliberately off the audio grid — that misalignment is the whole point.
    #[test]
    fn consecutive_audio_fragments_leave_no_seam() {
        let a = AudioTrack::aac([0x11, 0x90], 48000, 2, 128_000);
        let mut s = AudioSegmenter::new(a, 2.0);
        for i in 0..120u64 {
            s.push(&[0xFF; 200], i * 1920); // 1024 samples @48k == 1920 ticks @90k
        }

        // Boundaries that do not fall on frame edges, as real IDR cuts do not.
        let mut end: Option<u64> = None;
        let mut fragments = 0;
        for boundary in [5000u64, 11_300, 17_900, 26_000, 33_333, 40_000] {
            let Some((_seq, base, samples)) = s.take_pending_samples(boundary) else {
                continue;
            };
            fragments += 1;
            if let Some(prev_end) = end {
                assert_eq!(
                    base, prev_end,
                    "fragment starts at {base} but the one before ended at {prev_end} — \
                     a {} tick hole that costs the viewer a segment of picture",
                    base as i64 - prev_end as i64
                );
            }
            assert!(
                samples.iter().all(|x| x.duration > 0),
                "every audio sample must have a real duration"
            );
            end = Some(base + samples.iter().map(|x| x.duration as u64).sum::<u64>());
        }
        assert_eq!(fragments, 6, "every boundary has audio queued before it");
        // Frame 40 (at 40 960) sits past the 40 000 cut and stays queued, so the
        // last fragment ends exactly where it starts.
        assert_eq!(end, Some(40 * 1024));
    }

    /// The frame with nothing behind it runs one real frame, on every rate.
    ///
    /// Only the successor branch was pinned: reverting the fallback alone —
    /// ending the last frame at the video cut — passed every test, and that
    /// is the branch a fragment takes whenever the buffer is drained to the
    /// cut, which the muxed path does on every segment the audio has not run
    /// ahead of the video.
    #[test]
    fn a_last_frame_with_no_successor_runs_one_frame() {
        for (rate, samples_per_frame) in [(48_000u32, 1024u64), (44_100, 1024)] {
            let a = AudioTrack::aac([0x11, 0x90], rate, 2, 128_000);
            let mut s = AudioSegmenter::new(a, 2.0);
            let ticks_per_frame = samples_per_frame * 90_000 / rate as u64;
            for i in 0..10u64 {
                s.push(&[0xFF; 200], i * ticks_per_frame);
            }
            // Everything queued is before the boundary: no successor.
            let (_, base, out) = s.take_pending_samples(u64::MAX / 2).expect("a fragment");
            assert_eq!(out.len(), 10);
            let stride = out[0].duration as u64;
            assert!(
                (stride as i64 - samples_per_frame as i64).abs() <= 1,
                "{rate} Hz: one frame is {samples_per_frame} samples, got {stride}"
            );
            assert!(
                (out[9].duration as i64 - stride as i64).abs() <= 1,
                "{rate} Hz: the last frame must run a real frame, not to the cut: {}",
                out[9].duration
            );
            assert_eq!(base, 0);
        }
    }

    /// A gap before the last frame is not copied onto it as its duration.
    #[test]
    fn a_source_gap_does_not_stretch_the_last_frame() {
        let a = AudioTrack::aac([0x11, 0x90], 48_000, 2, 128_000);
        let mut s = AudioSegmenter::new(a, 2.0);
        for i in 0..5u64 {
            s.push(&[0xFF; 200], i * 1920);
        }
        // Ten frames lost, then one more.
        s.push(&[0xFF; 200], 15 * 1920);
        let (_, _, out) = s.take_pending_samples(u64::MAX / 2).expect("a fragment");
        assert_eq!(out.len(), 6);
        assert_eq!(out[4].duration, 11 * 1024, "the gap itself is real and stays");
        assert_eq!(out[5].duration, 1024, "the frame after it is one frame long");
    }

    #[test]
    fn audio_take_pending_for_muxing() {
        let a = AudioTrack::aac([0x11, 0x90], 48000, 2, 128_000);
        let mut s = AudioSegmenter::new(a, 2.0);
        for i in 0..10 {
            s.push(&[0xFF; 100], (i * 1920) as u64);
        }
        let result = s.take_pending_samples(20_000);
        assert!(result.is_some());
        let (seq, _base, samples) = result.unwrap();
        assert_eq!(seq, 0);
        assert_eq!(samples.len(), 10);
    }
}
