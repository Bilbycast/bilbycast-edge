// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! AMF0 (Action Message Format version 0) encoder and decoder.
//!
//! AMF0 is used in RTMP command messages (`msg_type 20`) to encode method names,
//! transaction IDs, and command/response objects. This module implements the
//! subset of AMF0 needed for RTMP publishing:
//!
//! ## Encoded types
//! - **Number** (marker `0x00`): IEEE 754 double (8 bytes, big-endian)
//! - **Boolean** (marker `0x01`): 1 byte (0 = false, 1 = true)
//! - **String** (marker `0x02`): UTF-8 with 2-byte length prefix
//! - **Object** (marker `0x03`): sequence of `[key: AMF0 String (no marker)] [value: AMF0 Value]`,
//!   terminated by `0x00 0x00 0x09` (empty key + object-end marker)
//! - **Null** (marker `0x05`)
//!
//! ## Decoded types
//! We decode enough to handle server responses: `_result`, `_error`, `onStatus`.

use anyhow::{Context, Result, bail};
use bytes::{BufMut, BytesMut};

// ---------------------------------------------------------------------------
// AMF0 type markers
// ---------------------------------------------------------------------------

const MARKER_NUMBER: u8 = 0x00;
const MARKER_BOOLEAN: u8 = 0x01;
const MARKER_STRING: u8 = 0x02;
const MARKER_OBJECT: u8 = 0x03;
const MARKER_NULL: u8 = 0x05;
const MARKER_OBJECT_END: u8 = 0x09;

// ---------------------------------------------------------------------------
// Value type
// ---------------------------------------------------------------------------

/// An AMF0 value. Covers the subset used in RTMP command messages.
#[derive(Debug, Clone, PartialEq)]
pub enum Amf0Value {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, Amf0Value)>),
    Null,
}

impl Amf0Value {
    /// Convenience: try to interpret this value as a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Amf0Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Convenience: try to interpret this value as a number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Amf0Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Look up a property inside an Object value.
    pub fn get_property(&self, key: &str) -> Option<&Amf0Value> {
        match self {
            Amf0Value::Object(props) => props.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Encode a single AMF0 value into the buffer.
pub fn encode(buf: &mut BytesMut, value: &Amf0Value) {
    match value {
        Amf0Value::Number(n) => {
            buf.put_u8(MARKER_NUMBER);
            buf.put_f64(*n);
        }
        Amf0Value::Boolean(b) => {
            buf.put_u8(MARKER_BOOLEAN);
            buf.put_u8(if *b { 1 } else { 0 });
        }
        Amf0Value::String(s) => {
            encode_string(buf, s);
        }
        Amf0Value::Object(props) => {
            buf.put_u8(MARKER_OBJECT);
            for (key, val) in props {
                // Object keys are AMF0 strings WITHOUT the type marker.
                encode_raw_string(buf, key);
                encode(buf, val);
            }
            // Object end: empty key (0x00 0x00) + object-end marker (0x09).
            buf.put_u16(0);
            buf.put_u8(MARKER_OBJECT_END);
        }
        Amf0Value::Null => {
            buf.put_u8(MARKER_NULL);
        }
    }
}

/// Encode an AMF0 string with its type marker.
fn encode_string(buf: &mut BytesMut, s: &str) {
    buf.put_u8(MARKER_STRING);
    encode_raw_string(buf, s);
}

/// Encode a raw AMF0 string (2-byte length + UTF-8 bytes), without the type marker.
fn encode_raw_string(buf: &mut BytesMut, s: &str) {
    let bytes = s.as_bytes();
    buf.put_u16(bytes.len() as u16);
    buf.put_slice(bytes);
}

/// Encode a sequence of AMF0 values into a new buffer and return the bytes.
pub fn encode_values(values: &[Amf0Value]) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(256);
    for v in values {
        encode(&mut buf, v);
    }
    buf.to_vec()
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Maximum AMF0 object/array nesting depth we will decode. AMF0 objects nest
/// via `decode_object` → `decode_value` → `decode_object`; an unauthenticated
/// RTMP `connect` payload of deeply-nested empty objects would otherwise
/// recurse until the thread stack overflows and aborts the whole process.
/// Real RTMP command payloads nest only a handful of levels.
const MAX_AMF0_DEPTH: u32 = 32;

/// Maximum number of AMF0 *values* one `decode_all` call will materialise.
///
/// [`MAX_AMF0_DEPTH`] bounds **recursion**, not **breadth** — it stops a chain
/// of nested objects from overflowing the stack, but says nothing about how
/// many siblings live at one level. That gap is a memory-amplification
/// primitive on the unauthenticated pre-publish path: every 3 input bytes
/// (`00 00 05` — empty key, `Null` value) become one `(String, Amf0Value)`
/// property, i.e. 56 bytes of `Vec` element, and a bare `05` at top level buys
/// a 32-byte enum slot per *byte*. With the 24-bit RTMP `message_length` that
/// is ~16 MiB of wire turning into hundreds of MiB of heap on a connection
/// that has presented no credential, times `MAX_RTMP_CONNECTIONS` — straight
/// to the OOM killer.
///
/// The budget is threaded through `decode_all` → `decode_value` →
/// `decode_object` and charged **once per decoded value**, so it bounds the
/// total across every nesting level rather than per container.
///
/// 50 000 is far above anything real. A `connect` from OBS / ffmpeg / Wirecast
/// carries a dozen-odd object properties (~15 values); `publish` carries 4;
/// a live `onMetaData` carries ~20. The largest AMF0 payload that exists in
/// the wild at all is an FLV **VOD** `onMetaData` keyframe index
/// (`filepositions` + `times`) — a file construct a live publisher cannot even
/// produce — and a two-hour asset at a 2 s GOP would still only reach ~14 400
/// values. Tripping this limit means the payload is not an RTMP command.
///
/// This is one of three bounds on the same unauthenticated path and none of
/// them subsumes the others: `MAX_PREPUBLISH_MSG_LEN` bounds how many wire
/// bytes reach this decoder, this budget bounds what those bytes expand into,
/// and `MAX_CHUNK_STREAMS` bounds the per-connection state a peer can retain
/// without sending a body at all.
const MAX_AMF0_VALUES: usize = 50_000;

/// Decode all AMF0 values from a byte slice.
pub fn decode_all(data: &[u8]) -> Result<Vec<Amf0Value>> {
    let mut pos = 0;
    let mut values = Vec::new();
    let mut budget = MAX_AMF0_VALUES;
    while pos < data.len() {
        let (val, new_pos) = decode_value(data, pos, 0, &mut budget)?;
        values.push(val);
        pos = new_pos;
    }
    Ok(values)
}

/// Decode a single AMF0 value starting at `pos`. Returns `(value, new_pos)`.
/// `depth` bounds object/array nesting to defeat stack-overflow DoS; `budget`
/// bounds the total number of values decoded (see [`MAX_AMF0_VALUES`]) to
/// defeat sibling-flood memory amplification, which `depth` does not see.
fn decode_value(
    data: &[u8],
    pos: usize,
    depth: u32,
    budget: &mut usize,
) -> Result<(Amf0Value, usize)> {
    if depth > MAX_AMF0_DEPTH {
        bail!("AMF0: nesting depth exceeds {MAX_AMF0_DEPTH} — refusing to decode (possible malicious payload)");
    }
    if *budget == 0 {
        bail!("AMF0: decoded value count exceeds {MAX_AMF0_VALUES} — refusing to decode (possible malicious payload)");
    }
    *budget -= 1;
    if pos >= data.len() {
        bail!("AMF0: unexpected end of data at position {pos}");
    }
    let marker = data[pos];
    let pos = pos + 1;

    match marker {
        MARKER_NUMBER => {
            ensure_remaining(data, pos, 8)?;
            let n = f64::from_be_bytes(data[pos..pos + 8].try_into().unwrap());
            Ok((Amf0Value::Number(n), pos + 8))
        }
        MARKER_BOOLEAN => {
            ensure_remaining(data, pos, 1)?;
            Ok((Amf0Value::Boolean(data[pos] != 0), pos + 1))
        }
        MARKER_STRING => {
            let (s, new_pos) = decode_raw_string(data, pos)?;
            Ok((Amf0Value::String(s), new_pos))
        }
        MARKER_OBJECT => decode_object(data, pos, depth, budget),
        MARKER_NULL => Ok((Amf0Value::Null, pos)),
        // Ecma Array (marker 0x08) — some servers use this in _result.
        // It has a 4-byte "count" hint followed by key-value pairs + object-end.
        0x08 => {
            ensure_remaining(data, pos, 4)?;
            // Skip the 4-byte count; parse like a regular object. The count is
            // deliberately NOT used to pre-reserve — it is attacker-controlled
            // and need not match the number of pairs that follow.
            decode_object(data, pos + 4, depth, budget)
        }
        other => {
            // Skip unknown types gracefully — consume remaining data.
            tracing::warn!("AMF0: unknown marker 0x{other:02X} at position {}", pos - 1);
            Ok((Amf0Value::Null, data.len()))
        }
    }
}

/// Decode a raw AMF0 string (2-byte length + bytes) without type marker.
fn decode_raw_string(data: &[u8], pos: usize) -> Result<(String, usize)> {
    ensure_remaining(data, pos, 2)?;
    let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
    let start = pos + 2;
    ensure_remaining(data, start, len)?;
    let s = String::from_utf8_lossy(&data[start..start + len]).into_owned();
    Ok((s, start + len))
}

/// Decode an AMF0 object (key-value pairs terminated by empty-key + 0x09).
/// `depth` is the current nesting level; each contained value is decoded at
/// `depth + 1` so [`decode_value`] can bound total recursion. `budget` is the
/// shared value allowance — each property's value charges it, which is what
/// bounds a flat flood of siblings (see [`MAX_AMF0_VALUES`]).
fn decode_object(
    data: &[u8],
    mut pos: usize,
    depth: u32,
    budget: &mut usize,
) -> Result<(Amf0Value, usize)> {
    let mut props = Vec::new();
    loop {
        // Check for object-end marker: 0x00 0x00 0x09
        if pos + 3 <= data.len() && data[pos] == 0x00 && data[pos + 1] == 0x00 && data[pos + 2] == MARKER_OBJECT_END {
            return Ok((Amf0Value::Object(props), pos + 3));
        }
        // Read key (raw string without type marker).
        let (key, new_pos) = decode_raw_string(data, pos)
            .context("AMF0 object: failed to decode key")?;
        pos = new_pos;
        // Read value.
        let (val, new_pos) = decode_value(data, pos, depth + 1, budget)
            .context("AMF0 object: failed to decode value")?;
        pos = new_pos;
        props.push((key, val));

        // Safety: prevent infinite loops on malformed data.
        if pos >= data.len() {
            break;
        }
    }
    Ok((Amf0Value::Object(props), pos))
}

fn ensure_remaining(data: &[u8], pos: usize, need: usize) -> Result<()> {
    if pos + need > data.len() {
        bail!(
            "AMF0: need {need} bytes at position {pos}, but only {} remain",
            data.len() - pos
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeply_nested_object_is_rejected_not_overflow() {
        // Each `00 00 03` is an empty-key property whose value is another
        // object — i.e. one nesting level. Without the depth guard this many
        // levels overflow the thread stack and abort the process (the
        // unauthenticated RTMP DoS). With the guard, decode bails cleanly.
        let mut data = vec![MARKER_OBJECT];
        for _ in 0..5000 {
            data.extend_from_slice(&[0x00, 0x00, MARKER_OBJECT]);
        }
        let res = decode_all(&data);
        assert!(res.is_err(), "deeply nested AMF0 must be rejected, not overflow the stack");
    }

    #[test]
    fn sibling_object_property_flood_is_rejected_not_amplified() {
        // The unauthenticated amplification primitive: one `fmt 0` COMMAND_AMF0
        // chunk whose body is `03` followed by the 3-byte pattern `00 00 05`
        // (empty key, Null value) repeated to fill the 24-bit message_length.
        // Nesting depth stays at 1 the whole way, so MAX_AMF0_DEPTH never
        // fires; each 3 wire bytes buy a 56-byte `(String, Amf0Value)` slot.
        let mut data = vec![MARKER_OBJECT];
        for _ in 0..200_000 {
            data.extend_from_slice(&[0x00, 0x00, MARKER_NULL]);
        }
        let err = decode_all(&data).expect_err("sibling flood must be refused");
        assert!(
            format!("{err:#}").contains("decoded value count"),
            "expected the value-budget guard to fire, got: {err:#}"
        );
    }

    #[test]
    fn top_level_value_flood_is_rejected_not_amplified() {
        // Same primitive without even an object: a bare `05` (Null) is one
        // wire byte and one 32-byte enum slot in the returned Vec.
        let data = vec![MARKER_NULL; 200_000];
        let err = decode_all(&data).expect_err("top-level flood must be refused");
        assert!(
            format!("{err:#}").contains("decoded value count"),
            "expected the value-budget guard to fire, got: {err:#}"
        );
    }

    #[test]
    fn value_budget_boundary_admits_below_and_refuses_above() {
        // Pin the number, not just the off-by-one: written purely in terms of
        // MAX_AMF0_VALUES this test passes at any budget, including one wide
        // enough to reinstate the amplification primitive. The doc comment
        // argues 50 000 against a ~14 400-value worst case, so the number is
        // load-bearing and a change to it must break a test, not slip through.
        assert_eq!(
            MAX_AMF0_VALUES, 50_000,
            "the budget is load-bearing; see the doc comment on MAX_AMF0_VALUES"
        );
        assert!(decode_all(&vec![MARKER_NULL; 50_000]).is_ok());
        assert!(decode_all(&vec![MARKER_NULL; 50_001]).is_err());
    }

    #[test]
    fn realistic_large_on_metadata_is_still_accepted() {
        // Guards `decode_all` against over-tightening, using the largest AMF0
        // payload that exists anywhere as the yardstick. Note it is NOT
        // evidence about the server's `onMetaData` handling: the RTMP server
        // never AMF0-decodes DATA_AMF0 at all (`receive_media_loop` sends
        // `RtmpMediaMessage::Metadata` and drops the payload). The callers this
        // protects are the server's COMMAND_AMF0 path and the outbound client's
        // `_result` decode.
        //
        // A live publisher's onMetaData is ~20 values. The biggest AMF0
        // payload that exists anywhere is a VOD keyframe index; build one at
        // 2 000 keyframes (a 2 h asset at a ~3.6 s GOP) carried as two
        // parallel property sets, plus the usual encoder fields. ~4 000 values
        // and tens of KB of wire — comfortably inside the budget, and it must
        // decode byte-identically.
        let mut positions = Vec::with_capacity(2000);
        let mut times = Vec::with_capacity(2000);
        for i in 0..2000 {
            positions.push((format!("{i}"), Amf0Value::Number((i as f64) * 65_536.0)));
            times.push((format!("{i}"), Amf0Value::Number((i as f64) * 3.6)));
        }
        let meta = Amf0Value::Object(vec![
            ("duration".into(), Amf0Value::Number(7200.0)),
            ("width".into(), Amf0Value::Number(1920.0)),
            ("height".into(), Amf0Value::Number(1080.0)),
            ("videodatarate".into(), Amf0Value::Number(6000.0)),
            ("framerate".into(), Amf0Value::Number(50.0)),
            ("videocodecid".into(), Amf0Value::Number(7.0)),
            ("audiodatarate".into(), Amf0Value::Number(160.0)),
            ("audiosamplerate".into(), Amf0Value::Number(48000.0)),
            ("audiosamplesize".into(), Amf0Value::Number(16.0)),
            ("stereo".into(), Amf0Value::Boolean(true)),
            ("audiocodecid".into(), Amf0Value::Number(10.0)),
            ("encoder".into(), Amf0Value::String("obs-output module (libobs version 30.2.3)".into())),
            (
                "keyframes".into(),
                Amf0Value::Object(vec![
                    ("filepositions".into(), Amf0Value::Object(positions)),
                    ("times".into(), Amf0Value::Object(times)),
                ]),
            ),
        ]);
        let values = vec![
            Amf0Value::String("@setDataFrame".into()),
            Amf0Value::String("onMetaData".into()),
            meta,
        ];
        let encoded = encode_values(&values);
        assert!(
            encoded.len() > 40_000,
            "expected a genuinely large payload, got {} bytes",
            encoded.len()
        );
        let decoded = decode_all(&encoded).expect("a realistic large onMetaData must decode");
        assert_eq!(decoded, values);
    }

    #[test]
    fn round_trip_number() {
        let val = Amf0Value::Number(42.5);
        let encoded = encode_values(std::slice::from_ref(&val));
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn round_trip_string() {
        let val = Amf0Value::String("connect".into());
        let encoded = encode_values(std::slice::from_ref(&val));
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn round_trip_object() {
        let val = Amf0Value::Object(vec![
            ("app".into(), Amf0Value::String("live".into())),
            ("type".into(), Amf0Value::String("nonprivate".into())),
            ("flashVer".into(), Amf0Value::String("FMLE/3.0".into())),
        ]);
        let encoded = encode_values(std::slice::from_ref(&val));
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn round_trip_null() {
        let val = Amf0Value::Null;
        let encoded = encode_values(std::slice::from_ref(&val));
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn round_trip_mixed() {
        let values = vec![
            Amf0Value::String("_result".into()),
            Amf0Value::Number(1.0),
            Amf0Value::Object(vec![
                ("fmsVer".into(), Amf0Value::String("FMS/3,5,7,7009".into())),
                ("capabilities".into(), Amf0Value::Number(31.0)),
            ]),
            Amf0Value::Null,
        ];
        let encoded = encode_values(&values);
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, values);
    }
}
