//! RFC 6184 H.264 RTP packetizer.
//!
//! Converts H.264 NAL units into RTP packets suitable for WebRTC transport.
//! Supports Single NAL Unit mode (small NALUs) and FU-A fragmentation
//! (large NALUs exceeding the MTU).

/// Maximum RTP payload size for WebRTC (accounting for SRTP overhead).
const MAX_PAYLOAD_SIZE: usize = 1200;

/// FU-A indicator byte: F=0, NRI from NALU, Type=28 (FU-A)
const FU_A_TYPE: u8 = 28;

/// A single RTP payload produced by the packetizer.
pub struct RtpPayload {
    /// RTP payload data (ready to be sent via str0m).
    pub data: Vec<u8>,
    /// Whether this is the last packet of the current access unit (frame).
    /// Set the RTP marker bit on this packet.
    pub marker: bool,
}

/// H.264 RTP packetizer per RFC 6184.
///
/// Call `packetize()` for each NAL unit. The last NALU of a frame
/// should have `last_nalu_of_frame = true` to set the marker bit.
pub struct H264Packetizer;

impl H264Packetizer {
    /// Packetize a single H.264 NAL unit into one or more RTP payloads.
    ///
    /// - NALUs ≤ `MAX_PAYLOAD_SIZE` are sent as Single NAL Unit packets.
    /// - Larger NALUs are fragmented using FU-A (Fragmentation Unit type A).
    ///
    /// `last_nalu_of_frame` should be true for the last NALU of an access
    /// unit (frame boundary), which sets the RTP marker bit.
    pub fn packetize(nalu: &[u8], last_nalu_of_frame: bool) -> Vec<RtpPayload> {
        if nalu.is_empty() {
            return Vec::new();
        }

        if nalu.len() <= MAX_PAYLOAD_SIZE {
            // Single NAL Unit packet: payload is the NALU directly
            vec![RtpPayload {
                data: nalu.to_vec(),
                marker: last_nalu_of_frame,
            }]
        } else {
            // FU-A fragmentation
            Self::fragment_fu_a(nalu, last_nalu_of_frame)
        }
    }

    /// Fragment a large NALU using FU-A (Fragmentation Unit type A).
    ///
    /// Each FU-A packet has:
    /// - FU indicator byte: F(1) | NRI(2) | Type(5) = same F+NRI as NALU, Type=28
    /// - FU header byte: S(1) | E(1) | R(1) | Type(5) = original NALU type
    /// - Fragment data
    fn fragment_fu_a(nalu: &[u8], last_nalu_of_frame: bool) -> Vec<RtpPayload> {
        let nalu_header = nalu[0];
        let nri = nalu_header & 0x60; // NRI bits
        let nalu_type = nalu_header & 0x1F; // NALU type

        // FU indicator: same NRI, type = 28 (FU-A)
        let fu_indicator = (nalu_header & 0x80) | nri | FU_A_TYPE;

        // Fragment the NALU body (skip the header byte, it's in the FU header)
        let body = &nalu[1..];
        // Each fragment can carry (MAX_PAYLOAD_SIZE - 2) bytes of body
        // (2 bytes for FU indicator + FU header)
        let max_fragment = MAX_PAYLOAD_SIZE - 2;

        let mut payloads = Vec::new();
        let mut offset = 0;

        while offset < body.len() {
            let remaining = body.len() - offset;
            let fragment_size = remaining.min(max_fragment);
            let is_first = offset == 0;
            let is_last = offset + fragment_size >= body.len();

            // FU header: S(start) | E(end) | R(reserved=0) | Type(original NALU type)
            let fu_header = if is_first {
                0x80 | nalu_type // S=1, E=0
            } else if is_last {
                0x40 | nalu_type // S=0, E=1
            } else {
                nalu_type // S=0, E=0
            };

            let mut data = Vec::with_capacity(2 + fragment_size);
            data.push(fu_indicator);
            data.push(fu_header);
            data.extend_from_slice(&body[offset..offset + fragment_size]);

            payloads.push(RtpPayload {
                data,
                marker: is_last && last_nalu_of_frame,
            });

            offset += fragment_size;
        }

        payloads
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_nalu_single_packet() {
        // Small NALU (< MTU) should produce exactly one packet
        let nalu = vec![0x65, 0x01, 0x02, 0x03]; // IDR slice
        let packets = H264Packetizer::packetize(&nalu, true);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data, nalu);
        assert!(packets[0].marker);
    }

    #[test]
    fn test_large_nalu_fu_a_fragmentation() {
        // Large NALU (> MAX_PAYLOAD_SIZE) should be fragmented
        let mut nalu = vec![0x65]; // IDR NALU header (type=5, NRI=3)
        nalu.extend(vec![0xAB; MAX_PAYLOAD_SIZE + 500]);

        let packets = H264Packetizer::packetize(&nalu, true);
        assert!(packets.len() >= 2);

        // First fragment: S=1, E=0
        let fu_indicator = packets[0].data[0];
        let fu_header = packets[0].data[1];
        assert_eq!(fu_indicator & 0x1F, FU_A_TYPE); // Type=28
        assert_eq!(fu_indicator & 0x60, 0x60); // NRI=3 (from original 0x65)
        assert!(fu_header & 0x80 != 0); // S=1
        assert!(fu_header & 0x40 == 0); // E=0
        assert_eq!(fu_header & 0x1F, 5); // Original type=5 (IDR)
        assert!(!packets[0].marker);

        // Last fragment: S=0, E=1
        let last = &packets[packets.len() - 1];
        let fu_header_last = last.data[1];
        assert!(fu_header_last & 0x80 == 0); // S=0
        assert!(fu_header_last & 0x40 != 0); // E=1
        assert!(last.marker); // Marker on last fragment of last NALU

        // All fragment bodies reassemble to original NALU body
        let mut reassembled = vec![nalu[0]]; // Start with NALU header
        for pkt in &packets {
            reassembled.extend_from_slice(&pkt.data[2..]); // Skip FU indicator + FU header
        }
        assert_eq!(reassembled, nalu);
    }

    #[test]
    fn test_marker_only_on_last_nalu() {
        let nalu = vec![0x41, 0x01, 0x02]; // Non-IDR
        let packets = H264Packetizer::packetize(&nalu, false);
        assert_eq!(packets.len(), 1);
        assert!(!packets[0].marker); // Not last NALU of frame
    }

    #[test]
    fn test_empty_nalu() {
        let packets = H264Packetizer::packetize(&[], true);
        assert!(packets.is_empty());
    }
}
