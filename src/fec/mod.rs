// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SMPTE 2022-1 Forward Error Correction (FEC) encoder and decoder.
//!
//! Provides column and row XOR parity across an L x D matrix of RTP packets.
//! The [`encoder::FecEncoder`] generates FEC packets on the output side.
//! The [`decoder::FecDecoder`] recovers lost packets on the input side.
//! Both use [`matrix::FecMatrix`] for the underlying XOR computation.
//!
//! ## FEC Matrix Layout
//!
//! Packets are arranged in a column-major L x D grid:
//! - **L** (columns): number of columns (1-20)
//! - **D** (rows): number of rows (4-20)
//! - Column FEC: XOR parity of all D packets in one column
//! - Row FEC: XOR parity of all L packets in one row
//! - Recovery: if exactly 1 packet is missing in a column/row, XOR the remaining
//!   packets with the FEC packet to recover it.

pub mod decoder;
pub mod encoder;
pub mod matrix;

/// In-band marker that distinguishes a bilbycast-2022-1 FEC datagram from an
/// RTP media datagram on the same UDP port.
///
/// RTP's first byte is always `0x80..=0xBF` (V=2, X/CC/P bits). `'B'` is
/// `0x42`, so any datagram starting with these four bytes cannot be mistaken
/// for RTP. Encoder prepends this; decoder checks for it before running the
/// RTP-shape test.
pub const FEC_MAGIC: [u8; 4] = *b"BCFE";

/// Length of the bilbycast FEC header **after** the magic prefix.
/// Keeps encoder/decoder in sync if the header layout ever evolves.
pub const FEC_HEADER_LEN: usize = 10;
