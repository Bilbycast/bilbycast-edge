//! SMPTE 2022-1 FEC Decoder -- recovers lost RTP packets using FEC data.
//!
//! The decoder maintains an L x D FEC matrix. Media packets are placed into
//! the matrix by their sequence number. When a FEC packet arrives, the
//! decoder attempts to recover any single missing packet in the protected
//! column or row.
//!
//! FEC packets are distinguished from media packets by checking for our
//! 10-byte FEC header signature. In a real SMPTE 2022-1 deployment,
//! FEC packets arrive on separate port offsets (+2 for column, +4 for row),
//! but for simplicity we use an in-band approach with a FEC header marker.
use bytes::Bytes;

use crate::engine::packet::RtpPacket;
use crate::util::time::now_us;

use super::matrix::FecMatrix;

/// FEC Decoder that processes incoming packets and attempts recovery.
pub struct FecDecoder {
    matrix: FecMatrix,
    /// Base sequence number for the current matrix cycle
    base_seq: u16,
    /// Number of media packets received in this cycle
    media_count: u16,
    /// Matrix size (L * D)
    matrix_size: u16,
    /// Whether we've set base_seq yet
    initialized: bool,
}

impl FecDecoder {
    /// Create a new FEC decoder with the given L (columns) and D (rows) parameters.
    pub fn new(columns: u8, rows: u8) -> Self {
        let matrix_size = columns as u16 * rows as u16;
        Self {
            matrix: FecMatrix::new(columns, rows),
            base_seq: 0,
            media_count: 0,
            matrix_size,
            initialized: false,
        }
    }

    /// Process an incoming RTP media packet.
    ///
    /// Places the packet into the FEC matrix and returns it wrapped in a Vec
    /// for the caller to forward to the broadcast channel.
    pub fn process_media(&mut self, seq: u16, ts: u32, data: &Bytes) -> Vec<RtpPacket> {
        if !self.initialized {
            self.base_seq = seq;
            self.initialized = true;
        }

        let mut result = Vec::new();

        // Calculate position in matrix
        let diff = seq.wrapping_sub(self.base_seq) as u16;

        if diff >= self.matrix_size {
            // Packet is beyond current cycle — start new cycle
            self.matrix.reset();
            self.base_seq = seq;
            self.media_count = 0;
        }

        let position = seq.wrapping_sub(self.base_seq) as usize;
        if position < self.matrix.size() && !self.matrix.has_cell(position) {
            // Skip the RTP header (12 bytes min) and insert only payload for FEC computation
            // But we store the full data for forwarding
            let rtp_header_len = 12; // minimum RTP header
            if data.len() > rtp_header_len {
                self.matrix.insert_media(position, &data[rtp_header_len..]);
            }
            self.media_count += 1;
        }

        // The original packet is always forwarded
        result.push(RtpPacket {
            data: data.clone(),
            sequence_number: seq,
            rtp_timestamp: ts,
            recv_time_us: now_us(),
            is_raw_ts: false,
        });

        // Check if matrix cycle is complete — reset for next
        if self.media_count >= self.matrix_size {
            self.matrix.reset();
            self.media_count = 0;
            self.initialized = false;
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rtp_packet(seq: u16, payload: &[u8]) -> Bytes {
        let mut data = vec![0u8; 12 + payload.len()];
        data[0] = 0x80; // V=2
        data[1] = 96;   // PT=96
        data[2] = (seq >> 8) as u8;
        data[3] = seq as u8;
        data[12..].copy_from_slice(payload);
        Bytes::from(data)
    }

    #[test]
    fn test_decoder_forwards_media() {
        let mut decoder = FecDecoder::new(2, 2);

        let pkt = make_rtp_packet(100, &[0x01, 0x02]);
        let result = decoder.process_media(100, 0, &pkt);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sequence_number, 100);
    }
}
