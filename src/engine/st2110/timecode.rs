// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

#![allow(dead_code)]

//! SMPTE ST 12M ancillary timecode parser/serializer.
//!
//! ST 12M-2 LTC and ATC ("Ancillary Time Code") packets carry SMPTE timecode
//! over the RFC 8331 ANC pipeline. The user-data layout is the SMPTE 12M-2
//! payload format used by RP 188 / SMPTE 291M:
//!
//! ```text
//! UDW0  : DBB1 (Distributed Binary Bits 1)
//! UDW1  : DBB2 (Distributed Binary Bits 2)
//! UDW2  : Frames units      (low 4 bits)
//! UDW3  : Frames tens       (low 2 bits) + frame flags
//! UDW4  : Seconds units     (low 4 bits)
//! UDW5  : Seconds tens      (low 3 bits) + flags
//! UDW6  : Minutes units     (low 4 bits)
//! UDW7  : Minutes tens      (low 3 bits) + flags
//! UDW8  : Hours units       (low 4 bits)
//! UDW9  : Hours tens        (low 2 bits) + flags
//! ```
//!
//! Bilbycast only needs to extract the `HH:MM:SS:FF` value plus the drop-frame
//! flag for event annotation; the parser is intentionally minimal and ignores
//! the binary group bits. The serializer is symmetric so we can re-emit a
//! timecode unchanged on the output side.
//!
//! Pure Rust, no dependencies. Lives in [`crate::engine::st2110`] alongside
//! the other ANC payload decoders.

use super::ancillary::AncPacket;

/// Decoded SMPTE 12M-2 timecode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    /// Drop-frame flag (true for 29.97/59.94 drop-frame timecode).
    pub drop_frame: bool,
    /// Color frame flag.
    pub color_frame: bool,
}

impl Timecode {
    /// Format as `HH:MM:SS:FF` (drop-frame uses `;` between SS and FF, per
    /// SMPTE convention).
    pub fn to_string(&self) -> String {
        let sep = if self.drop_frame { ';' } else { ':' };
        format!(
            "{:02}:{:02}:{:02}{}{:02}",
            self.hours, self.minutes, self.seconds, sep, self.frames
        )
    }
}

/// Errors returned by [`parse_timecode`].
#[derive(Debug, PartialEq, Eq)]
pub enum TimecodeError {
    /// Fewer than 10 user-data words.
    Truncated,
    /// One of the BCD digits exceeded its allowed range.
    InvalidBcd,
}

impl std::fmt::Display for TimecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimecodeError::Truncated => f.write_str("ATC payload truncated"),
            TimecodeError::InvalidBcd => f.write_str("ATC contains invalid BCD digit"),
        }
    }
}

impl std::error::Error for TimecodeError {}

/// Parse an ATC ANC packet's user_data into a [`Timecode`].
///
/// The ATC DID/SDID is `0x60/0x60` (LTC) or `0x60/0x61` (VITC). This function
/// does not enforce the DID/SDID — pass `&pkt.user_data` directly. Validation
/// of the wrapping ANC packet should happen earlier (e.g. by checking the DID
/// against [`is_atc_packet`]).
pub fn parse_timecode(udw: &[u8]) -> Result<Timecode, TimecodeError> {
    if udw.len() < 10 {
        return Err(TimecodeError::Truncated);
    }
    let frames_u = udw[2] & 0x0F;
    let frames_t = udw[3] & 0x03;
    let drop_frame = (udw[3] & 0x04) != 0;
    let color_frame = (udw[3] & 0x08) != 0;
    let secs_u = udw[4] & 0x0F;
    let secs_t = udw[5] & 0x07;
    let mins_u = udw[6] & 0x0F;
    let mins_t = udw[7] & 0x07;
    let hours_u = udw[8] & 0x0F;
    let hours_t = udw[9] & 0x03;

    if frames_u > 9 || secs_u > 9 || mins_u > 9 || hours_u > 9 {
        return Err(TimecodeError::InvalidBcd);
    }

    let frames = frames_t * 10 + frames_u;
    let seconds = secs_t * 10 + secs_u;
    let minutes = mins_t * 10 + mins_u;
    let hours = hours_t * 10 + hours_u;
    if seconds >= 60 || minutes >= 60 || hours >= 24 {
        return Err(TimecodeError::InvalidBcd);
    }
    Ok(Timecode {
        hours,
        minutes,
        seconds,
        frames,
        drop_frame,
        color_frame,
    })
}

/// Serialize a [`Timecode`] back into the 10-byte SMPTE 12M user-data layout.
/// DBB1/DBB2 are zeroed.
pub fn serialize_timecode(tc: &Timecode) -> Vec<u8> {
    let mut out = vec![0u8; 10];
    out[2] = tc.frames % 10;
    let mut frames_t = tc.frames / 10;
    frames_t &= 0x03;
    out[3] = frames_t
        | if tc.drop_frame { 0x04 } else { 0 }
        | if tc.color_frame { 0x08 } else { 0 };
    out[4] = tc.seconds % 10;
    out[5] = (tc.seconds / 10) & 0x07;
    out[6] = tc.minutes % 10;
    out[7] = (tc.minutes / 10) & 0x07;
    out[8] = tc.hours % 10;
    out[9] = (tc.hours / 10) & 0x03;
    out
}

/// True if the ANC packet is an SMPTE 12M-2 ATC packet (DID `0x60`,
/// SDID `0x60`/`0x61`).
pub fn is_atc_packet(p: &AncPacket) -> bool {
    p.did == 0x60 && (p.sdid == 0x60 || p.sdid == 0x61)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip_non_drop() {
        let tc = Timecode {
            hours: 10,
            minutes: 30,
            seconds: 45,
            frames: 12,
            drop_frame: false,
            color_frame: false,
        };
        let bytes = serialize_timecode(&tc);
        let parsed = parse_timecode(&bytes).unwrap();
        assert_eq!(parsed, tc);
        assert_eq!(parsed.to_string(), "10:30:45:12");
    }

    #[test]
    fn test_round_trip_drop_frame() {
        let tc = Timecode {
            hours: 1,
            minutes: 2,
            seconds: 3,
            frames: 29,
            drop_frame: true,
            color_frame: false,
        };
        let bytes = serialize_timecode(&tc);
        let parsed = parse_timecode(&bytes).unwrap();
        assert_eq!(parsed, tc);
        assert_eq!(parsed.to_string(), "01:02:03;29");
    }

    #[test]
    fn test_truncated() {
        assert_eq!(parse_timecode(&[0; 5]), Err(TimecodeError::Truncated));
    }

    #[test]
    fn test_is_atc_packet() {
        let p = AncPacket::simple(0x60, 0x60, 9, vec![]);
        assert!(is_atc_packet(&p));
        let p = AncPacket::simple(0x60, 0x61, 9, vec![]);
        assert!(is_atc_packet(&p));
        let p = AncPacket::simple(0x41, 0x07, 9, vec![]);
        assert!(!is_atc_packet(&p));
    }
}
