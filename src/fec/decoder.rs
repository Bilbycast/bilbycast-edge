// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SMPTE 2022-1 FEC Decoder — recovers lost RTP packets using FEC data.
//!
//! The decoder maintains an L × D FEC matrix. Media packets are placed into
//! the matrix by their sequence number and accumulated into per-column and
//! per-row XOR parity. Column and row FEC packets arrive on the same UDP
//! port prefixed with [`super::FEC_MAGIC`] and the 10-byte bilbycast FEC
//! header (see [`super::encoder`] for the exact layout).
//!
//! When a column or row FEC packet arrives and exactly one media cell in
//! the protected group is missing, the decoder XORs the FEC payload against
//! the current column/row parity to reconstruct the missing payload. The
//! recovered payload is wrapped back into an RTP datagram (synthesising an
//! RTP header from the last-seen media header) and returned so the input
//! task can publish it into the broadcast channel.
//!
//! Recovery is iterative: a newly reconstructed cell may complete another
//! row or column, allowing a cascade. Each incoming packet therefore
//! triggers a bounded recovery sweep.
//!
//! Media packets are filed by the placement rule the encoder shares
//! ([`super::matrix::Placement`]): a packet past the window starts the next
//! matrix, as does one a whole matrix or more behind it (a stream that
//! moved). One less than a matrix behind is late, reordered on the way, and
//! never discards the open matrix (edge#214): it moves SNBase back onto
//! itself only while no FEC has named SNBase and the move is short and
//! fits, and is otherwise returned unfiled. A FEC packet whose SNBase
//! differs from the decoder's realigns the matrix to it (the first packets
//! of a matrix lost or reordered, so it opened late), dropping any FEC held
//! for the old SNBase. Only a packet with a payload fills a cell, once; a
//! matrix that fills is closed and takes nothing more until a new SNBase is
//! established; and only a filed packet sets the header template rebuilt
//! packets reuse.
//!
//! ## Limitations
//!
//! - The bilbycast FEC header carries no `TS Recovery` value (it is zeroed
//!   by the encoder). Recovered packets reuse the most recent media
//!   timestamp as a best-effort value.
//! - FEC received before the first media packet is discarded, and the
//!   first cycle after startup can drop losses without repair. A parity
//!   that arrives before any media covers only packets sent before this
//!   receiver started, so it can only be applied to the wrong matrix.
//! - The header names a column or row in 3 bits, so a parity packet is
//!   filed under its index modulo 8. The encoder sends none for columns or
//!   rows from 8 up ([`super::encoder::FEC_INDEXED_GROUPS`]); one from an
//!   older encoder that did is filed in the wrong slot.
//! - A packet rebuilt here is usually rebuilt after newer ones were
//!   received. What the caller does with it is the caller's: a single-leg
//!   RTP input publishes only one ahead of everything already published
//!   (`engine::input_rtp::RepairOrder`), using [`FecDecoder::holds_payload`]
//!   to drop a received datagram the matrix already holds, and its own
//!   memory of recent repairs for one whose matrix has since closed.
use bytes::Bytes;

use crate::engine::packet::RtpPacket;
use crate::util::time::now_us;

use super::matrix::{FecMatrix, Placement};
use super::{FEC_HEADER_LEN, FEC_MAGIC};

/// Size of the RTP header synthesised around recovered payloads.
const RTP_HEADER_LEN: usize = 12;

/// FEC Decoder that processes incoming packets and attempts recovery.
pub struct FecDecoder {
    matrix: FecMatrix,
    columns: u8,
    rows: u8,
    matrix_size: u16,
    /// Base sequence number for the current matrix cycle.
    base_seq: u16,
    /// Number of media packets received in this cycle.
    media_count: u16,
    /// Whether `base_seq` has been set.
    initialized: bool,
    /// Where the previous matrix's window ended, when the open matrix
    /// followed it ([`Placement::Past`]): a late packet behind it belongs to
    /// that matrix. `None` for the first matrix opened and after a step
    /// back of a whole matrix ([`Placement::Moved`]).
    floor: Option<u16>,
    /// Whether the open matrix's SNBase is settled: a FEC packet has named
    /// it, or the matrix has filled. A late packet no longer moves it back.
    base_settled: bool,
    /// Whether the matrix filled and was closed. A closed matrix takes
    /// nothing -- no media packet, no FEC -- until a new SNBase is
    /// established, by a packet past its window or a FEC packet naming
    /// another. It used to keep its SNBase over an emptied matrix and file
    /// what came next under it, so a sender re-sending a number with other
    /// bytes had that copy combined with the row FEC sent after the
    /// original: with two columns, a packet rebuilt from bytes never sent.
    closed: bool,
    /// The SNBase the open matrix was opened at, or a FEC packet named. A
    /// late packet's move back leaves it, and a stream that moved is told
    /// against it: the encoder never moves back, so judged against the
    /// moved SNBase the decoder could keep a matrix the encoder has left.
    opened_at: u16,
    /// Last seen media RTP header template. Used to synthesise RTP headers
    /// around recovered payloads. `None` before any media has arrived.
    last_rtp_header: Option<[u8; RTP_HEADER_LEN]>,
    /// Last seen media timestamp — best-effort fill-in when recovering.
    last_rtp_timestamp: u32,
    /// Per-column FEC payloads pending recovery. Index = column (0..L).
    col_fec: Vec<Option<Vec<u8>>>,
    /// Per-row FEC payloads pending recovery. Index = row (0..D).
    row_fec: Vec<Option<Vec<u8>>>,
    /// Count of successfully recovered media packets since construction.
    recovered_count: u64,
}

/// Parsed bilbycast FEC header (post-magic).
#[derive(Debug, Clone, Copy)]
struct FecHeader {
    sn_base: u16,
    is_column: bool,
    index: u8,
    /// Kept for parity with real SMPTE 2022-1 `Offset` field; unused today
    /// because column/row addressing is derived from `index` + matrix shape.
    _offset: u8,
}

impl FecHeader {
    fn parse(body: &[u8]) -> Option<Self> {
        if body.len() < FEC_HEADER_LEN {
            return None;
        }
        let sn_base = ((body[0] as u16) << 8) | body[1] as u16;
        let is_column = body[8] & 0x40 != 0;
        let index = body[8] & 0x07;
        let _offset = body[9];
        Some(Self { sn_base, is_column, index, _offset })
    }
}

/// Returns `true` if `data` carries the bilbycast FEC magic prefix.
///
/// Safe to call on any datagram — RTP's V=2 prefix (`0x80..=0xBF`) can
/// never collide with [`FEC_MAGIC`]'s first byte (`'B' = 0x42`).
pub fn is_fec_packet(data: &[u8]) -> bool {
    data.len() >= FEC_MAGIC.len() + FEC_HEADER_LEN
        && data[..FEC_MAGIC.len()] == FEC_MAGIC
}

impl FecDecoder {
    /// Create a new FEC decoder with the given L (columns) and D (rows) parameters.
    pub fn new(columns: u8, rows: u8) -> Self {
        let matrix_size = columns as u16 * rows as u16;
        Self {
            matrix: FecMatrix::new(columns, rows),
            columns,
            rows,
            matrix_size,
            base_seq: 0,
            media_count: 0,
            initialized: false,
            floor: None,
            base_settled: false,
            closed: false,
            opened_at: 0,
            last_rtp_header: None,
            last_rtp_timestamp: 0,
            col_fec: vec![None; columns as usize],
            row_fec: vec![None; rows as usize],
            recovered_count: 0,
        }
    }

    /// Total number of packets recovered through FEC since construction.
    #[allow(dead_code)]
    pub fn recovered_count(&self) -> u64 {
        self.recovered_count
    }

    /// Whether the current matrix already holds exactly this media
    /// datagram's payload under its sequence number: a copy of a packet
    /// received before, or one FEC has already rebuilt. Ask it before
    /// [`Self::process_media`], which files the datagram by the same rule
    /// (a datagram outside the window, past it or behind it, is never held).
    pub fn holds_payload(&self, seq: u16, data: &[u8]) -> bool {
        if !self.initialized || data.len() <= RTP_HEADER_LEN {
            return false;
        }
        let diff = seq.wrapping_sub(self.base_seq);
        diff < self.matrix_size
            && self.matrix.cell(diff as usize) == Some(&data[RTP_HEADER_LEN..])
    }

    /// Process an incoming RTP media packet.
    ///
    /// Places the packet into the FEC matrix by the rule the encoder shares
    /// ([`FecMatrix::place`]), captures a header template for later
    /// recovery, then attempts to repair any protected group that now has
    /// exactly one missing cell. Returns the originally received packet
    /// followed by every packet that was reconstructed as a side effect.
    ///
    /// A packet behind the open matrix by less than its size is late, not a
    /// new stream: it moves SNBase back onto itself only while nothing has
    /// settled SNBase and the move is short and fits (`take_late`), and is
    /// otherwise returned but filed nowhere. It used to start a new matrix,
    /// dropping the open one's cells and pending FEC, so a reorder across a
    /// matrix boundary cost the next matrix its repairs (edge#214).
    pub fn process_media(&mut self, seq: u16, ts: u32, data: &Bytes) -> Vec<RtpPacket> {
        let position = if !self.initialized {
            self.start_matrix(seq, None);
            Some(0)
        } else {
            match self.matrix.place(self.base_seq, seq) {
                Placement::Cell(_) if self.closed => None,
                Placement::Cell(position) => Some(position),
                // A whole matrix behind where the matrix opened: a stream
                // that moved, as the encoder (which never moves back) sees it.
                Placement::Late(_) if self.opened_at.wrapping_sub(seq) >= self.matrix_size => {
                    self.start_matrix(seq, None);
                    Some(0)
                }
                Placement::Late(behind) => self.take_late(seq, behind).then_some(0),
                Placement::Past => {
                    let end = self.base_seq.wrapping_add(self.matrix_size);
                    self.start_matrix(seq, Some(end));
                    Some(0)
                }
                Placement::Moved => {
                    self.start_matrix(seq, None);
                    Some(0)
                }
            }
        };
        // Only a packet with a payload fills a cell, and only the first copy
        // of it: a header-only datagram, or one repeated, used to count
        // towards a full matrix too, closing it with a cell still empty.
        if let Some(position) = position
            && !self.matrix.has_cell(position)
            && data.len() > RTP_HEADER_LEN
        {
            self.matrix.insert_media(position, &data[RTP_HEADER_LEN..]);
            self.media_count += 1;
            // The latest filed packet's RTP header is the template recovered
            // packets reuse. Only the first 12 bytes are kept — CSRC
            // identifiers / header extensions are not preserved, which
            // matches the simplified 2022-1 semantics we emit on the encoder
            // side. A packet left out of the matrix (a late one, with an old
            // timestamp) does not set it.
            let mut hdr = [0u8; RTP_HEADER_LEN];
            hdr.copy_from_slice(&data[..RTP_HEADER_LEN]);
            self.last_rtp_header = Some(hdr);
            self.last_rtp_timestamp = ts;
        }

        let mut result = Vec::with_capacity(1);
        result.push(RtpPacket {
            data: data.clone(),
            sequence_number: seq,
            rtp_timestamp: ts,
            recv_time_us: now_us(),
            is_raw_ts: false,
            // The wire seq: a FEC-decoded input stamps it like any RTP
            // input, so a seq-aware Hitless slot can merge it.
            upstream_seq: Some(seq),
            upstream_leg_id: None,
            sender_timestamp_us: None,
        });

        self.attempt_recovery(&mut result);

        if self.media_count >= self.matrix_size {
            self.reset_cycle();
            self.base_settled = true;
            self.closed = true;
        }

        result
    }

    /// Open the next matrix at `seq`. `floor` is where the window being
    /// left ended, when `seq` moved past it.
    fn start_matrix(&mut self, seq: u16, floor: Option<u16>) {
        self.reset_cycle();
        self.base_seq = seq;
        self.opened_at = seq;
        self.initialized = true;
        self.floor = floor;
        self.base_settled = false;
        self.closed = false;
    }

    /// A late packet ([`Placement::Late`]), `behind` cells before SNBase.
    /// Returns whether it is filed (at position 0).
    ///
    /// While no FEC packet has named this matrix's SNBase and the matrix has
    /// not filled, the packet may be the matrix's own first, lost or
    /// reordered behind later ones so the matrix opened late: SNBase moves
    /// back onto it when the matrix allows ([`FecMatrix::can_move_back`])
    /// and the packet is fewer than L - 1 behind where the matrix opened.
    /// The encoder never moves back, so where its matrix did start at the
    /// opening (it reordered the packet itself), its first column FEC,
    /// sent after the first cell of its last row, must realign the decoder
    /// before the moved window fills or ends; a longer move would close a
    /// matrix the encoder is still sending. Otherwise the packet belongs to
    /// an earlier matrix, or does not fit this one, and is left out.
    ///
    /// Once SNBase is settled, a late packet is a straggler or the first of
    /// a restart about to send numbers this matrix holds again, perhaps
    /// with other bytes. The FEC held for groups still short of a cell
    /// covers the copies sent first, so a second copy filed into one of
    /// those gaps would rebuild a packet from bytes never sent: that FEC is
    /// dropped. (The encoder sends no more FEC over a cell re-sent with
    /// other bytes.)
    fn take_late(&mut self, seq: u16, behind: u16) -> bool {
        if self.base_settled {
            self.col_fec.fill(None);
            self.row_fec.fill(None);
            return false;
        }
        if self.opened_at.wrapping_sub(seq) + 1 >= self.columns as u16
            || !self.matrix.can_move_back(seq, behind, self.floor)
        {
            return false;
        }
        self.align_base_to(seq)
    }

    /// Process an incoming FEC packet (column or row parity).
    ///
    /// Returns any media packets that were reconstructed as a result of
    /// this FEC payload, including cascades.
    pub fn process_fec(&mut self, data: &[u8]) -> Vec<RtpPacket> {
        let mut result = Vec::new();

        if data.len() < FEC_MAGIC.len() + FEC_HEADER_LEN || data[..FEC_MAGIC.len()] != FEC_MAGIC {
            return result;
        }
        let body_start = FEC_MAGIC.len();
        let hdr = match FecHeader::parse(&data[body_start..]) {
            Some(h) => h,
            None => return result,
        };
        let payload = &data[body_start + FEC_HEADER_LEN..];

        // Before the first media packet, discard the parity. The encoder
        // sends each one right after the last cell it covers (a column's at
        // its last row, the rows' at the matrix's last cell — see
        // `FecEncoder::process`), so one received before any media covers
        // only packets sent before this receiver started. Kept, it would be
        // applied to the first matrix the receiver opens and rebuild
        // packets with bytes never sent, or a real packet a second time
        // (edge#214).
        if !self.initialized {
            return result;
        }

        // The FEC header's `sn_base` is authoritative. When the first
        // media packet of a group was lost, `self.base_seq` was set to a
        // later sequence and every cell sits at a shifted position.
        // Realign the matrix against `sn_base` before storing this FEC; on
        // a closed matrix that establishes a new SNBase. FEC for a closed
        // matrix's own SNBase is the rest of what the sender sent for it,
        // which a closed matrix no longer takes.
        if hdr.sn_base != self.base_seq {
            if !self.align_base_to(hdr.sn_base) {
                return result;
            }
            self.closed = false;
        } else if self.closed {
            return result;
        }

        self.store_fec(&hdr, payload);
        // The sender has named this matrix's SNBase: a late packet no
        // longer moves it back (`take_late`).
        self.base_settled = true;
        self.opened_at = self.base_seq;
        self.attempt_recovery(&mut result);
        result
    }

    /// Reseat the matrix so `base_seq == new_base`, preserving already-
    /// received media cells by re-inserting them at their correct
    /// positions relative to the new base, and dropping any FEC held for
    /// the old base. Returns `false` if the shift
    /// would move the base outside the current cycle's window (e.g.
    /// stale FEC from a prior matrix) — callers should discard in that
    /// case.
    fn align_base_to(&mut self, new_base: u16) -> bool {
        let shift = new_base.wrapping_sub(self.base_seq);
        // A forward shift beyond a cycle means this FEC refers to a
        // future matrix; a backward shift that doesn't fit either means
        // the FEC is stale. Accept only shifts where every already-
        // stored cell still lands inside the new window.
        let backward = self.base_seq.wrapping_sub(new_base);
        let within_window = (shift as usize) < self.matrix.size()
            || (backward as usize) < self.matrix.size();
        if !within_window {
            return false;
        }

        // Snapshot existing cells by absolute sequence number.
        let mut snapshot: Vec<(u16, Vec<u8>)> = Vec::new();
        for pos in 0..self.matrix.size() {
            if let Some(payload) = self.matrix.cell(pos) {
                let seq = self.base_seq.wrapping_add(pos as u16);
                snapshot.push((seq, payload.to_vec()));
            }
        }

        self.matrix.reset();
        self.media_count = 0;
        // FEC held for the old SNBase was computed over that layout's
        // groups: against the new one a column or row index names other
        // cells, and it would rebuild a packet from another group's parity.
        // So it goes. (Only FEC that settled SNBase is ever held, so a late
        // packet's move back drops none; a FEC packet naming another
        // SNBase after one was held does.)
        self.col_fec.fill(None);
        self.row_fec.fill(None);
        self.base_seq = new_base;

        for (seq, payload) in snapshot {
            let diff = seq.wrapping_sub(new_base);
            if (diff as usize) < self.matrix.size() {
                self.matrix.insert_media(diff as usize, &payload);
                self.media_count += 1;
            }
        }
        true
    }

    fn store_fec(&mut self, hdr: &FecHeader, payload: &[u8]) {
        if hdr.is_column {
            if let Some(slot) = self.col_fec.get_mut(hdr.index as usize) {
                *slot = Some(payload.to_vec());
            }
        } else if let Some(slot) = self.row_fec.get_mut(hdr.index as usize) {
            *slot = Some(payload.to_vec());
        }
    }

    /// Sweep every column and row with pending FEC; when exactly one cell is
    /// missing, reconstruct it, synthesise an RTP datagram, push it onto
    /// `out`, and loop again in case the insertion unlocks another group.
    fn attempt_recovery(&mut self, out: &mut Vec<RtpPacket>) {
        let template = match self.last_rtp_header {
            Some(h) => h,
            None => return,
        };

        // Bound the cascade at 2 × matrix_size — the theoretical maximum is
        // one recovery per cell, so this is defensive against pathological
        // inputs, not a functional limit.
        let max_iters = (self.matrix_size as usize).saturating_mul(2).max(8);
        for _ in 0..max_iters {
            let mut progressed = false;

            for c in 0..self.columns {
                if self.col_fec[c as usize].is_some()
                    && self.matrix.col_count(c) == self.rows.saturating_sub(1)
                    && let Some(pkt) = self.recover_column(c, &template) {
                        out.push(pkt);
                        progressed = true;
                    }
            }

            for r in 0..self.rows {
                if self.row_fec[r as usize].is_some()
                    && self.matrix.row_count(r) == self.columns.saturating_sub(1)
                    && let Some(pkt) = self.recover_row(r, &template) {
                        out.push(pkt);
                        progressed = true;
                    }
            }

            if !progressed {
                break;
            }
        }
    }

    fn recover_column(&mut self, col: u8, template: &[u8; RTP_HEADER_LEN]) -> Option<RtpPacket> {
        let fec_payload = self.col_fec[col as usize].take()?;
        let missing_position = self.find_missing_in_column(col)?;

        // XOR: missing_payload = fec_payload XOR (XOR of all other cells in the column).
        let mut recovered = fec_payload;
        xor_with_slice(&mut recovered, self.matrix.col_parity(col));

        let missing_seq = self.base_seq.wrapping_add(missing_position as u16);
        self.matrix.insert_media(missing_position, &recovered);
        self.media_count += 1;
        self.recovered_count += 1;

        Some(self.wrap_recovered(missing_seq, template, &recovered))
    }

    fn recover_row(&mut self, row: u8, template: &[u8; RTP_HEADER_LEN]) -> Option<RtpPacket> {
        let fec_payload = self.row_fec[row as usize].take()?;
        let missing_position = self.find_missing_in_row(row)?;

        let mut recovered = fec_payload;
        xor_with_slice(&mut recovered, self.matrix.row_parity(row));

        let missing_seq = self.base_seq.wrapping_add(missing_position as u16);
        self.matrix.insert_media(missing_position, &recovered);
        self.media_count += 1;
        self.recovered_count += 1;

        Some(self.wrap_recovered(missing_seq, template, &recovered))
    }

    fn find_missing_in_column(&self, col: u8) -> Option<usize> {
        let columns = self.columns as usize;
        let rows = self.rows as usize;
        for row in 0..rows {
            let pos = row * columns + col as usize;
            if !self.matrix.has_cell(pos) {
                return Some(pos);
            }
        }
        None
    }

    fn find_missing_in_row(&self, row: u8) -> Option<usize> {
        let columns = self.columns as usize;
        for col in 0..columns {
            let pos = row as usize * columns + col;
            if !self.matrix.has_cell(pos) {
                return Some(pos);
            }
        }
        None
    }

    fn wrap_recovered(
        &self,
        seq: u16,
        template: &[u8; RTP_HEADER_LEN],
        payload: &[u8],
    ) -> RtpPacket {
        let mut buf = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        buf.extend_from_slice(template);
        // Patch sequence number into the RTP header bytes 2-3.
        buf[2] = (seq >> 8) as u8;
        buf[3] = seq as u8;
        // Patch timestamp (bytes 4-7) with last observed media timestamp as a
        // best-effort fill. The 2022-1 `TS Recovery` field would give the
        // exact value but our simplified framing zeroes it.
        buf[4] = (self.last_rtp_timestamp >> 24) as u8;
        buf[5] = (self.last_rtp_timestamp >> 16) as u8;
        buf[6] = (self.last_rtp_timestamp >> 8) as u8;
        buf[7] = self.last_rtp_timestamp as u8;
        buf.extend_from_slice(payload);

        RtpPacket {
            data: Bytes::from(buf),
            sequence_number: seq,
            rtp_timestamp: self.last_rtp_timestamp,
            recv_time_us: now_us(),
            is_raw_ts: false,
            // The recovered packet's wire seq, as for a received one.
            upstream_seq: Some(seq),
            upstream_leg_id: None,
            sender_timestamp_us: None,
        }
    }

    fn reset_cycle(&mut self) {
        self.matrix.reset();
        self.media_count = 0;
        self.col_fec.fill(None);
        self.row_fec.fill(None);
    }
}

fn xor_with_slice(target: &mut Vec<u8>, source: &[u8]) {
    if source.len() > target.len() {
        target.resize(source.len(), 0);
    }
    for (t, s) in target.iter_mut().zip(source.iter()) {
        *t ^= *s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::encoder::FecEncoder;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    fn make_rtp_packet(seq: u16, ts: u32, payload: &[u8]) -> Bytes {
        let mut data = vec![0u8; RTP_HEADER_LEN + payload.len()];
        data[0] = 0x80; // V=2
        data[1] = 96;   // PT=96
        data[2] = (seq >> 8) as u8;
        data[3] = seq as u8;
        data[4] = (ts >> 24) as u8;
        data[5] = (ts >> 16) as u8;
        data[6] = (ts >> 8) as u8;
        data[7] = ts as u8;
        data[RTP_HEADER_LEN..].copy_from_slice(payload);
        Bytes::from(data)
    }

    #[test]
    fn test_decoder_forwards_media() {
        let mut decoder = FecDecoder::new(2, 2);
        let pkt = make_rtp_packet(100, 0, &[0x01, 0x02]);
        let result = decoder.process_media(100, 0, &pkt);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sequence_number, 100);
    }

    #[test]
    fn is_fec_packet_rejects_rtp() {
        let rtp = make_rtp_packet(0, 0, &[0; 8]);
        assert!(!is_fec_packet(&rtp));
    }

    #[test]
    fn column_fec_recovers_single_missing_media() {
        // L=2 D=2 grid. Generate FEC for all four cells, drop cell 0, and
        // verify the column FEC for column 0 reconstructs the payload.
        let stats = Arc::new(AtomicU64::new(0));
        let mut enc = FecEncoder::new(2, 2, stats.clone());

        let payloads: [[u8; 4]; 4] = [
            [0x11, 0x22, 0x33, 0x44],
            [0x55, 0x66, 0x77, 0x88],
            [0x99, 0xAA, 0xBB, 0xCC],
            [0xDD, 0xEE, 0xFF, 0x00],
        ];
        let base_seq = 200u16;

        let mut fec_packets: Vec<Vec<u8>> = Vec::new();
        for (i, p) in payloads.iter().enumerate() {
            let seq = base_seq.wrapping_add(i as u16);
            let emitted = enc.process(seq, p);
            for f in emitted {
                fec_packets.push(f.to_vec());
            }
        }
        assert!(fec_packets.iter().any(|p| is_fec_packet(p)));

        let mut dec = FecDecoder::new(2, 2);
        // Deliver cells 1, 2, 3 but drop cell 0 (column 0, row 0).
        for i in [1usize, 2, 3] {
            let seq = base_seq.wrapping_add(i as u16);
            let rtp = make_rtp_packet(seq, 9_000, &payloads[i]);
            dec.process_media(seq, 9_000, &rtp);
        }
        assert_eq!(dec.recovered_count(), 0);

        // Feed the column-0 FEC packet. Should recover cell 0.
        // Column-0 FEC is emitted when the encoder receives the second
        // packet in column 0, which is position 2 (media index 2).
        // So in `fec_packets`, the first entry corresponds to column 0 FEC
        // emitted at encoder step 3 (0-indexed: after p[2]).
        // Let's scan and pick the column FEC for column 0.
        for pkt in &fec_packets {
            let recovered = dec.process_fec(pkt);
            if !recovered.is_empty() {
                assert_eq!(recovered[0].sequence_number, base_seq);
                let body = &recovered[0].data[RTP_HEADER_LEN..];
                assert_eq!(body, &payloads[0]);
                assert_eq!(dec.recovered_count(), 1);
                return;
            }
        }
        panic!("no recovery happened from column FEC");
    }

    #[test]
    fn row_fec_recovers_single_missing_media() {
        // L=3 D=2 grid. Drop cell at row 0, col 1; expect row-0 FEC recovery.
        let stats = Arc::new(AtomicU64::new(0));
        let mut enc = FecEncoder::new(3, 2, stats.clone());
        let payloads: Vec<[u8; 3]> = (0..6)
            .map(|i| [i as u8, (i * 2) as u8, (i * 3) as u8])
            .collect();
        let base_seq = 1000u16;

        let mut fec_packets: Vec<Vec<u8>> = Vec::new();
        for (i, p) in payloads.iter().enumerate() {
            let seq = base_seq.wrapping_add(i as u16);
            for f in enc.process(seq, p) {
                fec_packets.push(f.to_vec());
            }
        }

        let mut dec = FecDecoder::new(3, 2);
        let drop_idx: usize = 1;
        for (i, p) in payloads.iter().enumerate() {
            if i == drop_idx { continue; }
            let seq = base_seq.wrapping_add(i as u16);
            let rtp = make_rtp_packet(seq, 11_000, p);
            dec.process_media(seq, 11_000, &rtp);
        }
        // Feed every FEC packet; recovery should fire on the matching row.
        let mut total_recovered = 0;
        for f in &fec_packets {
            total_recovered += dec.process_fec(f).len();
        }
        assert!(total_recovered >= 1);
        assert!(dec.recovered_count() >= 1);
    }

    /// FEC that arrives before any media covers packets sent before the
    /// receiver started, so it is discarded. Kept, it was applied to the
    /// first matrix the receiver opened: media seq 4 opened a matrix at 4,
    /// and the stored column-0 parity "rebuilt" seq 6 from 0xAA bytes
    /// never sent under it (edge#214).
    #[test]
    fn fec_before_any_media_is_discarded() {
        let mut dec = FecDecoder::new(2, 2);
        // A column-0 FEC packet, SNBase 0, payload 0xAA...
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&FEC_MAGIC);
        pkt.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0x40, 2]);
        pkt.extend_from_slice(&[0xAA; 4]);
        assert!(dec.process_fec(&pkt).is_empty());
        let media = make_rtp_packet(4, 0, &[0x01, 0x02, 0x03, 0x04]);
        let out = dec.process_media(4, 0, &media);
        assert_eq!(out.len(), 1, "only the received packet");
        assert_eq!(out[0].sequence_number, 4);
        assert_eq!(dec.recovered_count(), 0);
    }

    #[test]
    fn fec_with_garbage_is_ignored() {
        let mut dec = FecDecoder::new(2, 2);
        assert!(dec.process_fec(&[0, 1, 2]).is_empty());
        assert!(dec.process_fec(&[0; 32]).is_empty()); // wrong magic
    }

    // ── edge#206: encoder and decoder end to end ────────────────────────

    /// A payload of `len` bytes, distinct per sequence number.
    fn body(seq: u16, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (seq as usize * 131 + i * 7 + (i >> 3) * 29) as u8 ^ (seq >> 8) as u8)
            .collect()
    }

    enum Wire {
        Media(u16, Bytes),
        Fec(Bytes),
    }

    /// What an output sends for `seqs`: each media datagram, then the FEC
    /// it completes. A header-only datagram is not given to the encoder, as
    /// `output_rtp::fec_for_datagram` does not.
    fn send(l: u8, d: u8, seqs: &[u16], len: impl Fn(u16) -> usize) -> Vec<Wire> {
        let mut enc = FecEncoder::new(l, d, Arc::new(AtomicU64::new(0)));
        let mut wire = Vec::new();
        for &seq in seqs {
            let p = body(seq, len(seq));
            let fec = if p.is_empty() { Vec::new() } else { enc.process(seq, &p) };
            wire.push(Wire::Media(seq, make_rtp_packet(seq, 0, &p)));
            wire.extend(fec.into_iter().map(Wire::Fec));
        }
        wire
    }

    /// Replay `wire` through a fresh decoder, leaving out the media of
    /// `lost` and the FEC datagrams `drop_fec` names (by their order among
    /// the FEC). Asserts every packet the decoder rebuilds is byte for byte
    /// the one sent under its sequence number, and returns their numbers.
    fn receive(
        l: u8,
        d: u8,
        wire: &[Wire],
        lost: &[u16],
        drop_fec: impl Fn(usize) -> bool,
    ) -> Vec<u16> {
        receive_from(l, d, wire, 0, lost, drop_fec)
    }

    /// [`receive`] by a receiver that starts at `wire[start]`: what was
    /// sent before it never reaches the decoder. The packets sent are read
    /// off the whole wire, so a correct rebuild of one sent before the
    /// start is accepted; `drop_fec` still counts the FEC from the wire's
    /// beginning.
    fn receive_from(
        l: u8,
        d: u8,
        wire: &[Wire],
        start: usize,
        lost: &[u16],
        drop_fec: impl Fn(usize) -> bool,
    ) -> Vec<u16> {
        replay(l, d, wire, start, lost, drop_fec)
            .into_iter()
            .filter_map(|(seq, rebuilt)| rebuilt.then_some(seq))
            .collect()
    }

    /// [`receive_from`]'s replay, returning every packet the decoder hands
    /// back in the order it hands it back: each received one (`false`)
    /// followed by those it rebuilt (`true`), each rebuild checked against
    /// what was sent.
    fn replay(
        l: u8,
        d: u8,
        wire: &[Wire],
        start: usize,
        lost: &[u16],
        drop_fec: impl Fn(usize) -> bool,
    ) -> Vec<(u16, bool)> {
        let sent: std::collections::HashMap<u16, &Bytes> = wire
            .iter()
            .filter_map(|w| match w {
                Wire::Media(s, b) => Some((*s, b)),
                Wire::Fec(_) => None,
            })
            .collect();
        let mut dec = FecDecoder::new(l, d);
        let mut handed_back = Vec::new();
        let mut fec_index = 0;
        for (i, w) in wire.iter().enumerate() {
            if i < start {
                if let Wire::Fec(_) = w {
                    fec_index += 1;
                }
                continue;
            }
            let out = match w {
                Wire::Media(seq, data) => {
                    if lost.contains(seq) {
                        continue;
                    }
                    let mut out = dec.process_media(*seq, 0, data);
                    handed_back.push((out.remove(0).sequence_number, false));
                    out
                }
                Wire::Fec(f) => {
                    fec_index += 1;
                    if drop_fec(fec_index - 1) {
                        continue;
                    }
                    dec.process_fec(f)
                }
            };
            for p in out {
                let original = sent
                    .get(&p.sequence_number)
                    .unwrap_or_else(|| panic!("rebuilt seq {} was never sent", p.sequence_number));
                assert_eq!(
                    &p.data[RTP_HEADER_LEN..],
                    &original[RTP_HEADER_LEN..],
                    "seq {} rebuilt with other bytes than were sent",
                    p.sequence_number,
                );
                handed_back.push((p.sequence_number, true));
            }
        }
        handed_back
    }

    /// A receiver that starts while its sender is already running (a node
    /// or flow restart, an upgrade, `add_input`), at every point of the
    /// wire and with no loss: everything it rebuilds is byte for byte what
    /// was sent under that number, and none of it is a packet it received.
    /// A parity received before the first media packet used to be applied
    /// to the first matrix the receiver opened, rebuilding packets from
    /// bytes never sent (edge#214).
    #[test]
    fn a_receiver_started_mid_stream_rebuilds_only_what_was_sent() {
        for (l, d) in [(4u8, 4u8), (5, 5), (8, 8), (10, 10)] {
            let base = 7_000u16;
            let n = 3 * l as u16 * d as u16;
            let seqs: Vec<u16> = (0..n).map(|i| base.wrapping_add(i)).collect();
            let wire = send(l, d, &seqs, |_| 20);
            for start in 0..wire.len() {
                let received: Vec<u16> = wire[start..]
                    .iter()
                    .filter_map(|w| match w {
                        Wire::Media(s, _) => Some(*s),
                        Wire::Fec(_) => None,
                    })
                    .collect();
                for seq in receive_from(l, d, &wire, start, &[], |_| false) {
                    assert!(
                        !received.contains(&seq),
                        "{l}x{d} from wire[{start}]: seq {seq} rebuilt a second time",
                    );
                }
            }
        }
    }

    /// A receiver that starts at media seq 5 of the first matrix (4x4) and
    /// loses seq 13 still rebuilds it, from the parities sent after it
    /// started. No FEC precedes its first media packet here (the first
    /// parity follows seq 12), so this pins only that the first matrix's
    /// later parities are kept, not repair after a discard: that is
    /// `a_receiver_started_at_a_parity_repairs_the_next_matrix`.
    #[test]
    fn a_receiver_started_mid_matrix_still_repairs_its_last_row() {
        let base = 9_000u16;
        let seqs: Vec<u16> = (0..48).map(|i| base + i).collect();
        let wire = send(4, 4, &seqs, |_| 20);
        let start = wire
            .iter()
            .position(|w| matches!(w, Wire::Media(s, _) if *s == base + 5))
            .expect("seq 5 was sent");
        let got = receive_from(4, 4, &wire, start, &[base + 13], |_| false);
        assert!(got.contains(&(base + 13)), "{got:?}");
    }

    /// The discard keeps repairing: a receiver whose first datagram is a
    /// parity (any of the first matrix's column or row FEC, so the decoder
    /// discards it and every parity up to the next media packet) still
    /// rebuilds a packet lost in the next matrix's last row from that
    /// matrix's column FEC.
    #[test]
    fn a_receiver_started_at_a_parity_repairs_the_next_matrix() {
        for (l, d) in [(4u8, 4u8), (5, 5)] {
            let base = 11_000u16;
            let m = l as u16 * d as u16;
            let seqs: Vec<u16> = (0..3 * m).map(|i| base + i).collect();
            let wire = send(l, d, &seqs, |_| 20);
            let next_matrix = wire
                .iter()
                .position(|w| matches!(w, Wire::Media(s, _) if *s == base + m))
                .expect("the second matrix was sent");
            // The second matrix's last row, column 1.
            let lost = base + m + (d as u16 - 1) * l as u16 + 1;
            let starts: Vec<usize> = (0..next_matrix)
                .filter(|&i| matches!(wire[i], Wire::Fec(_)))
                .collect();
            assert_eq!(starts.len(), l as usize + d as usize, "{l}x{d}: every column and row FEC");
            for start in starts {
                let got = receive_from(l, d, &wire, start, &[lost], |_| false);
                assert!(got.contains(&lost), "{l}x{d} from wire[{start}]: {got:?}");
            }
        }
    }

    /// Sequence numbers with an upstream gap of 3 inside the second
    /// matrix (what a 2022-7 output forwarding RTP sends when its upstream
    /// lost packets): both ends file every cell at seq - SNBase, so a loss
    /// in a whole column is rebuilt exactly and nothing is made up.
    #[test]
    fn encoder_and_decoder_agree_across_an_upstream_gap() {
        let seqs: Vec<u16> = (1000u16..1048).filter(|s| !(1018..=1020).contains(s)).collect();
        let wire = send(4, 4, &seqs, |_| 16);
        assert!(receive(4, 4, &wire, &[], |_| false).is_empty(), "no loss, no rebuild");
        // 1025: column 1 of the gapped matrix (1017/1021/1025/1029, all sent).
        // 1040: the third matrix.
        assert_eq!(receive(4, 4, &wire, &[1025, 1040], |_| false), vec![1025, 1040]);
        // 1022 shares column 2 and row 1 with the gap: nothing protects it.
        assert!(receive(4, 4, &wire, &[1022], |_| false).is_empty());
    }

    /// L=1: the encoder sends no row FEC, so a decoder that has closed a
    /// complete matrix never rebuilds one of its packets a second time.
    #[test]
    fn one_column_stream_is_never_duplicated() {
        let seqs: Vec<u16> = (2000u16..2012).collect();
        let wire = send(1, 4, &seqs, |_| 16);
        let mut dec = FecDecoder::new(1, 4);
        let mut published = 0;
        for w in &wire {
            published += match w {
                Wire::Media(seq, data) => dec.process_media(*seq, 0, data).len(),
                Wire::Fec(f) => dec.process_fec(f).len(),
            };
        }
        assert_eq!(published, seqs.len(), "every packet published exactly once");
        assert_eq!(receive(1, 4, &wire, &[2006], |_| false), vec![2006]);
    }

    /// Matrices larger than the 3-bit header index can name (10x10, the
    /// documented shape, and 20x20): column 8's parity is never filed in
    /// column 0's slot, so no rebuild is ever made from another group's
    /// parity, and a packet with neither a column nor a row FEC stays lost.
    #[test]
    fn matrices_past_eight_never_fabricate_a_packet() {
        for (l, d) in [(10u8, 10u8), (20, 20)] {
            let (lu, du) = (l as u16, d as u16);
            let base = 64_000u16;
            let seqs: Vec<u16> = (0..2 * lu * du).map(|i| base.wrapping_add(i)).collect();
            let wire = send(l, d, &seqs, |_| 24);
            let at = |m: u16, r: u16, c: u16| base.wrapping_add(m * lu * du + r * lu + c);
            assert!(receive(l, d, &wire, &[], |_| false).is_empty());
            // Two losses in column 0, rows 0 and 1: their rows rebuild them.
            let mut got = receive(l, d, &wire, &[at(1, 0, 0), at(1, 1, 0)], |_| false);
            got.sort_unstable();
            assert_eq!(got, vec![at(1, 0, 0), at(1, 1, 0)], "{l}x{d}");
            // One loss in column 0 and that column's FEC lost too: row 0.
            let col0 = |i: usize| i == (l.min(8) + d.min(8)) as usize; // matrix 1's first FEC
            assert_eq!(receive(l, d, &wire, &[at(1, 0, 0)], col0), vec![at(1, 0, 0)], "{l}x{d}");
            // Two losses in row 0: columns 0 and 1 rebuild them.
            let mut got = receive(l, d, &wire, &[at(1, 0, 0), at(1, 0, 1)], |_| false);
            got.sort_unstable();
            assert_eq!(got, vec![at(1, 0, 0), at(1, 0, 1)], "{l}x{d}");
            // Column 8, last row: no column FEC (index 8) and no row FEC
            // (row >= 8): stays lost, and nothing is made up for it.
            assert!(receive(l, d, &wire, &[at(1, du - 1, 8)], |_| false).is_empty(), "{l}x{d}");
            // Column 9, row 2: row 2's FEC rebuilds it.
            assert_eq!(receive(l, d, &wire, &[at(1, 2, 9)], |_| false), vec![at(1, 2, 9)]);
        }
    }

    /// A header-only datagram, mid-matrix and as a matrix's last cell: the
    /// decoder files nothing for it, so the encoder protects nothing that
    /// holds it (a matrix whose last cell is header-only sends no row FEC),
    /// nothing is made up, and the next matrix starts at the same number on
    /// both ends.
    #[test]
    fn header_only_datagrams_are_never_protected_or_fabricated() {
        let seqs: Vec<u16> = (3000u16..3032).collect();
        let len = |s: u16| if s == 3005 || s == 3015 { 0 } else { 16 };
        let wire = send(4, 4, &seqs, len);
        assert!(receive(4, 4, &wire, &[], |_| false).is_empty());
        // Column 1 holds 3005; row 0 has no FEC, the matrix's last cell
        // being header-only.
        assert!(receive(4, 4, &wire, &[3001], |_| false).is_empty());
        // Column 2 is whole.
        assert_eq!(receive(4, 4, &wire, &[3002], |_| false), vec![3002]);
        // The next matrix, at 3016 on both ends.
        assert_eq!(receive(4, 4, &wire, &[3016], |_| false), vec![3016]);
        assert_eq!(receive(4, 4, &wire, &[3031], |_| false), vec![3031]);
    }

    /// The sender steps back a whole matrix or more (an upstream restarting
    /// on an earlier number) and the first packet after the step is lost on
    /// the link: the decoder's FEC-driven rebase lands on the same matrix
    /// the encoder opened there, so the loss is rebuilt exactly. (3980 is
    /// 20 behind the open matrix's SNBase, 4000, at 4x4.)
    #[test]
    fn a_step_back_with_its_first_packet_lost_rebuilds_exactly() {
        let seqs: Vec<u16> = (4000u16..4011).chain(3980..4040).collect();
        let wire = send(4, 4, &seqs, |_| 16);
        assert!(receive(4, 4, &wire, &[], |_| false).is_empty());
        let got = receive(4, 4, &wire, &[3980], |_| false);
        assert!(got.contains(&3980), "{got:?}");
        assert!(got.iter().all(|s| *s == 3980), "{got:?}");
    }

    /// A step back of less than a matrix (4x4: 4000..=4010, then 3990 on,
    /// 10 behind SNBase) is late rather than a new matrix: 3990..=3999 go
    /// out unprotected and the matrix at 4000 is protected in full. The
    /// decoder moves its SNBase back onto 3998, the first of them fewer
    /// than L - 1 behind 4000, until column 0's FEC realigns it to 4000. No
    /// single loss rebuilds anything but the packet lost, exactly.
    #[test]
    fn a_step_back_of_less_than_a_matrix_is_late_on_both_ends() {
        let seqs: Vec<u16> = (4000u16..4011).chain(3990..4040).collect();
        let wire = send(4, 4, &seqs, |_| 16);
        assert!(receive(4, 4, &wire, &[], |_| false).is_empty());
        for lost in 3990u16..4040 {
            let got = receive(4, 4, &wire, &[lost], |_| false);
            assert!(got.iter().all(|s| *s == lost), "lost {lost}: {got:?}");
        }
        for lost in 3990u16..4000 {
            assert!(receive(4, 4, &wire, &[lost], |_| false).is_empty(), "{lost} unprotected");
        }
        assert_eq!(receive(4, 4, &wire, &[4013], |_| false), vec![4013]);
    }

    /// What the decoder rebuilds of `sent` (sequence number and datagram,
    /// in send order, a number possibly twice with other bytes, a datagram
    /// possibly header-only) through an L x D encoder, with the datagrams at
    /// the indices in `lost` lost. Asserts each rebuild is a datagram sent
    /// under its number.
    fn rebuilt_without(l: u8, d: u8, sent: &[(u16, Bytes)], lost: &[usize]) -> Vec<u16> {
        let mut enc = FecEncoder::new(l, d, Arc::new(AtomicU64::new(0)));
        let mut dec = FecDecoder::new(l, d);
        let mut rebuilt = Vec::new();
        for (i, (seq, dg)) in sent.iter().enumerate() {
            // A header-only datagram is not given to the encoder, as
            // `output_rtp::fec_for_datagram` does not.
            let fec = if dg.len() > RTP_HEADER_LEN {
                enc.process(*seq, &dg[RTP_HEADER_LEN..])
            } else {
                Vec::new()
            };
            let mut out = Vec::new();
            if !lost.contains(&i) {
                out = dec.process_media(*seq, 0, dg);
                out.remove(0);
            }
            for f in &fec {
                out.extend(dec.process_fec(f));
            }
            for p in out {
                assert!(
                    sent.iter().any(|(s, d)| *s == p.sequence_number
                        && d[RTP_HEADER_LEN..] == p.data[RTP_HEADER_LEN..]),
                    "seq {} rebuilt with bytes never sent under it (lost {lost:?})",
                    p.sequence_number,
                );
                rebuilt.push(p.sequence_number);
            }
        }
        rebuilt
    }

    /// `seqs` sent with payloads from `content`, so one number can be sent
    /// twice with other bytes.
    fn sent_as(seqs: impl IntoIterator<Item = (u16, u16)>) -> Vec<(u16, Bytes)> {
        seqs.into_iter()
            .map(|(s, content)| (s, make_rtp_packet(s, 0, &body(content, 16))))
            .collect()
    }

    /// The sender steps back a little, into the open matrix, with new bytes
    /// under numbers it already sent (L=2 D=4: 100..=105, then 102..=111
    /// changed) -- an upstream restart a 2022-7 output forwards unchanged.
    /// A receiver holds whichever copy of such a number reached it, so the
    /// encoder sends no FEC over those cells: whatever two datagrams the
    /// link loses, nothing rebuilt differs from a datagram sent under its
    /// number, and a loss the untouched rows cover is still rebuilt.
    #[test]
    fn a_number_resent_with_other_bytes_is_never_rebuilt_from_the_other_copy() {
        let sent = sent_as(
            (100u16..106).map(|s| (s, s)).chain((102u16..112).map(|s| (s, s ^ 0x5A5A))),
        );
        for i in 0..sent.len() {
            for j in i..sent.len() {
                rebuilt_without(2, 4, &sent, &[i, j]);
            }
        }
        // The first 102 and the (only) 106: row 3 (106/107, never re-sent)
        // still rebuilds 106.
        assert_eq!(rebuilt_without(2, 4, &sent, &[2, 10]), vec![106]);
    }

    /// A sender restarting a few numbers back with other bytes, from where
    /// the decoder's matrix cannot follow it exactly. Numbers then sent
    /// again with other bytes:
    /// - L=2 D=4, behind the open matrix, where its first packets are late
    ///   (edge#214): from the second matrix, behind where the first ended
    ///   (100..=111, then 105..=116); from the first, where the decoder's
    ///   SNBase can move back onto 99 though the encoder's stays at 100
    ///   (100..=105, then 97..=110); and from the second matrix after its
    ///   column 0 FEC has gone out (100..=115, then 104..=120) -- a
    ///   receiver that lost two of that column holds the FEC, and would
    ///   rebuild the second from it and the other's second copy, but drops
    ///   it at the first late packet.
    /// - Into a matrix the decoder has completed and closed, L=2 D=4
    ///   (100..=107, then 106..=113) and L=3 D=4 (100..=111, then
    ///   109..=120): the closed matrix used to file the second copies and
    ///   rebuild the row's last packet from them and the row FEC sent after
    ///   it, with no loss at all.
    /// - L=2 D=4 with 103 header-only and duplicated (100..=107, then
    ///   105..=112): the duplicate used to count twice, closing the matrix
    ///   at 106, which then filed 107 and the second copies.
    ///
    /// With no loss, and whatever two datagrams the link loses, nothing
    /// rebuilt differs from a datagram sent under its number.
    #[test]
    fn a_restart_is_never_rebuilt_from_the_other_copy() {
        let resent = |first: std::ops::Range<u16>, again: std::ops::Range<u16>| {
            sent_as(first.map(|s| (s, s)).chain(again.map(|s| (s, s ^ 0x5A5A))))
        };
        let mut header_only = resent(100..108, 105..113);
        let empty = make_rtp_packet(103, 0, &[]);
        header_only[3].1 = empty.clone();
        header_only.insert(4, (103, empty));
        let restarts = [
            (2u8, 4u8, resent(100..112, 105..117)),
            (2, 4, resent(100..106, 97..111)),
            (2, 4, resent(100..116, 104..121)),
            (2, 4, resent(100..108, 106..114)),
            (3, 4, resent(100..112, 109..121)),
            (2, 4, header_only),
        ];
        for (l, d, sent) in &restarts {
            rebuilt_without(*l, *d, sent, &[]);
            for i in 0..sent.len() {
                for j in i..sent.len() {
                    rebuilt_without(*l, *d, sent, &[i, j]);
                }
            }
        }
    }

    /// A FEC packet for one SNBase as the bilbycast header frames it.
    fn fec_packet(sn_base: u16, is_column: bool, index: u8, payload: &[u8]) -> Vec<u8> {
        let mut pkt = FEC_MAGIC.to_vec();
        pkt.extend_from_slice(&sn_base.to_be_bytes());
        pkt.extend_from_slice(&[0; 6]);
        pkt.push(if is_column { 0x40 } else { 0 } | index);
        pkt.push(if is_column { 2 } else { 1 });
        pkt.extend_from_slice(payload);
        pkt
    }

    /// A FEC packet naming another SNBase inside the window realigns the
    /// matrix to it, and the FEC held for the old SNBase goes: it was
    /// computed over other groups. L=2 D=4: 100, 101, 104 and 105 received,
    /// row 1's FEC under SNBase 100 held (102 and 103 both missing). A
    /// column FEC under 101 moves the matrix one on, where row 1 is 103 and
    /// 104; kept, the held parity rebuilt "103" from row 1 of SNBase 100
    /// and 104.
    #[test]
    fn fec_held_for_another_snbase_is_dropped_when_the_matrix_realigns() {
        let mut dec = FecDecoder::new(2, 4);
        for seq in [100u16, 101, 104, 105] {
            dec.process_media(seq, 0, &make_rtp_packet(seq, 0, &body(seq, 16)));
        }
        assert!(dec.process_fec(&fec_packet(100, false, 1, &[0xAA; 16])).is_empty());
        let out = dec.process_fec(&fec_packet(101, true, 1, &[0x55; 16]));
        let rebuilt: Vec<u16> = out.iter().map(|p| p.sequence_number).collect();
        assert!(rebuilt.is_empty(), "rebuilt from FEC of SNBase 100: {rebuilt:?}");
    }

    /// A header-only datagram files no cell, so it counts nothing towards
    /// filling the matrix, however often it comes. L=2 D=4, 103 header-only
    /// and duplicated, 101 lost: only row 0's FEC, sent after 107, protects
    /// 101 (103 leaves column 1 without FEC). The duplicate used to count
    /// 103 twice, so the matrix closed as full at 107 with 101 missing,
    /// before that FEC came.
    #[test]
    fn a_repeated_header_only_datagram_never_closes_the_matrix_early() {
        let seqs = [100u16, 101, 102, 103, 103, 104, 105, 106, 107];
        let wire = send(2, 4, &seqs, |s| if s == 103 { 0 } else { 16 });
        assert_eq!(receive(2, 4, &wire, &[101], |_| false), vec![101]);
    }

    /// A datagram the matrix leaves out is not the template a rebuilt
    /// packet's header is taken from: a late one carries an old timestamp.
    /// L=2 D=4, timestamps 10 a number: the second matrix loses 114, and
    /// the late 105 arrives just before column 0's FEC rebuilds it. 114
    /// carries 113's timestamp, the newest filed, not 105's.
    #[test]
    fn a_datagram_left_out_of_the_matrix_sets_no_header_template() {
        let mut enc = FecEncoder::new(2, 4, Arc::new(AtomicU64::new(0)));
        let mut dec = FecDecoder::new(2, 4);
        let ts = |s: u16| u32::from(s) * 10;
        for s in 100u16..114 {
            let fec = enc.process(s, &body(s, 16));
            dec.process_media(s, ts(s), &make_rtp_packet(s, ts(s), &body(s, 16)));
            for f in fec {
                dec.process_fec(&f);
            }
        }
        let column_0 = enc.process(114, &body(114, 16));
        dec.process_media(105, ts(105), &make_rtp_packet(105, ts(105), &body(105, 16)));
        let rebuilt: Vec<RtpPacket> = column_0.iter().flat_map(|f| dec.process_fec(f)).collect();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].sequence_number, 114);
        assert_eq!(rebuilt[0].rtp_timestamp, ts(113));
        assert_eq!(&rebuilt[0].data[4..8], &ts(113).to_be_bytes());
    }

    // ── edge#214: packets behind the open matrix ───────────────────────

    /// `wire` with the media datagram `seq` moved to just after the media
    /// datagram `after`; every FEC datagram stays where it was sent.
    fn delay(wire: &mut Vec<Wire>, seq: u16, after: u16) {
        let media = |n: u16| move |w: &Wire| matches!(w, Wire::Media(s, _) if *s == n);
        let late = wire.remove(wire.iter().position(media(seq)).expect("seq was sent"));
        let at = wire.iter().position(media(after)).expect("after was sent");
        wire.insert(at + 1, late);
    }

    /// Where `seq` is first handed back in a [`replay`].
    fn handed_at(out: &[(u16, bool)], seq: u16) -> usize {
        out.iter().position(|(s, _)| *s == seq).unwrap_or_else(|| panic!("{seq} never handed back"))
    }

    fn rebuilt_of(out: &[(u16, bool)]) -> Vec<u16> {
        out.iter().filter_map(|(s, rebuilt)| rebuilt.then_some(*s)).collect()
    }

    /// The first reproduction in edge#214, 4x4: 13 (the first matrix's last
    /// row) arrives only after 17, and 28 (the second matrix's last row) is
    /// lost. Column 1's FEC, sent after 13, rebuilds it before 14. The late
    /// original is behind where the first matrix ended, so it leaves the
    /// open one alone, and column 0's FEC rebuilds 28 as it arrives, before
    /// 29 -- what a single-leg input can publish. The late 13 used to start
    /// a new matrix at 13, dropping 16 and 17: 28 waited for its row's FEC
    /// at the matrix's end, and 16 and 17 were rebuilt though received.
    #[test]
    fn a_late_packet_from_the_previous_matrix_leaves_the_open_one_alone() {
        let seqs: Vec<u16> = (0..48).collect();
        let mut wire = send(4, 4, &seqs, |_| 20);
        delay(&mut wire, 13, 17);
        let out = replay(4, 4, &wire, 0, &[28], |_| false);
        assert_eq!(rebuilt_of(&out), vec![13, 28], "{out:?}");
        assert!(handed_at(&out, 28) < handed_at(&out, 29), "{out:?}");
    }

    /// The second reproduction, 4x4: 15 arrives after 16, behind the FEC
    /// sent after it, with 28 lost. Column 3's FEC rebuilds 15 before 16
    /// arrives; the late 15 is behind where its matrix ended and leaves the
    /// next alone, so 28 is rebuilt before 29.
    #[test]
    fn a_reorder_across_a_matrix_boundary_leaves_the_next_matrix_alone() {
        let seqs: Vec<u16> = (0..48).collect();
        let mut wire = send(4, 4, &seqs, |_| 20);
        delay(&mut wire, 15, 16);
        let out = replay(4, 4, &wire, 0, &[28], |_| false);
        assert_eq!(rebuilt_of(&out), vec![15, 28], "{out:?}");
        assert!(handed_at(&out, 28) < handed_at(&out, 29), "{out:?}");
    }

    /// A matrix's own first packet arriving after its second (4x4: 16
    /// after 17, with 28 and 29 lost) moves SNBase back onto itself, so the
    /// matrix keeps both and columns 0 and 1 rebuild 28 and 29 before 30.
    /// It used to start a new matrix at 16, dropping 17: 29 waited for row
    /// 0's FEC to rebuild 17 first, and 17 was rebuilt though received.
    #[test]
    fn a_late_first_packet_moves_its_matrix_back() {
        let seqs: Vec<u16> = (0..48).collect();
        let mut wire = send(4, 4, &seqs, |_| 20);
        delay(&mut wire, 16, 17);
        let out = replay(4, 4, &wire, 0, &[28, 29], |_| false);
        assert_eq!(rebuilt_of(&out), vec![28, 29], "{out:?}");
        assert!(handed_at(&out, 29) < handed_at(&out, 30), "{out:?}");
    }

    /// Reordering, on the link or upstream of the encoder (a 2022-7 output
    /// forwards RTP in the order it came), never rebuilds bytes that were
    /// not sent: every packet of four matrices, delayed by 1, 2, L, L*D-1,
    /// L*D and L*D+1 datagrams, with and without a loss, across the
    /// sequence-number wrap.
    #[test]
    fn reordered_packets_never_rebuild_bytes_that_were_not_sent() {
        for (l, d) in [(4u8, 4u8), (5, 5), (2, 4), (1, 4)] {
            let m = l as u16 * d as u16;
            let n = 4 * m;
            let base = 65_500u16;
            let seq = |i: u16| base.wrapping_add(i);
            let in_order: Vec<u16> = (0..n).map(seq).collect();
            for i in 0..n {
                for k in [1, 2, l as u16, m - 1, m, m + 1] {
                    if i + k >= n {
                        continue;
                    }
                    let lost = seq((i * 7 + k * 3) % n);
                    let mut link = send(l, d, &in_order, |_| 20);
                    delay(&mut link, seq(i), seq(i + k));
                    let mut upstream_order = in_order.clone();
                    let late = upstream_order.remove(i as usize);
                    upstream_order.insert((i + k) as usize, late);
                    let upstream = send(l, d, &upstream_order, |_| 20);
                    for wire in [&link, &upstream] {
                        receive(l, d, wire, &[], |_| false);
                        receive(l, d, wire, &[lost], |_| false);
                    }
                }
            }
        }
    }

    /// `holds_payload` tells a datagram the matrix already holds -- received
    /// before, or rebuilt -- from a new one, and from one with other bytes.
    #[test]
    fn holds_payload_tells_a_held_datagram() {
        let mut dec = FecDecoder::new(2, 4);
        let a = make_rtp_packet(10, 0, &[1, 2, 3, 4]);
        assert!(!dec.holds_payload(10, &a), "nothing held before the first media");
        dec.process_media(10, 0, &a);
        assert!(dec.holds_payload(10, &a));
        assert!(!dec.holds_payload(10, &make_rtp_packet(10, 0, &[9, 9, 9, 9])));
        assert!(!dec.holds_payload(11, &make_rtp_packet(11, 0, &[1, 2, 3, 4])));
        assert!(!dec.holds_payload(10 + 8, &make_rtp_packet(18, 0, &[1, 2, 3, 4])), "outside the window");
        assert!(!dec.holds_payload(10, &a[..RTP_HEADER_LEN]), "header only");
    }
}
