// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-output A/V alignment corrector for TS-carrying outputs.
//!
//! Measures the PTS offset between video and audio elementary streams,
//! then rewrites audio PTS/DTS to compensate. Zero-latency — packets
//! pass through immediately; only the PTS timestamps are adjusted.
//!
//! Corrects baked-in source A/V offset and transcode-induced drift
//! (e.g. AAC encode priming latency, video encode pipeline delay).
//!
//! Single-owner, inline on the output task — same pattern as
//! [`TsNullPadder`](super::ts_null_padder) and
//! [`TsPidRemapper`](super::ts_pid_remapper). No Mutex, no allocation
//! on the hot path.

use std::collections::VecDeque;

use crate::engine::ts_parse::{self, TS_PACKET_SIZE};

const SETTLE_SAMPLES: usize = 32;
const EMA_ALPHA_NUM: i64 = 1;
const EMA_ALPHA_DEN: i64 = 4;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AlignConfig {
    pub alignment_window_ms: u32,
    pub tolerance_ms: u32,
    pub max_hold_ms: u32,
    pub stream_presence_timeout_ms: u32,
    pub capacity_packets: usize,
}

impl Default for AlignConfig {
    fn default() -> Self {
        Self {
            alignment_window_ms: 100,
            tolerance_ms: 5,
            max_hold_ms: 200,
            stream_presence_timeout_ms: 500,
            capacity_packets: 0,
        }
    }
}

#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct AlignBufStatsSnapshot {
    pub alignment_offset_ms: i64,
    pub correction_applied_ms: i64,
    pub pes_corrected: u64,
    pub resets: u64,
    pub mode: &'static str,
}

pub struct TsAvAlignBuffer {
    // PSI discovery
    pmt_pid: Option<u16>,
    last_pat_version: Option<u8>,
    last_pmt_version: Option<u8>,

    // PID classification
    video_pids: Vec<u16>,
    audio_pids: Vec<u16>,

    // Offset measurement: collect V-A deltas during settle phase
    settle_deltas: VecDeque<i64>,
    last_video_pts: Option<u64>,
    last_audio_pts: Option<u64>,

    // Correction state
    correction_90k: i64,
    settled: bool,

    // Stats
    pes_corrected: u64,
    resets: u64,
}

impl TsAvAlignBuffer {
    pub fn new(_config: AlignConfig) -> Self {
        Self {
            pmt_pid: None,
            last_pat_version: None,
            last_pmt_version: None,
            video_pids: Vec::new(),
            audio_pids: Vec::new(),
            settle_deltas: VecDeque::with_capacity(SETTLE_SAMPLES),
            last_video_pts: None,
            last_audio_pts: None,
            correction_90k: 0,
            settled: false,
            pes_corrected: 0,
            resets: 0,
        }
    }

    pub fn initial_capacity(_window_ms: u32) -> usize {
        0
    }

    /// Process TS-sync'd bytes. Output bytes are written to `output`.
    /// Packets pass through immediately; only audio PES PTS/DTS are
    /// rewritten to correct the measured V-A offset.
    pub fn process(&mut self, input: &[u8], output: &mut Vec<u8>) {
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= input.len() {
            let pkt = &input[offset..offset + TS_PACKET_SIZE];
            if pkt[0] != ts_parse::TS_SYNC_BYTE {
                output.push(pkt[0]);
                offset += 1;
                continue;
            }

            let pid = ts_parse::ts_pid(pkt);

            // PAT — discover PMT PID
            if pid == ts_parse::PAT_PID && ts_parse::ts_pusi(pkt) {
                self.handle_pat(pkt);
            }

            // PMT — discover video/audio PIDs
            if Some(pid) == self.pmt_pid && ts_parse::ts_pusi(pkt) {
                self.handle_pmt(pkt);
            }

            // Track video PTS for offset measurement
            if self.video_pids.contains(&pid) && ts_parse::ts_pusi(pkt) {
                if let Some(pts) = ts_parse::extract_pes_pts(pkt) {
                    self.last_video_pts = Some(pts);
                    self.try_measure_offset(false);
                }
            }

            // Audio PES: measure offset AND apply correction
            if self.audio_pids.contains(&pid) && ts_parse::ts_pusi(pkt) {
                if let Some(pts) = ts_parse::extract_pes_pts(pkt) {
                    self.last_audio_pts = Some(pts);
                    self.try_measure_offset(true);
                }

                if self.settled && self.correction_90k != 0 {
                    let mut corrected = [0u8; TS_PACKET_SIZE];
                    corrected.copy_from_slice(pkt);
                    self.apply_pts_correction(&mut corrected);
                    output.extend_from_slice(&corrected);
                    self.pes_corrected += 1;
                    offset += TS_PACKET_SIZE;
                    continue;
                }
            }

            // All other packets pass through unchanged
            output.extend_from_slice(pkt);
            offset += TS_PACKET_SIZE;
        }
    }

    #[allow(dead_code)]
    pub fn flush(&mut self, _output: &mut Vec<u8>) {
        // No buffering — nothing to flush
    }

    #[allow(dead_code)]
    pub fn update_capacity(&mut self, _bitrate_bps: u64) {
        // No ring buffer — nothing to resize
    }

    #[allow(dead_code)]
    pub fn stats(&self) -> AlignBufStatsSnapshot {
        AlignBufStatsSnapshot {
            alignment_offset_ms: self.correction_90k as i64 / 90,
            correction_applied_ms: self.correction_90k as i64 / 90,
            pes_corrected: self.pes_corrected,
            resets: self.resets,
            mode: if self.settled { "correcting" } else { "settling" },
        }
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.settle_deltas.clear();
        self.last_video_pts = None;
        self.last_audio_pts = None;
        self.correction_90k = 0;
        self.settled = false;
        self.resets += 1;
    }

    fn handle_pat(&mut self, pkt: &[u8]) {
        let version = psi_version(pkt);
        if let Some(v) = version {
            if self.last_pat_version == Some(v) {
                return;
            }
            self.last_pat_version = Some(v);
        }
        let programs = ts_parse::parse_pat_programs(pkt);
        if let Some(&(_, pmt_pid)) = programs.first() {
            if self.pmt_pid != Some(pmt_pid) {
                self.pmt_pid = Some(pmt_pid);
                self.video_pids.clear();
                self.audio_pids.clear();
                self.last_pmt_version = None;
                self.reset();
            }
        }
    }

    fn handle_pmt(&mut self, pkt: &[u8]) {
        let version = psi_version(pkt);
        if let Some(v) = version {
            if self.last_pmt_version == Some(v) {
                return;
            }
            self.last_pmt_version = Some(v);
        }

        if let Some((streams, _pcr_pid)) = parse_pmt_streams_with_pcr(pkt) {
            let mut new_video = Vec::new();
            let mut new_audio = Vec::new();
            for &(es_pid, stream_type) in &streams {
                if is_video_stream_type(stream_type) {
                    new_video.push(es_pid);
                } else if is_audio_stream_type(stream_type) {
                    new_audio.push(es_pid);
                }
            }

            if new_video != self.video_pids || new_audio != self.audio_pids {
                self.video_pids = new_video;
                self.audio_pids = new_audio;
                self.reset();
            }
        }
    }

    fn try_measure_offset(&mut self, triggered_by_audio: bool) {
        // Only measure when a new audio PTS arrives — audio is the
        // corrected stream so we want each audio PES to get a fresh
        // delta. Video PTS updates the reference but doesn't trigger
        // measurement on its own.
        if !triggered_by_audio {
            return;
        }
        let (v, a) = match (self.last_video_pts, self.last_audio_pts) {
            (Some(v), Some(a)) => (v, a),
            _ => return,
        };

        // A-V delta in 90 kHz ticks (positive = audio is later than video)
        let delta_90k = pts_delta_90k(a, v);
        let delta_ms = delta_90k / 90;

        // Ignore discontinuities
        if delta_ms.abs() > 500 {
            return;
        }

        if !self.settled {
            self.settle_deltas.push_back(delta_90k);
            if self.settle_deltas.len() >= SETTLE_SAMPLES {
                // Compute median of settle deltas
                let mut sorted: Vec<i64> = self.settle_deltas.iter().copied().collect();
                sorted.sort_unstable();
                let median = sorted[sorted.len() / 2];
                self.correction_90k = median;
                self.settled = true;
                tracing::info!(
                    "A/V align: settled after {} samples, correction={}ms (median={}ms, range={}..{}ms)",
                    SETTLE_SAMPLES,
                    median / 90,
                    median / 90,
                    sorted.first().unwrap_or(&0) / 90,
                    sorted.last().unwrap_or(&0) / 90,
                );
            }
        } else {
            // EMA update to track slow drift
            self.correction_90k += EMA_ALPHA_NUM * (delta_90k - self.correction_90k) / EMA_ALPHA_DEN;
        }
    }

    fn apply_pts_correction(&self, pkt: &mut [u8]) {
        let afc = (pkt[3] >> 4) & 0x03;
        let payload_offset: usize = match afc {
            0b01 => 4,
            0b11 => {
                let af_len = pkt[4] as usize;
                5 + af_len
            }
            _ => return,
        };
        if payload_offset + 14 > TS_PACKET_SIZE {
            return;
        }
        let payload = &pkt[payload_offset..];
        if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
            return;
        }
        let pts_dts_flags = (payload[7] >> 6) & 0x03;

        if pts_dts_flags == 0b10 || pts_dts_flags == 0b11 {
            // Rewrite PTS
            let pts_off = payload_offset + 9;
            if let Some(pts) = extract_pts_from_bytes(&pkt[pts_off..pts_off + 5]) {
                let marker = if pts_dts_flags == 0b10 { 0x20 } else { 0x30 };
                let corrected = (pts as i64 - self.correction_90k).rem_euclid(1i64 << 33) as u64;
                write_pts_5bytes(&mut pkt[pts_off..pts_off + 5], marker, corrected);
            }
        }

        if pts_dts_flags == 0b11 {
            // Rewrite DTS
            let dts_off = payload_offset + 14;
            if dts_off + 5 <= TS_PACKET_SIZE {
                if let Some(dts) = extract_pts_from_bytes(&pkt[dts_off..dts_off + 5]) {
                    let corrected = (dts as i64 - self.correction_90k).rem_euclid(1i64 << 33) as u64;
                    write_pts_5bytes(&mut pkt[dts_off..dts_off + 5], 0x10, corrected);
                }
            }
        }
    }
}

fn extract_pts_from_bytes(p: &[u8]) -> Option<u64> {
    if p.len() < 5 {
        return None;
    }
    let pts: u64 = (((p[0] >> 1) & 0x07) as u64) << 30
        | (p[1] as u64) << 22
        | (((p[2] >> 1) & 0x7F) as u64) << 15
        | (p[3] as u64) << 7
        | ((p[4] >> 1) as u64);
    Some(pts)
}

fn write_pts_5bytes(dst: &mut [u8], marker_top_nibble: u8, value_33bit: u64) {
    let v = value_33bit & 0x1_FFFF_FFFF;
    dst[0] = marker_top_nibble | (((v >> 29) as u8) & 0x0E) | 0x01;
    dst[1] = ((v >> 22) & 0xFF) as u8;
    dst[2] = (((v >> 14) as u8) & 0xFE) | 0x01;
    dst[3] = ((v >> 7) & 0xFF) as u8;
    dst[4] = (((v << 1) as u8) & 0xFE) | 0x01;
}

fn pts_delta_90k(pts_a: u64, pts_b: u64) -> i64 {
    let modulus = 1u64 << 33;
    let a = pts_a & (modulus - 1);
    let b = pts_b & (modulus - 1);
    let raw = a.wrapping_sub(b) & (modulus - 1);
    if raw > modulus / 2 {
        raw as i64 - modulus as i64
    } else {
        raw as i64
    }
}

fn psi_version(pkt: &[u8]) -> Option<u8> {
    let mut offset = 4usize;
    if ts_parse::ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 6 > TS_PACKET_SIZE {
        return None;
    }
    // PSI: table_id(1) + section_length(2) + table_id_extension(2) + version/current(1)
    Some((pkt[offset + 5] >> 1) & 0x1F)
}

fn parse_pmt_streams_with_pcr(pkt: &[u8]) -> Option<(Vec<(u16, u8)>, Option<u16>)> {
    let mut offset = 4usize;
    if ts_parse::ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 {
        return None;
    }
    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let pcr_pid = ((pkt[offset + 8] as u16 & 0x1F) << 8) | pkt[offset + 9] as u16;
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut streams = Vec::new();
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let st = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        streams.push((es_pid, st));
        pos += 5 + es_info_len;
    }
    Some((streams, Some(pcr_pid)))
}

fn is_video_stream_type(st: u8) -> bool {
    matches!(st, 0x01 | 0x02 | 0x1B | 0x24)
}

fn is_audio_stream_type(st: u8) -> bool {
    matches!(st, 0x03 | 0x04 | 0x06 | 0x0F | 0x11 | 0x81 | 0x87)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: usize = TS_PACKET_SIZE;

    fn build_pat(pmt_pid: u16, version: u8) -> [u8; TS] {
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        pkt[1] = 0x40;
        pkt[2] = 0x00;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x00;
        pkt[6] = 0xB0;
        pkt[7] = 13;
        pkt[8] = 0x00; pkt[9] = 0x01;
        pkt[10] = 0xC0 | (version << 1) | 0x01;
        pkt[11] = 0x00; pkt[12] = 0x00;
        pkt[13] = 0x00; pkt[14] = 0x01;
        pkt[15] = 0xE0 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[16] = (pmt_pid & 0xFF) as u8;
        let crc = ts_parse::mpeg2_crc32(&pkt[5..17]);
        pkt[17] = (crc >> 24) as u8;
        pkt[18] = (crc >> 16) as u8;
        pkt[19] = (crc >> 8) as u8;
        pkt[20] = crc as u8;
        pkt
    }

    fn build_pmt(pmt_pid: u16, video_pid: u16, audio_pid: u16, pcr_pid: u16, version: u8) -> [u8; TS] {
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x02;
        pkt[6] = 0xB0;
        pkt[7] = 23;
        pkt[8] = 0x00; pkt[9] = 0x01;
        pkt[10] = 0xC0 | (version << 1) | 0x01;
        pkt[11] = 0x00; pkt[12] = 0x00;
        pkt[13] = 0xE0 | ((pcr_pid >> 8) as u8 & 0x1F);
        pkt[14] = (pcr_pid & 0xFF) as u8;
        pkt[15] = 0xF0; pkt[16] = 0x00;
        pkt[17] = 0x1B;
        pkt[18] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F);
        pkt[19] = (video_pid & 0xFF) as u8;
        pkt[20] = 0xF0; pkt[21] = 0x00;
        pkt[22] = 0x0F;
        pkt[23] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[24] = (audio_pid & 0xFF) as u8;
        pkt[25] = 0xF0; pkt[26] = 0x00;
        let crc = ts_parse::mpeg2_crc32(&pkt[5..27]);
        pkt[27] = (crc >> 24) as u8;
        pkt[28] = (crc >> 16) as u8;
        pkt[29] = (crc >> 8) as u8;
        pkt[30] = crc as u8;
        pkt
    }

    fn build_pes_packet(pid: u16, pts_90k: u64) -> [u8; TS] {
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x00;
        pkt[6] = 0x01;
        pkt[7] = 0xE0;
        pkt[8] = 0x00; pkt[9] = 0x00;
        pkt[10] = 0x80;
        pkt[11] = 0x80; // PTS only
        pkt[12] = 0x05;
        let pts = pts_90k & ((1u64 << 33) - 1);
        write_pts_5bytes(&mut pkt[13..18], 0x20, pts);
        pkt
    }

    #[test]
    fn passthrough_when_no_pmt() {
        let mut buf = TsAvAlignBuffer::new(AlignConfig::default());
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        pkt[1] = 0x01; pkt[2] = 0x00; // PID 0x100, no PUSI
        pkt[3] = 0x10;
        let mut out = Vec::new();
        buf.process(&pkt, &mut out);
        assert_eq!(out.len(), TS);
        assert_eq!(&out[..], &pkt[..]);
    }

    #[test]
    fn pat_pmt_pass_through() {
        let mut buf = TsAvAlignBuffer::new(AlignConfig::default());
        let pat = build_pat(0x1000, 0);
        let pmt = build_pmt(0x1000, 0x100, 0x101, 0x100, 0);
        let mut out = Vec::new();
        buf.process(&pat, &mut out);
        assert_eq!(out.len(), TS);
        buf.process(&pmt, &mut out);
        assert_eq!(out.len(), 2 * TS);
    }

    #[test]
    fn corrects_audio_pts_after_settling() {
        let mut buf = TsAvAlignBuffer::new(AlignConfig::default());
        let pat = build_pat(0x1000, 0);
        let pmt = build_pmt(0x1000, 0x100, 0x101, 0x100, 0);
        let mut out = Vec::new();
        buf.process(&pat, &mut out);
        buf.process(&pmt, &mut out);

        // Feed SETTLE_SAMPLES pairs with a constant 200ms offset
        // (video PTS = 90000, audio PTS = 108000 → audio is 200ms later)
        let offset_90k: u64 = 18000; // 200ms in 90kHz
        for i in 0..SETTLE_SAMPLES as u64 {
            let base = 90000 + i * 3600; // advance by 40ms each pair
            let v = build_pes_packet(0x100, base);
            let a = build_pes_packet(0x101, base + offset_90k);
            buf.process(&v, &mut out);
            buf.process(&a, &mut out);
        }

        assert!(buf.settled, "should be settled after {} samples", SETTLE_SAMPLES);
        assert_eq!(buf.correction_90k, offset_90k as i64);

        // Now send one more pair — audio PTS should be corrected
        out.clear();
        let base = 90000 + SETTLE_SAMPLES as u64 * 3600;
        let v = build_pes_packet(0x100, base);
        let a = build_pes_packet(0x101, base + offset_90k);
        buf.process(&v, &mut out);
        buf.process(&a, &mut out);

        // Extract the corrected audio PTS from the output
        let audio_pkt = &out[TS..2 * TS];
        let corrected_pts = ts_parse::extract_pes_pts(audio_pkt).unwrap();
        let expected_pts = base; // should match video PTS now
        assert_eq!(corrected_pts, expected_pts,
            "audio PTS should be corrected to match video PTS");
    }

    #[test]
    fn no_correction_when_aligned() {
        let mut buf = TsAvAlignBuffer::new(AlignConfig::default());
        let pat = build_pat(0x1000, 0);
        let pmt = build_pmt(0x1000, 0x100, 0x101, 0x100, 0);
        let mut out = Vec::new();
        buf.process(&pat, &mut out);
        buf.process(&pmt, &mut out);

        // Feed aligned pairs (no offset)
        for i in 0..SETTLE_SAMPLES as u64 {
            let base = 90000 + i * 3600;
            let v = build_pes_packet(0x100, base);
            let a = build_pes_packet(0x101, base);
            buf.process(&v, &mut out);
            buf.process(&a, &mut out);
        }

        assert!(buf.settled);
        assert_eq!(buf.correction_90k, 0);

        // Next pair — should pass through unchanged
        out.clear();
        let base = 90000 + SETTLE_SAMPLES as u64 * 3600;
        let a_in = build_pes_packet(0x101, base);
        let v = build_pes_packet(0x100, base);
        buf.process(&v, &mut out);
        buf.process(&a_in, &mut out);

        let audio_out = &out[TS..2 * TS];
        assert_eq!(audio_out, &a_in[..], "aligned audio should not be modified");
    }

    #[test]
    fn null_pid_passes_through() {
        let mut buf = TsAvAlignBuffer::new(AlignConfig::default());
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        pkt[1] = 0x1F; pkt[2] = 0xFF;
        pkt[3] = 0x10;
        let mut out = Vec::new();
        buf.process(&pkt, &mut out);
        assert_eq!(out.len(), TS);
    }

    #[test]
    fn pmt_version_change_resets() {
        let mut buf = TsAvAlignBuffer::new(AlignConfig::default());
        let pat = build_pat(0x1000, 0);
        let pmt_v0 = build_pmt(0x1000, 0x100, 0x101, 0x100, 0);
        let mut out = Vec::new();
        buf.process(&pat, &mut out);
        buf.process(&pmt_v0, &mut out);

        // Settle the correction
        for i in 0..SETTLE_SAMPLES as u64 {
            let base = 90000 + i * 3600;
            buf.process(&build_pes_packet(0x100, base), &mut out);
            buf.process(&build_pes_packet(0x101, base + 9000), &mut out);
        }
        assert!(buf.settled);

        // PMT version change with new PIDs
        let pmt_v1 = build_pmt(0x1000, 0x200, 0x201, 0x200, 1);
        buf.process(&pmt_v1, &mut out);

        assert!(!buf.settled, "PMT change should reset settling");
        assert!(buf.resets >= 1);
    }

    #[test]
    fn pts_round_trip() {
        // Verify write_pts_5bytes + extract round-trips
        let test_values: &[u64] = &[0, 1, 90000, 0x1_FFFF_FFFF, 8_100_000_000];
        for &v in test_values {
            let v = v & 0x1_FFFF_FFFF;
            let mut buf = [0u8; 5];
            write_pts_5bytes(&mut buf, 0x20, v);
            let read = extract_pts_from_bytes(&buf).unwrap();
            assert_eq!(read, v, "PTS round-trip failed for {v}");
        }
    }
}
