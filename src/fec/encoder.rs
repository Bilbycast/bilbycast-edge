// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SMPTE 2022-1 FEC Encoder -- generates column and row FEC packets.
//!
//! Accumulates media packets in an L x D matrix. After each complete column
//! (every D media packets in that column), emits a column FEC packet.
//! After a full matrix cycle (L*D media packets), emits row FEC packets
//! for all D rows and resets the matrix.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

use super::matrix::FecMatrix;
use super::{FEC_HEADER_LEN, FEC_MAGIC};

/// FEC Encoder that generates FEC packets from a stream of media packets.
pub struct FecEncoder {
    matrix: FecMatrix,
    /// Number of media packets inserted in the current matrix cycle
    media_count: u16,
    /// Total cells in one cycle (L * D)
    matrix_size: u16,
    /// Base RTP sequence number for this cycle
    base_seq: u16,
    /// Counter for FEC packets generated (wired to stats)
    stats_fec_sent: Arc<AtomicU64>,
    /// Whether this is the first packet (to set base_seq)
    initialized: bool,
}

impl FecEncoder {
    /// Create a new FEC encoder with the given L (columns) and D (rows) parameters.
    ///
    /// `stats_fec_sent` is an atomic counter incremented each time a FEC packet
    /// is generated, allowing the stats subsystem to track FEC overhead.
    pub fn new(columns: u8, rows: u8, stats_fec_sent: Arc<AtomicU64>) -> Self {
        let matrix_size = columns as u16 * rows as u16;
        Self {
            matrix: FecMatrix::new(columns, rows),
            media_count: 0,
            matrix_size,
            base_seq: 0,
            stats_fec_sent,
            initialized: false,
        }
    }

    /// Feed a media packet into the encoder.
    ///
    /// Returns a Vec of FEC packets to emit (may be empty, or contain column/row FEC).
    /// The returned Bytes are raw FEC payloads prefixed with a minimal FEC header.
    pub fn process(&mut self, seq: u16, payload: &[u8]) -> Vec<Bytes> {
        if !self.initialized {
            self.base_seq = seq;
            self.initialized = true;
        }

        let position = self.media_count as usize;
        self.matrix.insert_media(position, payload);
        self.media_count += 1;

        let mut fec_packets = Vec::new();

        let (col, row) = self.matrix.position_to_col_row(position);

        // Check if this column is now complete (has all D packets)
        // A column is complete when we've filled all its rows
        // Column `col` cells are at positions: col, col+L, col+2L, ..., col+(D-1)*L
        // The col's count is tracked by the matrix internally
        if let Some(col_fec_payload) = self.matrix.generate_col_fec(col) {
            let fec_header = build_fec_header(
                self.base_seq,
                col,
                self.matrix.columns,
                true, // is_column_fec
            );
            let mut fec_pkt = fec_header;
            fec_pkt.extend_from_slice(&col_fec_payload);
            fec_packets.push(Bytes::from(fec_pkt));
            self.stats_fec_sent.fetch_add(1, Ordering::Relaxed);
        }

        // If matrix cycle is complete, emit row FEC packets
        if self.media_count >= self.matrix_size {
            for r in 0..self.matrix.rows {
                if let Some(row_fec_payload) = self.matrix.generate_row_fec(r) {
                    let fec_header = build_fec_header(
                        self.base_seq,
                        r,
                        1, // row FEC offset = 1 (adjacent packets)
                        false,
                    );
                    let mut fec_pkt = fec_header;
                    fec_pkt.extend_from_slice(&row_fec_payload);
                    fec_packets.push(Bytes::from(fec_pkt));
                    self.stats_fec_sent.fetch_add(1, Ordering::Relaxed);
                }
            }

            // Reset for next cycle
            self.matrix.reset();
            self.media_count = 0;
            self.initialized = false; // next packet sets new base_seq
        }

        // Suppress the row variable warning
        let _ = row;

        fec_packets
    }
}

/// Build the bilbycast FEC framing (4-byte magic + 10-byte header).
///
/// Fields in the 10-byte header (bytes 4..14 of the emitted packet):
///   - SNBase (2 bytes): base sequence number of protected packets
///   - Length Recovery (2 bytes): set to 0 (simplified)
///   - E (1 bit) + PT recovery (7 bits): set to 0
///   - Mask (3 bytes): set to 0 (we use offset/NA addressing)
///   - TS Recovery (4 bytes): set to 0
///   - Flags (byte 8): D bit (0x40) for column, Type (bits 5-3), Index (bits 2-0)
///   - Offset (byte 9): L for column FEC, 1 for row FEC
///
/// Prepended with the [`FEC_MAGIC`] four-byte marker so receivers can
/// distinguish FEC datagrams from RTP media on the same UDP port.
fn build_fec_header(
    sn_base: u16,
    index: u8,
    offset: u8,
    is_column: bool,
) -> Vec<u8> {
    let mut hdr = Vec::with_capacity(FEC_MAGIC.len() + FEC_HEADER_LEN);
    hdr.extend_from_slice(&FEC_MAGIC);
    hdr.resize(FEC_MAGIC.len() + FEC_HEADER_LEN, 0);

    let body = &mut hdr[FEC_MAGIC.len()..];
    body[0] = (sn_base >> 8) as u8;
    body[1] = sn_base as u8;

    let d_bit = if is_column { 0x40 } else { 0x00 };
    body[8] = d_bit | (index & 0x07);
    body[9] = offset;

    hdr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoder_emits_column_fec() {
        let stats = Arc::new(AtomicU64::new(0));
        let mut encoder = FecEncoder::new(2, 2, stats.clone()); // L=2, D=2

        // Matrix layout:
        // Row 0: [pos 0, pos 1]
        // Row 1: [pos 2, pos 3]
        // Column 0: pos 0, pos 2
        // Column 1: pos 1, pos 3

        // Packet at pos 0 (col 0, row 0) — col 0 not complete yet
        let fec = encoder.process(100, &[0x01]);
        assert!(fec.is_empty());

        // Packet at pos 1 (col 1, row 0) — col 1 not complete yet
        let fec = encoder.process(101, &[0x02]);
        assert!(fec.is_empty());

        // Packet at pos 2 (col 0, row 1) — col 0 now complete!
        let fec = encoder.process(102, &[0x04]);
        assert_eq!(fec.len(), 1); // column 0 FEC

        // Packet at pos 3 (col 1, row 1) — col 1 complete + end of matrix → row FECs
        let fec = encoder.process(103, &[0x08]);
        // Should emit: col 1 FEC + row 0 FEC + row 1 FEC = 3 packets
        assert_eq!(fec.len(), 3);

        // Total FEC packets: 1 + 3 = 4
        assert_eq!(stats.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn fec_packets_carry_bcfe_magic() {
        let stats = Arc::new(AtomicU64::new(0));
        let mut encoder = FecEncoder::new(2, 2, stats.clone());

        encoder.process(100, &[0x01]);
        encoder.process(101, &[0x02]);
        let fec = encoder.process(102, &[0x04]);
        assert_eq!(fec.len(), 1);
        let pkt = &fec[0];
        assert!(pkt.len() >= FEC_MAGIC.len() + FEC_HEADER_LEN);
        assert_eq!(&pkt[..FEC_MAGIC.len()], &FEC_MAGIC);
        // First post-magic byte is SNBase high; cannot collide with RTP V=2 prefix.
        assert_ne!(pkt[0] & 0xC0, 0x80);
    }

    #[test]
    fn test_encoder_resets_after_cycle() {
        let stats = Arc::new(AtomicU64::new(0));
        let mut encoder = FecEncoder::new(2, 2, stats.clone());

        // Fill first cycle
        encoder.process(0, &[0x01]);
        encoder.process(1, &[0x02]);
        encoder.process(2, &[0x04]);
        let fec = encoder.process(3, &[0x08]);
        assert!(!fec.is_empty()); // end of cycle

        // Next cycle should start fresh
        let fec = encoder.process(4, &[0x10]);
        assert!(fec.is_empty()); // first packet of new cycle, no FEC yet
    }

}
