// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! In-process AAC audio decoder.
//!
//! Bridges compressed audio (AAC carried in MPEG-TS / ADTS) into the existing
//! PCM audio pipeline so contribution sources like RTMP, RTSP, and SRT/UDP-TS
//! can land into the PCM-only outputs (ST 2110-30/-31, `rtp_audio`, SMPTE 302M).
//!
//! ## Backends
//!
//! - **`fdk-aac` feature (default)**: Fraunhofer FDK AAC via FFI. Supports
//!   AAC-LC, HE-AAC v1 (SBR), HE-AAC v2 (PS), AAC-LD, AAC-ELD, and
//!   multichannel up to 7.1. This is the recommended backend.
//!
//! - **Fallback (no `fdk-aac` feature)**: `symphonia-codec-aac` (pure Rust).
//!   AAC-LC mono/stereo only. Build with
//!   `--no-default-features --features tls,webrtc` to use this.
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
//! │  ─ decode to PCM     │
//! │  ─ planar f32 out    │
//! └──────────┬───────────┘
//!            ▼
//!  Vec<Vec<f32>> (planar PCM, channel-major)
//! ```
//!
//! Downstream consumers ([`crate::engine::audio_302m::S302mPacketizer::packetize_f32`]
//! for SMPTE 302M, the existing PCM packetizers via [`crate::engine::audio_transcode::TranscodeStage`]
//! for ST 2110 / RTP audio) accept planar f32 directly.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};

// ── Backend-specific imports ────────────────────────────────────────────────

#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::audio::{AudioBufferRef, Channels, Signal, SignalSpec};
#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder, DecoderOptions};
#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::errors::Error as SymphoniaError;
#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::formats::Packet;
#[cfg(not(feature = "fdk-aac"))]
use symphonia::default::codecs::AacDecoder as SymphoniaAacDecoder;

// ── Public types ────────────────────────────────────────────────────────────

/// Errors produced by [`AacDecoder`].
#[derive(Debug)]
pub enum AacDecodeError {
    /// The cached ADTS profile bits indicate something other than AAC-LC.
    /// `profile` is the raw ADTS profile field (0..=3); the corresponding
    /// AOT (Audio Object Type) is `profile + 1`.
    ///
    /// Note: with the `fdk-aac` backend, all profiles are supported and this
    /// error is only returned for truly unknown/reserved profile values.
    UnsupportedProfile { profile: u8, aot: u8 },

    /// The cached `sample_rate_index` does not map to a known sample rate.
    UnsupportedSampleRateIndex(u8),

    /// The cached `channel_config` is outside the supported range.
    /// With `fdk-aac`: 1-7 supported (mono through 7.1).
    /// Without: only 1-2 (mono/stereo).
    UnsupportedChannelConfig(u8),

    /// The underlying fdk-aac decoder returned an error.
    #[cfg(feature = "fdk-aac")]
    FdkAac(aac_codec::AacError),

    /// The underlying symphonia decoder returned an error.
    #[cfg(not(feature = "fdk-aac"))]
    Symphonia(String),
}

impl std::fmt::Display for AacDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AacDecodeError::UnsupportedProfile { profile, aot } => {
                #[cfg(feature = "fdk-aac")]
                write!(
                    f,
                    "unsupported AAC profile: ADTS profile={profile} (AOT={aot})"
                )?;
                #[cfg(not(feature = "fdk-aac"))]
                write!(
                    f,
                    "unsupported AAC profile: ADTS profile={profile} (AOT={aot}). \
                     Only AAC-LC (profile=1, AOT=2) is supported in the pure-Rust build"
                )?;
                Ok(())
            }
            AacDecodeError::UnsupportedSampleRateIndex(idx) => {
                write!(f, "unsupported AAC sample rate index: {idx}")
            }
            AacDecodeError::UnsupportedChannelConfig(c) => {
                #[cfg(feature = "fdk-aac")]
                write!(
                    f,
                    "unsupported AAC channel config: {c} (only 1-7 supported)"
                )?;
                #[cfg(not(feature = "fdk-aac"))]
                write!(
                    f,
                    "unsupported AAC channel config: {c} (only mono and stereo supported)"
                )?;
                Ok(())
            }
            #[cfg(feature = "fdk-aac")]
            AacDecodeError::FdkAac(e) => write!(f, "fdk-aac decoder error: {e}"),
            #[cfg(not(feature = "fdk-aac"))]
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
    /// Number of PCM frame blocks emitted.
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

/// Inverse of [`sample_rate_from_index`]: map an ADTS-known sample rate
/// (Hz) back to its 4-bit index. Returns `None` for non-standard rates.
pub fn sr_index_from_hz(hz: u32) -> Option<u8> {
    Some(match hz {
        96_000 => 0,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        12_000 => 9,
        11_025 => 10,
        8_000 => 11,
        7_350 => 12,
        _ => return None,
    })
}

/// Returns `true` if the given input type is one that can carry MPEG-TS
/// (and therefore potentially AAC audio that we need to decode for PCM
/// outputs).
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

// ══════════════════════════════════════════════════════════════════════════════
// fdk-aac backend (default)
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(feature = "fdk-aac")]
pub struct AacDecoder {
    inner: aac_audio::AacDecoder,
    sample_rate: u32,
    channels: u8,
    next_ts: u64,
}

#[cfg(feature = "fdk-aac")]
impl std::fmt::Debug for AacDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AacDecoder")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("next_ts", &self.next_ts)
            .field("backend", &"fdk-aac")
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "fdk-aac")]
impl AacDecoder {
    /// Construct an AAC decoder from the demuxer-cached ADTS config.
    ///
    /// `cached_aac_config` is the tuple returned by
    /// [`crate::engine::ts_demux::TsDemuxer::cached_aac_config`]:
    /// `(profile, sample_rate_index, channel_config)`.
    ///
    /// With the fdk-aac backend, all standard AAC profiles are supported
    /// (AAC-LC, HE-AAC v1, HE-AAC v2, AAC-LD, AAC-ELD) and multichannel
    /// up to 7.1.
    pub fn from_adts_config(
        profile: u8,
        sample_rate_index: u8,
        channel_config: u8,
    ) -> Result<Self, AacDecodeError> {
        let sample_rate = sample_rate_from_index(sample_rate_index)
            .ok_or(AacDecodeError::UnsupportedSampleRateIndex(sample_rate_index))?;

        if channel_config == 0 || channel_config > 7 {
            return Err(AacDecodeError::UnsupportedChannelConfig(channel_config));
        }

        // Build a 2-byte AudioSpecificConfig from the ADTS fields.
        // The demuxer already stripped the ADTS header, so we use TT_MP4_RAW
        // transport and provide the ASC explicitly.
        let asc = aac_audio::decoder::build_audio_specific_config(
            profile,
            sample_rate_index,
            channel_config,
        );

        let inner = aac_audio::AacDecoder::open_raw(&asc)
            .map_err(AacDecodeError::FdkAac)?;

        // Channel count from channel_config — fdk-aac will confirm after
        // first decode, but we need it for pre-decode callers.
        let channels = match channel_config {
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 5,
            6 => 6,
            7 => 8, // 7.1
            _ => return Err(AacDecodeError::UnsupportedChannelConfig(channel_config)),
        };

        Ok(Self {
            inner,
            sample_rate,
            channels,
            next_ts: 0,
        })
    }

    /// Sample rate of the decoded PCM, in Hz.
    pub fn sample_rate(&self) -> u32 {
        // After first decode, fdk-aac may report a different rate (e.g. SBR
        // upsampling). Use the fdk-aac value if available, otherwise the
        // ADTS-derived value.
        self.inner.sample_rate().unwrap_or(self.sample_rate)
    }

    /// Number of channels in the decoded PCM.
    pub fn channels(&self) -> u8 {
        self.inner.channels().unwrap_or(self.channels)
    }

    /// Decode one ADTS-stripped AAC frame into planar f32 PCM.
    ///
    /// `frame_bytes` is the raw frame contents matching
    /// [`crate::engine::ts_demux::DemuxedFrame::Aac::data`] — the ADTS header
    /// has already been stripped by the demuxer.
    ///
    /// Returns a `Vec<Vec<f32>>` shaped as `[channel][sample]` (planar,
    /// channel-major).
    pub fn decode_frame(&mut self, frame_bytes: &[u8]) -> Result<Vec<Vec<f32>>, AacDecodeError> {
        self.next_ts = self.next_ts.saturating_add(1024);

        let decoded = self.inner.decode_frame(frame_bytes)
            .map_err(AacDecodeError::FdkAac)?;

        // Update cached values from actual decoded stream info
        if let Some(info) = self.inner.stream_info() {
            self.sample_rate = info.sample_rate;
            self.channels = info.channels;
        }

        Ok(decoded.planar)
    }

    /// Human-readable codec name for the decoded stream.
    ///
    /// After the first successful decode, returns the actual detected profile
    /// (e.g. "HE-AAC v1" if SBR was detected in an implicit-signaling stream).
    /// Before the first decode, returns the profile derived from the ADTS header.
    pub fn codec_name(&self) -> &'static str {
        if let Some(info) = self.inner.stream_info() {
            match info.aot {
                2 => "AAC-LC",
                5 => "HE-AAC v1",
                29 => "HE-AAC v2",
                23 => "AAC-LD",
                39 => "AAC-ELD",
                1 => "AAC-Main",
                _ => "AAC",
            }
        } else {
            // Before first decode, use the ADTS-derived profile
            "AAC"
        }
    }

    /// Reset the decoder's internal state without dropping it.
    pub fn reset(&mut self) {
        self.inner.reset();
        self.next_ts = 0;
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// symphonia fallback (when fdk-aac feature is disabled)
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(not(feature = "fdk-aac"))]
pub struct AacDecoder {
    inner: SymphoniaAacDecoder,
    sample_rate: u32,
    channels: u8,
    next_ts: u64,
}

#[cfg(not(feature = "fdk-aac"))]
impl std::fmt::Debug for AacDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AacDecoder")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("next_ts", &self.next_ts)
            .field("backend", &"symphonia")
            .finish_non_exhaustive()
    }
}

#[cfg(not(feature = "fdk-aac"))]
impl AacDecoder {
    /// Construct an AAC-LC decoder from the demuxer-cached ADTS config.
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

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn decode_frame(&mut self, frame_bytes: &[u8]) -> Result<Vec<Vec<f32>>, AacDecodeError> {
        let packet = Packet::new_from_slice(0, self.next_ts, 1024, frame_bytes);
        self.next_ts = self.next_ts.saturating_add(1024);

        let buf_ref = self
            .inner
            .decode(&packet)
            .map_err(|e: SymphoniaError| AacDecodeError::Symphonia(e.to_string()))?;

        Ok(audio_buffer_ref_to_planar_f32(&buf_ref))
    }

    /// Human-readable codec name. Symphonia only supports AAC-LC.
    pub fn codec_name(&self) -> &'static str {
        "AAC-LC"
    }

    pub fn reset(&mut self) {
        self.inner.reset();
        self.next_ts = 0;
    }
}

#[cfg(not(feature = "fdk-aac"))]
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

#[cfg(not(feature = "fdk-aac"))]
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

#[cfg(not(feature = "fdk-aac"))]
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

    // ── Profile rejection tests ─────────────────────────────────────────

    // With fdk-aac: all standard profiles are accepted, only reserved values rejected.
    // Without fdk-aac: only AAC-LC (profile=1) accepted.

    #[cfg(not(feature = "fdk-aac"))]
    #[test]
    fn rejects_aac_main() {
        let err = AacDecoder::from_adts_config(0, 3, 2).unwrap_err();
        match err {
            AacDecodeError::UnsupportedProfile { profile: 0, aot: 1 } => {}
            other => panic!("expected UnsupportedProfile{{profile=0,aot=1}}, got {other:?}"),
        }
    }

    #[cfg(not(feature = "fdk-aac"))]
    #[test]
    fn rejects_aac_ssr() {
        let err = AacDecoder::from_adts_config(2, 3, 2).unwrap_err();
        assert!(matches!(
            err,
            AacDecodeError::UnsupportedProfile { profile: 2, aot: 3 }
        ));
    }

    #[cfg(not(feature = "fdk-aac"))]
    #[test]
    fn rejects_aac_ltp() {
        let err = AacDecoder::from_adts_config(3, 3, 2).unwrap_err();
        assert!(matches!(
            err,
            AacDecodeError::UnsupportedProfile { profile: 3, aot: 4 }
        ));
    }

    #[cfg(feature = "fdk-aac")]
    #[test]
    fn fdk_rejects_aac_main_gracefully() {
        // AAC-Main (profile=0, AOT=1) is not supported by fdk-aac's decoder
        // (only LC, HE-v1, HE-v2, LD, ELD). Verify it returns an error, not a panic.
        let dec = AacDecoder::from_adts_config(0, 3, 2);
        assert!(dec.is_err(), "fdk-aac should reject AAC-Main");
    }

    #[cfg(feature = "fdk-aac")]
    #[test]
    fn accepts_he_aac_v1_with_fdk() {
        // HE-AAC v1 (SBR): ADTS profile=4, AOT=5
        // Note: ADTS profile field is only 2 bits (0-3), so HE-AAC v1
        // typically signals as AAC-LC in ADTS with implicit SBR.
        // The decoder discovers SBR from the bitstream.
        let dec = AacDecoder::from_adts_config(1, 4, 2);
        assert!(dec.is_ok(), "fdk-aac should accept (implicit HE-AAC v1): {dec:?}");
    }

    // ── Common tests ────────────────────────────────────────────────────

    #[test]
    fn rejects_reserved_sample_rate_index() {
        let err = AacDecoder::from_adts_config(1, 13, 2).unwrap_err();
        assert!(matches!(err, AacDecodeError::UnsupportedSampleRateIndex(13)));
    }

    #[cfg(not(feature = "fdk-aac"))]
    #[test]
    fn rejects_multichannel_symphonia() {
        let err = AacDecoder::from_adts_config(1, 3, 6).unwrap_err();
        assert!(matches!(err, AacDecodeError::UnsupportedChannelConfig(6)));
    }

    #[cfg(feature = "fdk-aac")]
    #[test]
    fn accepts_multichannel_fdk() {
        // 5.1 (channel_config=6) is supported by fdk-aac
        let dec = AacDecoder::from_adts_config(1, 3, 6);
        assert!(dec.is_ok(), "fdk-aac should accept 5.1: {dec:?}");
    }

    #[cfg(feature = "fdk-aac")]
    #[test]
    fn rejects_invalid_channel_config_fdk() {
        // channel_config=0 (program_config_element, unsupported in this API)
        let err = AacDecoder::from_adts_config(1, 3, 0).unwrap_err();
        assert!(matches!(err, AacDecodeError::UnsupportedChannelConfig(0)));
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

    #[test]
    fn decode_garbage_returns_error_not_panic() {
        let mut dec = AacDecoder::from_adts_config(1, 3, 2).unwrap();
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF, 0x55, 0xAA];
        let result = dec.decode_frame(&garbage);
        // We don't care whether it succeeds or fails, only that it does not
        // panic. Most malformed inputs will return Err.
        let _ = result;
    }
}
