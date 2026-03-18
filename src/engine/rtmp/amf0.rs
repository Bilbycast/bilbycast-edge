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

/// Decode all AMF0 values from a byte slice.
pub fn decode_all(data: &[u8]) -> Result<Vec<Amf0Value>> {
    let mut pos = 0;
    let mut values = Vec::new();
    while pos < data.len() {
        let (val, new_pos) = decode_value(data, pos)?;
        values.push(val);
        pos = new_pos;
    }
    Ok(values)
}

/// Decode a single AMF0 value starting at `pos`. Returns `(value, new_pos)`.
fn decode_value(data: &[u8], pos: usize) -> Result<(Amf0Value, usize)> {
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
        MARKER_OBJECT => decode_object(data, pos),
        MARKER_NULL => Ok((Amf0Value::Null, pos)),
        // Ecma Array (marker 0x08) — some servers use this in _result.
        // It has a 4-byte "count" hint followed by key-value pairs + object-end.
        0x08 => {
            ensure_remaining(data, pos, 4)?;
            // Skip the 4-byte count; parse like a regular object.
            decode_object(data, pos + 4)
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
fn decode_object(data: &[u8], mut pos: usize) -> Result<(Amf0Value, usize)> {
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
        let (val, new_pos) = decode_value(data, pos)
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
    fn round_trip_number() {
        let val = Amf0Value::Number(42.5);
        let encoded = encode_values(&[val.clone()]);
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn round_trip_string() {
        let val = Amf0Value::String("connect".into());
        let encoded = encode_values(&[val.clone()]);
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
        let encoded = encode_values(&[val.clone()]);
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn round_trip_null() {
        let val = Amf0Value::Null;
        let encoded = encode_values(&[val.clone()]);
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
