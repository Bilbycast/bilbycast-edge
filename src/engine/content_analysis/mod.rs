// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! In-depth content-analysis subsystem (Phase 1–3).
//!
//! Mirrors the tier layout of [`crate::config::models::ContentAnalysisConfig`].
//! Each tier is spawned as a **dedicated broadcast subscriber** — it cannot
//! add jitter or backpressure to the data path. On `broadcast::Lagged` the
//! analyser drops samples and increments a counter; the data path continues
//! uninterrupted.
//!
//! Phase 1 delivers the **Lite** tier (compressed-domain only) covering:
//! GOP structure, container/codec signalling (HDR / AR / colour / AFD),
//! SMPTE timecode, CEA-608/708 caption presence, SCTE-35 cue presence, and
//! Media Delivery Index (RFC 4445). Audio Full and Video Full arrive in
//! Phases 2 and 3 respectively; their accumulator fields are already
//! carried on [`crate::stats::collector::ContentAnalysisAccumulator`] so
//! the manager UI shape doesn't churn between phases.

pub mod audio_full;
pub mod bitreader;
pub mod captions;
pub mod gop;
pub mod lite;
pub mod mdi;
pub mod scte35;
pub mod signalling;
pub mod timecode;
pub mod video_full;

pub use audio_full::spawn_content_analysis_audio_full;
pub use lite::spawn_content_analysis_lite;
pub use video_full::spawn_content_analysis_video_full;
