// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Silent-audio generator + drop watchdog.
//!
//! When an output's `audio_encode` block sets `silent_fallback: true`,
//! the output feeds a continuous zero-filled PCM track into its
//! [`AudioEncoder`](super::audio_encode::AudioEncoder) whenever the
//! upstream source has no audio PID at all, or stops delivering audio
//! mid-stream. This guarantees every emitted container always carries
//! a valid, monotonically-timestamped audio track.
//!
//! Motivation:
//! - Twitch / YouTube / Facebook RTMP ingest gate their live-preview
//!   thumbnailer on the presence of audio. A video-only source (common
//!   on IP cameras, drones, slide feeds) makes the service show LIVE
//!   but never renders a picture.
//! - WebRTC browsers emit muted-track warnings and may disable the
//!   video decoder when the audio track fails to ever produce samples.
//! - CMAF / HLS segmenters expect monotonic audio timestamps per
//!   segment; an audio drop that leaves a hole causes player stalls.
//! - Flow-group start: one member arriving before its audio source is
//!   ready shouldn't block the whole group.
//!
//! Architecture:
//! - Caller builds a [`SilenceGenerator`] at output startup with the
//!   declared sample-rate / channel count from the `audio_encode`
//!   block (defaulting to 48 kHz stereo when the block leaves them
//!   unset).
//! - Caller calls [`SilenceGenerator::mark_real_audio`] every time a
//!   real audio frame arrives. This resets the watchdog and snaps the
//!   internal PTS to the real audio's PTS so silent chunks emitted
//!   after a drop resume seamlessly.
//! - Caller drives a tokio interval at the silent-chunk cadence
//!   (~21 ms at 48 kHz). On every tick it checks
//!   [`SilenceGenerator::should_emit`]; if true, it calls
//!   [`SilenceGenerator::next_chunk`] and submits the zero-filled
//!   planar PCM to the encoder.
//!
//! The generator is **passive** — it never spawns a task, never holds
//! a timer. All timing decisions live in the caller's `tokio::select!`
//! loop so the silence stream stays synchronised with the rest of the
//! output's packet flow.

use std::time::{Duration, Instant};

/// Default samples per silent chunk. 1024 is the AAC-LC frame size —
/// the most common case. The [`super::audio_encode::AudioEncoder`]
/// accumulates internally so misaligned chunk sizes are harmless.
pub const DEFAULT_CHUNK_SAMPLES: usize = 1024;

/// Default grace window before silence kicks in. A brief gap (≤ this)
/// is absorbed by the encoder's own input buffer and downstream pacing,
/// so we only inject silence once the gap is long enough that a
/// downstream player would actually notice.
pub const DEFAULT_GRACE: Duration = Duration::from_millis(500);

/// Passive silent-PCM generator + audio-drop watchdog.
///
/// See the module-level doc for the intended usage pattern.
pub struct SilenceGenerator {
    sample_rate: u32,
    channels: u8,
    samples_per_chunk: usize,
    zero_planar: Vec<Vec<f32>>,
    /// Running 90 kHz PTS for the next silent chunk we'll emit.
    pts_90k: u64,
    /// `Instant` of the last real audio frame we observed. `None`
    /// means we've never seen one — silence is active from startup.
    last_real_audio_at: Option<Instant>,
    /// How long after the last real audio frame we wait before we
    /// consider the track dropped and start injecting silence.
    grace: Duration,
    /// Whether we're currently inside a silence burst (for logging /
    /// stats transitions).
    emitting: bool,
}

impl SilenceGenerator {
    /// Build a generator at `sample_rate` / `channels`, seeded with
    /// `initial_pts_90k` (use `0` if the output has no prior PTS
    /// anchor). Chunk size defaults to [`DEFAULT_CHUNK_SAMPLES`] and
    /// grace to [`DEFAULT_GRACE`].
    pub fn new(sample_rate: u32, channels: u8, initial_pts_90k: u64) -> Self {
        Self::with_options(
            sample_rate,
            channels,
            initial_pts_90k,
            DEFAULT_CHUNK_SAMPLES,
            DEFAULT_GRACE,
        )
    }

    /// Build a generator with explicit chunk size and grace window.
    /// Zero-channel or zero-sample-rate configs are clamped to the
    /// minimum valid values (2 channels, 48 kHz) so a misconfigured
    /// `audio_encode` block can't panic the generator.
    pub fn with_options(
        sample_rate: u32,
        channels: u8,
        initial_pts_90k: u64,
        samples_per_chunk: usize,
        grace: Duration,
    ) -> Self {
        let sample_rate = if sample_rate == 0 { 48_000 } else { sample_rate };
        let channels = if channels == 0 { 2 } else { channels };
        let samples_per_chunk = samples_per_chunk.max(1);
        let zero_planar: Vec<Vec<f32>> = (0..channels)
            .map(|_| vec![0.0_f32; samples_per_chunk])
            .collect();
        Self {
            sample_rate,
            channels,
            samples_per_chunk,
            zero_planar,
            pts_90k: initial_pts_90k,
            last_real_audio_at: None,
            grace,
            emitting: false,
        }
    }

    /// Tokio interval period for the silence tick. Caller should
    /// drive a `tokio::time::interval(chunk_duration())` loop.
    pub fn chunk_duration(&self) -> Duration {
        Duration::from_nanos(
            (self.samples_per_chunk as u64) * 1_000_000_000 / (self.sample_rate as u64),
        )
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Reset the watchdog after a real audio frame arrived. `pts_90k`
    /// is the frame's presentation timestamp; the generator snaps its
    /// internal PTS counter to it so the next silent chunk (if we
    /// have to emit one later) picks up from the last real PTS.
    pub fn mark_real_audio(&mut self, pts_90k: u64) {
        self.last_real_audio_at = Some(Instant::now());
        self.pts_90k = pts_90k;
        if self.emitting {
            self.emitting = false;
        }
    }

    /// Returns true when the caller should emit a silent chunk this
    /// tick: either no real audio has ever arrived, or the last real
    /// audio is older than `grace`.
    pub fn should_emit(&self) -> bool {
        match self.last_real_audio_at {
            None => true,
            Some(at) => at.elapsed() >= self.grace,
        }
    }

    /// Pull the next silent planar PCM chunk and advance the internal
    /// 90 kHz PTS by one chunk. Returns `(planar, pts_90k)` so the
    /// caller can submit directly to
    /// [`AudioEncoder::submit_planar`](super::audio_encode::AudioEncoder::submit_planar).
    ///
    /// On the first call that actually emits silence after a period
    /// of real audio (or from startup), `emitting` flips to true —
    /// callers can read [`Self::just_started_emitting`] afterwards if
    /// they want to emit a log line / event on the transition.
    pub fn next_chunk(&mut self) -> (&[Vec<f32>], u64) {
        let pts = self.pts_90k;
        // Advance PTS in 90 kHz units: samples * 90_000 / sample_rate.
        let advance = (self.samples_per_chunk as u64) * 90_000 / (self.sample_rate as u64);
        self.pts_90k = self.pts_90k.wrapping_add(advance);
        let transitioning = !self.emitting;
        self.emitting = true;
        // If the caller cares about the transition, they can inspect
        // `emitting` via [`is_emitting`] before/after this call.
        let _ = transitioning;
        (&self.zero_planar, pts)
    }

    /// Whether the generator is currently inside a silence burst.
    pub fn is_emitting(&self) -> bool {
        self.emitting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_filled_planar_matches_channel_count() {
        let g = SilenceGenerator::new(48_000, 2, 0);
        let (planar, _) = clone_next(&mut SilenceGenerator::new(48_000, 2, 0));
        assert_eq!(g.channels() as usize, planar.len());
        for ch in planar {
            assert!(ch.iter().all(|&s| s == 0.0));
            assert_eq!(ch.len(), DEFAULT_CHUNK_SAMPLES);
        }
    }

    #[test]
    fn chunk_duration_is_sample_accurate() {
        let g = SilenceGenerator::new(48_000, 2, 0);
        // 1024 samples @ 48kHz = 21_333_333 ns (21.33 ms).
        assert_eq!(g.chunk_duration(), Duration::from_nanos(21_333_333));
    }

    #[test]
    fn pts_advances_by_one_chunk_in_90khz() {
        let mut g = SilenceGenerator::new(48_000, 2, 100);
        let (_, p0) = g.next_chunk();
        let (_, p1) = g.next_chunk();
        // 1024 * 90000 / 48000 = 1920 per chunk.
        assert_eq!(p0, 100);
        assert_eq!(p1 - p0, 1920);
    }

    #[test]
    fn should_emit_until_first_real_audio() {
        let mut g = SilenceGenerator::new(48_000, 2, 0);
        assert!(g.should_emit(), "no real audio yet → must emit");
        g.mark_real_audio(500_000);
        assert!(!g.should_emit(), "real audio just arrived → suppress");
    }

    #[test]
    fn grace_expiry_reenables_silence() {
        let mut g = SilenceGenerator::with_options(
            48_000,
            2,
            0,
            DEFAULT_CHUNK_SAMPLES,
            Duration::from_millis(5),
        );
        g.mark_real_audio(12345);
        assert!(!g.should_emit());
        std::thread::sleep(Duration::from_millis(10));
        assert!(g.should_emit(), "grace window expired → emit silence");
    }

    #[test]
    fn mark_real_audio_snaps_pts() {
        let mut g = SilenceGenerator::new(48_000, 2, 0);
        g.mark_real_audio(777_000);
        let (_, pts) = g.next_chunk();
        assert_eq!(pts, 777_000, "silent PTS resumes from last real PTS");
    }

    #[test]
    fn zero_channels_clamped_to_stereo() {
        let g = SilenceGenerator::new(48_000, 0, 0);
        assert_eq!(g.channels(), 2);
    }

    #[test]
    fn zero_sample_rate_clamped_to_48k() {
        let g = SilenceGenerator::new(0, 2, 0);
        assert_eq!(g.sample_rate(), 48_000);
    }

    /// Helper: pull one chunk and clone into an owned buffer so the
    /// caller doesn't have to juggle the borrow lifetime.
    fn clone_next(g: &mut SilenceGenerator) -> (Vec<Vec<f32>>, u64) {
        let (planar, pts) = g.next_chunk();
        (planar.to_vec(), pts)
    }
}
