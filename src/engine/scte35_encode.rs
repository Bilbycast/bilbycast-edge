// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pure-Rust SCTE-104 single-operation to SCTE-35 `splice_insert` encoder.

// This whole module (the SCTE-35 section codec) is wired only into the SDI VANC
// SCTE-35 paths — `engine::sdi_io` (extraction) and `engine::output_sdi`
// (injection), both `#[cfg(feature = "sdi-decklink")]`. Its unit tests run
// unconditionally, so it stays compiled in every build; silence dead-code only
// when that consuming feature is compiled out.
#![cfg_attr(not(feature = "sdi-decklink"), allow(dead_code))]

use crate::engine::st2110::scte104::{Scte104Message, SpliceOpcode};
use crate::engine::ts_parse::mpeg2_crc32;

const PTS_MASK: u64 = (1u64 << 33) - 1;

/// Build one complete `splice_info_section` (table id `0xFC`).
pub fn build_splice_insert_section(msg: &Scte104Message, current_pts_90k: u64) -> Vec<u8> {
    let event_id = msg.splice_event_id.unwrap_or(0);
    let cancel = matches!(msg.opcode, SpliceOpcode::SpliceCancel);
    let immediate = msg.pre_roll_ms.unwrap_or(0) == 0;
    let duration = msg.break_duration_ms.filter(|_| !cancel);

    let mut command = Vec::new();
    command.extend_from_slice(&event_id.to_be_bytes());
    command.push(if cancel { 0xFF } else { 0x7F });
    if !cancel {
        let out_of_network = matches!(msg.opcode, SpliceOpcode::SpliceStartNormal);
        command.push(
            (if out_of_network { 0x80 } else { 0 })
                | 0x40
                | (if duration.is_some() { 0x20 } else { 0 })
                | (if immediate { 0x10 } else { 0 })
                | 0x0F,
        );
        if !immediate {
            let pts =
                current_pts_90k.wrapping_add(msg.pre_roll_ms.unwrap_or(0) as u64 * 90) & PTS_MASK;
            command.push(0xFE | ((pts >> 32) as u8 & 1));
            command.extend_from_slice(&(pts as u32).to_be_bytes());
        }
        if let Some(ms) = duration {
            let ticks = (ms as u64 * 90) & PTS_MASK;
            command.push(
                (if msg.auto_return.unwrap_or(false) {
                    0x80
                } else {
                    0
                }) | 0x7E
                    | ((ticks >> 32) as u8 & 1),
            );
            command.extend_from_slice(&(ticks as u32).to_be_bytes());
        }
        command.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]); // unique_program_id, avail fields
    }

    let section_length = 1 + 5 + 1 + 3 + 1 + command.len() + 2 + 4;
    let mut section = Vec::with_capacity(3 + section_length);
    section.push(0xFC);
    // section_syntax_indicator=0, private_indicator=0, then sap_type. SCTE 35
    // 2019+ defines those two bits as sap_type with default '11' (Not
    // Specified); emitting 00 falsely asserts "Type 1 SAP". Older editions
    // name them reserved '11'. Either way the correct default is 0b11.
    section.push((0x30 | ((section_length >> 8) & 0x0F)) as u8);
    section.push(section_length as u8);
    section.push(msg.protocol_version);
    section.extend_from_slice(&[0; 5]); // clear packet, pts_adjustment = 0
    section.push(0); // cw_index
    section.push(0xFF); // tier = 0xFFF
    section.push(0xF0 | (((command.len() >> 8) & 0x0F) as u8));
    section.push(command.len() as u8);
    section.push(0x05);
    section.extend_from_slice(&command);
    section.extend_from_slice(&[0, 0]); // descriptor_loop_length
    let crc = mpeg2_crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

/// Decode a clear, CRC-valid `splice_insert()` into the shared SCTE-104 model.
pub fn decode_splice_insert_to_scte104(section: &[u8], current_pts_90k: u64) -> Option<Scte104Message> {
    if section.len() < 18 || section[0] != 0xfc || mpeg2_crc32(section) != 0 { return None; }
    let section_length = (((section[1] & 0x0f) as usize) << 8) | section[2] as usize;
    if section_length + 3 != section.len() || section[4] & 0x80 != 0 { return None; }
    let pts_adjustment = (((section[4] & 1) as u64) << 32)
        | u32::from_be_bytes(section[5..9].try_into().ok()?) as u64;
    let command_length = (((section[11] & 0x0f) as usize) << 8) | section[12] as usize;
    if section[13] != 0x05 || 14 + command_length + 6 > section.len() { return None; }
    let cmd = &section[14..14 + command_length];
    if cmd.len() < 5 { return None; }
    let event_id = u32::from_be_bytes(cmd[0..4].try_into().ok()?);
    if cmd[4] & 0x80 != 0 {
        return Some(Scte104Message { opcode: SpliceOpcode::SpliceCancel, splice_event_id: Some(event_id), pre_roll_ms: Some(0), break_duration_ms: None, auto_return: None, protocol_version: section[3] });
    }
    if cmd.len() < 6 || cmd[5] & 0x40 == 0 { return None; }
    let flags = cmd[5];
    let mut pos = 6;
    let pre_roll_ms = if flags & 0x10 != 0 {
        0
    } else {
        if pos + 5 > cmd.len() || cmd[pos] & 0x80 == 0 { return None; }
        let pts = (((cmd[pos] & 1) as u64) << 32)
            | u32::from_be_bytes(cmd[pos + 1..pos + 5].try_into().ok()?) as u64;
        pos += 5;
        let adjusted = pts.wrapping_add(pts_adjustment) & PTS_MASK;
        ((adjusted.wrapping_sub(current_pts_90k & PTS_MASK) & PTS_MASK) / 90)
            .min(u16::MAX as u64) as u16
    };
    let (break_duration_ms, auto_return) = if flags & 0x20 != 0 {
        if pos + 5 > cmd.len() { return None; }
        let ticks = (((cmd[pos] & 1) as u64) << 32)
            | u32::from_be_bytes(cmd[pos + 1..pos + 5].try_into().ok()?) as u64;
        (Some((ticks / 90).min(u32::MAX as u64) as u32), Some(cmd[pos] & 0x80 != 0))
    } else { (None, None) };
    Some(Scte104Message {
        opcode: if flags & 0x80 != 0 { SpliceOpcode::SpliceStartNormal } else { SpliceOpcode::SpliceEndNormal },
        splice_event_id: Some(event_id), pre_roll_ms: Some(pre_roll_ms), break_duration_ms,
        auto_return, protocol_version: section[3],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(opcode: SpliceOpcode, pre_roll_ms: u16) -> Scte104Message {
        Scte104Message {
            opcode,
            splice_event_id: Some(0x12345678),
            pre_roll_ms: Some(pre_roll_ms),
            break_duration_ms: None,
            auto_return: None,
            protocol_version: 0,
        }
    }

    #[test]
    fn scheduled_cue_round_trips_through_tracker() {
        let section =
            build_splice_insert_section(&msg(SpliceOpcode::SpliceStartNormal, 1500), 90_000);
        let mut tracker = crate::engine::content_analysis::scte35::Scte35Tracker::new();
        tracker.set_pids(vec![0x1fc]);
        let mut payload = vec![0];
        payload.extend_from_slice(&section);
        tracker.observe(
            true,
            &payload,
            "test",
            &crate::manager::events::event_channel().0,
        );
        assert_eq!(tracker.last_command(), Some("splice_insert"));
        assert_eq!(tracker.last_pts(), Some(225_000));
        assert_eq!(mpeg2_crc32(&section), 0);
    }

    #[test]
    fn immediate_and_cancel_suppress_pts() {
        for m in [
            msg(SpliceOpcode::SpliceEndNormal, 0),
            msg(SpliceOpcode::SpliceCancel, 100),
        ] {
            let section = build_splice_insert_section(&m, 90_000);
            let mut tracker = crate::engine::content_analysis::scte35::Scte35Tracker::new();
            let mut payload = vec![0];
            payload.extend_from_slice(&section);
            tracker.observe(
                true,
                &payload,
                "test",
                &crate::manager::events::event_channel().0,
            );
            assert_eq!(tracker.last_pts(), None);
        }
    }

    #[test]
    fn decoder_round_trips_and_rejects_bad_crc() {
        let original = Scte104Message { opcode: SpliceOpcode::SpliceStartNormal, splice_event_id: Some(0xabcdef01), pre_roll_ms: Some(1250), break_duration_ms: Some(30_000), auto_return: Some(true), protocol_version: 0 };
        let mut section = build_splice_insert_section(&original, 900_000);
        assert_eq!(decode_splice_insert_to_scte104(&section, 900_000), Some(original));
        section[8] ^= 1;
        assert_eq!(decode_splice_insert_to_scte104(&section, 900_000), None);
    }
}
