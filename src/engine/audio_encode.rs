// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! ffmpeg-sidecar audio encoder.
//!
//! Wraps a long-running ffmpeg subprocess as a PCM → compressed-audio
//! encoder. One persistent ffmpeg process per encoded output. Planar f32
//! PCM in via [`AudioEncoder::submit_planar`] (converted internally to
//! interleaved s32le on stdin), framed encoded ES out via
//! [`AudioEncoder::drain`]. stderr is drained concurrently for diagnostics.
//!
//! ## Layering
//!
//! ```text
//! Vec<Vec<f32>>  (planar PCM, channel-major, from AacDecoder)
//!         │
//!         ▼
//! ┌──────────────────────┐
//! │  AudioEncoder        │
//! │  ─ pcm_tx (bounded)  │ ◄── submit_planar() try_send, drop-on-full
//! │  ─ ffmpeg subprocess │
//! │  ─ stdin writer task │
//! │  ─ stdout reader +   │
//! │    per-codec framer  │
//! │  ─ stderr drainer    │
//! │  ─ supervisor        │ (restart with backoff, up to MAX_RESTARTS)
//! └──────────┬───────────┘
//!            ▼
//!  Vec<EncodedFrame>  (codec ES frames with PTS, via drain())
//! ```
//!
//! ## Pure Rust binary
//!
//! ffmpeg is invoked as a runtime subprocess via `tokio::process::Command`,
//! never linked. `testbed/check-binary-purity.sh` continues to pass.
//! When ffmpeg is missing at startup, [`AudioEncoder::spawn`] returns
//! [`AudioEncoderError::FfmpegNotFound`] and the caller is expected to
//! emit a clear failure event and tear down the affected output. Outputs
//! that don't request `audio_encode` keep working unchanged.
//!
//! ## What this module does NOT do
//!
//! - It does NOT decode the input. Decoding is the job of
//!   [`crate::engine::audio_decode::AacDecoder`] (Phase A). The decoder
//!   feeds planar f32 into [`AudioEncoder::submit_planar`].
//! - It does NOT RTP-packetize or container-mux the encoded output.
//!   The per-codec framer emits raw codec frames; the consumer (RTMP
//!   FLV writer, HLS TS muxer, WebRTC RTP packetizer) handles the
//!   container.
//! - It does NOT block. `submit_planar` is non-blocking via a bounded
//!   channel with drop-on-full. Slow ffmpeg → `packets_dropped`, never
//!   cascading backpressure.

#![allow(dead_code)]

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::OutputStatsAccumulator;

// ── Public types ────────────────────────────────────────────────────────────

/// Codecs Phase B can produce. The codec×output validity matrix is enforced
/// in `config/validation.rs` (RTMP=AAC family, HLS=AAC+MP2+AC3, WebRTC=Opus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    AacLc,
    HeAacV1,
    HeAacV2,
    Opus,
    Mp2,
    Ac3,
}

impl AudioCodec {
    /// Parse the wire-format codec name (used in `AudioEncodeConfig.codec`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "aac_lc" => Some(Self::AacLc),
            "he_aac_v1" => Some(Self::HeAacV1),
            "he_aac_v2" => Some(Self::HeAacV2),
            "opus" => Some(Self::Opus),
            "mp2" => Some(Self::Mp2),
            "ac3" => Some(Self::Ac3),
            _ => None,
        }
    }

    /// Default bitrate (kbps) when the operator does not specify one.
    pub fn default_bitrate_kbps(&self) -> u32 {
        match self {
            Self::AacLc => 128,
            Self::HeAacV1 => 64,
            Self::HeAacV2 => 32,
            Self::Opus => 96,
            Self::Mp2 => 192,
            Self::Ac3 => 192,
        }
    }

    /// Wire identifier as it appears in tracing / event payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AacLc => "aac_lc",
            Self::HeAacV1 => "he_aac_v1",
            Self::HeAacV2 => "he_aac_v2",
            Self::Opus => "opus",
            Self::Mp2 => "mp2",
            Self::Ac3 => "ac3",
        }
    }
}

/// Resolved encoder parameters: input PCM format + target codec / bitrate
/// / sample rate / channel count.
#[derive(Debug, Clone)]
pub struct EncoderParams {
    pub codec: AudioCodec,
    /// Input PCM sample rate (Hz). Determined from the upstream decoder
    /// (e.g. AacDecoder::sample_rate()).
    pub sample_rate: u32,
    /// Input PCM channel count. From AacDecoder::channels().
    pub channels: u8,
    /// Resolved target bitrate in kbps. Falls back to
    /// [`AudioCodec::default_bitrate_kbps`] if the operator did not set one.
    pub target_bitrate_kbps: u32,
    /// Resolved target sample rate (Hz). Defaults to `sample_rate`.
    pub target_sample_rate: u32,
    /// Resolved target channel count. Defaults to `channels`.
    pub target_channels: u8,
}

/// Construction / runtime errors from the encoder.
#[derive(Debug)]
pub enum AudioEncoderError {
    /// ffmpeg was not found in `PATH`.
    FfmpegNotFound,
    /// `tokio::process::Command::spawn` failed.
    FfmpegSpawnFailed(String),
    /// Caller passed a codec that has no ffmpeg command line mapping.
    UnsupportedCodec(AudioCodec),
    /// Input PCM format is invalid (e.g. zero channels).
    InvalidPcmFormat { reason: String },
    /// Supervisor exhausted its restart budget.
    EncoderCrashed { exit_status: String, stderr_tail: String },
    /// Internal channel was closed unexpectedly.
    ChannelClosed,
    /// HE-AAC v1/v2 was requested but the local ffmpeg doesn't have the
    /// `libfdk_aac` encoder built in. The native `aac` encoder hard-rejects
    /// the `aac_he` and `aac_he_v2` profiles, so we refuse to spawn rather
    /// than producing zero output / cryptic ffmpeg `-22` errors.
    HeAacRequiresLibfdk,
}

impl std::fmt::Display for AudioEncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FfmpegNotFound => write!(
                f,
                "ffmpeg required for audio_encode but not found in PATH"
            ),
            Self::FfmpegSpawnFailed(e) => write!(f, "failed to spawn ffmpeg: {e}"),
            Self::UnsupportedCodec(c) => write!(f, "unsupported audio codec: {}", c.as_str()),
            Self::InvalidPcmFormat { reason } => write!(f, "invalid PCM format: {reason}"),
            Self::EncoderCrashed { exit_status, stderr_tail } => write!(
                f,
                "audio encoder crashed (exit: {exit_status}): {stderr_tail}"
            ),
            Self::ChannelClosed => write!(f, "audio encoder channel closed"),
            Self::HeAacRequiresLibfdk => write!(
                f,
                "he_aac_v1/he_aac_v2 require an ffmpeg build with libfdk_aac \
                 (the native ffmpeg `aac` encoder does not support the aac_he / \
                 aac_he_v2 profiles). Install ffmpeg with `--enable-libfdk-aac` \
                 (e.g. via the homebrew-ffmpeg/ffmpeg tap on macOS) and restart \
                 the edge node, or switch this output to `aac_lc`."
            ),
        }
    }
}

impl std::error::Error for AudioEncoderError {}

/// One PCM chunk submitted to the encoder. `data` is interleaved s32le.
#[derive(Debug, Clone)]
pub struct PcmChunk {
    pub data: Bytes,
    /// Upstream PTS (90 kHz from the demuxer); propagated through the
    /// encoder so output framers can attach the right timestamp to the
    /// encoded frame.
    pub pts: u64,
}

/// One encoded audio frame emitted by the framer.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub pts: u64,
}

// ── Constants ───────────────────────────────────────────────────────────────

/// Maximum bytes accepted per stdin write before timing out and triggering
/// a restart. Catches a hung ffmpeg process.
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounded PCM input channel depth. At 48 kHz / stereo / s32le that's
/// roughly `(64 * 1024 * 4 * 2) / (48000 * 4 * 2)` ≈ 0.7 s of audio buffer
/// for the typical case — plenty for ffmpeg's input pacing without
/// risking unbounded growth on a stuck process.
const PCM_CHANNEL_DEPTH: usize = 64;

/// Bounded encoded output channel depth. Each entry is a single codec
/// frame (≤ ~2 KB), so 256 entries is generous.
const ENCODED_CHANNEL_DEPTH: usize = 256;

/// Maximum process restarts within [`RESTART_WINDOW`] before the
/// supervisor gives up and reports `EncoderCrashed`.
const MAX_RESTARTS: u32 = 5;

/// Sliding window for [`MAX_RESTARTS`].
const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Maximum exponential-backoff sleep between restarts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long to wait for ffmpeg to exit naturally during graceful shutdown
/// before SIGKILL.
const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum stderr tail (bytes) captured for crash reports.
const STDERR_TAIL_BYTES: usize = 4096;

// ── Public API ──────────────────────────────────────────────────────────────

/// Check whether ffmpeg is available on this system. Mirrors
/// [`crate::engine::thumbnail::check_ffmpeg_available`] so callers can
/// gate `audio_encode`-using outputs at startup with a single boolean.
pub fn check_ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns true if the local ffmpeg has the `libfdk_aac` encoder built in.
///
/// HE-AAC v1 / v2 encoding is only reliably supported by `libfdk_aac` —
/// ffmpeg's native `aac` encoder hard-rejects the `aac_he` and `aac_he_v2`
/// profiles with "Profile not supported!", and `aac_at` (Apple AudioToolbox)
/// silently downgrades to LC. We surface this at output startup so HE-AAC
/// configs fail loudly with a clear message instead of producing zero
/// segments / cryptic ffmpeg `-22` errors downstream.
///
/// Cached on first call; subsequent calls return the cached result so we
/// don't fork ffmpeg per output.
pub fn check_libfdk_aac_available() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();
        match out {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.lines().any(|line| {
                    // ffmpeg lists encoders as "<flags> <name> ..."; look
                    // for the libfdk_aac entry specifically.
                    let trimmed = line.trim_start();
                    let mut parts = trimmed.split_whitespace();
                    parts.next(); // skip flag column
                    parts.next() == Some("libfdk_aac")
                })
            }
            Err(_) => false,
        }
    })
}

/// A spawned audio encoder, owning the bounded PCM input channel and the
/// supervisor task that manages the ffmpeg subprocess. Drop the encoder
/// (or cancel its `CancellationToken`) to tear down the subprocess.
pub struct AudioEncoder {
    params: EncoderParams,
    pcm_tx: mpsc::Sender<PcmChunk>,
    encoded_rx: mpsc::Receiver<EncodedFrame>,
    /// Supervisor task handle. Held so the task is detached only on Drop.
    _supervisor: JoinHandle<()>,
    cancel: CancellationToken,
    /// Bytes-per-frame for the input PCM (s32le interleaved): channels × 4.
    bytes_per_frame: usize,
    /// Scratch buffer for planar→interleaved conversion. Reused across
    /// `submit_planar` calls to avoid per-call allocation.
    pack_scratch: BytesMut,
}

impl std::fmt::Debug for AudioEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEncoder")
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl AudioEncoder {
    /// Spawn the encoder: validate params, build the ffmpeg command line,
    /// and start the supervisor + child driver tasks. Returns
    /// [`AudioEncoderError::FfmpegNotFound`] if ffmpeg is missing.
    ///
    /// `flow_id` and `output_id` are tracing labels only; they appear in
    /// log lines so operators can correlate encoder messages with the
    /// flow/output that produced them.
    ///
    /// `stats` is the parent output's stats accumulator. The encoder
    /// increments `packets_dropped` on bounded-channel-full events and
    /// `packets_sent` / `bytes_sent` are NOT touched here (those belong
    /// to the consumer that actually sends the encoded frame on the wire).
    pub fn spawn(
        params: EncoderParams,
        cancel: CancellationToken,
        flow_id: String,
        output_id: String,
        stats: Arc<OutputStatsAccumulator>,
        events: Option<EventSender>,
    ) -> Result<Self, AudioEncoderError> {
        // Reject obviously bad PCM formats early so we don't spawn ffmpeg
        // for an unfixable input.
        if params.channels == 0 || params.channels > 8 {
            return Err(AudioEncoderError::InvalidPcmFormat {
                reason: format!("channels={} (must be 1..=8)", params.channels),
            });
        }
        if params.sample_rate == 0 || params.sample_rate > 192_000 {
            return Err(AudioEncoderError::InvalidPcmFormat {
                reason: format!("sample_rate={} (must be 1..=192000 Hz)", params.sample_rate),
            });
        }

        if !check_ffmpeg_available() {
            return Err(AudioEncoderError::FfmpegNotFound);
        }

        // HE-AAC needs libfdk_aac specifically — fail loudly if missing
        // instead of producing zero output and cryptic ffmpeg `-22` errors
        // per segment downstream.
        if matches!(params.codec, AudioCodec::HeAacV1 | AudioCodec::HeAacV2)
            && !check_libfdk_aac_available()
        {
            return Err(AudioEncoderError::HeAacRequiresLibfdk);
        }

        let (pcm_tx, pcm_rx) = mpsc::channel::<PcmChunk>(PCM_CHANNEL_DEPTH);
        let (encoded_tx, encoded_rx) = mpsc::channel::<EncodedFrame>(ENCODED_CHANNEL_DEPTH);

        let bytes_per_frame = (params.channels as usize) * 4;

        let supervisor = tokio::spawn(supervisor_loop(
            params.clone(),
            pcm_rx,
            encoded_tx,
            cancel.clone(),
            flow_id,
            output_id,
            stats,
            events,
        ));

        Ok(Self {
            params,
            pcm_tx,
            encoded_rx,
            _supervisor: supervisor,
            cancel,
            bytes_per_frame,
            pack_scratch: BytesMut::new(),
        })
    }

    /// Encoder parameters (mirrors what was passed to [`Self::spawn`]).
    pub fn params(&self) -> &EncoderParams {
        &self.params
    }

    /// Submit a planar f32 PCM frame from the upstream decoder.
    ///
    /// Converts to interleaved s32le and `try_send`s onto the bounded PCM
    /// channel. **Non-blocking.** On full / closed channel the chunk is
    /// dropped silently — slow ffmpeg must never cascade backpressure into
    /// the input task. The caller's stats accumulator is incremented on
    /// drop. Returns `true` if the chunk was queued, `false` on drop.
    pub fn submit_planar(&mut self, planar: &[Vec<f32>], pts: u64) -> bool {
        if planar.is_empty() {
            return false;
        }
        // Validate channel count matches what we promised ffmpeg.
        if planar.len() != self.params.channels as usize {
            tracing::debug!(
                "audio_encode: planar channel count {} != configured {}, dropping",
                planar.len(),
                self.params.channels
            );
            return false;
        }
        let frames = planar[0].len();
        // All channels must have the same frame count.
        for ch in planar.iter().skip(1) {
            if ch.len() != frames {
                tracing::debug!(
                    "audio_encode: planar channel length mismatch ({} vs {}), dropping",
                    ch.len(),
                    frames
                );
                return false;
            }
        }

        let total_bytes = frames * self.bytes_per_frame;
        self.pack_scratch.clear();
        self.pack_scratch.reserve(total_bytes);
        pack_planar_to_s32le_interleaved(planar, frames, &mut self.pack_scratch);

        let chunk = PcmChunk {
            data: self.pack_scratch.clone().freeze(),
            pts,
        };

        match self.pcm_tx.try_send(chunk) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Backpressure — silently drop. The encoder is slow or
                // ffmpeg is restarting. The output's stats accumulator
                // will reflect this on the consumer side too.
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Pull the next encoded frame, if any. Non-blocking.
    pub fn try_recv(&mut self) -> Option<EncodedFrame> {
        self.encoded_rx.try_recv().ok()
    }

    /// Drain all currently-available encoded frames. Non-blocking — returns
    /// an empty vec if the channel is empty.
    pub fn drain(&mut self) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        while let Ok(frame) = self.encoded_rx.try_recv() {
            out.push(frame);
        }
        out
    }

    /// Cancel the supervisor and wait for it to exit. Used by tests; in
    /// production the cancellation token is shared with the parent output
    /// task and gets cancelled by the flow lifecycle.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

// ── Internal: ffmpeg command line ───────────────────────────────────────────

/// Build the ffmpeg argv for the requested codec. All commands include
/// `-hide_banner -nostats -loglevel warning -flush_packets 1` to keep
/// stderr clean and minimise output latency.
fn build_ffmpeg_args(params: &EncoderParams) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-loglevel".into(),
        "warning".into(),
        "-fflags".into(),
        "+nobuffer".into(),
        // Input: raw s32le PCM on stdin.
        "-f".into(),
        "s32le".into(),
        "-ar".into(),
        params.sample_rate.to_string(),
        "-ac".into(),
        params.channels.to_string(),
        "-i".into(),
        "pipe:0".into(),
    ];

    // Per-codec encoder + output container.
    let bitrate = format!("{}k", params.target_bitrate_kbps);
    let target_sr = params.target_sample_rate.to_string();
    let target_ch = params.target_channels.to_string();

    match params.codec {
        AudioCodec::AacLc => {
            args.extend([
                "-c:a".into(), "aac".into(),
                "-profile:a".into(), "aac_low".into(),
                "-b:a".into(), bitrate,
                "-ar".into(), target_sr,
                "-ac".into(), target_ch,
                "-f".into(), "adts".into(),
            ]);
        }
        AudioCodec::HeAacV1 => {
            // HE-AAC v1: requires libfdk_aac. The spawn() path checks
            // this and refuses to start if libfdk_aac is missing, so
            // by the time we build args we know it's available.
            args.extend([
                "-c:a".into(), "libfdk_aac".into(),
                "-profile:a".into(), "aac_he".into(),
                "-b:a".into(), bitrate,
                "-ar".into(), target_sr,
                "-ac".into(), target_ch,
                "-f".into(), "adts".into(),
            ]);
        }
        AudioCodec::HeAacV2 => {
            // HE-AAC v2 (PS): requires libfdk_aac. Same gating as v1.
            args.extend([
                "-c:a".into(), "libfdk_aac".into(),
                "-profile:a".into(), "aac_he_v2".into(),
                "-b:a".into(), bitrate,
                "-ar".into(), target_sr,
                "-ac".into(), target_ch,
                "-f".into(), "adts".into(),
            ]);
        }
        AudioCodec::Opus => {
            // Opus mandates 48 kHz on the wire (libopus internal rate).
            // Mux as ogg so each Opus packet lands in a discrete OGG page
            // we can frame on stdout.
            args.extend([
                "-c:a".into(), "libopus".into(),
                "-b:a".into(), bitrate,
                "-application".into(), "audio".into(),
                "-frame_duration".into(), "20".into(),
                "-ar".into(), "48000".into(),
                "-ac".into(), target_ch,
                "-f".into(), "ogg".into(),
            ]);
        }
        AudioCodec::Mp2 => {
            args.extend([
                "-c:a".into(), "mp2".into(),
                "-b:a".into(), bitrate,
                "-ar".into(), target_sr,
                "-ac".into(), target_ch,
                "-f".into(), "mp2".into(),
            ]);
        }
        AudioCodec::Ac3 => {
            args.extend([
                "-c:a".into(), "ac3".into(),
                "-b:a".into(), bitrate,
                "-ar".into(), target_sr,
                "-ac".into(), target_ch,
                "-f".into(), "ac3".into(),
            ]);
        }
    }

    args.extend([
        "-flush_packets".into(),
        "1".into(),
        "pipe:1".into(),
    ]);

    args
}

// ── Internal: planar → interleaved s32le packing ────────────────────────────

/// Convert planar f32 PCM (channel-major `Vec<Vec<f32>>`) into interleaved
/// s32le bytes, clipping to `[-1.0, 1.0]` and scaling by `i32::MAX`.
///
/// Output layout (for stereo): `[L0 R0 L1 R1 L2 R2 ...]` with each sample
/// little-endian s32. This is the canonical PCM exchange format used by
/// ffmpeg's `-f s32le` input.
fn pack_planar_to_s32le_interleaved(planar: &[Vec<f32>], frames: usize, out: &mut BytesMut) {
    let n_ch = planar.len();
    for f in 0..frames {
        for ch in 0..n_ch {
            let s = planar[ch][f].clamp(-1.0, 1.0);
            // i32::MAX (0x7FFF_FFFF) keeps positive full-scale exactly
            // representable; the negative side has one extra LSB headroom.
            let s32 = (s * i32::MAX as f32) as i32;
            out.extend_from_slice(&s32.to_le_bytes());
        }
    }
}

// ── Internal: per-codec stdout framers ──────────────────────────────────────

/// State held across stdout reads to support frame-spanning byte chunks.
struct FramerState {
    codec: AudioCodec,
    /// Sample rate at which the encoder runs (used to derive PTS deltas).
    target_sample_rate: u32,
    /// Bytes left from a previous read that didn't form a full frame.
    leftover: BytesMut,
    /// Running PTS counter in 90 kHz units. Initialised on the first
    /// frame from the upstream input PTS via [`set_anchor_pts`].
    pts_90k: u64,
    /// Whether the PTS anchor has been set.
    anchor_set: bool,
}

impl FramerState {
    fn new(codec: AudioCodec, target_sample_rate: u32) -> Self {
        Self {
            codec,
            target_sample_rate,
            leftover: BytesMut::new(),
            pts_90k: 0,
            anchor_set: false,
        }
    }

    fn set_anchor_pts(&mut self, pts: u64) {
        if !self.anchor_set {
            self.pts_90k = pts;
            self.anchor_set = true;
        }
    }

    /// Increment the running PTS by `samples` worth of duration at the
    /// target sample rate, expressed in 90 kHz ticks.
    fn advance_pts(&mut self, samples: u32) {
        if self.target_sample_rate == 0 {
            return;
        }
        let ticks = (samples as u64) * 90_000 / (self.target_sample_rate as u64);
        self.pts_90k = self.pts_90k.saturating_add(ticks);
    }

    /// Feed new bytes from ffmpeg stdout. Returns any complete frames
    /// extracted (possibly empty if more bytes are needed).
    fn feed(&mut self, new_bytes: &[u8]) -> Vec<EncodedFrame> {
        self.leftover.extend_from_slice(new_bytes);
        let mut out = Vec::new();
        match self.codec {
            AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2 => {
                self.frame_adts(&mut out);
            }
            AudioCodec::Opus => {
                self.frame_ogg_opus(&mut out);
            }
            AudioCodec::Mp2 => {
                self.frame_mp2(&mut out);
            }
            AudioCodec::Ac3 => {
                self.frame_ac3(&mut out);
            }
        }
        out
    }

    /// ADTS framer: AAC frames are self-delimited via the 13-bit
    /// `frame_length` field starting 30 bits into the 7-byte header.
    /// Sync word = `0xFFF` in the top 12 bits of the first 2 bytes.
    fn frame_adts(&mut self, out: &mut Vec<EncodedFrame>) {
        loop {
            // Find sync word.
            let buf = &self.leftover[..];
            let mut start: Option<usize> = None;
            let mut i = 0;
            while i + 1 < buf.len() {
                if buf[i] == 0xFF && (buf[i + 1] & 0xF0) == 0xF0 {
                    start = Some(i);
                    break;
                }
                i += 1;
            }
            let Some(s) = start else {
                // No sync — discard everything (resync) but keep last
                // byte in case it's the start of a sync word.
                let keep = self.leftover.len().saturating_sub(1);
                let _ = self.leftover.split_to(keep);
                return;
            };
            if s > 0 {
                let _ = self.leftover.split_to(s);
            }
            if self.leftover.len() < 7 {
                return; // need more bytes for the header
            }
            // frame_length is bits 30..43 (13 bits) measured from MSB of
            // byte 0. That's: ((b3 & 0x03) << 11) | (b4 << 3) | (b5 >> 5).
            let b3 = self.leftover[3] as usize;
            let b4 = self.leftover[4] as usize;
            let b5 = self.leftover[5] as usize;
            let frame_len = ((b3 & 0x03) << 11) | (b4 << 3) | (b5 >> 5);
            if frame_len < 7 || frame_len > 8191 {
                // Bogus length — skip one byte and resync.
                let _ = self.leftover.split_to(1);
                continue;
            }
            if self.leftover.len() < frame_len {
                return; // need more bytes for the body
            }
            let frame_bytes = self.leftover.split_to(frame_len).freeze();
            let pts = self.pts_90k;
            self.advance_pts(1024); // AAC-LC = 1024 samples per frame
            out.push(EncodedFrame { data: frame_bytes, pts });
        }
    }

    /// OGG/Opus framer: parses OGG pages and emits one EncodedFrame per
    /// Opus packet (one packet per RTP payload per RFC 7587).
    ///
    /// OGG page header (RFC 3533 §6):
    /// ```text
    ///   0 capture_pattern[4]   "OggS"
    ///   4 stream_structure_version (1)
    ///   5 header_type_flag       (1)
    ///   6 granule_position       (8 LE)
    ///  14 bitstream_serial       (4 LE)
    ///  18 page_sequence          (4 LE)
    ///  22 CRC32                  (4 LE)
    ///  26 page_segments          (1)
    ///  27 segment_table[page_segments]
    ///     payload follows
    /// ```
    /// Each Opus packet is one or more consecutive segments of length 255
    /// terminated by a segment of length < 255 (or by the page boundary if
    /// the packet is continued on the next page).
    fn frame_ogg_opus(&mut self, out: &mut Vec<EncodedFrame>) {
        loop {
            let buf = &self.leftover[..];
            // Find OggS.
            let mut start: Option<usize> = None;
            let mut i = 0;
            while i + 4 <= buf.len() {
                if &buf[i..i + 4] == b"OggS" {
                    start = Some(i);
                    break;
                }
                i += 1;
            }
            let Some(s) = start else {
                // No OggS in buffer — keep last 3 bytes in case they're
                // the start of a marker.
                let keep = self.leftover.len().saturating_sub(3);
                let _ = self.leftover.split_to(keep);
                return;
            };
            if s > 0 {
                let _ = self.leftover.split_to(s);
            }
            if self.leftover.len() < 27 {
                return; // need more bytes for the fixed header
            }
            let header_type = self.leftover[5];
            let n_segments = self.leftover[26] as usize;
            let header_len = 27 + n_segments;
            if self.leftover.len() < header_len {
                return; // need more bytes for the segment table
            }
            // Sum segment lengths to get the page payload size.
            let mut payload_len: usize = 0;
            for j in 0..n_segments {
                payload_len += self.leftover[27 + j] as usize;
            }
            if self.leftover.len() < header_len + payload_len {
                return; // need more bytes for the page payload
            }

            // We have a complete page. Copy the segment table and payload
            // out of the leftover buffer so we can mutate self while
            // processing them.
            let segment_table: Vec<usize> = (0..n_segments)
                .map(|j| self.leftover[27 + j] as usize)
                .collect();
            let payload_vec: Vec<u8> =
                self.leftover[header_len..header_len + payload_len].to_vec();

            // Consume the page from the buffer up front.
            let _ = self.leftover.split_to(header_len + payload_len);

            // Walk the segment table to extract individual Opus packets.
            // A packet is a run of 255-length segments terminated by a
            // < 255 segment. Skip OpusHead / OpusTags container metadata.
            let mut packet_buf: Vec<u8> = Vec::new();
            let mut packet_offset: usize = 0;
            for &seg_len in &segment_table {
                let seg = &payload_vec[packet_offset..packet_offset + seg_len];
                packet_buf.extend_from_slice(seg);
                packet_offset += seg_len;
                if seg_len < 255 {
                    // End of an Opus packet.
                    if !packet_buf.is_empty() && !is_opus_header(&packet_buf, header_type) {
                        let frame_data = Bytes::from(std::mem::take(&mut packet_buf));
                        let pts = self.pts_90k;
                        // Opus frame_duration=20ms ⇒ 960 samples @ 48 kHz.
                        let samples_per_frame =
                            (self.target_sample_rate / 50).max(1);
                        self.advance_pts(samples_per_frame);
                        out.push(EncodedFrame { data: frame_data, pts });
                    } else {
                        packet_buf.clear();
                    }
                }
            }
        }
    }

    /// MP2 framer: MPEG audio layer II frames. Sync word `0xFFF` (top 12
    /// bits). Frame size is computed from the bitrate and sample rate
    /// fields in the header.
    fn frame_mp2(&mut self, out: &mut Vec<EncodedFrame>) {
        loop {
            let buf = &self.leftover[..];
            // Find sync word.
            let mut start: Option<usize> = None;
            let mut i = 0;
            while i + 1 < buf.len() {
                if buf[i] == 0xFF && (buf[i + 1] & 0xF0) == 0xF0 {
                    start = Some(i);
                    break;
                }
                i += 1;
            }
            let Some(s) = start else {
                let keep = self.leftover.len().saturating_sub(1);
                let _ = self.leftover.split_to(keep);
                return;
            };
            if s > 0 {
                let _ = self.leftover.split_to(s);
            }
            if self.leftover.len() < 4 {
                return;
            }
            let h1 = self.leftover[1];
            let h2 = self.leftover[2];
            // version: bits 19-20: 11=MPEG-1, 10=MPEG-2, 00=MPEG-2.5
            let version_id = (h1 >> 3) & 0x03;
            // layer: bits 17-18: 10=Layer II
            let layer = (h1 >> 1) & 0x03;
            if layer != 0b10 {
                let _ = self.leftover.split_to(1);
                continue;
            }
            // bitrate index (4 bits) bits 12-15
            let br_idx = (h2 >> 4) & 0x0F;
            // sample rate index (2 bits) bits 10-11
            let sr_idx = (h2 >> 2) & 0x03;
            // padding bit, bit 9
            let padding = ((h2 >> 1) & 0x01) as usize;
            let bitrate_kbps = mp2_bitrate_kbps(version_id, br_idx);
            let sample_rate = mp2_sample_rate_hz(version_id, sr_idx);
            if bitrate_kbps == 0 || sample_rate == 0 {
                let _ = self.leftover.split_to(1);
                continue;
            }
            // Layer II frame size (bytes) = (144 * bitrate / sample_rate) + padding.
            let frame_len = (144 * bitrate_kbps * 1000 / sample_rate as u32) as usize + padding;
            if frame_len < 4 || self.leftover.len() < frame_len {
                if frame_len < 4 {
                    let _ = self.leftover.split_to(1);
                    continue;
                }
                return;
            }
            let frame_bytes = self.leftover.split_to(frame_len).freeze();
            let pts = self.pts_90k;
            // MP2: 1152 samples per frame (Layer II).
            self.advance_pts(1152);
            out.push(EncodedFrame { data: frame_bytes, pts });
        }
    }

    /// AC-3 framer: sync word `0x0B77`. Frame size from `frmsizecod` field
    /// in byte 4 (top 6 bits). See ATSC A/52 §5.3.
    fn frame_ac3(&mut self, out: &mut Vec<EncodedFrame>) {
        loop {
            let buf = &self.leftover[..];
            // Find 0x0B77.
            let mut start: Option<usize> = None;
            let mut i = 0;
            while i + 1 < buf.len() {
                if buf[i] == 0x0B && buf[i + 1] == 0x77 {
                    start = Some(i);
                    break;
                }
                i += 1;
            }
            let Some(s) = start else {
                let keep = self.leftover.len().saturating_sub(1);
                let _ = self.leftover.split_to(keep);
                return;
            };
            if s > 0 {
                let _ = self.leftover.split_to(s);
            }
            if self.leftover.len() < 6 {
                return;
            }
            // fscod = byte 4 bits 6-7, frmsizecod = byte 4 bits 0-5.
            let fscod = (self.leftover[4] >> 6) & 0x03;
            let frmsizecod = self.leftover[4] & 0x3F;
            let frame_size_words = ac3_frame_size_words(fscod, frmsizecod);
            if frame_size_words == 0 {
                let _ = self.leftover.split_to(1);
                continue;
            }
            let frame_len = frame_size_words * 2;
            if self.leftover.len() < frame_len {
                return;
            }
            let frame_bytes = self.leftover.split_to(frame_len).freeze();
            let pts = self.pts_90k;
            // AC-3: 1536 samples per frame.
            self.advance_pts(1536);
            out.push(EncodedFrame { data: frame_bytes, pts });
        }
    }
}

/// Detect Opus header packets ("OpusHead" / "OpusTags") which the framer
/// must skip — they're container metadata, not audio frames. The first OGG
/// page in an Opus stream has the BOS flag (0x02) set.
fn is_opus_header(packet: &[u8], header_type_flag: u8) -> bool {
    if (header_type_flag & 0x02) != 0 {
        return true;
    }
    if packet.len() >= 8 {
        if &packet[..8] == b"OpusHead" || &packet[..8] == b"OpusTags" {
            return true;
        }
    }
    false
}

/// MP2 bitrate table (kbps). `version_id` and `bitrate_idx` are the 2-bit
/// version and 4-bit bitrate fields from the MPEG audio frame header.
/// Returns 0 for invalid combinations.
fn mp2_bitrate_kbps(version_id: u8, br_idx: u8) -> u32 {
    // Layer II bitrates per ISO/IEC 11172-3 (MPEG-1) and 13818-3 (MPEG-2/-2.5).
    // version_id: 11 = MPEG-1, 10 = MPEG-2, 00 = MPEG-2.5
    const MPEG1_L2: [u32; 16] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
    ];
    const MPEG2_L2: [u32; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    let idx = br_idx as usize;
    match version_id {
        0b11 => MPEG1_L2[idx],
        0b10 | 0b00 => MPEG2_L2[idx],
        _ => 0,
    }
}

/// MP2 sample rate table (Hz). Returns 0 for invalid combinations.
fn mp2_sample_rate_hz(version_id: u8, sr_idx: u8) -> u32 {
    let base = match (version_id, sr_idx) {
        (0b11, 0) => 44_100,
        (0b11, 1) => 48_000,
        (0b11, 2) => 32_000,
        (0b10, 0) => 22_050,
        (0b10, 1) => 24_000,
        (0b10, 2) => 16_000,
        (0b00, 0) => 11_025,
        (0b00, 1) => 12_000,
        (0b00, 2) => 8_000,
        _ => 0,
    };
    base
}

/// AC-3 frame size lookup (16-bit words). `fscod`/`frmsizecod` come from
/// the second 16 bits of the frame header. See ATSC A/52 Table 5.18.
/// Returns 0 for invalid combinations.
fn ac3_frame_size_words(fscod: u8, frmsizecod: u8) -> usize {
    // ATSC A/52 Table 5.18 — Frame Size Code Table (32-bit words shown here
    // halved into 16-bit words for the framer's split unit).
    // fscod: 0=48k, 1=44.1k, 2=32k, 3=reserved.
    // We index by frmsizecod (0..=37) and fscod.
    if frmsizecod >= 38 || fscod >= 3 {
        return 0;
    }
    // Words per syncframe (16-bit words) for each fscod×frmsizecod.
    // Row format: [fscod=0 (48k), fscod=1 (44.1k), fscod=2 (32k)]
    const TABLE: [[u32; 3]; 19] = [
        [64, 69, 96],     // 32 kbps
        [80, 87, 120],    // 40 kbps
        [96, 104, 144],   // 48 kbps
        [112, 121, 168],  // 56 kbps
        [128, 139, 192],  // 64 kbps
        [160, 174, 240],  // 80 kbps
        [192, 208, 288],  // 96 kbps
        [224, 243, 336],  // 112 kbps
        [256, 278, 384],  // 128 kbps
        [320, 348, 480],  // 160 kbps
        [384, 417, 576],  // 192 kbps
        [448, 487, 672],  // 224 kbps
        [512, 557, 768],  // 256 kbps
        [640, 696, 960],  // 320 kbps
        [768, 835, 1152], // 384 kbps
        [896, 975, 1344], // 448 kbps
        [1024, 1114, 1536], // 512 kbps
        [1152, 1253, 1728], // 576 kbps
        [1280, 1393, 1920], // 640 kbps
    ];
    // frmsizecod values come in pairs (one per bitrate, two per row, but
    // both halves of each pair give the same frame size). Map frmsizecod
    // to a row index by frmsizecod / 2.
    let row = (frmsizecod / 2) as usize;
    if row >= TABLE.len() {
        return 0;
    }
    TABLE[row][fscod as usize] as usize
}

// ── Internal: supervisor + driver tasks ─────────────────────────────────────

async fn supervisor_loop(
    params: EncoderParams,
    mut pcm_rx: mpsc::Receiver<PcmChunk>,
    encoded_tx: mpsc::Sender<EncodedFrame>,
    cancel: CancellationToken,
    flow_id: String,
    output_id: String,
    stats: Arc<OutputStatsAccumulator>,
    events: Option<EventSender>,
) {
    tracing::info!(
        "audio_encode supervisor started: flow={} output={} codec={} sr={}/{} ch={}/{} br={}k",
        flow_id, output_id,
        params.codec.as_str(),
        params.sample_rate, params.target_sample_rate,
        params.channels, params.target_channels,
        params.target_bitrate_kbps,
    );

    if let Some(ref ev) = events {
        ev.emit_flow_with_details(
            EventSeverity::Info,
            crate::manager::events::category::AUDIO_ENCODE,
            format!(
                "audio encoder started: output '{}' codec={} {} kbps",
                output_id, params.codec.as_str(), params.target_bitrate_kbps
            ),
            &flow_id,
            serde_json::json!({
                "output_id": output_id,
                "codec": params.codec.as_str(),
                "bitrate_kbps": params.target_bitrate_kbps,
                "sample_rate": params.target_sample_rate,
                "channels": params.target_channels,
            }),
        );
    }

    let mut restart_count: u32 = 0;
    let mut window_start = std::time::Instant::now();

    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Reset the restart window if enough time has passed.
        if window_start.elapsed() >= RESTART_WINDOW {
            window_start = std::time::Instant::now();
            restart_count = 0;
        }

        // Spawn ffmpeg.
        let args = build_ffmpeg_args(&params);
        let mut cmd = Command::new("ffmpeg");
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    "audio_encode '{}' failed to spawn ffmpeg: {e}",
                    output_id
                );
                restart_count += 1;
                if restart_count > MAX_RESTARTS {
                    break;
                }
                let backoff = compute_backoff(restart_count);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => break,
                }
                continue;
            }
        };

        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Spawn the three driver tasks. We use a per-process cancellation
        // token so we can tear them down without affecting the supervisor's
        // outer cancel token (which gets re-used for the next restart).
        let proc_cancel = cancel.child_token();

        // We can't move pcm_rx into the writer task because we need to
        // keep using it after the process restarts. Use a per-process
        // bridging channel that the supervisor pumps from pcm_rx into.
        // For B.1 simplicity, use a take/restore pattern: move pcm_rx into
        // a small task that owns it for this process iteration, then
        // recover it on exit. We do this via an Option dance.
        let (writer_done_tx, writer_done_rx) = tokio::sync::oneshot::channel();
        let (reader_done_tx, reader_done_rx) = tokio::sync::oneshot::channel();
        let (stderr_done_tx, stderr_done_rx) = tokio::sync::oneshot::channel();

        let pcm_rx_for_writer = std::mem::replace(&mut pcm_rx, mpsc::channel(1).1);
        let writer_cancel = proc_cancel.clone();
        let writer_stats = stats.clone();
        let writer_handle = tokio::spawn(stdin_writer_task(
            stdin,
            pcm_rx_for_writer,
            writer_cancel,
            writer_done_tx,
            writer_stats,
        ));

        let reader_cancel = proc_cancel.clone();
        let encoded_tx_clone = encoded_tx.clone();
        let reader_params = params.clone();
        let reader_handle = tokio::spawn(stdout_reader_task(
            stdout,
            reader_params,
            encoded_tx_clone,
            reader_cancel,
            reader_done_tx,
        ));

        let stderr_cancel = proc_cancel.clone();
        let stderr_label = format!("flow={} output={}", flow_id, output_id);
        let stderr_handle = tokio::spawn(stderr_drainer_task(
            stderr,
            stderr_label,
            stderr_cancel,
            stderr_done_tx,
        ));

        // Wait for any of: cancel, child exit, or one of the driver tasks
        // to finish (which signals an error).
        let exit_status: std::process::ExitStatus;
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(
                    "audio_encode '{}': cancellation received, tearing down",
                    output_id
                );
                proc_cancel.cancel();
                let _ = wait_with_timeout(&mut child).await;
                let _ = writer_handle.await;
                let _ = reader_handle.await;
                let _ = stderr_handle.await;
                break;
            }
            r = child.wait() => {
                exit_status = match r {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            "audio_encode '{}': child.wait error: {e}",
                            output_id
                        );
                        // Force a restart attempt.
                        restart_count += 1;
                        proc_cancel.cancel();
                        let _ = writer_handle.await;
                        let _ = reader_handle.await;
                        let _ = stderr_handle.await;
                        if restart_count > MAX_RESTARTS {
                            break;
                        }
                        let backoff = compute_backoff(restart_count);
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = cancel.cancelled() => break,
                        }
                        // Recover the writer's pcm_rx from its done channel.
                        match writer_done_rx.await {
                            Ok(rx) => pcm_rx = rx,
                            Err(_) => break,
                        }
                        let _ = reader_done_rx.await;
                        let _ = stderr_done_rx.await;
                        continue;
                    }
                };
                proc_cancel.cancel();
                let _ = writer_handle.await;
                let _ = reader_handle.await;
                let _ = stderr_handle.await;
            }
        }

        if !exit_status.success() {
            tracing::warn!(
                "audio_encode '{}': ffmpeg exited with {} (restart {}/{})",
                output_id, exit_status, restart_count + 1, MAX_RESTARTS
            );
        } else {
            tracing::debug!(
                "audio_encode '{}': ffmpeg exited cleanly",
                output_id
            );
        }

        // Recover the writer task's pcm_rx so the next iteration can
        // re-use it. The writer task hands it back via a oneshot when it
        // exits.
        match writer_done_rx.await {
            Ok(rx) => pcm_rx = rx,
            Err(_) => {
                // Writer dropped the sender without sending — channel
                // permanently closed. Bail.
                break;
            }
        }
        let _ = reader_done_rx.await;
        let _ = stderr_done_rx.await;

        if cancel.is_cancelled() {
            break;
        }

        restart_count += 1;
        if restart_count > MAX_RESTARTS {
            tracing::error!(
                "audio_encode '{}': giving up after {} restarts in {} seconds",
                output_id, MAX_RESTARTS, RESTART_WINDOW.as_secs()
            );
            if let Some(ref ev) = events {
                ev.emit_flow(
                    EventSeverity::Critical,
                    crate::manager::events::category::AUDIO_ENCODE,
                    format!(
                        "audio encoder failed: output '{}' exhausted {} restarts in {} s",
                        output_id, MAX_RESTARTS, RESTART_WINDOW.as_secs()
                    ),
                    &flow_id,
                );
            }
            break;
        }
        if let Some(ref ev) = events {
            ev.emit_flow_with_details(
                EventSeverity::Warning,
                crate::manager::events::category::AUDIO_ENCODE,
                format!(
                    "audio encoder restarted: output '{}' restart {}/{}",
                    output_id, restart_count, MAX_RESTARTS
                ),
                &flow_id,
                serde_json::json!({
                    "output_id": output_id,
                    "restart_count": restart_count,
                    "max_restarts": MAX_RESTARTS,
                }),
            );
        }
        let backoff = compute_backoff(restart_count);
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = cancel.cancelled() => break,
        }
    }

    tracing::info!(
        "audio_encode supervisor stopped: flow={} output={}",
        flow_id, output_id
    );
}

fn compute_backoff(restart_count: u32) -> Duration {
    let secs = (1u64 << restart_count.min(5)).min(MAX_BACKOFF.as_secs());
    Duration::from_secs(secs)
}

async fn wait_with_timeout(child: &mut Child) {
    match tokio::time::timeout(GRACEFUL_EXIT_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::debug!("audio_encode child.wait error: {e}"),
        Err(_) => {
            tracing::debug!("audio_encode child did not exit, killing");
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

/// stdin writer task: pulls PCM chunks from the bounded channel and
/// writes them to ffmpeg stdin. Hands the receiver back via a oneshot
/// on exit so the supervisor can re-use it for the next process iteration.
async fn stdin_writer_task(
    mut stdin: ChildStdin,
    mut pcm_rx: mpsc::Receiver<PcmChunk>,
    cancel: CancellationToken,
    done_tx: tokio::sync::oneshot::Sender<mpsc::Receiver<PcmChunk>>,
    _stats: Arc<OutputStatsAccumulator>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            opt = pcm_rx.recv() => {
                let Some(chunk) = opt else { break; };
                let write_fut = stdin.write_all(&chunk.data);
                match tokio::time::timeout(STDIN_WRITE_TIMEOUT, write_fut).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::debug!("audio_encode stdin write failed: {e}");
                        break;
                    }
                    Err(_) => {
                        tracing::warn!("audio_encode stdin write timed out");
                        break;
                    }
                }
            }
        }
    }
    let _ = stdin.shutdown().await;
    drop(stdin);
    let _ = done_tx.send(pcm_rx);
}

/// stdout reader task: reads bytes from ffmpeg stdout, feeds them through
/// the per-codec framer, and pushes complete frames into encoded_tx.
async fn stdout_reader_task(
    stdout: ChildStdout,
    params: EncoderParams,
    encoded_tx: mpsc::Sender<EncodedFrame>,
    cancel: CancellationToken,
    done_tx: tokio::sync::oneshot::Sender<()>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf = [0u8; 8192];
    let mut framer = FramerState::new(params.codec, params.target_sample_rate);
    // For Phase B's PTS anchor, the supervisor doesn't have a path to
    // pre-set the anchor PTS yet — the framer starts at PTS=0. The
    // consumer is responsible for re-stamping with upstream PTS as needed.
    framer.set_anchor_pts(0);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            r = reader.read(&mut buf) => {
                match r {
                    Ok(0) => break,
                    Ok(n) => {
                        let frames = framer.feed(&buf[..n]);
                        for f in frames {
                            // Drop on full — the consumer is too slow.
                            if encoded_tx.try_send(f).is_err() {
                                tracing::debug!("audio_encode encoded_tx full, dropping frame");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("audio_encode stdout read error: {e}");
                        break;
                    }
                }
            }
        }
    }
    let _ = done_tx.send(());
}

/// stderr drainer task: continuously reads stderr to keep the pipe from
/// filling. Logs every line at debug level. Critical: ffmpeg deadlocks
/// when the stderr pipe fills (typically 64 KB on Linux), so this task
/// must always run alongside the stdin writer and stdout reader.
async fn stderr_drainer_task(
    stderr: ChildStderr,
    label: String,
    cancel: CancellationToken,
    done_tx: tokio::sync::oneshot::Sender<()>,
) {
    let mut reader = BufReader::new(stderr);
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            r = reader.read(&mut buf) => {
                match r {
                    Ok(0) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&buf[..n]);
                        for line in s.lines() {
                            if !line.is_empty() {
                                tracing::debug!("audio_encode[{label}] ffmpeg: {line}");
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    let _ = done_tx.send(());
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codec_round_trip() {
        for (s, c) in [
            ("aac_lc", AudioCodec::AacLc),
            ("he_aac_v1", AudioCodec::HeAacV1),
            ("he_aac_v2", AudioCodec::HeAacV2),
            ("opus", AudioCodec::Opus),
            ("mp2", AudioCodec::Mp2),
            ("ac3", AudioCodec::Ac3),
        ] {
            assert_eq!(AudioCodec::parse(s), Some(c));
            assert_eq!(c.as_str(), s);
        }
        assert!(AudioCodec::parse("flac").is_none());
        assert!(AudioCodec::parse("").is_none());
    }

    #[test]
    fn default_bitrates_are_sane() {
        assert_eq!(AudioCodec::AacLc.default_bitrate_kbps(), 128);
        assert_eq!(AudioCodec::HeAacV1.default_bitrate_kbps(), 64);
        assert_eq!(AudioCodec::HeAacV2.default_bitrate_kbps(), 32);
        assert_eq!(AudioCodec::Opus.default_bitrate_kbps(), 96);
        assert_eq!(AudioCodec::Mp2.default_bitrate_kbps(), 192);
        assert_eq!(AudioCodec::Ac3.default_bitrate_kbps(), 192);
    }

    #[test]
    fn pack_planar_stereo_zero_silence() {
        let planar = vec![vec![0.0_f32; 4], vec![0.0_f32; 4]];
        let mut out = BytesMut::new();
        pack_planar_to_s32le_interleaved(&planar, 4, &mut out);
        // 4 frames × 2 channels × 4 bytes = 32 bytes of zeros.
        assert_eq!(out.len(), 32);
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn pack_planar_stereo_full_scale() {
        // L = +1.0, R = -1.0. Scaling by i32::MAX (which becomes
        // 2147483648.0 once promoted to f32 due to f32 rounding) and then
        // saturating-casting back to i32 gives i32::MAX for +1.0 and
        // i32::MIN for -1.0. That uses the full range and is what
        // ffmpeg's `-f s32le` consumer expects.
        let planar = vec![vec![1.0_f32; 1], vec![-1.0_f32; 1]];
        let mut out = BytesMut::new();
        pack_planar_to_s32le_interleaved(&planar, 1, &mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(&out[0..4], &i32::MAX.to_le_bytes());
        assert_eq!(&out[4..8], &i32::MIN.to_le_bytes());
    }

    #[test]
    fn pack_planar_clips_overflow() {
        // 2.0 must clip to +1.0, not wrap.
        let planar = vec![vec![2.0_f32; 1]];
        let mut out = BytesMut::new();
        pack_planar_to_s32le_interleaved(&planar, 1, &mut out);
        assert_eq!(&out[..], &i32::MAX.to_le_bytes());
    }

    #[test]
    fn ffmpeg_args_aac_lc_contain_essentials() {
        let params = EncoderParams {
            codec: AudioCodec::AacLc,
            sample_rate: 48_000,
            channels: 2,
            target_bitrate_kbps: 128,
            target_sample_rate: 48_000,
            target_channels: 2,
        };
        let args = build_ffmpeg_args(&params);
        let joined = args.join(" ");
        assert!(joined.contains("-f s32le"));
        assert!(joined.contains("-ar 48000"));
        assert!(joined.contains("-ac 2"));
        assert!(joined.contains("-i pipe:0"));
        assert!(joined.contains("-c:a aac"));
        assert!(joined.contains("-profile:a aac_low"));
        assert!(joined.contains("-b:a 128k"));
        assert!(joined.contains("-f adts"));
        assert!(joined.ends_with("pipe:1"));
    }

    #[test]
    fn ffmpeg_args_opus_force_48k() {
        let params = EncoderParams {
            codec: AudioCodec::Opus,
            sample_rate: 44_100,
            channels: 2,
            target_bitrate_kbps: 96,
            target_sample_rate: 44_100, // ignored — Opus forces 48 kHz
            target_channels: 2,
        };
        let args = build_ffmpeg_args(&params);
        let joined = args.join(" ");
        assert!(joined.contains("-c:a libopus"));
        assert!(joined.contains("-b:a 96k"));
        assert!(joined.contains("-frame_duration 20"));
        // Output sample rate must be forced to 48 kHz on the wire.
        assert!(joined.contains("-ar 48000 -ac 2 -f ogg"));
    }

    #[test]
    fn ffmpeg_args_he_aac_v1_v2_use_libfdk() {
        // HE-AAC v1/v2 must request the libfdk_aac encoder, not the
        // native `aac` encoder which hard-rejects the aac_he profile.
        for (codec, profile) in [
            (AudioCodec::HeAacV1, "aac_he"),
            (AudioCodec::HeAacV2, "aac_he_v2"),
        ] {
            let params = EncoderParams {
                codec,
                sample_rate: 48_000,
                channels: 2,
                target_bitrate_kbps: 64,
                target_sample_rate: 48_000,
                target_channels: 2,
            };
            let args = build_ffmpeg_args(&params);
            let joined = args.join(" ");
            assert!(joined.contains("-c:a libfdk_aac"), "args = {joined}");
            assert!(joined.contains(&format!("-profile:a {profile}")));
            assert!(joined.contains("-f adts"));
        }
    }

    #[test]
    fn adts_framer_extracts_synthetic_frames() {
        // Build a minimal valid ADTS header for a 16-byte frame
        // (header=7, body=9). frame_length=16.
        // ADTS header layout (no CRC):
        //   syncword(12) | id(1) | layer(2) | protection_absent(1)
        //   profile(2) | sampling_frequency_index(4) | private_bit(1)
        //   channel_configuration(3) | original_copy(1) | home(1)
        //   copyright_id_bit(1) | copyright_id_start(1) | frame_length(13)
        //   buffer_fullness(11) | number_of_raw_data_blocks(2)
        let frame_len: u16 = 16;
        let mut hdr = [0u8; 7];
        hdr[0] = 0xFF;
        hdr[1] = 0xF1; // sync(top12)+id=0+layer=00+protection_absent=1
        hdr[2] = 0x4C; // profile=AAC-LC(01) | sr_idx=0011 | private=0 | ch_cfg=0 (high bit)
        // ch_cfg(3) finishes in byte3 high bits. We want ch_cfg=2:
        // ch_cfg=010, original=0, home=0, copyright_id_bit=0, copyright_id_start=0,
        // then frame_length(13) = 16 = 0b0000000010000.
        // Byte3: ch_cfg(3 lsb) | original(1) | home(1) | cb(1) | cs(1) | fl[12] (1)
        //        = 010 0 0 0 0 0 = 0x40
        hdr[3] = 0x40 | (((frame_len >> 11) & 0x03) as u8);
        // Byte4 = fl[11..3] (8 bits)
        hdr[4] = ((frame_len >> 3) & 0xFF) as u8;
        // Byte5 = fl[2..0] in top 3 bits, then buffer_fullness top 5
        hdr[5] = (((frame_len & 0x07) << 5) as u8) | 0x1F;
        // Byte6 = buffer_fullness bottom 6 bits + nrdb(2)
        hdr[6] = 0xFC;

        let mut frame_a = hdr.to_vec();
        frame_a.extend_from_slice(&[0xAA; 9]);
        let mut frame_b = hdr.to_vec();
        frame_b.extend_from_slice(&[0xBB; 9]);

        let mut framer = FramerState::new(AudioCodec::AacLc, 48_000);
        let mut combined = Vec::new();
        combined.extend_from_slice(&frame_a);
        combined.extend_from_slice(&frame_b);

        let frames = framer.feed(&combined);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data.len(), 16);
        assert_eq!(frames[1].data.len(), 16);
        // PTS should advance by 1024 samples worth of 90 kHz ticks.
        // 1024 * 90000 / 48000 = 1920.
        assert_eq!(frames[0].pts, 0);
        assert_eq!(frames[1].pts, 1920);
    }

    #[test]
    fn adts_framer_handles_partial_writes() {
        // Same frame as above, but feed it byte-by-byte.
        let frame_len: u16 = 16;
        let mut hdr = [0u8; 7];
        hdr[0] = 0xFF;
        hdr[1] = 0xF1;
        hdr[2] = 0x4C;
        hdr[3] = 0x40 | (((frame_len >> 11) & 0x03) as u8);
        hdr[4] = ((frame_len >> 3) & 0xFF) as u8;
        hdr[5] = (((frame_len & 0x07) << 5) as u8) | 0x1F;
        hdr[6] = 0xFC;
        let mut frame = hdr.to_vec();
        frame.extend_from_slice(&[0xCC; 9]);

        let mut framer = FramerState::new(AudioCodec::AacLc, 48_000);
        let mut all_frames = Vec::new();
        for byte in &frame {
            all_frames.extend(framer.feed(&[*byte]));
        }
        assert_eq!(all_frames.len(), 1);
        assert_eq!(all_frames[0].data.len(), 16);
    }

    #[test]
    fn adts_framer_resyncs_on_garbage() {
        // Garbage prefix + valid frame. The framer should skip the garbage
        // and recover the frame.
        let frame_len: u16 = 16;
        let mut hdr = [0u8; 7];
        hdr[0] = 0xFF;
        hdr[1] = 0xF1;
        hdr[2] = 0x4C;
        hdr[3] = 0x40 | (((frame_len >> 11) & 0x03) as u8);
        hdr[4] = ((frame_len >> 3) & 0xFF) as u8;
        hdr[5] = (((frame_len & 0x07) << 5) as u8) | 0x1F;
        hdr[6] = 0xFC;
        let mut payload = vec![0x00, 0x11, 0x22, 0x33, 0x44]; // garbage
        payload.extend_from_slice(&hdr);
        payload.extend_from_slice(&[0xDD; 9]);

        let mut framer = FramerState::new(AudioCodec::AacLc, 48_000);
        let frames = framer.feed(&payload);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.len(), 16);
    }

    #[test]
    fn mp2_bitrate_table_known_values() {
        // MPEG-1 Layer II, bitrate idx 1 = 32 kbps
        assert_eq!(mp2_bitrate_kbps(0b11, 1), 32);
        // MPEG-1 Layer II, bitrate idx 14 = 384 kbps
        assert_eq!(mp2_bitrate_kbps(0b11, 14), 384);
        // Invalid bitrate (15) returns 0
        assert_eq!(mp2_bitrate_kbps(0b11, 15), 0);
        // Free format (0) returns 0
        assert_eq!(mp2_bitrate_kbps(0b11, 0), 0);
    }

    #[test]
    fn mp2_sample_rate_table_known_values() {
        assert_eq!(mp2_sample_rate_hz(0b11, 0), 44_100); // MPEG-1
        assert_eq!(mp2_sample_rate_hz(0b11, 1), 48_000);
        assert_eq!(mp2_sample_rate_hz(0b11, 2), 32_000);
        assert_eq!(mp2_sample_rate_hz(0b10, 1), 24_000); // MPEG-2
        assert_eq!(mp2_sample_rate_hz(0b11, 3), 0); // reserved
    }

    #[test]
    fn ac3_frame_size_table_known_values() {
        // 192 kbps @ 48 kHz: 384 16-bit words = 768 bytes.
        // frmsizecod 20 / 2 = row 10 (192 kbps), fscod 0 (48 kHz)
        assert_eq!(ac3_frame_size_words(0, 20), 384);
        // 192 kbps @ 44.1 kHz: 417 words = 834 bytes
        assert_eq!(ac3_frame_size_words(1, 20), 417);
        // 192 kbps @ 32 kHz: 576 words = 1152 bytes
        assert_eq!(ac3_frame_size_words(2, 20), 576);
        // Invalid frmsizecod
        assert_eq!(ac3_frame_size_words(0, 100), 0);
        // Reserved fscod
        assert_eq!(ac3_frame_size_words(3, 20), 0);
    }

    #[test]
    fn compute_backoff_caps_at_max() {
        assert_eq!(compute_backoff(1), Duration::from_secs(2));
        assert_eq!(compute_backoff(2), Duration::from_secs(4));
        assert_eq!(compute_backoff(3), Duration::from_secs(8));
        assert_eq!(compute_backoff(4), Duration::from_secs(16));
        assert_eq!(compute_backoff(5), Duration::from_secs(30)); // capped
        assert_eq!(compute_backoff(99), Duration::from_secs(30));
    }

    #[test]
    fn check_ffmpeg_available_doesnt_panic() {
        // We don't assert the result — depends on the host machine.
        let _ = check_ffmpeg_available();
    }

    #[test]
    fn spawn_without_ffmpeg_returns_clean_error() {
        // Even if ffmpeg IS available, this test exercises the validation
        // path: passing channels=0 should error before spawning.
        let params = EncoderParams {
            codec: AudioCodec::AacLc,
            sample_rate: 48_000,
            channels: 0,
            target_bitrate_kbps: 128,
            target_sample_rate: 48_000,
            target_channels: 2,
        };
        let cancel = CancellationToken::new();
        let stats = Arc::new(OutputStatsAccumulator::new(
            "test-output".into(),
            "test output".into(),
            "rtmp".into(),
        ));
        let err = AudioEncoder::spawn(
            params, cancel, "test-flow".into(), "test-output".into(), stats, None,
        ).unwrap_err();
        match err {
            AudioEncoderError::InvalidPcmFormat { .. } => {}
            other => panic!("expected InvalidPcmFormat, got {other:?}"),
        }
    }
}
