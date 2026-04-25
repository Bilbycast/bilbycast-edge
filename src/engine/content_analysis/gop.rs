// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! GOP structure detection from the video PES.
//!
//! Phase 1 implements frame counting and IDR detection for H.264 / H.265 /
//! MPEG-2. Finer-grained I / P / B splitting (which requires slice-header
//! Exp-Golomb decoding in H.264 or the `nuh_layer_id` / `TemporalId` path
//! in H.265) lands in a follow-up; the `i_count` / `p_count` / `b_count`
//! fields stay at zero in this release so operators reading the number
//! don't get misled.
//!
//! Every TS payload on the video PID is streamed into an internal PES
//! reassembler; every time a new PES starts (TS PUSI), we inspect the
//! first few bytes of the elementary-stream payload for NAL / slice
//! start codes.
//!
//! Closed-GOP detection (`closed_gop`) requires GOP-header parsing
//! (MPEG-2) or SPS `frame_mbs_only_flag` + recovery-point SEI (H.264) —
//! deferred.

use crate::stats::models::GopStats;

pub struct GopTracker {
    video_pid: Option<u16>,
    codec: Codec,
    idr_count: u64,
    frame_count: u64,
    last_idr_frame_index: Option<u64>,
    idr_interval_sum: u64,
    idr_interval_samples: u64,
    /// Rolling PES buffer — only the first few NAL units of the first PES
    /// after PUSI are inspected, so we cap the capture.
    pes_peek: Vec<u8>,
    capturing_pes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Unknown,
    Mpeg2,
    Avc,
    Hevc,
    Other,
}

impl Codec {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Mpeg2 => "mpeg2",
            Self::Avc => "h264",
            Self::Hevc => "h265",
            Self::Other => "other",
        }
    }
}

const PEEK_CAP: usize = 128;

impl GopTracker {
    pub fn new() -> Self {
        Self {
            video_pid: None,
            codec: Codec::Unknown,
            idr_count: 0,
            frame_count: 0,
            last_idr_frame_index: None,
            idr_interval_sum: 0,
            idr_interval_samples: 0,
            pes_peek: Vec::with_capacity(PEEK_CAP),
            capturing_pes: false,
        }
    }

    pub fn set_codec_from_stream_type(&mut self, stream_type: u8) {
        self.codec = match stream_type {
            0x01 | 0x02 => Codec::Mpeg2,
            0x1B => Codec::Avc,
            0x24 | 0x27 => Codec::Hevc,
            _ => Codec::Other,
        };
    }

    pub fn observe_ts(&mut self, pusi: bool, payload: &[u8]) {
        if pusi {
            // New PES starts. Finalise the previous peek (we've already
            // scanned what we captured) and start a new one.
            self.pes_peek.clear();
            self.capturing_pes = true;
            self.frame_count += 1;
        }
        if !self.capturing_pes {
            return;
        }
        // Skip PES header if we're at the very start.
        let start = if self.pes_peek.is_empty() { pes_payload_offset(payload) } else { 0 };
        if start >= payload.len() {
            return;
        }
        let take = (PEEK_CAP - self.pes_peek.len()).min(payload.len() - start);
        if take == 0 {
            self.capturing_pes = false;
            self.scan_peek();
            return;
        }
        self.pes_peek.extend_from_slice(&payload[start..start + take]);
        if self.pes_peek.len() >= PEEK_CAP {
            self.capturing_pes = false;
            self.scan_peek();
        }
    }

    fn scan_peek(&mut self) {
        // Take ownership of the peek buffer so the scan loop doesn't hold an
        // immutable borrow on `self` while we mutate running counters. The
        // next PES starts with an explicit `clear()`, so discarding the
        // buffer at scan-end is safe.
        let bytes = std::mem::take(&mut self.pes_peek);
        let mut i = 0;
        while i + 4 <= bytes.len() {
            // Start code: 0x000001 (3 bytes) or 0x00000001 (4 bytes)
            let is_sc3 = bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1;
            let is_sc4 = i + 4 <= bytes.len()
                && bytes[i] == 0
                && bytes[i + 1] == 0
                && bytes[i + 2] == 0
                && bytes[i + 3] == 1;
            if is_sc4 {
                let nal = bytes[i + 4];
                self.classify_nal(nal);
                i += 5;
                continue;
            }
            if is_sc3 {
                let nal = bytes[i + 3];
                self.classify_nal(nal);
                i += 4;
                continue;
            }
            i += 1;
        }
    }

    fn classify_nal(&mut self, nal: u8) {
        match self.codec {
            Codec::Avc => {
                // AVC NAL header: forbidden_zero(1) + nal_ref_idc(2) + nal_unit_type(5)
                let nut = nal & 0x1F;
                if nut == 5 {
                    self.record_idr();
                }
            }
            Codec::Hevc => {
                // HEVC NAL header: forbidden_zero(1) + nal_unit_type(6) + layer_id(6) + tid+1(3)
                // But this helper sees only the first byte — enough to
                // extract nal_unit_type from the top 7 bits.
                let nut = (nal >> 1) & 0x3F;
                // IDR_W_RADL (19), IDR_N_LP (20), CRA_NUT (21), BLA_* (16-18)
                if (16..=21).contains(&nut) {
                    self.record_idr();
                }
            }
            Codec::Mpeg2 => {
                // MPEG-2 start code 0x00 = picture_start_code; 0xB3 = sequence_header;
                // 0xB8 = group_of_pictures_start. GOP start ⇒ treat as IDR-equivalent.
                if nal == 0xB8 {
                    self.record_idr();
                }
            }
            _ => {}
        }
    }

    fn record_idr(&mut self) {
        self.idr_count += 1;
        let current = self.frame_count;
        if let Some(prev) = self.last_idr_frame_index {
            let interval = current.saturating_sub(prev);
            if interval > 0 && interval < 10_000 {
                self.idr_interval_sum += interval;
                self.idr_interval_samples += 1;
            }
        }
        self.last_idr_frame_index = Some(current);
    }

    pub fn snapshot(&self, video_pid: Option<u16>) -> Option<GopStats> {
        if self.codec == Codec::Unknown && self.frame_count == 0 {
            return None;
        }
        let idr_interval_frames = if self.idr_interval_samples > 0 {
            Some((self.idr_interval_sum as f32) / (self.idr_interval_samples as f32))
        } else {
            None
        };
        Some(GopStats {
            video_pid: video_pid.or(self.video_pid),
            codec: self.codec.as_str().into(),
            idr_count: self.idr_count,
            i_count: 0,
            p_count: 0,
            b_count: 0,
            idr_interval_frames,
            closed_gop: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_before_any_data() {
        let g = GopTracker::new();
        assert!(g.snapshot(None).is_none());
    }

    #[test]
    fn avc_idr_nal_increments_idr_count() {
        let mut g = GopTracker::new();
        g.set_codec_from_stream_type(0x1B); // H.264
        // Directly poke the peek buffer with a NAL start code + type 5 (IDR).
        g.pes_peek = vec![0x00, 0x00, 0x00, 0x01, 0x25, 0x00, 0x00, 0x00];
        g.capturing_pes = true;
        // Simulate frame count bump (first frame)
        g.frame_count += 1;
        g.scan_peek();
        let snap = g.snapshot(Some(0x1234)).expect("snapshot");
        assert_eq!(snap.codec, "h264");
        assert_eq!(snap.idr_count, 1);
    }

    #[test]
    fn hevc_cra_nal_increments_idr_count() {
        let mut g = GopTracker::new();
        g.set_codec_from_stream_type(0x24); // H.265
        // CRA_NUT = 21. HEVC NAL header byte layout: (nal_unit_type << 1) | 1 bit nuh_layer_id_msb
        let byte = (21 << 1) & 0x7E;
        g.pes_peek = vec![0x00, 0x00, 0x01, byte, 0x00];
        g.capturing_pes = true;
        g.frame_count += 1;
        g.scan_peek();
        let snap = g.snapshot(None).expect("snapshot");
        assert_eq!(snap.codec, "h265");
        assert_eq!(snap.idr_count, 1);
    }
}

/// Given a PES-payload-starting TS payload (right after PUSI), return the
/// byte offset at which the elementary-stream starts. Returns 0 on anything
/// we don't recognise so callers can safely scan from the beginning.
fn pes_payload_offset(payload: &[u8]) -> usize {
    if payload.len() < 9 {
        return 0;
    }
    if payload[0] != 0 || payload[1] != 0 || payload[2] != 1 {
        return 0;
    }
    let stream_id = payload[3];
    // Video stream IDs are 0xE0..=0xEF; audio 0xC0..=0xDF. We accept any
    // PES stream id here and let the NAL scan decide.
    if !(0xC0..=0xEF).contains(&stream_id) {
        return 0;
    }
    // PES_header_data_length is at offset 8
    let hdr_len = payload[8] as usize;
    let es_start = 9 + hdr_len;
    if es_start > payload.len() {
        return payload.len();
    }
    es_start
}
