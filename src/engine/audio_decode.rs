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
use symphonia::core::audio::{Audio, AudioSpec, Channels, GenericAudioBufferRef, Position};
#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::errors::Error as SymphoniaError;
#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::packet::PacketRef;
#[cfg(not(feature = "fdk-aac"))]
use symphonia::core::units::{Duration as SymphoniaDuration, Timestamp};
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
    // Exhaustive on purpose — no wildcard. The bridge this gates is
    // opt-in per input type, and a new TS-carrying variant that silently
    // defaults to "no bridge" forwards raw 188-byte TS packets onto a PCM
    // bus. Adding a variant must be a compile error, not a silent regression.
    match input {
        // Contribution / distribution transports that carry MPEG-TS
        // verbatim on the wire.
        InputConfig::Rtp(_)
        | InputConfig::Udp(_)
        | InputConfig::Srt(_)
        | InputConfig::Rist(_)
        | InputConfig::Rtmp(_)
        | InputConfig::Rtsp(_)
        // Bonded carries whatever the sender bonded; the broadcast case
        // is MPEG-TS, which is why the runtime already treats it as a TS
        // carrier (`InputConfig::is_ts_carrier`).
        | InputConfig::Bonded(_)
        // Synthetic / file-backed sources that publish fresh
        // MPEG-TS onto the broadcast channel. test_pattern always
        // includes AAC when `audio_enabled = true` (default);
        // media_player's source file may carry AAC / MP2 / AC-3 /
        // E-AC-3; replay reads back recorded TS verbatim. All
        // three feed the `compressed_audio_input` path in
        // `engine::st2110_io::run_st2110_audio_output` so a PCM-
        // only audio output (ST 2110-30/-31/rtp_audio) attached
        // to the same flow gets the de-embedded audio decoded to
        // PCM. Previously these were absent from the predicate
        // so ST 2110-30 outputs on test_pattern / media_player /
        // replay sources only emitted the 250 ms NULL-PID
        // heartbeat — surfaced as ~107 pps on cellPTP11.
        | InputConfig::TestPattern(_)
        | InputConfig::MediaPlayer(_)
        | InputConfig::Replay(_)
        // SDI muxes captured embedded audio into the TS audio PID as AAC
        // in ADTS (stream_type 0x0F), and audio capture is on by default.
        | InputConfig::Sdi(_) => true,
        // A mosaic muxes its own AAC alongside the composited video, so it
        // carries decodable audio exactly as the other synthetic producers do.
        #[cfg(feature = "multiviewer")]
        InputConfig::Mosaic(_) => true,
        // WebRTC audio is always Opus, carried as stream_type 0x06 + an
        // "Opus" registration descriptor — never AAC. The bridge decodes
        // AAC only and drops every other elementary stream, so claiming
        // the bridge here would swap the raw-TS forward for silence
        // without ever producing correct PCM.
        InputConfig::Webrtc(_) | InputConfig::Whep(_) => false,
        // Video-only TS: these synthesise an encoded video ES and carry
        // no audio PID at all (`video_encode` is mandatory, and there is
        // no `audio_encode` on their config). ST 2110 keeps audio on a
        // separate -30 essence stream.
        InputConfig::St2110_20(_)
        | InputConfig::St2110_23(_)
        | InputConfig::MxlVideo(_) => false,
        // PCM / AES3 essence. These become TS carriers only with
        // `audio_encode` set, and the only shape where this flag is read
        // is a PCM output on the same flow — i.e. a same-node
        // PCM → encode → PCM round-trip. Left off until that config is
        // shown to be real: `audio_encode = s302m` (the only codec -31
        // accepts) isn't AAC, so the bridge would emit silence rather
        // than the passthrough these paths deliver today.
        InputConfig::St2110_30(_)
        | InputConfig::St2110_31(_)
        | InputConfig::RtpAudio(_)
        | InputConfig::MxlAudio(_) => false,
        // RFC 8331 ancillary data — never MPEG-TS.
        InputConfig::St2110_40(_) | InputConfig::MxlAnc(_) => false,
    }
}

// ── Non-AAC PES helpers (MP2 / AC-3 / E-AC-3) ───────────────────────────────
//
// MP2 (stream_type 0x03/0x04), AC-3 (0x80/0x81/0xC1), and E-AC-3 (0x87/0xC2)
// PES payloads are concatenated codec frames with no in-band length field
// you can trust across every variant. The decode path is: walk the PES,
// slice at every sync word, and call `avcodec_send_packet` once per frame.
// Feeding the whole PES at once silently drops everything past the first
// access unit (`avcodec_send_packet` decodes one AU per call).

/// The name decode stats show for a libavcodec audio decoder's codec.
#[cfg(feature = "media-codecs")]
pub fn ff_codec_name(codec: video_codec::AudioDecoderCodec) -> &'static str {
    match codec {
        video_codec::AudioDecoderCodec::Mp2 => "MP2",
        video_codec::AudioDecoderCodec::Ac3 => "AC-3",
        video_codec::AudioDecoderCodec::Eac3 => "E-AC-3",
        video_codec::AudioDecoderCodec::Opus => "Opus",
        video_codec::AudioDecoderCodec::AacLatm => "AAC-LATM",
    }
}

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
#[cfg(feature = "media-codecs")]
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

/// What a `codec_needs_encode` Warning calls a non-AAC source audio codec
/// (`"AC-3 audio (stream_type 0x81)"`) — `Some` only for one the
/// re-encoding outputs can decode into their encoder (MP2, AC-3, E-AC-3,
/// AAC-LATM), so the Warning never names `audio_encode` for a source it
/// cannot help: AC-4 (0xAC), DTS, an unknown type, or Opus-in-TS (0x06: a
/// WebRTC output passes it through without `audio_encode` and re-encodes it
/// with one, and RTMP / CMAF drop it whatever the block says).
#[cfg(feature = "media-codecs")]
pub fn reencodable_audio_label(stream_type: u8) -> Option<String> {
    match ff_codec_for_stream_type(stream_type)? {
        video_codec::AudioDecoderCodec::Opus => None,
        codec => Some(format!("{} audio (stream_type 0x{stream_type:02X})", ff_codec_name(codec))),
    }
}

/// Private options every libavcodec audio decode in the edge opens with.
///
/// AC-3 / E-AC-3 only (the others take none):
/// - `drc_scale` 0 — no dynamic-range compression. libavcodec applies the
///   bitstream's line-mode `dynrng` gains by default (`drc_scale` 1), so a
///   re-encode carried the compressed programme, and the receiver — which
///   would have chosen line, RF or no compression from the metadata — could
///   no longer choose: the edge's encoders do not write `dynrng`. A 448 kbps
///   5.1 source that carries it (ESPN) decodes 24.7 dB SNR apart with and
///   without it. Loudness measurement (BS.1770) and baseband playout (SDI,
///   ST 2110-30, the display) want the uncompressed programme too.
/// - `cons_noisegen` 1 — the dither that fills zero-bit mantissas is seeded
///   from each frame instead of running on across frames, so a frame always
///   decodes to the same PCM. Two decodes of one 192 kbps stereo source
///   differ at 33.5 dB SNR otherwise, which is what capped the gate-6
///   measurement of an AC-3 → AC-3 transcode at 39 dB.
#[cfg(feature = "media-codecs")]
pub fn ff_decoder_options(
    codec: video_codec::AudioDecoderCodec,
) -> &'static [(&'static str, &'static str)] {
    use video_codec::AudioDecoderCodec;
    match codec {
        AudioDecoderCodec::Ac3 | AudioDecoderCodec::Eac3 => {
            &[("drc_scale", "0"), ("cons_noisegen", "1")]
        }
        _ => &[],
    }
}

/// Open the libavcodec decoder for `codec` with [`ff_decoder_options`].
#[cfg(feature = "media-codecs")]
pub fn open_ff_decoder(
    codec: video_codec::AudioDecoderCodec,
) -> Result<video_engine::AudioDecoder, video_codec::AudioError> {
    video_engine::AudioDecoder::open_with_options(codec, ff_decoder_options(codec))
}

/// `dialnorm` of an AC-3 syncframe or of an E-AC-3 independent substream 0
/// frame, in dB (-31..=-1; the reserved 0 reads as -31, as decoders take
/// it). `None` for anything else — a dependent or other substream, a
/// truncated or invalid header.
#[cfg(feature = "media-codecs")]
pub(crate) fn ac3_dialnorm(buf: &[u8]) -> Option<i8> {
    if buf.len() < 8 || buf[0] != 0x0B || buf[1] != 0x77 {
        return None;
    }
    let bit = |i: usize| (buf[i / 8] >> (7 - i % 8)) & 1;
    let bits = |at: usize, n: usize| (0..n).fold(0u8, |v, k| (v << 1) | bit(at + k));
    let bsid = buf[5] >> 3;
    let at = if bsid <= 10 {
        // A/52 §5.3.2: syncword 16, crc1 16, fscod 2, frmsizecod 6, bsid 5,
        // bsmod 3, acmod 3, then cmixlev / surmixlev / dsurmod by acmod,
        // lfeon 1.
        let acmod = bits(48, 3);
        let mut at = 51;
        if acmod & 1 != 0 && acmod != 1 {
            at += 2;
        }
        if acmod & 4 != 0 {
            at += 2;
        }
        if acmod == 2 {
            at += 2;
        }
        at + 1
    } else if bsid <= 16 {
        // Annex E §E.1.2.2: strmtyp 2, substreamid 3 — an independent
        // substream 0 (strmtyp 0 or 2) only — frmsiz 11, fscod 2,
        // fscod2 / numblkscod 2, acmod 3, lfeon 1, bsid 5.
        let strmtyp = buf[2] >> 6;
        let substreamid = (buf[2] >> 3) & 0x07;
        if strmtyp == 1 || strmtyp == 3 || substreamid != 0 {
            return None;
        }
        45
    } else {
        return None;
    };
    if buf.len() * 8 < at + 5 {
        return None;
    }
    Some(match bits(at, 5) {
        0 => -31,
        d => -(d as i8),
    })
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
#[cfg(feature = "media-codecs")]
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
#[cfg(feature = "media-codecs")]
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
/// produces misaligned slices. The header arithmetic (MPEG-1 only; free
/// format and reserved indices rejected) is
/// [`super::audio_au::mpa_header`], shared with the TS audio replacer.
#[cfg(feature = "media-codecs")]
fn split_mp2_frames(buf: &[u8]) -> Vec<&[u8]> {
    use super::audio_au::{mpa_header, Head};
    let mut out: Vec<&[u8]> = Vec::with_capacity(8);
    let mut i = 0;
    while i + 4 <= buf.len() {
        let frame_size = match mpa_header(&buf[i..]) {
            Head::Valid(h) => h.len,
            _ => {
                i += 1;
                continue;
            }
        };
        if i + frame_size > buf.len() {
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
/// a control header (`control_header_prefix = 0x3FF` in 11 bits — `0111
/// 1111 111`, so the header opens `0x7F`, then `0b111` — followed by
/// `start_trim_flag` / `end_trim_flag` / `control_extension_flag`, an
/// extensible `au_size` length field, and the optional control fields gated
/// by the flags). The libopus decoder expects raw Opus packets, so the
/// splitter strips the control header and yields the `au_size` bytes that
/// follow. The prefix an edge's own muxer wrote until 2026-09 — eleven 1s,
/// `0xFF` then `0b111` — is accepted too, so a stream from an older edge
/// still decodes.
///
/// `au_size` counts the Opus packet alone: the optional `start_trim` /
/// `end_trim` (two bytes each, a 13-bit sample count) and the control
/// extension sit between it and the packet, outside it — what ffmpeg's
/// `mpegts` muxer writes and its Opus parser reads. They used to be taken
/// out of `au_size`, so the AU carrying a trim (ffmpeg's first, with the
/// encoder pre-skip, and its last) came out two bytes short. The trims are
/// skipped, not applied: no consumer here trims (an RTP Opus stream cannot
/// signal it — RFC 7587), so a stream's priming plays, as from any RTP
/// sender.
///
/// Best-effort — malformed AUs cause a resync on the next valid prefix
/// rather than aborting the PES, and the walk never reads past `buf`.
#[cfg(any(feature = "media-codecs", feature = "webrtc"))]
pub fn split_opus_frames(buf: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::with_capacity(2);
    let mut i = 0;
    while i + 2 <= buf.len() {
        // 11-bit control_header_prefix = 0x3FF: byte[i] = 0x7F and
        // byte[i+1] >> 5 = 0b111 (ffmpeg's `AV_RB16 >> 5 == 0x3ff`); or the
        // older edge muxer's 0xFF there.
        if !matches!(buf[i], 0x7F | 0xFF) || (buf[i + 1] & 0xE0) != 0xE0 {
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
        let mut pos = i + 2;
        let mut au_size: usize = 0;
        loop {
            let Some(&b) = buf.get(pos) else {
                return out;
            };
            pos += 1;
            au_size = au_size.saturating_add(b as usize);
            if b != 0xFF {
                break;
            }
        }

        // The optional fields the flags announce follow `au_size`, ahead of
        // the packet (and are not counted in it).
        if start_trim_flag {
            pos += 2;
        }
        if end_trim_flag {
            pos += 2;
        }
        if control_extension_flag {
            let Some(&ext_len) = buf.get(pos) else {
                return out;
            };
            pos += 1 + ext_len as usize;
        }

        let Some(payload_end) = pos.checked_add(au_size).filter(|&end| end <= buf.len()) else {
            // Truncated: the AU runs past this PES.
            return out;
        };
        if au_size > 0 {
            out.push(&buf[pos..payload_end]);
        }
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
///
/// An E-AC-3 dependent substream frame (`strmtyp` 1) stays in the slice
/// of the frame before it: libavcodec merges a dependent frame only when it
/// follows its independent frame in the same packet, so 7.1 sent one
/// syncframe per `send_packet` decoded as its 5.1 core.
#[cfg(feature = "media-codecs")]
fn split_ac3_frames(buf: &[u8]) -> Vec<&[u8]> {
    // (start, end) of each AU.
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(8);
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
        let dependent = (buf[i + 5] >> 3) > 10 && buf[i + 2] >> 6 == 1;
        match out.last_mut() {
            Some(prev) if dependent && prev.1 == i => prev.1 = i + frame_bytes,
            _ => out.push((i, i + frame_bytes)),
        }
        i += frame_bytes;
    }
    out.into_iter().map(|(a, b)| &buf[a..b]).collect()
}

/// Smallest syncinfo we can parse: 16 bits syncword + bsi bytes through
/// the `bsid` field (byte 5). Anything shorter is treated as a sync
/// candidate that we can't validate yet — caller falls through.
pub(crate) const AC3_MIN_HEADER_BYTES: usize = 6;

/// AC-3 frame size table from ATSC A/52 § 5.4.1.4 Table 5.18 — entries
/// are in 16-bit words; the caller multiplies by 2 to get bytes. Indexed
/// by `frmsizecod` (0..=37); inner index is the sample-rate code
/// (0 = 48 kHz, 1 = 44.1 kHz, 2 = 32 kHz). `fscod = 0b11` is reserved
/// and bails out before this table is consulted.
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
pub(crate) fn ac3_frame_size(buf: &[u8]) -> Option<usize> {
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

/// Decodes one TS audio PID to PCM, access unit by access unit, for the
/// paths that only measure or monitor it (content analysis, the display's
/// level bars).
///
/// The AUs are cut across PES boundaries by the TS audio replacer's cutter
/// (`engine::audio_au`), fed whole TS packets (continuity checked). Parsing
/// each PES on its own — at the next PUSI, stopping at the first byte that
/// was not a sync word — lost an AU that straddled two PES and, the next PES
/// then opening on its tail, every AU of the next PES; it also never found
/// an ADTS sync in AAC-LATM, which is now decoded through libavcodec.
pub struct PidAudioDecoder {
    stream_type: u8,
    cutter: crate::engine::audio_au::AuCutter,
    /// ADTS: the decoder and the header config it was built for.
    aac: Option<(AacDecoder, (u8, u8, u8))>,
    #[cfg(feature = "media-codecs")]
    ff: Option<video_engine::AudioDecoder>,
}

impl PidAudioDecoder {
    /// A decoder for `stream_type`, or `None` for one this framing does not
    /// cover (Opus on 0x06, AC-4, LPCM).
    pub fn new(stream_type: u8) -> Option<Self> {
        let fmt = crate::engine::audio_au::AuFormat::for_stream_type(stream_type)?;
        Some(Self {
            stream_type,
            cutter: crate::engine::audio_au::AuCutter::new(fmt),
            aac: None,
            #[cfg(feature = "media-codecs")]
            ff: None,
        })
    }

    /// Feed one whole TS packet of the PID; `pcm` gets the planar PCM and
    /// sample rate of every AU it completes.
    pub fn push_packet(&mut self, pkt: &[u8], mut pcm: impl FnMut(&[Vec<f32>], u32)) {
        if !self.cutter.push_packet(pkt) {
            return;
        }
        while let Some(au) = self.cutter.next(false) {
            self.decode(&au.data, &mut pcm);
        }
    }

    fn decode(&mut self, au: &[u8], pcm: &mut impl FnMut(&[Vec<f32>], u32)) {
        if self.stream_type == 0x0F {
            if au.len() < 9 {
                return;
            }
            let header_len = if au[1] & 0x01 != 0 { 7 } else { 9 };
            let cfg = ((au[2] >> 6) & 0x03, (au[2] >> 2) & 0x0F, ((au[2] & 0x01) << 2) | ((au[3] >> 6) & 0x03));
            if self.aac.as_ref().is_none_or(|(_, c)| *c != cfg) {
                self.aac = AacDecoder::from_adts_config(cfg.0, cfg.1, cfg.2).ok().map(|d| (d, cfg));
            }
            if let Some((dec, _)) = self.aac.as_mut()
                && let Ok(planar) = dec.decode_frame(&au[header_len..])
            {
                pcm(&planar, dec.sample_rate());
            }
            return;
        }
        #[cfg(feature = "media-codecs")]
        {
            let Some(codec) = ff_codec_for_stream_type(self.stream_type) else {
                return;
            };
            if self.ff.is_none() {
                self.ff = open_ff_decoder(codec).ok();
            }
            if let Some(dec) = self.ff.as_mut()
                && dec.send_packet(au, 0).is_ok()
            {
                while let Ok(frame) = dec.receive_frame() {
                    pcm(&frame.planar, frame.sample_rate);
                }
            }
        }
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

/// The PCM format an [`AacDecoder`] built from this ADTS config produces
/// for `frame` (an ADTS-stripped access unit): the header's rate and
/// channel count describe the AAC core, which is not what HE-AAC decodes
/// to — SBR doubles the rate (a 48 kHz service carries 24000 in its
/// header) and PS widens v2's mono core to stereo. `None` when the frame
/// does not decode (the next one may).
pub fn aac_decoded_format(
    profile: u8,
    sample_rate_index: u8,
    channel_config: u8,
    frame: &[u8],
) -> Option<(u32, u8)> {
    let mut d = AacDecoder::from_adts_config(profile, sample_rate_index, channel_config).ok()?;
    let planar = d.decode_frame(frame).ok()?;
    if planar.first().is_none_or(|c| c.is_empty()) {
        return None;
    }
    Some((d.sample_rate(), planar.len() as u8))
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
            1 => Channels::Positioned(Position::FRONT_LEFT),
            2 => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
            _ => return Err(AacDecodeError::UnsupportedChannelConfig(channels)),
        };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(sample_rate)
            .with_channels(channels_mask);

        let inner = SymphoniaAacDecoder::try_new(&params, &AudioDecoderOptions::default())
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
        // `PacketRef` borrows `frame_bytes` — the owning `Packet` would copy every
        // AAC frame into a fresh `Box<[u8]>`, which this decode path runs per frame.
        let packet = PacketRef::new(
            0,
            Timestamp::new(self.next_ts as i64),
            SymphoniaDuration::new(1024),
            frame_bytes,
        );
        self.next_ts = self.next_ts.saturating_add(1024);

        let buf_ref = self
            .inner
            .decode_ref(&packet)
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
fn audio_buffer_ref_to_planar_f32(buf_ref: &GenericAudioBufferRef<'_>) -> Vec<Vec<f32>> {
    match buf_ref {
        // `plane` replaces 0.5's `Signal::chan` and returns `Option` rather than
        // panicking. A missing plane means the decoder produced fewer channels than
        // the spec advertises; treat it as silence rather than killing the flow.
        GenericAudioBufferRef::F32(buf) => extract_planar_f32(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[])),
        GenericAudioBufferRef::F64(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| *s as f32),
        GenericAudioBufferRef::S16(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| *s as f32 / 32_768.0),
        GenericAudioBufferRef::S24(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| s.inner() as f32 / 8_388_608.0),
        GenericAudioBufferRef::S32(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| *s as f32 / 2_147_483_648.0),
        GenericAudioBufferRef::U8(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| (*s as f32 - 128.0) / 128.0),
        GenericAudioBufferRef::U16(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| (*s as f32 - 32_768.0) / 32_768.0),
        GenericAudioBufferRef::U24(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| (s.inner() as f32 - 8_388_608.0) / 8_388_608.0),
        GenericAudioBufferRef::U32(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| (*s as f32 - 2_147_483_648.0) / 2_147_483_648.0),
        GenericAudioBufferRef::S8(buf) => extract_planar_with(buf.spec(), buf.frames(), |ch| buf.plane(ch).unwrap_or(&[]), |s| *s as f32 / 128.0),
    }
}

#[cfg(not(feature = "fdk-aac"))]
#[inline]
fn extract_planar_f32<'a, F>(spec: &AudioSpec, frames: usize, get_chan: F) -> Vec<Vec<f32>>
where
    F: Fn(usize) -> &'a [f32],
{
    let n_ch = spec.channels().count();
    (0..n_ch)
        .map(|ch| {
            // Honour the "missing plane is silence" contract in the caller:
            // slicing `[..frames]` on a short/empty plane would panic instead.
            // Every channel comes back exactly `frames` long, zero-padded.
            let plane = get_chan(ch);
            let n = plane.len().min(frames);
            let mut out = Vec::with_capacity(frames);
            out.extend_from_slice(&plane[..n]);
            out.resize(frames, 0.0);
            out
        })
        .collect()
}

#[cfg(not(feature = "fdk-aac"))]
#[inline]
fn extract_planar_with<'a, S, F, M>(
    spec: &AudioSpec,
    frames: usize,
    get_chan: F,
    map: M,
) -> Vec<Vec<f32>>
where
    S: 'a,
    F: Fn(usize) -> &'a [S],
    M: Fn(&S) -> f32,
{
    let n_ch = spec.channels().count();
    (0..n_ch)
        .map(|ch| {
            // Same silence contract as `extract_planar_f32` — see there.
            let plane = get_chan(ch);
            let n = plane.len().min(frames);
            let mut out: Vec<f32> = plane[..n].iter().map(&map).collect();
            out.resize(frames, 0.0);
            out
        })
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `codec_needs_encode` names `audio_encode` only for a source the
    /// re-encode can decode: never AC-4, DTS, an unknown type or Opus.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn only_a_decodable_source_is_labelled_for_a_re_encode() {
        assert_eq!(reencodable_audio_label(0x03).as_deref(), Some("MP2 audio (stream_type 0x03)"));
        assert_eq!(reencodable_audio_label(0x81).as_deref(), Some("AC-3 audio (stream_type 0x81)"));
        assert_eq!(reencodable_audio_label(0x87).as_deref(), Some("E-AC-3 audio (stream_type 0x87)"));
        assert_eq!(reencodable_audio_label(0x11).as_deref(), Some("AAC-LATM audio (stream_type 0x11)"));
        for st in [0xAC, 0x82, 0x85, 0x86, 0x8A, 0x06, 0x00] {
            assert_eq!(reencodable_audio_label(st), None, "stream_type 0x{st:02X}");
        }
    }

    /// Every access unit of an encoded 1 kHz tone, as the encoder framed it:
    /// ADTS or LOAS AAC (fdk-aac), MP2 or AC-3 (libavcodec).
    #[cfg(all(feature = "fdk-aac", feature = "media-codecs"))]
    fn encoded_aus(stream_type: u8, n: usize) -> Vec<Vec<u8>> {
        let tone = |len: usize, at: usize| -> Vec<Vec<f32>> {
            let c: Vec<f32> = (at..at + len)
                .map(|k| 0.3 * (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / 48_000.0).sin())
                .collect();
            vec![c.clone(), c]
        };
        let mut aus = Vec::new();
        match stream_type {
            0x0F | 0x11 => {
                let mut e = aac_audio::AacEncoder::open(&aac_codec::EncoderConfig {
                    profile: aac_codec::AacProfile::AacLc,
                    sample_rate: 48_000,
                    channels: 2,
                    bitrate: 128_000,
                    afterburner: true,
                    sbr_signaling: aac_codec::SbrSignaling::default(),
                    transport: if stream_type == 0x0F {
                        aac_codec::TransportType::Adts
                    } else {
                        aac_codec::TransportType::Latm
                    },
                })
                .unwrap();
                let mut at = 0;
                while aus.len() < n {
                    let ed = e.encode_frame(&tone(1024, at)).unwrap();
                    at += 1024;
                    if !ed.bytes.is_empty() {
                        aus.push(ed.bytes);
                    }
                }
            }
            _ => {
                let codec = if stream_type == 0x03 {
                    video_codec::AudioCodecType::Mp2
                } else {
                    video_codec::AudioCodecType::Ac3
                };
                let mut e = video_engine::AudioEncoder::open(&video_codec::AudioEncoderConfig {
                    codec,
                    sample_rate: 48_000,
                    channels: 2,
                    bitrate_kbps: 192,
                })
                .unwrap();
                let fs = e.frame_size();
                let mut at = 0;
                while aus.len() < n {
                    aus.extend(e.encode_frame(&tone(fs, at)).unwrap().into_iter().map(|f| f.data.to_vec()));
                    at += fs;
                }
                aus.truncate(n);
            }
        }
        aus
    }

    /// `PidAudioDecoder` decodes every AU of a stream whose PES cut through
    /// AUs (700-byte PES regardless of framing): as many samples as the
    /// same AUs fed to a decoder one by one. The per-PES parses it replaced
    /// in content analysis and the display's level bars lost each AU that
    /// straddled two PES (and, for ADTS, resynced through the next PES's
    /// head byte by byte); AAC-LATM was never decoded by content analysis
    /// at all.
    #[cfg(all(feature = "fdk-aac", feature = "media-codecs"))]
    #[test]
    fn pid_audio_decoder_decodes_every_au_across_pes_boundaries() {
        for st in [0x0F_u8, 0x11, 0x03, 0x81] {
            let aus = encoded_aus(st, 40);
            // Reference: the AUs decoded one by one.
            let mut reference = 0usize;
            if st == 0x0F {
                let mut d = aac_audio::AacDecoder::open_adts().unwrap();
                for au in &aus {
                    reference += d.decode_frame(au).map_or(0, |f| f.planar[0].len());
                }
            } else {
                let mut d = open_ff_decoder(ff_codec_for_stream_type(st).unwrap()).unwrap();
                for au in &aus {
                    if d.send_packet(au, 0).is_ok() {
                        while let Ok(f) = d.receive_frame() {
                            reference += f.planar[0].len();
                        }
                    }
                }
            }
            assert!(reference > 0, "0x{st:02X}: reference decode");
            let es: Vec<u8> = aus.concat();
            let mut cc = 0u8;
            let mut ts = Vec::new();
            for (k, chunk) in es.chunks(700).enumerate() {
                ts.extend(crate::engine::ts_test_fixtures::pes_packets(
                    0x101,
                    0xC0,
                    chunk,
                    900_000 + k as u64 * 1_000,
                    &mut cc,
                ));
            }
            let mut d = PidAudioDecoder::new(st).expect("a framed format");
            let mut decoded = 0usize;
            for p in ts.chunks(188) {
                d.push_packet(p, |planar, rate| {
                    assert_eq!(rate, 48_000);
                    decoded += planar[0].len();
                });
            }
            assert_eq!(decoded, reference, "0x{st:02X}: every AU decoded");
        }
        assert!(PidAudioDecoder::new(0x06).is_none(), "Opus is framed per PES elsewhere");
    }

    /// A 7.1 E-AC-3 stream (5.1 independent frame + a dependent frame
    /// carrying the wide pair, per 32 ms slot) decodes to eight channels on
    /// both framings the decode paths use: the AU cutter (here through
    /// `PidAudioDecoder`, as the TS audio replacer, the demuxer and HLS cut
    /// it) and the splitter behind the demuxer's `OtherAudio` consumers.
    /// Sent one syncframe per packet, libavcodec ignored every dependent
    /// frame ("Ignoring dependent frame without independent frame") and the
    /// programme decoded as its 5.1 core, with no error anywhere.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn a_7_1_e_ac3_stream_decodes_to_eight_channels() {
        use video_codec::AudioDecoderCodec;
        const ES: &[u8] = include_bytes!("testdata/eac3_5_1_plus_dependent_48k.ec3");
        let rms = |c: &[f32]| (c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32).sqrt();
        let check = |planar: &[Vec<f32>], rate: u32| {
            assert_eq!((planar.len(), rate), (8, 48_000), "FL FR FC LFE SL SR WL WR");
            assert_eq!(planar[0].len(), 1536);
            assert!(planar[..6].iter().all(|c| rms(c) > 0.01), "the core's tones");
            assert!(planar[6..].iter().all(|c| rms(c) < 1e-3), "the (silent) wide pair");
        };
        // Cut by the AU cutter, one slot per PES.
        let mut ts = Vec::new();
        let mut cc = 0u8;
        for (k, slot) in ES.chunks(896).enumerate() {
            ts.extend(crate::engine::ts_test_fixtures::pes_packets(
                0x101,
                0xBD,
                slot,
                900_000 + k as u64 * 2_880,
                &mut cc,
            ));
        }
        let mut d = PidAudioDecoder::new(0x87).expect("E-AC-3 is framed");
        let mut frames = 0;
        for p in ts.chunks(188) {
            d.push_packet(p, |planar, rate| {
                check(planar, rate);
                frames += 1;
            });
        }
        // The decoder hands a frame back one packet late.
        assert!(frames >= 6, "{frames} frames");
        // Split, as the demuxer's OtherAudio consumers do.
        let aus = split_audio_codec_frames(ES, AudioDecoderCodec::Eac3);
        assert_eq!(aus.iter().map(|a| a.len()).collect::<Vec<_>>(), [896; 7]);
        let mut d = open_ff_decoder(AudioDecoderCodec::Eac3).unwrap();
        let mut frames = 0;
        for au in aus {
            d.send_packet(au, 0).unwrap();
            while let Ok(f) = d.receive_frame() {
                check(&f.planar, f.sample_rate);
                frames += 1;
            }
        }
        assert!(frames >= 6, "{frames} frames");
    }

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

    /// A real AAC-LC bitstream, decoded and checked for content — deliberately
    /// NOT gated on a backend, so it covers fdk-aac AND symphonia.
    ///
    /// Until this existed, the symphonia half of this module had no test that
    /// decoded a single frame: the two round-trip tests are `#[cfg(feature =
    /// "fdk-aac")]` because they need the fdk *encoder*, and the only
    /// symphonia-specific test is a rejection test. So the pure-Rust fallback
    /// could have emitted silence, noise, or half a stream and every test would
    /// still have passed. That gap predated the 0.5.5 -> 0.6.0 bump and was the
    /// real reason the bump looked unverifiable.
    ///
    /// The fixture is 0.5 s of a 1 kHz sine at amplitude 0.5, 48 kHz stereo,
    /// AAC-LC 128 kbps in ADTS framing. Cross-checked against an independent
    /// decoder (ffmpeg 8.0.1) which resolves it to RMS 0.3494 over the same
    /// post-priming window — i.e. the tolerance below is calibrated against a
    /// second implementation, not just against our own arithmetic.
    ///
    /// Beware regenerating it: ffmpeg's `sine` lavfi source defaults to
    /// amplitude 0.125, not full scale. The first cut of this fixture was
    /// built with `volume=0.5` on top of that and came out 18 dB down, which
    /// read as a decoder fault until ffmpeg agreed with us to 4 decimal places.
    /// The correct chain is `sine=...,volume=4.0` for a 0.5 peak.
    #[test]
    fn decodes_real_aac_lc_sine_to_expected_rms() {
        const FIXTURE: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        const AMP: f64 = 0.5;

        let mut decoder = AacDecoder::from_adts_config(1, 3, 2).expect("decoder");
        assert_eq!(decoder.sample_rate(), 48_000);
        assert_eq!(decoder.channels(), 2);

        // Walk the ADTS frames: 13-bit aac_frame_length spans bytes 3..5.
        let mut left: Vec<f32> = Vec::new();
        let mut right: Vec<f32> = Vec::new();
        let mut off = 0usize;
        let mut frames = 0usize;
        while off + 7 <= FIXTURE.len() {
            assert_eq!(FIXTURE[off], 0xFF, "lost ADTS sync at {off}");
            let len = (((FIXTURE[off + 3] as usize) & 0x03) << 11)
                | ((FIXTURE[off + 4] as usize) << 3)
                | ((FIXTURE[off + 5] as usize) >> 5);
            if len == 0 || off + len > FIXTURE.len() {
                break;
            }
            let raw = strip_adts_header(&FIXTURE[off..off + len]);
            // The encoder's first frame carries its metadata comment and may
            // decode to nothing useful; a decode error there is not a failure.
            if let Ok(planar) = decoder.decode_frame(raw) {
                assert_eq!(planar.len(), 2, "expected stereo planar output");
                left.extend_from_slice(&planar[0]);
                right.extend_from_slice(&planar[1]);
            }
            off += len;
            frames += 1;
        }

        assert!(frames >= 15, "only walked {frames} ADTS frames");
        assert!(
            left.len() > 8_000,
            "decoded only {} samples — decoder produced no audio",
            left.len()
        );
        assert_eq!(left.len(), right.len(), "channel planes differ in length");

        // Skip encoder + decoder priming, then measure. A silence bug lands at
        // rms ~0 and a full-scale/format bug at ~0.707; the true value is
        // 0.5/sqrt(2) = 0.3536. AAC-LC at 128 kbps holds a single tone well
        // inside +/-20 %.
        let skip = 2_048.min(left.len() / 2);
        let body = &left[skip..];
        let sum_sq: f64 = body.iter().map(|s| (*s as f64).powi(2)).sum();
        let rms = (sum_sq / body.len() as f64).sqrt();
        let expected = AMP / std::f64::consts::SQRT_2;
        assert!(
            rms >= expected * 0.8 && rms <= expected * 1.2,
            "decoded 1 kHz sine RMS {rms:.4} outside [{:.4}, {:.4}]",
            expected * 0.8,
            expected * 1.2
        );

        // The fixture is dual-mono, so a channel-ordering or plane-aliasing
        // fault in the planar extraction shows up as a mismatch here.
        let r_sum_sq: f64 = right[skip..].iter().map(|s| (*s as f64).powi(2)).sum();
        let r_rms = (r_sum_sq / right[skip..].len() as f64).sqrt();
        assert!(
            (rms - r_rms).abs() < expected * 0.05,
            "L/R RMS diverge ({rms:.4} vs {r_rms:.4}) on a dual-mono source"
        );
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
    ///
    /// Not backend-gated: `decodes_real_aac_lc_sine_to_expected_rms` uses it on
    /// the symphonia build too.
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
            let encoded = encoder.encode_frame(&[l, r]).expect("encode");
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
    #[cfg(feature = "media-codecs")]
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
    #[cfg(feature = "media-codecs")]
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
    #[cfg(feature = "media-codecs")]
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
    #[cfg(feature = "media-codecs")]
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
    #[cfg(feature = "media-codecs")]
    fn make_eac3_frame(frame_bytes: usize) -> Vec<u8> {
        assert!(frame_bytes >= 6 && frame_bytes.is_multiple_of(2));
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
    #[cfg(feature = "media-codecs")]
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
    #[cfg(feature = "media-codecs")]
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
    #[cfg(feature = "media-codecs")]
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
    #[cfg(feature = "media-codecs")]
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
    #[cfg(feature = "media-codecs")]
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
    ///
    /// Gated to match `AC3_FRMSIZ_WORDS` itself, which only exists with the
    /// libavcodec audio layer — the same gate its eight sibling tests carry.
    #[cfg(feature = "media-codecs")]
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

    /// Opus-in-MPEG-TS access unit: control_header_prefix 0x3FF (the
    /// header opens `0x7F 0xE0`, as ffmpeg writes it), no flags, au_size =
    /// 7, then 7 bytes of Opus payload. The splitter must skip the 3-byte
    /// header and emit the 7-byte Opus packet — and take the `0xFF 0xE0`
    /// an older edge's muxer wrote the same way.
    #[cfg(any(feature = "media-codecs", feature = "webrtc"))]
    #[test]
    fn opus_splitter_strips_control_header() {
        for first in [0x7F, 0xFF] {
            let mut buf = vec![first, 0xE0, 0x07];
            buf.extend_from_slice(&[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70]);
            let frames = split_opus_frames(&buf);
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0], &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70]);
        }
    }

    /// `au_size` is variable-length: each `0xFF` byte adds 255 and
    /// continues. A 260-byte AU encodes as `[0xFF, 0x05]`.
    #[cfg(any(feature = "media-codecs", feature = "webrtc"))]
    #[test]
    fn opus_splitter_decodes_variable_length_au_size() {
        let mut buf = vec![0xFF, 0xE0, 0xFF, 0x05];
        buf.extend(vec![0xAA; 260]);
        let frames = split_opus_frames(&buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 260);
        assert!(frames[0].iter().all(|b| *b == 0xAA));
        // Exactly 255 is `[0xFF, 0x00]` — what ffmpeg's muxer writes.
        let mut buf = vec![0x7F, 0xE0, 0xFF, 0x00];
        buf.extend(vec![0xBB; 255]);
        assert_eq!(split_opus_frames(&buf), vec![&[0xBB; 255][..]]);
    }

    /// `au_size` is the Opus packet alone: the trims and the control
    /// extension sit between it and the packet. ffmpeg's muxer flags
    /// `start_trim` on its first AU (the encoder pre-skip, 312) and
    /// `end_trim` on its last; both came out two bytes short, taken out of
    /// `au_size`, and the walk resynced inside the packet's tail.
    #[cfg(any(feature = "media-codecs", feature = "webrtc"))]
    #[test]
    fn opus_splitter_skips_trims_and_extension_outside_au_size() {
        let buf = [
            0x7F, 0xF0, 0x03, 0x01, 0x38, 0x7C, 0x11, 0x22, // start_trim 312, 3-byte packet
            0x7F, 0xE8, 0x02, 0x02, 0x88, 0x7C, 0x33, // end_trim 648, 2-byte packet
            0x7F, 0xF4, 0x02, 0x00, 0x10, 0x02, 0xEE, 0xEE, 0x4C, 0x44, // trim + 2-byte extension
        ];
        assert_eq!(
            split_opus_frames(&buf),
            vec![&[0x7C, 0x11, 0x22][..], &[0x7C, 0x33][..], &[0x4C, 0x44][..]]
        );
    }

    /// ffmpeg's own Opus-in-TS (`-c:a libopus -f mpegts`, 0.4 s): every
    /// PES splits into its packets whole — the first, carrying the pre-skip
    /// as `start_trim`, and the last, carrying `end_trim`, included — each a
    /// 20 ms packet by its TOC. The two trimmed ones came out two bytes
    /// short.
    #[cfg(any(all(feature = "display", target_os = "linux"), feature = "webrtc"))]
    #[test]
    fn opus_splitter_takes_ffmpegs_trimmed_access_units_whole() {
        use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};
        const TS: &[u8] = include_bytes!("testdata/sine1k_opus_48k_stereo.ts");
        let mut demux = TsDemuxer::new(None);
        // A PES is handed over when the next starts: twice, for the last.
        let sizes: Vec<Vec<usize>> = demux
            .demux(&[TS, TS].concat())
            .into_iter()
            .filter_map(|f| match f {
                DemuxedFrame::Opus { data, .. } => {
                    Some(split_opus_frames(&data).iter().map(|p| p.len()).collect())
                }
                _ => None,
            })
            .take(5)
            .collect();
        assert_eq!(
            sizes,
            vec![
                vec![106, 93, 91, 90, 85],
                vec![86, 86, 85, 83, 56],
                vec![58, 54, 57, 57, 51],
                vec![59, 58, 54, 53, 110],
                vec![232],
            ]
        );
    }

    /// Malformed input stops the walk or resyncs; it never reads past the
    /// buffer: an `au_size` running off the end, a trim or an extension
    /// cut short, a zero-length AU (skipped), bytes before the first prefix.
    #[cfg(any(feature = "media-codecs", feature = "webrtc"))]
    #[test]
    fn opus_splitter_survives_malformed_access_units() {
        assert!(split_opus_frames(&[0x7F, 0xE0, 0x05, 0x7C]).is_empty());
        assert!(split_opus_frames(&[0x7F, 0xE0, 0xFF, 0xFF]).is_empty());
        assert!(split_opus_frames(&[0x7F, 0xF8, 0x01, 0x00]).is_empty());
        assert!(split_opus_frames(&[0x7F, 0xE4, 0x01]).is_empty());
        assert!(split_opus_frames(&[0x7F, 0xE4, 0x01, 0x09, 0x00]).is_empty());
        assert_eq!(
            split_opus_frames(&[0x00, 0x12, 0x7F, 0xE0, 0x00, 0x7F, 0xE0, 0x01, 0x7C]),
            vec![&[0x7C][..]]
        );
        for len in 0..64 {
            let junk: Vec<u8> = (0..len).map(|k| [0x7F, 0xFF, 0xE4, 0xFF][k % 4]).collect();
            let _ = split_opus_frames(&junk);
        }
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

    // ── input_can_carry_ts_audio ────────────────────────────────────────

    fn input_cfg(json: serde_json::Value) -> crate::config::models::InputConfig {
        serde_json::from_value(json).expect("input config should deserialize")
    }

    /// Pins the full decision table. `input_can_carry_ts_audio` gates the
    /// TS→AAC→PCM bridge; a variant that wrongly reports `false` forwards
    /// raw 188-byte TS packets onto the ST 2110-30 PCM bus.
    #[test]
    fn ts_audio_predicate_decision_table() {
        let sdi = serde_json::json!({
            "type": "sdi",
            "device": "DeckLink Quad (1)",
            "video_encode": { "codec": "x264" },
        });
        // SDI muxes embedded audio as AAC/ADTS and audio is on by default.
        assert!(input_can_carry_ts_audio(&input_cfg(sdi)));

        // Carries MPEG-TS verbatim — identical in kind to srt / udp / rtp.
        assert!(input_can_carry_ts_audio(&input_cfg(serde_json::json!({
            "type": "rist",
            "bind_addr": "0.0.0.0:5004",
        }))));

        // WebRTC audio is always Opus; the bridge is AAC-only.
        assert!(!input_can_carry_ts_audio(&input_cfg(serde_json::json!({
            "type": "webrtc",
            "bind_addr": "0.0.0.0:8080",
        }))));

        // Video-only TS — no audio PID to de-embed.
        assert!(!input_can_carry_ts_audio(&input_cfg(serde_json::json!({
            "type": "st2110_20",
            "bind_addr": "239.0.0.1:20000",
            "width": 1920,
            "height": 1080,
            "frame_rate_num": 25,
            "frame_rate_den": 1,
            "pixel_format": "yuv422_10bit",
            "pid_overrides": null,
            "video_encode": { "codec": "x264" },
        }))));

        // RFC 8331 ancillary data is never MPEG-TS.
        assert!(!input_can_carry_ts_audio(&input_cfg(serde_json::json!({
            "type": "st2110_40",
            "bind_addr": "239.0.0.1:20000",
        }))));
    }
}
