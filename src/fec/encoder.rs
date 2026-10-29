// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SMPTE 2022-1 FEC Encoder -- generates column and row FEC packets.
//!
//! Places each media packet in an L x D matrix at `seq - SNBase`, the same
//! rule [`super::decoder::FecDecoder`] files received packets by, so both
//! ends put every cell in the same place whatever gaps, repeats or steps
//! back the sequence numbers carry. When a column holds all D of its
//! packets, a column FEC packet is emitted. When the packet in the matrix's
//! last cell (by sequence number) arrives, a row FEC packet is emitted for
//! every complete row.
//!
//! Every FEC packet is the XOR of a complete group of cells of one length,
//! sitting at exactly SNBase plus that group's offsets, and names its group
//! by its true column or row number. So a decoder holding the same sequence
//! numbers rebuilds exactly the bytes that were sent. Groups that cannot
//! meet that get no FEC at all (see [`FecEncoder::process`]).
//!
//! The one thing the two ends can hold differently is a cell whose sequence
//! number is sent twice with different bytes (an upstream that restarts, or
//! is switched onto a stream numbered nearby, forwarded unchanged by a
//! 2022-7 output): the encoder keeps the first copy and a receiver whichever
//! reached it first. So from then on no FEC goes out for that cell's column
//! or row in that matrix. A full matrix keeps its cells until the next
//! matrix starts, so a step back into one the encoder has finished is a
//! repeat of every cell it touches and sends nothing: a receiver whose own
//! matrix is still open (it has a loss it could not rebuild) may hold the
//! first copies, and new parity over the second would let it rebuild bytes
//! never sent. FEC already sent over the first copy stays as sent; a
//! receiver that lost the first copy and holds the second can still rebuild
//! from it a packet that was never sent, which only happens when the step
//! back lands in a group whose FEC has already gone.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

use super::matrix::FecMatrix;
use super::{FEC_HEADER_LEN, FEC_MAGIC};

/// How many columns, and how many rows, the FEC header can name.
///
/// The bilbycast FEC header carries the column or row number in three bits
/// (byte 8, bits 2-0, see [`build_fec_header`]), and every released decoder
/// files a parity packet under that three-bit number. A column or row from
/// 8 up would be filed under the column or row 8 below it, where a decoder
/// would rebuild a packet from another group's parity. So the encoder sends
/// no FEC for them: with L or D above 8, columns and rows 8 and up have no
/// parity of their own, and a cell in both (column >= 8 and row >= 8) has no
/// protection at all (edge#206).
pub const FEC_INDEXED_GROUPS: u8 = 8;

/// FEC Encoder that generates FEC packets from a stream of media packets.
pub struct FecEncoder {
    matrix: FecMatrix,
    /// Total cells in one cycle (L * D)
    matrix_size: u16,
    /// Base RTP sequence number (SNBase) of the current matrix
    base_seq: u16,
    /// Columns (bit c) and rows (bit r) of the current matrix holding a
    /// cell whose sequence number came again with other bytes: they get no
    /// further FEC in this matrix. L and D are at most 20.
    unprotected_cols: u32,
    unprotected_rows: u32,
    /// Counter for FEC packets generated (wired to stats)
    stats_fec_sent: Arc<AtomicU64>,
    /// Whether `base_seq` has been set
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
            matrix_size,
            base_seq: 0,
            unprotected_cols: 0,
            unprotected_rows: 0,
            stats_fec_sent,
            initialized: false,
        }
    }

    /// Feed a media packet into the encoder: `seq` is the RTP sequence
    /// number the packet goes out under and `payload` everything after its
    /// 12-byte RTP header, which is what the decoder files and XORs.
    ///
    /// Returns the FEC packets this packet completes (may be empty), each
    /// prefixed with [`FEC_MAGIC`] and the 10-byte FEC header. FEC is sent
    /// for a column or row only when all of these hold:
    ///
    /// - every cell of it holds a packet (a sequence number the sender
    ///   never sent leaves its column and row unprotected);
    /// - its packets are all the same length: the XOR pads to the longest
    ///   and the header carries no length recovery, so a shorter packet
    ///   would be rebuilt with a zero tail;
    /// - its number is below [`FEC_INDEXED_GROUPS`];
    /// - for a row, the matrix has more than one column: a one-cell row's
    ///   parity is a copy of that packet, which a decoder that has already
    ///   closed the matrix would publish a second time;
    /// - no cell of it has had its sequence number sent again with other
    ///   bytes in this matrix: a receiver may hold either copy.
    pub fn process(&mut self, seq: u16, payload: &[u8]) -> Vec<Bytes> {
        // The decoder's placement rule (`FecDecoder::process_media`): a cell
        // sits at `seq - SNBase`, and a packet outside
        // [SNBase, SNBase + L*D) -- past the window, or any step back --
        // starts the next matrix at its own number.
        if !self.initialized || seq.wrapping_sub(self.base_seq) >= self.matrix_size {
            self.reset_matrix();
            self.base_seq = seq;
            self.initialized = true;
        }
        let position = seq.wrapping_sub(self.base_seq) as usize;
        if self.matrix.has_cell(position) {
            // A repeat. With the same bytes the cell and every parity over
            // it are unchanged. With other bytes a receiver holds whichever
            // copy reached it first, so no parity yet to be sent may cover
            // this cell. The comparison runs on repeats only.
            if self.matrix.cell(position) != Some(payload) {
                let (col, row) = self.matrix.position_to_col_row(position);
                self.unprotected_cols |= 1u32 << col;
                self.unprotected_rows |= 1u32 << row;
            }
            return Vec::new();
        }
        self.matrix.insert_media(position, payload);

        let mut fec_packets = Vec::new();
        let columns = self.matrix.columns;
        let rows = self.matrix.rows;

        let (col, _) = self.matrix.position_to_col_row(position);
        if col < FEC_INDEXED_GROUPS
            && self.matrix.col_count(col) == rows
            && self.unprotected_cols & (1u32 << col) == 0
            && self.group_is_uniform((0..rows as usize).map(|r| r * columns as usize + col as usize))
            && let Some(col_fec_payload) = self.matrix.generate_col_fec(col)
        {
            let mut fec_pkt = build_fec_header(self.base_seq, col, columns, true);
            fec_pkt.extend_from_slice(&col_fec_payload);
            fec_packets.push(Bytes::from(fec_pkt));
        }

        // The matrix's last cell by sequence number: send the row FEC.
        if position + 1 == self.matrix_size as usize && columns > 1 {
            for r in 0..rows.min(FEC_INDEXED_GROUPS) {
                let first = r as usize * columns as usize;
                if self.matrix.row_count(r) == columns
                    && self.unprotected_rows & (1u32 << r) == 0
                    && self.group_is_uniform(first..first + columns as usize)
                    && let Some(row_fec_payload) = self.matrix.generate_row_fec(r)
                {
                    let mut fec_pkt = build_fec_header(
                        self.base_seq,
                        r,
                        1, // row FEC offset = 1 (adjacent packets)
                        false,
                    );
                    fec_pkt.extend_from_slice(&row_fec_payload);
                    fec_packets.push(Bytes::from(fec_pkt));
                }
            }
        }

        // A matrix -- full or with a hole -- is kept until the next packet
        // past the window starts the next one at its own number, the
        // decoder's rollover. A late packet that fills a hole is still filed
        // under this SNBase; a step back into a full matrix is a repeat of
        // every cell, so no FEC goes out under this SNBase once it is full
        // (the decoder closes its matrix as it fills, a receiver with a loss
        // keeps its open, and it may hold either copy of a re-sent number).

        if !fec_packets.is_empty() {
            self.stats_fec_sent
                .fetch_add(fec_packets.len() as u64, Ordering::Relaxed);
        }
        fec_packets
    }

    /// Empty the matrix for the next one.
    fn reset_matrix(&mut self) {
        self.matrix.reset();
        self.unprotected_cols = 0;
        self.unprotected_rows = 0;
    }

    /// Whether every cell at `positions` holds a packet of one length.
    /// Called only for a complete group, at most 20 cells.
    fn group_is_uniform(&self, mut positions: impl Iterator<Item = usize>) -> bool {
        let Some(first) = positions.next() else {
            return true;
        };
        let len = self.matrix.cell(first).map(<[u8]>::len);
        positions.all(|p| self.matrix.cell(p).map(<[u8]>::len) == len)
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
/// The Index field holds three bits, so `index` must be below
/// [`FEC_INDEXED_GROUPS`]; [`FecEncoder::process`] never asks for more.
///
/// Prepended with the [`FEC_MAGIC`] four-byte marker so receivers can
/// distinguish FEC datagrams from RTP media on the same UDP port.
fn build_fec_header(
    sn_base: u16,
    index: u8,
    offset: u8,
    is_column: bool,
) -> Vec<u8> {
    debug_assert!(index < FEC_INDEXED_GROUPS, "FEC index {index} does not fit the header");
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

    // ── edge#206: cells by sequence number, and the FEC guards ──────────

    /// A decoded view of one emitted FEC packet.
    #[derive(Debug, Clone, PartialEq)]
    struct Fec {
        sn_base: u16,
        is_column: bool,
        index: u8,
        offset: u8,
        payload: Vec<u8>,
    }

    fn parse(pkt: &[u8]) -> Fec {
        assert_eq!(&pkt[..FEC_MAGIC.len()], &FEC_MAGIC);
        let body = &pkt[FEC_MAGIC.len()..];
        Fec {
            sn_base: u16::from_be_bytes([body[0], body[1]]),
            is_column: body[8] & 0x40 != 0,
            index: body[8] & 0x07,
            offset: body[9],
            payload: body[FEC_HEADER_LEN..].to_vec(),
        }
    }

    /// A distinct 8-byte payload per sequence number.
    fn payload(seq: u16) -> Vec<u8> {
        let s = seq.to_be_bytes();
        vec![s[0], s[1], 0xA5, s[1] ^ 0x5A, s[0].wrapping_add(7), 0x3C, s[1].wrapping_mul(3), 0x81]
    }

    fn xor_of(seqs: impl IntoIterator<Item = u16>) -> Vec<u8> {
        let mut acc = vec![0u8; 8];
        for s in seqs {
            for (a, b) in acc.iter_mut().zip(payload(s)) {
                *a ^= b;
            }
        }
        acc
    }

    fn feed(enc: &mut FecEncoder, seq: u16) -> Vec<Fec> {
        enc.process(seq, &payload(seq)).iter().map(|p| parse(p)).collect()
    }

    /// L=2 D=4 with seq 103 never sent. Column 0 (100/102/104/106) is
    /// whole and protected at SNBase 100 by its true XOR; column 1 and
    /// row 1 hold the hole and get nothing. Arrival counting placed 104
    /// in 103's cell and protected the wrong packets.
    #[test]
    fn cells_are_addressed_by_sequence_number() {
        let mut enc = FecEncoder::new(2, 4, Arc::new(AtomicU64::new(0)));
        let mut out = Vec::new();
        for seq in [100u16, 101, 102, 104, 105, 106] {
            out.extend(feed(&mut enc, seq));
        }
        assert_eq!(
            out,
            vec![Fec {
                sn_base: 100,
                is_column: true,
                index: 0,
                offset: 2,
                payload: xor_of([100, 102, 104, 106]),
            }],
            "column 0 is protected by its true cells; column 1 has a hole",
        );
        // 107 is the matrix's last cell: rows 0, 2 and 3 are whole.
        let rows = feed(&mut enc, 107);
        let expect = |r: u8, a: u16| Fec {
            sn_base: 100,
            is_column: false,
            index: r,
            offset: 1,
            payload: xor_of([a, a + 1]),
        };
        assert_eq!(rows, vec![expect(0, 100), expect(2, 104), expect(3, 106)]);
        // The next matrix starts at 108, its own number.
        let mut out = Vec::new();
        for seq in 108u16..=114 {
            out.extend(feed(&mut enc, seq));
        }
        assert!(out.iter().all(|f| f.sn_base == 108), "{out:?}");
        assert_eq!(out.iter().filter(|f| f.is_column).count(), 1, "column 0 of 108..");
    }

    /// A packet outside the current window -- forward past it, or any
    /// step back -- starts the next matrix at its own sequence number.
    #[test]
    fn a_packet_outside_the_window_starts_the_next_matrix() {
        for jump in [200u16, 95] {
            let mut enc = FecEncoder::new(2, 4, Arc::new(AtomicU64::new(0)));
            for seq in 100u16..=102 {
                assert!(feed(&mut enc, seq).is_empty());
            }
            let mut out = Vec::new();
            for k in 0..8u16 {
                out.extend(feed(&mut enc, jump.wrapping_add(k)));
            }
            assert!(!out.is_empty());
            assert!(out.iter().all(|f| f.sn_base == jump), "jump to {jump}: {out:?}");
            assert_eq!(out[0].payload, xor_of([jump, jump + 2, jump + 4, jump + 6]));
        }
    }

    /// A repeated sequence number neither moves a cell nor counts twice.
    #[test]
    fn a_repeated_sequence_number_changes_nothing() {
        let run = |repeat: bool| {
            let mut enc = FecEncoder::new(2, 4, Arc::new(AtomicU64::new(0)));
            let mut out = Vec::new();
            for seq in 100u16..108 {
                out.extend(feed(&mut enc, seq));
                if repeat && seq == 101 {
                    out.extend(feed(&mut enc, 101));
                }
            }
            out
        };
        assert_eq!(run(true), run(false));
        assert_eq!(run(false).len(), 2 + 4);
    }

    /// A number sent again with other bytes (L=2 D=4: 100..=103, then 101
    /// and 102 again, changed) leaves its column and row without further
    /// FEC in that matrix; a repeat with the same bytes changes nothing.
    #[test]
    fn a_number_resent_with_other_bytes_unprotects_its_column_and_row() {
        let mut enc = FecEncoder::new(2, 4, Arc::new(AtomicU64::new(0)));
        let mut out = Vec::new();
        for seq in 100u16..104 {
            out.extend(feed(&mut enc, seq));
        }
        out.extend(feed(&mut enc, 101)); // the same bytes again
        let changed = |s: u16| payload(s).iter().map(|b| b ^ 0xFF).collect::<Vec<u8>>();
        for seq in [101u16, 102] {
            out.extend(enc.process(seq, &changed(seq)).iter().map(|p| parse(p)));
        }
        for seq in 104u16..108 {
            out.extend(feed(&mut enc, seq));
        }
        // 101 is column 1 row 0, 102 column 0 row 1: only row 2 and row 3
        // are left to protect.
        let groups: Vec<(bool, u8)> = out.iter().map(|f| (f.is_column, f.index)).collect();
        assert_eq!(groups, vec![(false, 2), (false, 3)], "{out:?}");
        // The next matrix is protected in full.
        let mut out = Vec::new();
        for seq in 108u16..116 {
            out.extend(feed(&mut enc, seq));
        }
        assert_eq!(out.len(), 2 + 4);
    }

    /// A step back into a matrix the encoder has finished (L=2 D=4:
    /// 100..=107, then 102..=107 again, changed) sends nothing: a receiver
    /// still holding that matrix open may keep the first copies, and row
    /// FEC over the second would rebuild a cell from bytes never sent. The
    /// next matrix starts at the window's end and is protected in full.
    #[test]
    fn a_step_back_into_a_finished_matrix_sends_nothing() {
        let mut enc = FecEncoder::new(2, 4, Arc::new(AtomicU64::new(0)));
        let mut out = Vec::new();
        for seq in 100u16..108 {
            out.extend(feed(&mut enc, seq));
        }
        assert_eq!(out.len(), 2 + 4);
        let changed = |s: u16| payload(s).iter().map(|b| b ^ 0xFF).collect::<Vec<u8>>();
        for seq in 102u16..108 {
            assert!(enc.process(seq, &changed(seq)).is_empty(), "{seq} again");
        }
        let mut out = Vec::new();
        for seq in 108u16..116 {
            out.extend(feed(&mut enc, seq));
        }
        assert_eq!(out.len(), 2 + 4);
        assert!(out.iter().all(|f| f.sn_base == 108), "{out:?}");
    }

    /// L=1: a row is one cell, so row FEC would be a copy of a packet. Only
    /// the column FEC goes out (the old encoder sent 1 + D per matrix).
    #[test]
    fn one_column_matrix_sends_column_fec_only() {
        let stats = Arc::new(AtomicU64::new(0));
        let mut enc = FecEncoder::new(1, 4, stats.clone());
        let mut out = Vec::new();
        for seq in 500u16..504 {
            out.extend(feed(&mut enc, seq));
        }
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].is_column);
        assert_eq!(out[0].payload, xor_of(500..504));
        assert_eq!(stats.load(Ordering::Relaxed), 1);
    }

    /// A group with packets of two lengths gets no FEC (its rebuild would
    /// carry a zero tail); every other group is unaffected.
    #[test]
    fn unequal_lengths_suppress_that_group_only() {
        let mut enc = FecEncoder::new(2, 4, Arc::new(AtomicU64::new(0)));
        let mut out = Vec::new();
        for seq in 0u16..8 {
            let p = payload(seq);
            let p = if seq == 0 { &p[..4] } else { &p[..] }; // column 0, row 0
            out.extend(enc.process(seq, p).iter().map(|b| parse(b)));
        }
        let cols: Vec<u8> = out.iter().filter(|f| f.is_column).map(|f| f.index).collect();
        let rows: Vec<u8> = out.iter().filter(|f| !f.is_column).map(|f| f.index).collect();
        assert_eq!(cols, vec![1]);
        assert_eq!(rows, vec![1, 2, 3]);
    }

    /// With L or D above 8, the 3-bit header index cannot name column or
    /// row 8 and up: none is sent, and every header names its true group.
    #[test]
    fn columns_and_rows_past_eight_send_no_fec() {
        for (l, d) in [(10u8, 10u8), (20, 20), (8, 8), (9, 4)] {
            let stats = Arc::new(AtomicU64::new(0));
            let mut enc = FecEncoder::new(l, d, stats.clone());
            let size = l as u16 * d as u16;
            let mut out = Vec::new();
            for seq in 0..size * 2 {
                out.extend(feed(&mut enc, seq));
            }
            let per_matrix = l.min(FEC_INDEXED_GROUPS) as usize + d.min(FEC_INDEXED_GROUPS) as usize;
            assert_eq!(out.len(), 2 * per_matrix, "{l}x{d}");
            assert_eq!(stats.load(Ordering::Relaxed), out.len() as u64);
            for f in &out {
                assert!(f.index < FEC_INDEXED_GROUPS);
                let k = f.sn_base;
                let expect = if f.is_column {
                    xor_of((0..d as u16).map(|r| k + r * l as u16 + f.index as u16))
                } else {
                    xor_of((0..l as u16).map(|c| k + f.index as u16 * l as u16 + c))
                };
                assert_eq!(f.payload, expect, "{l}x{d} {f:?}");
            }
        }
    }

    /// The encoder before edge#206, verbatim in behaviour: cells by arrival
    /// count, column FEC when a column fills, every row at the end.
    fn pre_206_encoder(columns: u8, rows: u8, packets: &[(u16, Vec<u8>)]) -> Vec<Vec<u8>> {
        let (l, d) = (columns as usize, rows as usize);
        let mut out = Vec::new();
        let mut cells: Vec<Vec<u8>> = Vec::new();
        let mut base = 0u16;
        for (seq, p) in packets {
            if cells.is_empty() {
                base = *seq;
            }
            cells.push(p.clone());
            let pos = cells.len() - 1;
            let header = |idx: usize, offset: u8, column: bool| {
                let mut h = FEC_MAGIC.to_vec();
                h.extend_from_slice(&base.to_be_bytes());
                h.extend_from_slice(&[0; 6]);
                h.push(if column { 0x40 } else { 0 } | (idx as u8 & 0x07));
                h.push(offset);
                h
            };
            let xor = |members: Vec<usize>| {
                let mut acc: Vec<u8> = Vec::new();
                for m in members {
                    if acc.len() < cells[m].len() {
                        acc.resize(cells[m].len(), 0);
                    }
                    for (a, b) in acc.iter_mut().zip(&cells[m]) {
                        *a ^= b;
                    }
                }
                acc
            };
            if pos / l == d - 1 {
                let col = pos % l;
                let mut pkt = header(col, columns, true);
                pkt.extend(xor((0..d).map(|r| r * l + col).collect()));
                out.push(pkt);
            }
            if cells.len() == l * d {
                for r in 0..d {
                    let mut pkt = header(r, 1, false);
                    pkt.extend(xor((0..l).map(|c| r * l + c).collect()));
                    out.push(pkt);
                }
                cells.clear();
            }
        }
        out
    }

    /// Contiguous sequence numbers of one length -- every re-wrapped
    /// output -- give exactly the FEC the encoder sent before edge#206,
    /// byte for byte, across a sequence-number wrap.
    #[test]
    fn contiguous_input_is_byte_identical_to_before() {
        for (l, d) in [(3u8, 4u8), (2, 4), (8, 8), (5, 6)] {
            let packets: Vec<(u16, Vec<u8>)> = (0..3 * l as u16 * d as u16)
                .map(|i| {
                    let seq = 65_500u16.wrapping_add(i);
                    let mut p = payload(seq);
                    p.extend_from_slice(&(i as u32).wrapping_mul(2_654_435_761).to_be_bytes());
                    (seq, p)
                })
                .collect();
            let mut enc = FecEncoder::new(l, d, Arc::new(AtomicU64::new(0)));
            let got: Vec<Vec<u8>> = packets
                .iter()
                .flat_map(|(s, p)| enc.process(*s, p))
                .map(|b| b.to_vec())
                .collect();
            assert_eq!(got, pre_206_encoder(l, d, &packets), "{l}x{d}");
        }
    }
}
