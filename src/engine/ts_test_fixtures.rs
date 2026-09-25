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
