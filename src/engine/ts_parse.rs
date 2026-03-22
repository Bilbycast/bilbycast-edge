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

/// Parse a single-packet PAT to extract PMT PIDs.
///
/// Returns a list of PMT PIDs found in the PAT section. Skips the NIT
/// reference (program_number 0). Only processes PATs that start in this
/// packet (PUSI set).
pub fn parse_pat_pmt_pids(pkt: &[u8]) -> Vec<u16> {
    let mut pids = Vec::new();

    if !ts_pusi(pkt) {
        return pids;
    }

    // Find payload start offset
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return pids;
    }

    // pointer_field: number of bytes before section start
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;

    // PAT section header: table_id(1) + flags+length(2) + ts_id(2) +
    // version/cni(1) + section_number(1) + last_section(1) = 8 bytes
    if offset + 8 > TS_PACKET_SIZE {
        return pids;
    }
    let table_id = pkt[offset];
    if table_id != 0x00 {
        return pids; // Not a PAT
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
            pids.push(pid);
        }
        pos += 4;
    }

    pids
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
