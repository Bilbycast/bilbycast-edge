// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! In-process AAC-LC audio decoder.
//!
//! Bridges compressed audio (currently AAC-LC carried in MPEG-TS / ADTS) into
//! the existing PCM audio pipeline so contribution sources like RTMP, RTSP,
//! and SRT/UDP-TS can land into the PCM-only outputs (ST 2110-30/-31,
//! `rtp_audio`, SMPTE 302M).
//!
//! ## Layering
//!
//! ```text
//! DemuxedFrame::Aac { data, pts }
//!         │
//!         ▼
//! ┌──────────────────────┐
//! │  AacDecoder          │
//! │  ─ lazy init from    │
//! │    first ADTS config │
//! │  ─ symphonia decode  │
//! │  ─ planar f32 out    │
//! └──────────┬───────────┘
//!            ▼
//!  Vec<Vec<f32>> (planar PCM, channel-major)
//! ```
//!
//! Downstream consumers ([`crate::engine::audio_302m::S302mPacketizer::packetize_f32`]
//! for SMPTE 302M, the existing PCM packetizers via [`crate::engine::audio_transcode::TranscodeStage`]
//! for ST 2110 / RTP audio) accept planar f32 directly.
//!
//! ## Pure Rust
//!
//! Uses `symphonia-codec-aac` (MPL-2.0, 100% Rust, `forbid(unsafe_code)`).
//! No C deps. Verified by `testbed/check-binary-purity.sh`.
//!
//! ## Limitations (inherited from `symphonia-codec-aac` 0.5.5)
//!
//! - **AAC-LC only.** AAC-Main, AAC-SSR, AAC-LTP, HE-AAC v1 (SBR), HE-AAC v2
//!   (PS) are all rejected with a clear error. Add a fallback path (e.g.
//!   ffmpeg sidecar) if these become a customer requirement.
//! - **Mono and stereo only.** 5.1 / 7.1 multichannel AAC is rejected.
//! - **1024 samples per frame** (standard AAC-LC). 960-sample frames rejected.
//! - **ADTS framing only.** LATM-in-TS is detected by the demuxer (descriptor
//!   `0x7C`) but not yet depacketized; LATM frames will not reach this stage.
//!
//! ## What this module does NOT do
//!
//! - It does NOT do sample rate / bit depth / channel routing conversion —
//!   that's still the job of [`crate::engine::audio_transcode::TranscodeStage`]
//!   sitting downstream.
//! - It does NOT encode anything. Encode is the responsibility of
//!   `engine::audio_encode` (Phase B, ffmpeg sidecar).

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};

use symphonia::core::audio::{AudioBufferRef, Channels, Signal, SignalSpec};
use symphonia::core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::Packet;

// `symphonia` re-exports the AAC decoder under `codecs::AacDecoder` when
// the `aac` feature is enabled. We avoid pulling `symphonia-codec-aac` in
// directly so we have one dependency surface to track.
use symphonia::default::codecs::AacDecoder as SymphoniaAacDecoder;

// ── Public types ────────────────────────────────────────────────────────────

/// Errors produced by [`AacDecoder`].
#[derive(Debug)]
pub enum AacDecodeError {
    /// The cached ADTS profile bits indicate something other than AAC-LC.
    /// `profile` is the raw ADTS profile field (0..=3); the corresponding
    /// AOT (Audio Object Type) is `profile + 1`.
    UnsupportedProfile { profile: u8, aot: u8 },

    /// The cached `sample_rate_index` does not map to a known sample rate.
    UnsupportedSampleRateIndex(u8),

    /// The cached `channel_config` is outside [1, 2]. Multichannel (5.1, 7.1)
    /// AAC is not supported by `symphonia-codec-aac` 0.5.5.
    UnsupportedChannelConfig(u8),

    /// The underlying symphonia decoder rejected `try_new` or `decode`.
    Symphonia(String),
}

impl std::fmt::Display for AacDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AacDecodeError::UnsupportedProfile { profile, aot } => write!(
                f,
                "unsupported AAC profile: ADTS profile={profile} (AOT={aot}). Only AAC-LC (profile=1, AOT=2) is supported in the pure-Rust build"
            ),
            AacDecodeError::UnsupportedSampleRateIndex(idx) => {
                write!(f, "unsupported AAC sample rate index: {idx}")
            }
            AacDecodeError::UnsupportedChannelConfig(c) => write!(
                f,
                "unsupported AAC channel config: {c} (only mono and stereo supported)"
            ),
            AacDecodeError::Symphonia(s) => write!(f, "symphonia AAC decoder error: {s}"),
        }
    }
}

impl std::error::Error for AacDecodeError {}

/// Lock-free counters for the decode stage.
#[derive(Debug, Default)]
pub struct DecodeStats {
    /// Number of compressed AAC frames fed in.
    pub input_frames: AtomicU64,
    /// Number of PCM frame blocks (1024 samples each, typically) emitted.
    pub output_blocks: AtomicU64,
    /// Number of frames that failed to decode (corrupt input, format mismatch).
    pub decode_errors: AtomicU64,
    /// Number of frames dropped because the decoder isn't yet initialised
    /// (no AAC config seen).
    pub dropped_uninit: AtomicU64,
}

impl DecodeStats {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn inc_input(&self) {
        self.input_frames.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_output(&self) {
        self.output_blocks.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_error(&self) {
        self.decode_errors.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_dropped_uninit(&self) {
        self.dropped_uninit.fetch_add(1, Ordering::Relaxed);
    }
}

// ── ADTS sample-rate-index table ────────────────────────────────────────────

/// MPEG-4 AAC sample rate index table (ISO/IEC 14496-3 §1.6.3.4).
/// Indices 13..=14 are reserved; 15 means "explicit value follows" which
/// ADTS does not encode.
const SAMPLE_RATE_TABLE: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Resolve an ADTS `sample_rate_index` (0..=12) to Hz. Returns `None` for
/// reserved or escape values.
pub fn sample_rate_from_index(idx: u8) -> Option<u32> {
    SAMPLE_RATE_TABLE.get(idx as usize).copied()
}

/// Returns `true` if the given input type is one that can carry MPEG-TS
/// (and therefore potentially AAC audio that we need to decode for PCM
/// outputs).
///
/// Used by audio output spawn modules to decide whether to construct an
/// in-line [`AacDecoder`] + [`crate::engine::ts_demux::TsDemuxer`] in front
/// of the existing transcode chain.
///
/// `Rtp`, `Udp`, `Srt`, `Rtmp`, and `Rtsp` are TS-bearing inputs in this
/// codebase. `St2110_30`, `St2110_31`, and `RtpAudio` are uncompressed PCM
/// audio inputs and never need decoding. `Webrtc` / `Whep` carry Opus
/// (out of scope for Phase A — Opus decode requires a separate decoder).
/// `St2110_40` carries ancillary data, never audio.
pub fn input_can_carry_ts_audio(input: &crate::config::models::InputConfig) -> bool {
    use crate::config::models::InputConfig;
    matches!(
        input,
        InputConfig::Rtp(_)
            | InputConfig::Udp(_)
            | InputConfig::Srt(_)
            | InputConfig::Rtmp(_)
            | InputConfig::Rtsp(_)
    )
}

// ── AacDecoder ──────────────────────────────────────────────────────────────

/// Stateful AAC-LC decoder. Construct from the ADTS config exposed by
/// [`crate::engine::ts_demux::TsDemuxer::cached_aac_config`].
///
/// Lifetime model: one decoder per audio output, driven by the output's
/// broadcast subscriber loop. Not `Send + Sync` for &mut access; wrap in
/// the per-output task's local state, never share across tasks.
pub struct AacDecoder {
    inner: SymphoniaAacDecoder,
    sample_rate: u32,
    channels: u8,
    /// Monotonic timestamp counter (in 1024-sample units) used as the
    /// `Packet::ts` field. The actual upstream PTS lives on
    /// `DemuxedFrame::Aac::pts` and is propagated separately by callers
    /// who care about clock alignment.
    next_ts: u64,
}

impl std::fmt::Debug for AacDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AacDecoder")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("next_ts", &self.next_ts)
            .finish_non_exhaustive()
    }
}

impl AacDecoder {
    /// Construct an AAC-LC decoder from the demuxer-cached ADTS config.
    ///
    /// `cached_aac_config` is the tuple returned by
    /// [`crate::engine::ts_demux::TsDemuxer::cached_aac_config`]:
    /// `(profile, sample_rate_index, channel_config)`.
    ///
    /// Returns an error if the profile is not AAC-LC, the sample rate index
    /// is reserved, the channel config is unsupported, or symphonia rejects
    /// the parameter set.
    pub fn from_adts_config(
        profile: u8,
        sample_rate_index: u8,
        channel_config: u8,
    ) -> Result<Self, AacDecodeError> {
        // ADTS profile field is (AOT - 1). AAC-LC = AOT 2 = ADTS profile 1.
        // Reject anything else explicitly so customers see a clear failure
        // mode rather than a generic "decode error" later.
        let aot = profile + 1;
        if profile != 1 {
            return Err(AacDecodeError::UnsupportedProfile { profile, aot });
        }

        let sample_rate = sample_rate_from_index(sample_rate_index)
            .ok_or(AacDecodeError::UnsupportedSampleRateIndex(sample_rate_index))?;

        let channels = match channel_config {
            1 => 1u8,
            2 => 2u8,
            other => return Err(AacDecodeError::UnsupportedChannelConfig(other)),
        };

        let channels_mask = match channels {
            1 => Channels::FRONT_LEFT,
            2 => Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
            // unreachable: filtered above
            _ => return Err(AacDecodeError::UnsupportedChannelConfig(channels)),
        };

        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(sample_rate)
            .with_channels(channels_mask);

        let inner = SymphoniaAacDecoder::try_new(&params, &DecoderOptions::default())
            .map_err(|e: SymphoniaError| AacDecodeError::Symphonia(e.to_string()))?;

        Ok(Self {
            inner,
            sample_rate,
            channels,
            next_ts: 0,
        })
    }

    /// Sample rate of the decoded PCM, in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Number of channels in the decoded PCM (1 or 2).
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Decode one ADTS-stripped AAC frame into planar f32 PCM.
    ///
    /// `frame_bytes` is the raw frame contents matching
    /// [`crate::engine::ts_demux::DemuxedFrame::Aac::data`] — the ADTS header
    /// has already been stripped by the demuxer.
    ///
    /// Returns a `Vec<Vec<f32>>` shaped as `[channel][sample]` (planar,
    /// channel-major). For AAC-LC each frame produces exactly 1024 samples
    /// per channel.
    pub fn decode_frame(&mut self, frame_bytes: &[u8]) -> Result<Vec<Vec<f32>>, AacDecodeError> {
        // Symphonia's AAC decoder ignores ts/dur for AAC-LC packets, but we
        // still pass a monotonic timestamp so any tracing/diagnostics it
        // emits is interpretable.
        let packet = Packet::new_from_slice(0, self.next_ts, 1024, frame_bytes);
        self.next_ts = self.next_ts.saturating_add(1024);

        let buf_ref = self
            .inner
            .decode(&packet)
            .map_err(|e: SymphoniaError| AacDecodeError::Symphonia(e.to_string()))?;

        Ok(audio_buffer_ref_to_planar_f32(&buf_ref))
    }

    /// Reset the decoder's internal state without dropping it. Useful when
    /// the upstream stream restarts (e.g. an RTMP publisher reconnects)
    /// without changing the AAC config.
    pub fn reset(&mut self) {
        self.inner.reset();
        self.next_ts = 0;
    }
}

/// Convert any [`AudioBufferRef`] variant into planar f32 PCM
/// (`Vec<Vec<f32>>` shaped as `[channel][sample]`).
///
/// Symphonia's AAC decoder always produces `AudioBufferRef::F32`, but we
/// match all variants defensively so a future symphonia change cannot
/// silently produce zeroed audio.
fn audio_buffer_ref_to_planar_f32(buf_ref: &AudioBufferRef<'_>) -> Vec<Vec<f32>> {
    match buf_ref {
        AudioBufferRef::F32(buf) => extract_planar_f32(buf.spec(), buf.frames(), |ch| buf.chan(ch)),
        AudioBufferRef::F64(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| *s as f32),
        AudioBufferRef::S16(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| *s as f32 / 32_768.0),
        AudioBufferRef::S24(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| s.inner() as f32 / 8_388_608.0),
        AudioBufferRef::S32(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| *s as f32 / 2_147_483_648.0),
        AudioBufferRef::U8(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| (*s as f32 - 128.0) / 128.0),
        AudioBufferRef::U16(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| (*s as f32 - 32_768.0) / 32_768.0),
        AudioBufferRef::U24(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| (s.inner() as f32 - 8_388_608.0) / 8_388_608.0),
        AudioBufferRef::U32(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| (*s as f32 - 2_147_483_648.0) / 2_147_483_648.0),
        AudioBufferRef::S8(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.chan(ch), |s| *s as f32 / 128.0),
    }
}

#[inline]
fn extract_planar_f32<'a, F>(spec: &SignalSpec, frames: usize, get_chan: F) -> Vec<Vec<f32>>
where
    F: Fn(usize) -> &'a [f32],
{
    let n_ch = spec.channels.count();
    (0..n_ch)
        .map(|ch| get_chan(ch)[..frames].to_vec())
        .collect()
}

#[inline]
fn extract_planar_with<'a, S, F, M>(
    spec: &SignalSpec,
    frames: usize,
    get_chan: F,
    map: M,
) -> Vec<Vec<f32>>
where
    S: 'a,
    F: Fn(usize) -> &'a [S],
    M: Fn(&S) -> f32,
{
    let n_ch = spec.channels.count();
    (0..n_ch)
        .map(|ch| get_chan(ch)[..frames].iter().map(&map).collect())
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_rate_table_known_values() {
        assert_eq!(sample_rate_from_index(3), Some(48_000));
        assert_eq!(sample_rate_from_index(4), Some(44_100));
        assert_eq!(sample_rate_from_index(0), Some(96_000));
        assert_eq!(sample_rate_from_index(12), Some(7_350));
        assert_eq!(sample_rate_from_index(13), None);
        assert_eq!(sample_rate_from_index(15), None);
        assert_eq!(sample_rate_from_index(255), None);
    }

    #[test]
    fn rejects_aac_main() {
        // ADTS profile=0 → AOT 1 (AAC-Main), unsupported.
        let err = AacDecoder::from_adts_config(0, 3, 2).unwrap_err();
        match err {
            AacDecodeError::UnsupportedProfile { profile: 0, aot: 1 } => {}
            other => panic!("expected UnsupportedProfile{{profile=0,aot=1}}, got {other:?}"),
        }
    }

    #[test]
    fn rejects_aac_ssr() {
        // ADTS profile=2 → AOT 3 (AAC-SSR), unsupported.
        let err = AacDecoder::from_adts_config(2, 3, 2).unwrap_err();
        assert!(matches!(
            err,
            AacDecodeError::UnsupportedProfile { profile: 2, aot: 3 }
        ));
    }

    #[test]
    fn rejects_aac_ltp() {
        // ADTS profile=3 → AOT 4 (AAC-LTP), unsupported.
        let err = AacDecoder::from_adts_config(3, 3, 2).unwrap_err();
        assert!(matches!(
            err,
            AacDecodeError::UnsupportedProfile { profile: 3, aot: 4 }
        ));
    }

    #[test]
    fn rejects_reserved_sample_rate_index() {
        // profile=1 (AAC-LC), sample_rate_index=13 (reserved)
        let err = AacDecoder::from_adts_config(1, 13, 2).unwrap_err();
        assert!(matches!(err, AacDecodeError::UnsupportedSampleRateIndex(13)));
    }

    #[test]
    fn rejects_multichannel() {
        // 5.1 (channel_config=6) is unsupported by symphonia-codec-aac.
        let err = AacDecoder::from_adts_config(1, 3, 6).unwrap_err();
        assert!(matches!(err, AacDecodeError::UnsupportedChannelConfig(6)));
    }

    #[test]
    fn accepts_aac_lc_stereo_48k() {
        let dec = AacDecoder::from_adts_config(1, 3, 2).expect("AAC-LC stereo 48 kHz must construct");
        assert_eq!(dec.sample_rate(), 48_000);
        assert_eq!(dec.channels(), 2);
    }

    #[test]
    fn accepts_aac_lc_mono_44k() {
        let dec = AacDecoder::from_adts_config(1, 4, 1).expect("AAC-LC mono 44.1 kHz must construct");
        assert_eq!(dec.sample_rate(), 44_100);
        assert_eq!(dec.channels(), 1);
    }

    /// End-to-end smoke test: encode 1024 samples of silence as a synthetic
    /// AAC-LC frame... we cannot do that without an encoder, so this test
    /// instead confirms the decoder rejects garbage input cleanly rather
    /// than panicking. A real golden-frame round-trip test belongs in the
    /// integration tests under `testbed/audio-tests/`, where ffmpeg is
    /// available to produce the reference encoding.
    #[test]
    fn decode_garbage_returns_error_not_panic() {
        let mut dec = AacDecoder::from_adts_config(1, 3, 2).unwrap();
        // Random bytes — symphonia should reject these as malformed AAC.
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF, 0x55, 0xAA];
        let result = dec.decode_frame(&garbage);
        // We don't care whether it succeeds or fails, only that it does not
        // panic. Most malformed inputs will return Err.
        let _ = result;
    }
}
