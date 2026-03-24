// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! WebRTC WHIP/WHEP support for bilbycast-edge.
//!
//! Provides four modes of WebRTC operation:
//!
//! - **WHIP input** (server): Accept contributions from OBS, browsers via WHIP
//! - **WHIP output** (client): Push media to external WHIP endpoints (CDN, cloud)
//! - **WHEP output** (server): Serve media to browser viewers via WHEP
//! - **WHEP input** (client): Pull media from external WHEP servers
//!
//! All WebRTC code is feature-gated behind `#[cfg(feature = "webrtc")]`.
//! The WebRTC stack is `str0m` — a pure-Rust, sans-I/O implementation
//! that integrates with bilbycast's tokio event loop.

pub mod rtp_h264;
pub mod rtp_h264_depack;
pub mod session;
pub mod signaling;
pub mod ts_demux;
