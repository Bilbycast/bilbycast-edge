// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-program PMT walk + SPS dimension extraction for the media-library
//! file probe.
//!
//! The async wrapper in [`super::MediaLibrary::scan_programs`] reads a 512
//! KiB head window plus a 64 KiB tail window and calls into here to fill
//! the per-program codec / resolution / audio-track tables, plus a
//! file-wide bitrate estimate from the PCR delta between head and tail.
//!
//! Pure synchronous functions so the test harness can drive them from
//! `cargo test` against handcrafted TS buffers.

use serde::{Deserialize, Serialize};

use crate::engine::content_analysis::bitreader::{BitReader, unescape_rbsp};
use crate::engine::ts_parse::{
    TS_PACKET_SIZE, TS_SYNC_BYTE, extract_pcr, ts_has_adaptation, ts_pid, ts_pusi,
};

/// One video elementary stream inside a program. Surfaced to the manager
/// UI so the operator sees codec + resolution at a glance when picking a
/// file from the media library.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaVideoStreamInfo {
    pub pid: u16,
    pub codec: String,
    pub stream_type: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// One audio elementary stream inside a program. Language pulled from the
/// PMT ISO 639 descriptor (tag 0x0A) when present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaAudioStreamInfo {
    pub pid: u16,
    pub codec: String,
    pub stream_type: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// Result of [`scan_pmt_in_buf`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PmtScanResult {
    pub pcr_pid: Option<u16>,
    pub video_streams: Vec<MediaVideoStreamInfo>,
    pub audio_streams: Vec<MediaAudioStreamInfo>,
}

/// Walk a TS buffer at the given stride and parse the first PMT seen for
/// `target_pmt_pid`. Returns the elementary stream tables plus the PCR
/// PID.
pub fn scan_pmt_in_buf(
    buf: &[u8],
    stride: usize,
    start: usize,
    target_pmt_pid: u16,
) -> PmtScanResult {
    let mut offset = start;
    while offset + TS_PACKET_SIZE <= buf.len() {
        let pkt = &buf[offset..offset + TS_PACKET_SIZE];
        offset += stride;
        if pkt[0] != TS_SYNC_BYTE {
            continue;
        }
        if ts_pid(pkt) != target_pmt_pid {
            continue;
        }
        if !ts_pusi(pkt) {
            continue;
        }
        if let Some(res) = parse_pmt_packet(pkt) {
            return res;
        }
    }
    PmtScanResult::default()
}

/// Find the first PCR for the given PID in the buffer, in 27 MHz ticks.
pub fn find_first_pcr(buf: &[u8], stride: usize, start: usize, pcr_pid: u16) -> Option<u64> {
    let mut offset = start;
    while offset + TS_PACKET_SIZE <= buf.len() {
        let pkt = &buf[offset..offset + TS_PACKET_SIZE];
        offset += stride;
        if pkt[0] != TS_SYNC_BYTE {
            continue;
        }
        if ts_pid(pkt) != pcr_pid {
            continue;
        }
        if let Some(pcr) = extract_pcr(pkt) {
            return Some(pcr);
        }
    }
    None
}

/// Find the last PCR for the given PID in the buffer.
pub fn find_last_pcr(buf: &[u8], stride: usize, start: usize, pcr_pid: u16) -> Option<u64> {
    let mut offset = start;
    let mut last = None;
    while offset + TS_PACKET_SIZE <= buf.len() {
        let pkt = &buf[offset..offset + TS_PACKET_SIZE];
        offset += stride;
        if pkt[0] != TS_SYNC_BYTE {
            continue;
        }
        if ts_pid(pkt) != pcr_pid {
            continue;
        }
        if let Some(pcr) = extract_pcr(pkt) {
            last = Some(pcr);
        }
    }
    last
}

/// Walk a TS buffer at the given stride, accumulate the elementary stream
/// payload bytes for `video_pid` (skipping PES headers on PUSI packets),
/// and look for an SPS NAL to extract the coded picture dimensions.
///
/// Returns `(width, height)` if an SPS is found and parses cleanly.
/// `stream_type` selects the codec parser (0x1B = AVC, 0x24 = HEVC,
/// 0x01 / 0x02 = MPEG-1/2 video sequence header).
pub fn extract_video_dims(
    buf: &[u8],
    stride: usize,
    start: usize,
    video_pid: u16,
    stream_type: u8,
) -> Option<(u32, u32)> {
    // Cap the accumulated ES window — for typical broadcast bitrates the
    // first SPS lands within a handful of KB, and the SPS parsers will
    // bail on malformed input rather than scan unboundedly.
    const ES_BUDGET: usize = 256 * 1024;
    let mut es = Vec::with_capacity(8 * 1024);
    let mut offset = start;
    while offset + TS_PACKET_SIZE <= buf.len() && es.len() < ES_BUDGET {
        let pkt = &buf[offset..offset + TS_PACKET_SIZE];
        offset += stride;
        if pkt[0] != TS_SYNC_BYTE {
            continue;
        }
        if ts_pid(pkt) != video_pid {
            continue;
        }
        let mut payload_offset = 4;
        if ts_has_adaptation(pkt) {
            let af_len = pkt[4] as usize;
            payload_offset = 5 + af_len;
        }
        if payload_offset >= TS_PACKET_SIZE {
            continue;
        }
        let mut payload = &pkt[payload_offset..];
        if ts_pusi(pkt) {
            // PES header: 00 00 01 stream_id (4) + length (2) + flags (2) +
            // pes_header_data_length (1) + header_data (N).
            if payload.len() >= 9 && &payload[0..3] == b"\x00\x00\x01" {
                let pes_header_data_length = payload[8] as usize;
                let pes_total_header = 9 + pes_header_data_length;
                if payload.len() > pes_total_header {
                    payload = &payload[pes_total_header..];
                } else {
                    continue;
                }
            }
        }
        es.extend_from_slice(payload);
    }
    match stream_type {
        0x1B => find_avc_sps_dims(&es),
        0x24 => find_hevc_sps_dims(&es),
        0x01 | 0x02 => find_mpeg2_seq_dims(&es),
        _ => None,
    }
}

// ── PMT parser ───────────────────────────────────────────────────────────

fn parse_pmt_packet(pkt: &[u8]) -> Option<PmtScanResult> {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[offset] as usize;
    offset = offset.checked_add(1 + pointer)?;
    if offset + 12 > TS_PACKET_SIZE {
        return None;
    }
    if pkt[offset] != 0x02 {
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

    let mut res = PmtScanResult {
        pcr_pid: if pcr_pid == 0x1FFF { None } else { Some(pcr_pid) },
        ..PmtScanResult::default()
    };

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let stream_type = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_length =
            (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        let desc_start = pos + 5;
        let desc_end = (desc_start + es_info_length).min(data_end);
        let desc = &pkt[desc_start..desc_end];

        match stream_type {
            0x1B => res.video_streams.push(MediaVideoStreamInfo {
                pid: es_pid,
                codec: "H.264/AVC".into(),
                stream_type,
                width: None,
                height: None,
            }),
            0x24 => res.video_streams.push(MediaVideoStreamInfo {
                pid: es_pid,
                codec: "H.265/HEVC".into(),
                stream_type,
                width: None,
                height: None,
            }),
            0x01 => res.video_streams.push(MediaVideoStreamInfo {
                pid: es_pid,
                codec: "MPEG-1 Video".into(),
                stream_type,
                width: None,
                height: None,
            }),
            0x02 => res.video_streams.push(MediaVideoStreamInfo {
                pid: es_pid,
                codec: "MPEG-2 Video".into(),
                stream_type,
                width: None,
                height: None,
            }),
            0x61 => res.video_streams.push(MediaVideoStreamInfo {
                pid: es_pid,
                codec: "JPEG XS".into(),
                stream_type,
                width: None,
                height: None,
            }),
            0x03 | 0x04 => res.audio_streams.push(MediaAudioStreamInfo {
                pid: es_pid,
                codec: "MPEG Audio".into(),
                stream_type,
                language: parse_language_descriptor(desc),
            }),
            0x0F => res.audio_streams.push(MediaAudioStreamInfo {
                pid: es_pid,
                codec: "AAC".into(),
                stream_type,
                language: parse_language_descriptor(desc),
            }),
            0x11 => res.audio_streams.push(MediaAudioStreamInfo {
                pid: es_pid,
                codec: "AAC-LATM".into(),
                stream_type,
                language: parse_language_descriptor(desc),
            }),
            0x81 => res.audio_streams.push(MediaAudioStreamInfo {
                pid: es_pid,
                codec: "AC-3".into(),
                stream_type,
                language: parse_language_descriptor(desc),
            }),
            0x87 => res.audio_streams.push(MediaAudioStreamInfo {
                pid: es_pid,
                codec: "E-AC-3".into(),
                stream_type,
                language: parse_language_descriptor(desc),
            }),
            0xAC => res.audio_streams.push(MediaAudioStreamInfo {
                pid: es_pid,
                codec: "AC-4".into(),
                stream_type,
                language: parse_language_descriptor(desc),
            }),
            0x06 => {
                if let Some(codec) = detect_private_stream_codec(desc) {
                    res.audio_streams.push(MediaAudioStreamInfo {
                        pid: es_pid,
                        codec,
                        stream_type,
                        language: parse_language_descriptor(desc),
                    });
                }
            }
            _ => {}
        }

        pos += 5 + es_info_length;
    }

    Some(res)
}

fn parse_language_descriptor(descriptors: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos + 2 <= descriptors.len() {
        let tag = descriptors[pos];
        let len = descriptors[pos + 1] as usize;
        if pos + 2 + len > descriptors.len() {
            break;
        }
        if tag == 0x0A && len >= 3 {
            let lang = &descriptors[pos + 2..pos + 5];
            if lang.iter().all(|&b| b.is_ascii_alphabetic()) {
                return Some(String::from_utf8_lossy(lang).to_lowercase());
            }
        }
        pos += 2 + len;
    }
    None
}

/// Detect the codec carried by a `stream_type=0x06` (private data) ES via
/// its registration / format descriptors. Mirrors the small subset
/// `media_analysis` covers — AC-3 (tag 0x6A), E-AC-3 (tag 0x7A), AC-4
/// (registration "AC-4").
fn detect_private_stream_codec(descriptors: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos + 2 <= descriptors.len() {
        let tag = descriptors[pos];
        let len = descriptors[pos + 1] as usize;
        if pos + 2 + len > descriptors.len() {
            break;
        }
        match tag {
            0x6A => return Some("AC-3".into()),
            0x7A => return Some("E-AC-3".into()),
            0x05 if len >= 4 => {
                let reg = &descriptors[pos + 2..pos + 6];
                if reg == b"AC-3" {
                    return Some("AC-3".into());
                }
                if reg == b"EAC3" {
                    return Some("E-AC-3".into());
                }
                if reg == b"AC-4" {
                    return Some("AC-4".into());
                }
            }
            _ => {}
        }
        pos += 2 + len;
    }
    None
}

// ── SPS / sequence-header dimension extraction ───────────────────────────

fn find_avc_sps_dims(es: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0;
    while i + 4 < es.len() {
        let sc3 = es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1;
        let sc4 = i + 4 < es.len()
            && es[i] == 0
            && es[i + 1] == 0
            && es[i + 2] == 0
            && es[i + 3] == 1;
        if sc4 {
            i += 4;
        } else if sc3 {
            i += 3;
        } else {
            i += 1;
            continue;
        }
        if i >= es.len() {
            break;
        }
        let nut = es[i] & 0x1F;
        let nal_end = next_start_code(es, i + 1);
        if nut == 7 {
            // SPS — RBSP starts at i+1 (skip NAL header byte).
            let rbsp = unescape_rbsp(&es[i + 1..nal_end]);
            if let Some(dims) = parse_avc_sps_dims(&rbsp) {
                return Some(dims);
            }
        }
        i = nal_end;
    }
    None
}

fn find_hevc_sps_dims(es: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0;
    while i + 5 < es.len() {
        let sc3 = es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1;
        let sc4 = i + 4 < es.len()
            && es[i] == 0
            && es[i + 1] == 0
            && es[i + 2] == 0
            && es[i + 3] == 1;
        if sc4 {
            i += 4;
        } else if sc3 {
            i += 3;
        } else {
            i += 1;
            continue;
        }
        if i + 1 >= es.len() {
            break;
        }
        let nut = (es[i] >> 1) & 0x3F;
        let nal_end = next_start_code(es, i + 2);
        if nut == 33 {
            // SPS — strip the 2-byte NAL header.
            let rbsp = unescape_rbsp(&es[i + 2..nal_end]);
            if let Some(dims) = parse_hevc_sps_dims(&rbsp) {
                return Some(dims);
            }
        }
        i = nal_end;
    }
    None
}

fn find_mpeg2_seq_dims(es: &[u8]) -> Option<(u32, u32)> {
    // sequence_header_code = 0x000001B3, then 12 bits horizontal_size +
    // 12 bits vertical_size.
    let mut i = 0;
    while i + 7 < es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 && es[i + 3] == 0xB3 {
            let w = ((es[i + 4] as u32) << 4) | ((es[i + 5] as u32) >> 4);
            let h = (((es[i + 5] as u32) & 0x0F) << 8) | (es[i + 6] as u32);
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
        i += 1;
    }
    None
}

fn next_start_code(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 2 < bytes.len() {
        if bytes[i] == 0
            && bytes[i + 1] == 0
            && (bytes[i + 2] == 1
                || (i + 3 < bytes.len() && bytes[i + 2] == 0 && bytes[i + 3] == 1))
        {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

fn parse_avc_sps_dims(rbsp: &[u8]) -> Option<(u32, u32)> {
    if rbsp.len() < 4 {
        return None;
    }
    let profile_idc = rbsp[0];
    // Skip constraint_set + reserved (1 byte) and level_idc (1 byte).
    let mut br = BitReader::new(&rbsp[3..]);
    br.read_ue()?; // seq_parameter_set_id

    let extended_profile = matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    );
    if extended_profile {
        let chroma = br.read_ue().unwrap_or(1);
        if chroma == 3 {
            br.read_bit()?;
        }
        br.read_ue()?; // bit_depth_luma_minus8
        br.read_ue()?; // bit_depth_chroma_minus8
        br.read_bit()?; // qpprime_y_zero_transform_bypass_flag
        let seq_scaling = br.read_bit()?;
        if seq_scaling == 1 {
            return None;
        }
    }
    br.read_ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = br.read_ue()?;
    if pic_order_cnt_type == 0 {
        br.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        br.read_bit()?;
        br.read_se()?;
        br.read_se()?;
        let n = br.read_ue()?;
        for _ in 0..n.min(256) {
            br.read_se()?;
        }
    }
    br.read_ue()?; // max_num_ref_frames
    br.read_bit()?; // gaps_in_frame_num_value_allowed_flag

    let pic_w_mbs_m1 = br.read_ue()?;
    let pic_h_units_m1 = br.read_ue()?;
    let frame_mbs_only_flag = br.read_bit()?;
    if frame_mbs_only_flag == 0 {
        br.read_bit()?;
    }
    br.read_bit()?; // direct_8x8_inference_flag
    let crop_flag = br.read_bit()?;
    let (cl, cr, ct, cb) = if crop_flag == 1 {
        (
            br.read_ue()?,
            br.read_ue()?,
            br.read_ue()?,
            br.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };
    let width = (pic_w_mbs_m1 + 1) * 16 - (cl + cr) * 2;
    let height = (2 - frame_mbs_only_flag as u32) * (pic_h_units_m1 + 1) * 16
        - (ct + cb) * 2;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

fn parse_hevc_sps_dims(rbsp: &[u8]) -> Option<(u32, u32)> {
    let mut br = BitReader::new(rbsp);
    br.read_bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = br.read_bits(3)?;
    br.read_bit()?; // sps_temporal_id_nesting_flag
    if !skip_profile_tier_level(&mut br, max_sub_layers_minus1 as u8) {
        return None;
    }
    br.read_ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = br.read_ue()?;
    if chroma_format_idc == 3 {
        br.read_bit()?;
    }
    let pic_width = br.read_ue()?;
    let pic_height = br.read_ue()?;
    let conformance_window_flag = br.read_bit()?;
    let (cl, cr, ct, cb) = if conformance_window_flag == 1 {
        (
            br.read_ue()?,
            br.read_ue()?,
            br.read_ue()?,
            br.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };
    let width = pic_width.saturating_sub((cl + cr) * 2);
    let height = pic_height.saturating_sub((ct + cb) * 2);
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// Mirrors `engine::content_analysis::signalling::skip_profile_tier_level`
/// — see that function's commentary for why the per-sub-layer chunk is
/// 112 bits (32+32+16+32) rather than the spec-correct 88. The over-read
/// matches what real broadcast HEVC encoders emit and is necessary for
/// `pic_width` / `pic_height` to land at the right bit boundary.
fn skip_profile_tier_level(br: &mut BitReader, max_sub_layers_minus1: u8) -> bool {
    if br.read_bits(8).is_none() {
        return false;
    }
    if br.read_bits(32).is_none() {
        return false;
    }
    if br.read_bits(16).is_none() || br.read_bits(32).is_none() {
        return false;
    }
    if br.read_bits(8).is_none() {
        return false;
    }
    let mut sub_layer_profile_present = [0u8; 8];
    let mut sub_layer_level_present = [0u8; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_layer_profile_present[i] = br.read_bit().unwrap_or(0);
        sub_layer_level_present[i] = br.read_bit().unwrap_or(0);
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            let _ = br.read_bits(2);
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_layer_profile_present[i] == 1 {
            if br.read_bits(32).is_none()
                || br.read_bits(32).is_none()
                || br.read_bits(16).is_none()
                || br.read_bits(32).is_none()
            {
                return false;
            }
        }
        if sub_layer_level_present[i] == 1 && br.read_bits(8).is_none() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal PMT packet with the given (stream_type, es_pid) entries.
    /// PCR_PID is set to the first ES PID. CRC is left as buffer fill.
    fn build_pmt_packet(pmt_pid: u16, program_number: u16, streams: &[(u8, u16)]) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x40 | ((pmt_pid >> 8) & 0x1F) as u8;
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00; // pointer_field
        pkt[5] = 0x02; // table_id = PMT
        let n = streams.len();
        let section_length = 9 + 5 * n + 4;
        pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = (program_number >> 8) as u8;
        pkt[9] = (program_number & 0xFF) as u8;
        pkt[10] = 0xC1; // version 0, current
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        let pcr_pid = streams.first().map(|(_, p)| *p).unwrap_or(0x1FFF);
        pkt[13] = 0xE0 | ((pcr_pid >> 8) & 0x1F) as u8;
        pkt[14] = (pcr_pid & 0xFF) as u8;
        pkt[15] = 0xF0;
        pkt[16] = 0x00; // program_info_length = 0
        let mut pos = 17;
        for (st, pid) in streams {
            pkt[pos] = *st;
            pkt[pos + 1] = 0xE0 | ((pid >> 8) & 0x1F) as u8;
            pkt[pos + 2] = (*pid & 0xFF) as u8;
            pkt[pos + 3] = 0xF0;
            pkt[pos + 4] = 0x00; // es_info_length = 0
            pos += 5;
        }
        pkt
    }

    fn null_packet() -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x1F;
        pkt[2] = 0xFF;
        pkt[3] = 0x10;
        pkt
    }

    #[test]
    fn pmt_walk_captures_video_and_audio_streams() {
        let pmt_pid = 0x1000;
        let pmt = build_pmt_packet(pmt_pid, 1, &[(0x1B, 0x100), (0x0F, 0x101), (0x81, 0x102)]);
        let mut buf = Vec::new();
        buf.extend_from_slice(&pmt);
        buf.extend_from_slice(&null_packet());
        let res = scan_pmt_in_buf(&buf, 188, 0, pmt_pid);
        assert_eq!(res.pcr_pid, Some(0x100));
        assert_eq!(res.video_streams.len(), 1);
        assert_eq!(res.video_streams[0].pid, 0x100);
        assert_eq!(res.video_streams[0].codec, "H.264/AVC");
        assert_eq!(res.audio_streams.len(), 2);
        assert_eq!(res.audio_streams[0].codec, "AAC");
        assert_eq!(res.audio_streams[1].codec, "AC-3");
    }

    #[test]
    fn pmt_walk_returns_empty_when_target_pid_absent() {
        let pmt = build_pmt_packet(0x1000, 1, &[(0x1B, 0x100)]);
        let res = scan_pmt_in_buf(&pmt, 188, 0, 0x1500);
        assert_eq!(res, PmtScanResult::default());
    }

    #[test]
    fn mpeg2_seq_dims_extract_1280x720() {
        // 0x000001B3 + 12 bits w=1280 (0x500) + 12 bits h=720 (0x2D0) =
        // 50 02 D0 ... → bytes 0x50, 0x02, 0xD0.
        let es = [
            0x00, 0x00, 0x01, 0xB3, 0x50, 0x02, 0xD0, 0x00, 0x00,
        ];
        assert_eq!(find_mpeg2_seq_dims(&es), Some((1280, 720)));
    }

    #[test]
    fn mpeg2_seq_dims_returns_none_without_marker() {
        let es = [0xCC; 64];
        assert_eq!(find_mpeg2_seq_dims(&es), None);
    }
}
