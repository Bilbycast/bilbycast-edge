// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SMPTE 2022-7 hitless redundancy: merge (RX).
//!
//! - [`merger::HitlessMerger`] -- De-duplicates RTP packets arriving from two
//!   independent SRT input legs, forwarding only sequence-advancing packets
//!   downstream. Uses wrapping u16 arithmetic for correct handling of RTP
//!   sequence number wraparound at 65535.

pub mod merger;
