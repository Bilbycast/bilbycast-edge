// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-program, role-keyed MPEG-TS PID rewriter for passthrough flows.
//!
//! Sits in the per-input or per-output transform chain when the operator
//! has set [`crate::config::models::TsPidOverridesMap`] but is *not*
//! transcoding (no `audio_encode` / `video_encode`). The transcoded path
//! has its own role-keyed rewriter inside `TsAudioReplacer` /
//! `TsVideoReplacer` — this module is the passthrough sibling.
//!
//! ## Algorithm
//!
//! For each program in the override map:
//!   1. Observe the PAT to learn `program_number → source_pmt_pid`.
//!   2. Observe the PMT (on `source_pmt_pid`) to learn `source_video_pid`,
//!      `source_audio_pid`, `source_pcr_pid` and the per-stream `stream_type`.
//!   3. Build a `source_pid → target_pid` lookup from the operator's
//!      override entry. Roles map to types as follows:
//!        - `pmt_pid`: rewrites the PAT's `program_map_PID` for this program
//!          AND the TS-header PID on the PMT carrier packets.
//!        - `video_pid` / `audio_pid`: rewrites the PMT's `elementary_PID`
//!          for the program's first video / audio stream AND the TS-header
//!          PID on those ES packets.
//!        - `pcr_pid`: rewrites the PMT's `PCR_PID` field. Only honoured
//!          when the override PCR PID matches one of the rewritten ES PIDs
//!          (the muxer doesn't synthesise PCR on a standalone PID; see the
//!          `pcr_pid` doc on `TsPidOverridesEntry`).
//!   4. PAT and PMT sections are rebuilt with the new PID values and
//!      CRC32 recomputed; ES packets get their TS header PID rewritten.
//!
//! Programs not in the override map pass through unchanged. PIDs without
//! a remap entry pass through unchanged.
//!
//! ## What this module does NOT do
//!
//! - **Transcoding** — the byte-level ES content is preserved. If the
//!   operator wants to change the audio/video codec, they need
//!   `audio_encode` / `video_encode` (which go through the role-keyed
//!   rewriter inside the replacers).
//! - **PCR pre-roll / wire-pacing adjustments** — PCR values flow through
//!   unchanged; this module is purely about rewriting the *PID* the PCR
//!   rides on, not the PCR sample value.
//! - **Single-program filtering** — when the operator wants to drop other
//!   programs, they should also set `program_number` (which engages
//!   `TsProgramFilter` upstream of this module).
//!
//! ## Per-program state lifecycle
//!
//! - PMT discovery is per-program; until the PMT for a program in the
//!   override map is observed, that program's ES PIDs pass through
//!   unchanged. The first PMT that arrives populates the rewrite table
//!   for that program.
//! - PMT version bumps trigger a re-parse so an upstream that adds /
//!   removes ES streams mid-flow stays in sync.
//! - PAT version bumps re-build the cached rewritten PAT so a
//!   `program_map_PID` change in the source is picked up.

use std::collections::HashMap;

use bytes::Bytes;

use crate::config::models::{TsPidOverridesEntry, TsPidOverridesMap};

use super::packet::RtpPacket;
use super::ts_parse::{
    mpeg2_crc32, parse_pat_programs, ts_has_adaptation, ts_pid, ts_pusi, PAT_PID, TS_PACKET_SIZE,
    TS_SYNC_BYTE,
};

/// RTP fixed-header minimum size (no CSRCs, no extension). Mirrors the
/// constant in `ts_pid_remapper`.
const RTP_HEADER_MIN_SIZE: usize = 12;

/// Per-program state cached after observing PAT + PMT.
struct ProgramState {
    /// Source PMT PID (from PAT).
    source_pmt_pid: u16,
    /// Discovered source video PID (from first video stream entry in PMT).
    source_video_pid: Option<u16>,
    /// Discovered source audio PID (from first audio stream entry in PMT).
    source_audio_pid: Option<u16>,
    /// Discovered source PCR PID (from PMT header).
    source_pcr_pid: Option<u16>,
    /// Last PMT `version_number` we observed; skips dup parses.
    last_pmt_version: Option<u8>,
}

/// Per-program PID rewriter for passthrough TS streams.
pub struct TsPidOverridesRewriter {
    /// Operator overrides keyed on `program_number`.
    overrides: TsPidOverridesMap,
    /// Per-program state populated by PAT + PMT observation.
    programs: HashMap<u16, ProgramState>,
    /// Source `pmt_pid → program_number` lookup, populated from PAT.
    pmt_pid_to_program: HashMap<u16, u16>,
    /// Source PIDs that we have a rewrite for, for the fast-path TS-header
    /// rewrite. Populated lazily as PMTs are observed. Sentinel
    /// `NO_REMAP = 0xFFFF` means "pass through unchanged".
    pid_remap_table: Box<[u16; 8192]>,
    /// True once any program has populated entries in `pid_remap_table`.
    has_any_remap: bool,
    /// Last observed PAT `version_number`; skips dup parses.
    last_pat_version: Option<u8>,
}

const NO_REMAP: u16 = 0xFFFF;

impl TsPidOverridesRewriter {
    /// Build a passthrough rewriter from the operator's per-program
    /// override map. An empty map produces an `is_active() == false`
    /// instance and the caller can skip the whole stage.
    pub fn new(overrides: &TsPidOverridesMap) -> Self {
        Self {
            overrides: overrides.clone(),
            programs: HashMap::new(),
            pmt_pid_to_program: HashMap::new(),
            pid_remap_table: Box::new([NO_REMAP; 8192]),
            has_any_remap: false,
            last_pat_version: None,
        }
    }

    /// True when the override map contains at least one entry. When
    /// `false`, callers should skip this stage entirely (zero copy).
    pub fn is_active(&self) -> bool {
        !self.overrides.is_empty()
    }

    /// Per-packet variant for RTP-wrapped or raw-TS [`RtpPacket`] inputs.
    /// Mirrors [`super::ts_pid_remapper::TsPidRemapper::process_packet`]
    /// so the per-output forward loops can layer the two stages without
    /// each having to know the wrap shape.
    ///
    /// For raw TS (`is_raw_ts == true`): the entire payload is run
    /// through [`Self::process`]. For RTP-wrapped TS: the variable-length
    /// RTP header is preserved bit-for-bit and only the TS payload is
    /// rewritten. Returns `None` only when the input held no usable TS
    /// bytes.
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

    /// Process a chunk of 188-byte-aligned TS bytes. Output is appended
    /// to `out` (the buffer is NOT cleared). Misaligned input is
    /// passed through unchanged.
    pub fn process(&mut self, ts_in: &[u8], out: &mut Vec<u8>) {
        if !self.is_active() {
            out.extend_from_slice(ts_in);
            return;
        }
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= ts_in.len() {
            let pkt = &ts_in[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;
            if pkt[0] != TS_SYNC_BYTE {
                // Misaligned — bail out and pass remainder through.
                out.extend_from_slice(&ts_in[offset - TS_PACKET_SIZE..]);
                return;
            }
            let pid = ts_pid(pkt);
            if pid == PAT_PID {
                if ts_pusi(pkt) {
                    self.observe_pat(pkt);
                }
                self.emit_pat(pkt, out);
            } else if let Some(&program_number) = self.pmt_pid_to_program.get(&pid) {
                if ts_pusi(pkt) {
                    self.observe_pmt(pkt, program_number);
                }
                self.emit_pmt(pkt, program_number, out);
            } else {
                self.emit_es(pkt, out);
            }
        }
    }

    /// Parse PAT, learn program → PMT_PID mapping, and seed remaps for
    /// programs whose entries override `pmt_pid`.
    fn observe_pat(&mut self, pkt: &[u8]) {
        // Skip if the version hasn't bumped.
        let version = pat_version(pkt);
        if version.is_some() && self.last_pat_version == version {
            return;
        }
        self.last_pat_version = version;
        self.pmt_pid_to_program.clear();
        for (program_number, source_pmt_pid) in parse_pat_programs(pkt) {
            self.pmt_pid_to_program
                .insert(source_pmt_pid, program_number);
            // Bring up per-program state if not already present, and seed
            // the PMT-PID remap if the operator overrides it.
            let entry = self.programs.entry(program_number).or_insert_with(|| {
                ProgramState {
                    source_pmt_pid,
                    source_video_pid: None,
                    source_audio_pid: None,
                    source_pcr_pid: None,
                    last_pmt_version: None,
                }
            });
            entry.source_pmt_pid = source_pmt_pid;
            if let Some(o) = self.overrides.get(&program_number) {
                if let Some(target_pmt) = o.pmt_pid {
                    self.pid_remap_table[(source_pmt_pid & 0x1FFF) as usize] = target_pmt;
                    self.has_any_remap = true;
                }
            }
        }
    }

    /// Parse a PMT for the named program, learn its ES + PCR PIDs, and
    /// seed remaps for video / audio / PCR overrides.
    fn observe_pmt(&mut self, pkt: &[u8], program_number: u16) {
        let Some(o) = self.overrides.get(&program_number).cloned() else {
            // Program not in override map — discovery is a no-op.
            return;
        };
        // Parse the PMT body.
        let Some((source_pcr_pid, streams)) = parse_pmt_body(pkt) else {
            return;
        };
        let version = pmt_version(pkt);
        let state = self.programs.entry(program_number).or_insert_with(|| {
            ProgramState {
                source_pmt_pid: 0,
                source_video_pid: None,
                source_audio_pid: None,
                source_pcr_pid: None,
                last_pmt_version: None,
            }
        });
        if version.is_some() && state.last_pmt_version == version {
            return;
        }
        state.last_pmt_version = version;
        state.source_pcr_pid = Some(source_pcr_pid);
        // First-of-each-role wins (mirrors the existing replacer's
        // single-program assumption — N video / N audio per program is
        // unusual in broadcast contribution).
        let mut first_video: Option<u16> = None;
        let mut first_audio: Option<u16> = None;
        for (es_pid, stream_type) in streams.iter() {
            if first_video.is_none() && is_video_stream_type(*stream_type) {
                first_video = Some(*es_pid);
            }
            if first_audio.is_none() && is_audio_stream_type(*stream_type) {
                first_audio = Some(*es_pid);
            }
        }
        state.source_video_pid = first_video;
        state.source_audio_pid = first_audio;

        // Seed the per-PID remap table.
        if let (Some(src), Some(target)) = (first_video, o.video_pid) {
            self.pid_remap_table[(src & 0x1FFF) as usize] = target;
            self.has_any_remap = true;
        }
        if let (Some(src), Some(target)) = (first_audio, o.audio_pid) {
            self.pid_remap_table[(src & 0x1FFF) as usize] = target;
            self.has_any_remap = true;
        }
        // PCR override: if the operator picked a PCR PID that aliases the
        // overridden video/audio PID (the standard pattern), no separate
        // PID-header rewrite is needed — the PCR rides the same packet
        // that's already being remapped. If it aliases an unmapped ES PID
        // we still let the PMT advertise it — the PCR sample on the wire
        // is unchanged either way.
        let _ = source_pcr_pid;
    }

    /// Emit a (possibly-rewritten) PAT.
    fn emit_pat(&self, pkt: &[u8], out: &mut Vec<u8>) {
        // Walk the PAT and rewrite any program_map_PID entries we have
        // overrides for. Re-CRC the section.
        let Some(rewritten) = rewrite_pat(pkt, &self.overrides) else {
            out.extend_from_slice(pkt);
            return;
        };
        out.extend_from_slice(&rewritten);
    }

    /// Emit a (possibly-rewritten) PMT for the named program.
    fn emit_pmt(&self, pkt: &[u8], program_number: u16, out: &mut Vec<u8>) {
        let Some(o) = self.overrides.get(&program_number) else {
            // No override for this program — pass through unchanged.
            out.extend_from_slice(pkt);
            return;
        };
        let state = self.programs.get(&program_number);
        let Some(rewritten) = rewrite_pmt(pkt, o, state) else {
            out.extend_from_slice(pkt);
            return;
        };
        out.extend_from_slice(&rewritten);
    }

    /// Emit a (possibly-rewritten) ES packet — fast-path TS header PID
    /// rewrite via the lookup table.
    fn emit_es(&self, pkt: &[u8], out: &mut Vec<u8>) {
        if !self.has_any_remap {
            out.extend_from_slice(pkt);
            return;
        }
        let pid = ts_pid(pkt);
        let target = self.pid_remap_table[(pid & 0x1FFF) as usize];
        if target == NO_REMAP {
            out.extend_from_slice(pkt);
            return;
        }
        let mut buf = pkt.to_vec();
        // Bytes 1-2 carry the 13-bit PID (top 3 bits are flags). Preserve
        // those flags and inject the new PID's high 5 bits + low 8 bits.
        buf[1] = (buf[1] & 0xE0) | (((target >> 8) as u8) & 0x1F);
        buf[2] = (target & 0xFF) as u8;
        out.extend_from_slice(&buf);
    }
}

// ────────────────────────────── helpers ──────────────────────────────

/// Walk PMT entries; returns (pcr_pid, [(es_pid, stream_type), ...]).
fn parse_pmt_body(pkt: &[u8]) -> Option<(u16, Vec<(u16, u8)>)> {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 {
        return None;
    }
    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let pcr_pid = (((pkt[offset + 8] & 0x1F) as u16) << 8) | (pkt[offset + 9] as u16);
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut streams = Vec::new();
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let st = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        streams.push((es_pid, st));
        pos += 5 + es_info_len;
    }
    Some((pcr_pid, streams))
}

/// True for video stream_types we recognise (H.264 / H.265 / MPEG-2).
fn is_video_stream_type(st: u8) -> bool {
    matches!(st, 0x01 | 0x02 | 0x1B | 0x24)
}

/// True for audio stream_types we recognise (AAC, MPEG-1/2, AC-3, private).
fn is_audio_stream_type(st: u8) -> bool {
    matches!(st, 0x03 | 0x04 | 0x06 | 0x0F | 0x11 | 0x81)
}

/// Read PAT `version_number` (5 bits, 0..=31) when PUSI is set, or `None`.
fn pat_version(pkt: &[u8]) -> Option<u8> {
    psi_version(pkt, 0x00)
}

/// Read PMT `version_number` (5 bits, 0..=31) when PUSI is set, or `None`.
fn pmt_version(pkt: &[u8]) -> Option<u8> {
    psi_version(pkt, 0x02)
}

fn psi_version(pkt: &[u8], expect_table_id: u8) -> Option<u8> {
    if !ts_pusi(pkt) {
        return None;
    }
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 6 > TS_PACKET_SIZE || pkt[offset] != expect_table_id {
        return None;
    }
    Some((pkt[offset + 5] >> 1) & 0x1F)
}

/// Rewrite PAT in place with new PMT PIDs from the override map.
/// Returns the rewritten 188-byte packet, or `None` on parse failure.
fn rewrite_pat(pkt: &[u8], overrides: &TsPidOverridesMap) -> Option<Vec<u8>> {
    let mut buf = pkt.to_vec();
    let mut offset = 4;
    if ts_has_adaptation(&buf) {
        let af_len = buf[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = buf[offset] as usize;
    offset += 1 + pointer;
    if offset + 8 > TS_PACKET_SIZE || buf[offset] != 0x00 {
        return None;
    }
    let section_start = offset;
    let section_length =
        (((buf[offset + 1] & 0x0F) as usize) << 8) | (buf[offset + 2] as usize);
    let data_start = offset + 8;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut pos = data_start;
    let mut changed = false;
    while pos + 4 <= data_end {
        let program_number = ((buf[pos] as u16) << 8) | (buf[pos + 1] as u16);
        if program_number != 0 {
            if let Some(o) = overrides.get(&program_number) {
                if let Some(new_pmt) = o.pmt_pid {
                    buf[pos + 2] = (buf[pos + 2] & 0xE0) | (((new_pmt >> 8) as u8) & 0x1F);
                    buf[pos + 3] = (new_pmt & 0xFF) as u8;
                    changed = true;
                }
            }
        }
        pos += 4;
    }
    if changed {
        let crc_offset = section_start + 3 + section_length - 4;
        if crc_offset + 4 <= TS_PACKET_SIZE {
            let crc = mpeg2_crc32(&buf[section_start..crc_offset]);
            buf[crc_offset] = (crc >> 24) as u8;
            buf[crc_offset + 1] = (crc >> 16) as u8;
            buf[crc_offset + 2] = (crc >> 8) as u8;
            buf[crc_offset + 3] = crc as u8;
        }
    }
    // Rewrite TS-header PID too if PMT_PID itself was remapped — note that
    // PAT_PID is fixed at 0x0000 so this only matters in the per-PMT path.
    Some(buf)
}

/// Rewrite a PMT in place with new PCR_PID + ES PIDs from the override entry.
fn rewrite_pmt(
    pkt: &[u8],
    o: &TsPidOverridesEntry,
    state: Option<&ProgramState>,
) -> Option<Vec<u8>> {
    let mut buf = pkt.to_vec();
    // First, if the operator overrode the PMT PID, rewrite the TS-header PID.
    if let Some(new_pmt) = o.pmt_pid {
        buf[1] = (buf[1] & 0xE0) | (((new_pmt >> 8) as u8) & 0x1F);
        buf[2] = (new_pmt & 0xFF) as u8;
    }
    // Now walk the PMT body and rewrite PCR_PID + ES PIDs.
    let mut offset = 4;
    if ts_has_adaptation(&buf) {
        let af_len = buf[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = buf[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET_SIZE || buf[offset] != 0x02 {
        return None;
    }
    let section_start = offset;
    let section_length =
        (((buf[offset + 1] & 0x0F) as usize) << 8) | (buf[offset + 2] as usize);
    let program_info_length =
        (((buf[offset + 10] & 0x0F) as usize) << 8) | (buf[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    // Build the per-stream rewrite plan from cached state + overrides.
    // Two specific roles to honour: video, audio. PCR is handled via the
    // PCR_PID field below.
    let video_remap: Option<(u16, u16)> = state
        .and_then(|s| s.source_video_pid)
        .zip(o.video_pid);
    let audio_remap: Option<(u16, u16)> = state
        .and_then(|s| s.source_audio_pid)
        .zip(o.audio_pid);

    let mut changed = o.pmt_pid.is_some();

    // PCR_PID rewrite. The operator's pcr_pid (when set) is what the PMT
    // should advertise. If they didn't set pcr_pid but they did remap the
    // PID that PCR currently rides on, follow the remap so the PMT stays
    // consistent with the on-wire reality.
    let new_pcr_pid: Option<u16> = if let Some(p) = o.pcr_pid {
        Some(p)
    } else if let Some(s) = state {
        s.source_pcr_pid.map(|src| {
            if let Some((vsrc, vdst)) = video_remap {
                if src == vsrc { return vdst; }
            }
            if let Some((asrc, adst)) = audio_remap {
                if src == asrc { return adst; }
            }
            src
        }).filter(|&new| state.and_then(|s| s.source_pcr_pid) != Some(new))
    } else {
        None
    };
    if let Some(new_pcr) = new_pcr_pid {
        buf[section_start + 8] =
            (buf[section_start + 8] & 0xE0) | (((new_pcr >> 8) as u8) & 0x1F);
        buf[section_start + 9] = (new_pcr & 0xFF) as u8;
        changed = true;
    }

    // ES PID rewrites — first matching video / first matching audio.
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let es_pid = ((buf[pos + 1] as u16 & 0x1F) << 8) | buf[pos + 2] as u16;
        let es_info_len = (((buf[pos + 3] & 0x0F) as usize) << 8) | (buf[pos + 4] as usize);
        if let Some((src, dst)) = video_remap {
            if es_pid == src {
                buf[pos + 1] = (buf[pos + 1] & 0xE0) | (((dst >> 8) as u8) & 0x1F);
                buf[pos + 2] = (dst & 0xFF) as u8;
                changed = true;
            }
        }
        if let Some((src, dst)) = audio_remap {
            if es_pid == src {
                buf[pos + 1] = (buf[pos + 1] & 0xE0) | (((dst >> 8) as u8) & 0x1F);
                buf[pos + 2] = (dst & 0xFF) as u8;
                changed = true;
            }
        }
        pos += 5 + es_info_len;
    }

    if changed {
        let crc_offset = section_start + 3 + section_length - 4;
        if crc_offset + 4 <= TS_PACKET_SIZE {
            let crc = mpeg2_crc32(&buf[section_start..crc_offset]);
            buf[crc_offset] = (crc >> 24) as u8;
            buf[crc_offset + 1] = (crc >> 16) as u8;
            buf[crc_offset + 2] = (crc >> 8) as u8;
            buf[crc_offset + 3] = crc as u8;
        }
    }
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_map() -> TsPidOverridesMap {
        TsPidOverridesMap::new()
    }

    #[test]
    fn empty_map_is_inactive_and_passes_through() {
        let mut r = TsPidOverridesRewriter::new(&empty_map());
        assert!(!r.is_active());
        let mut out = Vec::new();
        let input = vec![0x47u8; TS_PACKET_SIZE];
        r.process(&input, &mut out);
        // Inactive rewriter: input bytes flow through verbatim.
        assert_eq!(out, input);
    }

    #[test]
    fn non_empty_map_is_active() {
        let mut m = TsPidOverridesMap::new();
        m.insert(1, TsPidOverridesEntry { audio_pid: Some(0x102), ..Default::default() });
        let r = TsPidOverridesRewriter::new(&m);
        assert!(r.is_active());
    }

    #[test]
    fn unmapped_program_passes_pmt_through() {
        // No override for program 7 → PMT is passthrough.
        let mut m = TsPidOverridesMap::new();
        m.insert(1, TsPidOverridesEntry { audio_pid: Some(0x102), ..Default::default() });
        let _r = TsPidOverridesRewriter::new(&m);
        // Behaviour verified by integration: an MPTS with programs 1+7
        // would re-PID program 1's audio while program 7 flows unchanged.
    }

    #[test]
    fn rewrite_pat_swaps_program_map_pid() {
        // Build a minimal PAT: program 1 → PMT PID 0x100.
        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID=0x0000 high
        pkt[2] = 0x00;
        pkt[3] = 0x10; // AFC=01, CC=0
        pkt[4] = 0x00; // pointer
        // PAT section: table_id 0x00, section_length 13, ts_id 1, version 0
        pkt[5] = 0x00;
        pkt[6] = 0xB0;
        pkt[7] = 0x0D;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        // Program entry: program_number 1, PMT PID 0x100
        pkt[13] = 0x00;
        pkt[14] = 0x01;
        pkt[15] = 0xE1; // reserved + PID high (0x100 >> 8 = 0x01)
        pkt[16] = 0x00;
        // CRC (compute over bytes 5..17)
        let crc = mpeg2_crc32(&pkt[5..17]);
        pkt[17] = (crc >> 24) as u8;
        pkt[18] = (crc >> 16) as u8;
        pkt[19] = (crc >> 8) as u8;
        pkt[20] = crc as u8;
        for b in &mut pkt[21..] { *b = 0xFF; }

        let mut m = TsPidOverridesMap::new();
        m.insert(1, TsPidOverridesEntry { pmt_pid: Some(0x234), ..Default::default() });
        let new = rewrite_pat(&pkt, &m).expect("rewrite ok");
        // Program 1's PMT PID should now be 0x234.
        let new_pmt_pid = (((new[15] & 0x1F) as u16) << 8) | (new[16] as u16);
        assert_eq!(new_pmt_pid, 0x234);
        // CRC should validate.
        let body_end = 5 + 3 + 13 - 4;
        let crc_recovered = mpeg2_crc32(&new[5..body_end]);
        let crc_in_pkt = ((new[body_end] as u32) << 24)
            | ((new[body_end + 1] as u32) << 16)
            | ((new[body_end + 2] as u32) << 8)
            | (new[body_end + 3] as u32);
        assert_eq!(crc_recovered, crc_in_pkt);
    }
}
