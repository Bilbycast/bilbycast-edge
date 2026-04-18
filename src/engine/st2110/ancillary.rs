// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

// Phase 1 step 4: SMPTE ST 2110-40 ancillary data packetizer/depacketizer
// per RFC 8331. SCTE-104, SMPTE 12M, and CEA-608/708 decoders are scoped
// for follow-up commits in the same step.
#![allow(dead_code)]

//! SMPTE ST 2110-40 ancillary data packetizer and depacketizer.
//!
//! ST 2110-40 carries arbitrary ancillary data packets (ANC) over RTP using
//! the `smpte291` payload format defined in RFC 8331. Each RTP packet
//! contains:
//!
//! ```text
//! +-- 12 bytes RTP header (PT=dynamic, marker bit gates frame end) --+
//! +-- 8 bytes ANC payload header --+
//! |  ExtSeqNum (16) | Length (16) | ANC_Count (8) | F (2) | reserved(22)|
//! +--+ for each ANC packet:
//! |  C (1) | Line_Number (11) | Horizontal_Offset (12) | S (1) |
//! |  StreamNum (7) | DID (10) | SDID (10) | Data_Count (10) |
//! |  UDW[0..N] (10 bits each) | checksum (10) | padding to 32-bit boundary
//! +--+
//! ```
//!
//! Field semantics:
//!
//! - **ExtSeqNum**: 16-bit extension to the RTP sequence number for sources
//!   that emit more than 65k ANC packets per second.
//! - **Length**: total octet count of the ANC payload (everything after
//!   `Length`).
//! - **ANC_Count**: number of ANC sub-packets in this RTP packet.
//! - **F**: field flag (0 = progressive / unspecified, 1 = field 1, 2 =
//!   field 2, 3 = invalid).
//! - **C**: color difference flag.
//! - **Line_Number**: 11-bit line number (0..=2047).
//! - **Horizontal_Offset**: 12-bit horizontal position (0..=4095).
//! - **S**: data stream flag.
//! - **StreamNum**: 7-bit data stream number.
//! - **DID/SDID**: 10-bit data identifier and secondary data identifier
//!   with even parity in the upper bit and a parity-check bit in the next.
//!   Bilbycast stores them as the lower 8 bits and recomputes parity on
//!   transmit.
//! - **Data_Count**: 10-bit user data word count.
//! - **UDW**: data words, each 10 bits with parity in the upper two bits.
//!
//! Phase 1 step 4 ships:
//!
//! - [`AncPacket`]: the broadcast-domain representation of one ANC packet
//!   (DID, SDID, line number, horizontal offset, raw 8-bit user data).
//! - [`pack_ancillary`]: build a single RFC 8331 RTP payload (without the
//!   12-byte RTP header) from a set of [`AncPacket`]s.
//! - [`unpack_ancillary`]: parse an RFC 8331 RTP payload back into a list
//!   of [`AncPacket`]s.
//!
//! The wrapping RTP header (sequence number, timestamp, SSRC) is added by
//! the input/output tasks (Phase 1 step 4 part 2) using the same helpers as
//! the audio path.
//!
//! ## Pure Rust
//!
//! No bit-vec dependency. Bit packing/unpacking is hand-rolled with a
//! tiny [`BitWriter`] / [`BitReader`] pair scoped to this module.

/// One ancillary data packet in its bilbycast in-memory form.
///
/// All multi-bit fields use the host-friendly bit widths from RFC 8331; the
/// 10-bit ANC user-data words are stored as plain `u8` since the parity bits
/// are deterministically derived from the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AncPacket {
    /// Color difference flag (0 = luma / general, 1 = chroma).
    pub color_difference: bool,
    /// 11-bit line number (0..=2047).
    pub line_number: u16,
    /// 12-bit horizontal offset (0..=4095).
    pub horizontal_offset: u16,
    /// Data stream flag (true when StreamNum is meaningful).
    pub stream_flag: bool,
    /// 7-bit stream number (0..=127).
    pub stream_num: u8,
    /// Data identifier (8-bit user value; the 10-bit on-wire form gets the
    /// upper two parity bits added/stripped automatically).
    pub did: u8,
    /// Secondary data identifier (8-bit user value).
    pub sdid: u8,
    /// User data words (8-bit values; on-wire form is 10 bits per word with
    /// parity in the upper two bits).
    pub user_data: Vec<u8>,
}

impl AncPacket {
    /// Convenience constructor for the common case (no color-difference,
    /// stream-flag clear, stream 0).
    pub fn simple(did: u8, sdid: u8, line_number: u16, user_data: Vec<u8>) -> Self {
        Self {
            color_difference: false,
            line_number,
            horizontal_offset: 0,
            stream_flag: false,
            stream_num: 0,
            did,
            sdid,
            user_data,
        }
    }
}

/// Field identifier for the RFC 8331 `F` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncField {
    /// 0 = progressive / no field association.
    Progressive,
    /// 1 = field 1 of an interlaced source.
    Field1,
    /// 2 = field 2 of an interlaced source.
    Field2,
}

impl AncField {
    fn as_bits(self) -> u8 {
        match self {
            AncField::Progressive => 0,
            AncField::Field1 => 1,
            AncField::Field2 => 2,
        }
    }
    fn from_bits(b: u8) -> Self {
        match b {
            1 => AncField::Field1,
            2 => AncField::Field2,
            _ => AncField::Progressive,
        }
    }
}

/// Errors returned by [`unpack_ancillary`].
#[derive(Debug, PartialEq, Eq)]
pub enum AncDecodeError {
    /// Buffer is shorter than the 8-byte ANC payload header.
    TooShortHeader,
    /// `Length` field exceeds buffer size.
    LengthOverflow,
    /// `ANC_Count` does not match the actual number of decoded packets.
    AncCountMismatch { declared: usize, actual: usize },
    /// User data word count would read past end of buffer.
    DataCountOverflow,
}

impl std::fmt::Display for AncDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AncDecodeError::TooShortHeader => f.write_str("ANC payload too short for header"),
            AncDecodeError::LengthOverflow => f.write_str("ANC Length field exceeds buffer"),
            AncDecodeError::AncCountMismatch { declared, actual } => {
                write!(f, "ANC_Count mismatch: declared {declared}, decoded {actual}")
            }
            AncDecodeError::DataCountOverflow => {
                f.write_str("ANC Data_Count would read past end of buffer")
            }
        }
    }
}

impl std::error::Error for AncDecodeError {}

/// Build an RFC 8331 ANC RTP payload (the bytes that go after the 12-byte
/// RTP header) from a list of ancillary packets.
///
/// `ext_seq_num` is the upper 16 bits of an extended sequence number; pass
/// `0` for the common case.
///
/// `field` indicates which field of an interlaced source the ANC packets
/// belong to; pass `AncField::Progressive` for progressive sources.
pub fn pack_ancillary(
    packets: &[AncPacket],
    ext_seq_num: u16,
    field: AncField,
) -> Vec<u8> {
    let mut bw = BitWriter::default();

    for p in packets {
        // C (1) | Line_Number (11) | Horizontal_Offset (12) | S (1) | StreamNum (7)
        bw.put(p.color_difference as u32, 1);
        bw.put(p.line_number as u32, 11);
        bw.put(p.horizontal_offset as u32, 12);
        bw.put(p.stream_flag as u32, 1);
        bw.put(p.stream_num as u32, 7);
        // DID (10), SDID (10), Data_Count (10)
        bw.put(parity10(p.did) as u32, 10);
        bw.put(parity10(p.sdid) as u32, 10);
        bw.put(parity10(p.user_data.len() as u8) as u32, 10);
        // UDW
        for udw in &p.user_data {
            bw.put(parity10(*udw) as u32, 10);
        }
        // Checksum (10): per RFC 8331 §2.2 / SMPTE ST 291, the checksum is
        // the 9-bit sum of DID, SDID, Data_Count, and the UDWs (modulo 512),
        // with bit 9 set to the inverse of bit 8.
        let cs = compute_checksum(p);
        bw.put(cs as u32, 10);
        // Padding to 32-bit boundary (as required by RFC 8331 §2.1).
        bw.align(32);
    }

    let body = bw.into_bytes();
    let length = body.len() as u16;
    let mut out = Vec::with_capacity(8 + body.len());
    // Header (8 bytes)
    out.extend_from_slice(&ext_seq_num.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.push(packets.len() as u8);
    // F (2 bits) | reserved (22 bits)
    let f_byte = (field.as_bits() & 0b11) << 6;
    out.push(f_byte);
    out.push(0);
    out.push(0);
    out.extend_from_slice(&body);
    out
}

/// Parse an RFC 8331 ANC RTP payload (without the leading 12-byte RTP
/// header) into a list of ancillary packets.
pub fn unpack_ancillary(buf: &[u8]) -> Result<Vec<AncPacket>, AncDecodeError> {
    if buf.len() < 8 {
        return Err(AncDecodeError::TooShortHeader);
    }
    let _ext_seq_num = u16::from_be_bytes([buf[0], buf[1]]);
    let length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let anc_count = buf[4] as usize;
    let _f = AncField::from_bits((buf[5] >> 6) & 0b11);

    if 8 + length > buf.len() && length != 0 {
        return Err(AncDecodeError::LengthOverflow);
    }
    let body_end = if length == 0 { buf.len() } else { 8 + length };
    let body = &buf[8..body_end];
    let mut br = BitReader::new(body);
    let mut out = Vec::with_capacity(anc_count);

    while out.len() < anc_count && br.bits_remaining() >= 62 {
        let color_difference = br.get(1) != 0;
        let line_number = br.get(11) as u16;
        let horizontal_offset = br.get(12) as u16;
        let stream_flag = br.get(1) != 0;
        let stream_num = br.get(7) as u8;
        let did = strip_parity10(br.get(10) as u16);
        let sdid = strip_parity10(br.get(10) as u16);
        let data_count = strip_parity10(br.get(10) as u16) as usize;

        if br.bits_remaining() < (data_count + 1) * 10 {
            return Err(AncDecodeError::DataCountOverflow);
        }

        let mut user_data = Vec::with_capacity(data_count);
        for _ in 0..data_count {
            user_data.push(strip_parity10(br.get(10) as u16));
        }
        let _checksum = br.get(10);

        out.push(AncPacket {
            color_difference,
            line_number,
            horizontal_offset,
            stream_flag,
            stream_num,
            did,
            sdid,
            user_data,
        });
        br.align(32);
    }

    if out.len() != anc_count {
        return Err(AncDecodeError::AncCountMismatch {
            declared: anc_count,
            actual: out.len(),
        });
    }
    Ok(out)
}

/// Compute the SMPTE ST 291 checksum for an [`AncPacket`].
///
/// The checksum is the 9-bit sum of DID, SDID, Data_Count, and all UDWs
/// (modulo 512). Bit 9 of the resulting 10-bit field is the inverse of
/// bit 8 to ensure the high bit follows even parity.
fn compute_checksum(p: &AncPacket) -> u16 {
    let mut sum: u16 = 0;
    sum = sum.wrapping_add(parity10(p.did));
    sum = sum.wrapping_add(parity10(p.sdid));
    sum = sum.wrapping_add(parity10(p.user_data.len() as u8));
    for udw in &p.user_data {
        sum = sum.wrapping_add(parity10(*udw));
    }
    let nine_bit = sum & 0x1FF;
    let bit8 = (nine_bit >> 8) & 1;
    let bit9 = bit8 ^ 1;
    nine_bit | (bit9 << 9)
}

/// Add the SMPTE ST 291 even-parity bits to an 8-bit user value, producing
/// the 10-bit on-wire form.
///
/// Per RFC 8331: bit 8 is the even-parity bit of bits 0..=7. Bit 9 is the
/// inverse of bit 8.
fn parity10(byte: u8) -> u16 {
    let parity = (byte.count_ones() & 1) as u16;
    let bit8 = parity;
    let bit9 = bit8 ^ 1;
    (byte as u16) | (bit8 << 8) | (bit9 << 9)
}

/// Drop the parity bits from a 10-bit on-wire word, returning the 8-bit
/// user value.
fn strip_parity10(word: u16) -> u8 {
    (word & 0xFF) as u8
}

/// Bit-level writer used by `pack_ancillary`. Lives in this module so we
/// don't take a `bitvec` dependency.
#[derive(Debug, Default)]
struct BitWriter {
    buf: Vec<u8>,
    /// Number of bits already written into the last byte (0..=8).
    bits_in_last: u8,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: u8) {
        debug_assert!(n <= 32);
        for i in (0..n).rev() {
            let bit = ((value >> i) & 1) as u8;
            if self.bits_in_last == 0 || self.bits_in_last == 8 {
                self.buf.push(0);
                self.bits_in_last = 0;
            }
            let last = self.buf.last_mut().unwrap();
            // MSB first: shift the bit into position 7 - bits_in_last.
            *last |= bit << (7 - self.bits_in_last);
            self.bits_in_last += 1;
        }
    }
    /// Pad with zero bits until the bit position is a multiple of `align` bits.
    fn align(&mut self, align: u32) {
        let total_bits = self.buf.len() as u32 * 8 - (8 - self.bits_in_last as u32) % 8;
        let pad = (align - (total_bits % align)) % align;
        for _ in 0..pad {
            self.put(0, 1);
        }
    }
    fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[derive(Debug)]
struct BitReader<'a> {
    buf: &'a [u8],
    bit_offset: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, bit_offset: 0 }
    }
    fn bits_remaining(&self) -> usize {
        self.buf.len() * 8 - self.bit_offset
    }
    fn get(&mut self, n: u8) -> u32 {
        let mut value: u32 = 0;
        for _ in 0..n {
            let byte = self.buf[self.bit_offset / 8];
            let bit = (byte >> (7 - (self.bit_offset % 8))) & 1;
            value = (value << 1) | bit as u32;
            self.bit_offset += 1;
        }
        value
    }
    fn align(&mut self, align: usize) {
        let remainder = self.bit_offset % align;
        if remainder != 0 {
            self.bit_offset += align - remainder;
        }
        if self.bit_offset > self.buf.len() * 8 {
            self.bit_offset = self.buf.len() * 8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_packet(did: u8, sdid: u8, n: usize) -> AncPacket {
        AncPacket::simple(did, sdid, 9, (0..n as u8).collect())
    }

    #[test]
    fn test_parity10_even_bytes() {
        // 0x00 has zero set bits → parity 0 → bit8=0, bit9=1.
        // The 10-bit on-wire word is therefore (1 << 9) | 0 = 0x200.
        assert_eq!(parity10(0x00), 0x200);
        // 0xFF has eight set bits → parity 0 → same shape (bit8=0, bit9=1)
        // with the low byte = 0xFF, giving (1 << 9) | 0xFF = 0x2FF.
        assert_eq!(parity10(0xFF), 0x2FF);
    }

    #[test]
    fn test_parity10_odd_bytes() {
        // 0x01 has one set bit → parity 1 → bit8=1, bit9=0.
        let w = parity10(0x01);
        assert_eq!(w & 0xFF, 0x01);
        assert_eq!((w >> 8) & 1, 1);
        assert_eq!((w >> 9) & 1, 0);
    }

    #[test]
    fn test_strip_parity10() {
        for b in 0u8..=255 {
            assert_eq!(strip_parity10(parity10(b)), b);
        }
    }

    #[test]
    fn test_pack_then_unpack_single_packet() {
        let packets = vec![sample_packet(0x41, 0x07, 4)]; // SCTE-104 DID/SDID
        let payload = pack_ancillary(&packets, 0, AncField::Progressive);
        let parsed = unpack_ancillary(&payload).expect("unpack");
        assert_eq!(parsed, packets);
    }

    #[test]
    fn test_pack_then_unpack_multiple_packets() {
        let packets = vec![
            sample_packet(0x41, 0x07, 8),
            sample_packet(0x60, 0x60, 8), // SMPTE 12M timecode DID/SDID
            sample_packet(0x61, 0x01, 16), // CEA-708 captions
        ];
        let payload = pack_ancillary(&packets, 0, AncField::Field1);
        let parsed = unpack_ancillary(&payload).expect("unpack");
        assert_eq!(parsed, packets);
    }

    #[test]
    fn test_pack_then_unpack_empty() {
        let payload = pack_ancillary(&[], 0, AncField::Progressive);
        let parsed = unpack_ancillary(&payload).expect("unpack empty");
        assert!(parsed.is_empty());
    }

    #[test]
    fn test_pack_payload_length_in_header() {
        let packets = vec![sample_packet(0x41, 0x07, 4)];
        let payload = pack_ancillary(&packets, 0, AncField::Progressive);
        let length = u16::from_be_bytes([payload[2], payload[3]]) as usize;
        assert_eq!(8 + length, payload.len());
        assert_eq!(payload[4] as usize, packets.len());
    }

    #[test]
    fn test_pack_field_flag_round_trips_field2() {
        let packets = vec![sample_packet(0x41, 0x07, 1)];
        let payload = pack_ancillary(&packets, 0, AncField::Field2);
        // F is the top two bits of byte 5.
        assert_eq!((payload[5] >> 6) & 0b11, 2);
    }

    #[test]
    fn test_pack_ext_seq_num_round_trips() {
        let packets = vec![sample_packet(0x41, 0x07, 1)];
        let payload = pack_ancillary(&packets, 0xCAFE, AncField::Progressive);
        let ext = u16::from_be_bytes([payload[0], payload[1]]);
        assert_eq!(ext, 0xCAFE);
    }

    #[test]
    fn test_unpack_rejects_short_header() {
        assert_eq!(unpack_ancillary(&[0u8; 4]), Err(AncDecodeError::TooShortHeader));
    }

    #[test]
    fn test_unpack_rejects_overflow_length() {
        // Header claims 1000 bytes of payload but buffer is only 8 bytes total.
        let mut buf = vec![0u8; 8];
        buf[2..4].copy_from_slice(&1000u16.to_be_bytes());
        buf[4] = 1; // anc_count = 1
        assert_eq!(unpack_ancillary(&buf), Err(AncDecodeError::LengthOverflow));
    }

    #[test]
    fn test_round_trip_with_color_difference_and_stream_num() {
        let packet = AncPacket {
            color_difference: true,
            line_number: 1234,
            horizontal_offset: 800,
            stream_flag: true,
            stream_num: 0x42,
            did: 0x52,
            sdid: 0x01,
            user_data: vec![0xCA, 0xFE, 0xBA, 0xBE],
        };
        let payload = pack_ancillary(&[packet.clone()], 0, AncField::Progressive);
        let parsed = unpack_ancillary(&payload).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], packet);
    }

    #[test]
    fn test_compute_checksum_consistency() {
        let p = sample_packet(0x41, 0x07, 8);
        let cs1 = compute_checksum(&p);
        let cs2 = compute_checksum(&p);
        assert_eq!(cs1, cs2);
        // bit9 must be inverse of bit8.
        let bit8 = (cs1 >> 8) & 1;
        let bit9 = (cs1 >> 9) & 1;
        assert_ne!(bit8, bit9);
    }

    #[test]
    fn test_bitwriter_basic() {
        let mut bw = BitWriter::default();
        bw.put(0b101, 3);
        bw.put(0b1100_0011, 8);
        bw.align(32);
        let bytes = bw.into_bytes();
        // First 11 bits of the stream are 101_1100_0011, packed MSB-first.
        assert_eq!(bytes[0], 0b1011_1000);
        assert_eq!(bytes[1] & 0b1110_0000, 0b0110_0000);
    }

    #[test]
    fn test_bitreader_basic() {
        let buf = [0b1011_1000, 0b0110_0000];
        let mut br = BitReader::new(&buf);
        assert_eq!(br.get(3), 0b101);
        assert_eq!(br.get(8), 0b1100_0011);
    }

    #[test]
    fn test_bitreader_writer_round_trip_random_lengths() {
        let mut bw = BitWriter::default();
        let values: &[(u32, u8)] = &[
            (0xa, 4),
            (0x1b, 5),
            (0x2c3, 10),
            (0x1, 1),
            (0xff, 8),
            (0x12345, 17),
        ];
        for (v, n) in values {
            bw.put(*v, *n);
        }
        let bytes = bw.into_bytes();
        let mut br = BitReader::new(&bytes);
        for (v, n) in values {
            assert_eq!(br.get(*n), *v);
        }
    }
}
