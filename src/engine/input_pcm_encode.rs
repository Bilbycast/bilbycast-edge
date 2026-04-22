// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ingress-side processing for raw PCM / AES3-transparent RTP inputs
//! (ST 2110-30, ST 2110-31, `rtp_audio`).
//!
//! This module handles the two optional blocks that a PCM-only input can carry:
//!
//! * **`transcode`** — planar PCM reshape (sample rate / bit depth / channel
//!   routing) via [`crate::engine::audio_transcode::TranscodeStage`]. The
//!   output stays PCM-RTP so the flow's broadcast-channel shape is unchanged;
//!   downstream PCM consumers (ST 2110-30 outputs, `rtp_audio` outputs,
//!   SMPTE 302M-over-TS) keep working transparently.
//!
//! * **`audio_encode`** — PCM → compressed audio (AAC-LC / HE-AAC v1 / v2)
//!   or SMPTE 302M-in-TS muxed into an audio-only MPEG-TS stream. This
//!   **changes the broadcast shape** from PCM-RTP to MPEG-TS, so
//!   `validate_config` rejects PCM-only outputs on the same flow (see
//!   [`crate::config::validation`]).
//!
//!   Phase 6.5 first-light supports `aac_lc`, `he_aac_v1`, `he_aac_v2` (in-
//!   process fdk-aac + `TsMuxer::mux_audio_pre_adts`) and `s302m` (the
//!   lossless SMPTE 302M wrap via [`crate::engine::audio_302m::S302mOutputPipeline`]).
//!   `mp2` and `ac3` validate successfully but fail loudly at flow bring-up
//!   with `pid_bus_audio_encode_codec_not_supported_on_input` until the
//!   matching [`TsMuxer`] wrappers land. ST 2110-31 is only valid with
//!   `s302m` (validator enforces) — its 337M sub-frames ride through the
//!   302M path bit-for-bit.
//!
//! # Non-blocking invariants
//!
//! Consistent with [`super::input_transcode`] and the output-side replacers:
//!
//! * Codec-class work (SRC, channel matrix, bit-depth conversion, AAC
//!   encode) runs inside the caller's `tokio::task::block_in_place` — no
//!   separate thread.
//! * Scratch buffers live inside the stage and are reused across ticks;
//!   steady-state allocations are zero.
//! * Stats go through [`crate::engine::audio_transcode::TranscodeStats`]
//!   atomics (already lock-free).

use std::sync::Arc;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::config::models::AudioEncodeConfig;

use super::audio_302m::S302mOutputPipeline;
use super::audio_encode::{AudioCodec, AudioEncoder, EncoderParams};
use super::audio_transcode::{
    decode_pcm_be, resolve_transcode, BitDepth, ChannelMatrix, InputFormat, MatrixSource,
    TranscodeJson, TranscodeStage, TranscodeStats,
};
use super::packet::RtpPacket;
use super::rtmp::ts_mux::TsMuxer;
use crate::stats::collector::OutputStatsAccumulator;

/// Synthetic-TS audio PID used by Phase 6.5. Matches the `AUDIO_PID` the
/// shared [`TsMuxer`] already bakes in. The `TsEsDemuxer` on the receive
/// side learns this PID from the synthesised PMT — the value here is
/// documentation only, the wire bytes are authoritative.
#[allow(dead_code)]
pub const SYNTH_AUDIO_PID: u16 = 0x0101;

/// Errors raised when constructing a [`PcmInputProcessor`] or driving it.
#[derive(Debug)]
#[allow(dead_code)]
pub enum PcmInputError {
    /// Upstream PCM bit depth is not one of 16/20/24.
    UnsupportedBitDepth(u8),
    /// Config would not resolve (matrix shape mismatch, unknown preset, etc.).
    Config(String),
    /// Operator picked a codec that validates but has no Phase 6.5 runtime
    /// path (today: `mp2`, `ac3`). Flow bring-up translates this into a
    /// Critical event with `error_code = pid_bus_audio_encode_codec_not_supported_on_input`
    /// before any input task runs.
    UnsupportedAudioEncodeCodec { codec: String },
    /// Setting both `transcode` and `audio_encode` on the same input is not
    /// yet wired — the synthesiser would need to chain the stages. Rejected
    /// loudly rather than silently dropping one.
    TranscodePlusAudioEncodeNotImplemented,
    /// AAC-family encoder requires input sample rate == target sample rate
    /// (the in-process fdk-aac backend does not resample). Mismatch is
    /// rejected up-front so it doesn't silently produce corrupted audio.
    AacSampleRateMismatch { input_hz: u32, target_hz: u32 },
    /// AAC-family encoder requires input channels == target channels (no
    /// internal channel-matrix step). Mismatch is rejected up-front.
    AacChannelCountMismatch { input_ch: u8, target_ch: u8 },
    /// `AudioEncoder::spawn` failed (ffmpeg missing, libfdk_aac missing, ...).
    EncoderInit(String),
}

impl std::fmt::Display for PcmInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedBitDepth(bd) => {
                write!(f, "unsupported PCM bit depth {bd} (expected 16, 20, or 24)")
            }
            Self::Config(msg) => write!(f, "transcode config: {msg}"),
            Self::UnsupportedAudioEncodeCodec { codec } => write!(
                f,
                "audio_encode.codec '{codec}' is accepted at config time but has no \
                 Phase 6.5 runtime path on PCM / AES3 inputs. Supported: aac_lc, \
                 he_aac_v1, he_aac_v2, s302m. mp2/ac3 are deferred to a follow-up."
            ),
            Self::TranscodePlusAudioEncodeNotImplemented => write!(
                f,
                "`transcode` combined with `audio_encode` on a PCM input is not yet \
                 implemented in the Phase 6.5 runtime. Drop one of the two."
            ),
            Self::AacSampleRateMismatch { input_hz, target_hz } => write!(
                f,
                "audio_encode (AAC) requires input sample rate {input_hz} Hz to match \
                 target sample rate {target_hz} Hz. Either drop `audio_encode.sample_rate` \
                 so it defaults to the input rate, or match the input."
            ),
            Self::AacChannelCountMismatch { input_ch, target_ch } => write!(
                f,
                "audio_encode (AAC) requires input channel count {input_ch} to match \
                 target channel count {target_ch}. Either drop `audio_encode.channels` \
                 so it defaults to the input count, or match the input."
            ),
            Self::EncoderInit(e) => write!(f, "AAC encoder init failed: {e}"),
        }
    }
}

impl std::error::Error for PcmInputError {}

/// Stream_type a synthesised audio ES will publish under — derived from
/// the operator's `audio_encode.codec`. Exposed so the flow bring-up
/// cross-check (Phase 6.5 task 4) can compare against the slot's
/// declared `stream_type` without replicating the mapping.
pub fn stream_type_for_codec(codec: &str) -> Option<u8> {
    match codec {
        "aac_lc" | "he_aac_v1" | "he_aac_v2" => Some(0x0F),
        "mp2" => Some(0x04),
        "ac3" => Some(0x81),
        "s302m" => Some(0x06),
        _ => None,
    }
}

/// Returns `true` when the codec has a working synthesiser today.
/// `false` means the flow bring-up classifier should raise
/// `pid_bus_audio_encode_codec_not_supported_on_input`.
pub fn codec_supported_first_light(codec: &str) -> bool {
    matches!(codec, "aac_lc" | "he_aac_v1" | "he_aac_v2" | "s302m")
}

/// Ingress PCM processor — depacketizes, optionally reshapes via
/// [`TranscodeStage`], or (when `audio_encode` is set) synthesises an
/// audio-only MPEG-TS carrying the encoded ES and fresh PAT/PMT tables.
///
/// Returns `Ok(None)` when both `transcode` and `audio_encode` are unset — the
/// caller can then skip the stage entirely (zero cost).
pub struct PcmInputProcessor {
    stage: Option<TranscodeStage>,
    synth: Option<SynthBackend>,
    /// Shared stats handle (lock-free atomics). Exposed via [`Self::stats`]
    /// so the input task can register it on the flow accumulator.
    stats: Arc<TranscodeStats>,
}

/// Inner backends for the `audio_encode` path. Split by codec family: AAC
/// family shares one stateful pipeline; s302m has its own because
/// [`S302mOutputPipeline`] bundles encode + mux in one type.
enum SynthBackend {
    Aac(AacSynth),
    S302m(S302mSynth),
}

struct AacSynth {
    encoder: AudioEncoder,
    ts_mux: TsMuxer,
    /// Source PCM bit depth (for `decode_pcm_be`).
    bit_depth: BitDepth,
    /// Source PCM channel count (for `decode_pcm_be`).
    channels: u8,
    /// Source PCM sample rate (for advancing PTS when the encoder queue is drained).
    sample_rate: u32,
    /// Input RTP frame size (channels × bytes-per-sample). Used to reject
    /// malformed packets before reaching the decoder.
    frame_size: usize,
    /// Reused planar scratch — `[channel][frame]` f32 samples. Allocated at
    /// new() to its steady-state capacity; `decode_pcm_be` only pushes into
    /// it so the Vec never reallocates after the first packet.
    planar_scratch: Vec<Vec<f32>>,
    /// Monotonic 90 kHz PTS counter. Advances by `n_frames * 90_000 / sample_rate`
    /// per input PCM packet — mirrors the cadence the transcode stage uses.
    pts_90khz: u64,
    /// Cancellation token given to `AudioEncoder::spawn` — dropped with
    /// the processor to clean up any subprocess supervisor (ffmpeg path).
    #[allow(dead_code)]
    cancel: CancellationToken,
}

struct S302mSynth {
    pipeline: S302mOutputPipeline,
}

impl PcmInputProcessor {
    /// Build a new processor from the optional `transcode` and `audio_encode`
    /// blocks on a PCM / AES3-transparent input.
    ///
    /// * `in_sample_rate`, `in_bit_depth`, `in_channels` describe the input
    ///   PCM format as declared in the input's config (validation has already
    ///   checked these against 32k/44.1k/48k/88.2k/96k × 16/20/24 × 1..=16).
    /// * `ssrc`, `initial_seq`, `initial_timestamp` are the RTP identity the
    ///   output PCM-RTP packets should carry when only `transcode` is active.
    /// * `flow_id`, `input_id` are tracing labels passed down to the encoder.
    ///
    /// Returns `Ok(None)` when neither block is set.
    #[cfg(test)]
    pub fn new(
        in_sample_rate: u32,
        in_bit_depth: u8,
        in_channels: u8,
        transcode: Option<&TranscodeJson>,
        audio_encode: Option<&AudioEncodeConfig>,
        ssrc: u32,
        initial_seq: u16,
        initial_timestamp: u32,
    ) -> Result<Option<Self>, PcmInputError> {
        Self::new_with_labels(
            in_sample_rate,
            in_bit_depth,
            in_channels,
            transcode,
            audio_encode,
            ssrc,
            initial_seq,
            initial_timestamp,
            "flow",
            "input",
        )
    }

    /// Labelled variant — same as [`Self::new`] but threads `flow_id` /
    /// `input_id` into the encoder for tracing. Input tasks that know
    /// their IDs should call this path so encoder logs are correlated.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_labels(
        in_sample_rate: u32,
        in_bit_depth: u8,
        in_channels: u8,
        transcode: Option<&TranscodeJson>,
        audio_encode: Option<&AudioEncodeConfig>,
        ssrc: u32,
        initial_seq: u16,
        initial_timestamp: u32,
        flow_id: &str,
        input_id: &str,
    ) -> Result<Option<Self>, PcmInputError> {
        if transcode.is_none() && audio_encode.is_none() {
            return Ok(None);
        }

        let bit_depth = BitDepth::from_u8(in_bit_depth)
            .ok_or(PcmInputError::UnsupportedBitDepth(in_bit_depth))?;
        let input_fmt = InputFormat {
            sample_rate: in_sample_rate,
            bit_depth,
            channels: in_channels,
        };

        let stats = Arc::new(TranscodeStats::new());

        // Combo-set guard — transcode + audio_encode not yet chained.
        if transcode.is_some() && audio_encode.is_some() {
            return Err(PcmInputError::TranscodePlusAudioEncodeNotImplemented);
        }

        // audio_encode-only branch.
        if let Some(ae) = audio_encode {
            if !codec_supported_first_light(&ae.codec) {
                return Err(PcmInputError::UnsupportedAudioEncodeCodec {
                    codec: ae.codec.clone(),
                });
            }
            let synth = build_synth(input_fmt, ae, flow_id, input_id)?;
            return Ok(Some(Self {
                stage: None,
                synth: Some(synth),
                stats,
            }));
        }

        // transcode-only branch (unchanged from the pre-6.5 build).
        let tj = transcode.expect("checked above");
        let cfg = resolve_transcode(tj, input_fmt).map_err(PcmInputError::Config)?;
        let matrix: ChannelMatrix = cfg.channel_matrix.clone();
        let matrix_source = MatrixSource::static_(matrix);
        let stage = TranscodeStage::new(
            input_fmt,
            cfg,
            matrix_source,
            stats.clone(),
            ssrc,
            initial_seq,
            initial_timestamp,
        );

        Ok(Some(Self {
            stage: Some(stage),
            synth: None,
            stats,
        }))
    }

    /// Push one input RTP packet through the processor, returning zero or more
    /// output wire-ready buffers.
    ///
    /// * `transcode`-only: output is one or more PCM-RTP packets (same shape
    ///   as the input).
    /// * `audio_encode`: output is zero or more 188-byte TS packets
    ///   (PAT / PMT / audio PES), ready to wrap as `RtpPacket { is_raw_ts: true }`.
    pub fn process(&mut self, packet: &RtpPacket) -> Vec<Bytes> {
        if let Some(synth) = self.synth.as_mut() {
            return synth.process(packet);
        }
        match self.stage.as_mut() {
            Some(stage) => stage.process(packet),
            None => vec![packet.data.clone()],
        }
    }

    /// True when the processor changes the broadcast-channel shape from
    /// PCM-RTP to MPEG-TS — i.e. `audio_encode` is active.
    pub fn shape_change(&self) -> bool {
        self.synth.is_some()
    }

    /// Shared handle to the transcode stats counters.
    #[allow(dead_code)]
    pub fn stats(&self) -> Arc<TranscodeStats> {
        self.stats.clone()
    }
}

fn build_synth(
    input_fmt: InputFormat,
    ae: &AudioEncodeConfig,
    flow_id: &str,
    input_id: &str,
) -> Result<SynthBackend, PcmInputError> {
    if ae.codec == "s302m" {
        // S302mOutputPipeline targets 48 kHz / 2/4/6/8 ch / 16-20-24 bit.
        // Default to matching the input shape when the operator left
        // `audio_encode.channels` / `audio_encode.bit_depth` unset; the
        // pipeline promotes as needed.
        let out_channels = ae.channels.unwrap_or_else(|| {
            // 302M requires 2/4/6/8 — round mono up to stereo; cap >8 at 8.
            match input_fmt.channels {
                1 => 2,
                n if n >= 8 => 8,
                n if n >= 6 => 6,
                n if n >= 4 => 4,
                _ => 2,
            }
        });
        let out_bit_depth = input_fmt.bit_depth.as_u8();
        let out_packet_time_us: u32 = 4_000; // 4 ms — standard contribution.
        let pipeline =
            S302mOutputPipeline::new(input_fmt, out_channels, out_bit_depth, out_packet_time_us)
                .map_err(PcmInputError::Config)?;
        tracing::info!(
            flow_id,
            input_id,
            codec = "s302m",
            channels = out_channels,
            bit_depth = out_bit_depth,
            "pcm→ts synth: SMPTE 302M wrap"
        );
        return Ok(SynthBackend::S302m(S302mSynth { pipeline }));
    }

    // AAC family. Reject mismatched input/target shapes — the in-process
    // fdk-aac backend does not resample or matrix.
    let target_sr = ae.sample_rate.unwrap_or(input_fmt.sample_rate);
    let target_ch = ae.channels.unwrap_or(input_fmt.channels);
    if target_sr != input_fmt.sample_rate {
        return Err(PcmInputError::AacSampleRateMismatch {
            input_hz: input_fmt.sample_rate,
            target_hz: target_sr,
        });
    }
    if target_ch != input_fmt.channels {
        return Err(PcmInputError::AacChannelCountMismatch {
            input_ch: input_fmt.channels,
            target_ch,
        });
    }

    let codec = AudioCodec::parse(&ae.codec)
        .ok_or_else(|| PcmInputError::UnsupportedAudioEncodeCodec { codec: ae.codec.clone() })?;
    let bitrate_kbps = ae.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    let params = EncoderParams {
        codec,
        sample_rate: input_fmt.sample_rate,
        channels: input_fmt.channels,
        target_bitrate_kbps: bitrate_kbps,
        target_sample_rate: target_sr,
        target_channels: target_ch,
    };

    let cancel = CancellationToken::new();
    // `AudioEncoder::spawn` wants an `OutputStatsAccumulator` so it can
    // wire up its encode counters the same way output-side encoders do.
    // There is no output-side accumulator on the input path; create a
    // throwaway one — its stats are read via `encoder.stats_handle()`
    // if we ever surface them.
    let encoder_stats = Arc::new(OutputStatsAccumulator::new(
        format!("pcm-input-encode:{input_id}"),
        "pcm-input-encode".to_string(),
        "synthetic".to_string(),
    ));
    let encoder = AudioEncoder::spawn(
        params,
        cancel.clone(),
        flow_id.to_string(),
        input_id.to_string(),
        encoder_stats,
        None,
    )
    .map_err(|e| PcmInputError::EncoderInit(e.to_string()))?;

    // Muxer carries audio only, AAC with ADTS stream_type = 0x0F (no
    // registration descriptor).
    let mut ts_mux = TsMuxer::new();
    ts_mux.set_has_video(false);
    ts_mux.set_has_audio(true);
    ts_mux.set_audio_stream(0x0F, None);

    let frame_size = input_fmt.channels as usize * input_fmt.bit_depth.wire_bytes();
    let planar_scratch: Vec<Vec<f32>> = (0..input_fmt.channels).map(|_| Vec::new()).collect();

    tracing::info!(
        flow_id,
        input_id,
        codec = ae.codec.as_str(),
        sample_rate = input_fmt.sample_rate,
        channels = input_fmt.channels,
        "pcm→ts synth: AAC encode"
    );

    Ok(SynthBackend::Aac(AacSynth {
        encoder,
        ts_mux,
        bit_depth: input_fmt.bit_depth,
        channels: input_fmt.channels,
        sample_rate: input_fmt.sample_rate,
        frame_size,
        planar_scratch,
        pts_90khz: 0,
        cancel,
    }))
}

impl SynthBackend {
    fn process(&mut self, packet: &RtpPacket) -> Vec<Bytes> {
        match self {
            SynthBackend::Aac(s) => s.process(packet),
            SynthBackend::S302m(s) => s.process(packet),
        }
    }
}

impl AacSynth {
    fn process(&mut self, packet: &RtpPacket) -> Vec<Bytes> {
        // Strip the RTP header and decode to planar f32.
        let data = &packet.data;
        if data.len() < 12 {
            return Vec::new();
        }
        let cc = (data[0] & 0x0F) as usize;
        let header_len = 12 + cc * 4;
        if data.len() < header_len {
            return Vec::new();
        }
        let payload = &data[header_len..];
        if payload.is_empty() || payload.len() % self.frame_size != 0 {
            return Vec::new();
        }

        if decode_pcm_be(payload, self.bit_depth, self.channels, &mut self.planar_scratch).is_err()
        {
            return Vec::new();
        }
        let n_frames = self.planar_scratch[0].len();
        if n_frames == 0 {
            return Vec::new();
        }

        // Submit to the encoder and drain any ADTS frames it produced.
        let pts = self.pts_90khz;
        self.encoder.submit_planar(&self.planar_scratch, pts);
        if self.sample_rate > 0 {
            self.pts_90khz = self
                .pts_90khz
                .wrapping_add((n_frames as u64) * 90_000 / (self.sample_rate as u64));
        }

        let encoded = self.encoder.drain();
        if encoded.is_empty() {
            return Vec::new();
        }

        let mut out: Vec<Bytes> = Vec::with_capacity(encoded.len() * 4);
        for ef in encoded {
            // `ef.data` is a complete ADTS frame — feed it to the shared
            // muxer which wraps it in a PES and emits 188-byte TS packets.
            let ts_pkts = self.ts_mux.mux_audio_pre_adts(&ef.data, ef.pts);
            out.extend(ts_pkts);
        }
        out
    }
}

impl S302mSynth {
    fn process(&mut self, packet: &RtpPacket) -> Vec<Bytes> {
        self.pipeline.process(packet);
        self.pipeline.take_ready_ts_packets()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_none_returns_none() {
        let p = PcmInputProcessor::new(48_000, 24, 2, None, None, 0, 0, 0).expect("construct");
        assert!(p.is_none(), "all-None config must be idle");
    }

    #[test]
    fn combo_is_rejected_loudly() {
        let tj = TranscodeJson {
            sample_rate: Some(48_000),
            channels: Some(2),
            ..Default::default()
        };
        let ae = AudioEncodeConfig {
            codec: "aac_lc".to_string(),
            bitrate_kbps: Some(128),
            sample_rate: None,
            channels: None,
            silent_fallback: false,
        };
        let err = match PcmInputProcessor::new(48_000, 24, 2, Some(&tj), Some(&ae), 0, 0, 0) {
            Err(e) => e,
            Ok(_) => panic!("combo must error"),
        };
        assert!(matches!(
            err,
            PcmInputError::TranscodePlusAudioEncodeNotImplemented
        ));
    }

    #[test]
    fn unsupported_codec_errors_with_clear_variant() {
        let ae = AudioEncodeConfig {
            codec: "mp2".to_string(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
            silent_fallback: false,
        };
        let err = match PcmInputProcessor::new(48_000, 24, 2, None, Some(&ae), 0, 0, 0) {
            Err(e) => e,
            Ok(_) => panic!("mp2 not in first-light scope"),
        };
        assert!(matches!(
            err,
            PcmInputError::UnsupportedAudioEncodeCodec { codec } if codec == "mp2"
        ));
    }

    #[test]
    fn s302m_synth_constructs_and_flips_shape() {
        let ae = AudioEncodeConfig {
            codec: "s302m".to_string(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
            silent_fallback: false,
        };
        let p = PcmInputProcessor::new(48_000, 24, 2, None, Some(&ae), 0, 0, 0)
            .expect("construct")
            .expect("stage");
        assert!(p.shape_change(), "audio_encode=s302m must flip shape to TS");
    }

    #[test]
    fn aac_sr_mismatch_is_rejected() {
        let ae = AudioEncodeConfig {
            codec: "aac_lc".to_string(),
            bitrate_kbps: Some(128),
            sample_rate: Some(44_100),
            channels: None,
            silent_fallback: false,
        };
        let err = match PcmInputProcessor::new(48_000, 24, 2, None, Some(&ae), 0, 0, 0) {
            Err(e) => e,
            Ok(_) => panic!("sr mismatch must error"),
        };
        assert!(matches!(
            err,
            PcmInputError::AacSampleRateMismatch { input_hz: 48_000, target_hz: 44_100 }
        ));
    }

    #[test]
    fn stream_type_for_codec_covers_first_light() {
        assert_eq!(stream_type_for_codec("aac_lc"), Some(0x0F));
        assert_eq!(stream_type_for_codec("he_aac_v1"), Some(0x0F));
        assert_eq!(stream_type_for_codec("he_aac_v2"), Some(0x0F));
        assert_eq!(stream_type_for_codec("mp2"), Some(0x04));
        assert_eq!(stream_type_for_codec("ac3"), Some(0x81));
        assert_eq!(stream_type_for_codec("s302m"), Some(0x06));
        assert_eq!(stream_type_for_codec("opus"), None);
    }

    #[test]
    fn first_light_supported_set() {
        assert!(codec_supported_first_light("aac_lc"));
        assert!(codec_supported_first_light("he_aac_v1"));
        assert!(codec_supported_first_light("he_aac_v2"));
        assert!(codec_supported_first_light("s302m"));
        assert!(!codec_supported_first_light("mp2"));
        assert!(!codec_supported_first_light("ac3"));
        assert!(!codec_supported_first_light("opus"));
    }

    #[test]
    fn transcode_only_constructs() {
        let tj = TranscodeJson {
            sample_rate: Some(48_000),
            channels: Some(1),
            channel_map_preset: Some("stereo_to_mono_3db".to_string()),
            ..Default::default()
        };
        let p = PcmInputProcessor::new(48_000, 24, 2, Some(&tj), None, 0x1234, 0, 0)
            .expect("construct")
            .expect("stage");
        assert!(!p.shape_change());
    }
}
