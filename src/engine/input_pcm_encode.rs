// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ingress-side processing for raw PCM-RTP inputs (ST 2110-30, `rtp_audio`).
//!
//! This module handles the two optional blocks that a PCM-only input can carry:
//!
//! * **`transcode`** — planar PCM reshape (sample rate / bit depth / channel
//!   routing) via [`crate::engine::audio_transcode::TranscodeStage`]. The
//!   output stays PCM-RTP so the flow's broadcast-channel shape is unchanged;
//!   downstream PCM consumers (ST 2110-30 outputs, `rtp_audio` outputs,
//!   SMPTE 302M-over-TS) keep working transparently.
//!
//! * **`audio_encode`** — PCM → compressed audio (AAC-LC / HE-AAC / MP2 / AC-3)
//!   muxed into an audio-only MPEG-TS stream. This **changes the broadcast
//!   shape** from PCM-RTP to MPEG-TS, so `validate_config` rejects PCM-only
//!   outputs on the same flow (see [`crate::config::validation`]).
//!
//!   Runtime support for `audio_encode` on a PCM input is deferred — the
//!   current build accepts the field at config time (so the manager UI can
//!   surface it) but returns [`PcmInputError::AudioEncodeNotYetImplemented`]
//!   when a flow with such an input starts. The config + validation work done
//!   in this PR establishes the API surface; the muxing pipeline (PCM
//!   accumulator → `AudioEncoder` → audio-only PAT/PMT/PES → TS packetization)
//!   lands in a follow-up. Until then the input task logs a clear error and
//!   falls back to PCM passthrough so the flow still runs.
//!
//! # Non-blocking invariants
//!
//! Consistent with [`super::input_transcode`] and the output-side replacers:
//!
//! * Codec-class work (SRC, channel matrix, bit-depth conversion) runs inside
//!   the caller's `tokio::task::block_in_place` — no separate thread.
//! * Scratch buffers live inside [`TranscodeStage`] and are reused across
//!   ticks; steady-state allocations are zero.
//! * Stats go through [`crate::engine::audio_transcode::TranscodeStats`]
//!   atomics (already lock-free).

use std::sync::Arc;

use bytes::Bytes;

use crate::config::models::AudioEncodeConfig;

use super::audio_transcode::{
    resolve_transcode, BitDepth, ChannelMatrix, InputFormat, MatrixSource, TranscodeJson,
    TranscodeStage, TranscodeStats,
};

/// Errors raised when constructing a [`PcmInputProcessor`] or driving it.
#[derive(Debug)]
#[allow(dead_code)]
pub enum PcmInputError {
    /// Upstream PCM bit depth is not one of 16/20/24.
    UnsupportedBitDepth(u8),
    /// Config would not resolve (matrix shape mismatch, unknown preset, etc.).
    Config(String),
    /// `audio_encode` on a PCM-only input is plumbed through the config +
    /// validation layers but the runtime muxer is not implemented yet.
    AudioEncodeNotYetImplemented { codec: String },
}

impl std::fmt::Display for PcmInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedBitDepth(bd) => {
                write!(f, "unsupported PCM bit depth {bd} (expected 16, 20, or 24)")
            }
            Self::Config(msg) => write!(f, "transcode config: {msg}"),
            Self::AudioEncodeNotYetImplemented { codec } => write!(
                f,
                "audio_encode (codec '{codec}') on a PCM-only input is accepted at \
                 config time but not yet supported at runtime; the input will fall \
                 back to raw PCM passthrough. Planned for a follow-up release."
            ),
        }
    }
}

impl std::error::Error for PcmInputError {}

/// Ingress PCM processor — depacketizes, optionally reshapes via
/// [`TranscodeStage`], optionally encodes to audio-only MPEG-TS.
///
/// Returns `Ok(None)` when both `transcode` and `audio_encode` are unset — the
/// caller can then skip the stage entirely (zero cost).
pub struct PcmInputProcessor {
    stage: Option<TranscodeStage>,
    /// When set, the operator configured `audio_encode` but the runtime muxer
    /// isn't available yet. The input task has already logged the fallback;
    /// this flag is kept so `shape_change()` can still report the *intended*
    /// broadcast shape for flow-level compatibility checks. Today
    /// `shape_change()` always returns `false` since we fall back to
    /// passthrough — once the muxer lands this flag flips to `true`.
    #[allow(dead_code)]
    audio_encode_requested: bool,
    /// Shared stats handle (lock-free atomics). Exposed via [`Self::stats`]
    /// so the input task can register it on the flow accumulator.
    stats: Arc<TranscodeStats>,
}

impl PcmInputProcessor {
    /// Build a new processor from the optional `transcode` and `audio_encode`
    /// blocks on a PCM-only input (ST 2110-30 or `rtp_audio`).
    ///
    /// * `in_sample_rate`, `in_bit_depth`, `in_channels` describe the input
    ///   PCM format as declared in the input's config (validation has already
    ///   checked these against 32k/44.1k/48k/88.2k/96k × 16/20/24 × 1..=16).
    /// * `ssrc`, `initial_seq`, `initial_timestamp` are the RTP identity the
    ///   output PCM-RTP packets should carry when only `transcode` is active.
    ///
    /// Returns `Ok(None)` when neither block is set.
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
        if transcode.is_none() && audio_encode.is_none() {
            return Ok(None);
        }

        // Gate audio_encode with a clear "not yet wired" error — see module
        // docs. The input task that receives this error should log it as
        // `warn` and continue without the stage (passthrough), so the flow
        // still runs.
        if let Some(ae) = audio_encode {
            return Err(PcmInputError::AudioEncodeNotYetImplemented {
                codec: ae.codec.clone(),
            });
        }

        // Transcode-only path. Build a TranscodeStage with the input shape.
        let bit_depth = BitDepth::from_u8(in_bit_depth)
            .ok_or(PcmInputError::UnsupportedBitDepth(in_bit_depth))?;
        let input = InputFormat {
            sample_rate: in_sample_rate,
            bit_depth,
            channels: in_channels,
        };

        let tj = transcode.expect("checked above");
        let cfg = resolve_transcode(tj, input).map_err(PcmInputError::Config)?;

        let matrix: ChannelMatrix = cfg.channel_matrix.clone();
        let matrix_source = MatrixSource::static_(matrix);

        let stats = Arc::new(TranscodeStats::new());
        let stage = TranscodeStage::new(
            input,
            cfg,
            matrix_source,
            stats.clone(),
            ssrc,
            initial_seq,
            initial_timestamp,
        );

        Ok(Some(Self {
            stage: Some(stage),
            audio_encode_requested: false,
            stats,
        }))
    }

    /// Push one input RTP packet through the processor, returning zero or more
    /// output RTP packets (ready-to-broadcast wire bytes).
    ///
    /// When no transcode stage is active (e.g. a follow-up build path where
    /// `audio_encode` is still unimplemented and we're in passthrough mode),
    /// the input is echoed unchanged.
    pub fn process(&mut self, packet: &super::packet::RtpPacket) -> Vec<Bytes> {
        match self.stage.as_mut() {
            Some(stage) => stage.process(packet),
            None => vec![packet.data.clone()],
        }
    }

    /// True when the processor changes the broadcast-channel shape from
    /// PCM-RTP to MPEG-TS (i.e. `audio_encode` is active and functional).
    /// Currently always `false` — the runtime audio-encode path is still
    /// deferred and the validator rejects flow configs that assume otherwise
    /// only when the encoder is actually active, which it is not yet.
    pub fn shape_change(&self) -> bool {
        false
    }

    /// Shared handle to the transcode stats counters.
    #[allow(dead_code)]
    pub fn stats(&self) -> Arc<TranscodeStats> {
        self.stats.clone()
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
    fn audio_encode_returns_clear_error() {
        let ae = AudioEncodeConfig {
            codec: "aac_lc".to_string(),
            bitrate_kbps: Some(128),
            sample_rate: None,
            channels: None,
            silent_fallback: false,
        };
        match PcmInputProcessor::new(48_000, 24, 2, None, Some(&ae), 0, 0, 0) {
            Err(PcmInputError::AudioEncodeNotYetImplemented { codec }) => {
                assert_eq!(codec, "aac_lc");
            }
            Err(other) => panic!("wrong error variant: {other:?}"),
            Ok(_) => panic!("audio_encode must currently return an error"),
        }
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
