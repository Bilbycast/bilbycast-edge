// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Flow engine: lifecycle management for input and output tasks.
//!
//! The [`manager::FlowManager`] orchestrates creating, starting, stopping, and
//! querying flows. Each flow is represented by a [`flow::FlowRuntime`] which
//! owns the input task, output tasks, a broadcast channel connecting them,
//! and a cancellation token hierarchy for graceful shutdown.
//!
//! Input tasks ([`input_rtp`], [`input_srt`], [`input_rtmp`]) receive packets and publish
//! [`packet::RtpPacket`] structs to the broadcast channel. Output tasks
//! ([`output_rtp`], [`output_srt`]) subscribe independently and forward
//! packets to their destinations.
//!
//! Optional features are inserted at the input/output boundaries:
//! - **FEC decode** on RTP input (SMPTE 2022-1 packet recovery)
//! - **FEC encode** on RTP output (SMPTE 2022-1 parity generation)
//! - **Redundancy merge** on SRT input (SMPTE 2022-7 de-duplication)
//! - **Redundancy duplicate** on SRT output (SMPTE 2022-7 dual-leg send)

pub mod flow;
pub mod input_rtmp;
pub mod input_rtsp;
#[cfg(feature = "webrtc")]
pub mod input_webrtc;
pub mod media_analysis;
pub mod input_rtp;
pub mod input_srt;
pub mod input_udp;
pub mod output_udp;
pub mod manager;
pub mod output_hls;
pub mod output_rtmp;
pub mod output_rtp;
pub mod output_srt;
pub mod output_webrtc;
pub mod packet;
pub mod rtmp;
pub mod tr101290;
pub mod ts_parse;
#[cfg(feature = "webrtc")]
pub mod webrtc;
