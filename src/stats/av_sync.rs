// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-output egress A/V sync drift metric.
//!
//! Measures `video_PES_PTS − audio_PES_PTS` at the egress point for every
//! TS-bearing output. Positive = video late relative to audio, negative =
//! video early. Units are **milliseconds** (A/V sync is meaningful at ms,
//! not µs). EBU R37 thresholds: |p95| > 20 ms = warning, > 40 ms = error.
//!
//! Self-bootstrapping: the sampler watches for PAT → PMT inline and
//! discovers the first video and first audio PID automatically. No
//! external metadata or PID map is needed.
//!
//! Same rotating-reservoir architecture as [`super::pcr_trust`]: 4096
//! signed samples, exact percentiles, natural ageing, bounded work.
//!
//! **Performance**: called per 188-byte TS packet at egress. The common
//! case (PID ≠ PAT, PMT, video, or audio) is a single u16 PID extraction
//! + one branch — sub-nanosecond. PES PTS extraction fires only on
//! PUSI-marked packets on the tracked video/audio PIDs (~50–75 Hz).

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::engine::ts_parse;
use crate::stats::models::AvSyncStats;

pub const AV_SYNC_RESERVOIR_SIZE: usize = 4096;
pub const AV_SYNC_WINDOW_SAMPLES: usize = 256;

/// Samples with |V−A offset| larger than this are treated as straddling
/// a discontinuity (input switch, stream restart, PTS wrap) and discarded.
/// Broadcast MPEG-TS with B-frame reorder can produce mux-position V−A
/// deltas of 1–2 seconds (video muxed ahead of audio by the T-STD buffer
/// depth). 2000 ms accommodates deep-buffered broadcast muxes while still
/// filtering genuine discontinuities.
pub const AV_SYNC_MAX_DRIFT_MS: i64 = 2000;

pub struct AvSyncDriftSampler {
    state: Mutex<AvSyncState>,
}

struct AvSyncState {
    samples: VecDeque<i64>,
    // Inline PSI tracking
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    last_pat_version: Option<u8>,
    last_pmt_version: Option<u8>,
    // Last observed PTS per stream type
    last_video_pts_90k: Option<u64>,
    last_audio_pts_90k: Option<u64>,
    // Cumulative counters (survive reservoir rollover)
    cumulative_drift_sum_ms: i64,
    cumulative_samples: u64,
}

impl AvSyncDriftSampler {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(AvSyncState {
                samples: VecDeque::with_capacity(AV_SYNC_RESERVOIR_SIZE),
                pmt_pid: None,
                video_pid: None,
                audio_pid: None,
                last_pat_version: None,
                last_pmt_version: None,
                last_video_pts_90k: None,
                last_audio_pts_90k: None,
                cumulative_drift_sum_ms: 0,
                cumulative_samples: 0,
            }),
        }
    }

    /// Feed one 188-byte TS packet at egress. Internally tracks PAT/PMT
    /// to discover video/audio PIDs, then measures V−A PTS offset on
    /// PUSI-marked PES packets.
    pub fn observe_packet(&self, pkt: &[u8]) {
        if pkt.len() < ts_parse::TS_PACKET_SIZE || pkt[0] != ts_parse::TS_SYNC_BYTE {
            return;
        }
        let pid = ts_parse::ts_pid(pkt);

        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        // PAT (PID 0) — learn PMT PID
        if pid == ts_parse::PAT_PID && ts_parse::ts_pusi(pkt) {
            if let Some(version) = psi_version(pkt) {
                if state.last_pat_version == Some(version) {
                    return;
                }
                state.last_pat_version = Some(version);
            }
            let programs = ts_parse::parse_pat_programs(pkt);
            if let Some(&(_, pmt_pid)) = programs.first() {
                if state.pmt_pid != Some(pmt_pid) {
                    state.pmt_pid = Some(pmt_pid);
                    // PMT PID changed — reset downstream state
                    state.video_pid = None;
                    state.audio_pid = None;
                    state.last_pmt_version = None;
                    state.last_video_pts_90k = None;
                    state.last_audio_pts_90k = None;
                }
            }
            return;
        }

        // PMT — learn video/audio PIDs
        if let Some(pmt_pid) = state.pmt_pid {
            if pid == pmt_pid && ts_parse::ts_pusi(pkt) {
                if let Some(version) = psi_version(pkt) {
                    if state.last_pmt_version == Some(version) {
                        return;
                    }
                    state.last_pmt_version = Some(version);
                }
                if let Some(streams) = parse_pmt_streams(pkt) {
                    let mut new_video: Option<u16> = None;
                    let mut new_audio: Option<u16> = None;
                    // 0x06 fallbacks, descriptor-discriminated: prefer a
                    // descriptor-confirmed audio ES (DVB AC-3 0x6A /
                    // E-AC-3 0x7A / AAC 0x7C / DTS 0x7B / registration),
                    // then a bare undescribed 0x06; NEVER a teletext /
                    // VBI / subtitling-marked ES (locking onto teletext
                    // as "audio" means the drift metric never sees a
                    // real audio PTS).
                    let mut descriptor_audio: Option<u16> = None;
                    let mut bare_private: Option<u16> = None;
                    for (es_pid, stream_type, es_info) in streams {
                        if new_video.is_none() && is_video_stream_type(stream_type) {
                            new_video = Some(es_pid);
                        }
                        if new_audio.is_none() {
                            if is_unambiguous_audio_stream_type(stream_type) {
                                new_audio = Some(es_pid);
                            } else if stream_type == 0x06 {
                                if ts_parse::descriptor_audio_kind(&es_info).is_some() {
                                    if descriptor_audio.is_none() {
                                        descriptor_audio = Some(es_pid);
                                    }
                                } else if bare_private.is_none()
                                    && !ts_parse::descriptors_indicate_text_service(&es_info)
                                {
                                    bare_private = Some(es_pid);
                                }
                            }
                        }
                        if new_video.is_some() && new_audio.is_some() {
                            break;
                        }
                    }
                    if new_audio.is_none() {
                        new_audio = descriptor_audio.or(bare_private);
                    }
                    if state.video_pid != new_video {
                        state.video_pid = new_video;
                        state.last_video_pts_90k = None;
                    }
                    if state.audio_pid != new_audio {
                        state.audio_pid = new_audio;
                        state.last_audio_pts_90k = None;
                    }
                }
                return;
            }
        }

        // Video PES PTS
        if let Some(vpid) = state.video_pid {
            if pid == vpid && ts_parse::ts_pusi(pkt) {
                if let Some(pts) = ts_parse::extract_pes_pts(pkt) {
                    state.last_video_pts_90k = Some(pts);
                    try_record_drift(&mut state);
                }
                return;
            }
        }

        // Audio PES PTS
        if let Some(apid) = state.audio_pid {
            if pid == apid && ts_parse::ts_pusi(pkt) {
                if let Some(pts) = ts_parse::extract_pes_pts(pkt) {
                    state.last_audio_pts_90k = Some(pts);
                    try_record_drift(&mut state);
                }
            }
        }
    }

    pub fn snapshot(&self) -> Option<AvSyncStats> {
        let state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.samples.is_empty() {
            return None;
        }

        // Compute percentiles on absolute values
        let mut abs_sorted: Vec<i64> = state.samples.iter().map(|s| s.abs()).collect();
        abs_sorted.sort_unstable();

        let p50_abs_ms = abs_percentile(&abs_sorted, 50.0);
        let p95_abs_ms = abs_percentile(&abs_sorted, 95.0);
        let p99_abs_ms = abs_percentile(&abs_sorted, 99.0);
        let max_abs_ms = *abs_sorted.last().unwrap_or(&0);

        // Short-window p95
        let window_len = abs_sorted.len().min(AV_SYNC_WINDOW_SAMPLES);
        let window_p95_abs_ms = if window_len >= 2 {
            let mut window: Vec<i64> = state
                .samples
                .iter()
                .rev()
                .take(window_len)
                .map(|s| s.abs())
                .collect();
            window.sort_unstable();
            abs_percentile(&window, 95.0)
        } else {
            p95_abs_ms
        };

        let avg_ms = if state.cumulative_samples > 0 {
            state.cumulative_drift_sum_ms / state.cumulative_samples as i64
        } else {
            0
        };

        Some(AvSyncStats {
            samples: state.samples.len() as u64,
            cumulative_samples: state.cumulative_samples,
            avg_ms,
            p50_abs_ms,
            p95_abs_ms,
            p99_abs_ms,
            max_abs_ms,
            window_samples: window_len as u64,
            window_p95_abs_ms,
            video_pid: state.video_pid.unwrap_or(0),
            audio_pid: state.audio_pid.unwrap_or(0),
        })
    }
}

impl Default for AvSyncDriftSampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute V−A drift in milliseconds and push into the reservoir if
/// both video and audio PTS are known and the drift is within bounds.
fn try_record_drift(state: &mut AvSyncState) {
    let (v, a) = match (state.last_video_pts_90k, state.last_audio_pts_90k) {
        (Some(v), Some(a)) => (v, a),
        _ => return,
    };

    let drift_ms = pts_delta_ms(v, a);

    if drift_ms.abs() > AV_SYNC_MAX_DRIFT_MS {
        // Discontinuity — reset both PTS trackers
        state.last_video_pts_90k = None;
        state.last_audio_pts_90k = None;
        return;
    }

    if state.samples.len() >= AV_SYNC_RESERVOIR_SIZE {
        state.samples.pop_front();
    }
    state.samples.push_back(drift_ms);
    state.cumulative_drift_sum_ms = state.cumulative_drift_sum_ms.saturating_add(drift_ms);
    state.cumulative_samples = state.cumulative_samples.saturating_add(1);
}

#[inline]
fn pts_delta_ms(video_pts: u64, audio_pts: u64) -> i64 {
    ts_parse::pts_delta_ms(video_pts, audio_pts)
}

/// Linear-interpolated percentile over a sorted slice of absolute
/// values. Returns 0 when the slice is empty. `q` is in [0, 100].
fn abs_percentile(sorted: &[i64], q: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (q / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let frac = rank - lo as f64;
    let a = sorted[lo] as f64;
    let b = sorted[hi] as f64;
    (a + (b - a) * frac).round() as i64
}

/// Read PSI `version_number` (5 bits, 0..=31) from the section header
/// of a PUSI-marked TS packet. Works for PAT (table_id 0x00) and PMT
/// (table_id 0x02).
fn psi_version(pkt: &[u8]) -> Option<u8> {
    let mut offset = 4usize;
    if ts_parse::ts_has_adaptation(pkt) {
        let af_len = *pkt.get(4)? as usize;
        offset = 5 + af_len;
    }
    if offset >= ts_parse::TS_PACKET_SIZE {
        return None;
    }
    let pointer = *pkt.get(offset)? as usize;
    offset += 1 + pointer;
    // version_number is at byte 5 of the section header (bits 5..1)
    let b = *pkt.get(offset + 5)?;
    Some((b >> 1) & 0x1F)
}

/// Walk a PMT section to extract `(es_pid, stream_type)` pairs.
/// Walk PMT ES entries; returns `(es_pid, stream_type, es_info)` with
/// the descriptor bytes copied out so the audio-PID pick can
/// discriminate DVB 0x06 audio from teletext / subtitling. Cold path —
/// runs only on PMT version bumps.
fn parse_pmt_streams(pkt: &[u8]) -> Option<Vec<(u16, u8, Vec<u8>)>> {
    let mut offset = 4usize;
    if ts_parse::ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= ts_parse::TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    // table_id must be 0x02 (PMT)
    if offset + 12 > ts_parse::TS_PACKET_SIZE || pkt[offset] != 0x02 {
        return None;
    }
    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(ts_parse::TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut streams = Vec::new();
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let st = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        let es_info_end = (pos + 5 + es_info_len).min(data_end);
        let es_info = pkt[pos + 5..es_info_end].to_vec();
        streams.push((es_pid, st, es_info));
        pos += 5 + es_info_len;
    }
    Some(streams)
}

fn is_video_stream_type(st: u8) -> bool {
    matches!(st, 0x01 | 0x02 | 0x1B | 0x24)
}

/// Audio stream types that unambiguously identify an audio ES.
/// 0x06 (ISO/IEC 13818-1 private data) is excluded because it is
/// shared with DVB teletext, subtitles, and other non-audio services.
/// Without parsing the PMT descriptor loop we cannot distinguish
/// AC-3-on-0x06 from teletext-on-0x06, and locking onto a teletext
/// PID as "audio" means the sampler never observes PES PTS.
fn is_unambiguous_audio_stream_type(st: u8) -> bool {
    matches!(st, 0x03 | 0x04 | 0x0F | 0x11 | 0x81 | 0x82 | 0x87 | 0x88)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: usize = ts_parse::TS_PACKET_SIZE;

    /// Build a minimal PAT packet with one program.
    fn build_pat(pmt_pid: u16, version: u8) -> [u8; TS] {
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        // PID 0, PUSI=1, payload only
        pkt[1] = 0x40;
        pkt[2] = 0x00;
        pkt[3] = 0x10; // AFC=01 (payload only), CC=0
        pkt[4] = 0x00; // pointer_field
        // PAT section: table_id(1) + flags+length(2) then body =
        // ts_id(2) + version(1) + section_num(1) + last_section(1) +
        // one program(4) + CRC(4) = 13 bytes
        pkt[5] = 0x00; // table_id = PAT
        pkt[6] = 0xB0;
        pkt[7] = 13;
        // ts_id
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        // version + current_next
        pkt[10] = (version << 1) | 0x01;
        // section_number / last_section_number
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        // program_number = 1
        pkt[13] = 0x00;
        pkt[14] = 0x01;
        // PMT PID (mask 0x1F on high byte)
        pkt[15] = 0xE0 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[16] = (pmt_pid & 0xFF) as u8;
        pkt
    }

    /// Build a minimal PMT packet with one video + one audio ES.
    fn build_pmt(
        pmt_pid: u16,
        version: u8,
        video_pid: u16,
        video_st: u8,
        audio_pid: u16,
        audio_st: u8,
    ) -> [u8; TS] {
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        // PID = pmt_pid, PUSI=1
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00; // pointer_field
        // PMT section: table_id(1) + flags+length(2) then body =
        // program_number(2) + version(1) + section_num(1) + last_section(1)
        // + pcr_pid(2) + program_info_length(2) + 2×5 ES entries + CRC(4)
        // = 23 bytes
        pkt[5] = 0x02; // table_id = PMT
        pkt[6] = 0xB0;
        pkt[7] = 23;
        // program_number = 1
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        // version + current_next
        pkt[10] = (version << 1) | 0x01;
        // section_number / last_section_number
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        // PCR_PID (same as video)
        pkt[13] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F);
        pkt[14] = (video_pid & 0xFF) as u8;
        // program_info_length = 0
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        // ES entry 1: video
        pkt[17] = video_st;
        pkt[18] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F);
        pkt[19] = (video_pid & 0xFF) as u8;
        pkt[20] = 0xF0;
        pkt[21] = 0x00; // ES_info_length = 0
        // ES entry 2: audio
        pkt[22] = audio_st;
        pkt[23] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[24] = (audio_pid & 0xFF) as u8;
        pkt[25] = 0xF0;
        pkt[26] = 0x00; // ES_info_length = 0
        pkt
    }

    /// Build a TS packet with a PES header carrying a specific PTS.
    fn build_pes_with_pts(pid: u16, pts_90k: u64) -> [u8; TS] {
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        // PID, PUSI=1
        pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10; // AFC=01 (payload only)
        // PES start code 0x000001
        pkt[4] = 0x00;
        pkt[5] = 0x00;
        pkt[6] = 0x01;
        pkt[7] = 0xE0; // stream_id (video)
        // PES packet length (0 = unbounded)
        pkt[8] = 0x00;
        pkt[9] = 0x00;
        // PES header flags: marker bits + PTS_DTS_flags = 0b10 (PTS only)
        pkt[10] = 0x80;
        pkt[11] = 0x80; // PTS_DTS_flags = 10
        pkt[12] = 0x05; // PES_header_data_length
        // 5-byte PTS field
        let pts = pts_90k & ((1u64 << 33) - 1);
        pkt[13] = 0x21 | (((pts >> 30) & 0x07) as u8) << 1;
        pkt[14] = ((pts >> 22) & 0xFF) as u8;
        pkt[15] = (((pts >> 15) & 0x7F) as u8) << 1 | 0x01;
        pkt[16] = ((pts >> 7) & 0xFF) as u8;
        pkt[17] = ((pts & 0x7F) as u8) << 1 | 0x01;
        pkt
    }

    #[test]
    fn empty_state_returns_none() {
        let s = AvSyncDriftSampler::new();
        assert!(s.snapshot().is_none());
    }

    #[test]
    fn no_snapshot_without_pmt_discovery() {
        let s = AvSyncDriftSampler::new();
        // Feed a random PES packet without prior PAT/PMT — should be ignored
        let pkt = build_pes_with_pts(0x100, 90_000);
        s.observe_packet(&pkt);
        assert!(s.snapshot().is_none());
    }

    #[test]
    fn zero_drift_when_pts_aligned() {
        let s = AvSyncDriftSampler::new();
        let video_pid = 0x100;
        let audio_pid = 0x101;
        let pmt_pid = 0x1000;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt(pmt_pid, 0, video_pid, 0x1B, audio_pid, 0x0F));
        // Same PTS for video and audio
        s.observe_packet(&build_pes_with_pts(video_pid, 90_000));
        s.observe_packet(&build_pes_with_pts(audio_pid, 90_000));

        let snap = s.snapshot().expect("should have a sample");
        assert_eq!(snap.samples, 1);
        assert_eq!(snap.avg_ms, 0);
        assert_eq!(snap.p50_abs_ms, 0);
        assert_eq!(snap.video_pid, video_pid);
        assert_eq!(snap.audio_pid, audio_pid);
    }

    #[test]
    fn positive_drift_when_video_late() {
        let s = AvSyncDriftSampler::new();
        let video_pid = 0x100;
        let audio_pid = 0x101;
        let pmt_pid = 0x1000;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt(pmt_pid, 0, video_pid, 0x1B, audio_pid, 0x0F));
        // Video PTS 30ms ahead of audio → +30 ms drift
        s.observe_packet(&build_pes_with_pts(video_pid, 90_000 + 2700)); // +30ms in 90kHz
        s.observe_packet(&build_pes_with_pts(audio_pid, 90_000));

        let snap = s.snapshot().unwrap();
        assert_eq!(snap.avg_ms, 30);
        assert_eq!(snap.p50_abs_ms, 30);
    }

    #[test]
    fn negative_drift_when_video_early() {
        let s = AvSyncDriftSampler::new();
        let video_pid = 0x100;
        let audio_pid = 0x101;
        let pmt_pid = 0x1000;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt(pmt_pid, 0, video_pid, 0x1B, audio_pid, 0x0F));
        // Audio PTS 20ms ahead of video → -20 ms drift
        s.observe_packet(&build_pes_with_pts(video_pid, 90_000));
        s.observe_packet(&build_pes_with_pts(audio_pid, 90_000 + 1800)); // +20ms

        let snap = s.snapshot().unwrap();
        assert_eq!(snap.avg_ms, -20);
        assert_eq!(snap.p50_abs_ms, 20);
    }

    #[test]
    fn discontinuity_guard_skips_large_drift() {
        let s = AvSyncDriftSampler::new();
        let video_pid = 0x100;
        let audio_pid = 0x101;
        let pmt_pid = 0x1000;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt(pmt_pid, 0, video_pid, 0x1B, audio_pid, 0x0F));
        // 2500ms offset — exceeds AV_SYNC_MAX_DRIFT_MS (2000ms) guard
        s.observe_packet(&build_pes_with_pts(video_pid, 90_000 + 225_000));
        s.observe_packet(&build_pes_with_pts(audio_pid, 90_000));

        assert!(s.snapshot().is_none(), "large drift should have been discarded");
    }

    /// Build a PMT with three ES entries: teletext (0x06) + video + audio.
    /// Reproduces the DVB layout where 0x06 teletext appears before the
    /// real audio PID.
    fn build_pmt_with_teletext(
        pmt_pid: u16,
        version: u8,
        ttx_pid: u16,
        video_pid: u16,
        video_st: u8,
        audio_pid: u16,
        audio_st: u8,
    ) -> [u8; TS] {
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x02;
        pkt[6] = 0xB0;
        // section_length: 5 (header) + 4 (preamble) + 3×5 (ES) + 4 (CRC) = 28
        pkt[7] = 28;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = (version << 1) | 0x01;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F);
        pkt[14] = (video_pid & 0xFF) as u8;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        // ES 1: teletext (stream_type 0x06)
        pkt[17] = 0x06;
        pkt[18] = 0xE0 | ((ttx_pid >> 8) as u8 & 0x1F);
        pkt[19] = (ttx_pid & 0xFF) as u8;
        pkt[20] = 0xF0;
        pkt[21] = 0x00;
        // ES 2: video
        pkt[22] = video_st;
        pkt[23] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F);
        pkt[24] = (video_pid & 0xFF) as u8;
        pkt[25] = 0xF0;
        pkt[26] = 0x00;
        // ES 3: audio
        pkt[27] = audio_st;
        pkt[28] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[29] = (audio_pid & 0xFF) as u8;
        pkt[30] = 0xF0;
        pkt[31] = 0x00;
        pkt
    }

    #[test]
    fn teletext_0x06_does_not_shadow_real_audio() {
        let s = AvSyncDriftSampler::new();
        let ttx_pid = 0x2B;
        let video_pid = 0x66;
        let audio_pid = 0x67;
        let pmt_pid = 0x400;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt_with_teletext(
            pmt_pid, 0, ttx_pid, video_pid, 0x1B, audio_pid, 0x03,
        ));
        s.observe_packet(&build_pes_with_pts(video_pid, 90_000 + 2700));
        s.observe_packet(&build_pes_with_pts(audio_pid, 90_000));

        let snap = s.snapshot().unwrap();
        assert_eq!(snap.video_pid, video_pid);
        assert_eq!(snap.audio_pid, audio_pid);
        assert_eq!(snap.avg_ms, 30);
    }

    #[test]
    fn fallback_to_0x06_when_no_unambiguous_audio() {
        let s = AvSyncDriftSampler::new();
        let video_pid = 0x100;
        let audio_pid = 0x101;
        let pmt_pid = 0x1000;

        s.observe_packet(&build_pat(pmt_pid, 0));
        // 0x06 is the only "audio" candidate (DVB AC-3 without descriptor)
        s.observe_packet(&build_pmt(pmt_pid, 0, video_pid, 0x1B, audio_pid, 0x06));
        s.observe_packet(&build_pes_with_pts(video_pid, 90_000 + 900));
        s.observe_packet(&build_pes_with_pts(audio_pid, 90_000));

        let snap = s.snapshot().unwrap();
        assert_eq!(snap.audio_pid, audio_pid);
        assert_eq!(snap.avg_ms, 10);
    }

    /// PMT with explicit ES-info descriptor loops: video (0x1B) +
    /// teletext-marked 0x06 (descriptor 0x56) + AC-3-marked 0x06
    /// (descriptor 0x6A). The Network-TEN shape.
    fn build_pmt_dvb_ac3_with_ttxt(
        pmt_pid: u16,
        version: u8,
        ttx_pid: u16,
        video_pid: u16,
        ac3_pid: u16,
    ) -> [u8; TS] {
        let ttx_descs: [u8; 7] = [0x56, 0x05, b'e', b'n', b'g', 0x10, 0x01];
        let ac3_descs: [u8; 3] = [0x6A, 0x01, 0x44];
        let mut pkt = [0xFFu8; TS];
        pkt[0] = 0x47;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x02;
        pkt[6] = 0xB0;
        // section_length: 5 + 4 + (5+0) + (5+7) + (5+3) + 4 = 38
        pkt[7] = 38;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = (version << 1) | 0x01;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F);
        pkt[14] = (video_pid & 0xFF) as u8;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        // ES 1: video, no descriptors
        pkt[17] = 0x1B;
        pkt[18] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F);
        pkt[19] = (video_pid & 0xFF) as u8;
        pkt[20] = 0xF0;
        pkt[21] = 0x00;
        // ES 2: teletext-marked 0x06 — listed BEFORE the audio
        pkt[22] = 0x06;
        pkt[23] = 0xE0 | ((ttx_pid >> 8) as u8 & 0x1F);
        pkt[24] = (ttx_pid & 0xFF) as u8;
        pkt[25] = 0xF0;
        pkt[26] = ttx_descs.len() as u8;
        pkt[27..27 + ttx_descs.len()].copy_from_slice(&ttx_descs);
        // ES 3: AC-3-marked 0x06 — the real audio
        let p = 27 + ttx_descs.len();
        pkt[p] = 0x06;
        pkt[p + 1] = 0xE0 | ((ac3_pid >> 8) as u8 & 0x1F);
        pkt[p + 2] = (ac3_pid & 0xFF) as u8;
        pkt[p + 3] = 0xF0;
        pkt[p + 4] = ac3_descs.len() as u8;
        pkt[p + 5..p + 5 + ac3_descs.len()].copy_from_slice(&ac3_descs);
        pkt
    }

    #[test]
    fn descriptor_confirmed_ac3_0x06_wins_over_preceding_teletext() {
        let s = AvSyncDriftSampler::new();
        let ttx_pid = 0x23F;
        let video_pid = 0x1FF;
        let ac3_pid = 0x289;
        let pmt_pid = 0x101;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt_dvb_ac3_with_ttxt(
            pmt_pid, 0, ttx_pid, video_pid, ac3_pid,
        ));
        s.observe_packet(&build_pes_with_pts(video_pid, 90_000 + 1800));
        s.observe_packet(&build_pes_with_pts(ac3_pid, 90_000));

        let snap = s.snapshot().unwrap();
        assert_eq!(
            snap.audio_pid, ac3_pid,
            "descriptor-confirmed AC-3-in-0x06 must win over the teletext that precedes it"
        );
        assert_eq!(snap.avg_ms, 20);
    }

    #[test]
    fn reservoir_caps_at_configured_size() {
        let s = AvSyncDriftSampler::new();
        let video_pid = 0x100;
        let audio_pid = 0x101;
        let pmt_pid = 0x1000;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt(pmt_pid, 0, video_pid, 0x1B, audio_pid, 0x0F));

        for i in 0..(AV_SYNC_RESERVOIR_SIZE as u64 + 500) {
            let base = i * 3600; // 40ms in 90kHz
            s.observe_packet(&build_pes_with_pts(video_pid, base + 90));
            s.observe_packet(&build_pes_with_pts(audio_pid, base));
        }

        let snap = s.snapshot().unwrap();
        assert!(
            snap.samples as usize <= AV_SYNC_RESERVOIR_SIZE,
            "reservoir grew past cap: {}",
            snap.samples
        );
        assert!(
            snap.cumulative_samples > AV_SYNC_RESERVOIR_SIZE as u64,
            "cumulative should keep counting: {}",
            snap.cumulative_samples
        );
    }

    #[test]
    fn percentiles_are_ordered() {
        let s = AvSyncDriftSampler::new();
        let video_pid = 0x100;
        let audio_pid = 0x101;
        let pmt_pid = 0x1000;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt(pmt_pid, 0, video_pid, 0x1B, audio_pid, 0x0F));

        // Feed samples with varying drift
        for i in 0..200u64 {
            let drift_90k = (i % 40) * 90; // 0..39 ms
            s.observe_packet(&build_pes_with_pts(video_pid, 90_000 + i * 3600 + drift_90k));
            s.observe_packet(&build_pes_with_pts(audio_pid, 90_000 + i * 3600));
        }

        let snap = s.snapshot().unwrap();
        assert!(snap.p50_abs_ms <= snap.p95_abs_ms);
        assert!(snap.p95_abs_ms <= snap.p99_abs_ms);
        assert!(snap.p99_abs_ms <= snap.max_abs_ms);
    }

    #[test]
    fn pmt_version_update_resets_pids() {
        let s = AvSyncDriftSampler::new();
        let pmt_pid = 0x1000;

        s.observe_packet(&build_pat(pmt_pid, 0));
        s.observe_packet(&build_pmt(pmt_pid, 0, 0x100, 0x1B, 0x101, 0x0F));
        s.observe_packet(&build_pes_with_pts(0x100, 90_000 + 900));
        s.observe_packet(&build_pes_with_pts(0x101, 90_000));

        let snap1 = s.snapshot().unwrap();
        assert_eq!(snap1.video_pid, 0x100);
        assert_eq!(snap1.audio_pid, 0x101);

        // PMT version bump with different PIDs
        s.observe_packet(&build_pmt(pmt_pid, 1, 0x200, 0x1B, 0x201, 0x0F));
        s.observe_packet(&build_pes_with_pts(0x200, 90_000 + 450));
        s.observe_packet(&build_pes_with_pts(0x201, 90_000));

        let snap2 = s.snapshot().unwrap();
        assert_eq!(snap2.video_pid, 0x200);
        assert_eq!(snap2.audio_pid, 0x201);
        assert_eq!(snap2.samples, 2);
    }

    #[test]
    fn pts_delta_ms_basic() {
        // 30ms in 90kHz = 2700 ticks
        assert_eq!(pts_delta_ms(92700, 90000), 30);
        assert_eq!(pts_delta_ms(90000, 92700), -30);
        assert_eq!(pts_delta_ms(90000, 90000), 0);
    }

    #[test]
    fn pts_delta_ms_wraps_correctly() {
        // Near the 33-bit boundary
        let near_max = ts_parse::PTS_MODULUS_90KHZ - 900; // 10ms before wrap
        let after_wrap = 900; // 10ms after wrap
        // Forward wrap: after_wrap - near_max should be ~20ms
        assert_eq!(pts_delta_ms(after_wrap, near_max), 20);
        // Backward wrap
        assert_eq!(pts_delta_ms(near_max, after_wrap), -20);
    }
}
