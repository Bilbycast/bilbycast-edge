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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

use crate::engine::packet::RtpPacket;
use crate::util::time::now_us;

use super::encoder::{FecHeaderInfo, parse_fec_header};
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
    /// Counter for recovered packets (wired to stats)
    stats_recovered: Arc<AtomicU64>,
}

impl FecDecoder {
    /// Create a new FEC decoder with the given L (columns) and D (rows) parameters.
    ///
    /// `stats_recovered` is an atomic counter incremented each time a lost
    /// media packet is successfully recovered via FEC.
    pub fn new(columns: u8, rows: u8, stats_recovered: Arc<AtomicU64>) -> Self {
        let matrix_size = columns as u16 * rows as u16;
        Self {
            matrix: FecMatrix::new(columns, rows),
            base_seq: 0,
            media_count: 0,
            matrix_size,
            initialized: false,
            stats_recovered,
        }
    }

    /// Process an incoming RTP packet (media or FEC).
    ///
    /// For media packets: places into the matrix, returns the packet unchanged.
    /// For FEC packets: attempts recovery, returns any recovered packets.
    ///
    /// The caller should check: if the returned vec is non-empty, forward all
    /// packets to the broadcast channel.
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
        });

        // Check if matrix cycle is complete — reset for next
        if self.media_count >= self.matrix_size {
            self.matrix.reset();
            self.media_count = 0;
            self.initialized = false;
        }

        result
    }

    /// Process a FEC packet and attempt recovery.
    ///
    /// Returns any recovered RTP packets.
    pub fn process_fec(&mut self, fec_data: &[u8]) -> Vec<RtpPacket> {
        let mut recovered = Vec::new();

        if let Some(info) = parse_fec_header(fec_data) {
            let fec_payload = &fec_data[10..]; // skip 10-byte FEC header

            if info.is_column {
                self.matrix.insert_col_fec(info.index, fec_payload);

                // Try column recovery
                if let Some(payload) = self.matrix.try_recover_column(info.index) {
                    if let Some(pkt) = self.build_recovered_packet(info.index as usize, &payload, &info, true) {
                        recovered.push(pkt);
                        self.stats_recovered.fetch_add(1, Ordering::Relaxed);
                    }
                }
            } else {
                self.matrix.insert_row_fec(info.index, fec_payload);

                // Try row recovery
                if let Some(payload) = self.matrix.try_recover_row(info.index) {
                    if let Some(pkt) = self.build_recovered_packet(info.index as usize, &payload, &info, false) {
                        recovered.push(pkt);
                        self.stats_recovered.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        recovered
    }

    /// Build a recovered RTP packet from the recovered payload.
    ///
    /// We reconstruct a minimal RTP header with the correct sequence number.
    fn build_recovered_packet(
        &self,
        index: usize,
        payload: &[u8],
        info: &FecHeaderInfo,
        is_column: bool,
    ) -> Option<RtpPacket> {
        // Find the missing position in the column/row
        let missing_pos = if is_column {
            // Column `index`: positions are index, index+L, index+2L, ...
            let col = index;
            (0..self.matrix.rows as usize)
                .map(|r| col + r * self.matrix.columns as usize)
                .find(|&pos| !self.matrix.has_cell(pos))
        } else {
            // Row `index`: positions are index*L, index*L+1, ..., index*L+(L-1)
            let row = index;
            let start = row * self.matrix.columns as usize;
            (start..start + self.matrix.columns as usize)
                .find(|&pos| !self.matrix.has_cell(pos))
        };

        let missing_pos = missing_pos?;
        let recovered_seq = self.base_seq.wrapping_add(missing_pos as u16);

        // Build a minimal RTP header (12 bytes) + recovered payload
        let mut rtp_data = vec![0u8; 12 + payload.len()];
        rtp_data[0] = 0x80; // V=2, no padding, no extension, CC=0
        rtp_data[1] = 96;   // PT=96 (dynamic)
        rtp_data[2] = (recovered_seq >> 8) as u8;
        rtp_data[3] = recovered_seq as u8;
        // Timestamp: estimate from base + offset (simplified)
        // Use 0 as we don't have the exact timestamp
        rtp_data[12..].copy_from_slice(payload);

        let _ = info; // suppress warning

        Some(RtpPacket {
            data: Bytes::from(rtp_data),
            sequence_number: recovered_seq,
            rtp_timestamp: 0,
            recv_time_us: now_us(),
        })
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
        let stats = Arc::new(AtomicU64::new(0));
        let mut decoder = FecDecoder::new(2, 2, stats);

        let pkt = make_rtp_packet(100, &[0x01, 0x02]);
        let result = decoder.process_media(100, 0, &pkt);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sequence_number, 100);
    }

    #[test]
    fn test_decoder_recovers_with_column_fec() {
        let stats = Arc::new(AtomicU64::new(0));
        let mut decoder = FecDecoder::new(2, 2, stats.clone());

        // L=2, D=2. Matrix:
        // Row 0: pos 0 (seq 100), pos 1 (seq 101)
        // Row 1: pos 2 (seq 102), pos 3 (seq 103)
        // Column 0: pos 0, pos 2
        // Column 1: pos 1, pos 3

        let payload_0 = vec![0xAA];
        let payload_1 = vec![0xBB];
        // payload_2 = 0xCC is LOST
        let payload_3 = vec![0xDD];

        // Receive packets 0, 1, 3 (skip 2)
        decoder.process_media(100, 0, &make_rtp_packet(100, &payload_0));
        decoder.process_media(101, 0, &make_rtp_packet(101, &payload_1));
        decoder.process_media(103, 0, &make_rtp_packet(103, &payload_3));

        // Column 0 FEC = XOR(payload_0, payload_2) = XOR(0xAA, 0xCC) = 0x66
        let col0_fec_payload = vec![0xAA ^ 0xCC]; // = 0x66
        let mut fec_data = vec![0u8; 10];
        fec_data[0] = 0; // SNBase high
        fec_data[1] = 100; // SNBase low (approximate)
        fec_data[8] = 0x40; // D bit set (column), index=0
        fec_data[9] = 2; // offset = L = 2
        fec_data.extend_from_slice(&col0_fec_payload);

        let recovered = decoder.process_fec(&fec_data);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].sequence_number, 102);

        // Verify the recovered payload
        let recovered_payload = &recovered[0].data[12..]; // skip RTP header
        assert_eq!(recovered_payload, &[0xCC]);

        assert_eq!(stats.load(Ordering::Relaxed), 1);
    }
}
