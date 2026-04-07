// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Shared MPEG-TS packet parsing helpers.
//!
//! Zero-allocation inline functions for reading TS packet fields and
//! PSI sections (PAT, PMT). Used by both the TR-101290 analyzer and the
//! media analysis module.

use super::packet::RtpPacket;

pub const TS_PACKET_SIZE: usize = 188;
pub const TS_SYNC_BYTE: u8 = 0x47;
pub const RTP_HEADER_MIN_SIZE: usize = 12;
pub const PAT_PID: u16 = 0x0000;
pub const NULL_PID: u16 = 0x1FFF;

// ── TS Packet Field Accessors ────────────────────────────────────────────

#[inline(always)]
pub fn ts_pid(pkt: &[u8]) -> u16 {
    ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16
}

#[inline(always)]
pub fn ts_tei(pkt: &[u8]) -> bool {
    (pkt[1] & 0x80) != 0
}

#[inline(always)]
pub fn ts_pusi(pkt: &[u8]) -> bool {
    (pkt[1] & 0x40) != 0
}

#[inline(always)]
pub fn ts_cc(pkt: &[u8]) -> u8 {
    pkt[3] & 0x0F
}

#[inline(always)]
pub fn ts_adaptation_field_control(pkt: &[u8]) -> u8 {
    (pkt[3] >> 4) & 0x03
}

#[inline(always)]
pub fn ts_has_payload(pkt: &[u8]) -> bool {
    ts_adaptation_field_control(pkt) & 0x01 != 0
}

#[inline(always)]
pub fn ts_has_adaptation(pkt: &[u8]) -> bool {
    ts_adaptation_field_control(pkt) & 0x02 != 0
}

/// Check the discontinuity_indicator flag in the adaptation field.
/// When set, a PCR discontinuity is expected and should not be flagged as an error.
#[inline(always)]
pub fn ts_discontinuity_indicator(pkt: &[u8]) -> bool {
    if !ts_has_adaptation(pkt) {
        return false;
    }
    let af_len = pkt[4] as usize;
    if af_len == 0 {
        return false;
    }
    // Bit 7 of the adaptation field flags byte
    (pkt[5] & 0x80) != 0
}

/// Extract the 42-bit PCR base and 9-bit extension from the adaptation field,
/// returning the full PCR value in 27 MHz ticks.
pub fn extract_pcr(pkt: &[u8]) -> Option<u64> {
    if !ts_has_adaptation(pkt) {
        return None;
    }
    let af_len = pkt[4] as usize;
    if af_len < 7 {
        return None; // Need flags byte + 6 PCR bytes
    }
    let flags = pkt[5];
    if flags & 0x10 == 0 {
        return None; // PCR flag not set
    }
    // PCR bytes start at offset 6 in the TS packet
    let base = ((pkt[6] as u64) << 25)
        | ((pkt[7] as u64) << 17)
        | ((pkt[8] as u64) << 9)
        | ((pkt[9] as u64) << 1)
        | ((pkt[10] as u64) >> 7);
    let ext = (((pkt[10] & 0x01) as u64) << 8) | (pkt[11] as u64);
    Some(base * 300 + ext)
}

// ── PAT Parsing ──────────────────────────────────────────────────────────

/// Parse a single-packet PAT to extract `(program_number, pmt_pid)` pairs.
///
/// Skips the NIT reference (program_number 0). Only processes PATs that
/// start in this packet (PUSI set).
pub fn parse_pat_programs(pkt: &[u8]) -> Vec<(u16, u16)> {
    let mut programs = Vec::new();

    if !ts_pusi(pkt) {
        return programs;
    }

    // Find payload start offset
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return programs;
    }

    // pointer_field: number of bytes before section start
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;

    // PAT section header: table_id(1) + flags+length(2) + ts_id(2) +
    // version/cni(1) + section_number(1) + last_section(1) = 8 bytes
    if offset + 8 > TS_PACKET_SIZE {
        return programs;
    }
    let table_id = pkt[offset];
    if table_id != 0x00 {
        return programs; // Not a PAT
    }
    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let data_start = offset + 8;
    // section_length includes 4-byte CRC at end
    let data_end = (offset + 3 + section_length).min(TS_PACKET_SIZE).saturating_sub(4);

    let mut pos = data_start;
    while pos + 4 <= data_end {
        let program_number = ((pkt[pos] as u16) << 8) | pkt[pos + 1] as u16;
        let pid = ((pkt[pos + 2] as u16 & 0x1F) << 8) | pkt[pos + 3] as u16;
        if program_number != 0 {
            // program_number 0 is NIT PID, skip it
            programs.push((program_number, pid));
        }
        pos += 4;
    }

    programs
}

/// Parse a single-packet PAT to extract PMT PIDs only (drops program_number).
///
/// Thin wrapper around [`parse_pat_programs`] for callers that don't need
/// the program identity.
pub fn parse_pat_pmt_pids(pkt: &[u8]) -> Vec<u16> {
    parse_pat_programs(pkt).into_iter().map(|(_, pid)| pid).collect()
}

// ── MPEG-2 CRC-32 ───────────────────────────────────────────────────────

/// MPEG-2 CRC-32 lookup table (polynomial 0x04C11DB7, no bit reversal).
/// Used to verify PAT, PMT, and other PSI section integrity per ISO/IEC 13818-1.
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i << 24;
        let mut j = 0;
        while j < 8 {
            if crc & 0x80000000 != 0 {
                crc = (crc << 1) ^ 0x04C11DB7;
            } else {
                crc <<= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

/// Compute the MPEG-2 CRC-32 over a byte slice.
/// A valid PSI section (including its trailing CRC-32 bytes) produces 0x00000000.
pub fn mpeg2_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        let idx = ((crc >> 24) ^ byte as u32) as usize;
        crc = (crc << 8) ^ CRC32_TABLE[idx];
    }
    crc
}

/// Verify the CRC-32 of a PSI section starting at `section_start` in a TS packet.
/// `section_start` points to the table_id byte. Returns `true` if the CRC is valid.
/// Returns `false` if the section is truncated or the CRC does not match.
pub fn verify_psi_crc(pkt: &[u8], section_start: usize) -> bool {
    if section_start + 3 > TS_PACKET_SIZE {
        return false;
    }
    let section_length =
        (((pkt[section_start + 1] & 0x0F) as usize) << 8) | (pkt[section_start + 2] as usize);
    let section_end = section_start + 3 + section_length;
    if section_end > TS_PACKET_SIZE {
        return false; // Section spans multiple packets — cannot verify in single-packet mode
    }
    // CRC-32 covers table_id through the CRC itself; result should be 0
    mpeg2_crc32(&pkt[section_start..section_end]) == 0
}

// ── RTP Header Stripping ─────────────────────────────────────────────────

/// Strip the RTP header from a packet and return the TS payload slice.
///
/// For raw TS packets (`is_raw_ts` = true), returns the entire data.
/// For RTP-wrapped TS, parses the variable-length RTP header (CSRC + extension)
/// and returns the payload after it.
pub fn strip_rtp_header<'a>(packet: &'a RtpPacket) -> &'a [u8] {
    let data = &packet.data;

    if packet.is_raw_ts {
        return &data[..];
    }

    if data.len() < RTP_HEADER_MIN_SIZE {
        return &[];
    }
    let cc_count = (data[0] & 0x0F) as usize;
    let has_extension = (data[0] & 0x10) != 0;
    let mut rtp_header_len = RTP_HEADER_MIN_SIZE + cc_count * 4;

    if has_extension && data.len() > rtp_header_len + 4 {
        let ext_len =
            ((data[rtp_header_len + 2] as usize) << 8 | data[rtp_header_len + 3] as usize) * 4;
        rtp_header_len += 4 + ext_len;
    }

    if rtp_header_len >= data.len() {
        return &[];
    }
    &data[rtp_header_len..]
}

/// Get the TS payload start offset within an MPEG-TS packet.
/// Returns the offset past the 4-byte header and adaptation field (if present),
/// accounting for the pointer field when PUSI is set.
pub fn ts_payload_offset(pkt: &[u8]) -> usize {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic PAT TS packet carrying the given (program_number, pmt_pid)
    /// entries plus an optional NIT entry (program_number 0).
    fn build_pat_packet(programs: &[(u16, u16)], with_nit: bool) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        // TS header: sync(0x47), PUSI=1 PID=0x0000, no adaptation, PUSI continuity 0
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID high = 0
        pkt[2] = 0x00; // PID low = 0 (PAT_PID)
        pkt[3] = 0x10; // adaptation_field_control=01 (payload only), CC=0
        // pointer_field
        pkt[4] = 0x00;
        // PAT section starts at offset 5
        let entries_count = programs.len() + if with_nit { 1 } else { 0 };
        // section_length covers from after section_length field through CRC
        // Header after table_id+length: ts_id(2) + version/cni(1) + section_number(1) +
        // last_section(1) = 5 bytes; entries: 4 bytes each; CRC: 4 bytes
        let section_length = 5 + 4 * entries_count + 4;
        pkt[5] = 0x00; // table_id = PAT
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F); // section_syntax_indicator=1, '0', reserved, length high
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00; // transport_stream_id high
        pkt[9] = 0x01; // transport_stream_id low
        pkt[10] = 0xC1; // reserved + version=0 + current_next=1
        pkt[11] = 0x00; // section_number
        pkt[12] = 0x00; // last_section_number
        let mut pos = 13;
        if with_nit {
            pkt[pos] = 0x00;
            pkt[pos + 1] = 0x00; // program_number=0 (NIT)
            pkt[pos + 2] = 0xE0;
            pkt[pos + 3] = 0x10; // NIT PID=0x0010
            pos += 4;
        }
        for (program_number, pmt_pid) in programs {
            pkt[pos] = (program_number >> 8) as u8;
            pkt[pos + 1] = (program_number & 0xFF) as u8;
            pkt[pos + 2] = 0xE0 | (((pmt_pid >> 8) as u8) & 0x1F);
            pkt[pos + 3] = (pmt_pid & 0xFF) as u8;
            pos += 4;
        }
        // CRC bytes (4) — value isn't checked by parse_pat_programs, leave as default
        pkt
    }

    #[test]
    fn parse_pat_programs_extracts_single_program() {
        let pkt = build_pat_packet(&[(1, 0x1000)], true);
        let programs = parse_pat_programs(&pkt);
        assert_eq!(programs, vec![(1, 0x1000)]);
    }

    #[test]
    fn parse_pat_programs_extracts_multiple_programs_for_mpts() {
        let pkt = build_pat_packet(&[(1, 0x1000), (2, 0x1100), (3, 0x1200)], true);
        let programs = parse_pat_programs(&pkt);
        assert_eq!(programs, vec![(1, 0x1000), (2, 0x1100), (3, 0x1200)]);
    }

    #[test]
    fn parse_pat_programs_skips_nit_program_zero() {
        let pkt = build_pat_packet(&[(7, 0x1F00)], true);
        let programs = parse_pat_programs(&pkt);
        // NIT (program_number 0) must be filtered out
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0], (7, 0x1F00));
    }

    #[test]
    fn parse_pat_pmt_pids_returns_pids_only() {
        let pkt = build_pat_packet(&[(1, 0x1000), (2, 0x1100)], false);
        let pids = parse_pat_pmt_pids(&pkt);
        assert_eq!(pids, vec![0x1000, 0x1100]);
    }
}
