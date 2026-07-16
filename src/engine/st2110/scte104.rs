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
//! ## Wire format (ANSI/SCTE 104)
//!
//! The leading 16-bit field discriminates the two message framings — a
//! `single_operation_message` carries a real opID there, a
//! `multiple_operation_message` carries the `0xFFFF` sentinel. Real ad-splice
//! inserters emit the multi-op form in VANC; this codec parses both and emits
//! the multi-op form.
//!
//! ```text
//! single_operation_message()          multiple_operation_message()
//!   16  opID (≠ 0xFFFF)                  16  rsvd (= 0xFFFF)
//!   16  message_size                     16  message_size
//!   16  result                            8  protocol_version
//!   16  result_extension                  8  AS_index
//!    8  protocol_version                  8  message_number
//!    8  AS_index                         16  DPI_PID_index
//!    8  message_number                    8  SCTE35_protocol_version
//!   16  DPI_PID_index                        timestamp()   (1 + 0/6/4/2 bytes)
//!        data()  (the operation)           8  num_ops
//!                                             num_ops × { 16 opID, 16 data_length, data }
//! ```
//!
//! The `splice_request_data()` operation (opID `0x0101`), 14 bytes:
//! `splice_insert_type(8) splice_event_id(32) unique_program_id(16)
//! pre_roll_time(16, ms) break_duration(16, tenths of a second) avail_num(8)
//! avails_expected(8) auto_return_flag(8)`. `splice_null()` is opID `0x0102`.
//! Field layout and opID values follow the libklvanc reference implementation.
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

/// SCTE-104 operation IDs (ANSI/SCTE 104; values match the libklvanc reference
/// implementation and real broadcast equipment).
const OPID_SPLICE_REQUEST: u16 = 0x0101; // MO_SPLICE_REQUEST_DATA
const OPID_SPLICE_NULL: u16 = 0x0102; // MO_SPLICE_NULL_REQUEST_DATA
/// A `multiple_operation_message` carries this sentinel in the leading 16-bit
/// field where a `single_operation_message` carries a real (non-0xFFFF) opID.
const MULTI_OP_SENTINEL: u16 = 0xFFFF;

impl SpliceOpcode {
    fn from_op_id(op: u16, splice_insert_type: Option<u8>) -> Self {
        match (op, splice_insert_type) {
            (OPID_SPLICE_NULL, _) => SpliceOpcode::SpliceNull,
            (OPID_SPLICE_REQUEST, Some(1 | 2)) => SpliceOpcode::SpliceStartNormal,
            (OPID_SPLICE_REQUEST, Some(3 | 4)) => SpliceOpcode::SpliceEndNormal,
            (OPID_SPLICE_REQUEST, Some(5)) => SpliceOpcode::SpliceCancel,
            (other, _) => SpliceOpcode::Other(other),
        }
    }
}

/// Build a standard ANSI/SCTE-104 `multiple_operation_message` carrying a
/// single operation (a `splice_request_data()` or `splice_null()`), ready to
/// travel in a SMPTE 2010 / RFC 8331 ANC packet (DID/SDID `0x41/0x07`).
///
/// This is the framing real broadcast equipment (Evertz, Ross, …) emits and
/// expects — a multiple-operation message with `num_ops = 1` and a `time_type
/// = 0` (immediate) timestamp. Routing fields not represented by
/// [`Scte104Message`] (`AS_index`, `message_number`, `DPI_PID_index`,
/// `unique_program_id`, `avail_num`) are emitted as zero.
pub fn build_scte104_message(msg: &Scte104Message) -> Vec<u8> {
    let (op_id, op_data) = match msg.opcode {
        SpliceOpcode::SpliceNull => (OPID_SPLICE_NULL, Vec::new()),
        SpliceOpcode::SpliceStartNormal
        | SpliceOpcode::SpliceEndNormal
        | SpliceOpcode::SpliceCancel => (OPID_SPLICE_REQUEST, build_splice_request_data(msg)),
        SpliceOpcode::Other(op) => (op, Vec::new()),
    };

    let mut out = Vec::with_capacity(14 + op_data.len());
    out.extend_from_slice(&MULTI_OP_SENTINEL.to_be_bytes()); // rsvd = 0xFFFF (multi-op)
    out.extend_from_slice(&0u16.to_be_bytes()); // messageSize placeholder (filled below)
    out.push(msg.protocol_version); // protocol_version
    out.push(0); // AS_index
    out.push(0); // message_number
    out.extend_from_slice(&0u16.to_be_bytes()); // DPI_PID_index
    out.push(0); // SCTE35_protocol_version
    out.push(0); // timestamp: time_type = 0 (none / immediate)
    out.push(1); // num_ops = 1
    out.extend_from_slice(&op_id.to_be_bytes()); // opID
    out.extend_from_slice(&(op_data.len() as u16).to_be_bytes()); // data_length
    out.extend_from_slice(&op_data);

    // messageSize is the total byte count of the whole message.
    let size = out.len().min(u16::MAX as usize) as u16;
    out[2..4].copy_from_slice(&size.to_be_bytes());
    out
}

/// Encode the 14-byte `splice_request_data()` operation payload.
///
/// `pre_roll_time` is milliseconds and `break_duration` is tenths of a second,
/// per SCTE-104 (`splice_time = pts + pre_roll × 90`, `duration_90k =
/// break × 9000`).
fn build_splice_request_data(msg: &Scte104Message) -> Vec<u8> {
    let immediate = msg.pre_roll_ms.unwrap_or(0) == 0;
    let splice_insert_type = match msg.opcode {
        SpliceOpcode::SpliceStartNormal => if immediate { 2 } else { 1 },
        SpliceOpcode::SpliceEndNormal => if immediate { 4 } else { 3 },
        SpliceOpcode::SpliceCancel => 5,
        _ => 0,
    };
    let tenths = msg
        .break_duration_ms
        .unwrap_or(0)
        .div_ceil(100)
        .min(u16::MAX as u32) as u16;

    let mut d = Vec::with_capacity(14);
    d.push(splice_insert_type); // splice_insert_type (8)
    d.extend_from_slice(&msg.splice_event_id.unwrap_or(0).to_be_bytes()); // splice_event_id (32)
    d.extend_from_slice(&0u16.to_be_bytes()); // unique_program_id (16)
    d.extend_from_slice(&msg.pre_roll_ms.unwrap_or(0).to_be_bytes()); // pre_roll_time (16, ms)
    d.extend_from_slice(&tenths.to_be_bytes()); // break_duration (16, tenths of a second)
    d.push(0); // avail_num (8)
    d.push(0); // avails_expected (8)
    d.push(u8::from(msg.auto_return.unwrap_or(false))); // auto_return_flag (8)
    d
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
    /// A `timestamp()` with a `time_type` the parser can't size, so the
    /// operations that follow it can't be located.
    UnknownTimeType(u8),
}

impl std::fmt::Display for Scte104Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scte104Error::Truncated => f.write_str("SCTE-104 payload truncated"),
            Scte104Error::UnknownProtocolVersion(v) => {
                write!(f, "SCTE-104 protocol version {v} not supported")
            }
            Scte104Error::UnknownTimeType(t) => {
                write!(f, "SCTE-104 timestamp time_type {t} not supported")
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

/// Parse an SCTE-104 message from an [`AncPacket::user_data`] byte slice.
///
/// Handles both framings: a `single_operation_message` (leading opID ≠
/// `0xFFFF`) and a `multiple_operation_message` (leading `0xFFFF` sentinel) —
/// the latter is what real ad-splice inserters emit in VANC. For a multi-op
/// message the first `splice_request_data` / `splice_null` operation is
/// decoded; other operations (descriptors, DTMF, segmentation) are skipped,
/// and a message with no splice operation surfaces as [`SpliceOpcode::Other`]
/// so the caller can still log it.
pub fn parse_scte104(udw: &[u8]) -> Result<Scte104Message, Scte104Error> {
    if udw.len() < 2 {
        return Err(Scte104Error::Truncated);
    }
    if u16::from_be_bytes([udw[0], udw[1]]) == MULTI_OP_SENTINEL {
        parse_multiple_operation_message(udw)
    } else {
        parse_single_operation_message(udw)
    }
}

/// `single_operation_message()` header (13 bytes): opID(16), message_size(16),
/// result(16), result_extension(16), protocol_version(8), AS_index(8),
/// message_number(8), DPI_PID_index(16). The operation data follows directly.
fn parse_single_operation_message(udw: &[u8]) -> Result<Scte104Message, Scte104Error> {
    const HEADER: usize = 13;
    if udw.len() < HEADER {
        return Err(Scte104Error::Truncated);
    }
    let op_id = u16::from_be_bytes([udw[0], udw[1]]);
    let protocol_version = udw[8];
    if protocol_version != 0 {
        return Err(Scte104Error::UnknownProtocolVersion(protocol_version));
    }
    Ok(decode_operation(op_id, &udw[HEADER..], protocol_version))
}

/// `multiple_operation_message()`: rsvd(16)=`0xFFFF`, message_size(16),
/// protocol_version(8), AS_index(8), message_number(8), DPI_PID_index(16),
/// SCTE35_protocol_version(8), `timestamp()`, num_ops(8), then `num_ops` ×
/// { opID(16), data_length(16), data\[data_length\] }.
fn parse_multiple_operation_message(udw: &[u8]) -> Result<Scte104Message, Scte104Error> {
    // Fixed bytes 0..10 are rsvd..SCTE35_protocol_version; the timestamp's
    // `time_type` is at offset 10.
    const TS_OFFSET: usize = 10;
    if udw.len() < TS_OFFSET + 1 {
        return Err(Scte104Error::Truncated);
    }
    let protocol_version = udw[4];
    if protocol_version != 0 {
        return Err(Scte104Error::UnknownProtocolVersion(protocol_version));
    }
    let ts_len = timestamp_len(udw[TS_OFFSET])?;
    let mut pos = TS_OFFSET + 1 + ts_len; // past time_type + timestamp body
    let num_ops = *udw.get(pos).ok_or(Scte104Error::Truncated)?;
    pos += 1;

    let mut first_op_id: Option<u16> = None;
    for _ in 0..num_ops {
        if pos + 4 > udw.len() {
            return Err(Scte104Error::Truncated);
        }
        let op_id = u16::from_be_bytes([udw[pos], udw[pos + 1]]);
        let data_length = u16::from_be_bytes([udw[pos + 2], udw[pos + 3]]) as usize;
        pos += 4;
        if pos + data_length > udw.len() {
            return Err(Scte104Error::Truncated);
        }
        let data = &udw[pos..pos + data_length];
        pos += data_length;
        first_op_id.get_or_insert(op_id);
        if op_id == OPID_SPLICE_REQUEST || op_id == OPID_SPLICE_NULL {
            return Ok(decode_operation(op_id, data, protocol_version));
        }
    }

    // A valid multi-op message that carries no splice operation — surface it
    // generically rather than erroring, so the caller can log the opID.
    Ok(Scte104Message {
        opcode: SpliceOpcode::Other(first_op_id.unwrap_or(MULTI_OP_SENTINEL)),
        splice_event_id: None,
        pre_roll_ms: None,
        break_duration_ms: None,
        auto_return: None,
        protocol_version,
    })
}

/// Byte length of a `timestamp()` body *after* its 1-byte `time_type` field.
fn timestamp_len(time_type: u8) -> Result<usize, Scte104Error> {
    match time_type {
        0 => Ok(0), // none / immediate
        1 => Ok(6), // UTC_seconds(32) + UTC_microseconds(16)
        2 => Ok(4), // hours / minutes / seconds / frames, 8 bits each
        3 => Ok(2), // GPI_number(8) + GPI_edge(8)
        other => Err(Scte104Error::UnknownTimeType(other)),
    }
}

/// Decode one operation's `data` payload into a [`Scte104Message`].
///
/// `splice_request_data()` (14 bytes): splice_insert_type(8),
/// splice_event_id(32), unique_program_id(16), pre_roll_time(16, ms),
/// break_duration(16, tenths of a second), avail_num(8), avails_expected(8),
/// auto_return_flag(8). Shorter payloads are decoded as far as they reach.
fn decode_operation(op_id: u16, data: &[u8], protocol_version: u8) -> Scte104Message {
    let bare = |opcode| Scte104Message {
        opcode,
        splice_event_id: None,
        pre_roll_ms: None,
        break_duration_ms: None,
        auto_return: None,
        protocol_version,
    };
    if op_id == OPID_SPLICE_NULL {
        return bare(SpliceOpcode::SpliceNull);
    }
    if op_id != OPID_SPLICE_REQUEST || data.is_empty() {
        return bare(SpliceOpcode::Other(op_id));
    }
    let sit = data[0];
    let splice_event_id =
        (data.len() >= 5).then(|| u32::from_be_bytes(data[1..5].try_into().unwrap()));
    let pre_roll_ms = (data.len() >= 9).then(|| u16::from_be_bytes([data[7], data[8]]));
    let break_duration_ms = (data.len() >= 11)
        .then(|| u16::from_be_bytes([data[9], data[10]]) as u32 * 100)
        .filter(|v| *v != 0);
    let auto_return = (data.len() >= 14 && break_duration_ms.is_some()).then(|| data[13] != 0);
    Scte104Message {
        opcode: SpliceOpcode::from_op_id(op_id, Some(sit)),
        splice_event_id,
        pre_roll_ms,
        break_duration_ms,
        auto_return,
        protocol_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 14-byte `splice_request_data()` operation payload.
    fn splice_request_data(
        splice_type: u8,
        event_id: u32,
        pre_roll_ms: u16,
        break_tenths: u16,
        auto_return: u8,
    ) -> Vec<u8> {
        let mut d = Vec::with_capacity(14);
        d.push(splice_type); // splice_insert_type
        d.extend_from_slice(&event_id.to_be_bytes()); // splice_event_id
        d.extend_from_slice(&0u16.to_be_bytes()); // unique_program_id
        d.extend_from_slice(&pre_roll_ms.to_be_bytes()); // pre_roll_time (ms)
        d.extend_from_slice(&break_tenths.to_be_bytes()); // break_duration (tenths)
        d.push(0); // avail_num
        d.push(0); // avails_expected
        d.push(auto_return); // auto_return_flag
        d
    }

    /// A standard `single_operation_message` carrying one splice op.
    fn single_op_splice(splice_type: u8, event_id: u32, pre_roll_ms: u16) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&OPID_SPLICE_REQUEST.to_be_bytes()); // opID (≠ 0xFFFF)
        buf.extend_from_slice(&0u16.to_be_bytes()); // message_size (not validated by parser)
        buf.extend_from_slice(&0u16.to_be_bytes()); // result
        buf.extend_from_slice(&0u16.to_be_bytes()); // result_extension
        buf.push(0); // protocol_version
        buf.push(0); // AS_index
        buf.push(0); // message_number
        buf.extend_from_slice(&0u16.to_be_bytes()); // DPI_PID_index
        buf.extend_from_slice(&splice_request_data(splice_type, event_id, pre_roll_ms, 0, 0));
        buf
    }

    /// A standard `multiple_operation_message` with the given timestamp body
    /// and operations — the framing real ad-splice inserters emit in VANC.
    fn multi_op(time_type: u8, ts_body: &[u8], ops: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MULTI_OP_SENTINEL.to_be_bytes()); // rsvd = 0xFFFF
        buf.extend_from_slice(&0u16.to_be_bytes()); // message_size
        buf.push(0); // protocol_version
        buf.push(0); // AS_index
        buf.push(0); // message_number
        buf.extend_from_slice(&0u16.to_be_bytes()); // DPI_PID_index
        buf.push(0); // SCTE35_protocol_version
        buf.push(time_type); // timestamp: time_type
        buf.extend_from_slice(ts_body); // timestamp body
        buf.push(ops.len() as u8); // num_ops
        for (op_id, data) in ops {
            buf.extend_from_slice(&op_id.to_be_bytes());
            buf.extend_from_slice(&(data.len() as u16).to_be_bytes());
            buf.extend_from_slice(data);
        }
        buf
    }

    #[test]
    fn parse_single_op_cue_out() {
        let msg = parse_scte104(&single_op_splice(1, 0xCAFEBABE, 1500)).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceStartNormal);
        assert_eq!(msg.splice_event_id, Some(0xCAFEBABE));
        assert_eq!(msg.pre_roll_ms, Some(1500));
    }

    #[test]
    fn parse_single_op_cue_in_immediate() {
        let msg = parse_scte104(&single_op_splice(4, 1, 0)).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceEndNormal);
        assert_eq!(msg.splice_event_id, Some(1));
    }

    #[test]
    fn parse_single_op_cancel() {
        let msg = parse_scte104(&single_op_splice(5, 99, 0)).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceCancel);
        assert_eq!(msg.splice_event_id, Some(99));
    }

    #[test]
    fn parse_break_duration_and_auto_return() {
        // 300 tenths of a second = 30_000 ms; auto_return flag set.
        let raw = multi_op(
            0,
            &[],
            &[(OPID_SPLICE_REQUEST, splice_request_data(1, 7, 500, 300, 1))],
        );
        let msg = parse_scte104(&raw).unwrap();
        assert_eq!(msg.break_duration_ms, Some(30_000));
        assert_eq!(msg.auto_return, Some(true));
    }

    /// A `multiple_operation_message` as real broadcast equipment emits it:
    /// `0xFFFF` sentinel, immediate timestamp, one `splice_request_data`.
    #[test]
    fn parse_real_multi_op_cue_out() {
        let raw = multi_op(
            0,
            &[],
            &[(OPID_SPLICE_REQUEST, splice_request_data(1, 0x1234, 2000, 300, 1))],
        );
        assert_eq!(&raw[0..2], &[0xFF, 0xFF]); // multi-op sentinel
        let msg = parse_scte104(&raw).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceStartNormal);
        assert_eq!(msg.splice_event_id, Some(0x1234));
        assert_eq!(msg.pre_roll_ms, Some(2000));
        assert_eq!(msg.break_duration_ms, Some(30_000));
        assert_eq!(msg.auto_return, Some(true));
    }

    /// A SMPTE-VITC timestamp (`time_type = 2`, 4 bytes) must be skipped so the
    /// operations after it are still located.
    #[test]
    fn parse_multi_op_skips_vitc_timestamp() {
        let raw = multi_op(
            2,
            &[10, 30, 15, 0], // hh:mm:ss:ff
            &[(OPID_SPLICE_REQUEST, splice_request_data(2, 42, 0, 0, 0))],
        );
        let msg = parse_scte104(&raw).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceStartNormal); // immediate
        assert_eq!(msg.splice_event_id, Some(42));
    }

    /// A non-splice operation ahead of the splice must not hide it.
    #[test]
    fn parse_multi_op_finds_splice_among_other_ops() {
        let raw = multi_op(
            0,
            &[],
            &[
                (0x0108, vec![0xAA, 0xBB]), // insert_descriptor — skipped
                (OPID_SPLICE_REQUEST, splice_request_data(3, 7, 500, 0, 0)),
            ],
        );
        let msg = parse_scte104(&raw).unwrap();
        assert_eq!(msg.opcode, SpliceOpcode::SpliceEndNormal);
        assert_eq!(msg.splice_event_id, Some(7));
    }

    #[test]
    fn parse_splice_null_is_opid_0x0102() {
        // splice_null (opID 0x0102) decodes to SpliceNull in both framings.
        let mut single = Vec::new();
        single.extend_from_slice(&OPID_SPLICE_NULL.to_be_bytes());
        single.extend_from_slice(&[0u8; 11]); // rest of the 13-byte header
        assert_eq!(parse_scte104(&single).unwrap().opcode, SpliceOpcode::SpliceNull);

        let multi = multi_op(0, &[], &[(OPID_SPLICE_NULL, Vec::new())]);
        assert_eq!(parse_scte104(&multi).unwrap().opcode, SpliceOpcode::SpliceNull);
    }

    #[test]
    fn parse_multi_op_without_splice_is_other() {
        let raw = multi_op(0, &[], &[(0x0108, vec![0x01, 0x02])]);
        assert_eq!(
            parse_scte104(&raw).unwrap().opcode,
            SpliceOpcode::Other(0x0108)
        );
    }

    #[test]
    fn build_emits_multi_op_and_round_trips() {
        for msg in [
            Scte104Message { opcode: SpliceOpcode::SpliceStartNormal, splice_event_id: Some(1), pre_roll_ms: Some(1500), break_duration_ms: Some(30_000), auto_return: Some(true), protocol_version: 0 },
            Scte104Message { opcode: SpliceOpcode::SpliceStartNormal, splice_event_id: Some(2), pre_roll_ms: Some(0), break_duration_ms: None, auto_return: None, protocol_version: 0 },
            Scte104Message { opcode: SpliceOpcode::SpliceEndNormal, splice_event_id: Some(3), pre_roll_ms: Some(500), break_duration_ms: None, auto_return: None, protocol_version: 0 },
            Scte104Message { opcode: SpliceOpcode::SpliceCancel, splice_event_id: Some(4), pre_roll_ms: Some(0), break_duration_ms: None, auto_return: None, protocol_version: 0 },
            Scte104Message { opcode: SpliceOpcode::SpliceNull, splice_event_id: None, pre_roll_ms: None, break_duration_ms: None, auto_return: None, protocol_version: 0 },
        ] {
            let wire = build_scte104_message(&msg);
            assert_eq!(&wire[0..2], &[0xFF, 0xFF], "emits the standard multi-op framing");
            // messageSize is the true total length.
            assert_eq!(u16::from_be_bytes([wire[2], wire[3]]) as usize, wire.len());
            assert_eq!(parse_scte104(&wire).unwrap(), msg);
        }
    }

    #[test]
    fn parse_rejects_bad_protocol_version_and_truncation() {
        let mut raw = single_op_splice(1, 1, 0);
        raw[8] = 9; // single-op protocol_version
        assert_eq!(
            parse_scte104(&raw),
            Err(Scte104Error::UnknownProtocolVersion(9))
        );
        assert_eq!(parse_scte104(&[0u8; 1]), Err(Scte104Error::Truncated));
        assert_eq!(
            parse_scte104(&[0xFF, 0xFF, 0x00]),
            Err(Scte104Error::Truncated)
        );
    }

    #[test]
    fn test_is_scte104_packet() {
        let p = AncPacket::simple(0x41, 0x07, 9, vec![]);
        assert!(is_scte104_packet(&p));
        let p = AncPacket::simple(0x60, 0x60, 9, vec![]);
        assert!(!is_scte104_packet(&p));
    }
}
