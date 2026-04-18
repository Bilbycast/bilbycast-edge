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
