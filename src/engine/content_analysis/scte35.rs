// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SCTE-35 splice-information presence detection.
//!
//! The full `splice_info_section` is not decoded — Phase 1 only flags
//! PID presence, counts cue arrivals, and captures the `splice_command_type`
//! + optional PTS for the manager UI. Full decode (time_signal +
//! segmentation_descriptor → ad-break markers) lands in a follow-up.
//!
//! Scope is set by [`parse::CUE_COMMAND_NAMES`] and [`parse::extract_pts`].
//! Table ID must be 0xFC and the command type must be one of the five
//! standardised values; everything else is counted but the `last_command`
//! falls back to `"unknown"`.

use crate::manager::events::EventSender;
use crate::stats::models::Scte35Stats;

pub struct Scte35Tracker {
    pids: Vec<u16>,
    cue_count: u64,
    last_command: Option<String>,
    last_pts: Option<u64>,
    // Incremental assembly buffers keyed by section continuation.
    // Most cues fit inside a single TS packet payload (≤ 184 bytes of
    // payload for an ≤ ~30-byte `splice_info_section`); we handle the
    // multi-packet case with a small per-instance buffer.
    pending: Option<Vec<u8>>,
    pending_total: usize,
}

impl Scte35Tracker {
    pub fn new() -> Self {
        Self {
            pids: Vec::new(),
            cue_count: 0,
            last_command: None,
            last_pts: None,
            pending: None,
            pending_total: 0,
        }
    }

    pub fn set_pids(&mut self, pids: Vec<u16>) {
        self.pids = pids;
    }

    pub fn cue_count(&self) -> u64 {
        self.cue_count
    }

    pub fn last_command(&self) -> Option<&str> {
        self.last_command.as_deref()
    }

    pub fn last_pts(&self) -> Option<u64> {
        self.last_pts
    }

    /// Observe a TS payload on a PMT-announced SCTE-35 PID.
    ///
    /// `pusi` indicates a new section start (after the pointer_field).
    /// SCTE-35 sections are unsectioned PSI — the table_id is 0xFC,
    /// `section_syntax_indicator` is 0, and `private_indicator` is 0.
    pub fn observe(
        &mut self,
        pusi: bool,
        payload: &[u8],
        _flow_id: &str,
        _events: &EventSender,
    ) {
        if pusi {
            // Start of a new section. Skip pointer_field.
            if payload.is_empty() {
                return;
            }
            let pointer = payload[0] as usize;
            if 1 + pointer + 3 > payload.len() {
                return;
            }
            let section = &payload[1 + pointer..];
            if section.is_empty() || section[0] != 0xFC {
                return;
            }
            let section_length =
                (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
            let total_len = 3 + section_length;

            if total_len <= section.len() {
                // Section fits in one TS packet payload — parse immediately.
                self.handle_section(&section[..total_len]);
                self.pending = None;
                self.pending_total = 0;
            } else {
                // Section continues into next TS payload. Buffer what we have.
                let mut buf = Vec::with_capacity(total_len);
                buf.extend_from_slice(section);
                self.pending = Some(buf);
                self.pending_total = total_len;
            }
        } else if let Some(ref mut buf) = self.pending {
            // Continuation payload — TS continuation payloads start with the
            // section-body bytes directly (no pointer_field).
            let remaining = self.pending_total.saturating_sub(buf.len());
            let take = remaining.min(payload.len());
            buf.extend_from_slice(&payload[..take]);
            if buf.len() >= self.pending_total {
                let complete = std::mem::take(buf);
                self.handle_section(&complete);
                self.pending = None;
                self.pending_total = 0;
            }
        }
    }

    fn handle_section(&mut self, section: &[u8]) {
        self.cue_count += 1;
        let (cmd_name, pts) = parse::decode_section(section);
        self.last_command = Some(cmd_name.into());
        if pts.is_some() {
            self.last_pts = pts;
        }
    }

    pub fn snapshot(&self) -> Option<Scte35Stats> {
        if self.pids.is_empty() && self.cue_count == 0 {
            return None;
        }
        let mut pids = self.pids.clone();
        pids.sort();
        Some(Scte35Stats {
            pids,
            cue_count: self.cue_count,
            last_command: self.last_command.clone(),
            last_pts: self.last_pts,
        })
    }
}

mod parse {
    //! Minimal SCTE-35 `splice_info_section` decoder. Reads just enough to
    //! populate `last_command` and `last_pts` for the manager UI.

    /// Decode the section down to (command_name, optional_pts). Panics are
    /// avoided via length guards — unknown / malformed sections return
    /// `("unknown", None)`.
    pub(super) fn decode_section(sec: &[u8]) -> (&'static str, Option<u64>) {
        // Layout (abbreviated):
        //   table_id (1), section_syntax_indicator+private+reserved+length (2),
        //   protocol_version (1), encrypted_packet+encryption_algorithm+
        //   pts_adjustment (5), cw_index (1), tier+splice_command_length (2),
        //   splice_command_type (1), splice_command_bytes (...)
        if sec.len() < 14 {
            return ("unknown", None);
        }
        let command_length = (((sec[11] as usize) & 0x0F) << 8) | sec[12] as usize;
        let command_type = sec[13];
        let cmd_bytes_start = 14;
        let cmd_bytes_end = cmd_bytes_start + command_length;
        let cmd_name = command_name(command_type);
        let pts = if cmd_bytes_end <= sec.len() {
            extract_pts(command_type, &sec[cmd_bytes_start..cmd_bytes_end])
        } else {
            None
        };
        (cmd_name, pts)
    }

    fn command_name(t: u8) -> &'static str {
        match t {
            0x00 => "splice_null",
            0x04 => "splice_schedule",
            0x05 => "splice_insert",
            0x06 => "time_signal",
            0x07 => "bandwidth_reservation",
            0xFF => "private_command",
            _ => "unknown",
        }
    }

    fn extract_pts(cmd_type: u8, bytes: &[u8]) -> Option<u64> {
        match cmd_type {
            // splice_insert: splice_event_id(4) +
            //   splice_event_cancel_indicator(1 bit) + reserved(7 bits) +
            //   if !cancel {
            //     out_of_network(1) + program_splice_flag(1) +
            //     duration_flag(1) + splice_immediate_flag(1) + reserved(4)
            //     if program_splice_flag && !splice_immediate_flag {
            //       splice_time(40 bits) — the PTS we want
            //     }
            //   }
            0x05 => {
                if bytes.len() < 5 {
                    return None;
                }
                let cancel = (bytes[4] & 0x80) != 0;
                if cancel {
                    return None;
                }
                if bytes.len() < 6 {
                    return None;
                }
                let flags = bytes[5];
                let program_splice = (flags & 0x80) != 0;
                let splice_immediate = (flags & 0x10) != 0;
                if !program_splice || splice_immediate {
                    return None;
                }
                if bytes.len() < 11 {
                    return None;
                }
                read_splice_time(&bytes[6..])
            }
            // time_signal: directly a splice_time()
            0x06 => read_splice_time(bytes),
            _ => None,
        }
    }

    /// `splice_time()` structure:
    ///   time_specified_flag (1)
    ///   if time_specified_flag {
    ///     reserved (6)
    ///     pts_time (33)
    ///   } else {
    ///     reserved (7)
    ///   }
    fn read_splice_time(bytes: &[u8]) -> Option<u64> {
        if bytes.is_empty() {
            return None;
        }
        let time_specified = (bytes[0] & 0x80) != 0;
        if !time_specified || bytes.len() < 5 {
            return None;
        }
        let pts: u64 = (((bytes[0] as u64) & 0x01) << 32)
            | ((bytes[1] as u64) << 24)
            | ((bytes[2] as u64) << 16)
            | ((bytes[3] as u64) << 8)
            | (bytes[4] as u64);
        Some(pts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tracker_no_snapshot() {
        let t = Scte35Tracker::new();
        assert!(t.snapshot().is_none());
    }

    #[test]
    fn decodes_time_signal_with_pts() {
        // Build a splice_info_section layout (byte offsets in comments):
        //   [0]       table_id                    = 0xFC
        //   [1..3]    flags + section_length      (value ignored by our decoder)
        //   [3]       protocol_version            = 0
        //   [4..9]    encryption + pts_adjustment = 5 zero bytes
        //   [9]       cw_index                    = 0
        //   [10..13]  tier(12) + cmd_length(12)   = 3 bytes; cmd_length = 5
        //   [13]      splice_command_type         = 0x06 (time_signal)
        //   [14..19]  splice_time                 = flag=1, pts=1
        let mut section = Vec::with_capacity(25);
        section.extend_from_slice(&[0xFC, 0x00, 0x13, 0x00]);
        section.extend_from_slice(&[0x00; 5]); // encrypted + pts_adjustment
        section.push(0x00); // cw_index
        section.extend_from_slice(&[0x00, 0x00, 0x05]); // tier + cmd_length=5
        section.push(0x06); // splice_command_type = time_signal
        section.extend_from_slice(&[0x80, 0x00, 0x00, 0x00, 0x01]); // splice_time
        section.extend_from_slice(&[0x00, 0x00]); // descriptor_loop_length = 0
        section.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // CRC32 placeholder

        let mut t = Scte35Tracker::new();
        t.set_pids(vec![0x1FC]);
        t.handle_section(&section);
        let snap = t.snapshot().expect("should produce a snapshot");
        assert_eq!(snap.cue_count, 1);
        assert_eq!(snap.last_command.as_deref(), Some("time_signal"));
        assert_eq!(snap.last_pts, Some(0x1));
    }

    #[test]
    fn unknown_command_type_still_increments_count() {
        // Same layout, but splice_command_type = 0xFE which isn't one of
        // the five named commands in `parse::command_name`.
        let mut section = Vec::with_capacity(25);
        section.extend_from_slice(&[0xFC, 0x00, 0x0E, 0x00]);
        section.extend_from_slice(&[0x00; 5]);
        section.push(0x00);
        section.extend_from_slice(&[0x00, 0x00, 0x00]);
        section.push(0xFE); // unknown command type
        let mut t = Scte35Tracker::new();
        t.handle_section(&section);
        let snap = t.snapshot().expect("snapshot");
        assert_eq!(snap.cue_count, 1);
        assert_eq!(snap.last_command.as_deref(), Some("unknown"));
    }
}
