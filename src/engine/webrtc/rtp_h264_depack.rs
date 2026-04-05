// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! RFC 6184 H.264 RTP depacketizer.
//!
//! Reassembles H.264 NAL units from incoming RTP packets. Handles:
//! - Single NAL Unit packets (type 1-23)
//! - FU-A fragmentation packets (type 28)
//! - STAP-A aggregation packets (type 24)
//!
//! Currently unused: str0m handles RTP depacketization internally.
//! Retained for future direct RTP H.264 input without str0m.

#[allow(dead_code)]
/// Reassembled NAL unit with timing information.
pub struct DepacketizedNalu {
    /// Complete NALU data (including the NALU header byte).
    pub data: Vec<u8>,
    /// RTP timestamp (90 kHz clock).
    pub rtp_timestamp: u32,
    /// Whether the RTP marker bit was set (last packet of access unit).
    pub marker: bool,
}

#[allow(dead_code)]
/// H.264 RTP depacketizer per RFC 6184.
///
/// Call `depacketize()` for each incoming RTP payload. Returns zero or
/// more complete NALUs. FU-A fragments are buffered internally until
/// the final fragment arrives.
pub struct H264Depacketizer {
    /// Buffer for in-progress FU-A reassembly.
    fu_a_buffer: Vec<u8>,
    /// Whether we're currently reassembling a FU-A sequence.
    fu_a_active: bool,
}

#[allow(dead_code)]
impl H264Depacketizer {
    pub fn new() -> Self {
        Self {
            fu_a_buffer: Vec::new(),
            fu_a_active: false,
        }
    }

    /// Depacketize an RTP payload into zero or more complete NALUs.
    ///
    /// `rtp_payload` is the RTP payload data (after the RTP header).
    /// `rtp_timestamp` and `marker` come from the RTP header.
    pub fn depacketize(
        &mut self,
        rtp_payload: &[u8],
        rtp_timestamp: u32,
        marker: bool,
    ) -> Vec<DepacketizedNalu> {
        if rtp_payload.is_empty() {
            return Vec::new();
        }

        let nalu_type = rtp_payload[0] & 0x1F;

        match nalu_type {
            // Single NAL Unit packet (type 1-23)
            1..=23 => {
                self.abort_fu_a(); // Any incomplete FU-A is discarded
                vec![DepacketizedNalu {
                    data: rtp_payload.to_vec(),
                    rtp_timestamp,
                    marker,
                }]
            }
            // STAP-A (Single-Time Aggregation Packet)
            24 => {
                self.abort_fu_a();
                self.depacketize_stap_a(rtp_payload, rtp_timestamp, marker)
            }
            // FU-A (Fragmentation Unit)
            28 => self.depacketize_fu_a(rtp_payload, rtp_timestamp, marker),
            // Unsupported types (STAP-B=25, MTAP16=26, MTAP24=27, FU-B=29)
            _ => {
                tracing::debug!("Unsupported H.264 RTP NAL type {nalu_type}, discarding");
                self.abort_fu_a();
                Vec::new()
            }
        }
    }

    /// Parse STAP-A packet into individual NALUs.
    fn depacketize_stap_a(
        &self,
        payload: &[u8],
        rtp_timestamp: u32,
        marker: bool,
    ) -> Vec<DepacketizedNalu> {
        let mut nalus = Vec::new();
        let mut offset = 1; // Skip STAP-A header byte

        while offset + 2 <= payload.len() {
            let nalu_size = ((payload[offset] as usize) << 8) | payload[offset + 1] as usize;
            offset += 2;

            if offset + nalu_size > payload.len() {
                tracing::warn!("STAP-A: NALU size {} exceeds remaining data", nalu_size);
                break;
            }

            nalus.push(DepacketizedNalu {
                data: payload[offset..offset + nalu_size].to_vec(),
                rtp_timestamp,
                marker: marker && offset + nalu_size >= payload.len(), // marker on last
            });

            offset += nalu_size;
        }

        nalus
    }

    /// Handle FU-A fragment. Returns complete NALU when final fragment arrives.
    fn depacketize_fu_a(
        &mut self,
        payload: &[u8],
        rtp_timestamp: u32,
        marker: bool,
    ) -> Vec<DepacketizedNalu> {
        if payload.len() < 2 {
            return Vec::new();
        }

        let fu_indicator = payload[0];
        let fu_header = payload[1];
        let start = (fu_header & 0x80) != 0;
        let end = (fu_header & 0x40) != 0;
        let nalu_type = fu_header & 0x1F;

        if start {
            // Start of new FU-A sequence: reconstruct NALU header
            let nalu_header = (fu_indicator & 0xE0) | nalu_type;
            self.fu_a_buffer.clear();
            self.fu_a_buffer.push(nalu_header);
            self.fu_a_buffer.extend_from_slice(&payload[2..]);
            self.fu_a_active = true;
        } else if self.fu_a_active {
            // Continuation or end fragment
            self.fu_a_buffer.extend_from_slice(&payload[2..]);
        } else {
            // Got continuation without start — discard
            return Vec::new();
        }

        if end {
            // Complete NALU assembled
            let nalu = std::mem::take(&mut self.fu_a_buffer);
            self.fu_a_active = false;
            vec![DepacketizedNalu {
                data: nalu,
                rtp_timestamp,
                marker,
            }]
        } else {
            Vec::new() // More fragments expected
        }
    }

    /// Discard any in-progress FU-A reassembly.
    fn abort_fu_a(&mut self) {
        if self.fu_a_active {
            tracing::debug!("Aborting incomplete FU-A reassembly ({} bytes)", self.fu_a_buffer.len());
            self.fu_a_buffer.clear();
            self.fu_a_active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_nalu() {
        let mut depack = H264Depacketizer::new();
        let payload = vec![0x65, 0x01, 0x02, 0x03]; // IDR slice (type=5)
        let nalus = depack.depacketize(&payload, 1000, true);
        assert_eq!(nalus.len(), 1);
        assert_eq!(nalus[0].data, payload);
        assert!(nalus[0].marker);
        assert_eq!(nalus[0].rtp_timestamp, 1000);
    }

    #[test]
    fn test_fu_a_reassembly() {
        let mut depack = H264Depacketizer::new();

        // Original NALU: type=5 (IDR), NRI=3 → header 0x65
        // FU indicator: F=0, NRI=3, Type=28 → 0x7C
        // First fragment: S=1, E=0, Type=5 → FU header 0x85
        let frag1 = vec![0x7C, 0x85, 0xAA, 0xBB];
        let nalus = depack.depacketize(&frag1, 2000, false);
        assert!(nalus.is_empty()); // Not complete yet

        // Middle fragment: S=0, E=0, Type=5 → FU header 0x05
        let frag2 = vec![0x7C, 0x05, 0xCC, 0xDD];
        let nalus = depack.depacketize(&frag2, 2000, false);
        assert!(nalus.is_empty());

        // Last fragment: S=0, E=1, Type=5 → FU header 0x45
        let frag3 = vec![0x7C, 0x45, 0xEE, 0xFF];
        let nalus = depack.depacketize(&frag3, 2000, true);
        assert_eq!(nalus.len(), 1);

        // Reconstructed NALU: header 0x65 + body from all fragments
        assert_eq!(nalus[0].data, vec![0x65, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert!(nalus[0].marker);
    }

    #[test]
    fn test_stap_a() {
        let mut depack = H264Depacketizer::new();

        // STAP-A with two NALUs: SPS (3 bytes) and PPS (2 bytes)
        let mut payload = vec![24u8]; // STAP-A header (type=24)
        // NALU 1: size=3, data=[0x67, 0x42, 0x00]
        payload.extend_from_slice(&[0x00, 0x03, 0x67, 0x42, 0x00]);
        // NALU 2: size=2, data=[0x68, 0xCE]
        payload.extend_from_slice(&[0x00, 0x02, 0x68, 0xCE]);

        let nalus = depack.depacketize(&payload, 3000, true);
        assert_eq!(nalus.len(), 2);
        assert_eq!(nalus[0].data, vec![0x67, 0x42, 0x00]);
        assert_eq!(nalus[1].data, vec![0x68, 0xCE]);
        assert!(nalus[1].marker); // Marker on last NALU
    }

    #[test]
    fn test_fu_a_abort_on_single_nalu() {
        let mut depack = H264Depacketizer::new();

        // Start a FU-A sequence
        let frag1 = vec![0x7C, 0x85, 0xAA];
        let _ = depack.depacketize(&frag1, 1000, false);
        assert!(depack.fu_a_active);

        // Receive a single NALU — should abort FU-A
        let single = vec![0x41, 0x01, 0x02];
        let nalus = depack.depacketize(&single, 2000, true);
        assert_eq!(nalus.len(), 1);
        assert!(!depack.fu_a_active);
    }
}
