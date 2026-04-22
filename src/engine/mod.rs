// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
pub mod degradation_monitor;
pub mod input_test_pattern;
pub mod bonded_scheduler;
pub mod delay_buffer;
pub mod flow;
pub mod resource_monitor;
pub mod input_bonded;
pub mod input_rtmp;
pub mod input_rtsp;
#[cfg(feature = "webrtc")]
pub mod input_webrtc;
pub mod media_analysis;
pub mod input_rist;
pub mod input_rtp;
pub mod input_srt;
pub mod input_udp;
pub mod output_bonded;
pub mod output_rist;
pub mod output_udp;
pub mod manager;
pub mod output_hls;
/// Fragmented-MP4 (CMAF / CMAF-LL) HTTP-push output with optional CENC
/// ClearKey encryption. Sibling to `output_hls` — same push model but
/// fMP4 instead of MPEG-TS.
pub mod cmaf;
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
pub mod ts_continuity_fixer;
pub mod ts_program_filter;
pub mod ts_pid_remapper;
pub mod ts_psi_catalog;
pub mod ts_es_bus;
pub mod ts_audio_replace;
pub mod ts_video_replace;
pub mod video_encode_util;

/// Ingress-side audio + video transcoding composer for TS-carrying inputs.
/// Wraps [`ts_audio_replace::TsAudioReplacer`] and [`ts_video_replace::TsVideoReplacer`]
/// in the same audio-first-then-video order the TS outputs already use, so
/// every Group A input (RTP, UDP, SRT, RIST, RTMP, RTSP, WebRTC) can normalise
/// its feed once before the flow broadcast channel and amortise codec work
/// across all attached outputs.
pub mod input_transcode;

/// Ingress-side PCM-shape processor for raw PCM-RTP inputs (ST 2110-30,
/// `rtp_audio`). Depacketizes, optionally reshapes via `PlanarAudioTranscoder`
/// (stays PCM), optionally encodes via `AudioEncoder` and muxes audio-only TS
/// (shape-change: PCM-RTP → MPEG-TS). Runs entirely inside the input task via
/// `tokio::task::block_in_place`.
pub mod input_pcm_encode;
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

/// Silent-audio generator + audio-drop watchdog used by
/// `audio_encode.silent_fallback` on RTMP, WebRTC and CMAF outputs.
/// Emits zero-filled planar PCM chunks at the encoder's native cadence
/// whenever the upstream source has no audio PID, or stops delivering
/// audio mid-stream.
pub mod audio_silence;

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

/// Shared runtime helpers for ST 2110-20 / -23 (uncompressed video). Heavy
/// codec work (encode / decode / scale) runs on dedicated blocking workers so
/// the tokio reactor is never blocked.
pub mod st2110_video_io;

pub mod input_st2110_30;
pub mod input_st2110_31;
pub mod input_st2110_40;
pub mod input_st2110_20;
pub mod input_st2110_23;
pub mod output_st2110_30;
pub mod output_st2110_31;
pub mod output_st2110_40;
pub mod output_st2110_20;
pub mod output_st2110_23;

/// Generic RFC 3551 PCM-over-RTP audio input. Wire-identical to ST 2110-30
/// but with no PTP / RFC 7273 / NMOS clock_domain. Drives the same shared
/// runtime as the ST 2110 audio modules.
pub mod input_rtp_audio;
/// Generic RFC 3551 PCM-over-RTP audio output (companion to `input_rtp_audio`).
/// Supports the same `transcode` block as ST 2110-30 outputs and a future
/// `transport_mode: "audio_302m"` for SMPTE 302M-in-MPEG-TS over RTP.
pub mod output_rtp_audio;
