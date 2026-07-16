// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(dead_code)]

//! Minimal SCTE-104 ad-marker parser, scoped to what bilbycast surfaces as
//! events on the management plane.
//!
//! SCTE-104 messages travel inside SMPTE 2010 / RFC 8331 ANC packets with
//! DID/SDID `0x41/0x07`. The full SCTE-104 specification covers a wide range
//! of multiple-operation messages and timing data; bilbycast intentionally
//! decodes only the bits the manager UI needs to display ad markers and
//! correlate them with flow events:
//!
//! - The single-operation `splice_request_data()` opcodes that signal cue-out,
//!   cue-in, splice-cancel, and the splice-null heartbeat.
//! - Their `splice_event_id` and `pre_roll_time` so the UI can show the marker
//!   identifier and how far in the future the splice will occur.
//!
//! Anything more elaborate (multiple-operation messages, segmentation
//! descriptors, encrypted variants) is parsed only far enough to identify the
//! opcode and is then surfaced as `Event::Other`. Operators can hook the raw
//! bytes via the manager event log if they need richer decoding.
//!
//! ## Wire format (SCTE-104 §10)
//!
//! ```text
//! 16  message_size                : total bytes including this field
//!  8  protocol_version            : 0
//!  8  AS_index                    : authorizing source
//!  8  message_number              : sender-assigned tag
//!  8  DPI_PID_index
//! 16  SCTE35_protocol_version
//! ...
//! 16  opID
//! 16  data_length
//!  N  payload
//! ```
//!
//! For single-operation messages the parser only needs the leading
//! `protocol_version`, the `opID`, and (for cue-out / cue-in) the
//! `splice_event_id` (32 bits) and `pre_roll_time` (16 bits, milliseconds).
//!
//! Pure Rust, no dependencies.

use super::ancillary::AncPacket;

/// SCTE-104 single-operation opcodes that bilbycast surfaces as events.
///
/// Mirrors the IDs from SCTE-104 Table 7-1; only the four operationally
/// useful ones are listed. All other opIDs map to [`SpliceOpcode::Other`]
/// and carry the raw u16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpliceOpcode {
    /// `splice_null()` (heartbeat / keep-alive).
    SpliceNull,
    /// `splice_request_data()` with `splice_insert_type = 1` (start_normal /
    /// cue-out).
    SpliceStartNormal,
    /// `splice_request_data()` end_normal/end_immediate (cue-in).
    SpliceEndNormal,
    /// `splice_request_data()` with `splice_insert_type = 5` (cancel).
    SpliceCancel,
    /// Any other opID. The raw u16 is preserved.
    Other(u16),
}

impl SpliceOpcode {
    fn from_op_id(op: u16, splice_insert_type: Option<u8>) -> Self {
        match (op, splice_insert_type) {
            (0x0000, _) => SpliceOpcode::SpliceNull,
            (0x0101, Some(1 | 2)) => SpliceOpcode::SpliceStartNormal,
            (0x0101, Some(3 | 4)) => SpliceOpcode::SpliceEndNormal,
            (0x0101, Some(5)) => SpliceOpcode::SpliceCancel,
            (other, _) => SpliceOpcode::Other(other),
        }
    }
}

/// Build a byte-aligned single-operation `splice_request_data()` message.
/// Header routing fields not represented by [`Scte104Message`] are zero.
pub fn build_scte104_message(msg: &Scte104Message) -> Vec<u8> {
    let immediate = msg.pre_roll_ms.unwrap_or(0) == 0;
    let splice_type = match msg.opcode {
        SpliceOpcode::SpliceStartNormal => if immediate { 2 } else { 1 },
        SpliceOpcode::SpliceEndNormal => if immediate { 4 } else { 3 },
        SpliceOpcode::SpliceCancel => 5,
        _ => 0,
    };
    let mut payload = Vec::with_capacity(13);
    payload.push(splice_type);
    payload.extend_from_slice(&msg.splice_event_id.unwrap_or(0).to_be_bytes());
    payload.push(0);
    payload.extend_from_slice(&msg.pre_roll_ms.unwrap_or(0).to_be_bytes());
    let tenths = msg.break_duration_ms.unwrap_or(0).div_ceil(100).min(u16::MAX as u32);
    payload.extend_from_slice(&(tenths as u16).to_be_bytes());
    payload.extend_from_slice(&[0, 0, u8::from(msg.auto_return.unwrap_or(false))]);

    let size = 12 + payload.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u16).to_be_bytes());
    out.push(msg.protocol_version);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0x0101u16.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decoded SCTE-104 single-operation message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scte104Message {
    pub opcode: SpliceOpcode,
    /// `splice_event_id` from the request payload (cue-out/cue-in only).
    pub splice_event_id: Option<u32>,
    /// `pre_roll_time` in milliseconds (cue-out/cue-in only).
    pub pre_roll_ms: Option<u16>,
    /// Break duration in milliseconds, when supplied by splice_request_data.
    pub break_duration_ms: Option<u32>,
    /// Whether downstream should return automatically after the break duration.
    pub auto_return: Option<bool>,
    /// Raw `protocol_version` from the message header.
    pub protocol_version: u8,
}

/// Errors returned by [`parse_scte104`].
#[derive(Debug, PartialEq, Eq)]
pub enum Scte104Error {
    Truncated,
    UnknownProtocolVersion(u8),
    /// Parser only handles single-operation messages today.
    Multiop,
}

impl std::fmt::Display for Scte104Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scte104Error::Truncated => f.write_str("SCTE-104 payload truncated"),
            Scte104Error::UnknownProtocolVersion(v) => {
                write!(f, "SCTE-104 protocol version {v} not supported")
            }
            Scte104Error::Multiop => {
                f.write_str("SCTE-104 multiple-operation messages are not parsed")
            }
        }
    }
}

impl std::error::Error for Scte104Error {}

/// True if the ANC packet carries an SCTE-104 message (DID `0x41`,
/// SDID `0x07`, per SMPTE 2010).
pub fn is_scte104_packet(p: &AncPacket) -> bool {
    p.did == 0x41 && p.sdid == 0x07
}

/// Parse the SCTE-104 message from an [`AncPacket::user_data`] byte slice.
///
/// Only single-operation messages are decoded today. Multi-operation messages
/// (`opID = 0xFFFF`) return [`Scte104Error::Multiop`] but the caller can still
/// surface that as a generic event.
pub fn parse_scte104(udw: &[u8]) -> Result<Scte104Message, Scte104Error> {
    // The minimum header is 12 bytes (message_size 2, protocol_version 1,
    // AS_index 1, message_number 1, DPI_PID_index 1, SCTE35_protocol_version 2,
    // opID 2, data_length 2).
    if udw.len() < 12 {
        return Err(Scte104Error::Truncated);
    }
    let _message_size = u16::from_be_bytes([udw[0], udw[1]]);
    let protocol_version = udw[2];
    if protocol_version != 0 {
        return Err(Scte104Error::UnknownProtocolVersion(protocol_version));
    }
    let _as_index = udw[3];
    let _message_number = udw[4];
    let _dpi_pid_index = udw[5];
    let _scte35_pv = u16::from_be_bytes([udw[6], udw[7]]);
    let op_id = u16::from_be_bytes([udw[8], udw[9]]);
    let data_length = u16::from_be_bytes([udw[10], udw[11]]) as usize;

    if op_id == 0xFFFF {
        return Err(Scte104Error::Multiop);
    }

    // Sanity-check declared data length.
    let payload_start = 12;
    let payload_end = payload_start + data_length;
    if payload_end > udw.len() {
        return Err(Scte104Error::Truncated);
    }
    let payload = &udw[payload_start..payload_end];

    // splice_request_data layout (SCTE-104 §10.3.3.1):
    //  8 splice_insert_type
    // 32 splice_event_id
    //  8 unique_program_id
    // 16 pre_roll_time
    // 16 break_duration (1/10 sec)
    //  8 avail_num
    //  8 avails_expected
    //  8 auto_return_flag
    let (splice_insert_type, splice_event_id, pre_roll_ms, break_duration_ms, auto_return) =
        if op_id == 0x0101 {
            if payload.len() < 1 {
                (None, None, None, None, None)
            } else {
                let sit = payload[0];
                let event_id = if payload.len() >= 5 {
                    Some(u32::from_be_bytes(payload[1..5].try_into().unwrap()))
                } else {
                    None
                };
                let pre_roll = if payload.len() >= 8 {
                    Some(u16::from_be_bytes([payload[6], payload[7]]))
                } else {
                    None
                };
                let duration = (payload.len() >= 10)
                    .then(|| u16::from_be_bytes([payload[8], payload[9]]) as u32 * 100);
                let duration = duration.filter(|v| *v != 0);
                let auto_return =
                    (payload.len() >= 13 && duration.is_some()).then(|| payload[12] != 0);
                (
                    Some(sit),
                    event_id,
                    pre_roll,
                    duration,
                    auto_return,
                )
            }
        } else {
            (None, None, None, None, None)
        };

    Ok(Scte104Message {
        opcode: SpliceOpcode::from_op_id(op_id, splice_insert_type),
        splice_event_id,
        pre_roll_ms,
        break_duration_ms,
        auto_return,
        protocol_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal single-op `splice_request_data` payload for tests.
    fn make_splice_request(splice_type: u8, event_id: u32, pre_roll: u16) -> Vec<u8> {
        // Header (12 bytes) + payload (13 bytes) = 25 bytes.
        let mut buf = Vec::with_capacity(25);
        buf.extend_from_slice(&25u16.to_be_bytes()); // message_size
        buf.push(0); // protocol_version
        buf.push(0); // AS_index
        buf.push(0); // message_number
        buf.push(0); // DPI_PID_index
        buf.extend_from_slice(&0u16.to_be_bytes()); // SCTE35_protocol_version
        buf.extend_from_slice(&0x0101u16.to_be_bytes()); // opID
        buf.extend_from_slice(&13u16.to_be_bytes()); // data_length

        buf.push(splice_type); // splice_insert_type
        buf.extend_from_slice(&event_id.to_be_bytes()); // splice_event_id
        buf.push(0); // unique_program_id
        buf.extend_from_slice(&pre_roll.to_be_bytes()); // pre_roll_time
        buf.extend_from_slice(&0u16.to_be_bytes()); // break_duration
        buf.push(0); // avail_num
        buf.push(0); // avails_expected
        buf.push(0); // auto_return
        buf
    }

    #[test]
    fn test_parse_break_duration_and_auto_return() {
        let mut raw = make_splice_request(1, 7, 500);
        raw[20..22].copy_from_slice(&300u16.to_be_bytes());
        raw[24] = 1;
        let msg = parse_scte104(&raw).unwrap();
        assert_eq!(msg.break_duration_ms, Some(30_000));
        assert_eq!(msg.auto_return, Some(true));
    }

    #[test]
    fn test_parse_cue_out() {
        let raw = make_splice_request(1, 0xCAFEBABE, 1500);
        let msg = parse_scte104(&raw).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceStartNormal);
        assert_eq!(msg.splice_event_id, Some(0xCAFEBABE));
        assert_eq!(msg.pre_roll_ms, Some(1500));
    }

    #[test]
    fn test_parse_cue_in() {
        let raw = make_splice_request(4, 1, 0);
        let msg = parse_scte104(&raw).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceEndNormal);
        assert_eq!(msg.splice_event_id, Some(1));
    }

    #[test]
    fn test_parse_cancel() {
        let raw = make_splice_request(5, 99, 0);
        let msg = parse_scte104(&raw).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceCancel);
    }

    #[test]
    fn test_build_message_round_trips() {
        for msg in [
            Scte104Message { opcode: SpliceOpcode::SpliceStartNormal, splice_event_id: Some(1), pre_roll_ms: Some(1500), break_duration_ms: Some(30_000), auto_return: Some(true), protocol_version: 0 },
            Scte104Message { opcode: SpliceOpcode::SpliceStartNormal, splice_event_id: Some(2), pre_roll_ms: Some(0), break_duration_ms: None, auto_return: None, protocol_version: 0 },
            Scte104Message { opcode: SpliceOpcode::SpliceEndNormal, splice_event_id: Some(3), pre_roll_ms: Some(500), break_duration_ms: None, auto_return: None, protocol_version: 0 },
            Scte104Message { opcode: SpliceOpcode::SpliceCancel, splice_event_id: Some(4), pre_roll_ms: Some(0), break_duration_ms: None, auto_return: None, protocol_version: 0 },
        ] {
            assert_eq!(parse_scte104(&build_scte104_message(&msg)).unwrap(), msg);
        }
    }

    #[test]
    fn test_parse_null_heartbeat() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&13u16.to_be_bytes());
        buf.push(0); // pv
        buf.push(0); // AS
        buf.push(0); // msg num
        buf.push(0); // DPI
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&0x0000u16.to_be_bytes()); // splice_null opID
        buf.extend_from_slice(&0u16.to_be_bytes()); // data_length
        let msg = parse_scte104(&buf).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceNull);
    }

    #[test]
    fn test_parse_unknown_opid_other() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&13u16.to_be_bytes());
        buf.push(0);
        buf.push(0);
        buf.push(0);
        buf.push(0);
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&0x0202u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        let msg = parse_scte104(&buf).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::Other(0x0202));
    }

    #[test]
    fn test_parse_multiop_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&13u16.to_be_bytes());
        buf.push(0);
        buf.push(0);
        buf.push(0);
        buf.push(0);
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&0xFFFFu16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        assert_eq!(parse_scte104(&buf), Err(Scte104Error::Multiop));
    }

    #[test]
    fn test_parse_truncated() {
        assert_eq!(parse_scte104(&[0u8; 5]), Err(Scte104Error::Truncated));
    }

    #[test]
    fn test_is_scte104_packet() {
        let p = AncPacket::simple(0x41, 0x07, 9, vec![]);
        assert!(is_scte104_packet(&p));
        let p = AncPacket::simple(0x60, 0x60, 9, vec![]);
        assert!(!is_scte104_packet(&p));
    }
}
