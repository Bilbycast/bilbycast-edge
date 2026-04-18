// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ingress-side audio + video transcoding composer for TS-carrying inputs.
//!
//! Wraps [`super::ts_audio_replace::TsAudioReplacer`] and
//! [`super::ts_video_replace::TsVideoReplacer`] in the same audio-first-then-video
//! order already used by the TS outputs (see `output_srt.rs`), so every Group A
//! input (RTP, UDP, SRT, RIST, RTMP, RTSP, WebRTC/WHIP, WebRTC/WHEP) can
//! normalise its feed *once* before the flow's broadcast channel and amortise
//! codec work across all attached outputs.
//!
//! The design goals come straight from the root CLAUDE.md:
//!
//! * **Never block the data path** — codec calls are synchronous and take
//!   single-digit milliseconds per frame. Callers wrap [`InputTranscoder::process`]
//!   in `tokio::task::block_in_place` exactly like the output-side does.
//! * **Lock-free on hot paths** — no mutexes, no allocations once the scratch
//!   buffers reach steady state. The composer owns two `Vec<u8>` scratches and
//!   reuses them across calls via `.clear() + extend_from_slice`.
//! * **Drop rather than stall** — if the underlying replacer produces no bytes
//!   for a tick (audio bootstrap, PMT not yet seen), `process` returns an
//!   empty slice; the input task just skips the `broadcast_tx.send` for that
//!   chunk and keeps going.
//!
//! See `docs/transcoding.md` for the licensing / feature-flag matrix.

use std::sync::Arc;

use crate::config::models::{AudioEncodeConfig, VideoEncodeConfig};

use super::audio_transcode::TranscodeJson;
use super::ts_audio_replace::{TsAudioReplaceError, TsAudioReplacer};
use super::ts_video_replace::{TsVideoReplaceError, TsVideoReplacer, VideoEncodeStats};

/// Errors raised when constructing an [`InputTranscoder`].
#[derive(Debug)]
#[allow(dead_code)]
pub enum InputTranscoderError {
    /// The audio replacer rejected its config (unknown codec, unsupported
    /// codec-for-TS, feature flag missing).
    Audio(TsAudioReplaceError),
    /// The video replacer rejected its config (unknown backend, backend
    /// feature not compiled in, video-engine missing).
    Video(TsVideoReplaceError),
}

impl std::fmt::Display for InputTranscoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Audio(e) => write!(f, "input audio transcode: {e}"),
            Self::Video(e) => write!(f, "input video transcode: {e}"),
        }
    }
}

impl std::error::Error for InputTranscoderError {}

impl From<TsAudioReplaceError> for InputTranscoderError {
    fn from(e: TsAudioReplaceError) -> Self {
        Self::Audio(e)
    }
}

impl From<TsVideoReplaceError> for InputTranscoderError {
    fn from(e: TsVideoReplaceError) -> Self {
        Self::Video(e)
    }
}

/// Composite ingress transcoder. Routes raw 188-byte-aligned MPEG-TS through
/// optional audio and video ES replacers in order.
///
/// Not `Sync` (the inner codecs hold raw C state), but `Send` so the whole
/// instance can be moved to the input task at spawn time.
pub struct InputTranscoder {
    audio: Option<TsAudioReplacer>,
    video: Option<TsVideoReplacer>,
    /// Scratch buffer that holds the audio-replacer output. Reused across
    /// ticks to avoid steady-state allocations.
    scratch_a: Vec<u8>,
    /// Scratch buffer that holds the final output (after the video replacer,
    /// or after the audio replacer when video is absent). Borrowed out via
    /// `process`.
    scratch_b: Vec<u8>,
}

impl InputTranscoder {
    /// Build a transcoder from optional `audio_encode`, `transcode`, and
    /// `video_encode` blocks.
    ///
    /// Returns `Ok(None)` when all three are `None` — the caller should skip
    /// the stage entirely in that case (zero cost, no scratch buffers
    /// allocated, no codec state opened).
    pub fn new(
        audio_encode: Option<&AudioEncodeConfig>,
        transcode: Option<&TranscodeJson>,
        video_encode: Option<&VideoEncodeConfig>,
    ) -> Result<Option<Self>, InputTranscoderError> {
        if audio_encode.is_none() && transcode.is_none() && video_encode.is_none() {
            return Ok(None);
        }

        let audio = match audio_encode {
            Some(ae) => Some(TsAudioReplacer::new(ae, transcode.cloned())?),
            None => None,
        };
        let video = match video_encode {
            Some(ve) => Some(TsVideoReplacer::new(ve)?),
            None => None,
        };

        // If `transcode` is set but `audio_encode` is not, the transcode block
        // has no encoder to feed — ignore silently to match the output-side
        // contract (transcode is a sub-block of audio_encode in practice).
        // We still return `Some` if video_encode is set.
        if audio.is_none() && video.is_none() {
            return Ok(None);
        }

        Ok(Some(Self {
            audio,
            video,
            scratch_a: Vec::with_capacity(32 * 1024),
            scratch_b: Vec::with_capacity(32 * 1024),
        }))
    }

    /// Pass one chunk of raw 188-byte-aligned TS through the audio stage, then
    /// the video stage. Returns the transformed output as a borrowed slice of
    /// the internal scratch buffer. The slice is valid until the next call to
    /// `process` / `flush`, so the caller must copy or clone before the next
    /// invocation.
    ///
    /// An empty return slice means "nothing to emit this tick" — either input
    /// was empty, or the replacers buffered the input into their internal PES
    /// accumulators waiting for more bytes.
    pub fn process<'a>(&'a mut self, input_ts: &'a [u8]) -> &'a [u8] {
        // Reset scratch buffers without giving up capacity.
        self.scratch_a.clear();
        self.scratch_b.clear();

        // Stage 1: audio. When absent, pass the input through unchanged.
        let after_audio: &[u8] = match self.audio.as_mut() {
            Some(a) => {
                a.process(input_ts, &mut self.scratch_a);
                &self.scratch_a
            }
            None => input_ts,
        };

        // Stage 2: video. When absent, return the audio-stage output directly.
        match self.video.as_mut() {
            Some(v) => {
                v.process(after_audio, &mut self.scratch_b);
                &self.scratch_b
            }
            None => after_audio,
        }
    }

    /// Drain buffered PES / encoder state into `output`. Call on graceful
    /// shutdown. No-op for unset stages.
    #[allow(dead_code)]
    pub fn flush(&mut self, output: &mut Vec<u8>) {
        if let Some(a) = self.audio.as_mut() {
            let mut tmp = Vec::new();
            a.flush(&mut tmp);
            // Feed the drained audio bytes through the video stage too.
            if let Some(v) = self.video.as_mut() {
                v.process(&tmp, output);
            } else {
                output.extend_from_slice(&tmp);
            }
        }
        if let Some(v) = self.video.as_mut() {
            v.flush(output);
        }
    }

    /// Returns a shared handle to the video-encode stats counters if a video
    /// stage is active. The input task registers this with the flow
    /// accumulator at startup so the manager dashboard can surface ingress
    /// codec health.
    pub fn video_stats(&self) -> Option<Arc<VideoEncodeStats>> {
        self.video.as_ref().map(|v| v.stats_handle())
    }

    /// Whether the composer has an active audio-encode stage.
    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Whether the composer has an active video-encode stage.
    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// Human-readable description of what stages are active, for logging.
    pub fn describe(&self) -> String {
        let a = self
            .audio
            .as_ref()
            .map(|a| format!("audio→{}", a.target_description()))
            .unwrap_or_default();
        let v = self
            .video
            .as_ref()
            .map(|v| format!("video→{}", v.target_description()))
            .unwrap_or_default();
        match (a.is_empty(), v.is_empty()) {
            (false, false) => format!("{a}, {v}"),
            (false, true) => a,
            (true, false) => v,
            (true, true) => "<idle>".to_string(),
        }
    }
}

/// Register an ingress transcoder's stats handles + static summary descriptor
/// with the flow stats accumulator, keyed by `input_id`. Called at flow start
/// by each Group A input right after `InputTranscoder::new` succeeds (and
/// always — even when the transcoder is `None` — so the UI can show the
/// "passthrough" pipeline tag when this input is the active one). Per-input
/// keying means a multi-input flow reports only the active input's pipeline
/// after a seamless switch; the passive inputs' registrations stay in place
/// for later reactivation but never leak into the snapshot.
pub fn register_ingress_stats(
    flow_stats: &crate::stats::collector::FlowStatsAccumulator,
    input_id: &str,
    transcoder: Option<&InputTranscoder>,
    audio_encode: Option<&AudioEncodeConfig>,
    video_encode: Option<&VideoEncodeConfig>,
) {
    use crate::stats::collector::EgressMediaSummaryStatic;

    // Wire the live video encode counters if a video stage is active.
    if let Some(t) = transcoder {
        if let Some(vs) = t.video_stats() {
            if let Some(enc) = video_encode {
                let backend = match enc.codec.as_str() {
                    "x264" | "x265" => enc.codec.clone(),
                    "h264_nvenc" | "hevc_nvenc" => "nvenc".to_string(),
                    other => other.to_string(),
                };
                let target_codec = match enc.codec.as_str() {
                    "x264" | "h264_nvenc" => "h264",
                    "x265" | "hevc_nvenc" => "hevc",
                    other => other,
                };
                flow_stats.set_input_video_encode_stats(
                    input_id,
                    vs,
                    String::new(),
                    target_codec.to_string(),
                    enc.width.unwrap_or(0),
                    enc.height.unwrap_or(0),
                    match (enc.fps_num, enc.fps_den) {
                        (Some(n), Some(d)) if d > 0 => n as f32 / d as f32,
                        _ => 0.0,
                    },
                    enc.bitrate_kbps.unwrap_or(0),
                    backend,
                );
            }
        }
    }

    // Static summary descriptor. Every ingress pipeline ends in raw TS on
    // the flow broadcast channel, hence `transport_mode = "ts"`. Passthrough
    // flags mirror the output-side convention: `true` means the stage leaves
    // that essence untouched.
    let has_audio_stage = audio_encode.is_some()
        || transcoder.map(|t| t.has_audio()).unwrap_or(false);
    let has_video_stage = video_encode.is_some()
        || transcoder.map(|t| t.has_video()).unwrap_or(false);
    flow_stats.set_ingress_static(
        input_id,
        EgressMediaSummaryStatic {
            transport_mode: Some("ts".to_string()),
            video_passthrough: !has_video_stage,
            audio_passthrough: !has_audio_stage,
            audio_only: false,
        },
    );
}

/// Parse the length of an RTP header including any CSRC list and extension
/// header. Returns `None` if the input is too short or malformed. Does not
/// validate the packet is RTP — callers must check that separately (e.g. via
/// [`crate::util::rtp_parse::is_likely_rtp`]).
fn rtp_header_length(data: &[u8]) -> Option<usize> {
    if data.len() < 12 {
        return None;
    }
    let cc = (data[0] & 0x0F) as usize;
    let has_ext = (data[0] & 0x10) != 0;
    let mut len = 12 + cc * 4;
    if has_ext {
        if data.len() < len + 4 {
            return None;
        }
        let ext_len = u16::from_be_bytes([data[len + 2], data[len + 3]]) as usize;
        len += 4 + ext_len * 4;
    }
    if data.len() < len {
        return None;
    }
    Some(len)
}

/// Helper that every Group A input uses at the point of publishing a freshly
/// received packet onto the flow's broadcast channel. When `transcoder` is
/// `None` the packet is sent verbatim; otherwise the TS payload (after
/// stripping an optional RTP header) is run through the transcoder inside
/// `tokio::task::block_in_place` and republished as raw TS.
///
/// Returns `true` when something was sent, `false` when the packet was
/// swallowed (e.g. the replacer is still bootstrapping its PMT discovery).
pub fn publish_input_packet(
    transcoder: &mut Option<InputTranscoder>,
    broadcast_tx: &tokio::sync::broadcast::Sender<super::packet::RtpPacket>,
    packet: super::packet::RtpPacket,
) -> bool {
    use bytes::Bytes;
    let Some(t) = transcoder.as_mut() else {
        return broadcast_tx.send(packet).is_ok();
    };

    // Locate the TS bytes. For `is_raw_ts: true` (or SRT's `Unknown` fallback)
    // the whole payload is TS. For RTP-wrapped TS, strip the RTP header.
    let ts_offset = if packet.is_raw_ts {
        0
    } else {
        match rtp_header_length(&packet.data) {
            Some(n) => n,
            None => {
                // Malformed header — publish unchanged, let the output side
                // deal with it (or the next packet recovers).
                return broadcast_tx.send(packet).is_ok();
            }
        }
    };
    let ts_in = &packet.data[ts_offset..];

    // Codec-class work runs on a blocking worker so the reactor is never
    // stalled. Matches the output-side contract (see output_srt.rs:787,798).
    let out: Vec<u8> = tokio::task::block_in_place(|| t.process(ts_in).to_vec());
    if out.is_empty() {
        // Replacer buffered the input; nothing to emit this tick.
        return false;
    }

    let new_packet = super::packet::RtpPacket {
        data: Bytes::from(out),
        sequence_number: packet.sequence_number,
        rtp_timestamp: packet.rtp_timestamp,
        recv_time_us: packet.recv_time_us,
        // After the input transcode stage the flow always carries raw TS.
        is_raw_ts: true,
    };
    broadcast_tx.send(new_packet).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_returns_none() {
        let t = InputTranscoder::new(None, None, None).expect("construct");
        assert!(t.is_none(), "all-None config must be idle (no stage)");
    }

    #[test]
    fn transcode_without_audio_encode_is_none() {
        // A transcode block without audio_encode has no encoder to feed — the
        // composer treats it as idle (matches the output-side contract where
        // transcode is a sub-block of audio_encode).
        let tj = TranscodeJson {
            sample_rate: Some(48_000),
            ..Default::default()
        };
        let t = InputTranscoder::new(None, Some(&tj), None).expect("construct");
        assert!(t.is_none());
    }

    #[test]
    fn audio_only_stage_passes_non_ts_through() {
        let ae = AudioEncodeConfig {
            codec: "aac_lc".to_string(),
            bitrate_kbps: Some(128),
            sample_rate: None,
            channels: None,
        };
        let mut t = InputTranscoder::new(Some(&ae), None, None)
            .expect("construct")
            .expect("stage");
        // Non-aligned input is passed through verbatim by TsAudioReplacer.
        let input = b"not a ts packet";
        let out = t.process(input);
        assert_eq!(out, input);
    }
}
