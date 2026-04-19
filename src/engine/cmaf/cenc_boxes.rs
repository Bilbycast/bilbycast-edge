// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CENC-related ISO-BMFF box writers (`tenc`, `senc`, `saio`, `saiz`,
//! `pssh`, `sinf`/`frma`/`schm`/`schi` wrappers).
//!
//! Spec reference: ISO/IEC 23001-7:2023 §8 (tenc), §9/§10 (cenc/cbcs),
//! ISO/IEC 14496-12:2022 §8.7.8/§8.7.9 (saio/saiz).

use super::box_writer::BoxWriter;
use super::cenc::{SampleCencInfo, Scheme};

/// ClearKey Common Encryption system ID (W3C EME ClearKey).
/// `1077efec-c0b2-4d02-ace3-3c1e52e2fb4b`.
pub const CLEARKEY_SYSTEM_ID: [u8; 16] = [
    0x10, 0x77, 0xef, 0xec, 0xc0, 0xb2, 0x4d, 0x02,
    0xac, 0xe3, 0x3c, 0x1e, 0x52, 0xe2, 0xfb, 0x4b,
];

// ────────────────────────────────────────────────────────────────────────
//  tenc — Track Encryption (inside trak/mdia/minf/stbl/stsd/<entry>/sinf/schi)
// ────────────────────────────────────────────────────────────────────────

/// Write a `tenc` box for the given scheme and key ID.
pub fn write_tenc(
    parent: &mut BoxWriter<'_>,
    scheme: Scheme,
    key_id: &[u8; 16],
    per_sample_iv_size: u8,
) {
    let mut tenc = parent.child_full(*b"tenc", 1, 0);
    tenc.u8(0); // reserved
    match scheme {
        Scheme::Cenc => {
            // reserved(6)=0 | default_crypt_byte_block(0) | default_skip_byte_block(0)
            tenc.u8(0);
        }
        Scheme::Cbcs => {
            // For cbcs: default_crypt_byte_block=1, default_skip_byte_block=9
            tenc.u8((1 << 4) | 9);
        }
    }
    tenc.u8(1); // default_isProtected = true
    tenc.u8(per_sample_iv_size); // default_Per_Sample_IV_Size (16 for cenc, 0 for cbcs constant IV)
    tenc.bytes(key_id); // default_KID
    if matches!(scheme, Scheme::Cbcs) {
        // default_constant_IV_size + default_constant_IV (16 bytes of zero).
        tenc.u8(16);
        tenc.bytes(&[0u8; 16]);
    }
}

// ────────────────────────────────────────────────────────────────────────
//  sinf / frma / schm / schi — Protection Scheme Information
// ────────────────────────────────────────────────────────────────────────

/// Wrap a `sinf` container with `frma`, `schm`, and a `schi` holding
/// `tenc`. Call this *inside* a sample entry (avc1→encv or mp4a→enca)
/// as required for CENC.
pub fn write_sinf(
    parent: &mut BoxWriter<'_>,
    original_format: [u8; 4],
    scheme: Scheme,
    key_id: &[u8; 16],
    per_sample_iv_size: u8,
) {
    let mut sinf = parent.child(*b"sinf");
    {
        let mut frma = sinf.child(*b"frma");
        frma.fourcc(original_format);
    }
    {
        let mut schm = sinf.child_full(*b"schm", 0, 0);
        schm.fourcc(scheme.fourcc());
        schm.u32(0x0001_0000); // scheme_version = 1.0
    }
    {
        let mut schi = sinf.child(*b"schi");
        write_tenc(&mut schi, scheme, key_id, per_sample_iv_size);
    }
}

// ────────────────────────────────────────────────────────────────────────
//  pssh — Protection System Specific Header
// ────────────────────────────────────────────────────────────────────────

/// Write a ClearKey `pssh` box. Version 1 is used because it carries
/// the KID list directly, letting the ClearKey CDM find the correct
/// key without parsing data payload.
pub fn write_clearkey_pssh(parent: &mut BoxWriter<'_>, key_id: &[u8; 16]) {
    let mut pssh = parent.child_full(*b"pssh", 1, 0);
    pssh.bytes(&CLEARKEY_SYSTEM_ID);
    pssh.u32(1); // KID_count
    pssh.bytes(key_id);
    pssh.u32(0); // DataSize — ClearKey carries no payload
}

/// Copy an operator-supplied pre-built `pssh` box verbatim into the
/// parent. `pssh_bytes` must be a complete box (size+type+body).
pub fn write_verbatim_pssh(parent: &mut BoxWriter<'_>, pssh_bytes: &[u8]) -> Result<(), &'static str> {
    if pssh_bytes.len() < 8 {
        return Err("pssh box too small");
    }
    if &pssh_bytes[4..8] != b"pssh" {
        return Err("not a pssh box");
    }
    parent.bytes(pssh_bytes);
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
//  senc / saio / saiz — per-sample CENC auxiliary data
// ────────────────────────────────────────────────────────────────────────

/// Write a `senc` box and return the absolute start offset of its
/// auxiliary-data payload (IV + subsample entries) so the caller can
/// patch `saio` with the offset-from-moof.
///
/// `moof_start` is the byte offset of the enclosing `moof` box inside
/// the overall output buffer; `saio` stores offsets relative to it.
/// Callers who are building a standalone fragment supply `moof_start = 0`.
///
/// Returns `aux_data_offset_from_moof` — the value to patch into saio.
pub fn write_senc(
    parent: &mut BoxWriter<'_>,
    scheme: Scheme,
    samples: &[SampleCencInfo],
    moof_start: usize,
    cursor_pos_in_buffer: usize,
) -> u32 {
    // version 0, flags 0x000002 = UseSubSampleEncryption when any
    // sample has a non-empty subsample list.
    let has_subsamples = samples.iter().any(|s| !s.subsamples.is_empty());
    let flags: u32 = if has_subsamples { 0x0002 } else { 0x0000 };
    let mut senc = parent.child_full(*b"senc", 0, flags);
    senc.u32(samples.len() as u32); // sample_count

    // Absolute offset of the first IV byte from the start of the
    // enclosing `moof`. `cursor_pos_in_buffer` is the buffer offset
    // *before* this senc's outer size+type header was written (8
    // bytes), then add 8+4 (full-box header) + 4 (sample_count) to
    // reach the first aux entry.
    let aux_offset_abs = cursor_pos_in_buffer + 8 + 4 + 4;
    let aux_offset_from_moof = (aux_offset_abs - moof_start) as u32;

    let per_sample_iv_size = match scheme {
        Scheme::Cenc => 16,
        Scheme::Cbcs => 0,
    };
    for s in samples {
        if per_sample_iv_size > 0 {
            senc.bytes(&s.iv);
        }
        if has_subsamples {
            senc.u16(s.subsamples.len() as u16);
            for ss in &s.subsamples {
                senc.u16(ss.bytes_of_clear_data);
                senc.u32(ss.bytes_of_protected_data);
            }
        }
    }

    aux_offset_from_moof
}

/// Write a `saiz` box. Each sample's aux-info size is IV size +
/// (num_subsamples × 6 + 2) bytes.
pub fn write_saiz(parent: &mut BoxWriter<'_>, scheme: Scheme, samples: &[SampleCencInfo]) {
    let mut saiz = parent.child_full(*b"saiz", 0, 0);
    // aux_info_type + aux_info_type_parameter are absent (flags=0).
    // default_sample_info_size(8) — 0 means per-sample sizes follow.
    saiz.u8(0);
    saiz.u32(samples.len() as u32);
    let iv_size = match scheme {
        Scheme::Cenc => 16,
        Scheme::Cbcs => 0,
    };
    for s in samples {
        let subsample_bytes = if s.subsamples.is_empty() {
            0
        } else {
            2 + 6 * s.subsamples.len()
        };
        let total = iv_size + subsample_bytes;
        saiz.u8(total as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenc_cenc_layout() {
        let mut buf = Vec::new();
        {
            let mut p = BoxWriter::open(&mut buf, *b"schi");
            write_tenc(&mut p, Scheme::Cenc, &[0x42u8; 16], 16);
        }
        // Locate tenc inside schi.
        let tenc_pos = buf.windows(4).position(|w| w == b"tenc").unwrap();
        // version(1) + flags(3) + reserved(1) + pattern(1) + isProtected(1) + iv_size(1) + KID(16)
        // at offsets after the 4-byte type marker.
        assert_eq!(buf[tenc_pos + 4], 1); // version 1
        let body = tenc_pos + 8;
        assert_eq!(buf[body], 0); // reserved
        assert_eq!(buf[body + 1], 0); // cenc pattern = 0
        assert_eq!(buf[body + 2], 1); // isProtected
        assert_eq!(buf[body + 3], 16); // iv_size
        assert_eq!(&buf[body + 4..body + 20], &[0x42u8; 16]);
    }

    #[test]
    fn tenc_cbcs_layout_includes_constant_iv() {
        let mut buf = Vec::new();
        {
            let mut p = BoxWriter::open(&mut buf, *b"schi");
            write_tenc(&mut p, Scheme::Cbcs, &[0x42u8; 16], 0);
        }
        let tenc_pos = buf.windows(4).position(|w| w == b"tenc").unwrap();
        let body = tenc_pos + 8;
        // Pattern byte: crypt=1 << 4 | skip=9 = 0x19
        assert_eq!(buf[body + 1], 0x19);
        assert_eq!(buf[body + 2], 1); // isProtected
        assert_eq!(buf[body + 3], 0); // per_sample iv size = 0
        // After KID: constant IV size (16) + 16 zero bytes
        let kid_end = body + 4 + 16;
        assert_eq!(buf[kid_end], 16);
        assert_eq!(&buf[kid_end + 1..kid_end + 17], &[0u8; 16]);
    }

    #[test]
    fn clearkey_pssh_v1_layout() {
        let mut buf = Vec::new();
        {
            let mut p = BoxWriter::open(&mut buf, *b"moov");
            write_clearkey_pssh(&mut p, &[0x42u8; 16]);
        }
        let pssh_pos = buf.windows(4).position(|w| w == b"pssh").unwrap();
        // version = 1
        assert_eq!(buf[pssh_pos + 4], 1);
        // systemID at pssh_pos + 8
        assert_eq!(&buf[pssh_pos + 8..pssh_pos + 24], &CLEARKEY_SYSTEM_ID);
        // KID_count(4) = 1, followed by one 16-byte KID
        assert_eq!(
            u32::from_be_bytes(buf[pssh_pos + 24..pssh_pos + 28].try_into().unwrap()),
            1
        );
        assert_eq!(&buf[pssh_pos + 28..pssh_pos + 44], &[0x42u8; 16]);
    }

    #[test]
    fn verbatim_pssh_rejects_non_pssh() {
        let mut buf = Vec::new();
        {
            let mut p = BoxWriter::open(&mut buf, *b"moov");
            assert!(write_verbatim_pssh(&mut p, b"\0\0\0\x08abcd").is_err());
        }
    }

    #[test]
    fn saiz_records_per_sample_size() {
        let samples = vec![
            SampleCencInfo {
                iv: [0u8; 16],
                subsamples: vec![super::super::cenc::Subsample {
                    bytes_of_clear_data: 4,
                    bytes_of_protected_data: 96,
                }],
            },
            SampleCencInfo {
                iv: [0u8; 16],
                subsamples: vec![],
            },
        ];
        let mut buf = Vec::new();
        {
            let mut p = BoxWriter::open(&mut buf, *b"traf");
            write_saiz(&mut p, Scheme::Cenc, &samples);
        }
        let saiz_pos = buf.windows(4).position(|w| w == b"saiz").unwrap();
        let body = saiz_pos + 4 + 4; // skip type + ver+flags
        assert_eq!(buf[body], 0); // flags-byte (aux_info absent / default_size=0)
        // sample_count(4) = 2
        assert_eq!(
            u32::from_be_bytes(buf[body + 1..body + 5].try_into().unwrap()),
            2
        );
        // per-sample sizes: 16 (IV) + 2 (subsample_count) + 6 (one subsample) = 24
        assert_eq!(buf[body + 5], 24);
        // second: 16 + 0 = 16
        assert_eq!(buf[body + 6], 16);
    }
}
