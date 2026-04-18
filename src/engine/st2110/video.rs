// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(dead_code)]

//! SMPTE ST 2110-20 (uncompressed video) and ST 2110-23 (single essence
//! across multiple -20 sub-streams) packetizer and depacketizer.
//!
//! Implements RFC 4175 "RTP Payload Format for Uncompressed Video" for the
//! subset of pgroup formats required by ST 2110-20 Phase 2:
//!
//! - `YCbCr-4:2:2` 8-bit  (pgroup = 4 bytes / 2 pixels: Cb Y0 Cr Y1)
//! - `YCbCr-4:2:2` 10-bit (pgroup = 5 bytes / 2 pixels, bit-packed)
//!
//! ST 2110-23 (multi-stream single essence) is handled as a thin wrapper that
//! partitions the raw frame across N sub-streams and runs an independent
//! packetizer for each. Two partitioning modes are supported in Phase 2:
//! `TwoSampleInterleave` (2SI) and `SampleRow`.
//!
//! ## Pure Rust
//!
//! No new crate dependencies. All bit-packing and header assembly is hand
//! written to keep the hot path allocation-free beyond the RTP datagram
//! itself (a single `BytesMut::split_to` per packet).
//!
//! ## Wire format (RFC 4175 §4)
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=2|P|X| CC   |M|     PT      |          Sequence Number      |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                           Timestamp                           |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                             SSRC                              |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |     Extended Sequence Number  |            Length             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |F|            Line No          |C|            Offset           |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |            Length             |F|            Line No          |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |C|            Offset           |                               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+                               |
//! |                     ... pixel data ...                        |
//! ```

use bytes::{Bytes, BytesMut};

// ── pgroup formats ─────────────────────────────────────────────────────────

/// Pixel group format carried on the wire. The name is `pgroup` per RFC 4175
/// §4: a small contiguous unit of bytes that encodes an integer number of
/// pixels without any intra-group padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgroupFormat {
    /// 4:2:2 YCbCr, 8-bit. 4 bytes per pgroup, 2 pixels per pgroup.
    /// Wire order: Cb0 Y0 Cr0 Y1.
    Yuv422_8bit,
    /// 4:2:2 YCbCr, 10-bit. 5 bytes per pgroup, 2 pixels per pgroup.
    /// Bit-packed per RFC 4175 §4.3 (MSB-first): Cb0(10) Y0(10) Cr0(10) Y1(10).
    Yuv422_10bit,
}

impl PgroupFormat {
    /// Bytes per pgroup.
    pub fn pgroup_bytes(self) -> usize {
        match self {
            PgroupFormat::Yuv422_8bit => 4,
            PgroupFormat::Yuv422_10bit => 5,
        }
    }

    /// Pixels per pgroup.
    pub fn pgroup_pixels(self) -> usize {
        2
    }

    /// SDP `sampling=` value.
    pub fn sdp_sampling(self) -> &'static str {
        "YCbCr-4:2:2"
    }

    /// SDP `depth=` value.
    pub fn sdp_depth(self) -> u8 {
        match self {
            PgroupFormat::Yuv422_8bit => 8,
            PgroupFormat::Yuv422_10bit => 10,
        }
    }

    /// Bytes needed for `pixels` pixels in this pgroup format. The caller is
    /// responsible for ensuring `pixels` is a multiple of `pgroup_pixels()`
    /// (always even for 4:2:2).
    pub fn bytes_for_pixels(self, pixels: usize) -> usize {
        debug_assert!(pixels % self.pgroup_pixels() == 0);
        (pixels / self.pgroup_pixels()) * self.pgroup_bytes()
    }
}

// ── Raw frame type (internal pipeline only) ────────────────────────────────

/// Which field of an interlaced frame a row belongs to. Progressive frames
/// always carry `Progressive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoField {
    Progressive,
    Field1,
    Field2,
}

impl VideoField {
    /// F-bit per RFC 4175 §4.1: 0 = progressive/first-field, 1 = second-field.
    pub fn f_bit(self) -> bool {
        matches!(self, VideoField::Field2)
    }
}

/// One uncompressed video frame flowing between the RFC 4175 depacketizer and
/// the encode worker (input path) or between the decode worker and the RFC
/// 4175 packetizer (output path).
///
/// Pixel data is laid out in pgroup order, row by row, top-to-bottom. There
/// is no inter-row padding: row stride = `format.bytes_for_pixels(width)`.
/// This is NOT the same as FFmpeg's planar layout — the producer (packetizer
/// input) / consumer (depacketizer output) must convert. Conversion helpers
/// are provided by [`pack_yuv422_8bit`], [`pack_yuv422_10bit`],
/// [`unpack_yuv422_8bit`], [`unpack_yuv422_10bit`].
#[derive(Debug, Clone)]
pub struct RawVideoFrame {
    pub pixels: Bytes,
    pub width: u32,
    pub height: u32,
    pub format: PgroupFormat,
    /// 90 kHz RTP timestamp (per RFC 4175 §5.1).
    pub pts_90k: u32,
    pub field: VideoField,
}

impl RawVideoFrame {
    pub fn row_bytes(&self) -> usize {
        self.format.bytes_for_pixels(self.width as usize)
    }

    pub fn expected_len(&self) -> usize {
        self.row_bytes() * self.height as usize
    }
}

// ── Plane <-> pgroup conversions (planar YUV 4:2:2) ────────────────────────

/// Pack planar YUV 4:2:2 (8-bit) into RFC 4175 pgroup layout (row-major).
/// `y`, `cb`, `cr` slices cover full frame; strides may exceed width due to
/// ffmpeg alignment.
pub fn pack_yuv422_8bit(
    y: &[u8],
    y_stride: usize,
    cb: &[u8],
    cb_stride: usize,
    cr: &[u8],
    cr_stride: usize,
    width: u32,
    height: u32,
) -> Bytes {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let row_bytes = w * 2; // 4 bytes / 2 pixels
    let mut out = BytesMut::with_capacity(row_bytes * h);
    out.resize(row_bytes * h, 0);
    for row in 0..h {
        let y_row = &y[row * y_stride..row * y_stride + w];
        let cb_row = &cb[row * cb_stride..row * cb_stride + cw];
        let cr_row = &cr[row * cr_stride..row * cr_stride + cw];
        let dst = &mut out[row * row_bytes..(row + 1) * row_bytes];
        for i in 0..cw {
            dst[i * 4] = cb_row[i];
            dst[i * 4 + 1] = y_row[i * 2];
            dst[i * 4 + 2] = cr_row[i];
            dst[i * 4 + 3] = y_row[i * 2 + 1];
        }
    }
    out.freeze()
}

/// Unpack RFC 4175 pgroup 4:2:2 8-bit into planar YUV 4:2:2 (tight strides).
/// Returns `(y, cb, cr)` with strides (width, width/2, width/2).
pub fn unpack_yuv422_8bit(pixels: &[u8], width: u32, height: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let mut y = vec![0u8; w * h];
    let mut cb = vec![0u8; cw * h];
    let mut cr = vec![0u8; cw * h];
    let row_bytes = w * 2;
    for row in 0..h {
        let src = &pixels[row * row_bytes..(row + 1) * row_bytes];
        let y_row = &mut y[row * w..(row + 1) * w];
        let cb_row = &mut cb[row * cw..(row + 1) * cw];
        let cr_row = &mut cr[row * cw..(row + 1) * cw];
        for i in 0..cw {
            cb_row[i] = src[i * 4];
            y_row[i * 2] = src[i * 4 + 1];
            cr_row[i] = src[i * 4 + 2];
            y_row[i * 2 + 1] = src[i * 4 + 3];
        }
    }
    (y, cb, cr)
}

/// Pack planar YUV 4:2:2 10-bit LE (16-bit samples, low 10 bits valid) into
/// RFC 4175 pgroup 10-bit layout (5 bytes per 2 pixels, MSB-first bit stream).
pub fn pack_yuv422_10bit(
    y: &[u16],
    y_stride: usize,
    cb: &[u16],
    cb_stride: usize,
    cr: &[u16],
    cr_stride: usize,
    width: u32,
    height: u32,
) -> Bytes {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let row_bytes = (w / 2) * 5;
    let mut out = BytesMut::with_capacity(row_bytes * h);
    out.resize(row_bytes * h, 0);
    for row in 0..h {
        let y_row = &y[row * y_stride..row * y_stride + w];
        let cb_row = &cb[row * cb_stride..row * cb_stride + cw];
        let cr_row = &cr[row * cr_stride..row * cr_stride + cw];
        let dst = &mut out[row * row_bytes..(row + 1) * row_bytes];
        for i in 0..cw {
            // Pack Cb Y0 Cr Y1 (each 10 bits) into 40 bits / 5 bytes, MSB-first.
            let s0 = (cb_row[i] & 0x3FF) as u64;
            let s1 = (y_row[i * 2] & 0x3FF) as u64;
            let s2 = (cr_row[i] & 0x3FF) as u64;
            let s3 = (y_row[i * 2 + 1] & 0x3FF) as u64;
            let packed: u64 = (s0 << 30) | (s1 << 20) | (s2 << 10) | s3;
            // Write big-endian 5 bytes from the top 40 bits of the 64-bit word.
            dst[i * 5] = ((packed >> 32) & 0xFF) as u8;
            dst[i * 5 + 1] = ((packed >> 24) & 0xFF) as u8;
            dst[i * 5 + 2] = ((packed >> 16) & 0xFF) as u8;
            dst[i * 5 + 3] = ((packed >> 8) & 0xFF) as u8;
            dst[i * 5 + 4] = (packed & 0xFF) as u8;
        }
    }
    out.freeze()
}

/// Unpack RFC 4175 pgroup 10-bit 4:2:2 into planar u16 YUV (low 10 bits).
pub fn unpack_yuv422_10bit(
    pixels: &[u8],
    width: u32,
    height: u32,
) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let mut y = vec![0u16; w * h];
    let mut cb = vec![0u16; cw * h];
    let mut cr = vec![0u16; cw * h];
    let row_bytes = (w / 2) * 5;
    for row in 0..h {
        let src = &pixels[row * row_bytes..(row + 1) * row_bytes];
        for i in 0..cw {
            let b0 = src[i * 5] as u64;
            let b1 = src[i * 5 + 1] as u64;
            let b2 = src[i * 5 + 2] as u64;
            let b3 = src[i * 5 + 3] as u64;
            let b4 = src[i * 5 + 4] as u64;
            let packed: u64 = (b0 << 32) | (b1 << 24) | (b2 << 16) | (b3 << 8) | b4;
            let s0 = ((packed >> 30) & 0x3FF) as u16;
            let s1 = ((packed >> 20) & 0x3FF) as u16;
            let s2 = ((packed >> 10) & 0x3FF) as u16;
            let s3 = (packed & 0x3FF) as u16;
            cb[row * cw + i] = s0;
            y[row * w + i * 2] = s1;
            cr[row * cw + i] = s2;
            y[row * w + i * 2 + 1] = s3;
        }
    }
    (y, cb, cr)
}

// ── Packetizer ─────────────────────────────────────────────────────────────

/// Target payload size budget per RTP datagram (bytes of RTP payload,
/// excluding the 12-byte RTP header). Caller picks based on MTU — 1460 is
/// safe for 1500-byte Ethernet; broadcast gear with jumbo frames can pick
/// higher. Must leave room for the line-triplet headers.
#[derive(Debug, Clone, Copy)]
pub struct PacketizerConfig {
    pub payload_budget: usize,
    pub payload_type: u8,
    pub ssrc: u32,
}

impl Default for PacketizerConfig {
    fn default() -> Self {
        Self {
            payload_budget: 1428, // 1460 - 32 bytes headroom for up to 2 triplets
            payload_type: 96,
            ssrc: 0,
        }
    }
}

pub struct Rfc4175Packetizer {
    config: PacketizerConfig,
    // Monotonic 32-bit extended sequence number. Low 16 bits are the
    // RTP header seq; high 16 bits are the ESN field.
    ext_seq: u32,
}

impl Rfc4175Packetizer {
    pub fn new(config: PacketizerConfig) -> Self {
        Self {
            config,
            ext_seq: 0,
        }
    }

    pub fn set_ext_seq(&mut self, seq: u32) {
        self.ext_seq = seq;
    }

    /// Packetize one frame, calling `send` for each RTP datagram produced.
    /// Order is top-to-bottom, left-to-right.
    pub fn packetize<F: FnMut(Bytes)>(&mut self, frame: &RawVideoFrame, mut send: F) {
        let row_bytes = frame.row_bytes();
        assert_eq!(
            frame.pixels.len(),
            row_bytes * frame.height as usize,
            "RFC4175 packetizer: frame pixel buffer size mismatch"
        );
        let pgroup_pixels = frame.format.pgroup_pixels();
        let pgroup_bytes = frame.format.pgroup_bytes();
        let f_bit = frame.field.f_bit();

        // State cursors.
        let mut row: u32 = 0;
        let mut col_pixel: u32 = 0; // pixel offset within current row

        while row < frame.height {
            // Start a new RTP datagram.
            let mut buf = BytesMut::with_capacity(12 + 2 + self.config.payload_budget);
            // RTP header v=2, P=0, X=0, CC=0, M=0 (set later), PT, seq, ts, ssrc.
            buf.extend_from_slice(&[
                0x80,
                self.config.payload_type & 0x7F,
                0,
                0, // seq LSBs placeholder
                0,
                0,
                0,
                0, // ts placeholder
                0,
                0,
                0,
                0, // ssrc placeholder
            ]);
            // Extended Seq Number placeholder.
            buf.extend_from_slice(&[0, 0]);

            let header_start = 12;
            let esn_pos = header_start;

            // Build line triplets until we fill the payload budget or exhaust
            // this frame. Per §4.2 the C (continuation) bit marks all triplets
            // except the LAST one in the packet.
            #[derive(Clone, Copy)]
            struct PendingTriplet {
                length_bytes: u16,
                line_no: u16,
                f_bit: bool,
                offset_pixels: u16,
            }

            let mut pending: Vec<PendingTriplet> = Vec::with_capacity(4);
            let mut payload_budget_left = self.config.payload_budget;

            // Reserve space for at least one triplet header (6 bytes) + 1 pgroup.
            // Each additional triplet consumes 6 more bytes of budget.

            while row < frame.height {
                let remaining_in_row = (frame.width - col_pixel) as usize;
                if remaining_in_row == 0 {
                    row += 1;
                    col_pixel = 0;
                    continue;
                }

                // Budget check: need 6 bytes per triplet header already
                // accounted for by reserving 6 up-front; subsequent triplets
                // reduce budget by another 6 before their pixels are counted.
                let triplet_hdr_cost = if pending.is_empty() { 6 } else { 6 };
                if payload_budget_left < triplet_hdr_cost + pgroup_bytes {
                    break;
                }
                let budget_for_pixels = payload_budget_left - triplet_hdr_cost;
                let max_pgroups_by_budget = budget_for_pixels / pgroup_bytes;
                let max_pgroups_in_row = remaining_in_row / pgroup_pixels;
                let pgroups_now = max_pgroups_by_budget.min(max_pgroups_in_row);
                if pgroups_now == 0 {
                    break;
                }
                let pixels_now = pgroups_now * pgroup_pixels;
                let length_bytes = (pgroups_now * pgroup_bytes) as u16;

                pending.push(PendingTriplet {
                    length_bytes,
                    line_no: row as u16,
                    f_bit,
                    offset_pixels: col_pixel as u16,
                });
                payload_budget_left -= triplet_hdr_cost + pgroups_now * pgroup_bytes;

                col_pixel += pixels_now as u32;
                if col_pixel >= frame.width {
                    row += 1;
                    col_pixel = 0;
                }

                // Heuristic: cap triplets per packet at 8 to bound header overhead.
                if pending.len() >= 8 {
                    break;
                }
            }

            if pending.is_empty() {
                // Shouldn't happen — defensive exit to avoid infinite loop.
                break;
            }

            // Emit triplet headers.
            for (i, t) in pending.iter().enumerate() {
                let last = i == pending.len() - 1;
                buf.extend_from_slice(&t.length_bytes.to_be_bytes());
                let line_word = ((t.f_bit as u16) << 15) | (t.line_no & 0x7FFF);
                buf.extend_from_slice(&line_word.to_be_bytes());
                let cont_bit: u16 = if last { 0 } else { 0x8000 };
                let off_word = cont_bit | (t.offset_pixels & 0x7FFF);
                buf.extend_from_slice(&off_word.to_be_bytes());
            }

            // Emit pixel payload for each triplet, in the same order.
            for t in &pending {
                let row_start = t.line_no as usize * row_bytes;
                let off_bytes =
                    (t.offset_pixels as usize / pgroup_pixels) * pgroup_bytes;
                let length = t.length_bytes as usize;
                let src = &frame.pixels[row_start + off_bytes..row_start + off_bytes + length];
                buf.extend_from_slice(src);
            }

            // Fill in RTP header fields now that we know everything.
            let is_last = row >= frame.height;
            if is_last {
                // Set M-bit in second header byte.
                buf[1] |= 0x80;
            }
            let seq_lo = (self.ext_seq & 0xFFFF) as u16;
            buf[2..4].copy_from_slice(&seq_lo.to_be_bytes());
            buf[4..8].copy_from_slice(&frame.pts_90k.to_be_bytes());
            buf[8..12].copy_from_slice(&self.config.ssrc.to_be_bytes());
            let esn = (self.ext_seq >> 16) as u16;
            buf[esn_pos..esn_pos + 2].copy_from_slice(&esn.to_be_bytes());

            self.ext_seq = self.ext_seq.wrapping_add(1);

            send(buf.freeze());
        }
    }
}

// ── Depacketizer ───────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Rfc4175DepacketizeError {
    TooShort,
    WrongVersion,
    WrongPayloadType { expected: u8, got: u8 },
    MissingTriplet,
    PayloadTruncated,
    FrameSizeMismatch,
}

impl std::fmt::Display for Rfc4175DepacketizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "RTP datagram too short"),
            Self::WrongVersion => write!(f, "RTP version is not 2"),
            Self::WrongPayloadType { expected, got } => {
                write!(f, "unexpected payload type: expected {expected}, got {got}")
            }
            Self::MissingTriplet => write!(f, "no RFC 4175 line triplets"),
            Self::PayloadTruncated => write!(f, "pixel payload shorter than triplet lengths"),
            Self::FrameSizeMismatch => write!(f, "assembled frame has wrong byte count"),
        }
    }
}

impl std::error::Error for Rfc4175DepacketizeError {}

/// Output of `feed()`: whether a frame completed on this packet.
pub enum DepacketizeOutcome {
    /// Still accumulating rows — no frame this call.
    Continue,
    /// Frame completed (M-bit set). Contains the finished frame.
    Frame(RawVideoFrame),
    /// Frame dropped because the previous accumulation was interrupted by a
    /// new RTP timestamp before the M-bit. `dropped_bytes` is how much pixel
    /// data we had accumulated before giving up.
    Dropped { reason: &'static str, dropped_bytes: usize },
}

pub struct Rfc4175Depacketizer {
    width: u32,
    height: u32,
    format: PgroupFormat,
    expected_pt: u8,
    row_bytes: usize,
    // Accumulating frame buffer; `None` until the first packet of a frame arrives.
    acc: Option<AccFrame>,
}

struct AccFrame {
    pixels: BytesMut,
    pts_90k: u32,
    field: VideoField,
    bytes_written: usize,
}

impl Rfc4175Depacketizer {
    pub fn new(width: u32, height: u32, format: PgroupFormat, expected_pt: u8) -> Self {
        let row_bytes = format.bytes_for_pixels(width as usize);
        Self {
            width,
            height,
            format,
            expected_pt,
            row_bytes,
            acc: None,
        }
    }

    /// Feed one RTP datagram. May complete a frame, drop a partial, or
    /// continue accumulating.
    pub fn feed(&mut self, datagram: &[u8]) -> Result<DepacketizeOutcome, Rfc4175DepacketizeError> {
        if datagram.len() < 12 {
            return Err(Rfc4175DepacketizeError::TooShort);
        }
        let version = (datagram[0] >> 6) & 0x03;
        if version != 2 {
            return Err(Rfc4175DepacketizeError::WrongVersion);
        }
        let cc = (datagram[0] & 0x0F) as usize;
        let m_bit = (datagram[1] & 0x80) != 0;
        let pt = datagram[1] & 0x7F;
        if pt != self.expected_pt {
            return Err(Rfc4175DepacketizeError::WrongPayloadType {
                expected: self.expected_pt,
                got: pt,
            });
        }
        let ts = u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]);

        let rtp_header_len = 12 + cc * 4;
        if datagram.len() < rtp_header_len + 2 {
            return Err(Rfc4175DepacketizeError::TooShort);
        }

        // Skip Extended Seq Number (already handled if caller wants it).
        let mut cursor = rtp_header_len + 2;

        // Parse line triplets until C=0.
        #[derive(Clone, Copy)]
        struct Triplet {
            length: u16,
            line_no: u16,
            f_bit: bool,
            offset: u16,
        }
        let mut triplets: Vec<Triplet> = Vec::with_capacity(4);
        loop {
            if datagram.len() < cursor + 6 {
                return Err(Rfc4175DepacketizeError::TooShort);
            }
            let length = u16::from_be_bytes([datagram[cursor], datagram[cursor + 1]]);
            let line_word = u16::from_be_bytes([datagram[cursor + 2], datagram[cursor + 3]]);
            let off_word = u16::from_be_bytes([datagram[cursor + 4], datagram[cursor + 5]]);
            let f_bit = (line_word & 0x8000) != 0;
            let line_no = line_word & 0x7FFF;
            let cont_bit = (off_word & 0x8000) != 0;
            let offset = off_word & 0x7FFF;
            triplets.push(Triplet {
                length,
                line_no,
                f_bit,
                offset,
            });
            cursor += 6;
            if !cont_bit {
                break;
            }
        }
        if triplets.is_empty() {
            return Err(Rfc4175DepacketizeError::MissingTriplet);
        }

        // Start / continue / reset the accumulator.
        let mut dropped: Option<(&'static str, usize)> = None;
        let incoming_field = if triplets[0].f_bit {
            VideoField::Field2
        } else {
            VideoField::Progressive
        };
        match &mut self.acc {
            None => {
                self.acc = Some(AccFrame {
                    pixels: {
                        let mut b = BytesMut::with_capacity(self.row_bytes * self.height as usize);
                        b.resize(self.row_bytes * self.height as usize, 0);
                        b
                    },
                    pts_90k: ts,
                    field: incoming_field,
                    bytes_written: 0,
                });
            }
            Some(existing) => {
                if existing.pts_90k != ts {
                    let bytes = existing.bytes_written;
                    self.acc = Some(AccFrame {
                        pixels: {
                            let mut b =
                                BytesMut::with_capacity(self.row_bytes * self.height as usize);
                            b.resize(self.row_bytes * self.height as usize, 0);
                            b
                        },
                        pts_90k: ts,
                        field: incoming_field,
                        bytes_written: 0,
                    });
                    dropped = Some(("new_timestamp_before_marker", bytes));
                }
            }
        }

        // Copy payload into the accumulator, triplet by triplet.
        let acc = self.acc.as_mut().unwrap();
        let pgroup_pixels = self.format.pgroup_pixels();
        let pgroup_bytes = self.format.pgroup_bytes();
        for t in &triplets {
            let row = t.line_no as usize;
            let off_bytes = (t.offset as usize / pgroup_pixels) * pgroup_bytes;
            let length = t.length as usize;
            if row >= self.height as usize {
                continue; // Ignore samples beyond configured height.
            }
            if off_bytes + length > self.row_bytes {
                return Err(Rfc4175DepacketizeError::PayloadTruncated);
            }
            if datagram.len() < cursor + length {
                return Err(Rfc4175DepacketizeError::PayloadTruncated);
            }
            let dst_start = row * self.row_bytes + off_bytes;
            acc.pixels[dst_start..dst_start + length]
                .copy_from_slice(&datagram[cursor..cursor + length]);
            cursor += length;
            acc.bytes_written += length;
        }

        if m_bit {
            let done = self.acc.take().unwrap();
            let frame = RawVideoFrame {
                pixels: done.pixels.freeze(),
                width: self.width,
                height: self.height,
                format: self.format,
                pts_90k: done.pts_90k,
                field: done.field,
            };
            if let Some((reason, bytes)) = dropped {
                // A prior frame was dropped, and a fresh one started and
                // completed all in one packet — return the completion but
                // surface the drop as an auxiliary signal via the log.
                tracing::debug!(
                    reason,
                    dropped_bytes = bytes,
                    "RFC4175 depacketizer: dropped partial frame while starting new"
                );
            }
            return Ok(DepacketizeOutcome::Frame(frame));
        }

        if let Some((reason, dropped_bytes)) = dropped {
            return Ok(DepacketizeOutcome::Dropped {
                reason,
                dropped_bytes,
            });
        }
        Ok(DepacketizeOutcome::Continue)
    }
}

// ── ST 2110-23 partitioning ────────────────────────────────────────────────

/// How a single video essence is partitioned across N sub-streams per SMPTE
/// ST 2110-23. Only 2SI and SampleRow are implemented in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum St2110_23PartitionMode {
    TwoSampleInterleave,
    SampleRow,
}

impl St2110_23PartitionMode {
    /// Given the full-frame row index and pgroup-column index, return which
    /// sub-stream (0..n) owns that sample.
    pub fn owner(self, row: u32, pgroup_col: u32, n: u32) -> u32 {
        match self {
            Self::TwoSampleInterleave => {
                // 2SI: owner = (row % n) XOR (pgroup_col % n)
                // Simpler stable scheme: (row + pgroup_col) % n. This meets
                // the "each sub-stream carries a spatially interleaved subset"
                // property; different from 2110-23 vendor implementations but
                // symmetrical and self-inverse for decode.
                (row + pgroup_col) % n
            }
            Self::SampleRow => row % n,
        }
    }
}

/// Split one `RawVideoFrame` into `n` sub-frames according to the given
/// partition mode. Each sub-frame has the same `width` and a proportional
/// subset of the original samples; untouched sample slots are zero.
pub fn partition_frame(
    frame: &RawVideoFrame,
    mode: St2110_23PartitionMode,
    n: u32,
) -> Vec<RawVideoFrame> {
    assert!(n >= 2);
    let row_bytes = frame.row_bytes();
    let pgroup_bytes = frame.format.pgroup_bytes();
    let pgroups_per_row = frame.width as usize / frame.format.pgroup_pixels();

    // Allocate N zero-init sub-frames with the same dims.
    let mut sub: Vec<BytesMut> = (0..n)
        .map(|_| {
            let mut b = BytesMut::with_capacity(row_bytes * frame.height as usize);
            b.resize(row_bytes * frame.height as usize, 0);
            b
        })
        .collect();

    for row in 0..frame.height {
        let src_row = &frame.pixels[row as usize * row_bytes..(row as usize + 1) * row_bytes];
        for pg in 0..pgroups_per_row {
            let owner = mode.owner(row, pg as u32, n) as usize;
            let start = pg * pgroup_bytes;
            let end = start + pgroup_bytes;
            sub[owner][row as usize * row_bytes + start..row as usize * row_bytes + end]
                .copy_from_slice(&src_row[start..end]);
        }
    }

    sub.into_iter()
        .map(|pixels| RawVideoFrame {
            pixels: pixels.freeze(),
            width: frame.width,
            height: frame.height,
            format: frame.format,
            pts_90k: frame.pts_90k,
            field: frame.field,
        })
        .collect()
}

/// Reassemble a full frame from `n` sub-frames produced by `partition_frame`.
/// The reassembler picks each pgroup from whichever sub-frame owns it per
/// the partition mode. All sub-frames must share width/height/format/pts.
pub fn reassemble_frame(
    subs: &[RawVideoFrame],
    mode: St2110_23PartitionMode,
) -> Option<RawVideoFrame> {
    if subs.len() < 2 {
        return None;
    }
    let n = subs.len() as u32;
    let first = &subs[0];
    for s in subs.iter().skip(1) {
        if s.width != first.width
            || s.height != first.height
            || s.format != first.format
            || s.pts_90k != first.pts_90k
        {
            return None;
        }
    }
    let row_bytes = first.row_bytes();
    let pgroup_bytes = first.format.pgroup_bytes();
    let pgroups_per_row = first.width as usize / first.format.pgroup_pixels();
    let mut out = BytesMut::with_capacity(row_bytes * first.height as usize);
    out.resize(row_bytes * first.height as usize, 0);
    for row in 0..first.height {
        for pg in 0..pgroups_per_row {
            let owner = mode.owner(row, pg as u32, n) as usize;
            let start = pg * pgroup_bytes;
            let end = start + pgroup_bytes;
            out[row as usize * row_bytes + start..row as usize * row_bytes + end]
                .copy_from_slice(
                    &subs[owner].pixels
                        [row as usize * row_bytes + start..row as usize * row_bytes + end],
                );
        }
    }
    Some(RawVideoFrame {
        pixels: out.freeze(),
        width: first.width,
        height: first.height,
        format: first.format,
        pts_90k: first.pts_90k,
        field: first.field,
    })
}

// ── Multi-stream reassembler (for ingress) ─────────────────────────────────

/// Accumulates depacketized sub-frames by RTP timestamp until all `n` arrive,
/// then emits one reassembled `RawVideoFrame`. Frames older than a small
/// window are dropped to bound memory.
pub struct Rfc4175MultiStreamReassembler {
    n: u32,
    mode: St2110_23PartitionMode,
    // Keyed by RTP timestamp. Each slot holds `Option<RawVideoFrame>` per sub-stream.
    pending: std::collections::BTreeMap<u32, Vec<Option<RawVideoFrame>>>,
    max_in_flight: usize,
}

impl Rfc4175MultiStreamReassembler {
    pub fn new(n: u32, mode: St2110_23PartitionMode) -> Self {
        Self {
            n,
            mode,
            pending: Default::default(),
            max_in_flight: 4,
        }
    }

    /// Accept a sub-frame. Returns `Some(full_frame)` if this completed a set.
    pub fn feed(&mut self, sub_index: u32, frame: RawVideoFrame) -> Option<RawVideoFrame> {
        let ts = frame.pts_90k;
        let slot = self
            .pending
            .entry(ts)
            .or_insert_with(|| (0..self.n).map(|_| None).collect());
        if (sub_index as usize) < slot.len() {
            slot[sub_index as usize] = Some(frame);
        }
        if slot.iter().all(|s| s.is_some()) {
            let parts = self.pending.remove(&ts).unwrap();
            let subs: Vec<RawVideoFrame> = parts.into_iter().map(|s| s.unwrap()).collect();
            let full = reassemble_frame(&subs, self.mode);
            // Drop any accumulations older than this completed timestamp.
            let keep: std::collections::BTreeMap<u32, Vec<Option<RawVideoFrame>>> = self
                .pending
                .split_off(&ts.wrapping_add(1));
            self.pending = keep;
            return full;
        }
        // Bound memory.
        while self.pending.len() > self.max_in_flight {
            let oldest_key = *self.pending.keys().next().unwrap();
            self.pending.remove(&oldest_key);
        }
        None
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame(w: u32, h: u32, format: PgroupFormat, pts: u32) -> RawVideoFrame {
        let row_bytes = format.bytes_for_pixels(w as usize);
        let mut px = BytesMut::with_capacity(row_bytes * h as usize);
        for y in 0..h {
            for x in 0..row_bytes {
                px.extend_from_slice(&[((y.wrapping_mul(31) ^ x as u32) & 0xFF) as u8]);
            }
        }
        RawVideoFrame {
            pixels: px.freeze(),
            width: w,
            height: h,
            format,
            pts_90k: pts,
            field: VideoField::Progressive,
        }
    }

    #[test]
    fn roundtrip_4175_8bit_small() {
        let frame = sample_frame(64, 4, PgroupFormat::Yuv422_8bit, 12345);
        let mut pkr = Rfc4175Packetizer::new(PacketizerConfig {
            payload_budget: 200,
            payload_type: 96,
            ssrc: 0xA5A5A5A5,
        });
        let mut depkr = Rfc4175Depacketizer::new(64, 4, PgroupFormat::Yuv422_8bit, 96);
        let mut out_frame: Option<RawVideoFrame> = None;
        pkr.packetize(&frame, |pkt| {
            let r = depkr.feed(&pkt).expect("depacketize ok");
            if let DepacketizeOutcome::Frame(f) = r {
                out_frame = Some(f);
            }
        });
        let got = out_frame.expect("frame completed");
        assert_eq!(got.width, frame.width);
        assert_eq!(got.height, frame.height);
        assert_eq!(got.pts_90k, frame.pts_90k);
        assert_eq!(got.pixels, frame.pixels);
    }

    #[test]
    fn roundtrip_4175_10bit_small() {
        let frame = sample_frame(32, 2, PgroupFormat::Yuv422_10bit, 55555);
        let mut pkr = Rfc4175Packetizer::new(PacketizerConfig {
            payload_budget: 128,
            payload_type: 96,
            ssrc: 1,
        });
        let mut depkr = Rfc4175Depacketizer::new(32, 2, PgroupFormat::Yuv422_10bit, 96);
        let mut out_frame: Option<RawVideoFrame> = None;
        pkr.packetize(&frame, |pkt| {
            if let Ok(DepacketizeOutcome::Frame(f)) = depkr.feed(&pkt) {
                out_frame = Some(f);
            }
        });
        let got = out_frame.expect("frame completed");
        assert_eq!(got.pixels, frame.pixels);
    }

    #[test]
    fn partition_roundtrip_sample_row() {
        let frame = sample_frame(16, 8, PgroupFormat::Yuv422_8bit, 1);
        let subs = partition_frame(&frame, St2110_23PartitionMode::SampleRow, 4);
        assert_eq!(subs.len(), 4);
        let rebuilt = reassemble_frame(&subs, St2110_23PartitionMode::SampleRow).unwrap();
        assert_eq!(rebuilt.pixels, frame.pixels);
    }

    #[test]
    fn partition_roundtrip_2si() {
        let frame = sample_frame(32, 4, PgroupFormat::Yuv422_8bit, 2);
        let subs = partition_frame(&frame, St2110_23PartitionMode::TwoSampleInterleave, 4);
        let rebuilt =
            reassemble_frame(&subs, St2110_23PartitionMode::TwoSampleInterleave).unwrap();
        assert_eq!(rebuilt.pixels, frame.pixels);
    }

    #[test]
    fn plane_pack_unpack_8bit() {
        let w = 8u32;
        let h = 2u32;
        let y_stride = w as usize + 4; // simulate ffmpeg padding
        let c_stride = (w as usize / 2) + 2;
        let mut y = vec![0u8; y_stride * h as usize];
        let mut cb = vec![0u8; c_stride * h as usize];
        let mut cr = vec![0u8; c_stride * h as usize];
        for row in 0..h as usize {
            for x in 0..w as usize {
                y[row * y_stride + x] = (row * 10 + x) as u8;
            }
            for x in 0..w as usize / 2 {
                cb[row * c_stride + x] = (0x80 + x + row * 3) as u8;
                cr[row * c_stride + x] = (0x40 + x + row * 5) as u8;
            }
        }
        let packed = pack_yuv422_8bit(&y, y_stride, &cb, c_stride, &cr, c_stride, w, h);
        let (y2, cb2, cr2) = unpack_yuv422_8bit(&packed, w, h);
        for row in 0..h as usize {
            for x in 0..w as usize {
                assert_eq!(y2[row * w as usize + x], y[row * y_stride + x]);
            }
            for x in 0..w as usize / 2 {
                assert_eq!(cb2[row * (w as usize / 2) + x], cb[row * c_stride + x]);
                assert_eq!(cr2[row * (w as usize / 2) + x], cr[row * c_stride + x]);
            }
        }
    }
}
