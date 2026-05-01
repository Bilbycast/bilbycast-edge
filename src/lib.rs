// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! # bilbycast-edge
//!
//! A high-performance RTP/SMPTE 2022-2 over SRT transport bridge with
//! SMPTE 2022-7 hitless redundancy and SMPTE 2022-1 Forward Error Correction.
//!
//! ## Architecture
//!
//! The application is organized around **flows**. Each flow has a single input
//! (RTP/UDP or SRT) that receives packets and fans them out via a tokio
//! broadcast channel to one or more outputs (RTP/UDP or SRT).
//!
//! ## Modules
//!
//! - [`api`] -- REST API, WebSocket, and Prometheus metrics server (axum-based)
//! - [`config`] -- Configuration models, JSON persistence, and validation
//! - [`engine`] -- Flow lifecycle management, input/output task spawning
//! - [`fec`] -- SMPTE 2022-1 Forward Error Correction encoder and decoder
//! - [`redundancy`] -- SMPTE 2022-7 hitless merge (RX) and duplication (TX)
//! - [`srt`] -- SRT connection helpers and statistics polling
//! - [`stats`] -- Lock-free atomic statistics collection and throughput estimation
//! - [`util`] -- RTP header parsing, UDP socket utilities, monotonic clock

pub mod api;
pub mod config;
#[cfg(all(feature = "display", target_os = "linux"))]
pub mod display;
pub mod engine;
pub mod fec;
pub mod manager;
/// Media-library storage for the file-backed media-player input.
/// Owns the on-disk directory, list / upload-chunk / delete primitives, and
/// the WS command handlers the manager forwards into.
pub mod media;
/// Edge-side observability outputs (structured-JSON log shipper for SIEM /
/// NMS pickup) that complement the manager WS event stream and Prometheus.
pub mod observability;
/// Replay store: continuous flow recording to rolling MPEG-TS segments
/// with a side-car timecode → byte-offset index, plus playback reader.
/// Lives behind the `replay` Cargo feature.
#[cfg(feature = "replay")]
pub mod replay;
pub mod redundancy;
pub mod setup;
pub mod srt;
pub mod stats;
pub mod tunnel;
pub mod util;
