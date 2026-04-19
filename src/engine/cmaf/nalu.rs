// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! H.264 / HEVC NAL unit helpers used by the CMAF muxer.
//!
//! Source frames arrive from `engine::ts_demux::TsDemuxer` with NAL units
//! already split by start code (start codes are stripped). Each CMAF sample
//! carries its NAL units in AVCC / HVCC length-prefixed form (4-byte BE
//! size + bytes), which is what browsers and `mov`/`mp4` parsers expect
//! for the `avc1`/`hvc1` sample entry.
//!
//! # H.264 NAL types (RFC 6184 / ITU-T H.264 §7.3.1)
//! - 1  coded slice (non-IDR)
//! - 5  coded slice (IDR)
//! - 6  SEI
//! - 7  SPS
//! - 8  PPS
//! - 9  access unit delimiter
//! - 12 filler data
//!
//! # HEVC NAL types (ITU-T H.265 §7.3.1.2)
//! - 19 IDR_W_RADL
//! - 20 IDR_N_LP
//! - 21 CRA_NUT
//! - 32 VPS
//! - 33 SPS
//! - 34 PPS

/// Pack a slice of NAL units into a single AVCC/HVCC sample buffer.
/// Each NAL unit gets a 4-byte big-endian length prefix.
pub fn to_length_prefixed(nalus: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = nalus.iter().map(|n| 4 + n.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nalu in nalus {
        out.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        out.extend_from_slice(nalu);
    }
    out
}

/// H.264 NAL header byte layout (RFC 6184 §1.3):
/// `forbidden_zero_bit(1) | nal_ref_idc(2) | nal_unit_type(5)`.
pub fn h264_nal_type(nal: &[u8]) -> u8 {
    nal.first().copied().unwrap_or(0) & 0x1F
}

/// HEVC NAL header (ITU-T H.265 §7.3.1.2):
/// `forbidden_zero_bit(1) | nal_unit_type(6) | nuh_layer_id(6) | temporal_id_plus1(3)`.
/// Packs into a 16-bit header. We return the type (`[0]>>1 & 0x3F`).
pub fn h265_nal_type(nal: &[u8]) -> u8 {
    (nal.first().copied().unwrap_or(0) >> 1) & 0x3F
}

pub const H264_NAL_SLICE_IDR: u8 = 5;
pub const H264_NAL_SEI: u8 = 6;
pub const H264_NAL_SPS: u8 = 7;
pub const H264_NAL_PPS: u8 = 8;
pub const H264_NAL_AUD: u8 = 9;

pub const H265_NAL_IDR_W_RADL: u8 = 19;
pub const H265_NAL_IDR_N_LP: u8 = 20;
pub const H265_NAL_CRA: u8 = 21;
pub const H265_NAL_VPS: u8 = 32;
pub const H265_NAL_SPS: u8 = 33;
pub const H265_NAL_PPS: u8 = 34;
pub const H265_NAL_AUD: u8 = 35;

/// Strip parameter sets and access-unit delimiters from a NAL list.
///
/// CMAF init segments carry SPS/PPS/VPS in the sample entry; fragments
/// must not repeat them inside the sample data (they confuse strict
/// players). AUDs are also dropped since they carry no decodable payload.
pub fn filter_frame_nalus_h264(nalus: &[Vec<u8>]) -> Vec<Vec<u8>> {
    nalus
        .iter()
        .filter(|n| {
            let t = h264_nal_type(n);
            t != H264_NAL_SPS && t != H264_NAL_PPS && t != H264_NAL_AUD
        })
        .cloned()
        .collect()
}

/// HEVC variant of [`filter_frame_nalus_h264`].
pub fn filter_frame_nalus_h265(nalus: &[Vec<u8>]) -> Vec<Vec<u8>> {
    nalus
        .iter()
        .filter(|n| {
            let t = h265_nal_type(n);
            t != H265_NAL_VPS && t != H265_NAL_SPS && t != H265_NAL_PPS && t != H265_NAL_AUD
        })
        .cloned()
        .collect()
}

/// Does this NAL list contain an H.264 IDR slice?
pub fn h264_has_idr(nalus: &[Vec<u8>]) -> bool {
    nalus.iter().any(|n| h264_nal_type(n) == H264_NAL_SLICE_IDR)
}

/// Does this NAL list contain an HEVC random-access point
/// (IDR_W_RADL / IDR_N_LP / CRA_NUT)?
pub fn h265_has_rap(nalus: &[Vec<u8>]) -> bool {
    nalus.iter().any(|n| {
        matches!(
            h265_nal_type(n),
            H265_NAL_IDR_W_RADL | H265_NAL_IDR_N_LP | H265_NAL_CRA
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefix_round_trips() {
        let nalus = vec![vec![0x67, 0x42, 0x00, 0x1E], vec![0x68, 0xCE], vec![0x65, 0xB8]];
        let out = to_length_prefixed(&nalus);
        // Expected layout:
        //   4 B len (4) | 4 B nalu | 4 B len (2) | 2 B nalu | 4 B len (2) | 2 B nalu
        assert_eq!(out.len(), 4 + 4 + 4 + 2 + 4 + 2);
        assert_eq!(&out[0..4], &[0, 0, 0, 4]);
        assert_eq!(&out[4..8], &[0x67, 0x42, 0x00, 0x1E]);
        assert_eq!(&out[8..12], &[0, 0, 0, 2]);
        assert_eq!(&out[12..14], &[0x68, 0xCE]);
        assert_eq!(&out[14..18], &[0, 0, 0, 2]);
        assert_eq!(&out[18..20], &[0x65, 0xB8]);
    }

    #[test]
    fn h264_nal_type_extracts_low_five_bits() {
        // 0x67: forbidden=0, nal_ref_idc=11, nal_unit_type=7 (SPS) — 0b0110_0111
        assert_eq!(h264_nal_type(&[0x67, 0x42]), 7);
        // 0x65: nal_unit_type=5 (IDR) — 0b0110_0101
        assert_eq!(h264_nal_type(&[0x65]), 5);
        // 0x41: nal_unit_type=1 (non-IDR slice)
        assert_eq!(h264_nal_type(&[0x41]), 1);
    }

    #[test]
    fn h265_nal_type_extracts_middle_six_bits() {
        // HEVC VPS: type=32, so byte[0] = (32 << 1) | fzb = 0x40
        assert_eq!(h265_nal_type(&[0x40, 0x01]), 32);
        // HEVC IDR_W_RADL: type=19 → 0x26
        assert_eq!(h265_nal_type(&[0x26, 0x01]), 19);
    }

    #[test]
    fn filter_drops_h264_sps_pps_aud() {
        let sps = vec![0x67, 0x42];
        let pps = vec![0x68, 0xCE];
        let aud = vec![0x09, 0x10];
        let idr = vec![0x65, 0xB8];
        let nalus = vec![sps, pps, aud, idr.clone()];
        let out = filter_frame_nalus_h264(&nalus);
        assert_eq!(out, vec![idr]);
    }

    #[test]
    fn h264_has_idr_detects_type_5() {
        let idr = vec![0x65, 0xB8];
        let p = vec![0x41, 0x00];
        assert!(h264_has_idr(&[idr.clone()]));
        assert!(!h264_has_idr(&[p.clone()]));
        assert!(h264_has_idr(&[p, idr]));
    }

    #[test]
    fn h265_has_rap_detects_idr_and_cra() {
        let idr_w_radl = vec![0x26, 0x01]; // type=19
        let cra = vec![0x2A, 0x01]; // type=21
        let trail = vec![0x02, 0x01]; // type=1
        assert!(h265_has_rap(&[idr_w_radl]));
        assert!(h265_has_rap(&[cra]));
        assert!(!h265_has_rap(&[trail]));
    }
}
