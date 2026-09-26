// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared PSI test fixtures (test builds only).
//!
//! The VH1 packets are real bytes from `testbed/test_ts/VH1.ts` (an ATSC /
//! DigiCipher capture): every PMT-PID packet carries a 24-byte short-form
//! 0xC0 section at the pointer target and the PMT right behind it at packet
//! offset 29. Every PMT parser that assumed "the pointer target is the PMT"
//! found nothing on this stream.

use super::ts_parse::{mpeg2_crc32, TS_PACKET_SIZE, TS_SYNC_BYTE};

fn hex_packet(hex: &str) -> [u8; TS_PACKET_SIZE] {
    let hex: String = hex.split_whitespace().collect();
    assert_eq!(hex.len(), TS_PACKET_SIZE * 2, "fixture must be one TS packet");
    let mut pkt = [0u8; TS_PACKET_SIZE];
    for (i, b) in pkt.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex");
    }
    pkt
}

/// VH1.ts packet #134 — PMT PID 0x0031, program 2010 (0x07DA), version 11.
/// Layout: `pointer 0 | 0xC0 section (24 B, SSI=0) | PMT at offset 29`.
/// PMT: PCR_PID 0x0E0F; program_info = CA (GI, 0x4749) + registration
/// "CUEI"; ES 0x02/0x0E0F (MPEG-2 video), 0x81/0x0E10 (AC-3, ISO 639
/// "eng"), 0x86/0x0E11 (SCTE-35), 0xC0/0x0E12..0x0E14 (private).
pub fn vh1_pmt_packet() -> [u8; TS_PACKET_SIZE] {
    hex_packet(concat!(
        "4740311500c000150007da00b600000000000001000000000042aa829b02b05607da",
        "d70000ee0ff00c09044749e10105044355454902ee0ff00081ee10f0060a04656e67",
        "0086ee11f000c0ee12f008050445545631a100c0ee13f009050445545631a20100c0",
        "ee14f008bf06496e766964699c7a7f3effffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffff",
    ))
}

/// VH1.ts packet #97 — the PAT: program 2010 → PMT PID 0x0031.
pub fn vh1_pat_packet() -> [u8; TS_PACKET_SIZE] {
    hex_packet(concat!(
        "4740001a0000b00d0000c1000007dae031266bf6a6ffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffff",
    ))
}

/// Offset of the PMT section inside [`vh1_pmt_packet`].
pub const VH1_PMT_OFFSET: usize = 29;
pub const VH1_PROGRAM: u16 = 2010;

/// Build a complete long-form PMT section (table_id .. CRC_32).
pub fn pmt_section(
    program_number: u16,
    version: u8,
    pcr_pid: u16,
    program_info: &[u8],
    es: &[(u8, u16, &[u8])],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&program_number.to_be_bytes());
    body.push(0xC1 | ((version & 0x1F) << 1));
    body.push(0x00);
    body.push(0x00);
    body.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
    body.push(pcr_pid as u8);
    body.push(0xF0 | ((program_info.len() >> 8) as u8 & 0x0F));
    body.push(program_info.len() as u8);
    body.extend_from_slice(program_info);
    for (st, pid, info) in es {
        body.push(*st);
        body.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        body.push(*pid as u8);
        body.push(0xF0 | ((info.len() >> 8) as u8 & 0x0F));
        body.push(info.len() as u8);
        body.extend_from_slice(info);
    }
    let section_length = body.len() + 4;
    let mut sec = vec![0x02, 0xB0 | ((section_length >> 8) as u8 & 0x0F), section_length as u8];
    sec.extend_from_slice(&body);
    let crc = mpeg2_crc32(&sec);
    sec.extend_from_slice(&crc.to_be_bytes());
    sec
}

/// Build a complete PAT section.
pub fn pat_section(programs: &[(u16, u16)], version: u8) -> Vec<u8> {
    let section_length = 5 + 4 * programs.len() + 4;
    let mut sec = vec![
        0x00,
        0xB0 | ((section_length >> 8) as u8 & 0x0F),
        section_length as u8,
        0x00,
        0x01,
        0xC1 | ((version & 0x1F) << 1),
        0x00,
        0x00,
    ];
    for (pn, pid) in programs {
        sec.extend_from_slice(&pn.to_be_bytes());
        sec.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        sec.push(*pid as u8);
    }
    let crc = mpeg2_crc32(&sec);
    sec.extend_from_slice(&crc.to_be_bytes());
    sec
}

/// Lay `sections` back to back on `pid` starting with pointer_field 0,
/// setting PUSI (with a pointer to the first new section) on every packet
/// in which a section starts, and 0xFF-stuffing the tail. CC counts up
/// from `cc_start`. Mirrors how a conforming muxer packs one PSI unit.
pub fn packetize_sections(pid: u16, sections: &[&[u8]], cc_start: u8) -> Vec<[u8; TS_PACKET_SIZE]> {
    let mut data = Vec::new();
    let mut starts = Vec::new();
    for s in sections {
        starts.push(data.len());
        data.extend_from_slice(s);
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut cc = cc_start & 0x0F;
    while pos < data.len() {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[2] = pid as u8;
        pkt[3] = 0x10 | cc;
        let next_start = starts.iter().copied().find(|&s| s >= pos);
        let cap = TS_PACKET_SIZE - 4;
        match next_start {
            Some(s) if s + 1 < pos + cap => {
                pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
                pkt[4] = (s - pos) as u8;
                let n = (cap - 1).min(data.len() - pos);
                pkt[5..5 + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
            Some(s) if s + 1 == pos + cap => {
                pkt[1] = (pid >> 8) as u8 & 0x1F;
                let n = cap - 1;
                pkt[4..4 + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
            _ => {
                pkt[1] = (pid >> 8) as u8 & 0x1F;
                let n = cap.min(data.len() - pos);
                pkt[4..4 + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
        }
        out.push(pkt);
        cc = (cc + 1) & 0x0F;
    }
    out
}

/// One-packet PAT for `programs` on PID 0 with CC `cc`.
pub fn pat_packet(programs: &[(u16, u16)], version: u8, cc: u8) -> [u8; TS_PACKET_SIZE] {
    let sec = pat_section(programs, version);
    packetize_sections(0, &[&sec], cc)[0]
}

/// A PMT that spans two packets: 12 audio ES, each carrying an ISO 639
/// language descriptor and a stream_identifier, with an MP2 target at the
/// end so it lands in the SECOND packet. Returns `(section, target_pid)`.
pub fn two_packet_pmt(program_number: u16, version: u8) -> (Vec<u8>, u16) {
    let descs: Vec<Vec<u8>> = (0..12u8)
        .map(|i| vec![0x0A, 0x04, b'e', b'n', b'a' + i, 0x00, 0x52, 0x01, i])
        .collect();
    let mut es: Vec<(u8, u16, &[u8])> = Vec::new();
    es.push((0x1B, 0x0100, &[]));
    for (i, d) in descs.iter().enumerate() {
        // 11 teletext-shaped private ES first, the MP2 target last.
        let st = if i == 11 { 0x03 } else { 0x06 };
        es.push((st, 0x0200 + i as u16, d.as_slice()));
    }
    let sec = pmt_section(program_number, version, 0x0100, &[], &es);
    assert!(sec.len() > TS_PACKET_SIZE - 5, "fixture must span two packets");
    (sec, 0x020B)
}

/// Write a 5-byte PES timestamp (`marker` = 0x2 PTS-only, 0x3 PTS with DTS
/// following, 0x1 DTS).
fn put_ts(dst: &mut [u8], marker: u8, v: u64) {
    let v = v & 0x1_FFFF_FFFF;
    dst[0] = (marker << 4) | (((v >> 29) as u8) & 0x0E) | 0x01;
    dst[1] = (v >> 22) as u8;
    dst[2] = (((v >> 14) as u8) & 0xFE) | 0x01;
    dst[3] = (v >> 7) as u8;
    dst[4] = (((v << 1) as u8) & 0xFE) | 0x01;
}

/// A PUSI packet starting a PES on `pid` (payload only, CC `cc`) with
/// `stream_id`, a PTS and an optional DTS; the rest of the packet is 0xAA
/// ES bytes.
pub fn pes_start_packet(
    pid: u16,
    cc: u8,
    stream_id: u8,
    pts: u64,
    dts: Option<u64>,
) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xAAu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = pid as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt[4..8].copy_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    pkt[8] = 0x00;
    pkt[9] = 0x00;
    pkt[10] = 0x80;
    match dts {
        Some(d) => {
            pkt[11] = 0xC0;
            pkt[12] = 10;
            put_ts(&mut pkt[13..18], 0x3, pts);
            put_ts(&mut pkt[18..23], 0x1, d);
        }
        None => {
            pkt[11] = 0x80;
            pkt[12] = 5;
            put_ts(&mut pkt[13..18], 0x2, pts);
        }
    }
    pkt
}

/// A payload-only continuation packet on `pid` (0xAA bytes), CC `cc`.
pub fn payload_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xAAu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = (pid >> 8) as u8 & 0x1F;
    pkt[2] = pid as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt
}

/// The TS packets of one PES on `pid` carrying `es` with `pts`
/// (`stream_id`, PES_packet_length set, data_alignment_indicator 0), the
/// last one padded with adaptation-field stuffing; CC counted on from `cc`.
pub fn pes_packets(pid: u16, stream_id: u8, es: &[u8], pts: u64, cc: &mut u8) -> Vec<u8> {
    let mut pes = vec![0x00, 0x00, 0x01, stream_id];
    pes.extend_from_slice(&((8 + es.len()) as u16).to_be_bytes());
    pes.extend_from_slice(&[0x80, 0x80, 0x05]);
    let mut ts = [0u8; 5];
    put_ts(&mut ts, 0x2, pts);
    pes.extend_from_slice(&ts);
    pes.extend_from_slice(es);
    let mut out = Vec::new();
    for (i, chunk) in pes.chunks(184).enumerate() {
        let mut pkt = vec![
            TS_SYNC_BYTE,
            if i == 0 { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1F),
            pid as u8,
        ];
        let stuffing = 184 - chunk.len();
        if stuffing == 0 {
            pkt.push(0x10 | (*cc & 0x0F));
        } else {
            pkt.push(0x30 | (*cc & 0x0F));
            pkt.push((stuffing - 1) as u8);
            if stuffing > 1 {
                pkt.push(0x00);
                pkt.extend(std::iter::repeat_n(0xFF, stuffing - 2));
            }
        }
        pkt.extend_from_slice(chunk);
        *cc = (*cc + 1) & 0x0F;
        out.extend_from_slice(&pkt);
    }
    out
}

/// A one-program TS (PAT, PMT with H.264 on 0x100 and AAC ADTS on 0x101)
/// carrying the ADTS frames of `adts`, one PES per frame — enough for a
/// demuxer to lock the audio PID and cache its AAC config.
pub fn aac_program_ts(adts: &[u8]) -> Vec<u8> {
    let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
    let mut ts = pat_packet(&[(1, 0x1000)], 0, 0).to_vec();
    ts.extend_from_slice(&packetize_sections(0x1000, &[&pmt], 0)[0]);
    let (mut off, mut cc, mut pts) = (0usize, 0u8, 90_000u64);
    while off + 7 <= adts.len() {
        let len = (((adts[off + 3] as usize) & 0x03) << 11)
            | ((adts[off + 4] as usize) << 3)
            | ((adts[off + 5] as usize) >> 5);
        if len < 7 || off + len > adts.len() {
            break;
        }
        ts.extend(pes_packets(0x101, 0xC0, &adts[off..off + len], pts, &mut cc));
        off += len;
        pts += 1920;
    }
    ts
}

/// One second of a 1 kHz tone (and 440 Hz on the right) as HE-AAC ADTS
/// frames — v1 (SBR) or v2 (SBR + PS) at 48 kHz stereo — from the fdk
/// encoder. ADTS signals SBR implicitly: every header carries the AAC-LC
/// profile and the 24 kHz core rate, and v2's mono core.
#[cfg(feature = "fdk-aac")]
pub fn he_aac_adts(v2: bool) -> Vec<u8> {
    let mut enc = aac_audio::AacEncoder::open(&aac_codec::EncoderConfig {
        profile: if v2 { aac_codec::AacProfile::HeAacV2 } else { aac_codec::AacProfile::HeAacV1 },
        sample_rate: 48_000,
        channels: 2,
        bitrate: if v2 { 32_000 } else { 64_000 },
        afterburner: true,
        sbr_signaling: aac_codec::SbrSignaling::Implicit,
        transport: aac_codec::TransportType::Adts,
    })
    .expect("fdk HE-AAC encoder");
    let n = enc.frame_size() as usize;
    let mut out = Vec::new();
    for k in 0..48_000 / n {
        let tone = |f: f32| -> Vec<f32> {
            (0..n)
                .map(|i| 0.3 * (2.0 * std::f32::consts::PI * f * (k * n + i) as f32 / 48_000.0).sin())
                .collect()
        };
        out.extend(enc.encode_frame(&[tone(1_000.0), tone(440.0)]).expect("encode").bytes);
    }
    out
}
