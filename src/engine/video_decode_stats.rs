// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! Lock-free counters for the video-decode stage. Surfaced on
//! [`crate::stats::models::VideoDecodeStatsSnapshot`] so the manager UI can
//! distinguish a true transcode (`video_decode` + `video_encode` on the same
//! output) from encode-only (ST 2110-20 ingress) and decode-only
//! (`engine::output_display`). The counter is intentionally tiny and
//! standalone — both `engine::ts_video_replace` (which couples it with an
//! encoder) and `engine::output_display` (which uses it on its own) share
//! the same shape.

use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free counters for the video-decode stage.
#[derive(Debug, Default)]
pub struct VideoDecodeStats {
    /// Compressed video frames fed into the decoder.
    pub input_frames: AtomicU64,
    /// Raw video frames successfully emitted.
    pub output_frames: AtomicU64,
    /// Frames that failed to decode (corrupt input, codec error).
    pub decode_errors: AtomicU64,
}

impl VideoDecodeStats {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn inc_input(&self) {
        self.input_frames.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_output(&self) {
        self.output_frames.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_error(&self) {
        self.decode_errors.fetch_add(1, Ordering::Relaxed);
    }
}
