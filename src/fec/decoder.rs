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
//! ## Limitations
//!
//! - The bilbycast FEC header carries no `TS Recovery` value (it is zeroed
//!   by the encoder). Recovered packets reuse the most recent media
//!   timestamp as a best-effort value.
//! - Recovery is disabled while the matrix is uninitialised, so the first
//!   cycle after startup can drop losses without repair.
use bytes::Bytes;

use crate::engine::packet::RtpPacket;
use crate::util::time::now_us;

use super::matrix::FecMatrix;
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

    /// Process an incoming RTP media packet.
    ///
    /// Places the packet into the FEC matrix, captures a header template for
    /// later recovery, then attempts to repair any protected group that now
    /// has exactly one missing cell. Returns the originally received packet
    /// followed by every packet that was reconstructed as a side effect.
    pub fn process_media(&mut self, seq: u16, ts: u32, data: &Bytes) -> Vec<RtpPacket> {
        if !self.initialized {
            self.base_seq = seq;
            self.initialized = true;
        }

        // Capture the latest RTP header so recovered packets can reuse it.
        // Only the first 12 bytes are kept — CSRC identifiers / header
        // extensions are not preserved, which matches the simplified 2022-1
        // semantics we emit on the encoder side.
        if data.len() >= RTP_HEADER_LEN {
            let mut hdr = [0u8; RTP_HEADER_LEN];
            hdr.copy_from_slice(&data[..RTP_HEADER_LEN]);
            self.last_rtp_header = Some(hdr);
            self.last_rtp_timestamp = ts;
        }

        let diff = seq.wrapping_sub(self.base_seq);
        if diff >= self.matrix_size {
            self.reset_cycle();
            self.base_seq = seq;
            self.initialized = true;
        }

        let position = seq.wrapping_sub(self.base_seq) as usize;
        if position < self.matrix.size() && !self.matrix.has_cell(position) {
            if data.len() > RTP_HEADER_LEN {
                self.matrix.insert_media(position, &data[RTP_HEADER_LEN..]);
            }
            self.media_count += 1;
        }

        let mut result = Vec::with_capacity(1);
        result.push(RtpPacket {
            data: data.clone(),
            sequence_number: seq,
            rtp_timestamp: ts,
            recv_time_us: now_us(),
            is_raw_ts: false,
        });

        self.attempt_recovery(&mut result);

        if self.media_count >= self.matrix_size {
            self.reset_cycle();
        }

        result
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

        // If we have never seen media yet, buffer the FEC payload but skip
        // any recovery attempt — there is no template RTP header to
        // reconstruct around.
        if !self.initialized {
            self.store_fec(&hdr, payload);
            return result;
        }

        // The FEC header's `sn_base` is authoritative. When the first
        // media packet of a group was lost, `self.base_seq` was set to a
        // later sequence and every cell sits at a shifted position.
        // Realign the matrix against `sn_base` before storing this FEC.
        if hdr.sn_base != self.base_seq {
            if !self.align_base_to(hdr.sn_base) {
                return result;
            }
        }

        self.store_fec(&hdr, payload);
        self.attempt_recovery(&mut result);
        result
    }

    /// Reseat the matrix so `base_seq == new_base`, preserving already-
    /// received media cells by re-inserting them at their correct
    /// positions relative to the new base. Returns `false` if the shift
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
        // Pending FEC payloads are addressed by column/row index relative
        // to the matrix layout and survive the rebase; media cells need
        // re-slotting only.
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
                {
                    if let Some(pkt) = self.recover_column(c, &template) {
                        out.push(pkt);
                        progressed = true;
                    }
                }
            }

            for r in 0..self.rows {
                if self.row_fec[r as usize].is_some()
                    && self.matrix.row_count(r) == self.columns.saturating_sub(1)
                {
                    if let Some(pkt) = self.recover_row(r, &template) {
                        out.push(pkt);
                        progressed = true;
                    }
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
        }
    }

    fn reset_cycle(&mut self) {
        self.matrix.reset();
        self.media_count = 0;
        for slot in &mut self.col_fec {
            *slot = None;
        }
        for slot in &mut self.row_fec {
            *slot = None;
        }
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

    #[test]
    fn fec_with_no_prior_media_is_buffered_not_panicking() {
        let mut dec = FecDecoder::new(2, 2);
        // Build a synthetic FEC packet (column 0) with arbitrary payload.
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&FEC_MAGIC);
        pkt.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0x40, 2]);
        pkt.extend_from_slice(&[0xAA; 4]);
        let out = dec.process_fec(&pkt);
        assert!(out.is_empty());
    }

    #[test]
    fn fec_with_garbage_is_ignored() {
        let mut dec = FecDecoder::new(2, 2);
        assert!(dec.process_fec(&[0, 1, 2]).is_empty());
        assert!(dec.process_fec(&[0; 32]).is_empty()); // wrong magic
    }
}
