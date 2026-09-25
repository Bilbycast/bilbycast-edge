// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
pub const PTS_MODULUS_90KHZ: u64 = 1u64 << 33;

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

/// Set the `discontinuity_indicator` (bit 7 of the AF flags byte) on a TS
/// packet that already carries an adaptation field with at least one flags
/// byte. Returns `true` when the bit was set, `false` when the packet has
/// no AF / a zero-length AF (so DI insertion would require restructuring
/// the packet — never done on the live data path because it would shift
/// payload bytes and corrupt ES/PES). Idempotent: calling on a packet
/// that already carries DI=1 leaves it set.
#[inline(always)]
pub fn set_discontinuity_indicator(pkt: &mut [u8]) -> bool {
    if !ts_has_adaptation(pkt) {
        return false;
    }
    let af_len = pkt[4] as usize;
    if af_len == 0 {
        return false;
    }
    pkt[5] |= 0x80;
    true
}

/// Scan a raw TS buffer (one or more aligned 188-byte packets) and return
/// the PCR carried by the **first** PCR-bearing packet, in 27 MHz ticks.
///
/// Used by output send paths to feed the per-output PCR trust sampler
/// (`OutputStatsAccumulator::record_pcr_egress`). Returns `None` when the
/// buffer carries no PCR — e.g. audio-only frames, or PAT/PMT-only
/// bundles. Reports only the first to keep the hot path O(frame_size) at
/// one branch per packet.
///
/// The buffer is assumed to start on a TS packet boundary (`0x47` sync).
/// Callers that hand us an RTP-wrapped buffer must strip the RTP header
/// first; cheap because every caller already distinguishes raw-TS vs
/// RTP-wrapped paths via `RtpPacket.is_raw_ts`.
pub fn first_pcr_in_ts_buffer(data: &[u8]) -> Option<u64> {
    first_pcr_in_ts_buffer_pid(data, None).map(|(pcr, _)| pcr)
}

/// Scan a raw TS buffer for the first PCR-bearing packet whose PID matches
/// `filter_pid` (or any PID when `filter_pid` is `None`). Returns
/// `Some((pcr_27mhz, pid))` on the first match.
///
/// **Why the PID filter matters.** In an MPTS, every program has its own
/// PCR PID and its own 27 MHz clock — there's no shared timebase across
/// programs. A naive PCR scan would report PCRs from whichever program
/// happens to carry one in the current datagram, producing a sequence of
/// values from independent clocks. Anything that derives wire-pacing
/// targets from this sequence (e.g. `engine::wire_emit::TargetState`) then
/// sees apparent "discontinuities" or random advances at every cross-
/// program transition, even when each individual program is internally
/// continuous. The fix is to lock onto a single PID's PCR cadence — the
/// caller passes the PID it observed first as `filter_pid` on subsequent
/// calls. Returns `None` if no packet in the buffer matches the filter
/// (the caller treats this as "between PCRs"), which is exactly the
/// right behaviour for between-program datagrams in an MPTS multiplex.
pub fn first_pcr_in_ts_buffer_pid(data: &[u8], filter_pid: Option<u16>) -> Option<(u64, u16)> {
    let mut i = 0;
    while i + TS_PACKET_SIZE <= data.len() {
        let pkt = &data[i..i + TS_PACKET_SIZE];
        if pkt[0] == TS_SYNC_BYTE
            && let Some(pcr) = extract_pcr(pkt) {
                let pid = ts_pid(pkt);
                if filter_pid.is_none_or(|f| f == pid) {
                    return Some((pcr, pid));
                }
            }
        i += TS_PACKET_SIZE;
    }
    None
}

/// Extract a 33-bit PTS from a PUSI-marked TS packet that begins a PES
/// payload. Returns `None` when the packet is not a PES, the
/// `PTS_DTS_flags` bits don't indicate PTS present, or the packet is too
/// short to carry the 5-byte PTS field.
///
/// Caller is responsible for ensuring `pkt.len() == 188` and PUSI=1 — the
/// function does the minimal work to return a clean `None` for malformed
/// packets so it can sit on the hot path.
///
/// Used by:
/// - [`crate::engine::tr101290`] — TS-quality PTS continuity sampling.
/// - [`crate::engine::pes_splice`] — PES Switch Phase 4 audio-aligned splice.
pub fn extract_pes_pts(pkt: &[u8]) -> Option<u64> {
    if pkt.len() < TS_PACKET_SIZE {
        return None;
    }
    // adaptation_field_control: 0b01 = payload only, 0b10 = af only,
    // 0b11 = af + payload, 0b00 = reserved.
    let afc = (pkt[3] >> 4) & 0x03;
    let payload_offset: usize = match afc {
        0b01 => 4,
        0b11 => {
            let af_len = pkt.get(4).copied()? as usize;
            5 + af_len
        }
        _ => return None,
    };
    if pkt.len() < payload_offset + 14 {
        return None;
    }
    let payload = &pkt[payload_offset..];
    // PES start code = 0x000001
    if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return None;
    }
    let pts_dts_flags = (payload[7] >> 6) & 0x03;
    // 0b10 = PTS only, 0b11 = PTS + DTS. Both have PTS at bytes 9-13.
    if pts_dts_flags != 0b10 && pts_dts_flags != 0b11 {
        return None;
    }
    let p = &payload[9..14];
    let pts: u64 = (((p[0] >> 1) & 0x07) as u64) << 30
        | (p[1] as u64) << 22
        | (((p[2] >> 1) & 0x7F) as u64) << 15
        | (p[3] as u64) << 7
        | ((p[4] >> 1) as u64);
    Some(pts)
}

/// Extract a 33-bit DTS from a PUSI-marked TS packet that begins a PES
/// payload. Returns `None` when the PES doesn't carry DTS (most audio
/// PESes only set PTS, only video with reordered B-frames sets both).
///
/// Layout per H.222.0 §2.4.3.6: when `PTS_DTS_flags == 0b11`, PTS is at
/// payload bytes 9-13 and DTS is at 14-18. Same 5-byte 33-bit field
/// shape as PTS.
///
/// Used by [`crate::engine::pcr_ingress_sampler::watch_source_discontinuities`]
/// to alarm on source-side DTS backward jumps (file-loop boundaries on
/// upstream senders).
pub fn extract_pes_dts(pkt: &[u8]) -> Option<u64> {
    if pkt.len() < TS_PACKET_SIZE {
        return None;
    }
    let afc = (pkt[3] >> 4) & 0x03;
    let payload_offset: usize = match afc {
        0b01 => 4,
        0b11 => {
            let af_len = pkt.get(4).copied()? as usize;
            5 + af_len
        }
        _ => return None,
    };
    if pkt.len() < payload_offset + 19 {
        return None;
    }
    let payload = &pkt[payload_offset..];
    if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return None;
    }
    let pts_dts_flags = (payload[7] >> 6) & 0x03;
    // DTS only present when both PTS and DTS are set (0b11).
    if pts_dts_flags != 0b11 {
        return None;
    }
    let d = &payload[14..19];
    let dts: u64 = (((d[0] >> 1) & 0x07) as u64) << 30
        | (d[1] as u64) << 22
        | (((d[2] >> 1) & 0x7F) as u64) << 15
        | (d[3] as u64) << 7
        | ((d[4] >> 1) as u64);
    Some(dts)
}

/// Locate the PES payload start (= ES bytes, e.g. ADTS frame, NAL unit)
/// inside a PUSI-marked TS packet. Returns `None` when the packet is not
/// a PES start, doesn't carry payload, or the declared PES header runs
/// off the packet boundary.
///
/// The returned offset is the byte index inside `pkt` where the ES data
/// (i.e. what would follow the PES header in a fresh PES) begins.
///
/// Used by [`crate::engine::pes_splice`] to peek at the leading ADTS
/// sync header for the codec-param sentinel.
pub fn pes_payload_offset(pkt: &[u8]) -> Option<usize> {
    if pkt.len() < TS_PACKET_SIZE {
        return None;
    }
    let afc = (pkt[3] >> 4) & 0x03;
    let payload_offset: usize = match afc {
        0b01 => 4,
        0b11 => {
            let af_len = pkt.get(4).copied()? as usize;
            5 + af_len
        }
        _ => return None,
    };
    // PES header (post-start-code) is at least 9 bytes: 4 (start code +
    // stream_id) + 2 (PES_packet_length) + 1 (flags1) + 1 (flags2) + 1
    // (PES_header_data_length). Plus PES_header_data_length bytes of
    // optional fields (PTS / DTS / ESCR / etc.).
    if payload_offset + 9 > TS_PACKET_SIZE {
        return None;
    }
    let payload = &pkt[payload_offset..];
    if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return None;
    }
    let pes_header_data_length = payload[8] as usize;
    let es_start = payload_offset + 9 + pes_header_data_length;
    if es_start >= TS_PACKET_SIZE {
        return None;
    }
    Some(es_start)
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

// ── PTS Arithmetic ──────────────────────────────────────────────────────

/// Signed PTS delta in milliseconds: `(pts_a − pts_b)` in modular 33-bit
/// PTS space. Positive = `a` is later than `b`, negative = `a` is earlier.
#[inline]
pub fn pts_delta_ms(pts_a: u64, pts_b: u64) -> i64 {
    let a = pts_a & (PTS_MODULUS_90KHZ - 1);
    let b = pts_b & (PTS_MODULUS_90KHZ - 1);
    let raw = a.wrapping_sub(b) & (PTS_MODULUS_90KHZ - 1);
    let signed = if raw > PTS_MODULUS_90KHZ / 2 {
        raw as i64 - PTS_MODULUS_90KHZ as i64
    } else {
        raw as i64
    };
    signed / 90
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

/// Parse a complete PAT **section** (starting at `table_id`, as produced
/// by [`SectionAssembler::feed`]) into `(program_number, pmt_pid)` pairs.
/// Unlike [`parse_pat_programs`] this is not limited to what fits in one
/// 188-byte TS packet — big cable / DTT muxes carry more programs than a
/// single packet holds.
pub fn parse_pat_section_programs(section: &[u8]) -> Vec<(u16, u16)> {
    let mut programs = Vec::new();
    if section.len() < 12 || section[0] != 0x00 {
        return programs;
    }
    let section_length = (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
    if 3 + section_length > section.len() || section_length < 9 {
        return programs;
    }
    let body_end = 3 + section_length - 4; // strip CRC
    let mut pos = 8;
    while pos + 4 <= body_end {
        let program_number = ((section[pos] as u16) << 8) | section[pos + 1] as u16;
        let pid = ((section[pos + 2] as u16 & 0x1F) << 8) | section[pos + 3] as u16;
        if program_number != 0 {
            programs.push((program_number, pid));
        }
        pos += 4;
    }
    programs
}

// ── PSI section location inside one packet ──────────────────────────────
//
// ISO/IEC 13818-1 §2.4.4 lets a payload unit carry several sections back
// to back — the pointer_field only locates the FIRST one — and lets a PMT
// PID carry tables other than the PMT (user-private table_ids 0x40-0xFE).
// ATSC / DigiCipher muxes do exactly that: VH1's PMT-PID packets carry a
// 24-byte short-form 0xC0 section at the pointer target and the PMT right
// behind it. Every parser that assumed "the pointer target IS the PMT"
// silently found nothing on such a stream. The walker below is the one
// shared way to find a section inside a packet.

/// Largest `section_length` a PAT / CAT / PMT may declare (ISO/IEC 13818-1
/// §2.4.4.3 / .5 / .8: the whole section is at most 1024 bytes).
pub const PSI_MAX_SECTION_LENGTH: usize = 1021;

/// Largest `section_length` any section may declare (private sections are
/// capped at 4096 bytes in total).
pub const PRIVATE_MAX_SECTION_LENGTH: usize = 4093;

/// Maximum `section_length` for a table, or `None` when the table_id is a
/// forbidden / stuffing value.
#[inline]
pub fn max_section_length(table_id: u8) -> Option<usize> {
    match table_id {
        0xFF => None,
        0x00..=0x02 => Some(PSI_MAX_SECTION_LENGTH),
        _ => Some(PRIVATE_MAX_SECTION_LENGTH),
    }
}

/// One PSI section found inside a buffer (a TS packet or a payload).
/// Offsets are relative to the buffer the walker was run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PsiSectionRef {
    /// Offset of the `table_id` byte.
    pub start: usize,
    pub table_id: u8,
    /// Declared `section_length` (bytes after the length field, CRC
    /// included when `ssi`).
    pub section_length: usize,
    /// `section_syntax_indicator`. A short-form private section (`false`)
    /// has no version_number and no CRC_32 and must never be stamped.
    pub ssi: bool,
    /// `true` when the whole section lies inside the buffer.
    pub complete: bool,
}

impl PsiSectionRef {
    /// Offset one past the section's last byte (may lie beyond the buffer
    /// when `!complete`).
    #[cfg(test)]
    #[inline]
    pub fn end(&self) -> usize {
        self.start + 3 + self.section_length
    }
}

/// Iterator over the PSI sections that START in one buffer, walking from
/// the pointer_field target in steps of `3 + section_length`. Stops at
/// 0xFF stuffing, at the first section that runs off the buffer (which it
/// still yields, with `complete == false`), and at any header that cannot
/// describe a real section (a PAT/CAT/PMT `section_length` of 0 or above
/// 1021, a long-form section too short for its own header + CRC, or a
/// header split across the buffer end). Every step advances by at least
/// three bytes, so a malformed chain always terminates.
pub struct PsiSectionIter<'a> {
    buf: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> Iterator for PsiSectionIter<'a> {
    type Item = PsiSectionRef;

    fn next(&mut self) -> Option<PsiSectionRef> {
        if self.done {
            return None;
        }
        let buf = self.buf;
        let pos = self.pos;
        if pos + 3 > buf.len() {
            self.done = true;
            return None;
        }
        let table_id = buf[pos];
        let Some(max_len) = max_section_length(table_id) else {
            // 0xFF: stuffing — every remaining byte is stuffing.
            self.done = true;
            return None;
        };
        let ssi = buf[pos + 1] & 0x80 != 0;
        let section_length = (((buf[pos + 1] & 0x0F) as usize) << 8) | buf[pos + 2] as usize;
        let too_short = if table_id <= 0x02 {
            // PAT / CAT / PMT are always long-form: 5 bytes of extended
            // header + the 4-byte CRC at minimum.
            section_length < 9
        } else {
            ssi && section_length < 9
        };
        if section_length > max_len || too_short {
            self.done = true;
            return None;
        }
        let end = pos + 3 + section_length;
        let complete = end <= buf.len();
        if complete {
            self.pos = end;
        } else {
            self.done = true;
        }
        Some(PsiSectionRef { start: pos, table_id, section_length, ssi, complete })
    }
}

/// Walk the sections of a PSI payload that begins with a pointer_field
/// (the payload of a PUSI=1 packet, after the adaptation field). Offsets
/// in the yielded refs are relative to `payload`.
pub fn psi_sections_in_payload(payload: &[u8]) -> PsiSectionIter<'_> {
    match payload.first() {
        Some(&pointer) => PsiSectionIter { buf: payload, pos: 1 + pointer as usize, done: false },
        None => PsiSectionIter { buf: payload, pos: 0, done: true },
    }
}

/// Offset of the payload inside a 188-byte TS packet (after the header and
/// any adaptation field), or `None` when the packet carries no payload or
/// the adaptation field swallows it.
#[inline]
pub fn ts_payload_start(pkt: &[u8]) -> Option<usize> {
    if pkt.len() < TS_PACKET_SIZE || !ts_has_payload(pkt) {
        return None;
    }
    let off = ts_payload_offset(pkt);
    if off >= TS_PACKET_SIZE { None } else { Some(off) }
}

/// Walk the PSI sections that start in one PUSI=1 TS packet. Offsets are
/// relative to `pkt`. Empty for a PUSI=0 packet or one without payload.
pub fn sections_in_packet(pkt: &[u8]) -> PsiSectionIter<'_> {
    let empty = PsiSectionIter { buf: pkt, pos: 0, done: true };
    if pkt.len() != TS_PACKET_SIZE || !ts_pusi(pkt) {
        return empty;
    }
    let Some(off) = ts_payload_start(pkt) else {
        return empty;
    };
    PsiSectionIter {
        buf: &pkt[..TS_PACKET_SIZE],
        pos: off + 1 + pkt[off] as usize,
        done: false,
    }
}

/// True when `section` (bytes from `table_id`) is a long-form section whose
/// `table_id_extension` equals `program_number` — for a PMT that field IS
/// the program_number. Needs the five header bytes to be present.
#[inline]
fn section_program_matches(buf: &[u8], s: &PsiSectionRef, program_number: Option<u16>) -> bool {
    match program_number {
        None => true,
        Some(want) => {
            s.ssi
                && s.start + 5 <= buf.len()
                && ((buf[s.start + 3] as u16) << 8 | buf[s.start + 4] as u16) == want
        }
    }
}

/// First section in a PUSI=1 packet with `table_id` (and, when
/// `program_number` is `Some`, that program_number / table_id_extension).
/// The returned section may be incomplete (`complete == false`) when it
/// continues in the next packet — single-packet parsers keep their
/// historical clipped-to-the-packet behaviour, rewriters must refuse it.
///
/// For a packet whose first section IS the wanted table the answer is the
/// pointer target, i.e. exactly what every parser computed before.
pub fn find_section_in_packet(
    pkt: &[u8],
    table_id: u8,
    program_number: Option<u16>,
) -> Option<PsiSectionRef> {
    sections_in_packet(pkt)
        .find(|s| s.table_id == table_id && section_program_matches(pkt, s, program_number))
}

/// [`find_section_in_packet`] over a pointer-field-prefixed payload.
pub fn find_section_in_payload(
    payload: &[u8],
    table_id: u8,
    program_number: Option<u16>,
) -> Option<PsiSectionRef> {
    psi_sections_in_payload(payload)
        .find(|s| s.table_id == table_id && section_program_matches(payload, s, program_number))
}

/// Offset of the first PMT section (`table_id` 0x02) in a PUSI=1 packet,
/// optionally restricted to one program. Shorthand for the preamble every
/// single-packet PMT parser used to open-code.
#[inline]
pub fn pmt_section_offset(pkt: &[u8], program_number: Option<u16>) -> Option<usize> {
    find_section_in_packet(pkt, 0x02, program_number).map(|s| s.start)
}

/// Offset of `program`'s PMT in a PUSI=1 packet. An exact program_number
/// match wins. When the PAT maps no other program to this PID
/// (`pid_shared == false`), the first PMT section is accepted too — what
/// every parser did before — so a mux whose PMT program_number disagrees
/// with its PAT keeps working; on a shared PID only the exact match counts.
pub fn pmt_section_offset_for(pkt: &[u8], program: u16, pid_shared: bool) -> Option<usize> {
    pmt_section_offset(pkt, Some(program))
        .or_else(|| if pid_shared { None } else { pmt_section_offset(pkt, None) })
}

// ── PSI section reassembly ──────────────────────────────────────────────

/// PSI sections are capped at 1024 bytes for PAT / PMT and 4096 bytes for
/// private sections (ISO 13818-1 §2.4.4); anything past this is
/// corruption — drop and resync.
pub const PSI_SECTION_CAP: usize = 3 + PRIVATE_MAX_SECTION_LENGTH;

/// Reassembles every PSI section carried on one PID, across TS packets.
///
/// Packet-level PAT / PMT parsers silently drop any section longer than
/// ~180 bytes — real broadcast MPTS PMTs (10+ ES entries with AC-3 /
/// teletext / subtitle descriptors) routinely exceed that, leaving a
/// consumer with zero audio PIDs on those muxes. One assembler instance
/// per PSI PID.
///
/// **Multi-section.** A payload unit may carry several sections back to
/// back, and a PMT PID may carry other tables too (ATSC / DigiCipher 0xC0
/// sections ahead of the PMT). [`Self::feed`] therefore yields EVERY
/// section that completes in the packet — the tail before the
/// pointer_field target first (it finishes the section in flight), then
/// each section after it — and callers filter on `table_id`. The old
/// single-result API returned only the section at the pointer target,
/// which is how a PMT sitting second in the packet went unseen.
///
/// [`Self::push_packet`] is the CC-checked variant: a continuity gap aborts
/// the section in flight rather than splicing unrelated bytes into it, and
/// a duplicate continuation packet (same CC, byte-identical) is ignored.
pub struct SectionAssembler {
    /// Bytes of the section in flight (from its `table_id`).
    buf: Vec<u8>,
    assembling: bool,
    /// Sections completed by the last feed, concatenated; `done_ends[i]` is
    /// the end offset of section `i` inside `done`, `done_spanned[i]`
    /// whether it started in an earlier packet.
    done: Vec<u8>,
    done_ends: Vec<usize>,
    done_spanned: Vec<bool>,
    /// Last payload CC seen by [`Self::push_packet`], and a digest of that
    /// packet so a true duplicate (ISO 13818-1 allows one, byte-identical)
    /// can be told from a CC error.
    last_cc: Option<u8>,
    last_digest: u32,
}

impl Default for SectionAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Sections completed by one [`SectionAssembler::feed`] call, in stream
/// order. Each item starts at `table_id` and is exactly `3 + section_length`
/// bytes long.
pub struct CompletedSections<'a> {
    done: &'a [u8],
    ends: &'a [usize],
    spanned: &'a [bool],
    i: usize,
}

impl<'a> CompletedSections<'a> {
    /// Pair each section with whether it spanned more than one packet
    /// (it started in an earlier packet than the one that completed it).
    pub fn with_span(self) -> impl Iterator<Item = (&'a [u8], bool)> {
        let spanned = self.spanned;
        let first = self.i;
        self.enumerate().map(move |(k, s)| (s, spanned[first + k]))
    }
}

impl<'a> Iterator for CompletedSections<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        let end = *self.ends.get(self.i)?;
        let start = if self.i == 0 { 0 } else { self.ends[self.i - 1] };
        self.i += 1;
        Some(&self.done[start..end])
    }
}

impl SectionAssembler {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(256),
            assembling: false,
            done: Vec::new(),
            done_ends: Vec::new(),
            done_spanned: Vec::new(),
            last_cc: None,
            last_digest: 0,
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.assembling = false;
        self.last_cc = None;
    }

    /// True while a section has started but not completed — the next
    /// continuation packet on this PID carries its bytes.
    pub fn in_flight(&self) -> bool {
        self.assembling
    }

    fn completed(&self) -> CompletedSections<'_> {
        CompletedSections {
            done: &self.done,
            ends: &self.done_ends,
            spanned: &self.done_spanned,
            i: 0,
        }
    }

    /// Needed total length of the in-flight section once its 3-byte header
    /// is present, or `Err(())` when the header is invalid.
    fn needed(&self) -> Result<Option<usize>, ()> {
        if self.buf.len() < 3 {
            return Ok(None);
        }
        let table_id = self.buf[0];
        let section_length = (((self.buf[1] as usize) & 0x0F) << 8) | self.buf[2] as usize;
        match max_section_length(table_id) {
            Some(max) if section_length <= max => Ok(Some(3 + section_length)),
            _ => Err(()),
        }
    }

    /// Move the in-flight section to `done` when it is complete. Bytes past
    /// its end are ignored (they can only be stuffing or garbage).
    fn try_finish(&mut self) {
        match self.needed() {
            Ok(Some(total)) if self.buf.len() >= total => {
                self.done.extend_from_slice(&self.buf[..total]);
                self.done_ends.push(self.done.len());
                self.done_spanned.push(true);
                self.buf.clear();
                self.assembling = false;
            }
            Ok(_) => {}
            Err(()) => {
                self.buf.clear();
                self.assembling = false;
            }
        }
    }

    /// Walk the sections that start at `from` inside `payload`.
    fn start_sections(&mut self, payload: &[u8], mut pos: usize) {
        while pos < payload.len() {
            if payload[pos] == 0xFF {
                return; // stuffing to the end of the packet
            }
            self.buf.clear();
            let remaining = payload.len() - pos;
            if remaining < 3 {
                // Header split across packets.
                self.buf.extend_from_slice(&payload[pos..]);
                self.assembling = true;
                return;
            }
            let table_id = payload[pos];
            let section_length =
                (((payload[pos + 1] as usize) & 0x0F) << 8) | payload[pos + 2] as usize;
            let Some(max) = max_section_length(table_id) else {
                return;
            };
            if section_length > max {
                return; // corruption — drop the rest of this packet
            }
            let total = 3 + section_length;
            if total <= remaining {
                self.done.extend_from_slice(&payload[pos..pos + total]);
                self.done_ends.push(self.done.len());
                self.done_spanned.push(false);
                pos += total;
            } else {
                self.buf.extend_from_slice(&payload[pos..]);
                self.assembling = true;
                return;
            }
        }
    }

    /// Feed one TS packet's payload (after the adaptation field; on a
    /// `pusi` packet it still starts with the pointer_field). Returns every
    /// section this packet completed.
    pub fn feed(&mut self, pusi: bool, payload: &[u8]) -> CompletedSections<'_> {
        self.feed_inner(pusi, payload, true);
        self.completed()
    }

    /// `tail_continues`: whether the bytes before the pointer target may be
    /// appended to the section in flight (false after a CC gap).
    fn feed_inner(&mut self, pusi: bool, payload: &[u8], tail_continues: bool) {
        self.done.clear();
        self.done_ends.clear();
        self.done_spanned.clear();
        if pusi {
            let Some(&pointer) = payload.first() else {
                self.buf.clear();
                self.assembling = false;
                return;
            };
            let sec_start = 1 + pointer as usize;
            if sec_start > payload.len() {
                self.buf.clear();
                self.assembling = false;
                return;
            }
            // Bytes before the pointer target finish the section in flight.
            if self.assembling && tail_continues && sec_start > 1 {
                self.buf.extend_from_slice(&payload[1..sec_start]);
                self.try_finish();
            }
            // Whatever was in flight and did not complete is truncated.
            self.buf.clear();
            self.assembling = false;
            self.start_sections(payload, sec_start);
        } else {
            if !self.assembling {
                return;
            }
            if self.buf.len() + payload.len() > PSI_SECTION_CAP + TS_PACKET_SIZE {
                self.buf.clear();
                self.assembling = false;
                return;
            }
            self.buf.extend_from_slice(payload);
            self.try_finish();
        }
    }

    /// CC-checked variant of [`Self::feed`] taking a whole 188-byte packet
    /// on this PID. A packet without payload completes nothing and leaves
    /// the CC untouched; a duplicate continuation packet (same CC as the
    /// previous payload packet and byte-identical to it) is ignored; any
    /// other CC discontinuity aborts the section in flight so it cannot be
    /// completed from unrelated bytes.
    pub fn push_packet(&mut self, pkt: &[u8]) -> CompletedSections<'_> {
        self.done.clear();
        self.done_ends.clear();
        self.done_spanned.clear();
        let Some(off) = ts_payload_start(pkt) else {
            return self.completed();
        };
        let cc = ts_cc(pkt);
        let pusi = ts_pusi(pkt);
        let digest = mpeg2_crc32(&pkt[..TS_PACKET_SIZE.min(pkt.len())]);
        let contiguous = match self.last_cc {
            // A duplicate continuation would append its bytes twice. A
            // duplicate PUSI packet simply restarts the same unit, which is
            // harmless — and some muxers never advance the CC on PSI, so
            // treating every repeat as a duplicate would lose them all.
            Some(prev) if prev == cc && digest == self.last_digest && !pusi => {
                return self.completed();
            }
            Some(prev) => (prev + 1) & 0x0F == cc,
            None => false,
        };
        self.last_cc = Some(cc);
        self.last_digest = digest;
        if !contiguous && !pusi {
            // Lost a middle packet — the in-flight section is garbage.
            self.buf.clear();
            self.assembling = false;
            return self.completed();
        }
        self.feed_inner(pusi, &pkt[off..TS_PACKET_SIZE], contiguous);
        self.completed()
    }
}

// ── PMT ES-info descriptor classification ───────────────────────────────
//
// `stream_type = 0x06` (ISO/IEC 13818-1 "PES private data") is the DVB
// carriage convention for AC-3 / E-AC-3 / AAC-LATM / DTS audio AND for
// teletext / VBI / DVB subtitling — the stream_type byte alone cannot
// distinguish a 5.1 AC-3 service from a teletext page. The ES-info
// descriptor loop is the discriminator (ETSI EN 300 468 §6). These
// helpers are the single shared implementation used by every module
// that must classify a private ES: `ts_pts_rewriter` (muxer-mode PES
// re-anchoring + lipsync routing), `ts_pid_overrides_rewriter`
// (singular `audio_pid` binding), `stats::av_sync` (A/V drift metric),
// and `ts_audio_replace` (transcode source identification).

/// Audio codec family resolved from a private-ES descriptor loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateEsAudioKind {
    /// DVB AC-3 descriptor (0x6A) or registration "AC-3".
    Ac3,
    /// DVB Enhanced-AC-3 descriptor (0x7A) or registration "EAC3".
    Eac3,
    /// DVB AAC descriptor (0x7C) — LATM/LOAS carriage.
    AacLatm,
    /// DVB DTS descriptor (0x7B) or registration "DTS1"/"DTS2"/"DTS3".
    Dts,
    /// Registration "Opus" (Opus-in-TS convention).
    Opus,
    /// Registration "BSSD" — SMPTE 302M LPCM audio in MPEG-TS.
    Smpte302m,
    /// DVB extension descriptor (0x7F) with AC-4 extension tag (0x15).
    Ac4,
}

/// Walk a PMT ES-info descriptor loop and resolve the audio codec family
/// a private ES (typically `stream_type = 0x06`) carries, or `None` when
/// no recognised audio descriptor is present.
///
/// Recognised (first match in loop order wins, mirroring receiver
/// behaviour):
/// - DVB AC-3 descriptor (tag 0x6A, ETSI EN 300 468 §6.2.1) → [`PrivateEsAudioKind::Ac3`]
/// - DVB Enhanced-AC-3 descriptor (tag 0x7A) → [`PrivateEsAudioKind::Eac3`]
/// - DVB DTS descriptor (tag 0x7B) → [`PrivateEsAudioKind::Dts`]
/// - DVB AAC descriptor (tag 0x7C) → [`PrivateEsAudioKind::AacLatm`]
/// - `registration_descriptor` (tag 0x05) with `format_identifier`
///   "AC-3" / "EAC3" / "DTS1" / "DTS2" / "DTS3" / "Opus" / "BSSD"
/// - DVB extension descriptor (tag 0x7F) with extension tag 0x15 (AC-4)
/// - ATSC AC-4 descriptor (tag 0xAC, A/342-2 § 6.2)
pub fn descriptor_audio_kind(descriptors: &[u8]) -> Option<PrivateEsAudioKind> {
    let mut pos = 0;
    while pos + 2 <= descriptors.len() {
        let tag = descriptors[pos];
        let len = descriptors[pos + 1] as usize;
        if pos + 2 + len > descriptors.len() {
            return None;
        }
        match tag {
            0x6A => return Some(PrivateEsAudioKind::Ac3),
            0x7A => return Some(PrivateEsAudioKind::Eac3),
            0x7B => return Some(PrivateEsAudioKind::Dts),
            0x7C => return Some(PrivateEsAudioKind::AacLatm),
            0x05 if len >= 4 => {
                let fmt = &descriptors[pos + 2..pos + 6];
                match fmt {
                    b"AC-3" => return Some(PrivateEsAudioKind::Ac3),
                    b"EAC3" => return Some(PrivateEsAudioKind::Eac3),
                    b"DTS1" | b"DTS2" | b"DTS3" => return Some(PrivateEsAudioKind::Dts),
                    b"Opus" => return Some(PrivateEsAudioKind::Opus),
                    b"BSSD" => return Some(PrivateEsAudioKind::Smpte302m),
                    b"AC-4" => return Some(PrivateEsAudioKind::Ac4),
                    _ => {}
                }
            }
            0x7F if len >= 1 => {
                // DVB extension descriptor: first body byte is the
                // descriptor_tag_extension. 0x15 = AC-4 (EN 300 468).
                if descriptors[pos + 2] == 0x15 {
                    return Some(PrivateEsAudioKind::Ac4);
                }
            }
            // ATSC AC-4 descriptor (A/342-2 § 6.2) — tag 0xAC directly,
            // the non-DVB signalling flavour.
            0xAC => return Some(PrivateEsAudioKind::Ac4),
            _ => {}
        }
        pos += 2 + len;
    }
    None
}

/// True when a PMT ES-info descriptor loop marks the ES as a text /
/// data service that is definitively NOT audio: DVB teletext (0x56),
/// VBI data (0x45), VBI teletext (0x46), or DVB subtitling (0x59).
///
/// Used to keep heuristic "first 0x06 PID is probably the audio"
/// fallbacks from latching a teletext or subtitle PID when the real
/// audio carries no recognisable descriptor.
pub fn descriptors_indicate_text_service(descriptors: &[u8]) -> bool {
    let mut pos = 0;
    while pos + 2 <= descriptors.len() {
        let tag = descriptors[pos];
        let len = descriptors[pos + 1] as usize;
        if pos + 2 + len > descriptors.len() {
            return false;
        }
        if matches!(tag, 0x45 | 0x46 | 0x56 | 0x59) {
            return true;
        }
        pos += 2 + len;
    }
    false
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

/// Byte offset of a PSI section's trailing CRC-32 inside a single TS packet,
/// or `None` when the declared `section_length` cannot describe a real section
/// that fits.
///
/// `section_length` counts the bytes *after* the length field and includes the
/// 4-byte CRC, so the section ends at `section_start + 3 + section_length` and
/// the CRC begins four bytes before that.
///
/// **Why this is a function.** Five PSI rewriters open-coded
/// `section_start + 3 + section_length - 4` with an upper-bound-only guard
/// (`crc_offset + 4 <= TS_PACKET_SIZE`), and four of them were wrong in the
/// same way: `section_length == 0` puts the CRC offset *before* the section
/// start, and the following `&pkt[section_start..crc_offset]` is a reversed
/// range, which panics. A remote attacker needs one 188-byte packet whose
/// payload begins `00 02 B0 00` (pointer_field 0, table_id 0x02,
/// section_length 0) to kill a transcode or output task — no credentials, and
/// the parse happens long before any authentication. See the lower-bound
/// check below, which is the half that was missing.
pub fn psi_crc_offset(section_start: usize, section_length: usize) -> Option<usize> {
    let section_end = section_start.checked_add(3)?.checked_add(section_length)?;
    let crc_offset = section_end.checked_sub(4)?;
    // The CRC is computed over `pkt[section_start..crc_offset]`, which must
    // cover at least the 3-byte table header. Equivalently: a section must be
    // long enough to hold its own CRC (`section_length >= 4`).
    if crc_offset < section_start + 3 {
        return None;
    }
    if crc_offset.checked_add(4)? > TS_PACKET_SIZE {
        return None;
    }
    Some(crc_offset)
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

/// Overwrite the `version_number` field in a PSI section (PAT or PMT)
/// carried in a single 188-byte TS packet with PUSI=1, then recompute
/// the CRC32. Forces receivers that cache tables by version to re-parse
/// the section — essential when:
///   - switching between inputs that use the same version number but
///     have different content (`TsContinuityFixer::on_switch` path);
///   - the egress transcoder rewrites a PMT's stream_type so the
///     codec the PMT advertises no longer matches what receivers have
///     cached against the source's version.
///
/// Layout (for PUSI=1, pointer_field=0):
///   byte 4:  pointer_field (0x00)
///   byte 5:  table_id
///   byte 6-7: section_syntax_indicator + section_length
///   byte 8-9: transport_stream_id (PAT) or program_number (PMT)
///   byte 10: reserved(2) + version_number(5) + current_next_indicator(1)
///   ...
///   last 4 bytes of section: CRC32
///
/// `version` is masked to 5 bits and written in place; `current_next`
/// and the two reserved bits are preserved. Silently no-ops if the
/// packet has PUSI=0 or the section is malformed.
///
/// This stamps **the section at the pointer target** — correct for the PAT
/// (PID 0 carries nothing else). A PMT PID may carry other tables ahead of
/// the PMT, so PMT callers locate the section with
/// [`find_section_in_packet`] and use [`set_psi_version_at`] (or
/// `ts_pmt_edit::restamp_pmt_unit` for a whole unit). The adaptation field
/// is honoured, and a short-form (SSI=0) section is never
/// touched: it has no version field and no CRC, so "stamping" one would
/// corrupt a data byte and its last four bytes.
pub fn set_psi_version(pkt: &mut [u8], version: u8) {
    if pkt.len() != TS_PACKET_SIZE || !ts_pusi(pkt) {
        return;
    }
    let Some(first) = sections_in_packet(pkt).next() else {
        return;
    };
    set_psi_version_at(pkt, first.start, version);
}

/// Stamp `version` into the long-form section starting at `section_start`
/// inside `buf` and recompute its CRC_32. Returns `false` (and leaves the
/// buffer untouched) when the section is short-form (SSI=0), shorter than
/// a long-form header + CRC, or does not end inside `buf` — a CRC that
/// lives in a later packet cannot be recomputed in place.
pub fn set_psi_version_at(buf: &mut [u8], section_start: usize, version: u8) -> bool {
    if section_start + 3 > buf.len() {
        return false;
    }
    if buf[section_start + 1] & 0x80 == 0 {
        return false; // SSI=0: no version_number, no CRC
    }
    let section_length =
        (((buf[section_start + 1] & 0x0F) as usize) << 8) | (buf[section_start + 2] as usize);
    let section_end = section_start + 3 + section_length;
    if section_end > buf.len() || section_length < 9 {
        return false;
    }
    let v = version & 0x1F;
    buf[section_start + 5] = (buf[section_start + 5] & 0xC1) | (v << 1);
    let crc_offset = section_end - 4;
    let crc = mpeg2_crc32(&buf[section_start..crc_offset]);
    buf[crc_offset..section_end].copy_from_slice(&crc.to_be_bytes());
    true
}

// ── RTP Header Stripping ─────────────────────────────────────────────────

/// Strip the RTP header from a packet and return the TS payload slice.
///
/// For raw TS packets (`is_raw_ts` = true), returns the entire data.
/// For RTP-wrapped TS, parses the variable-length RTP header (CSRC + extension)
/// and returns the payload after it.
pub fn strip_rtp_header(packet: &RtpPacket) -> &[u8] {
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

    /// Build a synthetic PMT TS packet with a known version_number.
    fn build_pmt_packet(_pmt_pid: u16, version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID high = 0 (use PAT-like header for layout simplicity)
        pkt[2] = 0x00;
        pkt[3] = 0x10; // payload-only
        pkt[4] = 0x00; // pointer_field
        // Section content (after section_length field, byte 8 onwards):
        // program_number(2) + version+cni(1) + section_num(1) + last_section(1)
        // + pcr_pid(2) + program_info_length(2) + CRC(4) = 13 bytes.
        let section_length = 13;
        pkt[5] = 0x02; // table_id = PMT
        pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00; // program_number high
        pkt[9] = 0x01; // program_number low
        pkt[10] = 0xC0 | ((version & 0x1F) << 1) | 0x01; // reserved + version + current_next
        pkt[11] = 0x00; // section_number
        pkt[12] = 0x00; // last_section_number
        pkt[13] = 0xE0; // reserved + PCR_PID high
        pkt[14] = 0xFF; // PCR_PID low
        pkt[15] = 0xF0; // reserved + program_info_length high
        pkt[16] = 0x00; // program_info_length low
        // CRC32 over bytes [5..17): table_id through program_info_length
        // (the section body excluding the trailing CRC itself).
        let crc = mpeg2_crc32(&pkt[5..17]);
        pkt[17] = (crc >> 24) as u8;
        pkt[18] = (crc >> 16) as u8;
        pkt[19] = (crc >> 8) as u8;
        pkt[20] = crc as u8;
        pkt
    }

    #[test]
    fn set_psi_version_writes_version_and_recomputes_crc() {
        let mut pkt = build_pmt_packet(0x1000, 7);
        // Verify initial version=7 and CRC valid.
        assert_eq!((pkt[10] >> 1) & 0x1F, 7);
        assert!(verify_psi_crc(&pkt, 5));
        // Bump to version 12.
        set_psi_version(&mut pkt, 12);
        assert_eq!((pkt[10] >> 1) & 0x1F, 12);
        // CRC must still validate after the rewrite.
        assert!(verify_psi_crc(&pkt, 5), "CRC must be recomputed by set_psi_version");
    }

    #[test]
    fn set_psi_version_no_op_on_pusi_zero() {
        let mut pkt = build_pmt_packet(0x1000, 7);
        // Clear PUSI.
        pkt[1] &= !0x40;
        let before = pkt;
        set_psi_version(&mut pkt, 12);
        assert_eq!(pkt, before, "no PUSI → set_psi_version is no-op");
    }

    // ── descriptor_audio_kind / descriptors_indicate_text_service ──────

    #[test]
    fn descriptor_audio_kind_dvb_tags() {
        // DVB AC-3 descriptor (0x6A) — the Network TEN / DVB-Australia
        // shape: 0x06 ES with AC-3 descriptor + ISO-639 language.
        let d = [0x0A, 0x04, b'e', b'n', b'g', 0x00, 0x6A, 0x01, 0x44];
        assert_eq!(descriptor_audio_kind(&d), Some(PrivateEsAudioKind::Ac3));

        let d = [0x7A, 0x01, 0x00];
        assert_eq!(descriptor_audio_kind(&d), Some(PrivateEsAudioKind::Eac3));

        let d = [0x7B, 0x05, 0, 0, 0, 0, 0];
        assert_eq!(descriptor_audio_kind(&d), Some(PrivateEsAudioKind::Dts));

        let d = [0x7C, 0x01, 0x00];
        assert_eq!(descriptor_audio_kind(&d), Some(PrivateEsAudioKind::AacLatm));
    }

    #[test]
    fn descriptor_audio_kind_registration_ids() {
        for (fmt, kind) in [
            (*b"AC-3", PrivateEsAudioKind::Ac3),
            (*b"EAC3", PrivateEsAudioKind::Eac3),
            (*b"DTS2", PrivateEsAudioKind::Dts),
            (*b"Opus", PrivateEsAudioKind::Opus),
            (*b"BSSD", PrivateEsAudioKind::Smpte302m),
        ] {
            let d = [0x05, 0x04, fmt[0], fmt[1], fmt[2], fmt[3]];
            assert_eq!(descriptor_audio_kind(&d), Some(kind), "fmt {fmt:?}");
        }
        // Unrecognised registration → None.
        let d = [0x05, 0x04, b'K', b'L', b'V', b'A'];
        assert_eq!(descriptor_audio_kind(&d), None);
    }

    #[test]
    fn descriptor_audio_kind_ac4_extension() {
        let d = [0x7F, 0x02, 0x15, 0x00];
        assert_eq!(descriptor_audio_kind(&d), Some(PrivateEsAudioKind::Ac4));
        // Other extension tags are not audio.
        let d = [0x7F, 0x02, 0x20, 0x00];
        assert_eq!(descriptor_audio_kind(&d), None);
    }

    #[test]
    fn descriptor_audio_kind_rejects_text_and_empty() {
        // Teletext descriptor only → not audio.
        let d = [0x56, 0x05, b'e', b'n', b'g', 0x10, 0x01];
        assert_eq!(descriptor_audio_kind(&d), None);
        assert!(descriptors_indicate_text_service(&d));
        // DVB subtitling.
        let d = [0x59, 0x08, b'e', b'n', b'g', 0x10, 0, 1, 0, 2];
        assert_eq!(descriptor_audio_kind(&d), None);
        assert!(descriptors_indicate_text_service(&d));
        // VBI data / VBI teletext.
        assert!(descriptors_indicate_text_service(&[0x45, 0x00]));
        assert!(descriptors_indicate_text_service(&[0x46, 0x00]));
        // Empty loop → neither audio nor text.
        assert_eq!(descriptor_audio_kind(&[]), None);
        assert!(!descriptors_indicate_text_service(&[]));
    }

    // ── multi-section walker / assembler (VH1 fixture) ─────────────────

    use crate::engine::ts_test_fixtures::{
        packetize_sections, pmt_section, two_packet_pmt, vh1_pmt_packet, VH1_PMT_OFFSET,
        VH1_PROGRAM,
    };

    #[test]
    fn walker_finds_the_pmt_behind_vh1s_private_section() {
        let pkt = vh1_pmt_packet();
        let secs: Vec<PsiSectionRef> = sections_in_packet(&pkt).collect();
        assert_eq!(secs.len(), 2, "0xC0 then the PMT: {secs:?}");
        assert_eq!((secs[0].start, secs[0].table_id, secs[0].ssi), (5, 0xC0, false));
        assert_eq!(secs[0].section_length, 21);
        assert_eq!((secs[1].start, secs[1].table_id, secs[1].ssi), (VH1_PMT_OFFSET, 0x02, true));
        assert!(secs.iter().all(|s| s.complete));
        let pmt = find_section_in_packet(&pkt, 0x02, Some(VH1_PROGRAM)).expect("PMT found");
        assert_eq!(pmt.start, VH1_PMT_OFFSET);
        assert_eq!(pmt_section_offset(&pkt, None), Some(VH1_PMT_OFFSET));
        // A program that is not in the packet is not found.
        assert_eq!(find_section_in_packet(&pkt, 0x02, Some(1)), None);
        assert!(verify_psi_crc(&pkt, VH1_PMT_OFFSET), "fixture PMT CRC is valid");
    }

    #[test]
    fn set_psi_version_never_stamps_a_short_form_section() {
        // The pointer target on VH1 is the SSI=0 0xC0 section. The old
        // set_psi_version overwrote a data byte and its last 4 bytes there.
        let mut pkt = vh1_pmt_packet();
        let before = pkt;
        set_psi_version(&mut pkt, 7);
        assert_eq!(pkt, before, "SSI=0 section has no version and no CRC");
    }

    #[test]
    fn pmt_version_stamp_leaves_the_private_section_byte_identical() {
        let mut pkt = vh1_pmt_packet();
        let before = pkt;
        let pmt = find_section_in_packet(&pkt, 0x02, None).unwrap();
        assert!(set_psi_version_at(&mut pkt, pmt.start, 3));
        assert_eq!(&pkt[..VH1_PMT_OFFSET], &before[..VH1_PMT_OFFSET], "0xC0 untouched");
        assert_eq!((pkt[VH1_PMT_OFFSET + 5] >> 1) & 0x1F, 3);
        assert!(verify_psi_crc(&pkt, VH1_PMT_OFFSET), "PMT CRC recomputed");
    }

    #[test]
    fn set_psi_version_honours_the_adaptation_field() {
        // PAT section behind a 7-byte adaptation field. The old code read
        // the pointer at a fixed pkt[4] (the AF length) and stamped garbage.
        let sec = crate::engine::ts_test_fixtures::pat_section(&[(1, 0x100)], 2);
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40;
        pkt[3] = 0x30; // AF + payload
        pkt[4] = 7; // AF length
        pkt[5] = 0x00; // AF flags
        pkt[12] = 0x00; // pointer_field at 5 + 7
        pkt[13..13 + sec.len()].copy_from_slice(&sec);
        set_psi_version(&mut pkt, 9);
        assert_eq!((pkt[13 + 5] >> 1) & 0x1F, 9);
        assert!(verify_psi_crc(&pkt, 13));
        assert_eq!(&pkt[4..13], &[7, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    }

    #[test]
    fn walker_terminates_on_malformed_chains() {
        // section_length 0 on a PMT, 0xFFF, and a chain running off the end.
        for body in [
            vec![0x00, 0x02, 0xB0, 0x00],
            vec![0x00, 0x02, 0xBF, 0xFF],
            vec![0x00, 0xC0, 0x00, 0x03, 1, 2, 3, 0xC0, 0x0F, 0xFF],
        ] {
            let mut pkt = [0xFFu8; TS_PACKET_SIZE];
            pkt[0] = TS_SYNC_BYTE;
            pkt[1] = 0x40;
            pkt[2] = 0x31;
            pkt[3] = 0x10;
            pkt[4..4 + body.len()].copy_from_slice(&body);
            let n = sections_in_packet(&pkt).count();
            assert!(n <= 2, "must terminate, got {n}");
            assert_eq!(find_section_in_packet(&pkt, 0x02, None), None);
        }
        // A pointer_field that points past the packet.
        let mut pkt = vh1_pmt_packet();
        pkt[4] = 250;
        assert_eq!(sections_in_packet(&pkt).count(), 0);
    }

    #[test]
    fn assembler_yields_every_section_of_the_unit() {
        let pkt = vh1_pmt_packet();
        let mut asm = SectionAssembler::new();
        let tables: Vec<u8> = asm.feed(true, &pkt[4..]).map(|s| s[0]).collect();
        assert_eq!(tables, vec![0xC0, 0x02]);
        let mut asm = SectionAssembler::new();
        let tables: Vec<u8> = asm.push_packet(&pkt).map(|s| s[0]).collect();
        assert_eq!(tables, vec![0xC0, 0x02]);
    }

    #[test]
    fn assembler_completes_a_section_from_the_next_pointer_tail() {
        // Two sections packed so the first ends inside the NEXT PUSI
        // packet's pre-pointer region, and a third starts after it.
        let a = pmt_section(1, 0, 0x100, &[0u8; 170], &[(0x1B, 0x100, &[])]);
        let b = pmt_section(2, 0, 0x200, &[], &[(0x0F, 0x201, &[])]);
        let pkts = packetize_sections(0x31, &[&a, &b], 0);
        assert_eq!(pkts.len(), 2);
        assert!(ts_pusi(&pkts[1]) && pkts[1][4] > 0, "tail before the pointer");
        let mut asm = SectionAssembler::new();
        assert_eq!(asm.push_packet(&pkts[0]).count(), 0);
        let got: Vec<Vec<u8>> = asm.push_packet(&pkts[1]).map(|s| s.to_vec()).collect();
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn assembler_reassembles_a_two_packet_pmt_and_aborts_on_cc_gap() {
        let (sec, _) = two_packet_pmt(7, 1);
        let pkts = packetize_sections(0x40, &[&sec], 5);
        assert_eq!(pkts.len(), 2);
        let mut asm = SectionAssembler::new();
        assert_eq!(asm.push_packet(&pkts[0]).count(), 0);
        let got: Vec<Vec<u8>> = asm.push_packet(&pkts[1]).map(|s| s.to_vec()).collect();
        assert_eq!(got, vec![sec.clone()]);
        // CC gap between the two packets: nothing completes.
        let mut asm = SectionAssembler::new();
        let mut second = pkts[1];
        second[3] = (second[3] & 0xF0) | ((ts_cc(&second) + 1) & 0x0F);
        assert_eq!(asm.push_packet(&pkts[0]).count(), 0);
        assert_eq!(asm.push_packet(&second).count(), 0);
        // A duplicate PUSI packet restarts the unit (harmless); a duplicate
        // continuation is ignored rather than appended twice.
        let big = pmt_section(9, 0, 0x100, &[0u8; 300], &[(0x1B, 0x100, &[])]);
        let pkts = packetize_sections(0x40, &[&big], 0);
        assert_eq!(pkts.len(), 2);
        let mut asm = SectionAssembler::new();
        assert_eq!(asm.push_packet(&pkts[0]).count(), 0);
        assert_eq!(asm.push_packet(&pkts[0]).count(), 0);
        let got: Vec<Vec<u8>> = asm.push_packet(&pkts[1]).map(|s| s.to_vec()).collect();
        assert_eq!(got, vec![big.clone()]);
        let huge = pmt_section(9, 0, 0x100, &[0u8; 500], &[(0x1B, 0x100, &[])]);
        let pkts = packetize_sections(0x40, &[&huge], 0);
        assert_eq!(pkts.len(), 3);
        let mut asm = SectionAssembler::new();
        assert_eq!(asm.push_packet(&pkts[0]).count(), 0);
        assert_eq!(asm.push_packet(&pkts[1]).count(), 0);
        assert_eq!(asm.push_packet(&pkts[1]).count(), 0, "duplicate continuation ignored");
        let got: Vec<Vec<u8>> = asm.push_packet(&pkts[2]).map(|s| s.to_vec()).collect();
        assert_eq!(got, vec![huge]);
    }

    #[test]
    fn descriptor_walk_handles_truncated_loop() {
        // Length runs past the slice — must bail, not panic.
        let d = [0x6A, 0x40, 0x00];
        assert_eq!(descriptor_audio_kind(&d), None);
        assert!(!descriptors_indicate_text_service(&[0x56, 0x40, 0x00]));
    }
}

#[cfg(test)]
mod psi_crc_offset_tests {
    use super::*;

    /// `section_start` for a PUSI packet with `pointer_field == 0`:
    /// 4-byte TS header + 1 pointer byte.
    const SECTION_START: usize = 5;

    #[test]
    fn well_formed_pmt_yields_the_crc_offset() {
        // A minimal legal PMT body is 13 bytes: 9 after the length field
        // plus the 4-byte CRC.
        let off = psi_crc_offset(SECTION_START, 13).expect("13 is a legal PMT section_length");
        assert_eq!(off, SECTION_START + 3 + 13 - 4);
        // The CRC slice covers the 3-byte table header plus the body.
        assert!(off > SECTION_START);
    }

    #[test]
    fn zero_section_length_is_rejected_not_reversed() {
        // THE BUG. `section_start + 3 + 0 - 4` is `section_start - 1`, so
        // `&pkt[section_start..crc_offset]` was a reversed range and panicked.
        // One 188-byte datagram carrying `00 02 B0 00` reached this from the
        // network, pre-auth, and killed the task.
        assert_eq!(psi_crc_offset(SECTION_START, 0), None);
    }

    #[test]
    fn section_shorter_than_its_own_crc_is_rejected() {
        for len in 0..4 {
            assert_eq!(
                psi_crc_offset(SECTION_START, len),
                None,
                "section_length {len} cannot hold a 4-byte CRC"
            );
        }
        assert!(psi_crc_offset(SECTION_START, 4).is_some(), "4 is the minimum");
    }

    #[test]
    fn section_overrunning_the_packet_is_rejected() {
        // section_length is 12 bits, so the wire can declare up to 4095 in a
        // 188-byte packet.
        assert_eq!(psi_crc_offset(SECTION_START, 4095), None);
        assert_eq!(psi_crc_offset(SECTION_START, 0x0FFF), None);
        // Exactly filling the packet is fine; one more byte is not.
        let max_len = TS_PACKET_SIZE - SECTION_START - 3;
        assert!(psi_crc_offset(SECTION_START, max_len).is_some());
        assert_eq!(psi_crc_offset(SECTION_START, max_len + 1), None);
    }

    #[test]
    fn never_panics_over_the_whole_reachable_input_space() {
        // section_length is 12 bits on the wire; section_start is bounded by
        // the packet. Every combination must return, not unwind — and any
        // offset it does return must be a valid, non-reversed slice bound.
        for section_start in 0..TS_PACKET_SIZE {
            for section_length in 0..=0x0FFF {
                if let Some(off) = psi_crc_offset(section_start, section_length) {
                    assert!(off >= section_start, "reversed slice range");
                    assert!(off + 4 <= TS_PACKET_SIZE, "write past the packet");
                }
            }
        }
    }
}
