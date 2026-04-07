// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

#![allow(dead_code)]

//! Minimal CEA-608 / CEA-708 caption frame extractor.
//!
//! Captions arrive over the ANC pipeline inside SMPTE 334-1 / 334-2 packets:
//!
//! - DID/SDID `0x61/0x01` — CEA-708 (DTVCC) caption distribution packets.
//! - DID/SDID `0x61/0x02` — CEA-608 line 21 closed-caption pairs.
//!
//! Bilbycast does not render captions in the manager UI itself; it surfaces
//! "captions detected" as an event with the language/service identifier so
//! operators can confirm captions are present in a flow. Anyone who wants to
//! decode the actual caption text can subscribe to the raw ANC stream.
//!
//! For Phase 1 we therefore only need to:
//!
//! - Identify CEA-608 vs CEA-708 packets via DID/SDID.
//! - Pull the channel/service number from the header so the event mentions
//!   "CC1" / "Service 1" / etc.
//! - Surface the packet length and data integrity (cc_count) so a malformed
//!   payload doesn't get swallowed silently.
//!
//! Pure Rust, hand-rolled, no `ccaption_codec_rs` dependency. The full
//! decoder pipeline can be added behind an opt-in feature later.

use super::ancillary::AncPacket;

/// Type of caption payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionType {
    Cea608,
    Cea708,
}

/// Lightweight summary of a caption packet — what bilbycast surfaces as a
/// `captions` event. The actual caption bytes are intentionally not parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionSummary {
    pub kind: CaptionType,
    /// Number of cc_data byte pairs in this packet (CEA-708) or number of
    /// 608 byte pairs (CEA-608). Useful for liveness monitoring.
    pub cc_count: u8,
    /// Number of bytes of payload following the header.
    pub payload_bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CaptionError {
    Truncated,
    UnknownDid,
}

impl std::fmt::Display for CaptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptionError::Truncated => f.write_str("Caption ANC payload truncated"),
            CaptionError::UnknownDid => f.write_str("ANC packet is not a CEA-608/708 caption"),
        }
    }
}

impl std::error::Error for CaptionError {}

/// True if the ANC packet is a CEA-608 or CEA-708 caption.
pub fn is_caption_packet(p: &AncPacket) -> bool {
    p.did == 0x61 && (p.sdid == 0x01 || p.sdid == 0x02)
}

/// Extract a [`CaptionSummary`] from an ANC packet's `user_data`.
///
/// The DID/SDID determines the CEA flavor; the cc_count comes from the
/// payload's standard SMPTE 334 header (5-bit cc_count in byte 1 for CEA-708,
/// 5-bit cc_count in byte 0 for CEA-608).
pub fn parse_caption(packet: &AncPacket) -> Result<CaptionSummary, CaptionError> {
    if !is_caption_packet(packet) {
        return Err(CaptionError::UnknownDid);
    }
    let kind = if packet.sdid == 0x01 {
        CaptionType::Cea708
    } else {
        CaptionType::Cea608
    };
    let udw = &packet.user_data;
    let cc_count = match kind {
        CaptionType::Cea708 => {
            // SMPTE 334-1 cdp() header:
            //  16 cdp_identifier (0x9669)
            //   8 cdp_length
            //   4 cdp_frame_rate
            //   4 reserved
            //   8 cdp_flags
            //  16 cdp_hdr_sequence_counter
            // ... time_code_section / cc_data_section / ccsvcinfo_section ...
            //
            // For Phase 1 we only need the cc_count from the cc_data_section.
            // We scan for the 0x72 marker that introduces it; if absent we
            // fall back to length-based truncation detection.
            if udw.len() < 8 {
                return Err(CaptionError::Truncated);
            }
            udw.iter()
                .position(|b| *b == 0x72)
                .and_then(|i| udw.get(i + 1).copied())
                .map(|b| b & 0x1F)
                .unwrap_or(0)
        }
        CaptionType::Cea608 => {
            // SMPTE 334-2: each captioning byte pair occupies 2 user_data
            // words plus a parity word. We approximate cc_count as the
            // user_data length divided by 3 to keep this dependency-free.
            if udw.len() < 3 {
                return Err(CaptionError::Truncated);
            }
            (udw.len() / 3) as u8
        }
    };
    Ok(CaptionSummary {
        kind,
        cc_count,
        payload_bytes: udw.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_caption_packet() {
        let p708 = AncPacket::simple(0x61, 0x01, 9, vec![0u8; 16]);
        let p608 = AncPacket::simple(0x61, 0x02, 9, vec![0u8; 16]);
        let other = AncPacket::simple(0x41, 0x07, 9, vec![]);
        assert!(is_caption_packet(&p708));
        assert!(is_caption_packet(&p608));
        assert!(!is_caption_packet(&other));
    }

    #[test]
    fn test_parse_708_with_cc_marker() {
        // Synthetic CDP: identifier + length + flags + 0x72 marker + cc_count(0x03) +
        // 3*3=9 bytes payload. Bilbycast only reads the cc_count nibble.
        let mut udw = vec![0x96, 0x69, 12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x72, 0x03];
        udw.extend_from_slice(&[0u8; 9]);
        let p = AncPacket::simple(0x61, 0x01, 9, udw);
        let s = parse_caption(&p).unwrap();
        assert_eq!(s.kind, CaptionType::Cea708);
        assert_eq!(s.cc_count, 3);
    }

    #[test]
    fn test_parse_608_byte_count() {
        // 9 bytes / 3 = cc_count of 3.
        let p = AncPacket::simple(0x61, 0x02, 9, vec![0u8; 9]);
        let s = parse_caption(&p).unwrap();
        assert_eq!(s.kind, CaptionType::Cea608);
        assert_eq!(s.cc_count, 3);
    }

    #[test]
    fn test_parse_truncated_708() {
        let p = AncPacket::simple(0x61, 0x01, 9, vec![0u8; 4]);
        assert_eq!(parse_caption(&p), Err(CaptionError::Truncated));
    }

    #[test]
    fn test_parse_unknown_did() {
        let p = AncPacket::simple(0x41, 0x07, 9, vec![0u8; 16]);
        assert_eq!(parse_caption(&p), Err(CaptionError::UnknownDid));
    }
}
