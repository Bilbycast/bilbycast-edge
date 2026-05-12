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

// ── Non-AAC PES helpers (MP2 / AC-3 / E-AC-3) ───────────────────────────────
//
// MP2 (stream_type 0x03/0x04), AC-3 (0x80/0x81/0xC1), and E-AC-3 (0x87/0xC2)
// PES payloads are concatenated codec frames with no in-band length field
// you can trust across every variant. The decode path is: walk the PES,
// slice at every sync word, and call `avcodec_send_packet` once per frame.
// Feeding the whole PES at once silently drops everything past the first
// access unit (`avcodec_send_packet` decodes one AU per call).

/// Map an MPEG-TS `stream_type` to the FFmpeg-backed audio decoder enum,
/// or `None` for codecs that aren't routed through libavcodec (AAC has
/// its own fdk-aac path).
///
/// Opus rides on `stream_type = 0x06` (`STREAM_TYPE_PRIVATE`) but only
/// after the demuxer has confirmed the registration descriptor. The
/// caller knows whether the PID is Opus (it routes Opus-bearing PES via
/// `DemuxedFrame::Opus`, AC-3-bearing PES via `DemuxedFrame::OtherAudio`),
/// so the `0x06` arm here only fires on the Opus path — AC-3 carried on
/// `0x06 + 0x6A descriptor` is synthesised as `0x81` upstream so it
/// matches the AC-3 arm below.
#[cfg(feature = "video-thumbnail")]
pub fn ff_codec_for_stream_type(stream_type: u8) -> Option<video_codec::AudioDecoderCodec> {
    use video_codec::AudioDecoderCodec;
    match stream_type {
        0x03 | 0x04 => Some(AudioDecoderCodec::Mp2),
        0x80 | 0x81 | 0xC1 => Some(AudioDecoderCodec::Ac3),
        0x87 | 0xC2 => Some(AudioDecoderCodec::Eac3),
        0x06 => Some(AudioDecoderCodec::Opus),
        0x11 => Some(AudioDecoderCodec::AacLatm),
        _ => None,
    }
}

/// Split a concatenated codec-frame buffer so each `avcodec_send_packet`
/// sees exactly one access unit.
///
/// **MP2 / MPEG-1 layer audio.** Sync is the 12-bit pattern `0xFFF` (byte0
/// = `0xFF`, top 4 bits of byte1 = `0xF`). The legacy 11-bit `0xFFE` prefix
/// is *part of* the MP2 sync word but also matches MPEG-2.5 (a reserved
/// PES context here) and — critically — collides with `0xFF` bytes inside
/// the body of an MP2 frame, which is what the
/// `bilbycast/testbed/quality/display-tests` matrix surfaced as 5–10 % of
/// MP2 frames being rejected by libavcodec with `Header missing`. Slice
/// each frame using the exact `frame_size` derived from the bitrate +
/// sample-rate fields — only frames the parser can compute the size for
/// are emitted, so libavcodec gets sync-aligned access units every time.
///
/// **AC-3 / E-AC-3.** Sync is `0x0B 0x77`. AC-3 + E-AC-3 frames don't
/// embed a payload-length we can trust as cheaply as MP2 does, so we keep
/// the "scan to next sync word" splitter — libavcodec's AC-3 decoder is
/// tolerant of the trailing-byte ambiguity.
///
/// **Opus** carriage in MPEG-TS prepends each Opus access unit with a
/// `control_header_prefix` (0x3FF) + flag-gated optional fields + an
/// extensible `au_size` byte count for the raw Opus packet that follows.
/// `split_opus_frames` strips the wrapper and yields raw Opus packets the
/// libavcodec Opus decoder consumes directly. Wired here so any output
/// with `audio_encode` set on an Opus-in-TS source path can decode →
/// re-encode end-to-end (previously this returned `Vec::new()` and the
/// audio silently disappeared).
#[cfg(feature = "video-thumbnail")]
pub fn split_audio_codec_frames(
    buf: &[u8],
    codec: video_codec::AudioDecoderCodec,
) -> Vec<&[u8]> {
    use video_codec::AudioDecoderCodec;
    if buf.is_empty() {
        return Vec::new();
    }
    match codec {
        AudioDecoderCodec::Mp2 => split_mp2_frames(buf),
        AudioDecoderCodec::Ac3 | AudioDecoderCodec::Eac3 => split_ac3_frames(buf),
        AudioDecoderCodec::Opus => split_opus_frames(buf),
        AudioDecoderCodec::AacLatm => split_loas_frames(buf),
    }
}

/// LOAS / LATM frame splitter for AAC carried with `stream_type=0x11`.
///
/// LOAS (ISO/IEC 14496-3 § 1.7.3 — "audioSyncStream") prefixes each
/// LATM `AudioMuxElement` with:
///
/// ```text
///   syncword           : 11 bits = 0x2B7
///   audioMuxLengthBytes: 13 bits  ← number of bytes that follow
///   payload            : audioMuxLengthBytes bytes (LATM AudioMuxElement)
/// ```
///
/// Each emitted slice is **header + payload** (i.e. a complete 3-byte
/// LOAS frame followed by the LATM payload) — that's the byte
/// sequence libavcodec's `AAC_LATM` decoder consumes. Malformed
/// frames trigger a single-byte resync to the next valid `0x2B7` sync.
#[cfg(feature = "video-thumbnail")]
pub fn split_loas_frames(buf: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::with_capacity(8);
    let mut i = 0;
    while i + 3 <= buf.len() {
        // 11-bit sync word `0x2B7`:
        //   byte[i]      = 0x56  (`0b0101 0110`)  ← top 8 bits of sync
        //   byte[i+1]>>5 = 0b111                  ← remaining 3 bits
        if buf[i] != 0x56 || (buf[i + 1] & 0xE0) != 0xE0 {
            i += 1;
            continue;
        }
        // 13-bit length: bottom 5 bits of byte[i+1] | byte[i+2].
        let frame_payload_len =
            (((buf[i + 1] & 0x1F) as usize) << 8) | (buf[i + 2] as usize);
        let total_len = 3 + frame_payload_len;
        if frame_payload_len == 0 || i + total_len > buf.len() {
            // Truncated tail or empty length — leave the slice for the
            // next PES rather than emitting a partial frame to libavcodec.
            break;
        }
        out.push(&buf[i..i + total_len]);
        i += total_len;
    }
    out
}

/// MPEG-1 layer 2 / 1 frame splitter that honours the `frame_size`
/// computed from the header instead of scanning for the next sync — the
/// scanning approach trips on `0xFF` bytes inside the audio payload and
/// produces misaligned slices.
#[cfg(feature = "video-thumbnail")]
fn split_mp2_frames(buf: &[u8]) -> Vec<&[u8]> {
    /// MPEG-1 layer II bitrate index (kbps) — ISO/IEC 11172-3 § 2.4.2.3,
    /// table B.211. Index 0 = "free format", index 15 = "bad". We treat
    /// both as unparseable and resync on the next valid header.
    const MP1_L2_BITRATES: [u32; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    /// MPEG-1 layer I bitrate index (kbps).
    const MP1_L1_BITRATES: [u32; 15] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    /// MPEG-1 sample-rate index (Hz).
    const MP1_SAMPLE_RATES: [u32; 3] = [44_100, 48_000, 32_000];

    let mut out: Vec<&[u8]> = Vec::with_capacity(8);
    let mut i = 0;
    while i + 4 <= buf.len() {
        // 12-bit sync `0xFFF` — byte 0 == 0xFF and bits [7:4] of byte 1
        // are all 1. The full MPEG-1 header is 32 bits; we need byte 2
        // for bitrate + sample-rate.
        if buf[i] != 0xFF || (buf[i + 1] & 0xF0) != 0xF0 {
            i += 1;
            continue;
        }
        let header2 = buf[i + 1];
        let header3 = buf[i + 2];
        // MPEG version: bits [4:3] of byte 1. `11` = MPEG-1 (the only
        // version DVB / ATSC carry as MP2). Anything else: skip and
        // resync — MPEG-2 layer II uses a different bitrate table and we
        // don't see it on broadcast.
        let version_id = (header2 >> 3) & 0x03;
        if version_id != 0b11 {
            i += 1;
            continue;
        }
        // Layer: bits [2:1] of byte 1. `10` = layer II, `11` = layer I.
        let layer = (header2 >> 1) & 0x03;
        let bitrate_idx = ((header3 >> 4) & 0x0F) as usize;
        let sr_idx = ((header3 >> 2) & 0x03) as usize;
        let padding = ((header3 >> 1) & 0x01) as u32;
        if bitrate_idx == 0 || bitrate_idx == 15 || sr_idx == 3 {
            i += 1;
            continue;
        }
        let sr = MP1_SAMPLE_RATES[sr_idx];
        // frame_size = floor(samples_per_frame * bitrate / sample_rate) + padding
        // Layer I: 384 samples, slot = 4 bytes, so frame = (12 * br / sr + pad) * 4.
        // Layer II: 1152 samples, slot = 1 byte, so frame = 144 * br / sr + pad.
        let frame_size = match layer {
            0b10 => {
                // Layer II
                let br = MP1_L2_BITRATES[bitrate_idx] * 1000;
                144 * br / sr + padding
            }
            0b11 => {
                // Layer I
                let br = MP1_L1_BITRATES[bitrate_idx] * 1000;
                (12 * br / sr + padding) * 4
            }
            _ => {
                i += 1;
                continue;
            }
        } as usize;
        if frame_size < 4 || i + frame_size > buf.len() {
            // Truncated tail — leave it for the next PES to resync. We
            // deliberately *do not* emit a partial frame to libavcodec;
            // that's what was producing `Header missing` on the matrix.
            break;
        }
        out.push(&buf[i..i + frame_size]);
        i += frame_size;
    }
    out
}

/// Walk an Opus-in-MPEG-TS PES payload and emit one slice per Opus
/// access unit. Opus carriage in MPEG-TS prepends every Opus frame with
/// a control header (`control_header_prefix = 0x3FF`, 11 bits of `1`,
/// followed by `start_trim_flag` / `end_trim_flag` /
/// `control_extension_flag`, an extensible `au_size` length field, and
/// the optional control fields gated by the flags). The libopus decoder
/// expects raw Opus packets, so the splitter strips the control header
/// and yields the `au_size` bytes that follow.
///
/// Best-effort — malformed AUs cause a resync on the next valid prefix
/// rather than aborting the PES.
#[cfg(feature = "video-thumbnail")]
pub fn split_opus_frames(buf: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::with_capacity(2);
    let mut i = 0;
    while i + 2 <= buf.len() {
        // 11-bit control_header_prefix = 0x3FF (= 11 bits all 1):
        //   byte[i]       = 0xFF
        //   byte[i+1]>>5  = 0b111
        if buf[i] != 0xFF || (buf[i + 1] & 0xE0) != 0xE0 {
            i += 1;
            continue;
        }
        let flags = buf[i + 1] & 0x1F;
        let start_trim_flag = (flags & 0x10) != 0;
        let end_trim_flag = (flags & 0x08) != 0;
        let control_extension_flag = (flags & 0x04) != 0;

        // au_size: variable-length unsigned. Each 0xFF byte adds 255 and
        // continues; the first non-0xFF byte adds its raw value and ends
        // the field.
        let mut size_pos = i + 2;
        let mut au_size: usize = 0;
        loop {
            if size_pos >= buf.len() {
                return out;
            }
            let b = buf[size_pos];
            size_pos += 1;
            au_size = au_size.saturating_add(b as usize);
            if b != 0xFF {
                break;
            }
        }

        // Optional fields (gated by flags) live *inside* `au_size`. Skip
        // them so the slice we emit is the Opus packet itself.
        let mut payload_start = size_pos;
        let mut consumed_optional: usize = 0;
        if start_trim_flag {
            if payload_start + 2 > buf.len() {
                return out;
            }
            payload_start += 2;
            consumed_optional += 2;
        }
        if end_trim_flag {
            if payload_start + 2 > buf.len() {
                return out;
            }
            payload_start += 2;
            consumed_optional += 2;
        }
        if control_extension_flag {
            if payload_start >= buf.len() {
                return out;
            }
            let ext_len = buf[payload_start] as usize;
            payload_start += 1;
            consumed_optional += 1;
            if payload_start + ext_len > buf.len() {
                return out;
            }
            payload_start += ext_len;
            consumed_optional += ext_len;
        }

        let opus_packet_size = au_size.saturating_sub(consumed_optional);
        let payload_end = payload_start + opus_packet_size;
        if opus_packet_size == 0 || payload_end > buf.len() {
            return out;
        }
        out.push(&buf[payload_start..payload_end]);
        i = payload_end;
    }
    out
}

/// AC-3 / E-AC-3 header-aware splitter.
///
/// A naive `0x0B 0x77` scan trips on the ~1–3 % of AC-3 frames whose
/// payload happens to contain that two-byte pattern, producing phantom
/// AUs that the decoder either drops or decodes into corrupt PCM —
/// either way the downstream PTS bookkeeping in
/// [`crate::engine::ts_audio_replace`] sees an extra decoded frame and
/// the output audio stream emits duplicate / glitched PTS values.
///
/// This walker validates each candidate sync by parsing the syncinfo
/// header to compute the real frame size (AC-3 via `frmsizecod` lookup,
/// E-AC-3 via the 11-bit `frmsiz` field), discriminating between the
/// two by `bsid` (AC-3: 0..=10, E-AC-3: 11..=16). Last-frame truncation
/// is dropped — the next PES will re-resync — matching
/// [`split_mp2_frames`]'s contract.
#[cfg(feature = "video-thumbnail")]
fn split_ac3_frames(buf: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::with_capacity(8);
    let mut i = 0;
    while i + AC3_MIN_HEADER_BYTES <= buf.len() {
        if buf[i] != 0x0B || buf[i + 1] != 0x77 {
            i += 1;
            continue;
        }
        let frame_bytes = match ac3_frame_size(&buf[i..]) {
            Some(n) => n,
            None => {
                i += 1;
                continue;
            }
        };
        if frame_bytes < AC3_MIN_HEADER_BYTES || i + frame_bytes > buf.len() {
            // Truncated tail — leave it for the next PES to resync on.
            break;
        }
        out.push(&buf[i..i + frame_bytes]);
        i += frame_bytes;
    }
    out
}

/// Smallest syncinfo we can parse: 16 bits syncword + bsi bytes through
/// the `bsid` field (byte 5). Anything shorter is treated as a sync
/// candidate that we can't validate yet — caller falls through.
const AC3_MIN_HEADER_BYTES: usize = 6;

/// AC-3 frame size table from ATSC A/52 § 5.4.1.4 Table 5.18 — entries
/// are in 16-bit words; the caller multiplies by 2 to get bytes. Indexed
/// by `frmsizecod` (0..=37); inner index is the sample-rate code
/// (0 = 48 kHz, 1 = 44.1 kHz, 2 = 32 kHz). `fscod = 0b11` is reserved
/// and bails out before this table is consulted.
#[cfg(feature = "video-thumbnail")]
const AC3_FRMSIZ_WORDS: [[u16; 3]; 38] = [
    [64, 69, 96],     // 32 kbps
    [64, 70, 96],
    [80, 87, 120],    // 40 kbps
    [80, 88, 120],
    [96, 104, 144],   // 48 kbps
    [96, 105, 144],
    [112, 121, 168],  // 56 kbps
    [112, 122, 168],
    [128, 139, 192],  // 64 kbps
    [128, 140, 192],
    [160, 174, 240],  // 80 kbps
    [160, 175, 240],
    [192, 208, 288],  // 96 kbps
    [192, 209, 288],
    [224, 243, 336],  // 112 kbps
    [224, 244, 336],
    [256, 278, 384],  // 128 kbps
    [256, 279, 384],
    [320, 348, 480],  // 160 kbps
    [320, 349, 480],
    [384, 417, 576],  // 192 kbps
    [384, 418, 576],
    [448, 487, 672],  // 224 kbps
    [448, 488, 672],
    [512, 557, 768],  // 256 kbps
    [512, 558, 768],
    [640, 696, 960],  // 320 kbps
    [640, 697, 960],
    [768, 835, 1152], // 384 kbps
    [768, 836, 1152],
    [896, 975, 1344], // 448 kbps
    [896, 976, 1344],
    [1024, 1114, 1536], // 512 kbps
    [1024, 1115, 1536],
    [1152, 1253, 1728], // 576 kbps
    [1152, 1254, 1728],
    [1280, 1393, 1920], // 640 kbps
    [1280, 1394, 1920],
];

/// Parse the AC-3 / E-AC-3 syncinfo at the start of `buf` and return the
/// total frame length in bytes. Returns `None` if the header is invalid
/// (reserved `fscod`, out-of-range `frmsizecod`, or reserved `bsid`),
/// which signals the caller to step one byte and resync.
#[cfg(feature = "video-thumbnail")]
fn ac3_frame_size(buf: &[u8]) -> Option<usize> {
    if buf.len() < AC3_MIN_HEADER_BYTES {
        return None;
    }
    if buf[0] != 0x0B || buf[1] != 0x77 {
        return None;
    }
    // `bsid` lives at the same bit position (40..=44) in both AC-3 and
    // E-AC-3 — the two formats differ in what precedes it but the
    // BSID byte is byte 5 regardless.
    let bsid = buf[5] >> 3;
    if bsid <= 10 {
        // AC-3: byte 4 = fscod(2) || frmsizecod(6).
        let byte4 = buf[4];
        let fscod = (byte4 >> 6) & 0x03;
        let frmsizecod = (byte4 & 0x3F) as usize;
        if fscod == 0b11 || frmsizecod >= AC3_FRMSIZ_WORDS.len() {
            return None;
        }
        let words = AC3_FRMSIZ_WORDS[frmsizecod][fscod as usize] as usize;
        Some(words * 2)
    } else if bsid <= 16 {
        // E-AC-3: byte 2 = strmtyp(2) || substreamid(3) || frmsiz_hi(3),
        // byte 3 = frmsiz_lo(8). frame_bytes = (frmsiz + 1) * 2.
        let frmsiz_hi = (buf[2] & 0x07) as usize;
        let frmsiz_lo = buf[3] as usize;
        let frmsiz = (frmsiz_hi << 8) | frmsiz_lo;
        Some((frmsiz + 1) * 2)
    } else {
        // Reserved bsid — treat as garbage.
        None
    }
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

    /// ADTS sample-rate index <-> Hz round-trip: every known rate must map to
    /// a unique index that maps back to the same rate.
    #[test]
    fn sr_index_from_hz_round_trips() {
        for idx in 0u8..=12 {
            let hz = sample_rate_from_index(idx).expect("known index");
            let back = sr_index_from_hz(hz).expect("known rate");
            assert_eq!(back, idx, "round-trip failed at idx {idx} ({hz} Hz)");
        }
        // Non-standard rates must return None.
        assert_eq!(sr_index_from_hz(42_000), None);
        assert_eq!(sr_index_from_hz(0), None);
    }

    // ── End-to-end encode → decode roundtrips (fdk-aac backend only) ──────
    //
    // The existing tests exercise constructor paths and the "garbage input
    // shouldn't panic" case, but never verify that decoding a valid AAC
    // frame produces correct PCM. A bug that emitted silence or the wrong
    // shape would pass every other test. These roundtrips close that gap
    // using the in-tree fdk-aac encoder.

    /// Strip the ADTS header from an encoded AAC frame so our decoder —
    /// which uses `open_raw` with an explicit ASC — can consume it.
    #[cfg(feature = "fdk-aac")]
    fn strip_adts_header(adts: &[u8]) -> &[u8] {
        assert!(adts.len() >= 7, "ADTS frame too short: {}", adts.len());
        assert_eq!(adts[0], 0xFF, "missing ADTS sync byte 0");
        assert_eq!(adts[1] & 0xF0, 0xF0, "missing ADTS sync byte 1 high nibble");
        // Bit 0 of byte 1 is `protection_absent`; when 0 the header carries
        // a 2-byte CRC after the standard 7 bytes.
        let protection_absent = (adts[1] & 0x01) != 0;
        let header_len = if protection_absent { 7 } else { 9 };
        &adts[header_len..]
    }

    /// Encode one frame of exact silence and verify the decoder emits PCM of
    /// the correct planar shape (2 channels × 1024 samples for AAC-LC) with
    /// every sample in range. Silent input → low-amplitude output catches
    /// "decoder returns garbage" without depending on SBR/priming quirks.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn fdk_decode_silence_roundtrip_produces_correct_shape() {
        use aac_codec::{AacProfile, EncoderConfig, SbrSignaling, TransportType};

        let config = EncoderConfig {
            profile: AacProfile::AacLc,
            sample_rate: 48_000,
            channels: 2,
            bitrate: 128_000,
            afterburner: true,
            sbr_signaling: SbrSignaling::default(),
            transport: TransportType::Adts,
        };
        let mut encoder = aac_audio::AacEncoder::open(&config).expect("open encoder");
        let frame_size = encoder.frame_size() as usize;
        let silence: Vec<Vec<f32>> = vec![vec![0.0_f32; frame_size]; 2];
        let encoded = encoder.encode_frame(&silence).expect("encode silence");
        assert!(
            !encoded.bytes.is_empty(),
            "encoder emitted no bytes for silence — prime frame expected"
        );

        let raw = strip_adts_header(&encoded.bytes);
        let mut decoder =
            AacDecoder::from_adts_config(1, 3, 2).expect("construct AAC-LC 48k stereo decoder");
        let planar = decoder.decode_frame(raw).expect("decode a valid frame");

        // Shape contract: planar[channel][sample], 2 channels, frame_size samples.
        assert_eq!(planar.len(), 2, "expected 2 channels, got {}", planar.len());
        assert_eq!(
            planar[0].len(),
            frame_size,
            "channel 0 sample count: got {}, want {frame_size}",
            planar[0].len()
        );
        assert_eq!(
            planar[0].len(),
            planar[1].len(),
            "channel counts desynced: {} vs {}",
            planar[0].len(),
            planar[1].len()
        );
        for (ch, samples) in planar.iter().enumerate() {
            for (i, s) in samples.iter().enumerate() {
                assert!(
                    s.abs() <= 1.0 && s.is_finite(),
                    "channel {ch} sample {i} = {s} out of range"
                );
            }
        }
        // After stream_info updates we should have the concrete codec name.
        assert_eq!(decoder.codec_name(), "AAC-LC");
    }

    /// Encode a 1 kHz sine over many frames, decode every frame, and verify
    /// the aggregate RMS matches the input. AAC-LC is lossy but a 1 kHz tone
    /// at -6 dBFS survives encode/decode within ~20 % RMS. A bug that returned
    /// zeros (decoder stall) or clipped to full-scale would fail this.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn fdk_decode_sine_roundtrip_preserves_rms() {
        use aac_codec::{AacProfile, EncoderConfig, SbrSignaling, TransportType};

        let config = EncoderConfig {
            profile: AacProfile::AacLc,
            sample_rate: 48_000,
            channels: 2,
            bitrate: 128_000,
            afterburner: true,
            sbr_signaling: SbrSignaling::default(),
            transport: TransportType::Adts,
        };
        let mut encoder = aac_audio::AacEncoder::open(&config).expect("open encoder");
        let frame_size = encoder.frame_size() as usize;
        let mut decoder = AacDecoder::from_adts_config(1, 3, 2).expect("decoder");

        // 20 frames * 1024 = 20480 samples ≈ 427 ms — enough to get past
        // AAC-LC's ~1024-sample encoder/decoder priming delay and measure a
        // stable RMS on the tail.
        const N_FRAMES: usize = 20;
        const FREQ: f32 = 1_000.0;
        const AMP: f32 = 0.5;

        let mut decoded_tail: Vec<f32> = Vec::new();
        let mut sample_idx: usize = 0;
        for _ in 0..N_FRAMES {
            let mut l = Vec::with_capacity(frame_size);
            let mut r = Vec::with_capacity(frame_size);
            for _ in 0..frame_size {
                let t = sample_idx as f32 / 48_000.0;
                let s = AMP * (2.0 * std::f32::consts::PI * FREQ * t).sin();
                l.push(s);
                r.push(s);
                sample_idx += 1;
            }
            let encoded = encoder.encode_frame(&vec![l, r]).expect("encode");
            if encoded.bytes.is_empty() {
                // Encoder still priming; skip.
                continue;
            }
            let raw = strip_adts_header(&encoded.bytes);
            let planar = decoder.decode_frame(raw).expect("decode");
            assert_eq!(planar.len(), 2);
            decoded_tail.extend_from_slice(&planar[0]);
        }

        // Skip the first 2048 decoded samples (encoder + decoder priming) and
        // measure RMS on the remainder.
        let skip = 2048.min(decoded_tail.len() / 2);
        let body = &decoded_tail[skip..];
        assert!(
            body.len() > 4_000,
            "not enough decoded samples after priming: {}",
            body.len()
        );
        let sum_sq: f64 = body.iter().map(|s| (*s as f64).powi(2)).sum();
        let rms = (sum_sq / body.len() as f64).sqrt();
        let expected = (AMP as f64) / std::f64::consts::SQRT_2; // 0.3536
        // AAC-LC at 128 kbps preserves a single-tone RMS within ~20 %.
        // Silence-bug (rms ≈ 0) and full-scale-bug (rms ≈ 0.707) both fail.
        let lo = expected * 0.8;
        let hi = expected * 1.2;
        assert!(
            rms >= lo && rms <= hi,
            "decoded 1kHz sine RMS {rms:.4} outside [{lo:.4}, {hi:.4}] — \
             encode/decode roundtrip broken"
        );
    }

    // ── Frame splitter tests (Bug B in DISPLAY_QUALITY_REPORT.md) ──────

    /// Build a minimum-valid MPEG-1 Layer II frame header given a bitrate
    /// and sample rate. Returns the full frame bytes (header + zero
    /// payload). The header table here is the audio path's source of
    /// truth — picks the indices straight out of ISO/IEC 11172-3 § 2.4.
    #[cfg(feature = "video-thumbnail")]
    fn make_mp2_frame(bitrate_kbps: u32, sample_rate_hz: u32, padding: bool) -> Vec<u8> {
        let bitrate_idx = match bitrate_kbps {
            32 => 1, 48 => 2, 56 => 3, 64 => 4, 80 => 5, 96 => 6,
            112 => 7, 128 => 8, 160 => 9, 192 => 10, 224 => 11,
            256 => 12, 320 => 13, 384 => 14,
            _ => panic!("unsupported MP2 bitrate: {bitrate_kbps}"),
        };
        let sr_idx = match sample_rate_hz {
            44_100 => 0, 48_000 => 1, 32_000 => 2,
            _ => panic!("unsupported MP2 sample rate: {sample_rate_hz}"),
        };
        // Frame size = 144 * bitrate / sample_rate + padding (Layer II).
        let pad_bit = if padding { 1u32 } else { 0 };
        let frame_size = (144 * bitrate_kbps * 1000 / sample_rate_hz + pad_bit) as usize;
        let mut buf = vec![0u8; frame_size];
        // Header byte 0: 0xFF (sync top 8 bits)
        buf[0] = 0xFF;
        // Byte 1: top 4 bits sync = 1, version (11 = MPEG-1), layer (10 = II),
        // protection (1 = no CRC) → 1111 1101 = 0xFD.
        buf[1] = 0xFD;
        // Byte 2: bitrate (4 bits) | sample_rate (2 bits) | padding (1 bit) | private (1 bit).
        buf[2] = ((bitrate_idx as u8) << 4) | ((sr_idx as u8) << 2) | (pad_bit as u8) << 1;
        // Byte 3: channel_mode (00 stereo) + the rest left as 0.
        buf[3] = 0x00;
        buf
    }

    /// Two concatenated MP2 frames must split exactly on the bitrate-
    /// table-derived frame_size, even when the audio payload contains
    /// `0xFF` bytes that look like a sync prefix to the legacy
    /// scan-to-next-sync splitter.
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn mp2_splitter_honours_frame_size_and_skips_payload_0xff() {
        use video_codec::AudioDecoderCodec;
        let mut a = make_mp2_frame(192, 48_000, false);
        let mut b = make_mp2_frame(192, 48_000, false);
        // Plant a 0xFF byte that the legacy 11-bit splitter would have
        // wrongly treated as the start of a fresh frame inside frame A.
        a[10] = 0xFF;
        a[11] = 0xE0;
        b[5] = 0xFF;
        b[6] = 0xF0; // looks like sync without our 12-bit gate
        let mut concat = a.clone();
        concat.extend_from_slice(&b);
        let frames = split_audio_codec_frames(&concat, AudioDecoderCodec::Mp2);
        assert_eq!(
            frames.len(),
            2,
            "expected exactly 2 MP2 frames, got {} (likely tripped on payload 0xFF bytes)",
            frames.len(),
        );
        assert_eq!(frames[0].len(), a.len(), "frame 0 size must match header-derived frame_size");
        assert_eq!(frames[1].len(), b.len(), "frame 1 size must match header-derived frame_size");
    }

    /// A truncated trailing MP2 frame is left behind for the next PES to
    /// resync on. The legacy splitter yielded the truncated bytes as a
    /// final slice and libavcodec rejected them with `Header missing`.
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn mp2_splitter_drops_truncated_trailing_frame() {
        use video_codec::AudioDecoderCodec;
        let a = make_mp2_frame(128, 48_000, false);
        let mut b = make_mp2_frame(128, 48_000, false);
        b.truncate(20); // partial — header but body cut short
        let mut concat = a.clone();
        concat.extend_from_slice(&b);
        let frames = split_audio_codec_frames(&concat, AudioDecoderCodec::Mp2);
        assert_eq!(frames.len(), 1, "truncated frame must not be emitted");
        assert_eq!(frames[0].len(), a.len());
    }

    // ── AC-3 / E-AC-3 splitter regression tests ──
    //
    // These tests document the contract that broadcast audio
    // transcoding (AC-3 → AAC / MP2 / AC-3 etc.) relies on: each AU
    // emitted by the splitter is one real syncframe — no phantom AUs
    // from `0x0B 0x77` bytes that happen to live inside the payload.
    // A phantom AU pushes an extra source PTS into the audio
    // replacer's accumulator and the receiver hears duplicate-PTS
    // PES frames; this is the broadcast-grade contract we're locking
    // in.

    /// Build a minimum-viable AC-3 syncframe of the requested byte
    /// length. The header carries a 48 kHz `fscod` and a real
    /// `frmsizecod` for the chosen size; the rest is zero filler.
    #[cfg(feature = "video-thumbnail")]
    fn make_ac3_frame_48k(frame_bytes: usize) -> Vec<u8> {
        // Look up a frmsizecod that maps to `frame_bytes / 2` 16-bit
        // words at 48 kHz (column 0 of AC3_FRMSIZ_WORDS).
        let target_words = (frame_bytes / 2) as u16;
        let frmsizecod = (0..AC3_FRMSIZ_WORDS.len())
            .find(|&i| AC3_FRMSIZ_WORDS[i][0] == target_words)
            .expect("frame_bytes must map to a valid AC-3 frmsizecod") as u8;
        let mut buf = vec![0u8; frame_bytes];
        buf[0] = 0x0B;
        buf[1] = 0x77;
        // byte 2-3: crc1 — opaque to the splitter.
        // byte 4: fscod(2) | frmsizecod(6); fscod = 00 (48 kHz).
        buf[4] = frmsizecod & 0x3F;
        // byte 5: bsid(5) | bsmod(3); bsid = 8 picks AC-3 (≤10).
        buf[5] = 8 << 3;
        buf
    }

    /// Build a minimum-viable E-AC-3 syncframe of the requested byte
    /// length. `frame_bytes` must be even and >= 6.
    #[cfg(feature = "video-thumbnail")]
    fn make_eac3_frame(frame_bytes: usize) -> Vec<u8> {
        assert!(frame_bytes >= 6 && frame_bytes % 2 == 0);
        let frmsiz: u16 = (frame_bytes as u16 / 2) - 1;
        let mut buf = vec![0u8; frame_bytes];
        buf[0] = 0x0B;
        buf[1] = 0x77;
        // byte 2: strmtyp(2)=0 | substreamid(3)=0 | frmsiz_hi(3)
        buf[2] = ((frmsiz >> 8) & 0x07) as u8;
        // byte 3: frmsiz_lo(8)
        buf[3] = (frmsiz & 0xFF) as u8;
        // byte 4: fscod(2) | numblkscod(2) | acmod(3) | lfeon(1)
        buf[4] = 0x00;
        // byte 5: bsid(5) = 16 (E-AC-3) | dialnorm hi bits
        buf[5] = 16 << 3;
        buf
    }

    /// Two valid AC-3 frames back-to-back land as two AUs.
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn ac3_splitter_walks_real_frame_size() {
        use video_codec::AudioDecoderCodec;
        // 384 kbps stereo 48 kHz frame = 1536 bytes (the 7HD Melbourne
        // case where the field bug originally surfaced).
        let a = make_ac3_frame_48k(1536);
        let b = make_ac3_frame_48k(1536);
        let mut concat = a.clone();
        concat.extend_from_slice(&b);
        let frames = split_audio_codec_frames(&concat, AudioDecoderCodec::Ac3);
        assert_eq!(frames.len(), 2, "expected exactly two AC-3 frames");
        assert_eq!(frames[0].len(), 1536);
        assert_eq!(frames[1].len(), 1536);
    }

    /// `0x0B 0x77` inside an AC-3 payload must NOT trigger a split.
    /// This is the regression for the duplicate-PTS audio bug — the
    /// naive byte scan splitter would have emitted three slices here
    /// (real start, phantom inside frame A, real start of frame B).
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn ac3_splitter_ignores_payload_syncword_pattern() {
        use video_codec::AudioDecoderCodec;
        let mut a = make_ac3_frame_48k(1536);
        // Plant the syncword pattern deep inside the AC-3 payload —
        // outside the header bytes the splitter validates.
        a[200] = 0x0B;
        a[201] = 0x77;
        a[900] = 0x0B;
        a[901] = 0x77;
        let b = make_ac3_frame_48k(1536);
        let mut concat = a.clone();
        concat.extend_from_slice(&b);
        let frames = split_audio_codec_frames(&concat, AudioDecoderCodec::Ac3);
        assert_eq!(
            frames.len(),
            2,
            "expected 2 AC-3 frames; payload 0x0B 0x77 bytes must not split"
        );
        assert_eq!(frames[0].len(), 1536);
        assert_eq!(frames[1].len(), 1536);
    }

    /// E-AC-3 walker honours the 11-bit `frmsiz` field. Same payload-
    /// pattern guard as AC-3 — the byte sequence isn't a frame boundary.
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn eac3_splitter_uses_frmsiz_field() {
        use video_codec::AudioDecoderCodec;
        let mut a = make_eac3_frame(768);
        a[100] = 0x0B;
        a[101] = 0x77;
        let b = make_eac3_frame(896);
        let mut concat = a.clone();
        concat.extend_from_slice(&b);
        let frames = split_audio_codec_frames(&concat, AudioDecoderCodec::Eac3);
        assert_eq!(frames.len(), 2, "expected 2 E-AC-3 frames");
        assert_eq!(frames[0].len(), 768);
        assert_eq!(frames[1].len(), 896);
    }

    /// A truncated trailing AC-3 frame is dropped — the next PES will
    /// resync on its own header. Matches `split_mp2_frames`'s contract.
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn ac3_splitter_drops_truncated_trailing_frame() {
        use video_codec::AudioDecoderCodec;
        let a = make_ac3_frame_48k(768);
        let mut b = make_ac3_frame_48k(768);
        b.truncate(40); // header + body cut short
        let mut concat = a.clone();
        concat.extend_from_slice(&b);
        let frames = split_audio_codec_frames(&concat, AudioDecoderCodec::Ac3);
        assert_eq!(frames.len(), 1, "truncated frame must not be emitted");
        assert_eq!(frames[0].len(), 768);
    }

    /// Reserved `fscod = 0b11` is rejected — the splitter resyncs to
    /// the next real sync. Guards against treating a syncword pattern
    /// embedded in payload as a real header.
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn ac3_splitter_skips_reserved_fscod() {
        use video_codec::AudioDecoderCodec;
        let real = make_ac3_frame_48k(768);
        // Garbage header at offset 0: syncword + fscod=11 + frmsizecod=0
        // — invalid, splitter should drop it and resync to `real`.
        let mut bad: Vec<u8> = vec![0x0B, 0x77, 0x00, 0x00, 0xC0, 8 << 3];
        bad.extend_from_slice(&real);
        let frames = split_audio_codec_frames(&bad, AudioDecoderCodec::Ac3);
        assert_eq!(frames.len(), 1, "reserved fscod must not yield a frame");
        assert_eq!(frames[0].len(), 768);
    }

    /// `AC3_FRMSIZ_WORDS` — spot-check the entries the broadcast
    /// transcode path will actually hit. The whole table is copied
    /// verbatim from ATSC A/52 § 5.4.1.4 Table 5.18 and a typo there
    /// would silently drop audio frames; lock in the most common
    /// 48 kHz bitrates.
    #[test]
    fn ac3_frmsiz_table_known_48k_bitrates() {
        // [bitrate_kbps, frmsiz_words, frame_bytes]
        let cases: &[(usize, u16, usize)] = &[
            // 128 kbps stereo
            (16, 256, 512),
            // 192 kbps (typical TS broadcast AC-3 stereo)
            (20, 384, 768),
            // 256 kbps
            (24, 512, 1024),
            // 384 kbps (7HD Melbourne case)
            (28, 768, 1536),
            // 448 kbps (5.1 high-rate)
            (30, 896, 1792),
            // 640 kbps (max)
            (36, 1280, 2560),
        ];
        for &(frmsizecod, words, frame_bytes) in cases {
            assert_eq!(
                AC3_FRMSIZ_WORDS[frmsizecod][0], words,
                "AC-3 48 kHz frmsizecod={frmsizecod} entry off"
            );
            assert_eq!(
                words as usize * 2,
                frame_bytes,
                "frame_bytes derivation off for frmsizecod={frmsizecod}"
            );
        }
    }

    /// Opus-in-MPEG-TS access unit: control_header_prefix `0xFFE0` (top
    /// 11 bits all 1), no flags, au_size = 7, then 7 bytes of Opus
    /// payload. The splitter must skip the 3-byte header and emit the
    /// 7-byte Opus packet.
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn opus_splitter_strips_control_header() {
        let mut buf = vec![0xFF, 0xE0, 0x07];
        buf.extend_from_slice(&[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70]);
        let frames = split_opus_frames(&buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70]);
    }

    /// `au_size` is variable-length: each `0xFF` byte adds 255 and
    /// continues. A 260-byte AU encodes as `[0xFF, 0x05]`.
    #[cfg(feature = "video-thumbnail")]
    #[test]
    fn opus_splitter_decodes_variable_length_au_size() {
        let mut buf = vec![0xFF, 0xE0, 0xFF, 0x05];
        buf.extend(vec![0xAA; 260]);
        let frames = split_opus_frames(&buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 260);
        assert!(frames[0].iter().all(|b| *b == 0xAA));
    }

    /// `DecodeStats` is a public hot-path counter struct. Trivial but
    /// previously untested — a regression to non-atomic ordering would not
    /// be caught by existing tests.
    #[test]
    fn decode_stats_counters_increment_independently() {
        let stats = DecodeStats::new();
        stats.inc_input();
        stats.inc_input();
        stats.inc_output();
        stats.inc_error();
        stats.inc_dropped_uninit();
        assert_eq!(stats.input_frames.load(Ordering::Relaxed), 2);
        assert_eq!(stats.output_blocks.load(Ordering::Relaxed), 1);
        assert_eq!(stats.decode_errors.load(Ordering::Relaxed), 1);
        assert_eq!(stats.dropped_uninit.load(Ordering::Relaxed), 1);
    }
}
