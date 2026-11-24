// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Container / codec signalling tracker.
//!
//! Walks the video PES looking for SPS + SEI NAL units and extracts:
//! - **Aspect ratio** from SPS VUI `aspect_ratio_idc` / extended SAR
//!   combined with the cropped picture dimensions
//! - **Colour primaries / transfer / matrix / range** from VUI
//!   `colour_description`
//! - **HDR family** derived from `transfer_characteristics`
//!   (`smpte2084` = HDR10, `arib-std-b67` = HLG, else `sdr`)
//! - **MaxFALL / MaxCLL** from H.264 SEI type 144 (content light level)
//! - **AFD** from MPEG-TS user-data PES (ATSC A/53 + SMPTE 2016-1, scanned
//!   on both H.264/H.265 SEI user-data and MPEG-2 user-data)
//!
//! H.265 SPS decode is simpler than H.264 for VUI fields (same layout) and
//! is handled by the same code path once we detect the codec family.

use crate::stats::models::SignallingStats;

use super::bitreader::{unescape_rbsp, BitReader};
use crate::engine::h264_sps::picture_size;
use crate::engine::hevc_sps;

pub struct SignallingTracker {
    codec: Codec,
    seen_any: bool,
    // Decoded signalling state
    aspect_ratio: Option<String>,
    colour_primaries: Option<&'static str>,
    transfer_characteristics: Option<&'static str>,
    matrix_coefficients: Option<&'static str>,
    video_range: Option<&'static str>,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
    afd: Option<u8>,
    // Capture buffer for scanning
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

const PEEK_CAP: usize = 4096;

impl SignallingTracker {
    pub fn new() -> Self {
        Self {
            codec: Codec::Unknown,
            seen_any: false,
            aspect_ratio: None,
            colour_primaries: None,
            transfer_characteristics: None,
            matrix_coefficients: None,
            video_range: None,
            max_cll: None,
            max_fall: None,
            afd: None,
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
        self.seen_any = true;
    }

    /// Observe a TS payload on the video PID. On PUSI we restart a peek
    /// buffer; we scan after enough bytes have accumulated to cover the
    /// PES header + initial NAL units (SPS typically lives in an IDR
    /// GOP alongside PPS + the first slice).
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
        match self.codec {
            Codec::Avc => self.scan_avc(&bytes),
            Codec::Hevc => self.scan_hevc(&bytes),
            _ => {}
        }
        // MPEG-TS user-data PES can carry AFD for all codec families —
        // scan regardless. ATSC A/53 Part 4 defines the user_data syntax.
        self.scan_for_afd(&bytes);
    }

    fn scan_avc(&mut self, bytes: &[u8]) {
        // Walk NAL start codes; identify SPS (type 7) and SEI (type 6).
        let mut i = 0;
        while i + 4 < bytes.len() {
            // Start codes: 0x000001 (3B) or 0x00000001 (4B).
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
            let nal_header = bytes[i];
            let nut = nal_header & 0x1F;
            // find end: next start code or EOF
            let nal_end = find_next_start_code(bytes, i + 1);
            let nal_bytes = &bytes[i + 1..nal_end];
            match nut {
                7 => {
                    // SPS
                    let rbsp = unescape_rbsp(nal_bytes);
                    self.parse_avc_sps(&rbsp);
                }
                6 => {
                    // SEI — may contain MaxFALL/CLL (type 144)
                    let rbsp = unescape_rbsp(nal_bytes);
                    self.parse_avc_sei(&rbsp);
                }
                _ => {}
            }
            i = nal_end;
        }
    }

    fn scan_hevc(&mut self, bytes: &[u8]) {
        // HEVC NAL header is 2 bytes: (nal_unit_type << 1)|layer_id[5] + layer_id[0..5]|tid+1
        let mut i = 0;
        while i + 5 < bytes.len() {
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
            if i + 1 >= bytes.len() {
                break;
            }
            let hdr0 = bytes[i];
            let nut = (hdr0 >> 1) & 0x3F;
            let nal_end = find_next_start_code(bytes, i + 2);
            let nal_bytes = &bytes[i + 2..nal_end];
            match nut {
                33 => {
                    // SPS_NUT
                    let rbsp = unescape_rbsp(nal_bytes);
                    self.parse_hevc_sps(&rbsp);
                }
                39 | 40 => {
                    // PREFIX_SEI / SUFFIX_SEI
                    let rbsp = unescape_rbsp(nal_bytes);
                    self.parse_hevc_sei(&rbsp);
                }
                _ => {}
            }
            i = nal_end;
        }
    }

    fn parse_avc_sps(&mut self, rbsp: &[u8]) {
        if rbsp.len() < 4 {
            return;
        }
        let profile_idc = rbsp[0];
        // Skip constraint_set + reserved (1 byte) and level_idc (1 byte).
        let mut br = BitReader::new(&rbsp[3..]);
        if br.read_ue().is_none() {
            return; // seq_parameter_set_id
        }

        // Certain profiles add chroma / scaling-list fields. Skip them.
        let extended_profile = matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        );
        if extended_profile {
            let chroma = br.read_ue().unwrap_or(1);
            if chroma == 3 {
                let _ = br.read_bit(); // separate_colour_plane_flag
            }
            let _ = br.read_ue(); // bit_depth_luma_minus8
            let _ = br.read_ue(); // bit_depth_chroma_minus8
            let _ = br.read_bit(); // qpprime_y_zero_transform_bypass_flag
            // seq_scaling_matrix_present_flag → if set, parsing scaling
            // lists is complex; bail gracefully — we only want VUI.
            let seq_scaling = br.read_bit().unwrap_or(0);
            if seq_scaling == 1 {
                return;
            }
        }
        let _ = br.read_ue(); // log2_max_frame_num_minus4
        let pic_order_cnt_type = br.read_ue().unwrap_or(0);
        if pic_order_cnt_type == 0 {
            let _ = br.read_ue(); // log2_max_pic_order_cnt_lsb_minus4
        } else if pic_order_cnt_type == 1 {
            let _ = br.read_bit();
            let _ = br.read_se();
            let _ = br.read_se();
            let n = br.read_ue().unwrap_or(0);
            for _ in 0..n.min(256) {
                let _ = br.read_se();
            }
        }
        let _ = br.read_ue(); // max_num_ref_frames
        let _ = br.read_bit(); // gaps_in_frame_num_value_allowed_flag

        let pic_width_in_mbs_minus1 = br.read_ue().unwrap_or(0);
        let pic_height_in_map_units_minus1 = br.read_ue().unwrap_or(0);
        let frame_mbs_only_flag = br.read_bit().unwrap_or(1);
        if frame_mbs_only_flag == 0 {
            let _ = br.read_bit(); // mb_adaptive_frame_field_flag
        }
        let _ = br.read_bit(); // direct_8x8_inference_flag
        let frame_cropping_flag = br.read_bit().unwrap_or(0);
        let crop = if frame_cropping_flag == 1 {
            (
                br.read_ue().unwrap_or(0),
                br.read_ue().unwrap_or(0),
                br.read_ue().unwrap_or(0),
                br.read_ue().unwrap_or(0),
            )
        } else {
            (0, 0, 0, 0)
        };
        // A size or crop out of range has no aspect ratio, as 0x0 has none.
        let (width, height) = picture_size(
            pic_width_in_mbs_minus1,
            pic_height_in_map_units_minus1,
            frame_mbs_only_flag == 1,
            crop,
            (2, 2),
        )
        .unwrap_or((0, 0));
        let vui_present = br.read_bit().unwrap_or(0);
        if vui_present != 1 {
            // Still compute AR from coded dimensions if we can.
            self.aspect_ratio = format_aspect_ratio(width, height, 1, 1);
            return;
        }

        self.parse_vui(&mut br, width, height);
    }

    fn parse_hevc_sps(&mut self, rbsp: &[u8]) {
        // Read as far as the VUI's colours, where no scaling lists are spelt
        // out and the reference picture sets are in range; otherwise bail.
        let mut br = BitReader::new(rbsp);
        let _ = br.read_bits(4); // sps_video_parameter_set_id
        let max_sub_layers_minus1 = br.read_bits(3).unwrap_or(0);
        let _ = br.read_bit(); // sps_temporal_id_nesting_flag
        if !skip_profile_tier_level(&mut br, max_sub_layers_minus1) {
            return;
        }
        let _ = br.read_ue(); // sps_seq_parameter_set_id
        let chroma_format_idc = br.read_ue().unwrap_or(1);
        if chroma_format_idc == 3 {
            let _ = br.read_bit();
        }
        let pic_width = br.read_ue().unwrap_or(0);
        let pic_height = br.read_ue().unwrap_or(0);
        let conformance_window_flag = br.read_bit().unwrap_or(0);
        let conf_win = if conformance_window_flag == 1 {
            (
                br.read_ue().unwrap_or(0),
                br.read_ue().unwrap_or(0),
                br.read_ue().unwrap_or(0),
                br.read_ue().unwrap_or(0),
            )
        } else {
            (0, 0, 0, 0)
        };
        // A size or crop out of range has no aspect ratio, as 0x0 has none.
        let (width, height) =
            hevc_sps::picture_size(pic_width, pic_height, conf_win, (2, 2)).unwrap_or((0, 0));
        let _ = br.read_ue(); // bit_depth_luma_minus8
        let _ = br.read_ue(); // bit_depth_chroma_minus8
        let log2_max_pic_order_cnt_lsb_minus4 = br.read_ue().unwrap_or(0);
        let sps_sub_layer_ordering_info_present = br.read_bit().unwrap_or(0);
        let num = if sps_sub_layer_ordering_info_present == 1 {
            max_sub_layers_minus1 as i32 + 1
        } else {
            1
        };
        for _ in 0..num {
            let _ = br.read_ue(); // max_dec_pic_buffering_minus1
            let _ = br.read_ue(); // max_num_reorder_pics
            let _ = br.read_ue(); // max_latency_increase_plus1
        }
        let _ = br.read_ue(); // log2_min_luma_coding_block_size_minus3
        let _ = br.read_ue(); // log2_diff_max_min_luma_coding_block_size
        let _ = br.read_ue(); // log2_min_luma_transform_block_size_minus2
        let _ = br.read_ue(); // log2_diff_max_min_luma_transform_block_size
        let _ = br.read_ue(); // max_transform_hierarchy_depth_inter
        let _ = br.read_ue(); // max_transform_hierarchy_depth_intra
        let scaling_list_enabled = br.read_bit().unwrap_or(0);
        if scaling_list_enabled == 1 {
            let sps_scaling_list_present = br.read_bit().unwrap_or(0);
            if sps_scaling_list_present == 1 {
                return; // custom scaling lists are not read
            }
        }
        let _ = br.read_bit(); // amp_enabled_flag
        let _ = br.read_bit(); // sample_adaptive_offset_enabled_flag
        let pcm_enabled = br.read_bit().unwrap_or(0);
        if pcm_enabled == 1 {
            let _ = br.read_bits(4);
            let _ = br.read_bits(4);
            let _ = br.read_ue();
            let _ = br.read_ue();
            let _ = br.read_bit();
        }
        let ref_pic_sets = hevc_sps::skip_ref_pic_sets(
            &mut br,
            log2_max_pic_order_cnt_lsb_minus4,
            BitReader::read_bits,
            BitReader::read_ue,
        );
        if ref_pic_sets.is_none() {
            return;
        }
        let _ = br.read_bit(); // sps_temporal_mvp_enabled_flag
        let _ = br.read_bit(); // strong_intra_smoothing_enabled_flag
        let vui_present = br.read_bit().unwrap_or(0);
        if vui_present != 1 {
            self.aspect_ratio = format_aspect_ratio(width, height, 1, 1);
            return;
        }
        self.parse_vui(&mut br, width, height);
    }

    fn parse_vui(&mut self, br: &mut BitReader, pic_width: u32, pic_height: u32) {
        let aspect_ratio_info_present = br.read_bit().unwrap_or(0);
        let (sar_w, sar_h) = if aspect_ratio_info_present == 1 {
            let aspect_ratio_idc = br.read_bits(8).unwrap_or(0) as u8;
            if aspect_ratio_idc == 255 {
                let sar_w = br.read_bits(16).unwrap_or(1);
                let sar_h = br.read_bits(16).unwrap_or(1);
                (sar_w, sar_h)
            } else {
                // Table E-1 in H.264 spec (subset covering common values).
                match aspect_ratio_idc {
                    1 => (1, 1),
                    2 => (12, 11),
                    3 => (10, 11),
                    4 => (16, 11),
                    5 => (40, 33),
                    6 => (24, 11),
                    7 => (20, 11),
                    8 => (32, 11),
                    9 => (80, 33),
                    10 => (18, 11),
                    11 => (15, 11),
                    12 => (64, 33),
                    13 => (160, 99),
                    14 => (4, 3),
                    15 => (3, 2),
                    16 => (2, 1),
                    _ => (1, 1),
                }
            }
        } else {
            (1, 1)
        };

        self.aspect_ratio = format_aspect_ratio(pic_width, pic_height, sar_w, sar_h);

        let overscan_info_present = br.read_bit().unwrap_or(0);
        if overscan_info_present == 1 {
            let _ = br.read_bit(); // overscan_appropriate_flag
        }
        let video_signal_type_present = br.read_bit().unwrap_or(0);
        if video_signal_type_present == 1 {
            let _ = br.read_bits(3); // video_format
            let video_full_range_flag = br.read_bit().unwrap_or(0);
            self.video_range = Some(if video_full_range_flag == 1 { "full" } else { "limited" });
            let colour_description_present = br.read_bit().unwrap_or(0);
            if colour_description_present == 1 {
                let cp = br.read_bits(8).unwrap_or(2) as u8;
                let tc = br.read_bits(8).unwrap_or(2) as u8;
                let mx = br.read_bits(8).unwrap_or(2) as u8;
                self.colour_primaries = Some(colour_primaries_name(cp));
                self.transfer_characteristics = Some(transfer_name(tc));
                self.matrix_coefficients = Some(matrix_name(mx));
            }
        }
    }

    fn parse_avc_sei(&mut self, rbsp: &[u8]) {
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
            // payload_type 144 = content_light_level_info
            if payload_type == 144 && payload.len() >= 4 {
                self.max_cll = Some(((payload[0] as u32) << 8) | payload[1] as u32);
                self.max_fall = Some(((payload[2] as u32) << 8) | payload[3] as u32);
            }
            if i < rbsp.len() && rbsp[i] == 0x80 {
                return;
            }
            i = end;
        }
    }

    fn parse_hevc_sei(&mut self, rbsp: &[u8]) {
        // Same structure as AVC SEI (ITU-T H.265 §7.3.2.4).
        self.parse_avc_sei(rbsp);
    }

    /// Scan PES bytes for an AFD wrapper. In ATSC A/53 user_data, AFD rides
    /// inside `user_data_type_code = 0x00` followed by a single byte where
    /// the low 4 bits carry the `active_format` value (0..15). H.264/H.265
    /// carry this inside SEI T.35 country_code 0xB5 + ATSC provider 0x0031.
    fn scan_for_afd(&mut self, bytes: &[u8]) {
        // Naïve scan for the ATSC user-data signature `B5 00 31 47 41 39 34`
        // followed by user_data_type_code 0x05 (AFD_data).
        let sig = [0xB5u8, 0x00, 0x31, 0x47, 0x41, 0x39, 0x34];
        let mut i = 0;
        while i + sig.len() + 2 < bytes.len() {
            if bytes[i..i + sig.len()] == sig {
                let type_code = bytes[i + sig.len()];
                // 0x05 = AFD_data per ATSC A/53
                if type_code == 0x05 {
                    // AFD byte: `1 _ a a a a _ _` where `a` bits 3..0 are
                    // active_format. Spec-wise the AFD byte is after a
                    // `0b1_0000000` marker, but in practice we can read
                    // the low 4 bits of the next byte.
                    let afd_byte = bytes[i + sig.len() + 1];
                    self.afd = Some(afd_byte & 0x0F);
                }
                i += sig.len();
            } else {
                i += 1;
            }
        }
    }

    pub fn snapshot(&self) -> Option<SignallingStats> {
        if !self.seen_any {
            return None;
        }
        let hdr = match self.transfer_characteristics {
            Some("smpte2084") => "hdr10",
            Some("arib-std-b67") => "hlg",
            Some(_) => "sdr",
            None => "unknown",
        };
        Some(SignallingStats {
            aspect_ratio: self.aspect_ratio.clone(),
            colour_primaries: self.colour_primaries.map(String::from),
            transfer_characteristics: self.transfer_characteristics.map(String::from),
            matrix_coefficients: self.matrix_coefficients.map(String::from),
            video_range: self.video_range.map(String::from),
            hdr: hdr.into(),
            max_cll: self.max_cll,
            max_fall: self.max_fall,
            afd: self.afd,
        })
    }
}

fn find_next_start_code(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 2 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && (bytes[i + 2] == 1 || (i + 3 < bytes.len() && bytes[i + 2] == 0 && bytes[i + 3] == 1)) {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_profile_tier_level(br: &mut BitReader, max_sub_layers_minus1: u32) -> bool {
    // general_profile_space(2) + general_tier_flag(1) + general_profile_idc(5)
    if br.read_bits(8).is_none() {
        return false;
    }
    // general_profile_compatibility_flag[32]
    if br.read_bits(32).is_none() {
        return false;
    }
    // general_progressive(1) interlaced(1) non_packed(1) frame_only(1) + 44 reserved
    if br.read_bits(16).is_none() || br.read_bits(32).is_none() {
        return false;
    }
    // general_level_idc (8)
    if br.read_bits(8).is_none() {
        return false;
    }
    hevc_sps::skip_sub_layers(max_sub_layers_minus1, |n| br.read_bits(n)).is_some()
}

fn format_aspect_ratio(pic_w: u32, pic_h: u32, sar_w: u32, sar_h: u32) -> Option<String> {
    if pic_w == 0 || pic_h == 0 {
        return None;
    }
    // Display AR numerator / denominator.
    let dar_w = pic_w * sar_w.max(1);
    let dar_h = pic_h * sar_h.max(1);
    let g = gcd(dar_w, dar_h);
    let n = dar_w / g;
    let d = dar_h / g;
    // Snap to well-known labels — exact match first, then a tolerance
    // pass for SPS conformance-window quirks (e.g. some 4K HEVC encoders
    // pad coded_height to a CTB boundary and over-report the crop, so the
    // raw ratio comes out 480:271 even though ffprobe reads 16:9 cleanly
    // off the same SPS). 0.5% is tight enough that genuine 1.85:1
    // theatrical or 5:3 boutique ratios still print exactly.
    let ratio = n as f64 / d as f64;
    const TOLERANCE: f64 = 0.005;
    let snaps: &[(f64, &str)] = &[
        (16.0 / 9.0, "16:9"),
        (4.0 / 3.0, "4:3"),
        (21.0 / 9.0, "21:9"),
        (256.0 / 135.0, "17:9 (DCI)"),
        (1.85, "1.85:1"),
        (2.39, "2.39:1"),
    ];
    for &(target, label) in snaps {
        if (ratio - target).abs() / target < TOLERANCE {
            return Some(label.into());
        }
    }
    Some(format!("{}:{}", n, d))
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
    }
}

fn colour_primaries_name(c: u8) -> &'static str {
    match c {
        1 => "bt709",
        4 => "bt470m",
        5 => "bt470bg",
        6 => "smpte170m",
        7 => "smpte240m",
        8 => "film",
        9 => "bt2020",
        10 => "smpte428",
        11 => "smpte431",
        12 => "smpte432",
        22 => "ebu3213",
        _ => "unspecified",
    }
}

fn transfer_name(t: u8) -> &'static str {
    match t {
        1 => "bt709",
        4 => "bt470m",
        5 => "bt470bg",
        6 => "smpte170m",
        7 => "smpte240m",
        8 => "linear",
        9 => "log100",
        10 => "log316",
        11 => "iec61966-2-4",
        12 => "bt1361",
        13 => "iec61966-2-1",
        14 => "bt2020-10",
        15 => "bt2020-12",
        16 => "smpte2084",
        17 => "smpte428",
        18 => "arib-std-b67",
        _ => "unspecified",
    }
}

fn matrix_name(m: u8) -> &'static str {
    match m {
        0 => "gbr",
        1 => "bt709",
        4 => "fcc",
        5 => "bt470bg",
        6 => "smpte170m",
        7 => "smpte240m",
        8 => "ycgco",
        9 => "bt2020-ncl",
        10 => "bt2020-cl",
        11 => "smpte2085",
        12 => "chroma-ncl",
        13 => "chroma-cl",
        14 => "ictcp",
        _ => "unspecified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::h264_sps::tests::{RANDOM, Sps, crops, edge_213, random_sps, sizes};

    /// The aspect ratio read off `sps`, after one off 4:3 pictures.
    fn aspect_ratio(sps: &Sps) -> Option<String> {
        let mut tracker = SignallingTracker::new();
        tracker.scan_avc(
            &Sps { width_in_mbs_minus1: 43, height_in_map_units_minus1: 32, ..Sps::default() }.annex_b(),
        );
        assert_eq!(tracker.aspect_ratio.as_deref(), Some("4:3"));
        tracker.scan_avc(&sps.annex_b());
        tracker.aspect_ratio
    }

    /// edge#213: an SPS whose size or crop is out of range has no aspect
    /// ratio. One with scaling lists is not read as far as its size, and
    /// leaves the last there was.
    #[test]
    fn an_avc_sps_out_of_range_has_no_aspect_ratio() {
        let [width, height, crop, scaling_list] = edge_213();
        for sps in [width, height, crop] {
            assert_eq!(aspect_ratio(&sps), None);
        }
        assert_eq!(aspect_ratio(&scaling_list).as_deref(), Some("4:3"));
        for (sps, size) in sizes().into_iter().chain(crops()) {
            assert_eq!(aspect_ratio(&sps), size.and_then(|(w, h)| format_aspect_ratio(w, h, 1, 1)));
        }
    }

    #[test]
    fn a_random_avc_sps_never_panics() {
        for sps in random_sps(RANDOM) {
            SignallingTracker::new().parse_avc_sps(&sps[1..]);
        }
    }

    /// edge#213: an HEVC SPS whose size or crop is out of range has no aspect
    /// ratio. One whose reference picture sets are is not read as far as its
    /// VUI, and leaves the last there was.
    #[test]
    fn an_hevc_sps_out_of_range_has_no_aspect_ratio() {
        use hevc_sps::tests::{Sps, crops, edge_213, sizes};
        let aspect_ratio = |sps: Sps| {
            let mut tracker = SignallingTracker::new();
            tracker.scan_hevc(&Sps { width: 704, height: 528, ..Sps::default() }.annex_b());
            assert_eq!(tracker.aspect_ratio.as_deref(), Some("4:3"));
            tracker.scan_hevc(&sps.annex_b());
            tracker.aspect_ratio
        };
        for (sps, size) in sizes().into_iter().chain(crops()) {
            for vui in [false, true] {
                let ratio = size.and_then(|(w, h)| format_aspect_ratio(w, h, 1, 1));
                assert_eq!(aspect_ratio(Sps { vui, ..sps }), ratio);
            }
        }
        assert_eq!(aspect_ratio(edge_213()).as_deref(), Some("4:3"));
    }

    /// What is shown of one HEVC SPS.
    fn shown(sps: &[u8]) -> SignallingStats {
        let mut tracker = SignallingTracker::new();
        tracker.set_codec_from_stream_type(0x24);
        tracker.scan_hevc(&[&[0, 0, 1], sps, &[0, 0, 1, 0x44, 0x01]].concat());
        tracker.snapshot().unwrap()
    }

    #[test]
    fn a_real_hevc_sps_shows_what_ffprobe_reads() {
        let real = hevc_sps::tests::real_sps();
        for (what, ratio, colours, range) in hevc_sps::tests::signalled() {
            let (_, sps, ..) = real.iter().find(|(of, ..)| *of == what).unwrap();
            let shown = shown(sps);
            assert_eq!(shown.aspect_ratio.as_deref(), ratio, "{what}");
            let [primaries, transfer, matrix] = colours.map_or([None; 3], |colours| colours.map(Some));
            assert_eq!(shown.colour_primaries.as_deref(), primaries, "{what}");
            assert_eq!(shown.transfer_characteristics.as_deref(), transfer, "{what}");
            assert_eq!(shown.matrix_coefficients.as_deref(), matrix, "{what}");
            assert_eq!(shown.video_range.as_deref(), range, "{what}");
        }
    }

    #[test]
    fn an_hevc_sps_is_read_past_its_sub_layers_and_reference_picture_sets() {
        use hevc_sps::tests::{Sps, sub_layered};
        for sps in sub_layered() {
            // 720x576 is 5:4 of square samples.
            assert_eq!(shown(&sps.nal()).aspect_ratio.as_deref(), Some("5:4"), "{:?}", sps.sub_layers);
        }
        let ratio = |ref_pic_sets, long_term_pics, log2_max_poc_lsb_minus4, vui| {
            let sps = Sps { ref_pic_sets, long_term_pics, log2_max_poc_lsb_minus4, vui, ..Sps::default() };
            shown(&sps.nal()).aspect_ratio
        };
        for (sets, pics) in [(2, None), (64, Some(0)), (0, Some(1)), (0, Some(32)), (3, Some(5))] {
            for vui in [false, true] {
                assert_eq!(ratio(sets, pics, 4, vui).as_deref(), Some("16:9"), "{sets} {pics:?} {vui}");
            }
        }
        // Past their bounds, the SPS is not read.
        for (sets, pics, lsb_minus4) in [(65, None, 4), (0, Some(33), 4), (0, Some(1), 13)] {
            assert_eq!(ratio(sets, pics, lsb_minus4, true), None, "{sets} {pics:?} {lsb_minus4}");
        }
    }

    #[test]
    fn a_random_hevc_sps_in_range_shows_what_it_says() {
        use hevc_sps::tests::{VALID, valid_sps};
        for (i, (sps, says)) in valid_sps(VALID).enumerate() {
            let shown = shown(&sps);
            let shown = (
                shown.aspect_ratio,
                shown.colour_primaries,
                shown.transfer_characteristics,
                shown.matrix_coefficients,
                shown.video_range,
            );
            if says.scaling_lists {
                assert_eq!(shown, (None, None, None, None, None), "{i}");
                continue;
            }
            let size = hevc_sps::picture_size(says.coded.0, says.coded.1, says.conf_win, (2, 2)).unwrap();
            let ratio = format_aspect_ratio(size.0, size.1, says.sample_aspect.0, says.sample_aspect.1);
            let name = |i: usize, name: fn(u8) -> &'static str| says.colours.map(|codes| name(codes[i]).into());
            let range = says.full_range.map(|full| if full { "full" } else { "limited" }.into());
            let says = (ratio, name(0, colour_primaries_name), name(1, transfer_name), name(2, matrix_name), range);
            assert_eq!(shown, says, "{i}");
        }
    }

    #[test]
    fn an_hevc_crop_is_in_units_of_2x2() {
        for (what, sps, (w, h)) in hevc_sps::tests::cropped_by_2x2() {
            let mut tracker = SignallingTracker::new();
            tracker.scan_hevc(&[&[0, 0, 1], &sps[..], &[0, 0, 1, 0x44, 0x01]].concat());
            let ratio = format_aspect_ratio(w, h, 1, 1);
            assert!(ratio.is_some() && tracker.aspect_ratio == ratio, "{what}: {:?}", tracker.aspect_ratio);
        }
    }

    /// The widest and tallest picture taken, by the widest and tallest sample
    /// aspect ratio there is.
    #[test]
    fn the_largest_aspect_ratio_there_is_fits() {
        assert_eq!(format_aspect_ratio(16_384, 1, 65_535, 1).as_deref(), Some("1073725440:1"));
        assert_eq!(format_aspect_ratio(1, 16_384, 1, 65_535).as_deref(), Some("1:1073725440"));
    }

    #[test]
    fn a_random_hevc_sps_never_panics() {
        for sps in hevc_sps::tests::random_sps(hevc_sps::tests::RANDOM) {
            SignallingTracker::new().parse_hevc_sps(&unescape_rbsp(&sps[2..]));
        }
    }
}
