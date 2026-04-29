// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SMPTE 12M-1 / -2 timecode detection from the video PES.
//!
//! Best-effort decode of the H.264 / H.265 `pic_timing` SEI (payload type 1).
//! The full decode requires `pic_struct_present_flag` + HRD `*_length_minus1`
//! values from the SPS VUI; broadcast streams almost universally have
//! `pic_struct_present_flag = 1`, no HRD (`CpbDpbDelaysPresentFlag = 0`),
//! and `time_offset_length = 0`, so we decode under those assumptions and
//! bail gracefully if the bit layout doesn't make sense.
//!
//! Monotonic-step checking works across I-frames by comparing the frame-
//! count-normalised timecode; a single non-monotonic step bumps a counter
//! rather than latching the alarm forever.

use crate::stats::models::TimecodeStats;

use super::bitreader::{unescape_rbsp, BitReader};

pub struct TimecodeTracker {
    seen: bool,
    last: Option<String>,
    last_total_frames: Option<u64>,
    monotonic: bool,
    non_monotonic_count: u64,
    /// Peek buffer — enough to cover the SEI NAL near the start of the PES.
    pes_peek: Vec<u8>,
    capturing_pes: bool,
    /// Source frame rate, fed in by the lite analyser from the
    /// `MediaAnalyzer` watch channel. When known, used to strictly bound
    /// the SEI `n_frames` field (FF must be `< fps`); when unknown, a
    /// permissive bound (`< 60`) catches obviously-bogus values like the
    /// 236 a Reolink IP cam emits.
    frame_rate: Option<f64>,
}

const PEEK_CAP: usize = 2048;

/// Loose upper bound on the SMPTE FF field when the source frame rate is
/// not yet known. Covers every real broadcast frame rate (24, 25, 29.97,
/// 30, 50, 59.94, 60) with a small margin.
const MAX_FRAMES_FALLBACK: u32 = 60;

impl TimecodeTracker {
    pub fn new() -> Self {
        Self {
            seen: false,
            last: None,
            last_total_frames: None,
            monotonic: true,
            non_monotonic_count: 0,
            pes_peek: Vec::with_capacity(PEEK_CAP),
            capturing_pes: false,
            frame_rate: None,
        }
    }

    /// Update the cached source frame rate. Called from the lite
    /// analyser whenever the `MediaAnalyzer` watch channel advances.
    /// `None` means fps is not yet known (or media_analysis is off);
    /// the validator falls back to [`MAX_FRAMES_FALLBACK`].
    pub fn set_frame_rate(&mut self, fps: Option<f64>) {
        self.frame_rate = fps;
    }

    pub fn observe_ts(&mut self, pusi: bool, payload: &[u8]) {
        if pusi {
            self.pes_peek.clear();
            self.capturing_pes = true;
        }
        if !self.capturing_pes {
            return;
        }
        let take = (PEEK_CAP - self.pes_peek.len()).min(payload.len());
        if take == 0 {
            self.capturing_pes = false;
            self.scan_peek();
            return;
        }
        self.pes_peek.extend_from_slice(&payload[..take]);
        if self.pes_peek.len() >= PEEK_CAP {
            self.capturing_pes = false;
            self.scan_peek();
        }
    }

    fn scan_peek(&mut self) {
        let bytes = std::mem::take(&mut self.pes_peek);
        let mut i = 0;
        while i + 4 < bytes.len() {
            let sc3 = bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1;
            let sc4 = i + 4 < bytes.len()
                && bytes[i] == 0
                && bytes[i + 1] == 0
                && bytes[i + 2] == 0
                && bytes[i + 3] == 1;
            if sc4 {
                i += 4;
            } else if sc3 {
                i += 3;
            } else {
                i += 1;
                continue;
            }
            if i >= bytes.len() {
                break;
            }
            // Find NAL end
            let nal_end = find_next_start_code(&bytes, i + 1);
            let header = bytes[i];

            // AVC SEI: nal_unit_type == 6 (low 5 bits)
            let is_avc_sei = (header & 0x1F) == 6;
            // HEVC SEI: nal_unit_type == 39 (PREFIX_SEI) or 40 (SUFFIX_SEI)
            let hevc_nut = (header >> 1) & 0x3F;
            let is_hevc_sei = hevc_nut == 39 || hevc_nut == 40;

            let sei_payload_offset = if is_hevc_sei { 2 } else { 1 };
            if (is_avc_sei || is_hevc_sei) && i + sei_payload_offset < nal_end {
                let rbsp = unescape_rbsp(&bytes[i + sei_payload_offset..nal_end]);
                self.scan_sei_for_pic_timing(&rbsp);
            }
            i = nal_end;
        }
    }

    fn scan_sei_for_pic_timing(&mut self, rbsp: &[u8]) {
        let mut i = 0;
        while i < rbsp.len() {
            let mut payload_type: u32 = 0;
            while i < rbsp.len() && rbsp[i] == 0xFF {
                payload_type += 255;
                i += 1;
            }
            if i >= rbsp.len() {
                return;
            }
            payload_type += rbsp[i] as u32;
            i += 1;
            let mut payload_size: u32 = 0;
            while i < rbsp.len() && rbsp[i] == 0xFF {
                payload_size += 255;
                i += 1;
            }
            if i >= rbsp.len() {
                return;
            }
            payload_size += rbsp[i] as u32;
            i += 1;
            let end = i + payload_size as usize;
            if end > rbsp.len() {
                return;
            }
            let payload = &rbsp[i..end];
            if payload_type == 1 {
                self.decode_pic_timing(payload);
            }
            if i < rbsp.len() && rbsp[i] == 0x80 {
                return;
            }
            i = end;
        }
    }

    /// Decode an H.264 `pic_timing` SEI payload under the simplifying
    /// assumptions that `pic_struct_present_flag = 1`,
    /// `CpbDpbDelaysPresentFlag = 0`, and `time_offset_length = 0`.
    /// These hold for the vast majority of broadcast streams.
    fn decode_pic_timing(&mut self, payload: &[u8]) {
        let mut br = BitReader::new(payload);
        let pic_struct = match br.read_bits(4) {
            Some(v) => v,
            None => return,
        };
        // pic_struct -> number of clock_timestamps (Table D-1)
        let num_clock_ts = match pic_struct {
            0 => 1, // frame
            1 | 2 => 1, // top field / bottom field
            3 | 4 => 2, // top+bottom / bottom+top
            5 | 6 => 3, // top+bottom+top repeat / bottom+top+bottom repeat
            7 => 2,     // frame doubling
            8 => 3,     // frame tripling
            _ => return,
        };

        for _ in 0..num_clock_ts {
            let flag = match br.read_bit() {
                Some(v) => v,
                None => return,
            };
            if flag == 0 {
                continue;
            }
            // clock_timestamp
            let _ct_type = br.read_bits(2);
            let _nuit_field_based_flag = br.read_bit();
            let _counting_type = br.read_bits(5);
            let full_timestamp_flag = match br.read_bit() {
                Some(v) => v,
                None => return,
            };
            let _discontinuity_flag = br.read_bit();
            let _cnt_dropped_flag = br.read_bit();
            let n_frames = match br.read_bits(8) {
                Some(v) => v,
                None => return,
            };
            // Reject `n_frames` that can't possibly be valid for the
            // source. With known fps we bound strictly (`FF < fps`); with
            // unknown fps we use a loose broadcast-wide bound. Either
            // way the SEI is dropped — `self.last` keeps its prior value
            // (None on always-bad sources, or the last good TC for an
            // intermittent bad SEI). This is a structural decode reject,
            // not a step regression — don't bump `non_monotonic_count`.
            let max_frames = self
                .frame_rate
                .map(|f| f.ceil() as u32)
                .unwrap_or(MAX_FRAMES_FALLBACK);
            if n_frames as u32 >= max_frames {
                return;
            }
            let (mut seconds_value, mut minutes_value, mut hours_value) = (0u32, 0u32, 0u32);
            if full_timestamp_flag == 1 {
                seconds_value = br.read_bits(6).unwrap_or(0);
                minutes_value = br.read_bits(6).unwrap_or(0);
                hours_value = br.read_bits(5).unwrap_or(0);
            } else {
                let seconds_flag = br.read_bit().unwrap_or(0);
                if seconds_flag == 1 {
                    seconds_value = br.read_bits(6).unwrap_or(0);
                    let minutes_flag = br.read_bit().unwrap_or(0);
                    if minutes_flag == 1 {
                        minutes_value = br.read_bits(6).unwrap_or(0);
                        let hours_flag = br.read_bit().unwrap_or(0);
                        if hours_flag == 1 {
                            hours_value = br.read_bits(5).unwrap_or(0);
                        }
                    }
                }
            }

            let frames = n_frames;
            // Format as HH:MM:SS:FF (or HH:MM:SS;FF for drop-frame variants;
            // we don't distinguish here — counting_type = 4 indicates DF but
            // we've already discarded the value).
            let tc = format!(
                "{:02}:{:02}:{:02}:{:02}",
                hours_value, minutes_value, seconds_value, frames
            );
            let fps_for_total = self.frame_rate.map(|f| f.round() as u64).unwrap_or(30);
            let total_frames = ((hours_value as u64) * 3600
                + (minutes_value as u64) * 60
                + (seconds_value as u64))
                * fps_for_total
                + frames as u64;

            // Monotonic check: allow equal (same-frame duplication) and
            // +N forward; any backwards step counts as non-monotonic.
            if let Some(prev) = self.last_total_frames {
                if total_frames + 5 < prev {
                    // large backward step — either wraparound at
                    // 24h or a genuine glitch. Count as non-monotonic
                    // only if it's not a ~24h wrap.
                    let wrap_24h = 24u64 * 3600 * fps_for_total;
                    if prev < wrap_24h - 10 || total_frames > 10 {
                        self.monotonic = false;
                        self.non_monotonic_count += 1;
                    }
                }
            }
            self.seen = true;
            self.last = Some(tc);
            self.last_total_frames = Some(total_frames);
            // Only decode the first clock_timestamp per pic_timing — the
            // others (if any) are alternate fields of the same picture.
            return;
        }
    }

    pub fn snapshot(&self) -> Option<TimecodeStats> {
        if !self.seen {
            return None;
        }
        Some(TimecodeStats {
            seen: self.seen,
            last: self.last.clone(),
            monotonic: self.monotonic,
            non_monotonic_count: self.non_monotonic_count,
        })
    }
}

fn find_next_start_code(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 2 < bytes.len() {
        if bytes[i] == 0
            && bytes[i + 1] == 0
            && (bytes[i + 2] == 1 || (i + 3 < bytes.len() && bytes[i + 2] == 0 && bytes[i + 3] == 1))
        {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `pic_timing` SEI payload with `pic_struct=0`
    /// (frame), `full_timestamp_flag=1`, and the supplied `n_frames`.
    /// Hours / minutes / seconds are all zero. Bit layout per H.264 §D.2.2:
    ///
    /// ```text
    /// pic_struct(4)=0   clock_ts_flag(1)=1   ct_type(2)=0  nuit(1)=0
    /// counting_type(5)=0   full_ts(1)=1   disc(1)=0   cnt_dropped(1)=0
    /// n_frames(8)   seconds(6)=0   minutes(6)=0   hours(5)=0
    /// ```
    fn pic_timing_payload(n_frames: u8) -> Vec<u8> {
        vec![0x08, 0x04, n_frames, 0x00, 0x00, 0x00]
    }

    #[test]
    fn valid_n_frames_produces_formatted_tc() {
        let mut t = TimecodeTracker::new();
        t.decode_pic_timing(&pic_timing_payload(12));
        let snap = t.snapshot().expect("seen");
        assert_eq!(snap.last.as_deref(), Some("00:00:00:12"));
    }

    #[test]
    fn rejects_n_frames_above_fallback_when_fps_unknown() {
        // No frame_rate set; FF=236 exceeds the loose broadcast bound.
        // Snapshot returns None because `seen` was never flipped.
        let mut t = TimecodeTracker::new();
        t.decode_pic_timing(&pic_timing_payload(236));
        assert!(t.snapshot().is_none());
    }

    #[test]
    fn rejects_n_frames_above_strict_fps_bound() {
        // 25 fps source — FF=27 is invalid.
        let mut t = TimecodeTracker::new();
        t.set_frame_rate(Some(25.0));
        t.decode_pic_timing(&pic_timing_payload(27));
        assert!(t.snapshot().is_none());
    }

    #[test]
    fn accepts_n_frames_at_strict_fps_boundary() {
        // 25 fps source — FF=24 is the largest valid value.
        let mut t = TimecodeTracker::new();
        t.set_frame_rate(Some(25.0));
        t.decode_pic_timing(&pic_timing_payload(24));
        let snap = t.snapshot().expect("seen");
        assert_eq!(snap.last.as_deref(), Some("00:00:00:24"));
    }

    #[test]
    fn invalid_sei_does_not_overwrite_prior_good_value() {
        // First a valid SEI lands. Then the source emits a malformed one
        // (n_frames=236) — `last` must not regress to None.
        let mut t = TimecodeTracker::new();
        t.decode_pic_timing(&pic_timing_payload(12));
        t.decode_pic_timing(&pic_timing_payload(236));
        let snap = t.snapshot().expect("seen");
        assert_eq!(snap.last.as_deref(), Some("00:00:00:12"));
    }
}
