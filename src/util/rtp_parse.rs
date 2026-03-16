/// Minimum RTP header size (no CSRC, no extensions)
pub const RTP_HEADER_MIN_SIZE: usize = 12;

/// Parse the RTP sequence number from a raw packet.
/// Returns None if the packet is too short to contain an RTP header.
pub fn parse_rtp_sequence_number(data: &[u8]) -> Option<u16> {
    if data.len() < RTP_HEADER_MIN_SIZE {
        return None;
    }
    Some(u16::from_be_bytes([data[2], data[3]]))
}

/// Parse the RTP timestamp from a raw packet.
pub fn parse_rtp_timestamp(data: &[u8]) -> Option<u32> {
    if data.len() < RTP_HEADER_MIN_SIZE {
        return None;
    }
    Some(u32::from_be_bytes([data[4], data[5], data[6], data[7]]))
}

/// Parse the RTP SSRC from a raw packet.
pub fn parse_rtp_ssrc(data: &[u8]) -> Option<u32> {
    if data.len() < RTP_HEADER_MIN_SIZE {
        return None;
    }
    Some(u32::from_be_bytes([data[8], data[9], data[10], data[11]]))
}

/// Parse the RTP payload type from a raw packet.
pub fn parse_rtp_payload_type(data: &[u8]) -> Option<u8> {
    if data.len() < RTP_HEADER_MIN_SIZE {
        return None;
    }
    Some(data[1] & 0x7F)
}

/// Parse the RTP version from a raw packet. Should be 2.
pub fn parse_rtp_version(data: &[u8]) -> Option<u8> {
    if data.is_empty() {
        return None;
    }
    Some((data[0] >> 6) & 0x03)
}

/// Quick validation: checks if a packet looks like a valid RTP packet.
pub fn is_likely_rtp(data: &[u8]) -> bool {
    if data.len() < RTP_HEADER_MIN_SIZE {
        return false;
    }
    let version = (data[0] >> 6) & 0x03;
    version == 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rtp_packet(seq: u16, ts: u32, ssrc: u32, pt: u8) -> Vec<u8> {
        let mut pkt = vec![0u8; 20]; // 12 header + 8 payload
        pkt[0] = 0x80; // Version 2, no padding, no extension, 0 CSRC
        pkt[1] = pt & 0x7F;
        pkt[2] = (seq >> 8) as u8;
        pkt[3] = (seq & 0xFF) as u8;
        pkt[4] = (ts >> 24) as u8;
        pkt[5] = (ts >> 16) as u8;
        pkt[6] = (ts >> 8) as u8;
        pkt[7] = (ts & 0xFF) as u8;
        pkt[8] = (ssrc >> 24) as u8;
        pkt[9] = (ssrc >> 16) as u8;
        pkt[10] = (ssrc >> 8) as u8;
        pkt[11] = (ssrc & 0xFF) as u8;
        pkt
    }

    #[test]
    fn test_parse_sequence_number() {
        let pkt = make_rtp_packet(12345, 0, 0, 33);
        assert_eq!(parse_rtp_sequence_number(&pkt), Some(12345));
    }

    #[test]
    fn test_parse_timestamp() {
        let pkt = make_rtp_packet(0, 0xDEADBEEF, 0, 33);
        assert_eq!(parse_rtp_timestamp(&pkt), Some(0xDEADBEEF));
    }

    #[test]
    fn test_parse_ssrc() {
        let pkt = make_rtp_packet(0, 0, 0x12345678, 33);
        assert_eq!(parse_rtp_ssrc(&pkt), Some(0x12345678));
    }

    #[test]
    fn test_parse_payload_type() {
        let pkt = make_rtp_packet(0, 0, 0, 33);
        assert_eq!(parse_rtp_payload_type(&pkt), Some(33));
    }

    #[test]
    fn test_is_likely_rtp() {
        let pkt = make_rtp_packet(1, 100, 999, 33);
        assert!(is_likely_rtp(&pkt));
        assert!(!is_likely_rtp(&[0u8; 4])); // too short
        assert!(!is_likely_rtp(&[0x00; 12])); // version 0
    }

    #[test]
    fn test_short_packet() {
        assert_eq!(parse_rtp_sequence_number(&[0u8; 5]), None);
    }

    #[test]
    fn test_sequence_wraparound() {
        let pkt = make_rtp_packet(65535, 0, 0, 33);
        assert_eq!(parse_rtp_sequence_number(&pkt), Some(65535));
        let pkt2 = make_rtp_packet(0, 0, 0, 33);
        assert_eq!(parse_rtp_sequence_number(&pkt2), Some(0));
    }
}
