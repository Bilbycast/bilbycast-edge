// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Muxer-mode TS clock rewriter — regenerates **PCR + PES PTS/DTS**
//! per the industry-standard remux model (Sencore RMX, Cobalt 9970-MX,
//! Cisco D9036, Imagine Selenio MX, Appear X10 mux mode).
//!
//! ## What it does
//!
//! Treats one TS stream as a muxer would: tracks the source's PCR
//! sequence, regenerates output PCR + PES PTS values from the
//! per-flow master clock, preserves the source's PCR→PTS pre-roll
//! delta naturally, and **absorbs source-side discontinuities**
//! (file-loop wraps, encoder restarts, SCTE-35 splice clocks) so the
//! receiver sees one continuous monotonic clock.
//!
//! ## Why
//!
//! ETR 290 §5.7 PCR_AC compliance (≤ 500 ns tier 1) is impossible to
//! deliver via passthrough on a packet network — source-side network
//! jitter alone blows the budget. The only way to deliver tier-1
//! PCR_AC at the output is to regenerate PCR locally from a paced
//! clock. Every professional broadcast multiplexer does this; treating
//! passthrough as the default leaves bilbycast-edge a relay rather
//! than a contribution gateway.
//!
//! Loop-wraps on file-replay sources (`ffmpeg -stream_loop`, the
//! `media_player` input) used to propagate as PCR discontinuities to
//! the receiver. Marked with DI=1 but still glitched VLC and certain
//! hardware decoders. Muxer mode eliminates the discontinuity entirely
//! — no glitch, no DI=1 needed for that case.
//!
//! ## Algorithm
//!
//! One shared `ClockAnchor` per rewriter instance. PCR establishes
//! the anchor; subsequent PCRs and all PES PTS/DTS values are
//! computed off the same anchor so the PCR→PTS pre-roll delta and
//! relative timing are preserved exactly.
//!
//! ```text
//! On first PCR observed:
//!     anchor.src_27mhz = src_pcr
//!     anchor.out_27mhz = master.now_27mhz() − PCR_PREROLL_27MHZ
//!     anchor.established = true
//!     last_src_pcr = src_pcr
//!     last_master  = master.now_27mhz()
//!
//! On every subsequent PCR:
//!     delta_src = src_pcr − last_src_pcr      (33-bit wrap-safe)
//!     if |delta_src| > 500 ms in 27 MHz:
//!         # Source discontinuity (loop wrap, splice, encoder restart)
//!         # Bridge using master-clock elapsed time so the receiver
//!         # sees a continuous monotonic PCR instead of a backward
//!         # jump. The bridge value is what real wallclock elapsed
//!         # between observations, which for an instantaneous loop
//!         # wrap is ~ms — so the output PCR effectively pauses for
//!         # one packet then resumes, no visible glitch.
//!         out_pcr_at_last = anchor.out_27mhz + (last_src_pcr − anchor.src_27mhz)
//!         delta_master    = master.now_27mhz() − last_master
//!         anchor.src_27mhz = src_pcr
//!         anchor.out_27mhz = out_pcr_at_last + delta_master
//!     out_pcr = anchor.out_27mhz + (src_pcr − anchor.src_27mhz)
//!     last_src_pcr = src_pcr
//!     last_master  = master.now_27mhz()
//!
//! On every PES PTS / DTS:
//!     out_pts_27mhz = anchor.out_27mhz + (src_pts × 300 − anchor.src_27mhz)
//!     out_pts_90k   = (out_pts_27mhz / 300) & 0x1_FFFF_FFFF
//!     (lipsync_offset_90k added on audio PIDs)
//!     out_dts preserves source PTS−DTS delta naturally because both
//!     are anchored against the same src_27mhz.
//! ```
//!
//! ## Properties
//!
//! - **Monotonic by construction** — output PCR + PTS only advance.
//! - **Rate-preserving** — output PCR rate = source PCR rate during
//!   continuous segments.
//! - **PCR→PTS delta preserved** — output stream's apparent T-STD
//!   buffer pre-roll matches source's. Receivers compute buffer fill
//!   the same way they did against source.
//! - **DTS reorder preserved** — H.264 / HEVC B-frames decode correctly.
//! - **Loop-wrap absorbed** — discontinuity bridged with real elapsed
//!   wallclock; receiver sees no jump.
//! - **No safety-check fallback** — the algorithm uses master clock
//!   only for *deltas* (not absolute values), so it works regardless
//!   of how master and source absolute values compare.
//!
//! ## Caveats
//!
//! - **Per-input anchor** — each input's rewriter has its own
//!   `ClockAnchor`. For passthrough flows (one active input at a
//!   time) this is fine. For assembled (PID-bus) flows where
//!   multiple inputs contribute ES, per-input anchors may produce
//!   PES PTS values on different timelines, which complicates
//!   cross-input PES-aligned splice. Recommendation: leave
//!   `passthrough_clock: true` on inputs feeding assembled flows
//!   until the assembler grows its own muxer-mode rewriter (Phase 9).
//! - **Late-joining inputs** — an input added to a flow long after
//!   start anchors at a different master_now value. Cross-input
//!   PES-aligned splice expects roughly-coherent PTS values across
//!   inputs; late-joiners may need a flow restart to align.
//! - **PCR repetition rate** (TR 101 290 §PCR_RR ≤ 40 ms): not
//!   enforced. If the source emits PCR sparsely, the output is
//!   sparse too. A future enhancement could inject PCR padding when
//!   the inter-PCR gap exceeds 40 ms.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::av_sync_mux::AvSyncPacer;
use super::ts_parse::{
    extract_pcr, extract_pes_dts, extract_pes_pts, parse_pat_programs, ts_has_adaptation, ts_pid,
    ts_pusi, PAT_PID, TS_PACKET_SIZE, TS_SYNC_BYTE,
};

/// PCR pre-roll in 27 MHz ticks. Matches `av_sync_mux::PCR_PREROLL_27MHZ`.
pub const PCR_PREROLL_27MHZ: u64 = 2_160_000;

/// Source-PCR jump above this triggers a discontinuity bridge.
/// 500 ms in 27 MHz ticks. Matches the threshold used by the audio
/// replacer and the wire pacer.
const DISCONTINUITY_THRESHOLD_27MHZ: u64 = 500 * 27_000;

/// 42-bit PCR space modulus (33-bit base × 300).
const PCR_MODULUS_27MHZ: u64 = (1u64 << 33) * 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PidRole {
    Audio,
    Video,
    Other,
}

/// Classify an MPEG-TS `stream_type` into audio/video/other for the
/// purpose of routing lipsync trim (audio PIDs only).
///
/// Per ISO/IEC 13818-1 Table 2-34, ATSC A/53, DVB EN 300 468, and
/// SCTE 35. `0x06` (private/PES — often AC-3 with DVB descriptor
/// 0x6A / E-AC-3 with 0x7A) falls through as `Other` because we don't
/// parse ES descriptors here; the audio path handles this via the
/// PMT-side `registration_descriptor` logic in `ts_audio_replace`.
fn classify_stream_type(stream_type: u8) -> PidRole {
    match stream_type {
        // Video
        0x01 | 0x02 | 0x10 | 0x1B | 0x20 | 0x21 | 0x24 | 0x42 | 0x52 | 0xD1 => PidRole::Video,
        // Audio
        0x03 | 0x04 | 0x0F | 0x11 | 0x1C | 0x80 | 0x81 | 0x82 | 0x83 | 0x84 | 0x85 | 0x86
        | 0x87 | 0x88 | 0xC1 | 0xC2 => PidRole::Audio,
        _ => PidRole::Other,
    }
}

/// Per-rewriter shared clock anchor. Established on the first
/// observed PCR; subsequent PCRs and all PES PTS/DTS values are
/// computed off this anchor.
#[derive(Default)]
struct ClockAnchor {
    /// Source 27 MHz value at the anchor point (first PCR after
    /// (re-)establishment).
    src_27mhz: u64,
    /// Master-clock 27 MHz value at the anchor point.
    out_27mhz: u64,
    /// Last observed source PCR (27 MHz) — for discontinuity detection.
    last_src_pcr_27mhz: u64,
    /// Last observed master clock (27 MHz) at the moment of last PCR —
    /// for discontinuity bridge sizing.
    last_master_27mhz: u64,
    established: bool,
}

/// PCR + PES PTS/DTS rewriter — single instance per input, holds an
/// `Arc<AvSyncPacer>` and a shared clock anchor.
pub struct TsPtsRewriter {
    pacer: Arc<AvSyncPacer>,
    /// PMT PIDs from the most recently observed PAT.
    pmt_pids: HashSet<u16>,
    /// PCR PIDs discovered from the most recently observed PMT(s).
    pcr_pids: HashSet<u16>,
    /// ES PID → role classification from the most recently observed PMT.
    pid_role: HashMap<u16, PidRole>,
    /// Shared clock anchor across PCR + all PES PTS/DTS on this input.
    anchor: ClockAnchor,
    /// Version-gate the PAT parse.
    last_pat_version: Option<u8>,
    /// Per-PMT-PID version-gate.
    last_pmt_versions: HashMap<u16, u8>,
}

impl TsPtsRewriter {
    pub fn new(pacer: Arc<AvSyncPacer>) -> Self {
        Self {
            pacer,
            pmt_pids: HashSet::new(),
            pcr_pids: HashSet::new(),
            pid_role: HashMap::new(),
            anchor: ClockAnchor::default(),
            last_pat_version: None,
            last_pmt_versions: HashMap::new(),
        }
    }

    /// Process a chunk of 188-byte-aligned TS bytes. Appends rewritten
    /// output to `out` (caller is responsible for any prior clear).
    /// Output length always equals input length: PCR (6B in AF) and
    /// PES PTS/DTS (5B in PES header) are fixed-size fields rewritten
    /// in place without changing packet boundaries.
    pub fn process(&mut self, ts_in: &[u8], out: &mut Vec<u8>) {
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= ts_in.len() {
            let pkt = &ts_in[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if pkt[0] != TS_SYNC_BYTE {
                // Out-of-sync byte — pass through; upstream framing is
                // probably wrong and we don't want to drop the packet.
                out.extend_from_slice(pkt);
                continue;
            }

            let pid = ts_pid(pkt);

            if pid == PAT_PID && ts_pusi(pkt) {
                self.observe_pat(pkt);
            } else if self.pmt_pids.contains(&pid) && ts_pusi(pkt) {
                self.observe_pmt(pkt);
            }

            // Rewrite PCR if this packet carries one on a learned PCR_PID
            // (or any PID before PMT is learned — first-PCR establishes
            // anchor regardless).
            let mut buf = [0u8; TS_PACKET_SIZE];
            let mut rewritten = false;

            if let Some(src_pcr) = extract_pcr(pkt) {
                // Only rewrite PCRs on learned PCR_PIDs once PMT is
                // observed. Before PMT, treat every PCR as anchor candidate
                // (covers initial sync window).
                let is_pcr_pid =
                    self.pcr_pids.is_empty() || self.pcr_pids.contains(&pid);
                if is_pcr_pid {
                    buf.copy_from_slice(pkt);
                    let new_pcr = self.rewrite_pcr_value(src_pcr);
                    if write_pcr_field_in_packet(&mut buf, new_pcr).is_some() {
                        rewritten = true;
                    }
                }
            }

            // Rewrite PES PTS/DTS on PUSI packets (only on classified
            // audio/video PIDs after PMT is learned).
            if ts_pusi(pkt) {
                let role = self.pid_role.get(&pid).copied().unwrap_or(PidRole::Other);
                if matches!(role, PidRole::Audio | PidRole::Video) {
                    if let Some(src_pts) = extract_pes_pts(pkt) {
                        if !rewritten {
                            buf.copy_from_slice(pkt);
                        }
                        let src_dts = extract_pes_dts(pkt);
                        let (new_pts, new_dts) = self.rewrite_pes_values(
                            src_pts,
                            src_dts,
                            matches!(role, PidRole::Audio),
                        );
                        if write_pes_timestamps(&mut buf, new_pts, new_dts).is_some() {
                            rewritten = true;
                        } else if rewritten {
                            // PCR-only rewrite already happened; keep
                            // buf as-is.
                        } else {
                            // Neither PCR nor PES write succeeded — emit
                            // source bytes unchanged.
                        }
                    }
                }
            }

            if rewritten {
                out.extend_from_slice(&buf);
            } else {
                out.extend_from_slice(pkt);
            }
        }
    }

    /// Compute output PCR from input PCR using the shared anchor.
    /// First PCR establishes the anchor; subsequent PCRs use anchor +
    /// source-delta. >500 ms source jump → re-anchor with master delta
    /// bridge so output stays monotonic.
    fn rewrite_pcr_value(&mut self, src_pcr_27mhz: u64) -> u64 {
        let master_now = self.pacer.now_27mhz();

        if !self.anchor.established {
            self.anchor.src_27mhz = src_pcr_27mhz;
            self.anchor.out_27mhz = master_now.wrapping_sub(PCR_PREROLL_27MHZ);
            self.anchor.last_src_pcr_27mhz = src_pcr_27mhz;
            self.anchor.last_master_27mhz = master_now;
            self.anchor.established = true;
            return self.anchor.out_27mhz % PCR_MODULUS_27MHZ;
        }

        let delta_src = (src_pcr_27mhz as i64).wrapping_sub(self.anchor.last_src_pcr_27mhz as i64);

        // Industry-standard remux discontinuity handling:
        //
        // - **Backward jumps** (delta_src ≤ -500 ms): bridge with master
        //   elapsed so output PCR stays monotonic. PCR going backward
        //   is a clock fault to every receiver; we must never propagate
        //   it. Real source discontinuities (encoder restart, splice
        //   insertion, loop wrap on a source that resets PCR to file
        //   start) hit this path.
        //
        // - **Forward jumps** (delta_src > +500 ms): pass through.
        //   Forward PCR jumps are tolerated by receivers (with DI=1
        //   flag they re-anchor cleanly; without it they may glitch
        //   briefly but recover). Most "forward jumps" we see are
        //   actually file-loop boundaries (`ffmpeg -stream_loop`),
        //   SCTE-35 splice points, or live content edit points —
        //   passing them through preserves output rate accuracy
        //   (PCR_FO ≤ ±30 ppm per TR 101 290). Truncating them with
        //   a master-clock bridge would accumulate negative rate
        //   drift across each jump (one-way), violating PCR_FO.
        //
        // - **Continuous segment** (|delta_src| ≤ 500 ms): anchor stays
        //   put, source-delta drives output. Output rate = source rate
        //   exactly during continuous play.
        if delta_src < -(DISCONTINUITY_THRESHOLD_27MHZ as i64) {
            let out_at_last = self.anchor.out_27mhz.wrapping_add(
                self.anchor.last_src_pcr_27mhz.wrapping_sub(self.anchor.src_27mhz),
            );
            let delta_master = master_now.wrapping_sub(self.anchor.last_master_27mhz);
            self.anchor.src_27mhz = src_pcr_27mhz;
            self.anchor.out_27mhz = out_at_last.wrapping_add(delta_master);
            tracing::info!(
                src_pcr_27mhz,
                delta_src_27mhz = delta_src,
                delta_master_27mhz = delta_master,
                "ts_pts_rewriter: backward PCR discontinuity bridged"
            );
        }

        self.anchor.last_src_pcr_27mhz = src_pcr_27mhz;
        self.anchor.last_master_27mhz = master_now;

        let out_27mhz = self
            .anchor
            .out_27mhz
            .wrapping_add(src_pcr_27mhz.wrapping_sub(self.anchor.src_27mhz));
        out_27mhz % PCR_MODULUS_27MHZ
    }

    /// Compute output PES PTS (and DTS if present) from input values
    /// using the shared anchor. Preserves source PCR→PTS pre-roll and
    /// PTS→DTS delta naturally because both are anchored on
    /// `anchor.src_27mhz`.
    fn rewrite_pes_values(
        &self,
        src_pts_90k: u64,
        src_dts_90k: Option<u64>,
        is_audio: bool,
    ) -> (u64, Option<u64>) {
        if !self.anchor.established {
            // No PCR seen yet — pass through. Real broadcast streams
            // have PCR ≤ 40 ms, so this is brief at flow start.
            return (src_pts_90k, src_dts_90k);
        }

        let lipsync_90k = if is_audio {
            self.pacer.lipsync_offset_90k()
        } else {
            0
        };

        let new_pts = compute_anchored_value(&self.anchor, src_pts_90k, lipsync_90k);
        let new_dts = src_dts_90k.map(|d| compute_anchored_value(&self.anchor, d, lipsync_90k));
        (new_pts, new_dts)
    }

    fn observe_pat(&mut self, pkt: &[u8]) {
        let mut sec_off: usize = 4;
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
        let new_pmt_pids: HashSet<u16> = programs.into_iter().map(|(_, p)| p).collect();
        let lost: Vec<u16> = self.pmt_pids.difference(&new_pmt_pids).copied().collect();
        for pid in lost {
            self.last_pmt_versions.remove(&pid);
        }
        self.pmt_pids = new_pmt_pids;
    }

    fn observe_pmt(&mut self, pkt: &[u8]) {
        let mut sec_off: usize = 4;
        if ts_has_adaptation(pkt) {
            let af_len = pkt[4] as usize;
            sec_off = 5 + af_len;
        }
        if sec_off >= TS_PACKET_SIZE {
            return;
        }
        let pointer = pkt[sec_off] as usize;
        sec_off += 1 + pointer;
        if sec_off + 12 > TS_PACKET_SIZE {
            return;
        }
        if pkt[sec_off] != 0x02 {
            return;
        }
        let version = (pkt[sec_off + 5] >> 1) & 0x1F;
        let pmt_pid = ts_pid(pkt);
        if self.last_pmt_versions.get(&pmt_pid) == Some(&version) {
            return;
        }
        self.last_pmt_versions.insert(pmt_pid, version);

        let section_length =
            (((pkt[sec_off + 1] & 0x0F) as usize) << 8) | (pkt[sec_off + 2] as usize);
        let data_end = (sec_off + 3 + section_length)
            .min(TS_PACKET_SIZE)
            .saturating_sub(4);
        // PCR_PID at bytes sec_off+8..10 (13-bit, top 3 reserved).
        let pcr_pid = (((pkt[sec_off + 8] & 0x1F) as u16) << 8) | (pkt[sec_off + 9] as u16);
        if pcr_pid != 0x1FFF {
            self.pcr_pids.insert(pcr_pid);
        }
        let program_info_length =
            (((pkt[sec_off + 10] & 0x0F) as usize) << 8) | (pkt[sec_off + 11] as usize);
        let mut pos = sec_off + 12 + program_info_length;
        while pos + 5 <= data_end {
            let stream_type = pkt[pos];
            let es_pid = (((pkt[pos + 1] & 0x1F) as u16) << 8) | (pkt[pos + 2] as u16);
            let es_info_length =
                (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
            self.pid_role
                .insert(es_pid, classify_stream_type(stream_type));
            pos += 5 + es_info_length;
        }
    }
}

/// Anchored value computation — shared by PES PTS, DTS, and PCR paths.
fn compute_anchored_value(anchor: &ClockAnchor, src_pts_90k: u64, lipsync_90k: i64) -> u64 {
    let src_pts_27mhz = src_pts_90k.wrapping_mul(300);
    let delta_27mhz = src_pts_27mhz.wrapping_sub(anchor.src_27mhz);
    let out_pts_27mhz = anchor.out_27mhz.wrapping_add(delta_27mhz);
    let out_pts_90k = (out_pts_27mhz / 300) & 0x1_FFFF_FFFF;
    let with_lipsync = (out_pts_90k as i64).wrapping_add(lipsync_90k);
    (with_lipsync as u64) & 0x1_FFFF_FFFF
}

/// Overwrite the 6-byte PCR field inside a TS packet's adaptation
/// field. Returns `None` if the packet doesn't carry a PCR.
fn write_pcr_field_in_packet(pkt: &mut [u8; TS_PACKET_SIZE], pcr_27mhz: u64) -> Option<()> {
    if !ts_has_adaptation(pkt) {
        return None;
    }
    let af_len = pkt[4] as usize;
    if af_len < 7 {
        return None;
    }
    let flags = pkt[5];
    if flags & 0x10 == 0 {
        return None;
    }
    // PCR bytes start at offset 6 in the TS packet.
    let base = (pcr_27mhz / 300) & 0x1_FFFF_FFFF;
    let ext = (pcr_27mhz % 300) as u32;
    pkt[6] = ((base >> 25) & 0xFF) as u8;
    pkt[7] = ((base >> 17) & 0xFF) as u8;
    pkt[8] = ((base >> 9) & 0xFF) as u8;
    pkt[9] = ((base >> 1) & 0xFF) as u8;
    pkt[10] = (((base & 1) << 7) as u8) | 0x7E | (((ext >> 8) & 0x01) as u8);
    pkt[11] = (ext & 0xFF) as u8;
    Some(())
}

/// Overwrite the PES PTS (and DTS if `new_dts` is `Some`) fields in a
/// PUSI packet in place. Returns `None` if the layout doesn't match.
fn write_pes_timestamps(
    pkt: &mut [u8; TS_PACKET_SIZE],
    new_pts: u64,
    new_dts: Option<u64>,
) -> Option<()> {
    let afc = (pkt[3] >> 4) & 0x03;
    let payload_offset: usize = match afc {
        0b01 => 4,
        0b11 => {
            let af_len = pkt[4] as usize;
            5 + af_len
        }
        _ => return None,
    };
    if payload_offset + 14 > TS_PACKET_SIZE {
        return None;
    }
    {
        let payload = &pkt[payload_offset..];
        if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
            return None;
        }
        let pts_dts_flags = (payload[7] >> 6) & 0x03;
        if pts_dts_flags != 0b10 && pts_dts_flags != 0b11 {
            return None;
        }
    }
    let pts_dts_flags = (pkt[payload_offset + 7] >> 6) & 0x03;
    let pts_marker_top_nibble = if pts_dts_flags == 0b11 { 0x30 } else { 0x20 };
    write_pts_5bytes(
        &mut pkt[payload_offset + 9..payload_offset + 14],
        pts_marker_top_nibble,
        new_pts,
    );
    if pts_dts_flags == 0b11 {
        if let Some(d) = new_dts {
            if payload_offset + 19 > TS_PACKET_SIZE {
                return None;
            }
            write_pts_5bytes(
                &mut pkt[payload_offset + 14..payload_offset + 19],
                0x10,
                d,
            );
        }
    }
    Some(())
}

/// Encode a 33-bit value into a 5-byte PES PTS/DTS field per
/// ISO/IEC 13818-1 §2.4.3.7.
fn write_pts_5bytes(dst: &mut [u8], marker_top_nibble: u8, value_33bit: u64) {
    let v = value_33bit & 0x1_FFFF_FFFF;
    dst[0] = marker_top_nibble | (((v >> 29) as u8) & 0x0E) | 0x01;
    dst[1] = ((v >> 22) & 0xFF) as u8;
    dst[2] = (((v >> 14) as u8) & 0xFE) | 0x01;
    dst[3] = ((v >> 7) & 0xFF) as u8;
    dst[4] = (((v << 1) as u8) & 0xFE) | 0x01;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::master_clock::{MasterClockHandle, MasterClockKind, WallclockMaster};

    /// Build a PUSI TS packet on `pid` whose payload is a PES with
    /// PTS only.
    fn build_pes_packet_pts_only(pid: u16, pts: u64) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pid >> 8) as u8) & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10;
        let p = 4;
        pkt[p] = 0x00;
        pkt[p + 1] = 0x00;
        pkt[p + 2] = 0x01;
        pkt[p + 3] = 0xE0;
        pkt[p + 4] = 0x00;
        pkt[p + 5] = 0x00;
        pkt[p + 6] = 0x80;
        pkt[p + 7] = 0x80;
        pkt[p + 8] = 0x05;
        write_pts_5bytes(&mut pkt[p + 9..p + 14], 0x20, pts);
        pkt
    }

    fn build_pes_packet_pts_and_dts(
        pid: u16,
        pts: u64,
        dts: u64,
    ) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pid >> 8) as u8) & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10;
        let p = 4;
        pkt[p] = 0x00;
        pkt[p + 1] = 0x00;
        pkt[p + 2] = 0x01;
        pkt[p + 3] = 0xE0;
        pkt[p + 4] = 0x00;
        pkt[p + 5] = 0x00;
        pkt[p + 6] = 0x80;
        pkt[p + 7] = 0xC0;
        pkt[p + 8] = 0x0A;
        write_pts_5bytes(&mut pkt[p + 9..p + 14], 0x30, pts);
        write_pts_5bytes(&mut pkt[p + 14..p + 19], 0x10, dts);
        pkt
    }

    /// Build a PCR-bearing TS packet on `pid` with the given PCR value.
    /// Includes a minimal AF + payload pad.
    fn build_pcr_packet(pid: u16, pcr_27mhz: u64) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((pid >> 8) as u8) & 0x1F; // PUSI=0
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x30; // AF + payload
        pkt[4] = 7; // AF length: 1 flags byte + 6 PCR bytes
        pkt[5] = 0x10; // PCR flag set
        write_pcr_field_in_packet(&mut pkt, pcr_27mhz).unwrap();
        pkt
    }

    fn build_psi(video_pid: u16, audio_pid: u16) -> Vec<u8> {
        use crate::engine::ts_parse::mpeg2_crc32;
        let mut pat = [0xFFu8; TS_PACKET_SIZE];
        pat[0] = TS_SYNC_BYTE;
        pat[1] = 0x40;
        pat[2] = 0x00;
        pat[3] = 0x10;
        pat[4] = 0x00;
        let section_length: usize = 5 + 4 + 4;
        pat[5] = 0x00;
        pat[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pat[7] = (section_length & 0xFF) as u8;
        pat[8] = 0x00;
        pat[9] = 0x01;
        pat[10] = 0xC1;
        pat[11] = 0x00;
        pat[12] = 0x00;
        pat[13] = 0x00;
        pat[14] = 0x01;
        pat[15] = 0xE0 | (((0x100u16 >> 8) as u8) & 0x1F);
        pat[16] = 0x00;
        let crc_end = 5 + 3 + section_length;
        let crc = mpeg2_crc32(&pat[5..crc_end - 4]);
        pat[crc_end - 4] = (crc >> 24) as u8;
        pat[crc_end - 3] = (crc >> 16) as u8;
        pat[crc_end - 2] = (crc >> 8) as u8;
        pat[crc_end - 1] = crc as u8;

        let mut pmt = [0xFFu8; TS_PACKET_SIZE];
        pmt[0] = TS_SYNC_BYTE;
        pmt[1] = 0x40 | (((0x100u16 >> 8) as u8) & 0x1F);
        pmt[2] = 0x00;
        pmt[3] = 0x10;
        pmt[4] = 0x00;
        let body_len: usize = 9 + 5 * 2 + 4;
        pmt[5] = 0x02;
        pmt[6] = 0xB0 | (((body_len >> 8) as u8) & 0x0F);
        pmt[7] = (body_len & 0xFF) as u8;
        pmt[8] = 0x00;
        pmt[9] = 0x01;
        pmt[10] = 0xC1;
        pmt[11] = 0x00;
        pmt[12] = 0x00;
        pmt[13] = 0xE0 | (((video_pid >> 8) as u8) & 0x1F);
        pmt[14] = (video_pid & 0xFF) as u8;
        pmt[15] = 0xF0;
        pmt[16] = 0x00;
        pmt[17] = 0x1B;
        pmt[18] = 0xE0 | (((video_pid >> 8) as u8) & 0x1F);
        pmt[19] = (video_pid & 0xFF) as u8;
        pmt[20] = 0xF0;
        pmt[21] = 0x00;
        pmt[22] = 0x0F;
        pmt[23] = 0xE0 | (((audio_pid >> 8) as u8) & 0x1F);
        pmt[24] = (audio_pid & 0xFF) as u8;
        pmt[25] = 0xF0;
        pmt[26] = 0x00;
        let pmt_crc_end = 5 + 3 + body_len;
        let pmt_crc = mpeg2_crc32(&pmt[5..pmt_crc_end - 4]);
        pmt[pmt_crc_end - 4] = (pmt_crc >> 24) as u8;
        pmt[pmt_crc_end - 3] = (pmt_crc >> 16) as u8;
        pmt[pmt_crc_end - 2] = (pmt_crc >> 8) as u8;
        pmt[pmt_crc_end - 1] = pmt_crc as u8;

        let mut out = Vec::with_capacity(2 * TS_PACKET_SIZE);
        out.extend_from_slice(&pat);
        out.extend_from_slice(&pmt);
        out
    }

    fn make_wallclock_pacer() -> Arc<AvSyncPacer> {
        let handle = MasterClockHandle::new(
            Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        Arc::new(AvSyncPacer::new(handle))
    }

    /// PSI passes through unchanged.
    #[test]
    fn psi_packets_unchanged() {
        let mut r = TsPtsRewriter::new(make_wallclock_pacer());
        let psi = build_psi(0x100, 0x101);
        let mut buf = Vec::new();
        r.process(&psi, &mut buf);
        assert_eq!(buf, psi);
    }

    /// First PCR establishes the anchor; subsequent PCR values continue
    /// with source-delta arithmetic. Step size 40 ms (< 500 ms
    /// discontinuity threshold) verifies normal-path rate preservation.
    #[test]
    fn pcr_anchor_and_continue() {
        let mut r = TsPtsRewriter::new(make_wallclock_pacer());
        let mut buf = Vec::new();
        r.process(&build_psi(0x100, 0x101), &mut buf);

        // First PCR: anchor
        let src_pcr_1: u64 = 10_000_000;
        let pkt1 = build_pcr_packet(0x100, src_pcr_1);
        let mut out1 = Vec::new();
        r.process(&pkt1, &mut out1);
        let new_pcr_1 = extract_pcr(&out1[..TS_PACKET_SIZE]).unwrap();

        // Second PCR: source advances by 40 ms = 1_080_000 ticks
        // (below discontinuity threshold of 13_500_000)
        let src_pcr_2 = src_pcr_1 + 1_080_000;
        let pkt2 = build_pcr_packet(0x100, src_pcr_2);
        let mut out2 = Vec::new();
        r.process(&pkt2, &mut out2);
        let new_pcr_2 = extract_pcr(&out2[..TS_PACKET_SIZE]).unwrap();

        let out_delta = new_pcr_2.wrapping_sub(new_pcr_1);
        assert_eq!(
            out_delta, 1_080_000,
            "PCR rate preserved by anchor model on normal path"
        );
    }

    /// Source PCR discontinuity (>500 ms backward jump = loop wrap)
    /// is bridged with master elapsed; output PCR stays monotonic
    /// (advances by tiny master_delta, never jumps backward).
    #[test]
    fn pcr_discontinuity_bridged_monotonic() {
        let mut r = TsPtsRewriter::new(make_wallclock_pacer());
        let mut buf = Vec::new();
        r.process(&build_psi(0x100, 0x101), &mut buf);

        let src_pcr_1: u64 = 1_000_000_000;
        let mut out1 = Vec::new();
        r.process(&build_pcr_packet(0x100, src_pcr_1), &mut out1);
        let new_pcr_1 = extract_pcr(&out1[..TS_PACKET_SIZE]).unwrap();

        // Source jumps back 30 seconds (loop wrap) — well above
        // DISCONTINUITY_THRESHOLD_27MHZ (500 ms = 13.5M ticks).
        let src_pcr_2: u64 = src_pcr_1 - 30 * 27_000_000;
        let mut out2 = Vec::new();
        r.process(&build_pcr_packet(0x100, src_pcr_2), &mut out2);
        let new_pcr_2 = extract_pcr(&out2[..TS_PACKET_SIZE]).unwrap();

        // Forward advance (mod PCR space). 30s in either direction
        // would be ~810M ticks. A clean bridge gives ~ms (test
        // runtime). Either small forward (master elapsed) or zero is
        // acceptable; backward is a fail.
        let forward = (new_pcr_2 as i64).wrapping_sub(new_pcr_1 as i64);
        assert!(
            forward >= 0 && forward < 100_000_000,
            "output PCR should advance forward by at most ~ms after bridge; \
             got forward delta = {forward} 27 MHz ticks"
        );
    }

    /// PES PTS pre-PCR pass through (anchor not established yet).
    #[test]
    fn pes_pts_passthrough_before_first_pcr() {
        let mut r = TsPtsRewriter::new(make_wallclock_pacer());
        let mut buf = Vec::new();
        r.process(&build_psi(0x100, 0x101), &mut buf);
        // No PCR yet — PES PTS should pass through unchanged.
        let pkt = build_pes_packet_pts_only(0x100, 12_345_678);
        let mut out = Vec::new();
        r.process(&pkt, &mut out);
        let parsed = extract_pes_pts(&out[..TS_PACKET_SIZE]).unwrap();
        assert_eq!(parsed, 12_345_678);
    }

    /// After PCR anchor, PES PTS rewrites preserve source PCR→PTS delta.
    #[test]
    fn pes_pts_preserves_pcr_pts_delta_after_anchor() {
        let mut r = TsPtsRewriter::new(make_wallclock_pacer());
        let mut buf = Vec::new();
        r.process(&build_psi(0x100, 0x101), &mut buf);

        let src_pcr: u64 = 10_000_000; // 27 MHz
        let src_pcr_90k = src_pcr / 300;
        let _ = r.rewrite_pcr_value(src_pcr); // anchor

        // PES PTS at src_pcr_90k + 7200 (80 ms ahead = standard pre-roll)
        let src_pts = src_pcr_90k + 7200;
        let (new_pts, _) = r.rewrite_pes_values(src_pts, None, false);
        // Output PCR at anchor time = (master_now - PCR_PREROLL) / 300
        // Output PTS - Output PCR = (src_pts - src_pcr_90k) = 7200
        // So new_pts - new_pcr_90k = 7200
        let new_pcr_90k = (r.anchor.out_27mhz / 300) & 0x1_FFFF_FFFF;
        let delta = new_pts.wrapping_sub(new_pcr_90k) & 0x1_FFFF_FFFF;
        // Should be ~7200 (PCR pre-roll delta from source preserved)
        assert!(
            delta == 7200 || delta == 7199 || delta == 7201,
            "PCR-PTS delta should be preserved ~7200 (source pre-roll); got {delta}"
        );
    }

    /// DTS-PTS delta preserved.
    #[test]
    fn dts_pts_delta_preserved() {
        let mut r = TsPtsRewriter::new(make_wallclock_pacer());
        let mut buf = Vec::new();
        r.process(&build_psi(0x100, 0x101), &mut buf);
        // Anchor with PCR.
        let _ = r.rewrite_pcr_value(5_000_000);
        let src_pts: u64 = 5_000_000 / 300 + 9000; // 100 ms after PCR
        let src_dts: u64 = src_pts - 3600; // DTS leads PTS by 40 ms
        let (new_pts, new_dts) = r.rewrite_pes_values(src_pts, Some(src_dts), false);
        let new_delta = new_pts.wrapping_sub(new_dts.unwrap()) & 0x1_FFFF_FFFF;
        let src_delta = src_pts.wrapping_sub(src_dts);
        assert_eq!(new_delta, src_delta, "PTS-DTS delta preserved");
    }

    /// Audio lipsync offset is applied; video unaffected.
    #[test]
    fn lipsync_audio_only() {
        let handle = MasterClockHandle::new(
            Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        handle.set_lipsync_offset_90k(900); // +10 ms
        let pacer = Arc::new(AvSyncPacer::new(handle));
        let mut r = TsPtsRewriter::new(pacer);
        let mut buf = Vec::new();
        r.process(&build_psi(0x100, 0x101), &mut buf);
        let _ = r.rewrite_pcr_value(10_000_000);
        let src_pts: u64 = 33_333 + 7_200; // some PTS in the future
        let (audio_pts, _) = r.rewrite_pes_values(src_pts, None, true);
        let (video_pts, _) = r.rewrite_pes_values(src_pts, None, false);
        let diff = audio_pts.wrapping_sub(video_pts) & 0x1_FFFF_FFFF;
        assert_eq!(diff, 900, "audio PTS should be lipsync_offset (900) ahead of video");
    }

    /// PCR rate matches source rate over many samples.
    #[test]
    fn pcr_rate_tracking() {
        let mut r = TsPtsRewriter::new(make_wallclock_pacer());
        let mut buf = Vec::new();
        r.process(&build_psi(0x100, 0x101), &mut buf);
        // Feed 10 PCRs each 40 ms apart (broadcast standard)
        let start: u64 = 100_000_000;
        let step: u64 = 40 * 27_000;
        let mut last_out = None;
        for i in 0..10 {
            let pkt = build_pcr_packet(0x100, start + i * step);
            let mut out = Vec::new();
            r.process(&pkt, &mut out);
            let pcr = extract_pcr(&out[..TS_PACKET_SIZE]).unwrap();
            if let Some(prev) = last_out {
                let delta: u64 = pcr.wrapping_sub(prev);
                assert_eq!(
                    delta, step,
                    "output PCR advance should match source step exactly"
                );
            }
            last_out = Some(pcr);
        }
    }
}
