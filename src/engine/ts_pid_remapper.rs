// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MPEG-TS PID remapper — rewrites TS header PIDs + PSI PID references.
//!
//! Sits in the per-output transform chain after `TsProgramFilter` / audio /
//! video replacers. Given a `source_pid → target_pid` map, it rewrites:
//!
//! - The 13-bit PID field in the TS header for any packet whose PID is a
//!   remap source (bytes 1–2, mask 0x1FFF).
//! - Each `program_map_PID` in the PAT that's in the remap set — CRC32 of
//!   the PAT section is recomputed.
//! - `PCR_PID` and every `elementary_PID` in each PMT whose source/target
//!   is in the remap set — PMT CRC32 is recomputed.
//!
//! PID 0 (PAT) and 0x1FFF (null) are validated out of the map at config
//! load, so they never appear as source or target here. PID discovery is
//! done by parsing the live PAT: the remapper learns PMT PIDs from every
//! PAT it sees. Once a PMT PID is known, PMT packets on that PID are
//! parsed and rewritten in place.
//!
//! The module is byte-level and allocation-light: a single 16 KB lookup
//! table per remapper (`[u16; 8192]` with sentinel 0xFFFF for "no remap")
//! gives branch-free O(1) lookup. Output packets are emitted into the
//! caller-provided buffer; no per-packet allocations occur beyond the
//! occasional re-cache of a PAT when its version bumps.

use std::collections::{BTreeMap, HashMap, HashSet};

use bytes::Bytes;

use super::packet::RtpPacket;
use super::ts_parse::*;

/// Sentinel in the PID lookup table meaning "not remapped".
const NO_REMAP: u16 = 0xFFFF;

/// Per-output PID remapper. Created from a `source_pid → target_pid` map.
///
/// The map is validated by `crate::config::validation` before construction
/// (PID 0 and 0x1FFF rejected, target-uniqueness enforced), so this type
/// never sees an invalid map.
pub struct TsPidRemapper {
    /// Direct-index lookup table: `remap[source_pid] = target_pid` or
    /// [`NO_REMAP`] when the source is pass-through. 16 KB allocated once.
    remap: Box<[u16; 8192]>,
    /// Known PMT PIDs learned from observed PATs. Used to decide which
    /// packets need PMT-rewrite treatment. Uses *original* source PID
    /// space — a PMT PID that's remapped still needs PMT rewriting, and
    /// we detect that by checking the packet's TS header PID *before*
    /// remapping it.
    pmt_pids: HashSet<u16>,
    /// Last PAT version we've observed; skips reparse of duplicates.
    last_pat_version: Option<u8>,
    /// Last PMT version per PMT PID; skips reparse of duplicates.
    last_pmt_versions: HashMap<u16, u8>,
}

impl TsPidRemapper {
    /// Create a new remapper from a `source → target` map. Empty maps are
    /// valid and produce a no-op remapper.
    pub fn new(map: &BTreeMap<u16, u16>) -> Self {
        let mut remap = Box::new([NO_REMAP; 8192]);
        for (src, dst) in map {
            remap[*src as usize] = *dst;
        }
        Self {
            remap,
            pmt_pids: HashSet::new(),
            last_pat_version: None,
            last_pmt_versions: HashMap::new(),
        }
    }

    /// True if this remapper would touch the stream at all. Lets callers
    /// skip the whole stage when the map was empty.
    pub fn is_active(&self) -> bool {
        self.remap.iter().any(|p| *p != NO_REMAP)
    }

    /// Look up the target PID for `src`, or `src` itself if it's pass-through.
    #[inline]
    fn target(&self, src: u16) -> u16 {
        let mapped = self.remap[(src & 0x1FFF) as usize];
        if mapped == NO_REMAP {
            src
        } else {
            mapped
        }
    }

    /// True if `pid` is in the remap source set.
    #[inline]
    fn is_mapped(&self, pid: u16) -> bool {
        self.remap[(pid & 0x1FFF) as usize] != NO_REMAP
    }

    /// Process `ts_in` as a sequence of 188-byte TS packets, appending
    /// rewritten output to `out`. `out` is appended (not cleared); callers
    /// should `out.clear()` first if they want a fresh result.
    pub fn process(&mut self, ts_in: &[u8], out: &mut Vec<u8>) {
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= ts_in.len() {
            let pkt = &ts_in[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if pkt[0] != TS_SYNC_BYTE {
                continue;
            }

            let original_pid = ts_pid(pkt);

            // Decide the rewrite strategy for this packet.
            if original_pid == PAT_PID {
                // PAT: update our PMT-PID catalog, then rewrite program_map_PIDs.
                if ts_pusi(pkt) {
                    self.observe_pat(pkt);
                }
                self.emit_pat(pkt, out);
            } else if self.pmt_pids.contains(&original_pid) {
                // PMT: rewrite pcr_pid + elementary_PIDs, and remap header PID
                // if this PMT PID is itself being moved.
                if ts_pusi(pkt) {
                    self.observe_pmt(original_pid, pkt);
                }
                self.emit_pmt(original_pid, pkt, out);
            } else if self.is_mapped(original_pid) {
                // Ordinary PID remap — rewrite header PID field only.
                let target_pid = self.target(original_pid);
                let mut buf = [0u8; TS_PACKET_SIZE];
                buf.copy_from_slice(pkt);
                write_pid(&mut buf, target_pid);
                out.extend_from_slice(&buf);
            } else {
                // Pass-through.
                out.extend_from_slice(pkt);
            }
        }
    }

    /// Convenience: remap a whole [`RtpPacket`] in place, returning a fresh
    /// [`Bytes`] buffer. For RTP-wrapped input the variable-length RTP
    /// header (including CSRCs) is preserved unchanged — only the TS
    /// payload is rewritten. Returns `None` only when the input held no
    /// usable TS bytes.
    pub fn process_packet(
        &mut self,
        packet: &RtpPacket,
        scratch: &mut Vec<u8>,
    ) -> Option<Bytes> {
        scratch.clear();
        if packet.is_raw_ts {
            self.process(&packet.data, scratch);
            if scratch.is_empty() {
                None
            } else {
                Some(Bytes::copy_from_slice(scratch))
            }
        } else {
            if packet.data.len() < RTP_HEADER_MIN_SIZE {
                return None;
            }
            let cc = (packet.data[0] & 0x0F) as usize;
            let header_len = RTP_HEADER_MIN_SIZE + cc * 4;
            if packet.data.len() <= header_len {
                return None;
            }
            self.process(&packet.data[header_len..], scratch);
            if scratch.is_empty() {
                None
            } else {
                let mut out = Vec::with_capacity(header_len + scratch.len());
                out.extend_from_slice(&packet.data[..header_len]);
                out.extend_from_slice(scratch);
                Some(Bytes::from(out))
            }
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Parse a PAT and update our PMT-PID catalog.
    fn observe_pat(&mut self, pkt: &[u8]) {
        let mut sec_off = 4;
        if ts_has_adaptation(pkt) {
            let af_len = pkt[4] as usize;
            sec_off = 5 + af_len;
        }
        if sec_off >= TS_PACKET_SIZE {
            return;
        }
        let pointer = pkt[sec_off] as usize;
        sec_off += 1 + pointer;
        if sec_off + 8 > TS_PACKET_SIZE {
            return;
        }
        if pkt[sec_off] != 0x00 {
            return;
        }
        let version = (pkt[sec_off + 5] >> 1) & 0x1F;
        if self.last_pat_version == Some(version) {
            return;
        }
        self.last_pat_version = Some(version);

        let programs = parse_pat_programs(pkt);
        self.pmt_pids.clear();
        self.last_pmt_versions.clear();
        for (_, pmt_pid) in programs {
            self.pmt_pids.insert(pmt_pid);
        }
    }

    /// Parse a PMT — we don't cache its contents, only version-gate reparse.
    fn observe_pmt(&mut self, pmt_pid: u16, pkt: &[u8]) {
        if let Some(version) = pmt_version(pkt) {
            let prev = self.last_pmt_versions.insert(pmt_pid, version);
            if prev == Some(version) {
                // Version unchanged — nothing to do. (Re-emission still
                // happens in emit_pmt; this only avoids repeated log/work
                // when we later grow to cache PMT contents.)
            }
        }
    }

    /// Emit a PAT packet, rewriting `program_map_PID` fields for any PMT
    /// PID that's in the remap set. Recomputes CRC.
    fn emit_pat(&self, pkt: &[u8], out: &mut Vec<u8>) {
        let mut buf = [0u8; TS_PACKET_SIZE];
        buf.copy_from_slice(pkt);

        // If no PMT PIDs are remapped at all, we can short-circuit.
        let has_any_pmt_remap = self.pmt_pids.iter().any(|pid| self.is_mapped(*pid));
        if !has_any_pmt_remap {
            out.extend_from_slice(&buf);
            return;
        }

        // Locate the section.
        let mut sec_off = 4;
        if ts_has_adaptation(&buf) {
            let af_len = buf[4] as usize;
            sec_off = 5 + af_len;
        }
        if sec_off >= TS_PACKET_SIZE {
            out.extend_from_slice(&buf);
            return;
        }
        let pointer = buf[sec_off] as usize;
        sec_off += 1 + pointer;
        if sec_off + 8 > TS_PACKET_SIZE {
            out.extend_from_slice(&buf);
            return;
        }
        if buf[sec_off] != 0x00 {
            out.extend_from_slice(&buf);
            return;
        }
        let section_length =
            (((buf[sec_off + 1] & 0x0F) as usize) << 8) | (buf[sec_off + 2] as usize);
        let data_start = sec_off + 8;
        let data_end = (sec_off + 3 + section_length).min(TS_PACKET_SIZE).saturating_sub(4);

        // Walk entries: each is 4 bytes (program_number, reserved+pid).
        let mut pos = data_start;
        while pos + 4 <= data_end {
            let program_number = ((buf[pos] as u16) << 8) | buf[pos + 1] as u16;
            let pmt_pid = ((buf[pos + 2] as u16 & 0x1F) << 8) | buf[pos + 3] as u16;
            // NIT entry (program_number == 0) stays untouched.
            if program_number != 0 && self.is_mapped(pmt_pid) {
                let new_pid = self.target(pmt_pid);
                buf[pos + 2] = 0xE0 | (((new_pid >> 8) & 0x1F) as u8);
                buf[pos + 3] = (new_pid & 0xFF) as u8;
            }
            pos += 4;
        }

        // Recompute CRC over section from table_id (sec_off) to just
        // before the 4 CRC bytes at data_end..data_end+4.
        let crc_start = sec_off;
        let crc_end = data_end;
        if crc_end + 4 <= TS_PACKET_SIZE && crc_end >= crc_start {
            let crc = mpeg2_crc32(&buf[crc_start..crc_end]);
            buf[crc_end] = (crc >> 24) as u8;
            buf[crc_end + 1] = (crc >> 16) as u8;
            buf[crc_end + 2] = (crc >> 8) as u8;
            buf[crc_end + 3] = crc as u8;
        }

        // PAT's own header PID is 0 by definition and is never remapped.
        out.extend_from_slice(&buf);
    }

    /// Emit a PMT packet, rewriting `PCR_PID` + elementary `PID` fields
    /// that are in the remap set, recomputing CRC, and rewriting the TS
    /// header PID if the PMT PID itself is remapped.
    fn emit_pmt(&self, pmt_pid: u16, pkt: &[u8], out: &mut Vec<u8>) {
        let mut buf = [0u8; TS_PACKET_SIZE];
        buf.copy_from_slice(pkt);

        // Walk to the section start.
        let mut sec_off = 4;
        if ts_has_adaptation(&buf) {
            let af_len = buf[4] as usize;
            sec_off = 5 + af_len;
        }
        if sec_off >= TS_PACKET_SIZE {
            // Malformed — just retag header PID and emit.
            if self.is_mapped(pmt_pid) {
                write_pid(&mut buf, self.target(pmt_pid));
            }
            out.extend_from_slice(&buf);
            return;
        }
        let pointer = buf[sec_off] as usize;
        sec_off += 1 + pointer;
        if sec_off + 12 > TS_PACKET_SIZE {
            if self.is_mapped(pmt_pid) {
                write_pid(&mut buf, self.target(pmt_pid));
            }
            out.extend_from_slice(&buf);
            return;
        }
        if buf[sec_off] != 0x02 {
            // Not actually a PMT — leave alone.
            if self.is_mapped(pmt_pid) {
                write_pid(&mut buf, self.target(pmt_pid));
            }
            out.extend_from_slice(&buf);
            return;
        }

        let section_length =
            (((buf[sec_off + 1] & 0x0F) as usize) << 8) | (buf[sec_off + 2] as usize);

        // Rewrite PCR_PID (13 bits at sec_off+8..+9).
        let pcr_pid = ((buf[sec_off + 8] as u16 & 0x1F) << 8) | buf[sec_off + 9] as u16;
        if self.is_mapped(pcr_pid) {
            let new_pid = self.target(pcr_pid);
            buf[sec_off + 8] = 0xE0 | (((new_pid >> 8) & 0x1F) as u8);
            buf[sec_off + 9] = (new_pid & 0xFF) as u8;
        }

        let program_info_length =
            (((buf[sec_off + 10] & 0x0F) as usize) << 8) | (buf[sec_off + 11] as usize);
        let data_start = sec_off + 12 + program_info_length;
        let data_end = (sec_off + 3 + section_length).min(TS_PACKET_SIZE).saturating_sub(4);

        // Per-stream entries: stream_type(1) + reserved+PID(2) + reserved+es_info_len(2).
        let mut pos = data_start;
        while pos + 5 <= data_end {
            let es_pid = ((buf[pos + 1] as u16 & 0x1F) << 8) | buf[pos + 2] as u16;
            if self.is_mapped(es_pid) {
                let new_pid = self.target(es_pid);
                buf[pos + 1] = 0xE0 | (((new_pid >> 8) & 0x1F) as u8);
                buf[pos + 2] = (new_pid & 0xFF) as u8;
            }
            let es_info_length =
                (((buf[pos + 3] & 0x0F) as usize) << 8) | (buf[pos + 4] as usize);
            pos += 5 + es_info_length;
        }

        // Recompute PMT CRC.
        let crc_start = sec_off;
        let crc_end = data_end;
        if crc_end + 4 <= TS_PACKET_SIZE && crc_end >= crc_start {
            let crc = mpeg2_crc32(&buf[crc_start..crc_end]);
            buf[crc_end] = (crc >> 24) as u8;
            buf[crc_end + 1] = (crc >> 16) as u8;
            buf[crc_end + 2] = (crc >> 8) as u8;
            buf[crc_end + 3] = crc as u8;
        }

        // Finally, rewrite TS header PID if this PMT PID itself is remapped.
        if self.is_mapped(pmt_pid) {
            write_pid(&mut buf, self.target(pmt_pid));
        }
        out.extend_from_slice(&buf);
    }
}

/// Write a 13-bit PID into the TS header (bytes 1–2 of `pkt`) without
/// touching TEI/PUSI/TPR bits in byte 1 or scrambling/AFC/CC in byte 3.
#[inline]
fn write_pid(pkt: &mut [u8; TS_PACKET_SIZE], pid: u16) {
    let pid = pid & 0x1FFF;
    pkt[1] = (pkt[1] & 0xE0) | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
}

/// Extract the `version_number` (5 bits) from a PMT section payload.
fn pmt_version(pkt: &[u8]) -> Option<u8> {
    let mut sec_off = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        sec_off = 5 + af_len;
    }
    if sec_off >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[sec_off] as usize;
    sec_off += 1 + pointer;
    if sec_off + 6 > TS_PACKET_SIZE {
        return None;
    }
    if pkt[sec_off] != 0x02 {
        return None;
    }
    Some((pkt[sec_off + 5] >> 1) & 0x1F)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pat_packet(programs: &[(u16, u16)], version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID=0
        pkt[2] = 0x00;
        pkt[3] = 0x10; // payload only, CC=0
        pkt[4] = 0x00; // pointer_field
        let section_length = 5 + 4 * programs.len() + 4;
        pkt[5] = 0x00; // table_id PAT
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1 | ((version & 0x1F) << 1);
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        let mut pos = 13;
        for (pn, pmt_pid) in programs {
            pkt[pos] = (pn >> 8) as u8;
            pkt[pos + 1] = (pn & 0xFF) as u8;
            pkt[pos + 2] = 0xE0 | (((pmt_pid >> 8) as u8) & 0x1F);
            pkt[pos + 3] = (pmt_pid & 0xFF) as u8;
            pos += 4;
        }
        // CRC
        let crc_end = 5 + 3 + section_length - 4;
        let crc = mpeg2_crc32(&pkt[5..crc_end]);
        pkt[crc_end] = (crc >> 24) as u8;
        pkt[crc_end + 1] = (crc >> 16) as u8;
        pkt[crc_end + 2] = (crc >> 8) as u8;
        pkt[crc_end + 3] = crc as u8;
        pkt
    }

    fn build_pmt_packet(pmt_pid: u16, streams: &[(u8, u16)], pcr_pid: u16, version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let section_data_len = 9 + 5 * streams.len() + 4;
        let section_length: u16 = section_data_len as u16;

        pkt[5] = 0x02; // PMT
        pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00; // program_number hi
        pkt[9] = 0x01;
        pkt[10] = 0xC1 | ((version & 0x1F) << 1);
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | (((pcr_pid >> 8) as u8) & 0x1F);
        pkt[14] = (pcr_pid & 0xFF) as u8;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        let mut pos = 17;
        for (st, es_pid) in streams {
            pkt[pos] = *st;
            pkt[pos + 1] = 0xE0 | (((es_pid >> 8) as u8) & 0x1F);
            pkt[pos + 2] = (es_pid & 0xFF) as u8;
            pkt[pos + 3] = 0xF0;
            pkt[pos + 4] = 0x00;
            pos += 5;
        }
        let crc_end = 5 + 3 + section_length as usize - 4;
        let crc = mpeg2_crc32(&pkt[5..crc_end]);
        pkt[crc_end] = (crc >> 24) as u8;
        pkt[crc_end + 1] = (crc >> 16) as u8;
        pkt[crc_end + 2] = (crc >> 8) as u8;
        pkt[crc_end + 3] = crc as u8;
        pkt
    }

    fn build_es_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((pid >> 8) as u8) & 0x1F;
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F);
        pkt
    }

    #[test]
    fn empty_map_is_pass_through() {
        let map = BTreeMap::new();
        let mut r = TsPidRemapper::new(&map);
        assert!(!r.is_active());
        let pat = build_pat_packet(&[(1, 0x100)], 0);
        let es = build_es_packet(0x200, 3);
        let mut input = Vec::new();
        input.extend_from_slice(&pat);
        input.extend_from_slice(&es);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn remaps_es_pid_in_ts_header() {
        let map: BTreeMap<u16, u16> = [(0x200, 0x400)].into_iter().collect();
        let mut r = TsPidRemapper::new(&map);
        let es = build_es_packet(0x200, 5);
        let mut out = Vec::new();
        r.process(&es, &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE);
        assert_eq!(ts_pid(&out), 0x400);
        // CC must be preserved.
        assert_eq!(ts_cc(&out), 5);
    }

    #[test]
    fn remaps_pmt_pid_in_header_and_pat_entry() {
        let map: BTreeMap<u16, u16> = [(0x100, 0x500)].into_iter().collect();
        let mut r = TsPidRemapper::new(&map);
        let pat = build_pat_packet(&[(1, 0x100)], 0);
        let pmt = build_pmt_packet(0x100, &[(0x1B, 0x200)], 0x200, 0);
        let mut input = Vec::new();
        input.extend_from_slice(&pat);
        input.extend_from_slice(&pmt);
        let mut out = Vec::new();
        r.process(&input, &mut out);

        // PAT program entry's program_map_PID should now be 0x500.
        let out_pat = &out[..TS_PACKET_SIZE];
        let programs = parse_pat_programs(out_pat);
        assert_eq!(programs, vec![(1, 0x500)]);
        // PMT TS header PID should now be 0x500.
        let out_pmt = &out[TS_PACKET_SIZE..2 * TS_PACKET_SIZE];
        assert_eq!(ts_pid(out_pmt), 0x500);
    }

    #[test]
    fn remaps_elementary_pid_and_pcr_inside_pmt() {
        let map: BTreeMap<u16, u16> = [(0x200, 0x400), (0x201, 0x401)].into_iter().collect();
        let mut r = TsPidRemapper::new(&map);
        let pat = build_pat_packet(&[(1, 0x100)], 0);
        let pmt = build_pmt_packet(0x100, &[(0x1B, 0x200), (0x0F, 0x201)], 0x200, 0);
        let mut input = Vec::new();
        input.extend_from_slice(&pat);
        input.extend_from_slice(&pmt);
        let mut out = Vec::new();
        r.process(&input, &mut out);

        // PAT unchanged (PMT PID 0x100 not in map).
        assert_eq!(ts_pid(&out[..TS_PACKET_SIZE]), PAT_PID);
        assert_eq!(parse_pat_programs(&out[..TS_PACKET_SIZE]), vec![(1, 0x100)]);
        // PMT contents — parse PCR_PID and elementary_PIDs.
        let out_pmt = &out[TS_PACKET_SIZE..2 * TS_PACKET_SIZE];
        assert_eq!(ts_pid(out_pmt), 0x100); // PMT PID itself unchanged
        // sec_off for PMT (no adaptation, pointer_field=0, so section starts at byte 5).
        // Stream entries start at sec_off + 12 (after PSI header + pcr_pid + program_info_len
        // when program_info_length = 0). Each entry is 5 bytes: stream_type(1)
        // + reserved+pid_hi(1) + pid_lo(1) + reserved+es_info_len(2).
        let sec_off = 5;
        let pcr_pid = ((out_pmt[sec_off + 8] as u16 & 0x1F) << 8) | out_pmt[sec_off + 9] as u16;
        assert_eq!(pcr_pid, 0x400, "PCR_PID remapped");
        let stream1 = sec_off + 12;
        let video_pid = ((out_pmt[stream1 + 1] as u16 & 0x1F) << 8) | out_pmt[stream1 + 2] as u16;
        assert_eq!(video_pid, 0x400, "video elementary PID remapped");
        let stream2 = stream1 + 5;
        let audio_pid = ((out_pmt[stream2 + 1] as u16 & 0x1F) << 8) | out_pmt[stream2 + 2] as u16;
        assert_eq!(audio_pid, 0x401, "audio elementary PID remapped");
    }

    #[test]
    fn pmt_crc_valid_after_remap() {
        let map: BTreeMap<u16, u16> = [(0x200, 0x400)].into_iter().collect();
        let mut r = TsPidRemapper::new(&map);
        let pat = build_pat_packet(&[(1, 0x100)], 0);
        let pmt = build_pmt_packet(0x100, &[(0x1B, 0x200)], 0x200, 0);
        let mut input = Vec::new();
        input.extend_from_slice(&pat);
        input.extend_from_slice(&pmt);
        let mut out = Vec::new();
        r.process(&input, &mut out);

        let out_pmt = &out[TS_PACKET_SIZE..2 * TS_PACKET_SIZE];
        assert!(verify_psi_crc(out_pmt, 5), "PMT CRC32 must be valid after remap");
    }

    #[test]
    fn pat_crc_valid_after_pmt_pid_remap() {
        let map: BTreeMap<u16, u16> = [(0x100, 0x500)].into_iter().collect();
        let mut r = TsPidRemapper::new(&map);
        let pat = build_pat_packet(&[(1, 0x100)], 0);
        let mut out = Vec::new();
        r.process(&pat, &mut out);
        assert!(verify_psi_crc(&out[..TS_PACKET_SIZE], 5), "PAT CRC32 must be valid after PMT-PID remap");
    }

    #[test]
    fn null_packets_pass_through() {
        let map: BTreeMap<u16, u16> = [(0x200, 0x400)].into_iter().collect();
        let mut r = TsPidRemapper::new(&map);
        let null = build_es_packet(0x1FFF, 0);
        let mut out = Vec::new();
        r.process(&null, &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE);
        assert_eq!(ts_pid(&out), 0x1FFF);
    }

    #[test]
    fn pid_swap_works() {
        // Two-way swap: 0x200 ↔ 0x300.
        let map: BTreeMap<u16, u16> = [(0x200, 0x300), (0x300, 0x200)].into_iter().collect();
        let mut r = TsPidRemapper::new(&map);
        let p200 = build_es_packet(0x200, 0);
        let p300 = build_es_packet(0x300, 0);
        let mut input = Vec::new();
        input.extend_from_slice(&p200);
        input.extend_from_slice(&p300);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(ts_pid(&out[..TS_PACKET_SIZE]), 0x300);
        assert_eq!(ts_pid(&out[TS_PACKET_SIZE..2 * TS_PACKET_SIZE]), 0x200);
    }

    #[test]
    fn remap_preserves_non_pid_header_bits() {
        let map: BTreeMap<u16, u16> = [(0x200, 0x400)].into_iter().collect();
        let mut r = TsPidRemapper::new(&map);
        let mut pkt = build_es_packet(0x200, 7);
        pkt[1] |= 0x40; // PUSI=1
        pkt[1] |= 0x80; // TEI=1
        pkt[3] = 0x50 | 7; // scrambling=01, AFC=01, CC=7 — mask expected below
        let mut out = Vec::new();
        r.process(&pkt, &mut out);
        // PID rewritten, but TEI, PUSI, AFC, scrambling, CC must survive.
        assert_eq!(ts_pid(&out), 0x400);
        assert!(ts_tei(&out));
        assert!(ts_pusi(&out));
        assert_eq!(ts_cc(&out), 7);
        assert_eq!(out[3] & 0xF0, 0x50); // scrambling=01, AFC=01
    }
}
