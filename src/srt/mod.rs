// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SRT connection management.
//!
//! Uses the pure Rust [`srt_transport`] crate for async SRT I/O (no C library
//! dependency).
//!
//! - [`connection`] -- Connect helpers for caller and listener modes with
//!   exponential backoff retry (500ms to 30s cap). Includes convenience wrappers
//!   for input, output, and redundancy leg connections, plus optional AES
//!   encryption setup.
//! - [`bond_supervisor`] -- native libsrt socket-group bonding: per-member
//!   source / weight planning, re-adding a member libsrt dropped as broken,
//!   and the `srt_bond_member_down` / `srt_bond_member_restored` events.

pub(crate) mod bond_supervisor;
pub mod connection;
