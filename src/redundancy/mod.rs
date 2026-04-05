// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE 2022-7 hitless redundancy: merge (RX).
//!
//! - [`merger::HitlessMerger`] -- De-duplicates RTP packets arriving from two
//!   independent SRT input legs, forwarding only sequence-advancing packets
//!   downstream. Uses wrapping u16 arithmetic for correct handling of RTP
//!   sequence number wraparound at 65535.

pub mod merger;
