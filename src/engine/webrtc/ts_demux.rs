// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Re-export of the TS demuxer from `engine::ts_demux`.
//!
//! The demuxer was moved to `engine::ts_demux` so it can be used by both
//! WebRTC and RTMP outputs without feature-gating.

pub use crate::engine::ts_demux::*;
