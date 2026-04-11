// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

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

pub mod bandwidth_monitor;
pub mod delay_buffer;
pub mod flow;
pub mod resource_monitor;
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
pub mod thumbnail;
pub mod tr101290;
pub mod ts_demux;
pub mod ts_parse;
pub mod ts_program_filter;
#[cfg(feature = "webrtc")]
pub mod webrtc;

/// Per-output PCM audio transcoding stage (sample rate, bit depth, channel
/// routing, packet time, payload type). Pure Rust via `rubato`. Fed by ST 2110
/// audio inputs and the new `rtp_audio` input; consumed by ST 2110-30/-31,
/// `rtp_audio`, and SMPTE 302M-over-SRT/UDP/RTP outputs.
pub mod audio_transcode;

/// In-process AAC-LC decoder. Bridges compressed contribution audio (AAC in
/// MPEG-TS via RTMP/RTSP/SRT/UDP inputs) into the PCM pipeline so it can land
/// into the existing PCM-only outputs (ST 2110-30/-31, `rtp_audio`,
/// SMPTE 302M). Pure Rust via `symphonia-codec-aac`. AAC-LC mono/stereo only;
/// HE-AAC and multichannel AAC are rejected with a clear error.
pub mod audio_decode;

/// ffmpeg-sidecar audio encoder. Wraps a long-running ffmpeg subprocess as a
/// PCM → compressed-audio encoder for the RTMP, HLS, and WebRTC outputs.
/// Pure Rust binary — ffmpeg is invoked at runtime via subprocess, never
/// linked. Outputs that don't request `audio_encode` keep working unchanged.
/// Supports AAC-LC, HE-AAC v1/v2, Opus, MP2, AC-3.
pub mod audio_encode;

/// SMPTE 302M-2007 LPCM audio packetizer / depacketizer. Bit-packs 48 kHz
/// 16/20/24-bit audio into private MPEG-TS PES payloads tagged with the
/// `BSSD` registration descriptor. Pairs with [`audio_transcode`] (which
/// resamples non-48 kHz inputs and promotes to even channel counts) and
/// the TS muxer (which wraps the PES payload in `stream_type = 0x06`).
pub mod audio_302m;

/// Short-lived connection test tasks for validating input/output configs
/// without requiring a running flow.
pub mod test_connection;

/// Standby listener manager for passive-type inputs that are not assigned
/// to running flows. Keeps sockets bound so users can see connection status.
pub mod standby_listeners;

/// SMPTE ST 2110 essence-flow support (audio, ancillary, future video).
///
/// Phase 1 lands the foundation in three pure-Rust submodules:
/// - [`st2110::ptp`]: PTP state reporter (reads `ptp4l`'s management Unix
///   socket; reports `Unavailable` when no daemon is running)
/// - [`st2110::hwts`]: hardware timestamp helpers (`SO_TIMESTAMPING` enable
///   and cmsghdr parsing on Linux; no-op on other platforms)
/// - [`st2110::redblue`]: SMPTE 2022-7 dual-network ("Red"/"Blue") bind
///   helpers and recv loop, reusing `redundancy::merger::HitlessMerger`
///
/// Packetizers and runtime input/output tasks land in Phase 1 step 4.
pub mod st2110;

/// Shared input/output runtime helpers for ST 2110 essence flows. The thin
/// `input_st2110_*` / `output_st2110_*` modules below delegate to the helpers
/// in this module so the recv/send/RedBluePair plumbing lives in one place.
pub mod st2110_io;

pub mod input_st2110_30;
pub mod input_st2110_31;
pub mod input_st2110_40;
pub mod output_st2110_30;
pub mod output_st2110_31;
pub mod output_st2110_40;

/// Generic RFC 3551 PCM-over-RTP audio input. Wire-identical to ST 2110-30
/// but with no PTP / RFC 7273 / NMOS clock_domain. Drives the same shared
/// runtime as the ST 2110 audio modules.
pub mod input_rtp_audio;
/// Generic RFC 3551 PCM-over-RTP audio output (companion to `input_rtp_audio`).
/// Supports the same `transcode` block as ST 2110-30 outputs and a future
/// `transport_mode: "audio_302m"` for SMPTE 302M-in-MPEG-TS over RTP.
pub mod output_rtp_audio;
