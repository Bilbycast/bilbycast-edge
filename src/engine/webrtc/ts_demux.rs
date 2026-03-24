// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Re-export of the TS demuxer from `engine::ts_demux`.
//!
//! The demuxer was moved to `engine::ts_demux` so it can be used by both
//! WebRTC and RTMP outputs without feature-gating.

pub use crate::engine::ts_demux::*;
