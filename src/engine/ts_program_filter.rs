// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! MPEG-TS program filter — down-selects an MPTS to a single-program SPTS.
//!
//! Used by every TS-native output (UDP / RTP / SRT / HLS) and by the
//! thumbnail generator. Reads the PAT to discover the target program's
//! PMT PID, parses that PMT to derive the set of allowed elementary
//! stream PIDs (plus PCR PID), and rewrites the PAT into a single-program
//! form. All other TS packets are dropped silently.
//!
//! The filter is byte-level: it consumes a stream of 188-byte TS packets
//! and emits another stream of 188-byte TS packets. It does not touch
//! continuity counters of pass-through PIDs (those PIDs are unique within
//! an MPTS so their CC sequence is naturally intact). The synthetic PAT
//! packets carry their own private CC counter so the receiver sees a
//! valid CC progression on PID 0.

use std::collections::HashSet;

use bytes::Bytes;

use super::packet::RtpPacket;
use super::ts_parse::*;

/// Minimum RTP header size used when stripping wrapped RTP packets.
const RTP_HEADER_MIN_SIZE: usize = 12;

/// Down-selects an MPTS to a single program by PID-level filtering.
///
/// Lifecycle: create one per output via [`TsProgramFilter::new`], then call
/// [`TsProgramFilter::filter_into`] for every received TS payload chunk.
/// State is preserved across calls so PAT/PMT version bumps and program
/// disappearance/reappearance are handled gracefully.
pub struct TsProgramFilter {
    /// Program number we're locking onto.
    target_program: u16,
    /// PMT PID for the target program (set after the first PAT seen).
    target_pmt_pid: Option<u16>,
    /// Allowed pass-through PIDs (PMT PID + each ES PID + PCR PID).
    /// PID 0 (PAT) is handled specially and never appears in this set.
    allowed_pids: HashSet<u16>,
    /// Last PAT version we processed; used to skip rewriting on duplicates.
    last_pat_version: Option<u8>,
    /// Last PMT version we processed for the target program.
    last_pmt_version: Option<u8>,
    /// Continuity counter for our synthetic PAT packets (0..16).
    synthetic_pat_cc: u8,
    /// transport_stream_id carried into our synthetic PAT (taken from the
    /// observed PAT so receivers see a stable value).
    transport_stream_id: u16,
    /// Cached synthetic PAT packet (regenerated whenever the target's PMT
    /// PID changes; CC field is patched per emission).
    cached_pat: Option<[u8; TS_PACKET_SIZE]>,
}

impl TsProgramFilter {
    /// Create a new filter targeting `target_program`.
    pub fn new(target_program: u16) -> Self {
        Self {
            target_program,
            target_pmt_pid: None,
            allowed_pids: HashSet::new(),
            last_pat_version: None,
            last_pmt_version: None,
            synthetic_pat_cc: 0,
            transport_stream_id: 0x0001,
            cached_pat: None,
        }
    }

    /// Filter `ts_in` into `out`. Reads `ts_in` as a sequence of 188-byte
    /// TS packets; non-TS-aligned bytes are skipped silently. Each PAT
    /// packet is replaced with a freshly generated single-program PAT.
    /// Each PMT/ES/PCR packet belonging to the target program is appended
    /// untouched. Everything else is dropped.
    ///
    /// `out` is appended to (not cleared) so callers can reuse the buffer
    /// across calls. Callers should `out.clear()` first if they want a
    /// fresh result.
    pub fn filter_into(&mut self, ts_in: &[u8], out: &mut Vec<u8>) {
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= ts_in.len() {
            let pkt = &ts_in[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if pkt[0] != TS_SYNC_BYTE {
                continue;
            }

            let pid = ts_pid(pkt);

            if pid == PAT_PID {
                if ts_pusi(pkt) {
                    self.handle_pat(pkt);
                }
                if let Some(ref pat) = self.cached_pat {
                    let mut emitted = *pat;
                    // Patch CC field (lower nibble of byte 3) with our counter
                    emitted[3] = (emitted[3] & 0xF0) | (self.synthetic_pat_cc & 0x0F);
                    self.synthetic_pat_cc = self.synthetic_pat_cc.wrapping_add(1) & 0x0F;
                    out.extend_from_slice(&emitted);
                }
                continue;
            }

            // PMT for the target program — parse to update allowed_pids,
            // then pass through unchanged.
            if Some(pid) == self.target_pmt_pid {
                if ts_pusi(pkt) {
                    self.handle_pmt(pkt);
                }
                out.extend_from_slice(pkt);
                continue;
            }

            // PMT for a different program (or any other PID we don't care
            // about) is dropped. PIDs in allowed_pids pass through.
            if self.allowed_pids.contains(&pid) {
                out.extend_from_slice(pkt);
            }
        }
    }

    /// Filter a whole [`RtpPacket`] in one shot, returning the resulting
    /// `Bytes` (or `None` if everything was dropped). Preserves the RTP
    /// header (first 12 bytes) when `packet.is_raw_ts == false` so the
    /// caller's downstream wrapping (RTP / SRT) keeps working unchanged.
    ///
    /// `scratch` is a reusable workspace owned by the caller — saves a
    /// per-packet allocation.
    pub fn filter_packet(
        &mut self,
        packet: &RtpPacket,
        scratch: &mut Vec<u8>,
    ) -> Option<Bytes> {
        scratch.clear();
        if packet.is_raw_ts {
            self.filter_into(&packet.data, scratch);
            if scratch.is_empty() {
                None
            } else {
                Some(Bytes::copy_from_slice(scratch))
            }
        } else {
            if packet.data.len() < RTP_HEADER_MIN_SIZE {
                return None;
            }
            // Preserve the variable-length RTP header up to (and including)
            // any CSRC list. We don't recompute extension headers — they're
            // pre-payload metadata and survive a payload shrink.
            let cc = (packet.data[0] & 0x0F) as usize;
            let header_len = RTP_HEADER_MIN_SIZE + cc * 4;
            if packet.data.len() <= header_len {
                return None;
            }
            self.filter_into(&packet.data[header_len..], scratch);
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

    fn handle_pat(&mut self, pkt: &[u8]) {
        // Locate the PSI section header to read transport_stream_id and version
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
            return; // not a PAT table_id
        }
        let ts_id = ((pkt[sec_off + 3] as u16) << 8) | pkt[sec_off + 4] as u16;
        let version = (pkt[sec_off + 5] >> 1) & 0x1F;

        let programs = parse_pat_programs(pkt);
        let pmt_pid = programs
            .iter()
            .find(|(num, _)| *num == self.target_program)
            .map(|(_, pid)| *pid);

        // If the target disappeared from the PAT, drop the cached PAT
        // and clear allowed_pids so subsequent ES packets get filtered out
        // until the program reappears.
        let target_changed = match (self.target_pmt_pid, pmt_pid) {
            (Some(a), Some(b)) => a != b,
            (None, Some(_)) => true,
            (Some(_), None) => true,
            (None, None) => false,
        };

        if pmt_pid.is_none() {
            self.target_pmt_pid = None;
            self.allowed_pids.clear();
            self.cached_pat = None;
            self.last_pmt_version = None;
            self.last_pat_version = Some(version);
            self.transport_stream_id = ts_id;
            return;
        }

        // Skip regenerating if version is unchanged AND PMT PID hasn't moved.
        if !target_changed && self.last_pat_version == Some(version) {
            return;
        }

        if target_changed {
            // PMT PID changed — wipe ES set so the next PMT can repopulate.
            self.allowed_pids.clear();
            self.last_pmt_version = None;
        }

        self.target_pmt_pid = pmt_pid;
        self.transport_stream_id = ts_id;
        self.last_pat_version = Some(version);
        self.cached_pat = Some(self.build_synthetic_pat());
    }

    fn handle_pmt(&mut self, pkt: &[u8]) {
        if let Some((es_pids, pcr_pid, version)) = extract_pmt_streams(pkt) {
            if self.last_pmt_version == Some(version) {
                return;
            }
            self.last_pmt_version = Some(version);
            self.allowed_pids.clear();
            if let Some(pmt_pid) = self.target_pmt_pid {
                self.allowed_pids.insert(pmt_pid);
            }
            for pid in es_pids {
                self.allowed_pids.insert(pid);
            }
            if let Some(pcr) = pcr_pid {
                if pcr != 0x1FFF {
                    self.allowed_pids.insert(pcr);
                }
            }
        }
    }

    /// Build a fresh single-program PAT TS packet pointing at `target_pmt_pid`.
    /// CC bits are zeroed; the emitter patches them per packet.
    fn build_synthetic_pat(&self) -> [u8; TS_PACKET_SIZE] {
        let pmt_pid = self.target_pmt_pid.unwrap_or(0);
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];

        // TS header
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI = 1, TEI = 0, transport_priority = 0, PID hi = 0
        pkt[2] = 0x00; // PID lo = 0 (PAT_PID)
        pkt[3] = 0x10; // scrambling=00, AFC=01 (payload only), CC=0
        // pointer_field
        pkt[4] = 0x00;

        // PSI section starts at offset 5
        // PAT section: table_id(1) + len(2) + ts_id(2) + ver/cni(1) +
        //              section_no(1) + last_section(1) + 1 entry (4) + crc(4)
        // section_length covers from after the length field through CRC:
        //   ts_id(2) + ver/cni(1) + section_no(1) + last_section(1) +
        //   entry(4) + crc(4) = 13 bytes
        let section_length: u16 = 13;
        pkt[5] = 0x00; // table_id = PAT
        pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8); // syntax=1, '0', reserved=11, len hi
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = (self.transport_stream_id >> 8) as u8;
        pkt[9] = (self.transport_stream_id & 0xFF) as u8;
        pkt[10] = 0xC1; // reserved=11, version=0, current_next=1
        pkt[11] = 0x00; // section_number
        pkt[12] = 0x00; // last_section_number

        // Single program entry
        pkt[13] = (self.target_program >> 8) as u8;
        pkt[14] = (self.target_program & 0xFF) as u8;
        pkt[15] = 0xE0 | (((pmt_pid >> 8) & 0x1F) as u8); // reserved=111, pid hi
        pkt[16] = (pmt_pid & 0xFF) as u8;

        // CRC32 covers the section from table_id (offset 5) up to but not
        // including the CRC bytes themselves. Section ends at:
        //   section_start + 3 + section_length
        // = 5 + 3 + 13 = 21
        // CRC bytes occupy 17..21.
        let crc = mpeg2_crc32(&pkt[5..17]);
        pkt[17] = (crc >> 24) as u8;
        pkt[18] = (crc >> 16) as u8;
        pkt[19] = (crc >> 8) as u8;
        pkt[20] = crc as u8;
        // The remaining bytes (21..188) are already 0xFF padding.

        pkt
    }
}

/// Parse a single-packet PMT and extract its (es_pids, pcr_pid, version_number).
/// Mirrors the logic in `media_analysis::parse_pmt_streams` but stays local
/// to avoid coupling. Returns None if the packet is malformed.
fn extract_pmt_streams(pkt: &[u8]) -> Option<(Vec<u16>, Option<u16>, u8)> {
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
    if sec_off + 12 > TS_PACKET_SIZE {
        return None;
    }
    if pkt[sec_off] != 0x02 {
        return None;
    }
    let section_length =
        (((pkt[sec_off + 1] & 0x0F) as usize) << 8) | (pkt[sec_off + 2] as usize);
    let version = (pkt[sec_off + 5] >> 1) & 0x1F;
    let pcr_pid = ((pkt[sec_off + 8] as u16 & 0x1F) << 8) | pkt[sec_off + 9] as u16;
    let program_info_length =
        (((pkt[sec_off + 10] & 0x0F) as usize) << 8) | (pkt[sec_off + 11] as usize);

    let data_start = sec_off + 12 + program_info_length;
    let data_end = (sec_off + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut es_pids = Vec::new();
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_length =
            (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        es_pids.push(es_pid);
        pos += 5 + es_info_length;
    }
    Some((es_pids, Some(pcr_pid), version))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic PAT TS packet (same helper used by ts_parse tests).
    fn build_pat_packet(programs: &[(u16, u16)], with_nit: bool, version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40;
        pkt[2] = 0x00;
        pkt[3] = 0x10;
        pkt[4] = 0x00; // pointer_field
        let entries_count = programs.len() + if with_nit { 1 } else { 0 };
        let section_length = 5 + 4 * entries_count + 4;
        pkt[5] = 0x00; // table_id PAT
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00; // ts_id hi
        pkt[9] = 0x01; // ts_id lo
        pkt[10] = 0xC1 | ((version & 0x1F) << 1); // version + current_next=1
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        let mut pos = 13;
        if with_nit {
            pkt[pos] = 0x00;
            pkt[pos + 1] = 0x00;
            pkt[pos + 2] = 0xE0;
            pkt[pos + 3] = 0x10;
            pos += 4;
        }
        for (program_number, pmt_pid) in programs {
            pkt[pos] = (program_number >> 8) as u8;
            pkt[pos + 1] = (program_number & 0xFF) as u8;
            pkt[pos + 2] = 0xE0 | (((pmt_pid >> 8) as u8) & 0x1F);
            pkt[pos + 3] = (pmt_pid & 0xFF) as u8;
            pos += 4;
        }
        pkt
    }

    /// Build a synthetic PMT TS packet for `program_number` declaring the
    /// given (stream_type, es_pid) entries. PCR_PID is set to the first ES.
    fn build_pmt_packet(pmt_pid: u16, _program_number: u16, streams: &[(u8, u16)], version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00; // pointer_field
        // Section header: table_id + len(2) + program_number(2) + ver/cni(1)
        //   + section_no(1) + last_section(1) + reserved+pcr_pid(2) +
        //   reserved+program_info_length(2) = 12 bytes
        // Per-stream: stream_type(1) + reserved+pid(2) + reserved+es_info_len(2) = 5 bytes each
        // Trailer: CRC(4)
        let section_data_len = 9 + 5 * streams.len() + 4; // ts_id..end-of-loop + crc
        let section_length: u16 = section_data_len as u16;
        let pcr_pid = streams.first().map(|(_, p)| *p).unwrap_or(0x1FFF);

        pkt[5] = 0x02; // table_id = PMT
        pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00; // program_number hi
        pkt[9] = 0x01; // program_number lo (placeholder; tests don't validate this)
        pkt[10] = 0xC1 | ((version & 0x1F) << 1);
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | (((pcr_pid >> 8) as u8) & 0x1F);
        pkt[14] = (pcr_pid & 0xFF) as u8;
        pkt[15] = 0xF0; // reserved + program_info_length hi
        pkt[16] = 0x00; // program_info_length lo
        let mut pos = 17;
        for (stream_type, es_pid) in streams {
            pkt[pos] = *stream_type;
            pkt[pos + 1] = 0xE0 | (((es_pid >> 8) as u8) & 0x1F);
            pkt[pos + 2] = (es_pid & 0xFF) as u8;
            pkt[pos + 3] = 0xF0;
            pkt[pos + 4] = 0x00;
            pos += 5;
        }
        // CRC bytes — not validated by parse helper but fill in to be polite.
        let crc_section_end = 5 + 3 + section_length as usize;
        let crc = mpeg2_crc32(&pkt[5..crc_section_end - 4]);
        pkt[crc_section_end - 4] = (crc >> 24) as u8;
        pkt[crc_section_end - 3] = (crc >> 16) as u8;
        pkt[crc_section_end - 2] = (crc >> 8) as u8;
        pkt[crc_section_end - 1] = crc as u8;
        pkt
    }

    /// Build a 188-byte ES TS packet for `pid` carrying dummy payload.
    fn build_es_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((pid >> 8) as u8) & 0x1F;
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F);
        pkt
    }

    fn make_mpts_2programs(pat_version: u8, pmt_version: u8) -> Vec<u8> {
        // PAT advertises programs 1 (PMT 0x1000) and 2 (PMT 0x1500).
        // Program 1: video PID 0x1100, audio PID 0x1101.
        // Program 2: video PID 0x1600, audio PID 0x1601.
        // (All PIDs stay within the 13-bit MPEG-TS PID range, max 0x1FFF.)
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat_packet(&[(1, 0x1000), (2, 0x1500)], false, pat_version));
        buf.extend_from_slice(&build_pmt_packet(0x1000, 1, &[(0x1B, 0x1100), (0x0F, 0x1101)], pmt_version));
        buf.extend_from_slice(&build_pmt_packet(0x1500, 2, &[(0x1B, 0x1600), (0x0F, 0x1601)], pmt_version));
        buf.extend_from_slice(&build_es_packet(0x1100, 0));
        buf.extend_from_slice(&build_es_packet(0x1101, 0));
        buf.extend_from_slice(&build_es_packet(0x1600, 0));
        buf.extend_from_slice(&build_es_packet(0x1601, 0));
        // Throw in an unrelated PID that should be filtered away too.
        buf.extend_from_slice(&build_es_packet(0x1700, 0));
        buf
    }

    #[test]
    fn filter_keeps_only_selected_program_pids() {
        let mut filter = TsProgramFilter::new(2);
        let mpts = make_mpts_2programs(0, 0);
        let mut out = Vec::new();
        filter.filter_into(&mpts, &mut out);

        // Expected packets in `out`:
        //   * 1 synthetic PAT (replacing the original)
        //   * 1 PMT for program 2 (PID 0x1500)
        //   * 1 ES PID 0x1600
        //   * 1 ES PID 0x1601
        // = 4 × 188-byte packets. Program 1's PMT/ES and the unrelated 0x1700
        // PID must be dropped.
        assert_eq!(out.len(), 4 * TS_PACKET_SIZE);

        // Walk the output and check the PIDs we got.
        let mut pids: Vec<u16> = Vec::new();
        for chunk in out.chunks_exact(TS_PACKET_SIZE) {
            pids.push(ts_pid(chunk));
        }
        assert_eq!(pids, vec![PAT_PID, 0x1500, 0x1600, 0x1601]);
    }

    #[test]
    fn filter_drops_target_program_packets_when_program_absent() {
        let mut filter = TsProgramFilter::new(7); // not present in stream
        let mpts = make_mpts_2programs(0, 0);
        let mut out = Vec::new();
        filter.filter_into(&mpts, &mut out);
        // Nothing should pass through — no PAT, no PMTs, no ES.
        assert!(out.is_empty(), "expected empty output, got {} bytes", out.len());
    }

    #[test]
    fn synthetic_pat_round_trips_through_parse_pat_programs() {
        let mut filter = TsProgramFilter::new(2);
        let mpts = make_mpts_2programs(0, 0);
        let mut out = Vec::new();
        filter.filter_into(&mpts, &mut out);
        assert!(out.len() >= TS_PACKET_SIZE);

        // The first packet in the output is the synthetic PAT.
        let pat_pkt = &out[..TS_PACKET_SIZE];
        let programs = parse_pat_programs(pat_pkt);
        assert_eq!(programs, vec![(2, 0x1500)]);
    }

    /// Reproducer for the production bug: feed the real MPTS file into the
    /// filter in 1316-byte (7 × 188) chunks the way `output_udp.rs` delivers
    /// each broadcast packet, and verify both programs still emit data.
    #[test]
    fn filter_real_mpts_chunked_program_2_emits_data() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testbed/quality/refs/ref_mpts_60s.ts"
        );
        let Ok(data) = std::fs::read(path) else {
            eprintln!("skipping, fixture missing at {path}");
            return;
        };
        const CHUNK: usize = 7 * TS_PACKET_SIZE; // 1316, matches UDP datagram

        for target in [1u16, 2u16] {
            let mut filter = TsProgramFilter::new(target);
            let mut total = Vec::new();
            for chunk in data.chunks(CHUNK) {
                let mut out = Vec::new();
                filter.filter_into(chunk, &mut out);
                total.extend_from_slice(&out);
            }
            assert!(
                !total.is_empty(),
                "chunked filter for program {target} produced 0 bytes from {} byte MPTS",
                data.len()
            );
        }
    }

    /// Real-fixture test: load the testbed reference MPTS file (2 programs)
    /// and run the filter targeting program 2. Reproduces the bug where the
    /// filter produces zero output for any program > 1 against real
    /// ffmpeg-emitted MPTS data.
    #[test]
    fn filter_real_mpts_program_2_emits_data() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testbed/quality/refs/ref_mpts_60s.ts"
        );
        let Ok(data) = std::fs::read(path) else {
            // Fixture missing — skip rather than fail in environments without
            // the testbed checkout.
            eprintln!("skipping real-MPTS test, fixture not found at {path}");
            return;
        };
        // Verify the source PAT contains both programs first.
        // First PAT packet is at offset 188 in this file (after a leading
        // null packet) but a robust scan finds it.
        let mut pat_programs = None;
        for chunk in data.chunks_exact(TS_PACKET_SIZE).take(20) {
            if chunk[0] == TS_SYNC_BYTE && ts_pid(chunk) == PAT_PID && ts_pusi(chunk) {
                pat_programs = Some(parse_pat_programs(chunk));
                break;
            }
        }
        let programs = pat_programs.expect("no PAT in first 20 packets of fixture");
        assert!(
            programs.iter().any(|(n, _)| *n == 1),
            "fixture PAT missing program 1: {programs:?}"
        );
        assert!(
            programs.iter().any(|(n, _)| *n == 2),
            "fixture PAT missing program 2: {programs:?}"
        );

        // Run the filter on the entire file targeting program 2.
        let mut filter = TsProgramFilter::new(2);
        let mut out = Vec::new();
        filter.filter_into(&data, &mut out);
        assert!(
            !out.is_empty(),
            "filter for program 2 produced 0 bytes from a {}-byte real MPTS file containing program 2",
            data.len()
        );

        // Sanity: filter for program 1 also produces data.
        let mut filter1 = TsProgramFilter::new(1);
        let mut out1 = Vec::new();
        filter1.filter_into(&data, &mut out1);
        assert!(!out1.is_empty(), "filter for program 1 also produced 0 bytes");
    }

    #[test]
    fn filter_handles_pat_version_bump_dropping_target() {
        let mut filter = TsProgramFilter::new(2);

        // Round 1: programs {1,2}, target=2 → 4 packets emitted as before.
        let mpts_v0 = make_mpts_2programs(0, 0);
        let mut out = Vec::new();
        filter.filter_into(&mpts_v0, &mut out);
        assert_eq!(out.len(), 4 * TS_PACKET_SIZE);

        // Round 2: PAT bumps to v1 listing only programs {1, 3} — target 2 gone.
        // Build a fresh stream containing the new PAT and some ES of program 1.
        let mut mpts_v1 = Vec::new();
        mpts_v1.extend_from_slice(&build_pat_packet(&[(1, 0x1000), (3, 0x1A00)], false, 1));
        mpts_v1.extend_from_slice(&build_pmt_packet(0x1000, 1, &[(0x1B, 0x1100)], 0));
        mpts_v1.extend_from_slice(&build_es_packet(0x1100, 0));
        // Stale ES packets from program 2 still arriving — should be dropped.
        mpts_v1.extend_from_slice(&build_es_packet(0x1600, 0));
        mpts_v1.extend_from_slice(&build_es_packet(0x1601, 0));

        let mut out2 = Vec::new();
        filter.filter_into(&mpts_v1, &mut out2);
        assert!(out2.is_empty(), "after target program disappears, output must be empty (got {} bytes)", out2.len());
    }
}
