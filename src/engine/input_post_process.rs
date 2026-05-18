// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Composite ingress post-processor: program filter → role-keyed PID
//! overrides → mechanical PID remap → PES PTS/DTS regeneration.
//!
//! Wraps the four input-side TS rewriting stages so each TS-carrying
//! input task can pull in symmetric `program_number` / `pid_overrides` /
//! `pid_map` / `passthrough_clock` semantics with one helper. The chain
//! ordering mirrors the output-side equivalent in the per-output forward
//! loops:
//!
//! 1. **`TsProgramFilter`** narrows MPTS → SPTS at ingress when
//!    `program_number` is set. Drops every program except the named one
//!    before any other stage runs (saves work for the rest of the chain).
//! 2. **`TsPidOverridesRewriter`** applies role-keyed per-program PID
//!    rewrites. Single owner of PID remapping on both passthrough and
//!    transcoded paths: the transcode replacers re-encode on the source
//!    audio / video PID and let this stage handle every PAT/PMT/ES PID
//!    rename. Keying off the observed PAT program number means
//!    `pid_overrides` entries for programs other than 1 work the same
//!    way they do for passthrough.
//! 3. **`TsPidRemapper`** mechanical `source_pid → target_pid` lookup.
//!    Runs after the role-keyed override so the operator can layer
//!    "rewrite roles, then bulk-remap leftover PIDs".
//! 4. **`TsPtsRewriter`** byte-level PES PTS/DTS regeneration driven by
//!    the per-flow master clock. Runs last so the rewriter learns the
//!    *final* PID layout (post-rename) directly from the rewritten PMT.
//!    Gated by `passthrough_clock: true` + an attached `AvSyncPacer`; with
//!    either unset this stage is `None` and the byte stream passes
//!    through with zero cost. See [`super::ts_pts_rewriter`].
//!
//! The processor is byte-stream oriented: callers feed 188-byte-aligned
//! TS bytes, get rewritten 188-byte-aligned TS bytes back. One scratch
//! buffer per active stage so the borrow checker stays happy with
//! simultaneous immutable / mutable references to different stages'
//! outputs.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::models::TsPidOverridesMap;

use super::av_sync_mux::AvSyncPacer;
use super::ts_pid_overrides_rewriter::TsPidOverridesRewriter;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;
use super::ts_pts_rewriter::TsPtsRewriter;

/// Construction options assembled from one input config.
pub struct InputPostProcessConfig<'a> {
    pub program_number: Option<u16>,
    pub pid_overrides: Option<&'a TsPidOverridesMap>,
    pub pid_map: Option<&'a BTreeMap<u16, u16>>,
    /// Per-input opt-OUT of muxer-mode PCR + PES PTS regeneration.
    /// Default `false` → muxer mode active when an `av_sync_pacer` is
    /// attached. Set to `true` to forward source PCR/PTS bytes
    /// unchanged (relay / transparent-forwarder mode — preserves
    /// source bit patterns at the cost of inheriting source clock
    /// jitter and discontinuities).
    pub passthrough_clock: bool,
    /// Per-flow A/V sync pacer (master-clock handle). Required for
    /// the muxer-mode rewriter; `None` disables it regardless of
    /// `passthrough_clock`.
    pub av_sync_pacer: Option<&'a Arc<AvSyncPacer>>,
}

/// Composite TS post-processor for inputs.
///
/// Returns `None` from [`from_config`] when none of the stages have any
/// work to do — the caller can then skip the whole stage entirely
/// (zero cost, no scratch buffers allocated).
///
/// One scratch buffer per stage so the borrow checker is happy with
/// simultaneous immutable / mutable references to different stages'
/// outputs. ~128 KB total when all four stages are active.
pub struct InputPostProcess {
    program_filter: Option<TsProgramFilter>,
    pid_overrides_rewriter: Option<TsPidOverridesRewriter>,
    pid_remapper: Option<TsPidRemapper>,
    pts_rewriter: Option<TsPtsRewriter>,
    scratch_filter: Vec<u8>,
    scratch_overrides: Vec<u8>,
    scratch_remap: Vec<u8>,
    scratch_pts: Vec<u8>,
}

impl InputPostProcess {
    /// Construct from input config. Returns `None` when no stage has any
    /// work to do — callers should then skip the post-process entirely.
    pub fn from_config(cfg: &InputPostProcessConfig<'_>) -> Option<Self> {
        let program_filter = cfg.program_number.map(TsProgramFilter::new);

        let pid_overrides_rewriter = cfg.pid_overrides.and_then(|m| {
            let r = TsPidOverridesRewriter::new(m);
            if r.is_active() { Some(r) } else { None }
        });

        let pid_remapper = cfg.pid_map.and_then(|m| {
            let r = TsPidRemapper::new(m);
            if r.is_active() { Some(r) } else { None }
        });

        // Muxer-mode rewriter: ON when an A/V sync pacer is available
        // AND the operator hasn't opted out via passthrough_clock AND
        // the pacer hasn't been claimed by an assembler (PID-bus /
        // Node-Bus flow). When `is_assembler_owned`, the assembler
        // runs its own single-anchor rewriter on the assembled output,
        // so per-input rewriting would double-stamp + use per-input
        // anchors that don't align across inputs (breaks cross-input
        // PES splice arithmetic). Industry-standard default (Sencore
        // RMX / Cobalt 9970-MX / Cisco D9036 mux mode). See
        // [`super::ts_pts_rewriter`].
        let pts_rewriter = if cfg.passthrough_clock {
            None
        } else {
            cfg.av_sync_pacer.and_then(|p| {
                if p.is_assembler_owned() {
                    None
                } else {
                    Some(TsPtsRewriter::new(p.clone()))
                }
            })
        };

        if program_filter.is_none()
            && pid_overrides_rewriter.is_none()
            && pid_remapper.is_none()
            && pts_rewriter.is_none()
        {
            return None;
        }

        Some(Self {
            program_filter,
            pid_overrides_rewriter,
            pid_remapper,
            pts_rewriter,
            scratch_filter: Vec::with_capacity(32 * 1024),
            scratch_overrides: Vec::with_capacity(32 * 1024),
            scratch_remap: Vec::with_capacity(32 * 1024),
            scratch_pts: Vec::with_capacity(32 * 1024),
        })
    }

    /// Run the chain on one chunk of 188-byte-aligned TS bytes. Returns
    /// a borrowed slice valid until the next call.
    ///
    /// Each stage writes into its own dedicated scratch buffer so the
    /// borrow checker stays happy with the chained references.
    pub fn process<'a>(&'a mut self, ts_in: &'a [u8]) -> &'a [u8] {
        // Split the borrow so each stage can hold a &mut to its scratch
        // while the previous stage's scratch is held immutably.
        let Self {
            program_filter,
            pid_overrides_rewriter,
            pid_remapper,
            pts_rewriter,
            scratch_filter,
            scratch_overrides,
            scratch_remap,
            scratch_pts,
        } = self;
        scratch_filter.clear();
        scratch_overrides.clear();
        scratch_remap.clear();
        scratch_pts.clear();

        let after_filter: &[u8] = if let Some(f) = program_filter.as_mut() {
            f.filter_into(ts_in, scratch_filter);
            scratch_filter
        } else {
            ts_in
        };

        let after_overrides: &[u8] = if let Some(rw) = pid_overrides_rewriter.as_mut() {
            rw.process(after_filter, scratch_overrides);
            scratch_overrides
        } else {
            after_filter
        };

        let after_remap: &[u8] = if let Some(rm) = pid_remapper.as_mut() {
            rm.process(after_overrides, scratch_remap);
            scratch_remap
        } else {
            after_overrides
        };

        if let Some(rw) = pts_rewriter.as_mut() {
            rw.process(after_remap, scratch_pts);
            scratch_pts
        } else {
            after_remap
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::TsPidOverridesEntry;
    use crate::engine::ts_parse::{mpeg2_crc32, ts_pid, TS_PACKET_SIZE, TS_SYNC_BYTE};

    /// Build a single-section PAT TS packet listing the given
    /// `(program_number, pmt_pid)` pairs. Mirrors
    /// `ts_pid_overrides_rewriter::tests::build_pat_packet`.
    fn build_pat_packet(programs: &[(u16, u16)], version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID=0
        pkt[2] = 0x00;
        pkt[3] = 0x10;
        pkt[4] = 0x00; // pointer_field
        let section_length = 5 + 4 * programs.len() + 4;
        pkt[5] = 0x00;
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1 | ((version & 0x1F) << 1);
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        let mut pos = 13;
        for (program_number, pmt_pid) in programs {
            pkt[pos] = (program_number >> 8) as u8;
            pkt[pos + 1] = (program_number & 0xFF) as u8;
            pkt[pos + 2] = 0xE0 | (((pmt_pid >> 8) as u8) & 0x1F);
            pkt[pos + 3] = (pmt_pid & 0xFF) as u8;
            pos += 4;
        }
        let crc_section_end = 5 + 3 + section_length;
        let crc = mpeg2_crc32(&pkt[5..crc_section_end - 4]);
        pkt[crc_section_end - 4] = (crc >> 24) as u8;
        pkt[crc_section_end - 3] = (crc >> 16) as u8;
        pkt[crc_section_end - 2] = (crc >> 8) as u8;
        pkt[crc_section_end - 1] = crc as u8;
        pkt
    }

    /// Build a single-section PMT TS packet on `pmt_pid` with the given
    /// `(stream_type, es_pid)` entries. PCR_PID = first ES PID. Mirrors
    /// `ts_pid_overrides_rewriter::tests::build_pmt_packet`.
    fn build_pmt_packet(
        pmt_pid: u16,
        streams: &[(u8, u16)],
        version: u8,
    ) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let section_data_len = 9 + 5 * streams.len() + 4;
        let section_length: u16 = section_data_len as u16;
        let pcr_pid = streams.first().map(|(_, p)| *p).unwrap_or(0x1FFF);
        pkt[5] = 0x02;
        pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1 | ((version & 0x1F) << 1);
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | (((pcr_pid >> 8) as u8) & 0x1F);
        pkt[14] = (pcr_pid & 0xFF) as u8;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        let mut pos = 17;
        for (stream_type, es_pid) in streams {
            pkt[pos] = *stream_type;
            pkt[pos + 1] = 0xE0 | (((es_pid >> 8) as u8) & 0x1F);
            pkt[pos + 2] = (es_pid & 0xFF) as u8;
            pkt[pos + 3] = 0xF0;
            pkt[pos + 4] = 0x00;
            pos += 5;
        }
        let crc_section_end = 5 + 3 + section_length as usize;
        let crc = mpeg2_crc32(&pkt[5..crc_section_end - 4]);
        pkt[crc_section_end - 4] = (crc >> 24) as u8;
        pkt[crc_section_end - 3] = (crc >> 16) as u8;
        pkt[crc_section_end - 2] = (crc >> 8) as u8;
        pkt[crc_section_end - 1] = crc as u8;
        pkt
    }

    /// Build a minimal ES packet (no PUSI, no AF, no payload bytes) on
    /// `pid`. Sufficient to assert the TS-header PID after the rewrite.
    fn build_es_packet(pid: u16) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((pid >> 8) as u8) & 0x1F;
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt
    }

    /// Bug repro from the testbed: an input with `audio_encode` set AND
    /// `pid_overrides` keyed on a non-1 program number (1591 — a real
    /// DVB program number from the media-player config) used to silently
    /// ignore every override field, because the input post-process gated
    /// `TsPidOverridesRewriter` on `has_transcode = false`. The fix
    /// removed that gate so the rewriter is the single owner of PID
    /// remapping for both passthrough and transcoded paths.
    ///
    /// This test deliberately builds the post-processor with the same
    /// shape the input modules now produce (no `has_transcode` field —
    /// it was deleted alongside the gate) and walks a fabricated
    /// PAT/PMT/ES sequence through to confirm every PID is renamed to
    /// the operator's target.
    #[test]
    fn audio_encode_input_with_non_1_program_overrides_is_honoured() {
        // Operator override: program 1591 →
        //   pmt_pid    = 0x100 → 0x3E8
        //   video_pid  = 0x101 → 0x3E9
        //   audio_pid  = 0x102 → 0x3EA
        let mut overrides = TsPidOverridesMap::new();
        overrides.insert(
            1591,
            TsPidOverridesEntry {
                pmt_pid: Some(0x3E8),
                video_pid: Some(0x3E9),
                audio_pid: Some(0x3EA),
                audio_pids: None,
                pcr_pid: None,
            },
        );

        // Build the post-processor the same way every input module
        // does now. The fix means `pid_overrides` alone is enough to
        // build the rewriter — `audio_encode` being set on the input
        // is irrelevant to this stage.
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: None,
            pid_overrides: Some(&overrides),
            pid_map: None,
            passthrough_clock: false,
            av_sync_pacer: None,
        })
        .expect("rewriter must be built when pid_overrides is non-empty");

        // Source feed: PAT(program 1591 → PMT 0x100) +
        //              PMT(video 0x101 H.264, audio 0x102 AAC) +
        //              ES packets on the source PIDs.
        let pat = build_pat_packet(&[(1591, 0x100)], 0);
        let pmt = build_pmt_packet(
            0x100,
            &[
                (0x1B, 0x101), // H.264 video
                (0x0F, 0x102), // AAC ADTS audio
            ],
            0,
        );

        // PAT rewrite: program 1591's PMT_PID must point at 0x3E8.
        let mut pat_buf = Vec::new();
        pat_buf.extend_from_slice(post.process(&pat));
        assert_eq!(pat_buf.len(), TS_PACKET_SIZE, "PAT must pass through one-for-one");
        // Find the program_map_PID slot in the rewritten PAT.
        // Pointer at 4, table_id at 5, section header (8 bytes), then
        // program entries: program_number(2) + program_map_PID(2).
        let entry_pmt_pid = (((pat_buf[15] & 0x1F) as u16) << 8) | (pat_buf[16] as u16);
        assert_eq!(
            entry_pmt_pid, 0x3E8,
            "PAT for program 1591 must advertise target PMT PID 0x3E8 (got 0x{entry_pmt_pid:04X})"
        );

        // PMT rewrite: feeding the source PMT on PID 0x100 must produce
        // an output PMT on the rewritten PMT PID 0x3E8 carrying the
        // target video and audio PIDs.
        let mut pmt_buf = Vec::new();
        pmt_buf.extend_from_slice(post.process(&pmt));
        assert_eq!(pmt_buf.len(), TS_PACKET_SIZE, "PMT must pass through one-for-one");
        let pmt_ts_pid = ts_pid(&pmt_buf[..TS_PACKET_SIZE]);
        assert_eq!(
            pmt_ts_pid, 0x3E8,
            "PMT TS-header PID must be rewritten to the override target (got 0x{pmt_ts_pid:04X})"
        );
        // Walk the PMT body and assert both ES entries advertise the
        // overridden PIDs.
        // Pointer at 4, table_id at 5, section header (12 bytes) → ES
        // entries start at 17.
        let mut pos = 17;
        let mut saw_video_target = false;
        let mut saw_audio_target = false;
        while pos + 5 <= TS_PACKET_SIZE {
            let st = pmt_buf[pos];
            let es_pid =
                (((pmt_buf[pos + 1] & 0x1F) as u16) << 8) | (pmt_buf[pos + 2] as u16);
            let es_info_len =
                (((pmt_buf[pos + 3] & 0x0F) as usize) << 8) | (pmt_buf[pos + 4] as usize);
            if st == 0x1B && es_pid == 0x3E9 {
                saw_video_target = true;
            }
            if st == 0x0F && es_pid == 0x3EA {
                saw_audio_target = true;
            }
            pos += 5 + es_info_len;
            if pos >= TS_PACKET_SIZE || st == 0xFF {
                break;
            }
        }
        assert!(
            saw_video_target,
            "rewritten PMT must carry video at target PID 0x3E9"
        );
        assert!(
            saw_audio_target,
            "rewritten PMT must carry audio at target PID 0x3EA"
        );

        // ES packets on the source video / audio PIDs must come out the
        // other side on the override target PIDs.
        let mut video_out = Vec::new();
        video_out.extend_from_slice(post.process(&build_es_packet(0x101)));
        assert_eq!(
            ts_pid(&video_out[..TS_PACKET_SIZE]),
            0x3E9,
            "ES packets on source video PID 0x101 must ride target video PID 0x3E9"
        );
        let mut audio_out = Vec::new();
        audio_out.extend_from_slice(post.process(&build_es_packet(0x102)));
        assert_eq!(
            ts_pid(&audio_out[..TS_PACKET_SIZE]),
            0x3EA,
            "ES packets on source audio PID 0x102 must ride target audio PID 0x3EA"
        );
    }

    /// When the per-flow pacer has been claimed by an assembler
    /// (`mark_assembler_owned`), the per-input rewriter must NOT
    /// build — the assembler is responsible for the single-anchor
    /// muxer-mode rewrite on the assembled output.
    #[test]
    fn assembler_owned_pacer_disables_per_input_rewriter() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::MasterClockHandle;
        let pacer = Arc::new(AvSyncPacer::new(MasterClockHandle::wallclock()));
        pacer.mark_assembler_owned();
        let post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: None,
            pid_overrides: None,
            pid_map: None,
            passthrough_clock: false,
            av_sync_pacer: Some(&pacer),
        });
        assert!(
            post.is_none(),
            "assembler-owned pacer + no other stages → no post-processor"
        );
    }

    /// Non-assembler pacer (default) DOES build the per-input rewriter
    /// when other stages are absent.
    #[test]
    fn non_assembler_pacer_builds_rewriter() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::MasterClockHandle;
        let pacer = Arc::new(AvSyncPacer::new(MasterClockHandle::wallclock()));
        // not marked
        let post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: None,
            pid_overrides: None,
            pid_map: None,
            passthrough_clock: false,
            av_sync_pacer: Some(&pacer),
        });
        assert!(
            post.is_some(),
            "non-claimed pacer + muxer mode → rewriter built"
        );
    }
}
