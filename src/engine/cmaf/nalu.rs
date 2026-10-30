// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! H.264 / HEVC NAL unit helpers used by the CMAF muxer and the RTMP output.
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
//! - 14..=18 prefix / subset SPS / reserved
//!
//! # HEVC NAL types (ITU-T H.265 §7.3.1.2)
//! - 19 IDR_W_RADL
//! - 20 IDR_N_LP
//! - 21 CRA_NUT
//! - 32 VPS
//! - 33 SPS
//! - 34 PPS

use super::codecs::BitReader;

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

pub const H264_NAL_SLICE: u8 = 1;
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

/// payloadType of the recovery-point SEI message (H.264 §D.1.8).
const SEI_RECOVERY_POINT: usize = 6;

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

/// Whether an H.264 access unit is a random-access point that a
/// recovery-point SEI marks. **IDRs are excluded**: OR this with
/// [`h264_has_idr`] or the demuxer's keyframe flag.
///
/// The rule: the access unit's first picture is coded intra (every slice of
/// it I or SI), and ahead of that picture, within the access unit, an SEI NAL
/// holds a complete recovery-point message (payloadType 6, payloadSize at
/// least 1). "Within the access unit" starts at its own AUD — the last one
/// before its first VCL NAL, or the start of the list when there is none —
/// since an SEI before that AUD belongs to another access unit. This is how
/// an IDR-less open-GOP source marks where a decoder can start: every
/// non-IDR random-access point of the DVB services measured for edge#211 is
/// an I picture behind such an SEI, with SPS / PPS, and none of their P or B
/// pictures carries one.
///
/// Neither half alone. A recovery-point SEI on a P picture is gradual
/// decoding refresh: a receiver joining there would be handed a picture only
/// partly refreshed for `recovery_frame_cnt` pictures. An I picture alone may
/// be a scene cut whose followers reference pictures before it. Nor an access
/// unit that carries an SPS: an encoder that repeats its parameter sets on
/// every picture would key every P picture.
///
/// Only the first picture is judged. It ends at the next AUD, SEI, SPS, PPS,
/// NAL 15-18 (§7.4.1.2.3), or a later slice with `first_mb_in_slice` 0 — the
/// next field or picture in the same PES. A recovery point is often an I / P
/// field pair in one PES, which a rule wanting every slice of the PES intra
/// would miss. An SVC prefix NAL (14) precedes every base-layer slice of a
/// picture, so it is skipped rather than taken as a boundary.
///
/// `recovery_frame_cnt`, `exact_match_flag` and `broken_link_flag` are not
/// read, deliberately. Every recovery point measured has `exact_match_flag` 0
/// and `recovery_frame_cnt` 0, so requiring an exact match would key nothing,
/// and FFmpeg's parser keys on the message whatever its count. A payloadSize
/// of 0 is malformed — `recovery_point()` is at least 5 bits — and does not
/// count.
///
/// Every read is bounds-checked, so it never panics on what comes off the
/// wire. The intra check runs first: a P or B picture costs one slice header
/// (two Exp-Golomb reads, no allocation), and the SEI walk runs only on an
/// intra first picture (edge#211).
pub fn h264_is_sei_recovery_point(nalus: &[Vec<u8>]) -> bool {
    let Some(first) = nalus.iter().position(|n| matches!(h264_nal_type(n), 1..=5)) else {
        return false;
    };
    let (ahead, picture) = nalus.split_at(first);
    // An AUD opens an access unit: an SEI before it belongs to another.
    let ahead = &ahead[ahead.iter().rposition(|n| h264_nal_type(n) == H264_NAL_AUD).unwrap_or(0)..];
    h264_picture_is_intra(picture)
        && ahead.iter().any(|n| h264_nal_type(n) == H264_NAL_SEI && h264_sei_has_recovery_point(n))
}

/// Whether the first picture of `nalus` (which starts at its first VCL NAL)
/// is coded intra: every non-IDR slice of it I or SI. An IDR or a data
/// partition answers false — an IDR is the demuxer's keyframe, and data
/// partitions are Extended profile and not read.
fn h264_picture_is_intra(nalus: &[Vec<u8>]) -> bool {
    let mut slices = 0u32;
    for n in nalus {
        match h264_nal_type(n) {
            H264_NAL_SLICE => {
                let Some((first_mb, slice_type)) = h264_slice_head(n) else {
                    return false;
                };
                // The next field or picture.
                if slices > 0 && first_mb == 0 {
                    break;
                }
                match slice_type {
                    // Every slice of this picture is I / SI (Table 7-6).
                    7 | 9 => return true,
                    2 | 4 => slices += 1,
                    _ => return false,
                }
            }
            // The next access unit.
            H264_NAL_SEI | H264_NAL_SPS | H264_NAL_PPS | H264_NAL_AUD | 15..=18 => break,
            2..=H264_NAL_SLICE_IDR => return false,
            // Filler, end of sequence / stream, SVC prefix, auxiliary and
            // extension slices, an empty NAL.
            _ => {}
        }
    }
    slices > 0
}

/// `first_mb_in_slice` and `slice_type` from a slice header (H.264 §7.3.3,
/// Table 7-6); `None` when they cannot be read or the type is out of range.
fn h264_slice_head(nal: &[u8]) -> Option<(u32, u32)> {
    let mut r = BitReader::new(nal.get(1..)?);
    let first_mb = r.ue()?;
    let slice_type = r.ue()?;
    (slice_type <= 9).then_some((first_mb, slice_type))
}

/// Whether an SEI NAL holds a complete recovery-point message (H.264
/// §7.3.2.3.1). Each message's type and size are a run of 0xFF bytes plus a
/// final byte, and payloadSize counts RBSP bytes, so the walk skips payloads
/// with the emulation-prevention bytes taken out. A message counts only once
/// all of it is there. The trailing `0x80` reads as type 128 with no size
/// after it, which ends the walk; each pass consumes at least two bytes, so
/// the walk is bounded by the NAL's length.
fn h264_sei_has_recovery_point(nal: &[u8]) -> bool {
    fn walk(r: &mut BitReader<'_>) -> Option<()> {
        loop {
            let payload_type = sei_value(r)?;
            let size = sei_value(r)?;
            r.skip_bytes(size)?;
            if payload_type == SEI_RECOVERY_POINT && size >= 1 {
                return Some(());
            }
        }
    }
    let Some(body) = nal.get(1..) else {
        return false;
    };
    walk(&mut BitReader::new(body)).is_some()
}

/// One SEI payloadType or payloadSize: 0xFF bytes each adding 255, then a
/// final byte. In `usize`, never `u8`: type 262 must not read as 6.
fn sei_value(r: &mut BitReader<'_>) -> Option<usize> {
    let mut v = 0usize;
    loop {
        let b = r.read_bits(8)?;
        v = v.checked_add(b as usize)?;
        if b != 0xFF {
            return Some(v);
        }
    }
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
        assert!(h264_has_idr(std::slice::from_ref(&idr)));
        assert!(!h264_has_idr(std::slice::from_ref(&p)));
        assert!(h264_has_idr(&[p, idr]));
    }

    // ── edge#211: H.264 recovery points ──────────────────────────────────

    // Measured on broadcast-spts-h264-1080p25-ac3.ts: buffering_period, then
    // recovery_point (count 0, exact_match 0, broken_link 0), then
    // pic_timing; and an SEI holding pic_timing alone.
    const SEI_RP: &[u8] = &[0x06, 0x00, 0x01, 0xC0, 0x06, 0x01, 0x84, 0x01, 0x01, 0x32, 0x80];
    const SEI_PT: &[u8] = &[0x06, 0x01, 0x01, 0x32, 0x80];
    const I_SLICE: &[u8] = &[0x41, 0x88, 0xB8, 0xA4]; // slice_type 7
    const P_SLICE: &[u8] = &[0x41, 0x9A, 0xF0, 0x98]; // slice_type 5
    const B_SLICE: &[u8] = &[0x41, 0x9E, 0xE4, 0x8C]; // slice_type 6
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x01];
    const AUD: &[u8] = &[0x09, 0x10];
    const PPS: &[u8] = &[0x68, 0xFF, 0x3C, 0xB0];
    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xAD];
    // Hand-built slices, (first_mb_in_slice, slice_type).
    const I0: &[u8] = &[0x41, 0xB0, 0x80]; // (0, 2)
    const I120: &[u8] = &[0x41, 0x03, 0xCB, 0x80]; // (120, 2)
    const P120: &[u8] = &[0x41, 0x03, 0xCC, 0x80]; // (120, 0)
    const P0: &[u8] = &[0x41, 0xC0, 0x80]; // (0, 0)

    fn rp(nals: &[&[u8]]) -> bool {
        h264_is_sei_recovery_point(&nals.iter().map(|n| n.to_vec()).collect::<Vec<_>>())
    }

    #[test]
    fn a_recovery_point_i_picture_is_a_random_access_point() {
        assert!(rp(&[AUD, SPS, PPS, SEI_RP, I_SLICE]), "the measured access unit");
        assert!(rp(&[AUD, SEI_RP, I_SLICE]), "no SPS needed");
        assert!(rp(&[SEI_RP, I_SLICE]), "no AUD");
    }

    #[test]
    fn an_intra_picture_without_a_recovery_point_is_not() {
        assert!(!rp(&[AUD, SPS, PPS, SEI_PT, I_SLICE]));
    }

    /// Gradual decoding refresh stays out.
    #[test]
    fn a_recovery_point_on_a_p_or_b_picture_is_not() {
        assert!(!rp(&[SEI_RP, P_SLICE]));
        assert!(!rp(&[SEI_RP, B_SLICE]));
        assert!(!rp(&[AUD, SPS, PPS, SEI_PT, P_SLICE]));
    }

    #[test]
    fn an_idr_is_left_to_the_demuxer() {
        assert!(!rp(&[SPS, PPS, SEI_RP, IDR]));
    }

    #[test]
    fn every_slice_of_the_first_picture_must_be_intra() {
        assert_eq!(
            [I_SLICE, P_SLICE, B_SLICE, I0, I120, P120, P0].map(h264_slice_head),
            [Some((0, 7)), Some((0, 5)), Some((0, 6)), Some((0, 2)), Some((120, 2)), Some((120, 0)), Some((0, 0))],
            "the fixtures read as labelled",
        );
        assert!(rp(&[SEI_RP, I0, I120]));
        assert!(!rp(&[SEI_RP, I0, P120]));
    }

    #[test]
    fn only_the_first_picture_of_the_access_unit_counts() {
        assert!(rp(&[AUD, SPS, PPS, SEI_RP, I_SLICE, AUD, SEI_PT, P_SLICE]), "the measured I / P field pair");
        assert!(rp(&[SEI_RP, I0, P0]), "a field pair with no AUD between the fields");
        assert!(!rp(&[AUD, SEI_PT, P_SLICE, AUD, SEI_RP, I_SLICE]), "an I picture second in the PES");
        assert!(!rp(&[SEI_RP, AUD, SEI_PT, I_SLICE]), "an SEI before the access unit's AUD");
    }

    /// payloadSize counts RBSP bytes, and payloadType is not a byte.
    #[test]
    fn the_sei_walk_counts_rbsp_bytes() {
        // 300 RBSP bytes holding 00 00 01, which goes out as 00 00 03 01, and
        // ending in a byte that reads as type 6 to a walk off by one.
        let mut p = vec![0x11u8; 300];
        p[10..13].copy_from_slice(&[0x00, 0x00, 0x01]);
        p[299] = 0x06;
        let mut raw = p[..12].to_vec();
        raw.push(0x03);
        raw.extend_from_slice(&p[12..]);
        let sei = |after: &[u8]| {
            let mut n = vec![0x06, 0x05, 0xFF, 0x2D];
            n.extend_from_slice(&raw);
            n.extend_from_slice(after);
            n.push(0x80);
            n
        };
        // A raw-byte counter returns false on the first and true on the second.
        assert!(rp(&[&sei(&[0x06, 0x01, 0x84]), I_SLICE]));
        assert!(!rp(&[&sei(&[0x01, 0x01, 0x32]), I_SLICE]));
        // Type 255 + 7 = 262: as a u8 it would wrap to 6.
        assert!(!rp(&[&[0x06, 0xFF, 0x07, 0x01, 0xAA, 0x80], I_SLICE]));
    }

    #[test]
    fn malformed_access_units_are_false_and_never_panic() {
        assert!(!h264_is_sei_recovery_point(&[]));
        assert!(!h264_is_sei_recovery_point(&[vec![]]));
        assert!(!rp(&[SEI_RP, &[0x41]]));
        assert!(!rp(&[SEI_RP, &[0x41, 0, 0, 0, 0, 0]]), "a 32nd leading zero");
        assert!(!rp(&[&[0x06], I_SLICE]));
        assert!(!rp(&[&[0x06, 0x06], I_SLICE]), "a message cut short");
        assert!(!rp(&[&[0x06, 0x06, 0x05, 0x84], I_SLICE]), "a size past the end");
        assert!(!rp(&[&[0x06, 0x06, 0x00, 0x80], I_SLICE]), "a zero-size recovery point");
        let mut ff = vec![0x06];
        ff.extend(std::iter::repeat_n(0xFF, 10_000));
        assert!(!rp(&[&ff, I_SLICE]));
        // Every prefix of every NAL of the measured access unit, one NAL at
        // a time: whatever it answers, it answers without panicking.
        let full = [AUD, SPS, PPS, SEI_RP, I_SLICE];
        for i in 0..full.len() {
            for len in 0..=full[i].len() {
                let mut nals: Vec<Vec<u8>> = full.iter().map(|n| n.to_vec()).collect();
                nals[i].truncate(len);
                let _ = h264_is_sei_recovery_point(&nals);
            }
        }
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
