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

/// One completed media segment to upload.
pub struct CompletedSegment {
    pub sequence_number: u64,
    pub duration_90k: u64,
    pub bytes: Vec<u8>,
    pub kind: SegmentKind,
    /// 90 kHz DTS of the first sample in the segment.
    pub base_dts_90k: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Video,
    Audio,
}

// ────────────────────────────────────────────────────────────────────────
//  Video segmenter (passthrough)
// ────────────────────────────────────────────────────────────────────────

pub struct VideoSegmenter {
    pub track: VideoTrack,
    target_duration_90k: u64,
    pts_unwrap: PtsUnwrap,
    samples: Vec<PendingVideoSample>,
    segment_base_dts: Option<u64>,
    next_seq: u64,
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
    /// target). The bytes inside are the *video-only* fMP4 segment;
    /// callers that want a muxed segment should drain the audio buffer
    /// concurrently.
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
    pub fn new(track: VideoTrack, target_duration_secs: f64) -> Self {
        let target_duration_90k = (target_duration_secs * 90_000.0) as u64;
        Self {
            track,
            target_duration_90k,
            pts_unwrap: PtsUnwrap::new(),
            samples: Vec::new(),
            segment_base_dts: None,
            next_seq: 0,
        }
    }

    /// Push one access unit. Returns `PushOutcome` describing any
    /// segment that closed and whether a new segment started.
    pub fn push(
        &mut self,
        nalus: &[Vec<u8>],
        pts90k: u64,
        is_keyframe: bool,
    ) -> PushOutcome {
        let unwrapped = self.pts_unwrap.unwrap(pts90k);
        let dts = unwrapped;

        let mut completed = None;
        let mut completed_samples: Option<(u64, u64, Vec<Sample>)> = None;
        let mut new_segment_started = false;
        if is_keyframe {
            if let Some(base) = self.segment_base_dts {
                let elapsed = dts.saturating_sub(base);
                if elapsed >= self.target_duration_90k && !self.samples.is_empty() {
                    let (seq, sb, samples) = self.snapshot_samples(dts);
                    completed = Some(CompletedSegment {
                        sequence_number: seq,
                        duration_90k: dts.saturating_sub(sb),
                        bytes: super::fmp4::build_video_segment(seq as u32, sb, &samples),
                        kind: SegmentKind::Video,
                        base_dts_90k: sb,
                    });
                    completed_samples = Some((seq, sb, samples));
                    self.samples.clear();
                    self.segment_base_dts = Some(dts);
                    new_segment_started = true;
                }
            }
            if self.segment_base_dts.is_none() {
                self.segment_base_dts = Some(dts);
                new_segment_started = true;
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

impl AudioSegmenter {
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
    pub fn push(&mut self, frame: &[u8], pts90k: u64) -> Option<CompletedSegment> {
        let unwrapped = self.pts_unwrap.unwrap(pts90k);
        let dts_ts = unwrapped * self.track.sample_rate as u64 / 90_000;

        self.segment_base_dts.get_or_insert(dts_ts);
        self.samples.push_back(PendingAudioSample {
            dts_ts,
            data: frame.to_vec(),
        });

        if let Some(base) = self.segment_base_dts {
            let elapsed = dts_ts.saturating_sub(base);
            if elapsed >= self.target_duration_ts {
                return self.flush_current_segment(dts_ts + 1024);
            }
        }
        None
    }

    /// Drain pending audio frames as a `Sample` vector for a muxed
    /// segment. Returns `(sequence_number, base_dts_ts, samples)`.
    pub fn take_pending_samples(
        &mut self,
        boundary_dts_ts: u64,
    ) -> Option<(u64, u64, Vec<Sample>)> {
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
                    .unwrap_or(boundary_dts_ts);
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
        self.segment_base_dts = Some(boundary_dts_ts);
        Some((seq, base, samples_out))
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
        let outcome = s.push(&p_frame, 0, false);
        assert!(outcome.completed_video.is_none());
        assert!(!outcome.new_segment_started);
        assert!(s.samples.is_empty());
        let idr = vec![vec![0x65, 0xB8]];
        let outcome = s.push(&idr, 3000, true);
        assert!(outcome.completed_video.is_none());
        assert!(outcome.new_segment_started);
        assert_eq!(s.samples.len(), 1);
    }

    #[test]
    fn video_segmenter_cuts_on_next_idr_after_target() {
        let v = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
        let mut s = VideoSegmenter::new(v, 2.0);
        let idr = vec![vec![0x65, 0xB8]];
        let p = vec![vec![0x41, 0x00]];
        s.push(&idr, 0, true);
        for i in 1..60 {
            let pts = (i * 3000) as u64;
            assert!(s.push(&p, pts, false).completed_video.is_none(), "frame {i}");
        }
        let boundary_pts = 2 * 90_000;
        let outcome = s.push(&idr, boundary_pts, true);
        let seg = outcome.completed_video.expect("segment should cut here");
        assert_eq!(seg.sequence_number, 0);
        assert!(seg.duration_90k > 0);
        assert!(outcome.new_segment_started);
        assert_eq!(seg.kind, SegmentKind::Video);
        assert_eq!(s.samples.len(), 1);
    }

    #[test]
    fn audio_segmenter_cuts_on_target_duration() {
        let a = AudioTrack {
            audio_specific_config: [0x11, 0x90],
            sample_rate: 48000,
            channels: 2,
            avg_bitrate: 128_000,
        };
        let mut s = AudioSegmenter::new(a, 2.0);
        let data = vec![0xFF; 200];
        let mut seg = None;
        for i in 0..100 {
            let pts = (i * 1920) as u64;
            if let Some(s) = s.push(&data, pts) {
                seg = Some(s);
                break;
            }
        }
        let seg = seg.expect("audio segment should flush");
        assert_eq!(seg.kind, SegmentKind::Audio);
        assert!(seg.duration_90k > 0);
    }

    #[test]
    fn audio_take_pending_for_muxing() {
        let a = AudioTrack {
            audio_specific_config: [0x11, 0x90],
            sample_rate: 48000,
            channels: 2,
            avg_bitrate: 128_000,
        };
        let mut s = AudioSegmenter::new(a, 2.0);
        for i in 0..10 {
            s.push(&vec![0xFF; 100], (i * 1920) as u64);
        }
        let result = s.take_pending_samples(20_000);
        assert!(result.is_some());
        let (seq, _base, samples) = result.unwrap();
        assert_eq!(seq, 0);
        assert_eq!(samples.len(), 10);
    }
}
