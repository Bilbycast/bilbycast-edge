// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Streaming MPEG-TS video elementary-stream replacement.
//!
//! The video analog of [`super::ts_audio_replace::TsAudioReplacer`].
//! Consumes raw 188-byte-aligned TS, decodes the video ES (H.264 / HEVC)
//! in-process via `video-engine::VideoDecoder`, re-encodes it through a
//! feature-gated `VideoEncoder` backend (libx264 / libx265 / NVENC), and
//! muxes the result back into the output TS:
//!
//! - PAT is observed to learn `pmt_pid` and the program_number.
//! - Every PMT-PID packet goes through the reassembling
//!   `ts_pmt_edit::PsiUnitStage`; the program's PMT section (found by
//!   program_number, wherever it sits in the unit) teaches `video_pid` +
//!   source `stream_type`.
//! - That PMT is rebuilt: target stream_type, `PCR_PID` = video PID, the
//!   video descriptor policy, a content-tracked version and a valid CRC —
//!   also when the PMT spans packets.
//! - Until that PMT has been parsed only PSI / SI (PIDs ≤ 0x1F and the PMT
//!   PID) is forwarded: source video, audio and PCRs ahead of it would go
//!   out untranscoded, with a CC jump when the replacer took the PID over.
//!   A PMT that never parses opens the gate after 5 s (today's
//!   passthrough; the engage watchdog says why).
//! - Every input PCR on the source PCR_PID — inside a video payload packet
//!   too — leaves as an adaptation-field-only packet on the video PID at
//!   the same stream position, value and DI unchanged. The re-encoded PES
//!   carry no PCR: `engine::ts_pcr_remux`, after this stage, delays the
//!   input's PCR timeline by one measured transcode allowance.
//! - Video PID packets are buffered into PES, flushed on each PUSI,
//!   fed to the decoder, the resulting frames go through the encoder,
//!   and the encoded bitstream is repacketized as fresh TS. A decoded
//!   frame whose PTS does not advance past the last one admitted (within
//!   1 s) is dropped before the encoder, so output DTS never steps back.
//! - Every other PID (audio, PAT, null, etc.) is forwarded unchanged.
//!
//! # Scaling
//!
//! Resolution scaling is fully wired through
//! [`crate::engine::video_encode_util::ScaledVideoEncoder`]: when
//! `video_encode.width` / `.height` are set, the lazy-open path opens
//! the encoder at the requested dimensions and inserts a
//! `video_engine::VideoScaler` (libswscale) between the decoder and
//! encoder. Mid-stream source-resolution changes rebuild the scaler
//! while keeping the encoder open so downstream decoders don't see a
//! resolution flip. Operators must request even dimensions; validation
//! at config load enforces this so libx264 / libx265 / HW backends
//! never reject the open call.
//!
//! # Thread safety
//!
//! `TsVideoReplacer` is `Send` but not `Sync`. It must be driven from a
//! blocking-aware context (same contract as `TsAudioReplacer`) because
//! the in-process codec calls take single-digit milliseconds per frame.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::config::models::VideoEncodeConfig;

use super::ts_parse::{
    extract_pcr, parse_pat_programs, pcr_only_packet, ts_discontinuity_indicator, ts_has_payload,
    ts_payload_offset, ts_pid, ts_pusi, PAT_PID, TS_PACKET_SIZE, TS_SYNC_BYTE,
};

/// Lock-free runtime counters for the streaming TS video replacer.
///
/// Each counter is incremented by the replacer hot path and read once per
/// second by the stats snapshot path. Mirrors the shape of
/// `engine::audio_encode::EncodeStats` so the wiring on the accumulator side
/// is identical.
#[derive(Debug, Default)]
pub struct VideoEncodeStats {
    /// Compressed video frames fed into the decoder (one per source PES).
    pub input_frames: AtomicU64,
    /// Encoded video frames emitted by the encoder.
    pub output_frames: AtomicU64,
    /// Frames dropped inside the replacer (decode error, encoder backpressure,
    /// supervisor restart). Distinct from the broadcast `packets_dropped`.
    pub dropped_frames: AtomicU64,
    /// Most recent end-to-end frame latency through the replacer, in microseconds.
    pub last_latency_us: AtomicU64,
    /// Number of times the encoder supervisor restarted the backend.
    pub supervisor_restarts: AtomicU64,
    /// Decoded frames dropped before the encoder because their PTS did not
    /// advance past the last admitted frame (a splice without a clean
    /// random-access point). Also counted in `dropped_frames`.
    pub non_monotonic_frames_dropped: AtomicU64,
    /// Non-PSI packets dropped before the program's PMT was parsed.
    pub pre_pmt_dropped_packets: AtomicU64,
    /// Source video PID the replacer locked onto (discovered from the PMT
    /// or pinned via `video_encode.source_video_pid`). `0` means "not
    /// yet known" — the replacer hasn't seen the PMT yet on this run.
    /// Surfaced on stats snapshots so operators can see at a glance which
    /// source PID is being transcoded — answers "which video did the
    /// transcoder pick?" without digging through the PSI catalogue.
    pub source_pid: std::sync::atomic::AtomicU16,
    /// Source video stream_type byte (e.g. `0x1B` for H.264). Set
    /// alongside `source_pid` once the PMT is observed. `0` means unknown.
    pub source_stream_type: std::sync::atomic::AtomicU8,
    /// Backend the encoder actually opened with after lazy-open. The
    /// snapshot path prefers this over the requested-codec label
    /// captured on the stats handle, so the manager-UI badge reflects
    /// Auto-chain demotion (e.g. NVENC → x264 fallback). The encoder
    /// pipeline is given a clone of this `Arc` via
    /// [`crate::engine::video_encode_util::ScaledVideoEncoder::set_resolved_backend_sink`].
    pub resolved_backend: Arc<crate::engine::video_encode_util::ResolvedBackendCell>,
}

// ─────────────────────────── Public surface ───────────────────────────

/// Errors raised when constructing a [`TsVideoReplacer`].
#[derive(Debug)]
#[allow(dead_code)]
pub enum TsVideoReplaceError {
    /// Codec name not recognised at the config layer. Should have been
    /// caught by validation but surface cleanly anyway.
    UnknownCodec(String),
    /// This bilbycast build was compiled without the matching video
    /// encoder feature flag (`video-encoder-x264`, etc.).
    EncoderDisabled(&'static str),
    /// The dependent `media-codecs` feature is disabled, which means
    /// `video-engine` is not compiled in.
    VideoEngineMissing,
    /// `resolve_video_encoder` rejected the (codec, chroma, bit_depth)
    /// request — Auto found nothing on this host, or an explicit
    /// backend can't do the chroma cell, or the runtime probe didn't
    /// run. Carries the reason tag + rendered message so the spawn
    /// path can emit a structured event.
    EncoderUnavailable(String, String),
}

impl std::fmt::Display for TsVideoReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCodec(c) => write!(f, "unknown video codec '{c}'"),
            Self::EncoderDisabled(feat) => {
                write!(f, "video encoder disabled: rebuild with `{feat}` feature")
            }
            Self::VideoEngineMissing => write!(
                f,
                "video-engine is not compiled in (enable the media-codecs feature)"
            ),
            Self::EncoderUnavailable(_reason, msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for TsVideoReplaceError {}

/// Streaming MPEG-TS video elementary-stream replacer.
///
/// See module-level docs for the algorithm. Not `Sync`.
pub struct TsVideoReplacer {
    #[cfg(feature = "media-codecs")]
    inner: inner::Inner,
    /// Human-readable description for logging ("x264 @ 4000 kbps").
    description: String,
    /// Shared atomic counters surfaced via [`Self::stats_handle`] so the
    /// per-output stats accumulator can register them at startup.
    stats: Arc<VideoEncodeStats>,
    /// Internal-decoder counters surfaced via
    /// [`Self::decode_stats_handle`]. Pairs with the encode counters so
    /// the manager pipeline emits `video_decode` + `video_encode` →
    /// "Video Transcode" badge (vs encode-only ST 2110-20 ingress, which
    /// emits only `video_encode` → "Video Encode" badge).
    decode_stats: Arc<crate::engine::video_decode_stats::VideoDecodeStats>,
    /// One-shot IDR request for the next encoded frame. External callers
    /// (e.g. the input forwarder on a flow switch) flip this to `true`;
    /// the replacer consumes and clears it before the next encode call.
    /// The inner `Inner` holds a clone of this same `Arc`; this field is
    /// only kept so [`Self::force_idr_handle`] can hand it back out.
    #[allow(dead_code)]
    force_idr_on_next_frame: Arc<AtomicBool>,
    /// One-shot "input was switched" request. External callers (the
    /// flow's switch watcher on each transcoding output) flip this to
    /// `true` when the active input changes; the replacer consumes and
    /// clears it on entry to `process()` and runs the same
    /// `reset_source_state()` path that already fires on codec/PID
    /// change. Without this, a same-codec same-PID input swap leaves
    /// the replacer's PTS anchor pointing at the old input's epoch and
    /// the receiver sees PTS values incompatible with the master-clock-
    /// generated output PCR, which can stick the decoder permanently.
    #[allow(dead_code)]
    external_reset_on_switch: Arc<AtomicBool>,
}

impl TsVideoReplacer {
    /// Attach the per-input edge-added A/V skew reporter. See the inner
    /// `av_skew` field doc-comment.
    pub fn set_av_skew_reporter(
        &mut self,
        reporter: Arc<crate::stats::av_skew::AvSkewReporter>,
    ) {
        #[cfg(feature = "media-codecs")]
        {
            self.inner.av_skew = Some(reporter);
        }
        #[cfg(not(feature = "media-codecs"))]
        {
            let _ = reporter;
        }
    }

    /// Wire the manager event sender + output id so the replacer can emit a
    /// one-shot `video_transcode_decode_stalled` Warning when the decoder
    /// consumes input but produces no frames (silent audio-only output).
    /// Safe to call zero or one time before `process()` runs. `None`-event
    /// callers (the audio-only / ingress paths) simply never call this.
    pub fn set_decode_stall_watchdog(
        &mut self,
        event_sender: crate::manager::events::EventSender,
        output_id: impl Into<String>,
    ) {
        #[cfg(feature = "media-codecs")]
        {
            self.inner.event_sender = Some(event_sender);
            self.inner.output_id = output_id.into();
        }
        #[cfg(not(feature = "media-codecs"))]
        {
            let _ = (event_sender, output_id);
        }
    }

    /// Ingress variant of [`Self::set_decode_stall_watchdog`]. Wires the same
    /// one-shot decode-stall watchdog but emits the Warning **input-scoped**
    /// (keyed on `input_id`) so the manager attributes a silent ingress
    /// transcode failure to the input rather than a phantom output. Used by
    /// `engine::input_transcode::register_ingress_stats` — the path a
    /// `media_player` / RTP / SRT / … input with a `video_encode` block takes.
    pub fn set_decode_stall_watchdog_input(
        &mut self,
        event_sender: crate::manager::events::EventSender,
        input_id: impl Into<String>,
    ) {
        #[cfg(feature = "media-codecs")]
        {
            self.inner.event_sender = Some(event_sender);
            self.inner.output_id = input_id.into();
            self.inner.decode_stall_input_scope = true;
        }
        #[cfg(not(feature = "media-codecs"))]
        {
            let _ = (event_sender, input_id);
        }
    }
}

impl TsVideoReplacer {
    /// Build a new replacer from a `video_encode` block. Codec state is
    /// opened lazily on the first decoded frame.
    ///
    /// `force_idr`, when `Some`, is an externally-owned one-shot flag that
    /// lets upstream logic (e.g. the input forwarder on flow switch) ask
    /// the encoder to emit an IDR on its next frame. When `None`, the
    /// replacer allocates its own handle — useful for output-side callers
    /// that don't need external keyframe control.
    ///
    /// PID rewriting (the operator's `pid_overrides` map) is handled by the
    /// downstream `TsPidOverridesRewriter` stage — the replacer re-encodes
    /// video on the same source PID it learned from the PMT and lets that
    /// stage rename the PID afterwards.
    #[cfg(feature = "media-codecs")]
    pub fn new(
        cfg: &VideoEncodeConfig,
        force_idr: Option<Arc<AtomicBool>>,
    ) -> Result<Self, TsVideoReplaceError> {
        let stats = Arc::new(VideoEncodeStats::default());
        let decode_stats =
            Arc::new(crate::engine::video_decode_stats::VideoDecodeStats::new());
        let force_idr = force_idr.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let external_reset = Arc::new(AtomicBool::new(false));
        let inner = inner::Inner::from_config(
            cfg,
            stats.clone(),
            decode_stats.clone(),
            force_idr.clone(),
            external_reset.clone(),
            cfg.source_video_pid,
        )?;
        let description = inner.target_description();
        Ok(Self {
            inner,
            description,
            stats,
            decode_stats,
            force_idr_on_next_frame: force_idr,
            external_reset_on_switch: external_reset,
        })
    }

    /// Stub constructor when `media-codecs` is compiled out — returns
    /// `VideoEngineMissing` so callers fail cleanly at output startup.
    #[cfg(not(feature = "media-codecs"))]
    pub fn new(
        _cfg: &VideoEncodeConfig,
        _force_idr: Option<Arc<AtomicBool>>,
    ) -> Result<Self, TsVideoReplaceError> {
        Err(TsVideoReplaceError::VideoEngineMissing)
    }

    /// Human-readable summary of the configured encoder.
    pub fn target_description(&self) -> &str {
        &self.description
    }

    /// Shared handle to the atomic stats counters. Callers register this
    /// with [`crate::stats::collector::OutputStatsAccumulator::set_video_encode_stats`]
    /// at output startup.
    pub fn stats_handle(&self) -> Arc<VideoEncodeStats> {
        self.stats.clone()
    }

    /// Shared handle to the internal-decoder counters. Outputs register
    /// this via [`crate::stats::collector::OutputStatsAccumulator::set_video_decode_stats`]
    /// at spawn time so the egress pipeline emits `video_decode` — paired
    /// with `video_encode` the UI renders a single "Video Transcode" badge.
    pub fn decode_stats_handle(
        &self,
    ) -> Arc<crate::engine::video_decode_stats::VideoDecodeStats> {
        self.decode_stats.clone()
    }

    /// Attach the input-side `VideoDecodeStatsHandle` the replacer should
    /// keep refreshing as the PMT learns the source codec. Mirrors the
    /// audio-side `TsAudioReplacer::with_input_decode_handle` so ingress
    /// callers (see `engine::input_transcode::register_ingress_stats`)
    /// can flip the manager UI from `Video Encode` to `Video Transcode`
    /// once both decode + encode stats are present.
    #[cfg(feature = "media-codecs")]
    pub fn with_input_decode_handle(
        &mut self,
        handle: Arc<crate::stats::collector::VideoDecodeStatsHandle>,
    ) {
        self.inner.input_decode_handle = Some(handle);
        // Push whatever we know right now (placeholder until the PMT is
        // observed) so the UI doesn't lag a tick.
        self.inner.refresh_input_decode_label();
    }

    /// Stub variant when `media-codecs` is compiled out — the replacer
    /// never opens in that build, so there's no decoder to refresh.
    #[cfg(not(feature = "media-codecs"))]
    pub fn with_input_decode_handle(
        &mut self,
        _handle: Arc<crate::stats::collector::VideoDecodeStatsHandle>,
    ) {
    }

    /// Shared handle to the one-shot IDR request flag. Setting this to
    /// `true` causes the replacer's next encoded frame to be an IDR. The
    /// flag is consumed (cleared) by the replacer once honoured, so rapid
    /// repeated sets simply collapse into a single keyframe.
    ///
    /// Input-side callers normally pass their external flag into
    /// [`Self::new`] instead; this accessor exists so output-side callers
    /// (or tests) can retrieve the internally-created handle.
    #[allow(dead_code)]
    pub fn force_idr_handle(&self) -> Arc<AtomicBool> {
        self.force_idr_on_next_frame.clone()
    }

    /// Shared handle to the one-shot "input was switched" request flag.
    /// The flow's per-output switch watcher sets this to `true` when the
    /// active input changes (`active_input_rx.changed().await`); the
    /// replacer consumes and clears it on entry to its next `process()`
    /// call and runs the same `reset_source_state()` path that fires on
    /// codec/PID change, so the new input's first frame is encoded as an
    /// IDR with a fresh PTS anchor. Idempotent under rapid repeated
    /// switches — collapses into a single reset.
    #[allow(dead_code)]
    pub fn external_reset_handle(&self) -> Arc<AtomicBool> {
        self.external_reset_on_switch.clone()
    }

    /// Feed one chunk of 188-byte-aligned TS into the replacer. Output
    /// TS is appended to `output`. Mis-aligned input is passed through
    /// verbatim (best-effort for boundary recovery in the caller).
    #[allow(unused_variables)]
    pub fn process(&mut self, input_ts: &[u8], output: &mut Vec<u8>) {
        #[cfg(feature = "media-codecs")]
        {
            self.inner.process(input_ts, output);
        }
        #[cfg(not(feature = "media-codecs"))]
        {
            output.extend_from_slice(input_ts);
        }
    }

    /// The video PID whose PES this replacer re-encodes, once locked —
    /// what the chain's trailing PCR stage checks against its PCR.
    pub fn replaced_pid(&self) -> Option<u16> {
        #[cfg(feature = "media-codecs")]
        {
            self.inner.video_pid
        }
        #[cfg(not(feature = "media-codecs"))]
        {
            None
        }
    }

    /// Drain buffered PES / encoder state. Call on graceful shutdown.
    #[allow(dead_code, unused_variables)]
    pub fn flush(&mut self, output: &mut Vec<u8>) {
        #[cfg(feature = "media-codecs")]
        {
            self.inner.flush(output);
        }
    }
}

// ─────────────────────────── Implementation ───────────────────────────

#[cfg(feature = "media-codecs")]
mod inner {
    use super::*;
    use crate::engine::transcode_engage::{
        EngageEvent, PassthroughCc, PrePmtGate, TranscodeEngageWatch, TranscodeKind,
    };
    use crate::engine::ts_pmt_edit::{
        parse_pmt, pmt_index, rebuild_pmt_section, EsEdit, OutVersion, PmtEdit, PsiUnit,
        PsiUnitStage,
    };
    use crate::engine::video_encode_util::{FrameCadence, ScaledVideoEncoder};
    use video_codec::{VideoCodec, VideoEncoderCodec};
    use video_engine::VideoDecoder;

    /// PTS modulus (33-bit space, MPEG-TS spec).
    const PTS_MODULUS_90K: u64 = 1u64 << 33;
    /// Mask for 33-bit PTS values.
    const PTS_MASK_33B: u64 = PTS_MODULUS_90K - 1;
    /// Backward PTS step within which a decoded frame is taken to be out of
    /// order (and dropped) rather than the start of a new epoch: 1 s at
    /// 90 kHz. A larger step either way is an epoch change and passes.
    const PTS_JUMP_THRESHOLD_90K: u64 = 90_000;

    /// Frame-admission verdict for one decoded frame (defect 7c): the PTS to
    /// stamp, or `None` when the frame must be dropped before the encoder.
    ///
    /// `last` is the last admitted PTS. A frame whose PTS is at or behind it
    /// by at most [`PTS_JUMP_THRESHOLD_90K`] is out of order — FFmpeg's H.264
    /// decoder emits leading pictures of a splice without a clean random
    /// access point that way — and is dropped. A frame without a PTS gets
    /// `last + interval`, or `anchor` before any frame was admitted.
    pub(super) fn admit_pts(
        last: Option<u64>,
        frame_pts: Option<u64>,
        interval_90k: u64,
        anchor: u64,
    ) -> Option<u64> {
        let pts = match (frame_pts, last) {
            (Some(p), _) => p & PTS_MASK_33B,
            (None, Some(l)) => l.wrapping_add(interval_90k) & PTS_MASK_33B,
            (None, None) => anchor & PTS_MASK_33B,
        };
        if let Some(l) = last {
            let back = (l + PTS_MODULUS_90K - pts) % PTS_MODULUS_90K;
            if back <= PTS_JUMP_THRESHOLD_90K {
                return None;
            }
        }
        Some(pts)
    }

    /// Input frames the decoder may consume with **zero** decoded output
    /// before [`Inner::check_decode_stall`] declares a decode stall. Sized
    /// comfortably above every legitimate transient where output lags input
    /// (first-IDR wait, B-frame reorder, the deferred-encoder-open window of
    /// at most [`UNLOCKED_FRAME_CAP`] frames) so the watchdog never
    /// false-fires at startup or across an input switch. Matches the ST 2110
    /// egress watchdog's precedent (`EGRESS_DECODE_STALL_AUS = 200`): ~4 s at
    /// 50 fps, ~8 s at 25 fps.
    const DECODE_STALL_INPUT_FRAMES: u64 = 200;

    /// Decoded frames the replacer waits for a measurable rate before it
    /// opens the encoder at the fallback rate (~2 s of broadcast video).
    const UNLOCKED_FRAME_CAP: u32 = 60;

    /// H.264 PES the replacer passes over waiting for one that carries an
    /// SPS to open the decoder on, before opening on whatever arrives
    /// (~6-12 s; broadcast repeats the SPS every GOP).
    const SPS_WAIT_PES: u32 = 300;

    /// Whether an Annex B access unit carries an H.264 SPS NAL unit.
    fn carries_h264_sps(au: &[u8]) -> bool {
        video_engine::annexb_nal_units(au).any(|n| n.first().is_some_and(|b| b & 0x1F == 7))
    }

    pub struct Inner {
        #[allow(dead_code)]
        target_family: VideoCodec,
        fps_num: Option<u32>,
        fps_den: Option<u32>,

        pmt_pid: Option<u16>,
        /// program_number of the program the replacer follows (the lowest
        /// in the PAT). PMT sections are matched on it.
        program_number: Option<u16>,
        /// Pre-PMT gate: PSI only until the program's PMT parses.
        gate: PrePmtGate,
        /// Source PCR_PID from the program's PMT, before the rebuild points
        /// it at the video PID. `None` for 0x1FFF.
        pub(super) source_pcr_pid: Option<u16>,
        /// Last passthrough CC per PID (seeds `out_video_cc` on takeover).
        passthrough_cc: PassthroughCc,
        /// The PAT maps another program to the same PMT PID.
        pmt_pid_shared: bool,
        /// Reassembling PMT-PID stage — see `ts_pmt_edit::PsiUnitStage`.
        pmt_stage: PsiUnitStage,
        /// "Configured but never engaged" watchdog
        /// (`video_transcode_source_*`).
        engage: TranscodeEngageWatch,
        pub(super) video_pid: Option<u16>,
        /// Operator-pinned source video PID (`video_encode.source_video_pid`).
        /// When `Some`, PMT discovery looks for this PID specifically; when
        /// `None`, the legacy first-matching-codec rule applies.
        source_video_pid_pin: Option<u16>,
        /// De-duplication key for the `video_source_pid_not_found`
        /// warning — `Some((pinned, actual))` while the pin is unmet,
        /// cleared when the pin is found again.
        last_pinned_warn: Option<(u16, u16)>,
        source_stream_type: u8,
        target_stream_type: u8,

        pes_buffer: Vec<u8>,
        pes_started: bool,
        pending_pts: Option<u64>,

        pub(super) decoder: Option<VideoDecoder>,
        /// Shared encoder pipeline — wraps `VideoEncoder` + optional
        /// `VideoScaler`. Lazy-opens on the first decoded frame and
        /// handles the `video_encode.width` / `.height` override by
        /// scaling the decoded frame to the target resolution instead
        /// of letting libavcodec silently crop.
        pub(super) pipeline: ScaledVideoEncoder,

        pub(super) out_video_cc: u8,
        /// PTS anchor in the encoder time base (1 / fps_num).
        out_frame_count: i64,
        /// 90 kHz PTS for the next emitted PES, anchored to the first
        /// source PES PTS. Used as a fallback when the source-PTS queue
        /// is empty (e.g. encoder catch-up bursts).
        pts_90k: u64,
        pts_anchored: bool,
        /// Source PES PTSes pending output, one entry per source decoded
        /// frame fed into the encoder. Drained in FIFO order on each
        /// emitted output frame, so output PES PTS values track source's
        /// monotonic clock instead of the encoder's wall-time pipeline
        /// delay. This is the matching half of the audio replacer's
        /// `src_pts_queue` — together they keep output A/V in lock with
        /// source A/V regardless of how lumpy the per-stream encoder
        /// pipelines are. With `max_b_frames = 0` (default for the in-
        /// process pipeline) encode order = display order, so a plain
        /// FIFO queue gives the right pts. Once B-frame encoding is
        /// wired in, this should become DTS-aware (push/pop by encoded
        /// frame's `dts`-ordered position).
        pub(super) src_pts_queue: std::collections::VecDeque<u64>,

        /// Frame-rate meter over the decoder's frame PTS — what the
        /// encoder is actually called with, once per frame. The rate
        /// is never taken from PES DTS: a field-coded source (PAFF
        /// H.264, MPEG-2 field pictures) carries one field per PES, so
        /// its DTS step is the FIELD rate while the decoder hands out
        /// woven frames at half of it. See [`FrameCadence`].
        cadence: FrameCadence,
        /// True once the encoder rate is known — pinned by the
        /// operator, measured by [`Self::cadence`], or the encoder is
        /// already open (an input switch: libavcodec's time base cannot
        /// change). Until then decoded frames are dropped before the
        /// encoder, which cannot open without a rate.
        pub(super) source_fps_locked: bool,
        /// Decoded frames dropped while the rate was unknown. After
        /// [`UNLOCKED_FRAME_CAP`] the fallback rate is taken, so a source
        /// whose frames carry no usable PTS still gets an encoder.
        unlocked_frames: u32,
        /// Last input PES DTS (or PTS when the PES has no DTS) and the
        /// last delta between two within 90..=90 000 ticks — the coded
        /// PICTURE rate, used only by the fallback, divided by the
        /// PES-per-frame ratio below.
        last_input_dts: Option<u64>,
        pub(super) pes_dts_step_90k: Option<u64>,
        /// Video PES consumed and frames decoded since the last source
        /// reset: their ratio is 2 on a field-coded source.
        pub(super) pes_since_reset: u64,
        pub(super) frames_since_reset: u64,
        /// H.264 PES passed over while waiting for one that carries the
        /// SPS to open the decoder on (bounded by [`SPS_WAIT_PES`]).
        pub(super) pes_awaiting_sps: u32,
        /// One-shot guard for `video_encode_fps_mismatch` (the measured
        /// rate disagrees with the rate the encoder runs at: a pinned
        /// `video_encode.fps_num` / `fps_den`, or the rate an earlier
        /// source opened it at). Re-armed per source. The mismatch is
        /// load-bearing — A/V sync drift on cellPTP24 (ESPN.ts NTSC
        /// 29.97 fps + pinned 25/1) traced to it.
        fps_mismatch_warned: bool,

        description: String,
        stats: Arc<VideoEncodeStats>,
        decode_stats: Arc<crate::engine::video_decode_stats::VideoDecodeStats>,
        force_idr: Arc<AtomicBool>,
        /// Shared with the outer `TsVideoReplacer::external_reset_on_switch`.
        /// Checked once per `process()` entry; on `true` we run
        /// `reset_source_state("input switched")` and clear the flag.
        external_reset: Arc<AtomicBool>,
        /// Optional manager event sender for the decode-stall watchdog. When
        /// set, [`Inner::check_decode_stall`] emits a one-shot Warning
        /// (`video_transcode_decode_stalled`) if the decoder consumes input
        /// but produces no frames for a sustained window — the silent
        /// audio-only-output failure shape. `None` keeps the replacer silent
        /// (tests / ingress callers that don't wire events).
        pub event_sender: Option<crate::manager::events::EventSender>,
        /// Entity id this replacer serves — scopes the stall event. Holds an
        /// output id by default, or an input id when the watchdog was wired via
        /// [`TsVideoReplacer::set_decode_stall_watchdog_input`] (ingress path).
        pub output_id: String,
        /// When `true` the decode-stall Warning is emitted input-scoped
        /// (ingress transcoder, keyed on the input id); the default `false`
        /// keeps it output-scoped (the output transcode path). Set by the
        /// outer [`TsVideoReplacer::set_decode_stall_watchdog_input`], hence
        /// `pub` like the other watchdog fields.
        pub decode_stall_input_scope: bool,
        /// One-shot guard so the decode-stall event fires once per stall;
        /// re-armed when the decoder starts producing frames again.
        decode_stall_warned: bool,
        /// Highest `output_frames` count observed — the progress detector
        /// that drives the re-arm.
        last_output_frames_seen: u64,
        /// `input_frames` value when output last advanced; the stall window
        /// is `current input_frames − this`.
        input_frames_at_last_output: u64,
        /// PTS (90 kHz) of the last decoded frame admitted to the encoder —
        /// see [`admit_pts`]. Reset with the source.
        last_admitted_pts_90k: Option<u64>,
        /// Measured frame interval (90 kHz): the step a PTS-less frame
        /// advances by. Frame, not field, units. Learned only from frames
        /// that carried a decoder PTS — see [`Inner::admit_decoded`].
        decoded_interval_90k: Option<u64>,
        /// PTS of the last decoded frame that carried one, and the frames
        /// decoded since it.
        last_real_pts_90k: Option<u64>,
        frames_since_real_pts: u32,

        /// Operator's hardware-decoder preference for the input decode
        /// side of this transcode. Defaults to `Auto` (VAAPI ≻ NVDEC ≻
        /// QSV ≻ CPU per host capabilities). Resolved on the first
        /// decoded frame so the static-capabilities snapshot is
        /// guaranteed installed.
        hw_decode_pref: crate::config::models::HwDecodePreference,

        /// Edge-added A/V skew reporter (`stats::av_skew`). The video
        /// replacer stamps emitted PES with PTS dequeued from
        /// `src_pts_queue` (= source values), so its delta is 0 by
        /// design — reported anyway so a future regression in this
        /// invariant becomes visible on the dashboard immediately.
        pub av_skew: Option<Arc<crate::stats::av_skew::AvSkewReporter>>,

        /// Optional input-side `video_decode_stats` handle the replacer
        /// keeps refreshing as the PMT learns the source codec / geometry.
        /// Set by ingress-side callers via
        /// [`super::TsVideoReplacer::with_input_decode_handle`]; `None` on
        /// output-side replacers (output-side decode-stats refresh runs
        /// through the per-output `OutputStatsAccumulator` already).
        pub input_decode_handle:
            Option<Arc<crate::stats::collector::VideoDecodeStatsHandle>>,

        /// Output PMT `version_number`, derived from content: bumps when
        /// the rebuilt PMT differs from the last one — including when the
        /// audio replacer ahead of this stage changed its stream_type, so
        /// chained stages compose instead of this one re-stamping over the
        /// audio stage's bump — and on every `reset_source_state()`;
        /// otherwise holds, so an unchanged PMT never flaps.
        pmt_version: OutVersion,
    }

    impl Inner {
        pub fn from_config(
            cfg: &VideoEncodeConfig,
            stats: Arc<VideoEncodeStats>,
            decode_stats: Arc<crate::engine::video_decode_stats::VideoDecodeStats>,
            force_idr: Arc<AtomicBool>,
            external_reset: Arc<AtomicBool>,
            source_video_pid_pin: Option<u16>,
        ) -> Result<Self, TsVideoReplaceError> {
            // Resolve `*_auto` strings AND validate explicit backends
            // against the host's chroma/bit-depth matrix. We pull the
            // **full chain** so the lazy-open path can fall through on
            // `avcodec_open2` failure (Auto only — explicit backends
            // are a one-element chain). The legacy `parse_codec` path
            // is kept so existing tests that pass concrete strings
            // without a probed-caps snapshot still work — we only
            // invoke the resolver when a snapshot is installed (the
            // production path).
            let backend_chain: Vec<VideoEncoderCodec> =
                match crate::engine::hardware_probe::static_capabilities() {
                    Some(_) => match crate::engine::hardware_probe::resolve_chain_for_video_encode_config(cfg) {
                        Ok(chain) => {
                            if cfg.codec.ends_with("_auto") || cfg.codec == "auto" {
                                let names: Vec<&str> =
                                    chain.iter().map(|r| r.ffmpeg_name()).collect();
                                tracing::info!(
                                    "video_encode auto-resolved '{}' → chain {:?} (head fires first; later entries are fall-through)",
                                    cfg.codec,
                                    names,
                                );
                            }
                            chain.iter().map(|r| r.as_video_encoder_codec()).collect()
                        }
                        Err(e) => {
                            return Err(TsVideoReplaceError::EncoderUnavailable(
                                e.as_reason().to_string(),
                                e.message(),
                            ));
                        }
                    },
                    None => vec![parse_codec(&cfg.codec)?],
                };
            // Every backend in the chain produces the same output codec
            // family (Auto family is locked; explicit codec is locked).
            // Pick the head — `target_family` / `target_stream_type` /
            // description are family-level concerns, not backend-level.
            let backend_head = *backend_chain
                .first()
                .expect("resolver guaranteed at least one candidate");
            let target_family = backend_head.family();
            let target_stream_type = target_family.stream_type();

            let description = format!(
                "{} @ {} kbps",
                backend_head.ffmpeg_name(),
                cfg.bitrate_kbps.unwrap_or(4000),
            );

            // Default to 30 fps at open-time — the pipeline will use the
            // operator's fps_num/fps_den when set, otherwise it picks
            // 30/1 at first-frame lazy-open. MPEG-TS outputs emit
            // SPS/PPS in-band on every IDR (global_header = false).
            let (fps_num, fps_den) = match (cfg.fps_num, cfg.fps_den) {
                (Some(n), Some(d)) => (n, d),
                _ => (30, 1),
            };
            let mut pipeline = ScaledVideoEncoder::with_backend_chain(
                cfg.clone(),
                backend_chain,
                fps_num,
                fps_den,
                false,
                "ts_video_replace",
            );
            pipeline.set_resolved_backend_sink(stats.resolved_backend.clone());
            // An MPEG-TS re-encode feeds broadcast receivers, which display
            // interlace natively: `scan: auto` field-codes an interlaced
            // source here (see `VideoScan`).
            pipeline.allow_auto_field_coding();

            Ok(Self {
                target_family,
                fps_num: cfg.fps_num,
                fps_den: cfg.fps_den,
                pmt_pid: None,
                program_number: None,
                gate: PrePmtGate::default(),
                source_pcr_pid: None,
                passthrough_cc: PassthroughCc::default(),
                pmt_pid_shared: false,
                pmt_stage: PsiUnitStage::new("ts_video_replace"),
                engage: TranscodeEngageWatch::new(TranscodeKind::Video, source_video_pid_pin),
                video_pid: None,
                source_video_pid_pin,
                last_pinned_warn: None,
                source_stream_type: 0,
                target_stream_type,
                pes_buffer: Vec::with_capacity(256 * 1024),
                pes_started: false,
                pending_pts: None,
                decoder: None,
                pipeline,
                out_video_cc: 0,
                out_frame_count: 0,
                pts_90k: 0,
                pts_anchored: false,
                src_pts_queue: std::collections::VecDeque::with_capacity(64),
                cadence: FrameCadence::new(),
                source_fps_locked: cfg.fps_num.is_some() && cfg.fps_den.is_some(),
                unlocked_frames: 0,
                last_input_dts: None,
                pes_dts_step_90k: None,
                pes_since_reset: 0,
                frames_since_reset: 0,
                pes_awaiting_sps: 0,
                fps_mismatch_warned: false,
                description,
                stats,
                decode_stats,
                force_idr,
                external_reset,
                last_admitted_pts_90k: None,
                decoded_interval_90k: None,
                last_real_pts_90k: None,
                frames_since_real_pts: 0,
                hw_decode_pref: cfg.hw_decode.unwrap_or_default(),
                av_skew: None,
                input_decode_handle: None,
                pmt_version: OutVersion::new(),
                event_sender: None,
                output_id: String::new(),
                decode_stall_input_scope: false,
                decode_stall_warned: false,
                last_output_frames_seen: 0,
                input_frames_at_last_output: 0,
            })
        }

        /// Best-effort refresh of the registered input-side
        /// `video_decode_stats` handle. Called whenever
        /// `self.source_stream_type` changes; no-op when no handle was
        /// attached (output-side replacers).
        pub(super) fn refresh_input_decode_label(&self) {
            let Some(h) = self.input_decode_handle.as_ref() else {
                return;
            };
            let codec_label: &'static str = match self.source_stream_type {
                0x1B => "H.264",
                0x24 => "HEVC",
                0x02 => "MPEG-2",
                _ => "",
            };
            if !codec_label.is_empty() {
                h.set_input_codec(codec_label);
            }
        }

        pub fn target_description(&self) -> String {
            self.description.clone()
        }

        /// Drop decoder / PES / PTS state that is tied to the current
        /// source stream. Called when the source codec or video PID
        /// changes mid-flow (seamless input switching between inputs
        /// with different codecs, or a PAT/PMT program re-layout).
        ///
        /// The encoder pipeline is intentionally *not* reset — it targets
        /// the output's configured codec, which never changes.
        fn reset_source_state(&mut self, reason: &str) {
            tracing::info!("ts_video_replace: {reason}; reopening decoder");
            self.pes_buffer.clear();
            self.pes_started = false;
            self.pending_pts = None;
            self.decoder = None;
            // The new source's frames are admitted afresh; a backward step
            // across the switch is a new source, not a reordered picture.
            self.last_admitted_pts_90k = None;
            self.decoded_interval_90k = None;
            self.last_real_pts_90k = None;
            self.frames_since_real_pts = 0;
            // Re-anchor PTS to the new input's first frame so downstream
            // A/V stays in sync with the audio replacer (which will also
            // re-anchor on the audio-PID codec swap).
            self.pts_anchored = false;
            self.src_pts_queue.clear();
            // A new source measures its own rate. An encoder that is
            // already open keeps the rate it opened at (libavcodec's time
            // base is fixed), so its frames flow at once; the meter keeps
            // running and `check_rate` says so if the new source's rate
            // differs.
            self.cadence.reset();
            self.source_fps_locked =
                (self.fps_num.is_some() && self.fps_den.is_some()) || self.pipeline.is_open();
            self.unlocked_frames = 0;
            self.last_input_dts = None;
            self.pes_dts_step_90k = None;
            self.pes_since_reset = 0;
            self.frames_since_reset = 0;
            self.pes_awaiting_sps = 0;
            self.fps_mismatch_warned = false;
            // First post-switch encoded frame must be an IDR so receivers
            // get a clean entry point right at the switch boundary.
            self.force_idr.store(true, Ordering::Relaxed);
            // Bump the rewritten-PMT version (mod 32) so receivers see a
            // distinct version on the next PMT and re-parse. Content
            // changes bump on their own; this covers a reset whose PMT is
            // byte-identical to the previous source's.
            self.pmt_version.bump();
            // A new source gets a fresh engage window.
            self.engage.on_reset();
        }

        pub fn process(&mut self, input_ts: &[u8], output: &mut Vec<u8>) {
            self.process_at(input_ts, output, std::time::Instant::now());
        }

        pub(super) fn process_at(
            &mut self,
            input_ts: &[u8],
            output: &mut Vec<u8>,
            now: std::time::Instant,
        ) {
            if input_ts.is_empty() {
                return;
            }
            // External "input was switched" trigger. Set by the flow's
            // per-output switch watcher when `active_input_rx` changes.
            // Same-codec same-PID swaps don't fire the codec/PID-change
            // reset path below, so without this hook the replacer keeps
            // its previous PTS anchor and the decoder its old references.
            if self.external_reset.swap(false, Ordering::Relaxed) {
                self.reset_source_state("input switched");
            }
            if !input_ts.len().is_multiple_of(TS_PACKET_SIZE) {
                output.extend_from_slice(input_ts);
                return;
            }

            let mut offset = 0;
            while offset + TS_PACKET_SIZE <= input_ts.len() {
                let pkt = &input_ts[offset..offset + TS_PACKET_SIZE];
                offset += TS_PACKET_SIZE;

                if pkt[0] != TS_SYNC_BYTE {
                    output.extend_from_slice(pkt);
                    continue;
                }

                let pid = ts_pid(pkt);

                if pid == PAT_PID && ts_pusi(pkt) {
                    let mut programs = parse_pat_programs(pkt);
                    if !programs.is_empty() {
                        programs.sort_by_key(|(num, _)| *num);
                        let (new_program, new_pmt_pid) = programs[0];
                        self.pmt_pid_shared =
                            programs.iter().filter(|(_, p)| *p == new_pmt_pid).count() > 1;
                        self.engage.note_pat(new_program, new_pmt_pid);
                        if self.pmt_pid != Some(new_pmt_pid)
                            || self.program_number != Some(new_program)
                        {
                            if self.pmt_pid.is_some() {
                                // Input switched and chose a different PMT
                                // PID — anything cached about the old
                                // program is stale, and the gate re-arms
                                // until the new program's PMT parses.
                                self.video_pid = None;
                                self.source_stream_type = 0;
                                self.reset_source_state("PMT PID changed");
                                self.gate.rearm(now);
                                self.source_pcr_pid = None;
                            }
                            if self.pmt_pid != Some(new_pmt_pid) {
                                self.pmt_stage = PsiUnitStage::new("ts_video_replace");
                            }
                            self.pmt_pid = Some(new_pmt_pid);
                            self.program_number = Some(new_program);
                        }
                    }
                }

                // Every packet on the PMT PID goes through the reassembling
                // stage; a complete unit is inspected and, once the video
                // PID is known, the program's PMT is rebuilt (target
                // stream_type, video descriptor policy, PCR_PID = video PID,
                // content-tracked version). Other sections on the PID stay
                // byte-identical.
                if Some(pid) == self.pmt_pid {
                    if ts_pusi(pkt) {
                        self.engage.note_pmt_pusi();
                    }
                    if let Some(unit) = self.pmt_stage.push(pkt, output) {
                        self.handle_pmt_unit(unit, output);
                    }
                    continue;
                }

                // Pre-PMT gate: PSI / SI (PIDs 0x00-0x1F — PAT, NIT, SDT,
                // EIT, TDT) and null stuffing only. Source video, audio and
                // PCRs ahead of the PMT would reach the wire untranscoded,
                // with raw timestamps and a CC jump once the replacer took
                // over.
                if self.gate.drops(pid, now, "ts_video_replace") {
                    self.stats.pre_pmt_dropped_packets.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                // Every input PCR keeps its stream position as an
                // adaptation-field-only packet on the video PID — taken
                // before `feed_video_packet` swallows the packet it rides
                // in. Value and DI unchanged: the chain's trailing
                // `ts_pcr_remux` stage owns the delay. CC repeats the last
                // payload CC on the PID (the one before the first payload
                // before any).
                if let Some(vpid) = self.video_pid
                    && Some(pid) == self.source_pcr_pid
                    && let Some(pcr) = extract_pcr(pkt)
                {
                    let cc = self.out_video_cc.wrapping_sub(1) & 0x0F;
                    output.extend_from_slice(&pcr_only_packet(
                        vpid,
                        cc,
                        pcr,
                        ts_discontinuity_indicator(pkt),
                    ));
                }

                if Some(pid) == self.video_pid {
                    self.feed_video_packet(pkt, output);
                    continue;
                }

                self.passthrough_cc.note(pid, pkt);
                output.extend_from_slice(pkt);
            }

            // Surface the silent audio-only-output failure: a decoder that
            // consumes input but never produces frames (decode_errors
            // saturating, or a broken HW decode backend on this host).
            self.check_decode_stall();
            // ...and the silent "never engaged" one: no video PID learned
            // at all, so the decoder never even sees input.
            self.engage.note_packets((input_ts.len() / TS_PACKET_SIZE) as u64);
            self.poll_engage(now);
        }

        /// Advance the engage watchdog and emit whatever it raises (on the
        /// decode-stall watchdog's sender, with the same scoping). Returns
        /// the event for tests.
        pub(super) fn poll_engage(&mut self, now: std::time::Instant) -> Option<EngageEvent> {
            let ev = self.engage.tick(now)?;
            self.emit_engage(&ev);
            Some(ev)
        }

        fn emit_engage(&self, ev: &EngageEvent) {
            if let Some(es) = self.event_sender.as_ref() {
                self.engage.emit(ev, es, &self.output_id, self.decode_stall_input_scope);
            }
        }

        /// Inspect one complete PMT-PID unit, learn the video ES and the
        /// source PCR_PID, rebuild the program's PMT, and emit.
        fn handle_pmt_unit(&mut self, mut unit: PsiUnit, output: &mut Vec<u8>) {
            self.engage.note_pmt_unit(unit.first_table_id());
            let idx = self
                .program_number
                .and_then(|p| pmt_index(unit.sections(), p, self.pmt_pid_shared));
            let Some(i) = idx else {
                self.pmt_stage.emit(unit, output);
                return;
            };
            let section = unit.sections()[i].clone();
            // Never learn from, or rebuild (and so re-CRC), a damaged PMT.
            let Some(view) = parse_pmt(&section)
                .filter(|_| crate::engine::ts_parse::mpeg2_crc32(&section) == 0)
            else {
                self.pmt_stage.emit(unit, output);
                return;
            };
            // The program's PMT is known: the pre-PMT gate opens, and the
            // input PCR is followed on the source PCR_PID (before the
            // rebuild below points PCR_PID at the video PID).
            self.gate.open();
            self.source_pcr_pid = (view.pcr_pid != 0x1FFF).then_some(view.pcr_pid);
            let sel = select_video_es(&view, self.source_video_pid_pin);
            self.engage.note_pmt_parsed(sel.es.clone(), sel.unsupported_candidate);
            if let Some((vpid, vst)) = sel.chosen {
                // Operator-pinned PID not in PMT — warn once per distinct
                // (pinned, actual) pair. De-duplication clears when the pin
                // reappears.
                if let Some(pin) = self.source_video_pid_pin {
                    if pin != vpid && self.last_pinned_warn != Some((pin, vpid)) {
                        tracing::warn!(
                            error_code = "video_source_pid_not_found",
                            pinned_pid = format!("0x{pin:04X}"),
                            actual_pid = format!("0x{vpid:04X}"),
                            actual_stream_type = format!("0x{vst:02X}"),
                            "video_encode.source_video_pid pin not present in PMT — falling back to first-matching-codec video (pinned 0x{pin:04X} → actual 0x{vpid:04X})"
                        );
                        if let Some(es) = self.event_sender.as_ref() {
                            crate::engine::transcode_engage::emit_pinned_pid_absent(
                                TranscodeKind::Video,
                                es,
                                &self.output_id,
                                self.decode_stall_input_scope,
                                pin,
                                vpid,
                                vst,
                            );
                        }
                        self.last_pinned_warn = Some((pin, vpid));
                    } else if pin == vpid && self.last_pinned_warn.is_some() {
                        self.last_pinned_warn = None;
                    }
                }
                let codec_changed =
                    self.source_stream_type != 0 && self.source_stream_type != vst;
                let pid_changed = self.video_pid.is_some() && self.video_pid != Some(vpid);
                if codec_changed || pid_changed {
                    self.reset_source_state(&format!(
                        "source changed: stream_type {:#04x} -> {:#04x}, pid {:?} -> {}",
                        self.source_stream_type, vst, self.video_pid, vpid
                    ));
                }
                if self.video_pid != Some(vpid) {
                    // Taking the PID over: continue the CC sequence of
                    // whatever was passed through on it (the gate
                    // fallback, or a previous program layout).
                    if let Some(cc) = self.passthrough_cc.take_next_after(vpid) {
                        self.out_video_cc = cc;
                    }
                }
                self.video_pid = Some(vpid);
                self.source_stream_type = vst;
                // Surface for stats / UI badge: which source PID is the
                // transcoder actually transcoding? Updated on every PMT
                // discovery so input swaps are visible immediately.
                self.stats.source_pid.store(vpid, Ordering::Relaxed);
                self.stats.source_stream_type.store(vst, Ordering::Relaxed);
                // Refresh the input-side video_decode_stats handle's codec
                // label ("H.264" / "HEVC"). No-op on output-side replacers.
                self.refresh_input_decode_label();
                if let Some(ev) = self.engage.note_locked(vpid, vst) {
                    self.emit_engage(&ev);
                }
            } else {
                if self.video_pid.is_some() {
                    self.reset_source_state("PMT no longer carries decodable video");
                    self.video_pid = None;
                    self.source_stream_type = 0;
                }
                self.engage.note_unlocked(std::time::Instant::now());
            }
            // Rebuild the PMT once we know the video PID: target
            // stream_type, video descriptor policy, and PCR_PID = video_pid
            // (where this module emits PCR — a source whose PMT pointed
            // PCR_PID at a dedicated PCR PID would otherwise advertise PCR
            // somewhere nothing carries it, a TR 101 290 P1.6 violation).
            // When the source PMT already matches, only the version stamp
            // can differ, and it is content-tracked, so receivers see no
            // version flap.
            if let Some(video_pid) = self.video_pid {
                let edit = [EsEdit::Video { pid: video_pid, stream_type: self.target_stream_type }];
                if let Some(mut new_section) = rebuild_pmt_section(
                    &section,
                    &PmtEdit { es: &edit, pcr_pid: Some(video_pid), ..Default::default() },
                ) {
                    self.pmt_version.stamp(&mut new_section);
                    unit.replace_section(i, new_section);
                    self.pmt_stage.emit(unit, output);
                    return;
                }
            }
            // Not re-encoding: once this stage has stamped a version, the
            // passthrough PMT is stamped from the same sequence so it can
            // never repeat a version the rebuilt PMT already carried (see
            // `OutVersion::has_stamped`). Before any stamp it stays
            // byte-identical.
            if self.pmt_version.has_stamped() {
                let mut passthrough = section;
                self.pmt_version.stamp(&mut passthrough);
                unit.replace_section(i, passthrough);
            }
            self.pmt_stage.emit(unit, output);
        }

        /// One-shot decode-stall watchdog.
        ///
        /// Fires a Warning event (`video_transcode_decode_stalled`) when the
        /// internal decoder has consumed input frames but produced no output
        /// for a sustained window — the failure the field report described,
        /// where the SRT output silently carries audio only and
        /// `video_decode_stats` shows `output_frames == 0` with
        /// `decode_errors == input_frames`. Keyed on input-vs-output
        /// *progress* (not a raw `output == 0` test) so it catches both the
        /// "never produced a frame" and the "stalled mid-stream" cases, and
        /// re-arms once the decoder starts producing frames again.
        fn check_decode_stall(&mut self) {
            let Some(es) = self.event_sender.as_ref() else {
                return;
            };
            let inp = self.decode_stats.input_frames.load(Ordering::Relaxed);
            let out = self.decode_stats.output_frames.load(Ordering::Relaxed);

            // Decoder is producing — record the progress point and re-arm.
            if out > self.last_output_frames_seen {
                self.last_output_frames_seen = out;
                self.input_frames_at_last_output = inp;
                self.decode_stall_warned = false;
                return;
            }

            let gap = inp.saturating_sub(self.input_frames_at_last_output);
            if gap >= DECODE_STALL_INPUT_FRAMES && !self.decode_stall_warned {
                self.decode_stall_warned = true;
                let errors = self.decode_stats.decode_errors.load(Ordering::Relaxed);
                tracing::warn!(
                    output_id = %self.output_id,
                    input_frames = inp,
                    output_frames = out,
                    decode_errors = errors,
                    "ts_video_replace: decoder consumed {gap} input frames with no \
                     decoded output — video transcode decode stalled; output is \
                     carrying audio only"
                );
                // Same event either way; only the scoping noun + the
                // input/output id slot on the Event differ. Output scope keeps
                // the exact wording the output transcode path always emitted.
                let (noun, subject) = if self.decode_stall_input_scope {
                    ("Input", "this input")
                } else {
                    ("Output", "the output")
                };
                let message = format!(
                    "{noun} '{}': the video transcoder is consuming input but \
                     producing no decoded frames ({errors} decode errors over \
                     {gap} input frames) — {subject} is carrying audio only. \
                     Likely a decode-backend failure for this source; try a \
                     different decoder backend or remove video_encode to use \
                     passthrough.",
                    self.output_id,
                );
                let details = serde_json::json!({
                    "error_code": "video_transcode_decode_stalled",
                    "input_frames": inp,
                    "output_frames": out,
                    "decode_errors": errors,
                    "source_stream_type": self.source_stream_type,
                });
                if self.decode_stall_input_scope {
                    es.emit_input_with_details(
                        crate::manager::events::EventSeverity::Warning,
                        crate::manager::events::category::FLOW,
                        message,
                        &self.output_id,
                        details,
                    );
                } else {
                    es.emit_output_with_details(
                        crate::manager::events::EventSeverity::Warning,
                        crate::manager::events::category::FLOW,
                        message,
                        &self.output_id,
                        details,
                    );
                }
            }
        }

        pub fn flush(&mut self, output: &mut Vec<u8>) {
            if self.pes_started && !self.pes_buffer.is_empty() {
                let pes = std::mem::take(&mut self.pes_buffer);
                let _ = self.consume_pes(&pes, output);
                self.pes_started = false;
            }
            if self.pipeline.is_open()
                && let Ok(frames) = self.pipeline.flush()
            {
                let Some(vpid) = self.video_pid else {
                    return;
                };
                for ef in frames {
                    self.emit_encoded_frame(vpid, &ef.data, output);
                }
            }
        }

        /// Step a PTS-less frame advances by: the measured interval between
        /// admitted frames, else the pinned frame rate, else 25 fps. Never
        /// the PES DTS step, which is the per-FIELD delta on PAFF.
        fn frame_interval_90k(&self) -> u64 {
            if let Some(i) = self.decoded_interval_90k {
                return i;
            }
            match (self.fps_num, self.fps_den) {
                (Some(n), Some(d)) if n > 0 => (90_000u64 * d as u64 / n as u64).max(1),
                _ => 3_600,
            }
        }

        /// Frame admission (see [`admit_pts`]): the PTS to stamp on this
        /// decoded frame, or `None` to drop it before the encoder. A drop
        /// touches nothing else — not the PTS queue, the force-IDR request
        /// or the frame counter — so the next admitted frame takes them.
        ///
        /// The interval a PTS-less frame steps by is learned from decoder
        /// PTS only: the span between two frames that carried one, divided
        /// by the frames decoded across it, and only within 10-200 fps. A
        /// derived PTS never teaches it — it would only confirm the guess it
        /// was derived from, and a guess above the true interval runs every
        /// derived frame past the next real PTS, which then drops as out of
        /// order (a source stamping every 12th picture at 29.97 fps lost
        /// every real timestamp and ran 20 % fast on the 25 fps default).
        pub(super) fn admit_decoded(&mut self, frame_pts: Option<i64>) -> Option<u64> {
            let pts = frame_pts.filter(|p| *p >= 0).map(|p| p as u64 & PTS_MASK_33B);
            self.frames_since_real_pts = self.frames_since_real_pts.saturating_add(1);
            if let Some(p) = pts {
                if let Some(prev) = self.last_real_pts_90k {
                    let span = (p + PTS_MODULUS_90K - prev) % PTS_MODULUS_90K;
                    let step = span / u64::from(self.frames_since_real_pts);
                    if (450..=9_000).contains(&step) {
                        self.decoded_interval_90k = Some(step);
                    }
                }
                self.last_real_pts_90k = Some(p);
                self.frames_since_real_pts = 0;
            }
            let last = self.last_admitted_pts_90k;
            let Some(admitted) = admit_pts(last, pts, self.frame_interval_90k(), self.pts_90k)
            else {
                self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                self.stats.non_monotonic_frames_dropped.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            self.last_admitted_pts_90k = Some(admitted);
            Some(admitted)
        }

        /// Packetise one encoded frame with the next queued source PTS
        /// (DTS = PTS: the in-process encoders emit no B-frames). No PCR —
        /// the input's PCR positions travel as their own packets.
        fn emit_encoded_frame(&mut self, vpid: u16, data: &[u8], output: &mut Vec<u8>) {
            // Prefer the source PTS from the queue; fall back to the
            // running anchor when an encoder catch-up burst emits more
            // frames than were pushed since the last drain.
            let queued = self.src_pts_queue.pop_front();
            if queued.is_some()
                && let Some(rep) = self.av_skew.as_ref()
            {
                rep.set_video_delta(0); // PTS == source value
            }
            let pts_for_pes = queued.unwrap_or(self.pts_90k);
            let pes = build_video_pes(data, pts_for_pes);
            for p in &packetize_ts(vpid, &pes, &mut self.out_video_cc) {
                output.extend_from_slice(p);
            }
            // Keep the fallback anchor monotonic from the latest emitted
            // PTS so a later queue-exhausted emit still advances.
            self.pts_90k = pts_for_pes.wrapping_add(self.frame_interval_90k()) & PTS_MASK_33B;
            self.stats.output_frames.fetch_add(1, Ordering::Relaxed);
        }

        fn feed_video_packet(&mut self, pkt: &[u8], output: &mut Vec<u8>) {
            if !ts_has_payload(pkt) {
                return;
            }
            let pusi = ts_pusi(pkt);
            let payload_start = ts_payload_offset(pkt);
            if payload_start >= TS_PACKET_SIZE {
                return;
            }
            let payload = &pkt[payload_start..];

            if pusi {
                if self.pes_started && !self.pes_buffer.is_empty() {
                    let pes = std::mem::take(&mut self.pes_buffer);
                    let _ = self.consume_pes(&pes, output);
                }
                self.pes_buffer.clear();
                self.pes_buffer.extend_from_slice(payload);
                self.pes_started = true;
            } else if self.pes_started {
                // DoS guard: a stream that stops emitting PUSI would otherwise
                // grow this video PES buffer without bound. Drop + resync.
                // Bounds one coded frame, not throughput; sized above the
                // largest single I-frame of a high-bitrate contribution feed.
                const PES_CAP: usize = 64 * 1024 * 1024;
                if self.pes_buffer.len().saturating_add(payload.len()) > PES_CAP {
                    self.pes_buffer.clear();
                    self.pes_started = false;
                } else {
                    self.pes_buffer.extend_from_slice(payload);
                }
            }
        }

        fn consume_pes(&mut self, pes: &[u8], output: &mut Vec<u8>) -> Result<(), ()> {
            let (es_data, pts, pes_dts) = match extract_pes_video(pes) {
                Some(x) => {
                    self.stats.input_frames.fetch_add(1, Ordering::Relaxed);
                    x
                }
                None => {
                    self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                    return Err(());
                }
            };
            // When the source has no DTS (PTS_DTS_flags = 0b10), PTS itself
            // is the decode timestamp.
            let pes_dts = pes_dts.or(pts);
            self.pending_pts = pts;
            let pes_arrived_us = crate::util::time::now_us();

            if !self.pts_anchored
                && let Some(p) = pts
            {
                self.pts_90k = p;
                self.pts_anchored = true;
            }

            // The coded-picture step, for the fallback rate only (see
            // `fallback_rate`): on a field-coded source it is the FIELD
            // step. The encoder rate is measured from decoded frames.
            if let Some(dts) = pes_dts {
                if let Some(prev) = self.last_input_dts {
                    let delta = dts.wrapping_sub(prev) & PTS_MASK_33B;
                    if (90..=90_000).contains(&delta) {
                        self.pes_dts_step_90k = Some(delta);
                    }
                }
                self.last_input_dts = Some(dts);
            }

            if self.decoder.is_none() {
                let src_codec = match VideoCodec::from_stream_type(self.source_stream_type) {
                    Some(c) => c,
                    None => {
                        self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        return Err(());
                    }
                };
                // Resolve HW transcode-decoder preference. The static
                // probe runs at startup; we read it here to pick the
                // best backend the host has compiled in for this codec
                // family. Auto picks VAAPI ≻ NVDEC ≻ QSV ≻ CPU per
                // host capabilities. On any resolution error (forced
                // backend missing / capabilities not yet probed) we
                // fall back to CPU rather than fail the flow.
                let decoder_backend = match crate::engine::hardware_probe::static_capabilities() {
                    Some(caps) => {
                        match crate::engine::hardware_probe::resolve_transcode_decoder(
                            &self.hw_decode_pref,
                            Some(&caps),
                        ) {
                            Ok(r) => r.as_backend(),
                            Err(e) => {
                                tracing::warn!(
                                    "ts_video_replace: hw_decode preference {:?} unavailable ({:?}); falling back to CPU",
                                    self.hw_decode_pref,
                                    e,
                                );
                                video_engine::DecoderBackend::Cpu
                            }
                        }
                    }
                    None => video_engine::DecoderBackend::Cpu,
                };
                // H.264: open on the PES that carries the SPS. Nothing
                // decodes before one anyway, and the decoder's reorder depth
                // is seeded from the AU it opens on (`ReorderSeed`): 0 when
                // the SPS declares it (libavcodec then applies the declared
                // depth, 0 for IPPP), else 1, which keeps a join on a non-IDR
                // I picture from showing a GOP of garbage. Opening on a PES
                // without the SPS would cost a declaring IPPP source a frame
                // of latency for good. Bounded, for a stream whose SPS never
                // shows as a NAL unit here.
                if src_codec == VideoCodec::H264
                    && !carries_h264_sps(&es_data)
                    && self.pes_awaiting_sps < SPS_WAIT_PES
                {
                    self.pes_awaiting_sps += 1;
                    return Ok(());
                }
                match VideoDecoder::open_opts(
                    src_codec,
                    video_engine::DecoderOptions {
                        backend: decoder_backend,
                        reorder_seed: video_engine::ReorderSeed::FromAccessUnit(&es_data),
                        ..Default::default()
                    },
                ) {
                    Ok(d) => {
                        if !matches!(decoder_backend, video_engine::DecoderBackend::Cpu) {
                            tracing::info!(
                                "ts_video_replace: opened HW decoder (backend={:?})",
                                decoder_backend,
                            );
                        }
                        self.decoder = Some(d);
                        // Whether an interlaced frame from it is a woven
                        // frame (H.264, MPEG-2) or a single field (HEVC).
                        self.pipeline.set_source_codec(src_codec);
                    }
                    Err(e) => {
                        tracing::error!("ts_video_replace: failed to open decoder: {e}");
                        self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        return Err(());
                    }
                }
            }

            if let Some(dec) = self.decoder.as_mut() {
                // Pass the source PES PTS (already in 90 kHz ticks) into
                // libavcodec's reorder queue so each decoded frame echoes
                // it back via `frame.pts()`. That lets the encoder-side
                // queue tag every output PES with the matching source
                // PTS instead of the sample-counted anchor — see the
                // src_pts_queue field for the rationale. A PES without a
                // PTS goes in without one, so its frame carries none and
                // is neither measured nor admitted as a real timestamp.
                self.decode_stats.inc_input();
                self.pes_since_reset += 1;
                let sent = match pts {
                    Some(p) => dec.send_packet_with_pts(&es_data, p as i64),
                    None => dec.send_packet(&es_data),
                };
                if let Err(e) = sent {
                    // Partial/invalid packet — keep going.
                    self.decode_stats.inc_error();
                    tracing::debug!("ts_video_replace: send_packet: {e:?}");
                }
            }

            // Drain every frame the decoder can produce right now.
            loop {
                let frame = match self.decoder.as_mut().unwrap().receive_frame() {
                    Ok(f) => f,
                    Err(_) => break,
                };
                self.decode_stats.inc_output();
                self.frames_since_reset += 1;

                // Monotonic admission BEFORE anything else: a frame whose
                // PTS does not advance past the last admitted one (within
                // 1 s) is a splice's out-of-order leading picture. Dropping
                // it here keeps the PTS queue balanced (never pushed), and
                // leaves a pending force-IDR and the frame counter for the
                // next admitted frame.
                let decoder_pts = frame.pts();
                let Some(src_pts_for_frame) = self.admit_decoded(decoder_pts) else {
                    continue;
                };

                // The encoder rate: measured from the frames the encoder is
                // handed, once per frame. Until it is known the encoder
                // cannot open (libavcodec's time base is fixed at open), so
                // the frame is dropped; the force-IDR request (raised at
                // construction and on every reset) waits for the first
                // frame encoded after the lock.
                self.cadence.observe(decoder_pts);
                if !self.source_fps_locked {
                    if !self.try_lock_rate() {
                        continue;
                    }
                } else {
                    self.check_rate();
                }

                // Push the source PTS into the FIFO queue so the emit
                // path can pop it for each output PES. The decoder
                // propagates `pkt.pts → frame.pts` through its reorder
                // window, so for B-frame source streams we get the
                // display-order PTS automatically.
                self.src_pts_queue.push_back(src_pts_for_frame);

                // One-shot IDR request (forwarder signals on flow switch).
                // Consume the flag here so the keyframe lands on the very
                // first post-switch frame, not somewhere later in the GOP.
                // This is a no-op until the encoder lazy-opens inside
                // `pipeline.encode` below, but that's fine — the first
                // frame after a switch is always an IDR anyway (decoder
                // needs it to resync).
                let force_idr_now = self.force_idr.swap(false, Ordering::Relaxed);
                if force_idr_now {
                    self.pipeline.force_next_keyframe();
                }
                if !self.pipeline.is_open()
                    && let Some(dec) = self.decoder.as_ref()
                {
                    // The decoder's SAR, for a frame that carries none.
                    self.pipeline.set_source_sar_fallback(dec.sample_aspect_ratio());
                }

                let encoded = match self.pipeline.encode(&frame, Some(self.out_frame_count)) {
                    Ok(frames) => frames,
                    Err(e) => {
                        // `encoder_open_failed` is the only terminal
                        // error shape; any per-frame encode error is
                        // logged as a dropped frame and we keep going.
                        if !self.pipeline.is_open() {
                            tracing::error!(
                                "ts_video_replace: failed to open encoder: {e}"
                            );
                            return Err(());
                        }
                        tracing::debug!("ts_video_replace: encode error: {e}");
                        self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        // Keep the queue balanced: we pushed one entry
                        // for this decoded frame but the encoder won't
                        // emit anything for it. Pop the entry now so
                        // future emits stay in lock-step with the input
                        // we actually fed through.
                        let _ = self.src_pts_queue.pop_back();
                        continue;
                    }
                };
                self.out_frame_count += 1;
                if let Some(why) = self.pipeline.take_interlace_notice() {
                    self.emit_interlace_unavailable(&why);
                }

                // Encoded packets always ride the source video PID. Any
                // operator PID rename lands on the downstream
                // `TsPidOverridesRewriter` stage; the PMT rewrite above
                // advertises the source PID so the rewriter can match
                // and rename consistently.
                let vpid = self.video_pid.unwrap();
                for ef in encoded {
                    self.emit_encoded_frame(vpid, &ef.data, output);
                }
                let lat = crate::util::time::now_us().saturating_sub(pes_arrived_us);
                self.stats.last_latency_us.store(lat, Ordering::Relaxed);
            }

            Ok(())
        }

        /// One Warning per open: an explicit `video_encode.scan:
        /// interlaced` is coding progressive (no backend in the chain could
        /// open for field coding, or the source hands out single fields).
        pub(super) fn emit_interlace_unavailable(&self, why: &str) {
            let Some(es) = self.event_sender.as_ref() else {
                return;
            };
            let noun = if self.decode_stall_input_scope { "Input" } else { "Output" };
            let message = format!(
                "{noun} '{}': video_encode.scan=interlaced cannot be honoured — {why}; \
                 encoding progressive",
                self.output_id,
            );
            let details = serde_json::json!({
                "error_code": "video_encode_interlace_unavailable",
                "reason": why,
                "source_stream_type": self.source_stream_type,
            });
            let severity = crate::manager::events::EventSeverity::Warning;
            let category = crate::manager::events::category::VIDEO_ENCODE;
            if self.decode_stall_input_scope {
                es.emit_input_with_details(severity, category, message, &self.output_id, details);
            } else {
                es.emit_output_with_details(severity, category, message, &self.output_id, details);
            }
        }

        /// Video PES per decoded frame since the last reset — 2 on a
        /// field-coded (PAFF / field-picture) source, 1 otherwise.
        fn pes_per_frame(&self) -> f64 {
            self.pes_since_reset as f64 / self.frames_since_reset.max(1) as f64
        }

        /// Lock the encoder rate for an unpinned source once
        /// [`FrameCadence`] can say it, or at the fallback after
        /// [`UNLOCKED_FRAME_CAP`] decoded frames. Returns whether the rate
        /// is now locked (the frame in hand is then the first one
        /// encoded).
        pub(super) fn try_lock_rate(&mut self) -> bool {
            self.unlocked_frames = self.unlocked_frames.saturating_add(1);
            let (n, d, from) = match self.cadence.rate() {
                Some((n, d)) => (n, d, "decoded-frame cadence"),
                None if self.unlocked_frames >= UNLOCKED_FRAME_CAP => {
                    let (n, d) = self.fallback_rate();
                    tracing::warn!(
                        "ts_video_replace: source frame rate not measurable from {} decoded \
                         frames (no usable frame PTS) — opening the encoder at {n}/{d}, {} \
                         (output cadence may not track the source)",
                        self.unlocked_frames,
                        if self.pes_dts_step_90k.is_some() {
                            "the PES DTS step over the PES-per-frame ratio"
                        } else {
                            "the 30 fps placeholder"
                        },
                    );
                    (n, d, "fallback")
                }
                None => return false,
            };
            let acquired = self.pipeline.set_fps_if_unopened(n, d);
            let pes_per_frame = self.pes_per_frame();
            tracing::info!(
                "ts_video_replace: source fps {n}/{d} ({:.3} fps) from {from}, {pes_per_frame:.2} \
                 PES per decoded frame{} — encoder lock {}",
                n as f64 / d.max(1) as f64,
                if pes_per_frame > 1.5 { " (field-coded: one field per PES)" } else { "" },
                if acquired { "ACQUIRED" } else { "MISSED (encoder already open)" },
            );
            self.source_fps_locked = true;
            true
        }

        /// The rate when the cadence cannot be measured: the PES DTS step
        /// times the rounded PES-per-frame ratio (a field-coded source's
        /// step is a field), else 30/1.
        pub(super) fn fallback_rate(&self) -> (u32, u32) {
            match self.pes_dts_step_90k {
                Some(step) => {
                    let per_frame = self.pes_per_frame().round().max(1.0);
                    crate::engine::video_encode_util::rate_from_frame_duration(
                        step as f64 * per_frame,
                    )
                }
                None => (30, 1),
            }
        }

        /// The measured rate against the rate the encoder runs at — a
        /// pinned `fps_num` / `fps_den`, or the rate an earlier source
        /// opened it at (an input switch cannot reopen it). One warning
        /// per source when they differ by more than 0.1 %.
        pub(super) fn check_rate(&mut self) {
            if self.fps_mismatch_warned {
                return;
            }
            let Some((mn, md)) = self.cadence.rate() else {
                return;
            };
            let (en, ed) = self.pipeline.fps();
            let measured_fps = mn as f64 / md.max(1) as f64;
            let encoder_fps = en as f64 / ed.max(1) as f64;
            let ratio = measured_fps / encoder_fps;
            let off_pct = (ratio - 1.0).abs() * 100.0;
            if off_pct <= 0.1 {
                return;
            }
            self.fps_mismatch_warned = true;
            // Default GOP when the operator left `gop_size` unset — mirrors
            // `video_encode_util::build_encoder_config` exactly, integer
            // division included.
            let default_gop_frames = 2 * (en / ed.max(1)).max(1);
            if let (Some(n), Some(d)) = (self.fps_num, self.fps_den) {
                // Operator pinned `video_encode.fps_num` / `fps_den` — but
                // the measured source rate disagrees. The encoder runs at
                // the operator's time_base; the wire PES PTS values come
                // from `src_pts_queue` (= source rate). Observed on ESPN.ts
                // NTSC 29.97 fps with operator-pinned 25 fps (cellPTP24 / v3
                // report).
                tracing::warn!(
                    error_code = "video_encode_fps_mismatch",
                    cause = "pinned",
                    measured_fps = format!("{:.3}", measured_fps),
                    pinned_fps_num = n,
                    pinned_fps_den = d,
                    pinned_fps = format!("{:.3}", encoder_fps),
                    // Field name is published in docs/events-and-alarms.md
                    // and may back out-of-tree alerting rules — do not
                    // rename it. `bitrate_multiplier` is additive alongside
                    // it.
                    drift_pct = format!("{:.2}", off_pct),
                    bitrate_multiplier = format!("{:.2}", ratio),
                    "ts_video_replace: source fps ({:.3}) disagrees \
                     with `video_encode.fps_num`/`fps_den` \
                     ({}/{} = {:.3}). Every source frame is still \
                     encoded and output PES PTS still carry the \
                     source clock, so this does not cause a \
                     proportional lipsync drift on this path — but \
                     the encoder is tuned for the wrong rate: actual \
                     bitrate runs ~{:.2}x the configured value, and \
                     a default GOP of {} frames spans {:.2}s instead \
                     of the intended 2s. Remove the pinned fps to let \
                     the encoder auto-lock to the source rate, or \
                     set it to match the source (e.g. 30000/1001 \
                     for NTSC 29.97).",
                    measured_fps,
                    n,
                    d,
                    encoder_fps,
                    ratio,
                    default_gop_frames,
                    default_gop_frames as f64 / measured_fps,
                );
            } else {
                tracing::warn!(
                    error_code = "video_encode_fps_mismatch",
                    cause = "input_switch",
                    measured_fps = format!("{:.3}", measured_fps),
                    encoder_fps_num = en,
                    encoder_fps_den = ed,
                    drift_pct = format!("{:.2}", off_pct),
                    bitrate_multiplier = format!("{:.2}", ratio),
                    "ts_video_replace: encoder lock MISSED — this source runs at {:.3} fps but \
                     the encoder opened at {en}/{ed} ({:.3} fps) for an earlier one and cannot \
                     reopen at a new rate. Every frame is still encoded and output PES PTS \
                     carry the source clock, but the bitrate runs ~{:.2}x the configured value \
                     and the SPS VUI advertises {:.3} fps. Restart the output to lock the new \
                     rate, or pin video_encode.fps_num / fps_den.",
                    measured_fps,
                    encoder_fps,
                    ratio,
                    encoder_fps,
                );
            }
        }
    }

    fn parse_codec(s: &str) -> Result<VideoEncoderCodec, TsVideoReplaceError> {
        match s {
            "x264" => Ok(VideoEncoderCodec::X264),
            "x265" => Ok(VideoEncoderCodec::X265),
            "h264_nvenc" => Ok(VideoEncoderCodec::H264Nvenc),
            "hevc_nvenc" => Ok(VideoEncoderCodec::HevcNvenc),
            "h264_qsv" => Ok(VideoEncoderCodec::H264Qsv),
            "hevc_qsv" => Ok(VideoEncoderCodec::HevcQsv),
            "h264_vaapi" => Ok(VideoEncoderCodec::H264Vaapi),
            "hevc_vaapi" => Ok(VideoEncoderCodec::HevcVaapi),
            "h264_rkmpp" => Ok(VideoEncoderCodec::H264Rkmpp),
            "hevc_rkmpp" => Ok(VideoEncoderCodec::HevcRkmpp),
            other => Err(TsVideoReplaceError::UnknownCodec(other.to_string())),
        }
    }

}

// ─────────────────────────── Shared helpers ───────────────────────────

/// The video ES the replacer should lock onto, plus what the engage
/// watchdog needs to explain a miss.
#[cfg(feature = "media-codecs")]
#[derive(Debug, Default)]
struct VideoSelection {
    chosen: Option<(u16, u8)>,
    /// The program carries video, but only in a codec the decoder does not
    /// handle (MPEG-4 part 2, AVS, VC-1, JPEG 2000 / XS, VVC, SVC / MVC
    /// sub-bitstreams, …).
    unsupported_candidate: bool,
    es: Vec<(u16, u8)>,
}

/// Decodable video stream_types: MPEG-1 / MPEG-2 / H.264 / H.265.
#[cfg(feature = "media-codecs")]
fn video_replaceable(st: u8) -> bool {
    matches!(st, 0x01 | 0x02 | 0x1B | 0x24)
}

/// Video stream_types the decoder cannot handle.
#[cfg(feature = "media-codecs")]
fn video_unsupported(st: u8) -> bool {
    matches!(st, 0x10 | 0x1E..=0x21 | 0x28..=0x33 | 0x42 | 0x61 | 0xD1 | 0xEA)
}

/// Select the video ES from a parsed PMT.
///
/// `pinned_pid` (`Some(pid)`): operator-pinned source PID via
/// `video_encode.source_video_pid`. Use that exact PID when its
/// stream_type is decodable; otherwise fall through to first-match for
/// graceful degradation (the caller raises `video_source_pid_not_found`).
///
/// `None`: first-match — first ES with stream_type in
/// `{0x01 MPEG-1, 0x02 MPEG-2, 0x1B H.264, 0x24 H.265}`.
#[cfg(feature = "media-codecs")]
fn select_video_es(
    view: &crate::engine::ts_pmt_edit::PmtView<'_>,
    pinned_pid: Option<u16>,
) -> VideoSelection {
    let mut sel = VideoSelection::default();
    let mut first = None;
    let mut pinned_hit = None;
    for es in &view.es {
        sel.es.push((es.pid, es.stream_type));
        if video_replaceable(es.stream_type) {
            if first.is_none() {
                first = Some((es.pid, es.stream_type));
            }
            if pinned_pid == Some(es.pid) {
                pinned_hit = Some((es.pid, es.stream_type));
            }
        } else if video_unsupported(es.stream_type) {
            sel.unsupported_candidate = true;
        }
    }
    sel.chosen = pinned_hit.or(first);
    sel
}

/// Extract the ES payload, PTS and DTS from a complete PES packet. The PTS
/// is `None` when the PES carries none (PTS_DTS_flags `0b00`): it must not
/// reach the decoder as a real timestamp of 0.
#[cfg(feature = "media-codecs")]
fn extract_pes_video(pes: &[u8]) -> Option<(Vec<u8>, Option<u64>, Option<u64>)> {
    if pes.len() < 9 || pes[0] != 0x00 || pes[1] != 0x00 || pes[2] != 0x01 {
        return None;
    }
    let header_data_len = pes[8] as usize;
    let es_start = 9 + header_data_len;
    if es_start >= pes.len() {
        return None;
    }
    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    let pts = (pts_dts_flags >= 2 && pes.len() >= 14).then(|| parse_pts(&pes[9..14]));
    // PTS_DTS_flags == 0b11 means PTS+DTS both present; DTS sits at
    // bytes 14..19. Otherwise DTS == PTS (monotonic, no B-frames in
    // source) — return None so the caller doesn't double-count the
    // single timestamp as both PTS and DTS samples.
    let dts = if pts_dts_flags == 0b11 && pes.len() >= 19 {
        Some(parse_pts(&pes[14..19]))
    } else {
        None
    };
    Some((pes[es_start..].to_vec(), pts, dts))
}

/// Decode the 5-byte PTS / DTS in a PES optional header per ISO/IEC
/// 13818-1 §2.4.3.7. The 33-bit value spans byte 0's bits 3-1
/// (top 3 bits, bits 32-30), byte 1 (bits 29-22), byte 2 bits 7-1
/// (bits 21-15), byte 3 (bits 14-7), byte 4 bits 7-1 (bits 6-0). Each
/// of bytes 0, 2, 4 reserves bit 0 as a marker bit set to '1' on the
/// wire — this parser ignores those marker bits and shifts past them.
#[cfg(feature = "media-codecs")]
fn parse_pts(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;
    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) & 0x7F) << 15
        | (b3 << 7)
        | ((b4 >> 1) & 0x7F)
}

/// Append a 5-byte PTS or DTS field to the PES being built, per
/// ISO/IEC 13818-1 §2.4.3.7. `marker_top_nibble` is the top-nibble code
/// for this timestamp role: `0x20` for "PTS only", `0x30` for "PTS w/
/// DTS following", `0x10` for "DTS". `value_33bit` is the 33-bit
/// timestamp at 90 kHz.
///
/// This implementation replaces an earlier in-line encoder that had
/// off-by-one shift bugs in bytes 0 and 2 — pts bit 30 was silently
/// dropped, pts bit 15 was silently dropped, and pts bits 31-32 / 16-22
/// were shifted into the slots below them. Standard receivers (Appear,
/// VLC, ffmpeg) decoded the resulting PES with garbled high-order bits
/// once `value_33bit` exceeded 32 768 ticks (~ 364 ms at 90 kHz), which
/// caused decoders to lose PTS lock after the first few frames.
#[cfg(feature = "media-codecs")]
fn write_pes_timestamp(pes: &mut Vec<u8>, marker_top_nibble: u8, value_33bit: u64) {
    let v = value_33bit & 0x1_FFFF_FFFF;
    // Byte 0: marker_top_nibble << 4 already includes bits 7-4. We
    // OR in (v[32:30] << 1) at result bits 3-1, plus marker_bit at bit 0.
    pes.push(marker_top_nibble | (((v >> 29) as u8) & 0x0E) | 0x01);
    // Byte 1: v[29:22].
    pes.push(((v >> 22) & 0xFF) as u8);
    // Byte 2: v[21:15] in result bits 7-1, marker_bit at bit 0.
    pes.push((((v >> 14) as u8) & 0xFE) | 0x01);
    // Byte 3: v[14:7].
    pes.push(((v >> 7) & 0xFF) as u8);
    // Byte 4: v[6:0] in result bits 7-1, marker_bit at bit 0.
    pes.push((((v << 1) as u8) & 0xFE) | 0x01);
}

/// Wrap an encoded video frame in a PES packet with PTS + DTS.
///
/// Video PES packets use stream_id 0xE0 and unbounded length (the
/// 16-bit length field is zero for video). Both PTS and DTS are emitted
/// even when they are equal — broadcast hardware decoders (Appear and
/// similar) strict-check the PTS_DTS_flags field and reject PES that
/// only carry PTS (`flags = 0b10`) on a video PID. With `max_b_frames =
/// 0` (the current default for the in-process encoder pipeline) DTS ==
/// PTS; once B-frame encoding is wired in, the caller should pass the
/// encoder's `EncodedVideoFrame::dts` here scaled to 90 kHz.
#[cfg(feature = "media-codecs")]
fn build_video_pes(video_data: &[u8], pts: u64) -> Vec<u8> {
    build_video_pes_with_dts(video_data, pts, pts)
}

/// Same as [`build_video_pes`] but lets the caller pass an explicit DTS
/// distinct from the PTS. Kept separate so the no-B-frame default path
/// stays a one-argument call.
#[cfg(feature = "media-codecs")]
fn build_video_pes_with_dts(video_data: &[u8], pts: u64, dts: u64) -> Vec<u8> {
    let mut pes = Vec::with_capacity(19 + video_data.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01]);
    pes.push(0xE0); // video stream_id
    pes.extend_from_slice(&[0, 0]); // unbounded length
    pes.push(0x80); // marker bits
    pes.push(0xC0); // PTS + DTS both present (PTS_DTS_flags = 0b11)
    pes.push(10);   // PES header data length: 5 bytes PTS + 5 bytes DTS

    let pts = pts & 0x1_FFFF_FFFF;
    write_pes_timestamp(&mut pes, 0x30, pts); // 0011 marker (PTS w/ DTS)
    let dts = dts & 0x1_FFFF_FFFF;
    write_pes_timestamp(&mut pes, 0x10, dts); // 0001 marker (DTS)

    pes.extend_from_slice(video_data);
    pes
}

/// Pack a PES into 188-byte TS packets on `pid`, advancing `cc` per packet.
/// No packet carries a PCR: the replacer forwards the input's PCRs at their
/// own stream positions as adaptation-field-only packets, and the chain's
/// trailing `ts_pcr_remux` stage re-stamps them (the old per-frame PCR,
/// derived from the PTS it described, ran ~15 500 ppm fast and could only
/// appear once per frame).
fn packetize_ts(pid: u16, pes: &[u8], cc: &mut u8) -> Vec<[u8; 188]> {
    let mut packets = Vec::new();
    let mut offset = 0;
    let mut is_first = true;

    while offset < pes.len() {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        let pusi: u8 = if is_first { 1 } else { 0 };
        let current_cc = *cc;
        *cc = (*cc + 1) & 0x0F;

        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = (pusi << 6) | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = pid as u8;

        let remaining = pes.len() - offset;
        let payload_capacity = TS_PACKET_SIZE - 4;
        if remaining >= payload_capacity {
            pkt[3] = 0x10 | current_cc;
            pkt[4..TS_PACKET_SIZE].copy_from_slice(&pes[offset..offset + payload_capacity]);
            offset += payload_capacity;
        } else {
            // Short tail: an adaptation field of stuffing pads the payload
            // to the end of the packet.
            let stuff_len = payload_capacity - remaining;
            pkt[3] = 0x30 | current_cc;
            pkt[4] = (stuff_len - 1) as u8;
            if stuff_len > 1 {
                pkt[5] = 0x00;
                for b in &mut pkt[6..4 + stuff_len] {
                    *b = 0xFF;
                }
            }
            pkt[4 + stuff_len..4 + stuff_len + remaining].copy_from_slice(&pes[offset..]);
            offset += remaining;
        }
        is_first = false;
        packets.push(pkt);
    }
    packets
}

// ─────────────────────────── tests ───────────────────────────

#[cfg(all(test, feature = "media-codecs"))]
mod tests {
    use super::*;
    use crate::engine::ts_parse::{mpeg2_crc32, ts_cc};
    use crate::engine::ts_pmt_edit::{is_pmt_for, parse_pmt};

    /// Packet-level wrapper over `select_video_es` for the synth helpers.
    fn parse_pmt_video(pkt: &[u8], pinned: Option<u16>) -> Option<(u16, u8)> {
        let s = crate::engine::ts_parse::find_section_in_packet(pkt, 0x02, None)?;
        let view = parse_pmt(&pkt[s.start..s.end()])?;
        select_video_es(&view, pinned).chosen
    }

    /// The program's PMT section in an output buffer.
    fn pmt_in(out: &[u8], program: u16) -> Vec<u8> {
        let mut asm = crate::engine::ts_parse::SectionAssembler::new();
        let mut found = None;
        for p in out.chunks(TS_PACKET_SIZE) {
            for sec in asm.feed(ts_pusi(p), &p[ts_payload_offset(p)..]) {
                if is_pmt_for(sec, program) {
                    found = Some(sec.to_vec());
                }
            }
        }
        found.expect("PMT in output")
    }

    fn cfg(codec: &str) -> VideoEncodeConfig {
        VideoEncodeConfig {
            codec: codec.into(),
            width: None,
            height: None,
            fps_num: None,
            fps_den: None,
            bitrate_kbps: None,
            gop_size: None,
            preset: None,
            profile: None,
            chroma: None,
            bit_depth: None,
            rate_control: None,
            crf: None,
            max_bitrate_kbps: None,
            bframes: None,
            refs: None,
            level: None,
            tune: None,
            color_primaries: None,
            color_transfer: None,
            color_matrix: None,
            color_range: None,
            hw_decode: None,
             source_video_pid: None,
            scan: None,
        }
    }

    #[test]
    fn rejects_unknown_codec() {
        assert!(TsVideoReplacer::new(&cfg("vp9"), None).is_err());
    }

    #[test]
    fn accepts_x264_and_x265_and_nvenc() {
        assert!(TsVideoReplacer::new(&cfg("x264"), None).is_ok());
        assert!(TsVideoReplacer::new(&cfg("x265"), None).is_ok());
        assert!(TsVideoReplacer::new(&cfg("h264_nvenc"), None).is_ok());
        assert!(TsVideoReplacer::new(&cfg("hevc_nvenc"), None).is_ok());
    }

    #[test]
    fn process_empty_input_is_noop() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        r.process(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn process_misaligned_input_is_passthrough() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        let input = vec![0u8; 100];
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn process_unknown_pid_passes_through_verbatim() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x1F;
        pkt[2] = 0xFF;
        pkt[3] = 0x10;

        let mut out = Vec::new();
        r.process(&pkt, &mut out);
        assert_eq!(&out[..], &pkt[..]);
    }

    /// Build a single-PAT-section TS packet pointing at one program
    /// whose PMT lives at `pmt_pid`.
    fn synth_pat(pmt_pid: u16) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, pid high bits = 0 (PAT_PID = 0)
        pkt[2] = 0x00;
        pkt[3] = 0x10; // payload only, CC=0
        pkt[4] = 0x00; // pointer field
        let s = 5;
        pkt[s] = 0x00; // table_id = PAT
        // section_length counts transport_stream_id(2) + version/cur(1) +
        // section#(1) + last#(1) + one program entry(4) + CRC(4) = 13.
        let section_length: u16 = 13;
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00; // transport_stream_id hi
        pkt[s + 4] = 0x01; // transport_stream_id lo
        pkt[s + 5] = 0xC1; // reserved + version=0 + current=1
        pkt[s + 6] = 0x00; // section#
        pkt[s + 7] = 0x00; // last_section#
        // one program entry: program_number=1, pmt_pid
        pkt[s + 8] = 0x00;
        pkt[s + 9] = 0x01;
        pkt[s + 10] = 0xE0 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[s + 11] = pmt_pid as u8;
        let crc = mpeg2_crc32(&pkt[s..s + 12]);
        pkt[s + 12] = (crc >> 24) as u8;
        pkt[s + 13] = (crc >> 16) as u8;
        pkt[s + 14] = (crc >> 8) as u8;
        pkt[s + 15] = crc as u8;
        pkt
    }

    /// Build a minimal PMT TS packet with exactly one video ES entry.
    fn synth_pmt(pmt_pid: u16, video_pid: u16, stream_type: u8) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F); // PUSI=1
        pkt[2] = pmt_pid as u8;
        pkt[3] = 0x10; // payload only, CC=0
        pkt[4] = 0x00; // pointer field
        let s = 5;
        pkt[s] = 0x02; // table_id = PMT
        // program_number(2) + vsn/cur(1) + sec#(1) + last#(1) + PCR_PID(2) + prog_info_len(2)
        //   + ES: stream_type(1) + es_pid(2) + es_info_len(2) + CRC(4) = 18
        let section_length: u16 = 18;
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00; // program_number hi
        pkt[s + 4] = 0x01; // program_number lo
        pkt[s + 5] = 0xC1; // reserved + version + current
        pkt[s + 6] = 0x00; // section#
        pkt[s + 7] = 0x00; // last_section#
        pkt[s + 8] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F); // PCR_PID hi
        pkt[s + 9] = video_pid as u8; // PCR_PID lo
        pkt[s + 10] = 0xF0; // program_info_length hi (0)
        pkt[s + 11] = 0x00;
        pkt[s + 12] = stream_type;
        pkt[s + 13] = 0xE0 | ((video_pid >> 8) as u8 & 0x1F);
        pkt[s + 14] = video_pid as u8;
        pkt[s + 15] = 0xF0; // es_info_length hi (0)
        pkt[s + 16] = 0x00;
        let crc = mpeg2_crc32(&pkt[s..s + 17]);
        pkt[s + 17] = (crc >> 24) as u8;
        pkt[s + 18] = (crc >> 16) as u8;
        pkt[s + 19] = (crc >> 8) as u8;
        pkt[s + 20] = crc as u8;
        pkt
    }

    /// Confirms that our PMT synthesizer produces a packet
    /// `parse_pmt_video` agrees with, so the codec-change test below
    /// isn't observing parser failure instead of real behaviour.
    #[test]
    fn synth_pmt_round_trips_through_parser() {
        let pkt = synth_pmt(0x1000, 0x0100, 0x1B);
        assert_eq!(parse_pmt_video(&pkt, None), Some((0x0100, 0x1B)));
        let pkt2 = synth_pmt(0x1000, 0x0100, 0x24);
        assert_eq!(parse_pmt_video(&pkt2, None), Some((0x0100, 0x24)));
        let pkt3 = synth_pmt(0x1000, 0x0100, 0x02);
        assert_eq!(parse_pmt_video(&pkt3, None), Some((0x0100, 0x02)));
        let pkt4 = synth_pmt(0x1000, 0x0100, 0x01);
        assert_eq!(parse_pmt_video(&pkt4, None), Some((0x0100, 0x01)));
    }

    /// Confirms that our PAT synthesizer produces a packet
    /// `parse_pat_programs` agrees with.
    #[test]
    fn synth_pat_round_trips_through_parser() {
        let pkt = synth_pat(0x1000);
        assert_eq!(parse_pat_programs(&pkt), vec![(1u16, 0x1000u16)]);
    }

    /// Seamless input switching between inputs with different video
    /// codecs (H.264 → HEVC) must force the replacer's next encoded
    /// frame to be an IDR. Before this fix the replacer ignored the
    /// new PMT, kept its H.264 decoder, and silently dropped every
    /// post-switch frame.
    #[test]
    fn codec_change_on_pmt_update_raises_force_idr() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let force_idr = r.force_idr_handle();

        // Initial program: H.264 (stream_type 0x1B).
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt(0x1000, 0x0100, 0x1B), &mut out);
        assert!(
            !force_idr.load(Ordering::Relaxed),
            "first PMT must not trigger a forced IDR"
        );

        // Input switch: same PMT PID and video PID, but the new input
        // is HEVC (stream_type 0x24). This is exactly the scenario in
        // the user report.
        r.process(&synth_pmt(0x1000, 0x0100, 0x24), &mut out);
        assert!(
            force_idr.load(Ordering::Relaxed),
            "codec change must force an IDR on the next encoded frame"
        );
    }

    /// A PAT that moves the PMT PID (different program layout on the
    /// new input) must also trigger the reset path, so we re-learn
    /// everything downstream.
    #[test]
    fn pmt_pid_change_on_pat_update_raises_force_idr() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let force_idr = r.force_idr_handle();

        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt(0x1000, 0x0100, 0x1B), &mut out);
        assert!(!force_idr.load(Ordering::Relaxed));

        // New input exposes the PMT at a different PID.
        r.process(&synth_pat(0x1001), &mut out);
        assert!(
            force_idr.load(Ordering::Relaxed),
            "PMT PID change must force an IDR on the next encoded frame"
        );
    }

    /// Same codec, same PID → no reset. Guards against a regression
    /// where every PMT packet (many per second) would flip force_idr
    /// and turn every frame into an IDR.
    #[test]
    fn repeated_unchanged_pmt_does_not_raise_force_idr() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let force_idr = r.force_idr_handle();

        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt(0x1000, 0x0100, 0x1B), &mut out);
        r.process(&synth_pmt(0x1000, 0x0100, 0x1B), &mut out);
        r.process(&synth_pmt(0x1000, 0x0100, 0x1B), &mut out);
        assert!(
            !force_idr.load(Ordering::Relaxed),
            "unchanged PMT must not trigger IDR requests"
        );
    }

    #[test]
    fn build_video_pes_carries_pts_and_dts() {
        // Every emitted PES must carry both PTS and DTS so strict
        // hardware decoders (Appear / Tektronix) accept the stream.
        let pes = build_video_pes(&[0, 0, 0, 1], 0xABCD_EF12);
        assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01]);
        assert_eq!(pes[3], 0xE0); // video stream_id
        assert_eq!(pes[7], 0xC0); // PTS_DTS_flags = 0b11 (PTS + DTS)
        assert_eq!(pes[8], 10);   // 5 bytes PTS + 5 bytes DTS
        // PTS marker top nibble = 0011 (PTS w/ DTS following).
        assert_eq!(pes[9] & 0xF0, 0x30);
        // DTS marker top nibble = 0001.
        assert_eq!(pes[14] & 0xF0, 0x10);
        // Trailing ES bytes preserved verbatim.
        assert_eq!(&pes[pes.len() - 4..], &[0, 0, 0, 1]);
    }

    /// Round-trip the encoded PTS / DTS through the parser to catch any
    /// bit-shuffling regressions in the marker bits.
    #[test]
    fn build_video_pes_pts_and_dts_round_trip() {
        let pts: u64 = 0x0_1234_5678;
        let dts: u64 = 0x0_1111_2222;
        let pes = build_video_pes_with_dts(&[0xAA], pts, dts);
        // PES optional header: marker(1) + flags(1) + hdr_len(1) = 3,
        // then 5 bytes PTS at offset 9, 5 bytes DTS at offset 14.
        let parsed_pts = parse_pts(&pes[9..14]);
        let parsed_dts = parse_pts(&pes[14..19]);
        assert_eq!(parsed_pts, pts);
        assert_eq!(parsed_dts, dts);
    }

    /// PMT rewrite must force PCR_PID to the rebuilt video PID — even
    /// when the source's PMT pointed PCR_PID at a separate dedicated
    /// PCR PID. Without this the rebuilt stream emits PCR on the video
    /// PID while the PMT advertises it elsewhere, which professional
    /// decoders flag as a TR 101 290 P1.6 violation and refuse to lock.
    #[test]
    fn pmt_rewrite_forces_pcr_pid_to_video_pid() {
        // Synth a PMT whose PCR_PID is 0x1234 (some made-up dedicated
        // PCR PID), with one video ES at PID 0x0100, stream_type 0x1B.
        let mut pkt = synth_pmt(0x1000, 0x0100, 0x1B);
        // Override PCR_PID in the synth packet (section_start = 5,
        // PCR_PID = section_start + 8..=9).
        pkt[5 + 8] = 0xE0 | ((0x1234u16 >> 8) as u8 & 0x1F);
        pkt[5 + 9] = 0x1234u16 as u8;
        // Recompute CRC after the manual edit.
        let section_length = (((pkt[5 + 1] & 0x0F) as usize) << 8) | (pkt[5 + 2] as usize);
        let crc_offset = 5 + 3 + section_length - 4;
        let new_crc = mpeg2_crc32(&pkt[5..crc_offset]);
        pkt[crc_offset..crc_offset + 4].copy_from_slice(&new_crc.to_be_bytes());
        // Sanity: confirm the synth setup before exercising the rewrite.
        let pcr_pid_before = ((pkt[5 + 8] as u16 & 0x1F) << 8) | pkt[5 + 9] as u16;
        assert_eq!(pcr_pid_before, 0x1234);

        // Run it through the replacer. Target stream_type matches source
        // (0x1B) so only the PCR_PID change drives the edit.
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        r.process(&pkt, &mut out);
        let sec = pmt_in(&out, 1);
        let v = parse_pmt(&sec).expect("valid PMT");
        assert_eq!(v.pcr_pid, 0x0100, "PCR_PID must be re-pointed at video PID");
        assert_eq!(mpeg2_crc32(&sec), 0, "CRC must validate after rewrite");
    }

    /// VH1.ts: the PMT sits behind a 0xC0 section. The replacer learns the
    /// MPEG-2 video PID, rewrites it to H.264 and drops the MPEG-2-only
    /// descriptors, keeps PCR_PID on the video PID, and leaves the 0xC0
    /// section and the packet count untouched.
    #[test]
    fn vh1_pmt_is_learned_and_rewritten_behind_the_private_section() {
        use crate::engine::ts_test_fixtures::{
            vh1_pat_packet, vh1_pmt_packet, VH1_PMT_OFFSET, VH1_PROGRAM,
        };
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        r.process(&vh1_pat_packet(), &mut out);
        out.clear();
        let pmt = vh1_pmt_packet();
        r.process(&pmt, &mut out);
        assert_eq!(r.stats_handle().source_pid.load(Ordering::Relaxed), 0x0E0F);
        assert_eq!(out.len(), TS_PACKET_SIZE);
        assert_eq!(&out[..VH1_PMT_OFFSET], &pmt[..VH1_PMT_OFFSET]);
        let sec = pmt_in(&out, VH1_PROGRAM);
        assert_eq!(mpeg2_crc32(&sec), 0);
        let v = parse_pmt(&sec).unwrap();
        assert_eq!(v.pcr_pid, 0x0E0F);
        assert_eq!(v.es[0].stream_type, 0x1B);
        // The input PCR is followed on the source PCR_PID (here the video).
        assert_eq!(r.inner.source_pcr_pid, Some(0x0E0F));
    }

    /// `video_encode` on an audio-only program: the engage watchdog says
    /// so (`no_supported_es`) on the output's event feed.
    #[test]
    fn an_audio_only_program_raises_video_no_supported_es() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let (tx, mut rx) = crate::manager::events::event_channel();
        r.set_decode_stall_watchdog(tx, "out-v");
        let pmt = crate::engine::ts_test_fixtures::pmt_section(1, 0, 0x101, &[], &[(0x0F, 0x101, &[])]);
        let mut out = Vec::new();
        for i in 0..12u8 {
            r.process(&synth_pat(0x1000), &mut out);
            r.process(&crate::engine::ts_test_fixtures::packetize_sections(0x1000, &[&pmt], i)[0], &mut out);
        }
        let later = std::time::Instant::now() + std::time::Duration::from_secs(6);
        match r.inner.poll_engage(later) {
            Some(crate::engine::transcode_engage::EngageEvent::NotFound { reason, .. }) => {
                assert_eq!(reason.as_str(), "no_supported_es")
            }
            other => panic!("{other:?}"),
        }
        let ev = rx.try_recv().expect("event emitted");
        assert_eq!(ev.output_id.as_deref(), Some("out-v"));
        assert_eq!(ev.details.unwrap()["error_code"], "video_transcode_source_not_found");
    }

    /// The video counterpart of the audio stage's rule: after an input
    /// switch to a source with no decodable video (VC-1 here), the
    /// passthrough PMT gets a version from the stage's own sequence instead
    /// of its source version — which equalled the version the previous
    /// input's rebuilt PMT carried.
    #[test]
    fn a_passthrough_pmt_after_a_rebuilt_one_gets_a_new_version() {
        use crate::engine::ts_test_fixtures::{packetize_sections, pmt_section};
        let mpeg2 = pmt_section(1, 1, 0x100, &[], &[(0x02, 0x100, &[]), (0x0F, 0x101, &[])]);
        let vc1 = pmt_section(1, 1, 0x100, &[], &[(0xEA, 0x100, &[]), (0x0F, 0x101, &[])]);
        let ver = |s: &[u8]| (s[5] >> 1) & 0x1F;
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        r.process(&packetize_sections(0x1000, &[&mpeg2], 0)[0], &mut out);
        let a = pmt_in(&out, 1);
        assert_eq!(parse_pmt(&a).unwrap().es[0].stream_type, 0x1B);
        r.external_reset_handle().store(true, Ordering::Relaxed);
        out.clear();
        r.process(&packetize_sections(0x1000, &[&vc1], 1)[0], &mut out);
        let b = pmt_in(&out, 1);
        assert_eq!(parse_pmt(&b).unwrap().es[0].stream_type, 0xEA, "content passed through");
        assert_eq!(mpeg2_crc32(&b), 0);
        assert_ne!(ver(&b), ver(&a));
    }

    #[test]
    fn video_version_follows_the_audio_stage() {
        // Audio stage then video stage, as in transcode_chain: an audio
        // codec change must change the FINAL output PMT version, which the
        // video stage used to overwrite with its own unchanged counter.
        let mut audio = crate::engine::ts_audio_replace::TsAudioReplacer::new(
            &serde_json::from_value(serde_json::json!({ "codec": "aac_lc" })).unwrap(),
            None,
        )
        .unwrap();
        let mut video = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let chain = |a: &mut crate::engine::ts_audio_replace::TsAudioReplacer,
                     v: &mut TsVideoReplacer,
                     input: &[u8]| {
            let mut mid = Vec::new();
            a.process(input, &mut mid);
            let mut out = Vec::new();
            v.process(&mid, &mut out);
            out
        };
        let pat = synth_pat(0x1000);
        chain(&mut audio, &mut video, &pat);
        let pmt = |apid: u16, cc: u8| {
            let sec = crate::engine::ts_test_fixtures::pmt_section(
                1,
                0,
                0x100,
                &[],
                &[(0x1B, 0x100, &[]), (0x0F, apid, &[])],
            );
            crate::engine::ts_test_fixtures::packetize_sections(0x1000, &[&sec], cc)[0]
        };
        let ver = |s: &[u8]| (s[5] >> 1) & 0x1F;
        let v1 = pmt_in(&chain(&mut audio, &mut video, &pmt(0x101, 0)), 1);
        let v1b = pmt_in(&chain(&mut audio, &mut video, &pmt(0x101, 1)), 1);
        assert_eq!(v1, v1b, "unchanged: no flap");
        // An audio-only source change (the audio PID moves): only the audio
        // stage resets, but the final PMT content changed, so the final
        // version must change. The video stage used to re-stamp its own
        // unchanged counter over the audio stage's bump.
        let v2 = pmt_in(&chain(&mut audio, &mut video, &pmt(0x102, 2)), 1);
        assert_eq!(parse_pmt(&v2).unwrap().es[1].pid, 0x102);
        assert_ne!(ver(&v2), ver(&v1), "final output version changed");
        let v2b = pmt_in(&chain(&mut audio, &mut video, &pmt(0x102, 3)), 1);
        assert_eq!(ver(&v2b), ver(&v2), "and holds afterwards");
    }

    // ── PCR carry (defect 3 / x), pre-PMT gate (4), admission (7c) ──

    const MS27: u64 = 27_000;

    /// A video-PID packet carrying a PCR in its adaptation field AND
    /// payload (AFC = 11) — how Sky carries 3 220 of its 3 362 PCRs.
    fn pcr_payload_packet(pid: u16, cc: u8, pcr: u64, pusi: bool) -> [u8; 188] {
        let mut p = crate::engine::ts_parse::pcr_only_packet(pid, cc, pcr, false);
        p[3] = 0x30 | (cc & 0x0F);
        p[4] = 7;
        if pusi {
            p[1] |= 0x40;
            p[12..16].copy_from_slice(&[0, 0, 1, 0xE0]);
            p[16] = 0;
            p[17] = 0;
            p[18] = 0x80;
            p[19] = 0x80;
            p[20] = 5;
            p[21..26].copy_from_slice(&[0x21, 0, 1, 0, 1]);
        } else {
            for b in &mut p[12..] {
                *b = 0x55;
            }
        }
        p
    }

    fn pkts(out: &[u8]) -> Vec<[u8; 188]> {
        out.chunks(188)
            .map(|c| {
                let mut p = [0u8; 188];
                p.copy_from_slice(c);
                p
            })
            .collect()
    }

    fn two_es_pmt(pcr_pid: u16) -> [u8; 188] {
        let sec = crate::engine::ts_test_fixtures::pmt_section(
            1,
            0,
            pcr_pid,
            &[],
            &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])],
        );
        crate::engine::ts_test_fixtures::packetize_sections(0x1000, &[&sec], 0)[0]
    }

    #[test]
    fn packetize_ts_carries_no_pcr_and_advances_cc() {
        let pes = build_video_pes(&[0xAB; 400], 90_000);
        let mut cc = 14u8;
        let out = packetize_ts(0x100, &pes, &mut cc);
        assert_eq!(out.len(), 3);
        assert_eq!(cc, 1, "three payload packets: 14, 15, 0");
        assert!(ts_pusi(&out[0]) && !ts_pusi(&out[1]));
        for (i, p) in out.iter().enumerate() {
            assert_eq!(crate::engine::ts_parse::extract_pcr(p), None);
            assert_eq!(ts_cc(p), (14 + i as u8) & 0x0F);
        }
        // The short tail is padded with adaptation-field stuffing, payload
        // ending at byte 187.
        assert_eq!(out[2][3] & 0x30, 0x30);
        assert_eq!(out[2][187], 0xAB);
    }

    #[test]
    fn every_input_pcr_leaves_as_an_af_only_packet_on_the_video_pid() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&two_es_pmt(0x100), &mut out);
        out.clear();
        // PCR inside a PUSI payload packet, then in a continuation packet.
        let t0 = 5_000 * 27_000_000u64;
        let mut input = pcr_payload_packet(0x100, 3, t0, true).to_vec();
        input.extend_from_slice(&pcr_payload_packet(0x100, 4, t0 + 30 * MS27, false));
        let mut di = pcr_payload_packet(0x100, 5, t0 + 5_000 * MS27, false);
        crate::engine::ts_parse::set_discontinuity_indicator(&mut di);
        input.extend_from_slice(&di);
        r.process(&input, &mut out);
        let got: Vec<[u8; 188]> = pkts(&out).into_iter().filter(|p| ts_pid(p) == 0x100).collect();
        assert_eq!(got.len(), 3, "one AF-only packet per input PCR, no source payload");
        let pcrs: Vec<u64> = got
            .iter()
            .map(|p| crate::engine::ts_parse::extract_pcr(p).unwrap())
            .collect();
        assert_eq!(pcrs, vec![t0, t0 + 30 * MS27, t0 + 5_000 * MS27], "values unchanged");
        for p in &got {
            assert_eq!(crate::engine::ts_parse::ts_adaptation_field_control(p), 0b10);
            assert!(!ts_pusi(p));
            // No payload has been emitted on the PID yet: CC 15, so the
            // replacer's first payload packet (CC 0) follows.
            assert_eq!(ts_cc(p), 15);
        }
        assert!(!crate::engine::ts_parse::ts_discontinuity_indicator(&got[1]));
        assert!(crate::engine::ts_parse::ts_discontinuity_indicator(&got[2]), "DI copied");
    }

    #[test]
    fn a_dedicated_pcr_pid_is_carried_onto_the_video_pid() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        r.process(&two_es_pmt(0x1FF), &mut out);
        let sec = pmt_in(&out, 1);
        assert_eq!(parse_pmt(&sec).unwrap().pcr_pid, 0x100, "PCR_PID re-pointed at the video");
        out.clear();
        let src = crate::engine::ts_parse::pcr_only_packet(0x1FF, 9, 27_000_000, false);
        r.process(&src, &mut out);
        let got = pkts(&out);
        assert_eq!(got.len(), 2);
        assert_eq!(ts_pid(&got[0]), 0x100);
        assert_eq!(crate::engine::ts_parse::extract_pcr(&got[0]), Some(27_000_000));
        assert_eq!(got[1], src, "the source PCR packet passes through untouched");
    }

    #[test]
    fn nothing_but_psi_passes_before_the_pmt() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        let mut sdt = crate::engine::ts_test_fixtures::payload_packet(0x11, 0);
        sdt[1] |= 0x40;
        let before = [
            pcr_payload_packet(0x100, 0, 27_000_000, true),
            crate::engine::ts_test_fixtures::pes_start_packet(0x101, 0, 0xC0, 90_000, None),
            synth_pat(0x1000),
            sdt,
            crate::engine::ts_test_fixtures::payload_packet(0x100, 1),
        ];
        for p in &before {
            r.process(p, &mut out);
        }
        let got = pkts(&out);
        assert_eq!(got, vec![synth_pat(0x1000), sdt], "only PAT and SI");
        assert_eq!(r.stats_handle().pre_pmt_dropped_packets.load(Ordering::Relaxed), 3);
        // Once the PMT parsed, audio passes through.
        out.clear();
        r.process(&two_es_pmt(0x100), &mut out);
        out.clear();
        let audio = crate::engine::ts_test_fixtures::pes_start_packet(0x101, 1, 0xC0, 93_600, None);
        r.process(&audio, &mut out);
        assert_eq!(out, audio.to_vec());
    }

    #[test]
    fn no_pmt_in_five_seconds_falls_back_to_passthrough() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let t0 = std::time::Instant::now();
        let mut out = Vec::new();
        let audio = crate::engine::ts_test_fixtures::pes_start_packet(0x101, 0, 0xC0, 90_000, None);
        r.inner.process_at(&audio, &mut out, t0);
        assert!(out.is_empty());
        r.inner
            .process_at(&audio, &mut out, t0 + std::time::Duration::from_millis(4_900));
        assert!(out.is_empty());
        r.inner.process_at(&audio, &mut out, t0 + std::time::Duration::from_secs(5));
        assert_eq!(out, audio.to_vec(), "the gate gave up: today's passthrough");
    }

    #[test]
    fn a_takeover_continues_the_passthrough_cc() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let t0 = std::time::Instant::now();
        let late = t0 + std::time::Duration::from_secs(6);
        let mut out = Vec::new();
        r.inner.process_at(&synth_pat(0x1000), &mut out, t0);
        // No PMT for 6 s: the video passes through, CC 0..=6.
        for cc in 0..7u8 {
            let p = crate::engine::ts_test_fixtures::payload_packet(0x100, cc);
            r.inner.process_at(&p, &mut out, late);
        }
        r.inner.process_at(&two_es_pmt(0x100), &mut out, late);
        out.clear();
        r.inner
            .process_at(&pcr_payload_packet(0x100, 7, 27_000_000, false), &mut out, late);
        let got = pkts(&out);
        assert_eq!(got.len(), 1);
        assert_eq!(ts_cc(&got[0]), 6, "AF-only repeats the last CC on the wire");
        assert_eq!(r.inner.out_video_cc, 7, "the first re-encoded payload continues at 7");
        // The replacer then carries the PID for a while (CC 7..=11 out), an
        // input switch moves the PMT PID and the new PMT keeps video on
        // 0x100: nothing passed through since the takeover, so the CC
        // carries on from the replacer's own — not back to 7.
        r.inner.out_video_cc = 12;
        r.inner.process_at(&synth_pat(0x1001), &mut out, late);
        let sec = crate::engine::ts_test_fixtures::pmt_section(
            1,
            1,
            0x100,
            &[],
            &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])],
        );
        let pmt = crate::engine::ts_test_fixtures::packetize_sections(0x1001, &[&sec], 0)[0];
        r.inner.process_at(&pmt, &mut out, late);
        assert_eq!(r.inner.video_pid, Some(0x100));
        assert_eq!(r.inner.out_video_cc, 12, "no rewind to a stale passthrough CC");
    }

    #[test]
    fn admission_drops_the_r3_splice_leading_pictures() {
        use super::inner::admit_pts;
        let x = 7_052_804_052u64 & ((1 << 33) - 1);
        let seq = [x, x + 11_572, x + 22_372, x - 2_828, x + 25_972, x + 29_572, x + 33_172];
        let mut last = None;
        let mut out = Vec::new();
        for p in seq {
            if let Some(a) = admit_pts(last, Some(p), 3_600, 0) {
                out.push(a);
                last = Some(a);
            }
        }
        assert_eq!(out, vec![x, x + 11_572, x + 22_372, x + 25_972, x + 29_572, x + 33_172]);
        // A repeated PTS is dropped too; a 2 s step back is a new epoch.
        assert_eq!(admit_pts(Some(x), Some(x), 3_600, 0), None);
        assert_eq!(admit_pts(Some(x), Some(x - 180_000), 3_600, 0), Some(x - 180_000));
        assert_eq!(admit_pts(Some(x), Some(x - 90_000), 3_600, 0), None);
        // Across the 33-bit wrap, forward is forward.
        let top = (1u64 << 33) - 1_800;
        assert_eq!(admit_pts(Some(top), Some(1_800), 3_600, 0), Some(1_800));
        // A frame without PTS advances by the frame interval.
        assert_eq!(admit_pts(Some(x), None, 3_600, 0), Some(x + 3_600));
        assert_eq!(admit_pts(None, None, 3_600, 42), Some(42));
    }

    #[test]
    fn a_dropped_frame_leaves_the_idr_request_and_the_queue_alone() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let idr = r.force_idr_handle();
        idr.store(true, Ordering::Relaxed);
        assert_eq!(r.inner.admit_decoded(Some(90_000)), Some(90_000));
        assert_eq!(r.inner.admit_decoded(Some(93_600)), Some(93_600));
        assert_eq!(r.inner.admit_decoded(Some(86_400)), None);
        assert!(idr.load(Ordering::Relaxed), "the IDR request waits for an admitted frame");
        assert!(r.inner.src_pts_queue.is_empty());
        let st = r.stats_handle();
        assert_eq!(st.non_monotonic_frames_dropped.load(Ordering::Relaxed), 1);
        assert_eq!(st.dropped_frames.load(Ordering::Relaxed), 1);
        // A PTS-less frame steps by the measured frame interval (3600, not
        // the per-field DTS delta).
        assert_eq!(r.inner.admit_decoded(None), Some(97_200));
    }

    /// A 29.97 fps source whose PES carry a PTS on every 12th picture only,
    /// no fps pinned: the interval is learned from the real timestamps
    /// (36 036 ticks over 12 frames), never from the derived ones, so after
    /// the first GOPs every real timestamp is admitted as it is and the
    /// output keeps the source's rate. A single 0.5 s gap between two real
    /// timestamps is not a frame interval either.
    #[test]
    fn a_pts_less_frame_steps_by_the_interval_real_timestamps_show() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let p0 = 900_000u64;
        let mut admitted = Vec::new();
        for n in 0..120u64 {
            let pts = (n % 12 == 0).then_some((p0 + n * 3_003) as i64);
            admitted.push((n, pts, r.inner.admit_decoded(pts)));
        }
        for (n, pts, a) in &admitted {
            if *n >= 36
                && let Some(p) = pts
            {
                assert_eq!(*a, Some(*p as u64), "real PTS of frame {n} admitted as is");
            }
        }
        assert_eq!(admitted.last().unwrap().2, Some(p0 + 119 * 3_003), "no drift");
        // Frame-rate steps teach it; a 0.5 s hole does not.
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        for p in [90_000i64, 93_600, 97_200, 97_200 + 45_000] {
            r.inner.admit_decoded(Some(p));
        }
        assert_eq!(r.inner.admit_decoded(None), Some(97_200 + 45_000 + 3_600));
    }

    /// A PES without a PTS (PTS_DTS_flags 0b00) must not reach the decoder
    /// as a real timestamp of 0: the frame would carry PTS 0, which both the
    /// rate meter and frame admission read as a real timestamp.
    #[test]
    fn a_pes_without_pts_has_no_pts() {
        let pes = [0x00, 0x00, 0x01, 0xE0, 0, 0, 0x80, 0x00, 0x00, 0, 0, 0, 1, 0x09, 0xF0];
        let (es, pts, dts) = extract_pes_video(&pes).unwrap();
        assert_eq!((pts, dts), (None, None));
        assert_eq!(es, &pes[9..]);
        let with = build_video_pes(&[0, 0, 0, 1, 0x09, 0xF0], 123_456);
        assert_eq!(extract_pes_video(&with).unwrap().1, Some(123_456));
    }

    /// The fallback when frames carry no usable PTS: the PES DTS step is a
    /// field on a field-coded source, so it is multiplied by the rounded
    /// PES-per-frame ratio; with no DTS either, 30 fps.
    #[test]
    fn the_fallback_rate_turns_a_field_step_into_a_frame_rate() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        assert_eq!(r.inner.fallback_rate(), (30, 1));
        r.inner.pes_dts_step_90k = Some(1_800);
        r.inner.pes_since_reset = 121;
        r.inner.frames_since_reset = 60;
        assert_eq!(r.inner.fallback_rate(), (25, 1));
        r.inner.pes_since_reset = 62;
        assert_eq!(r.inner.fallback_rate(), (50, 1), "one PES per frame: 50p");
        r.inner.pes_dts_step_90k = Some(3_003);
        assert_eq!(r.inner.fallback_rate(), (30_000, 1001));
    }

    /// An explicit `scan: interlaced` that has to code progressive says so
    /// as a Warning on the replacer's scope.
    #[test]
    fn interlace_unavailable_is_a_warning_event() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let (tx, mut rx) = crate::manager::events::event_channel();
        r.set_decode_stall_watchdog(tx, "out-i");
        r.inner.emit_interlace_unavailable("no backend could");
        let ev = rx.try_recv().expect("event");
        assert_eq!(ev.output_id.as_deref(), Some("out-i"));
        assert_eq!(ev.category, crate::manager::events::category::VIDEO_ENCODE);
        let d = ev.details.unwrap();
        assert_eq!(d["error_code"], "video_encode_interlace_unavailable");
        assert_eq!(d["reason"], "no backend could");
    }

    /// An H.264 source whose SPS never shows: after `SPS_WAIT_PES` PES the
    /// decoder opens anyway, seeded from the AU in hand — an undeclared
    /// reorder depth, so 1 (the depth that keeps a non-IDR join clean).
    #[test]
    fn without_an_sps_the_decoder_opens_seeded_after_the_wait() {
        let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt(0x1000, 0x100, 0x1B), &mut out);
        let mut cc = 0u8;
        let mut feed = |r: &mut TsVideoReplacer, n: u64| {
            for i in 0..n {
                let pes = build_video_pes(&[0, 0, 0, 1, 0x09, 0xF0], 90_000 + i * 3_600);
                for p in packetize_ts(0x100, &pes, &mut cc) {
                    r.process(&p, &mut out);
                }
            }
        };
        feed(&mut r, 300);
        assert!(r.inner.decoder.is_none(), "still waiting for an SPS");
        feed(&mut r, 3);
        let dec = r.inner.decoder.as_ref().expect("opened after the wait");
        assert_eq!(dec.reorder_depth(), 1);
    }

    // ── Encoder rate from decoded frames (5a) and friends, through a real
    //    decoder and libx264 ──

    #[cfg(feature = "video-encoder-x264")]
    mod x264 {
        use super::*;
        use video_codec::{VideoEncoderCodec, VideoEncoderConfig, VideoFieldOrder, VideoPreset};

        /// `n` libx264 access units (Annex B, SPS/PPS in-band on every IDR)
        /// of a moving pattern at 25 fps.
        pub(super) fn x264_aus(
            n: usize,
            (w, h): (u32, u32),
            field_order: Option<VideoFieldOrder>,
            sar: Option<(u32, u32)>,
        ) -> Vec<Vec<u8>> {
            let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
                codec: VideoEncoderCodec::X264,
                width: w,
                height: h,
                fps_num: 25,
                fps_den: 1,
                bitrate_kbps: 1_500,
                gop_size: 25,
                preset: VideoPreset::Veryfast,
                global_header: false,
                field_order,
                sample_aspect_ratio: sar,
                ..VideoEncoderConfig::default()
            })
            .expect("libx264 opens");
            let (wu, hu) = (w as usize, h as usize);
            let mut aus = Vec::new();
            for i in 0..n {
                // Lines of the two fields differ, and move, so a woven
                // frame is not accidentally progressive content.
                let y: Vec<u8> = (0..wu * hu)
                    .map(|k| {
                        let (x, row) = (k % wu, k / wu);
                        ((x + 3 * i + if row % 2 == 1 { 40 } else { 0 }) % 220) as u8 + 16
                    })
                    .collect();
                let c = vec![128u8; wu / 2 * hu / 2];
                for ef in enc.encode_frame(&y, wu, &c, wu / 2, &c, wu / 2, Some(i as i64)).unwrap() {
                    aus.push(ef.data);
                }
            }
            for ef in enc.flush().unwrap() {
                aus.push(ef.data);
            }
            aus
        }

        /// TS: PAT, PMT (H.264 on 0x100), then one PES per access unit at
        /// 25 fps. With `field_pes`, every picture is followed by a second
        /// PES 1800 ticks later carrying only an access-unit delimiter —
        /// the PES cadence of a PAFF source (one field per PES) while the
        /// decoder hands out one frame per pair.
        pub(super) fn ts_of(aus: &[Vec<u8>], base: u64, field_pes: bool, cc: &mut u8) -> Vec<u8> {
            let mut ts = Vec::new();
            ts.extend_from_slice(&synth_pat(0x1000));
            ts.extend_from_slice(&synth_pmt(0x1000, 0x100, 0x1B));
            for (i, au) in aus.iter().enumerate() {
                let pts = base + i as u64 * 3_600;
                for p in packetize_ts(0x100, &build_video_pes(au, pts), cc) {
                    ts.extend_from_slice(&p);
                }
                if field_pes {
                    let aud = [0u8, 0, 0, 1, 0x09, 0xF0];
                    for p in packetize_ts(0x100, &build_video_pes(&aud, pts + 1_800), cc) {
                        ts.extend_from_slice(&p);
                    }
                }
            }
            ts
        }

        /// The re-encoded video PES (ES bytes, PTS) on PID 0x100.
        pub(super) fn out_pes(out: &[u8]) -> Vec<(Vec<u8>, u64)> {
            let mut bufs: Vec<Vec<u8>> = Vec::new();
            for p in out.chunks(TS_PACKET_SIZE) {
                if ts_pid(p) != 0x100 || !ts_has_payload(p) {
                    continue;
                }
                if ts_pusi(p) {
                    bufs.push(Vec::new());
                }
                if let Some(b) = bufs.last_mut() {
                    b.extend_from_slice(&p[ts_payload_offset(p)..]);
                }
            }
            bufs.iter()
                .filter_map(|b| extract_pes_video(b))
                .map(|(es, pts, _)| (es, pts.expect("re-encoded PES carry a PTS")))
                .collect()
        }

        pub(super) fn run(r: &mut TsVideoReplacer, ts: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            for chunk in ts.chunks(TS_PACKET_SIZE * 7) {
                r.process(chunk, &mut out);
            }
            out
        }

        pub(super) fn first_sps(out: &[u8]) -> video_engine::H264SpsInfo {
            out_pes(out)
                .iter()
                .find_map(|(es, _)| video_engine::find_h264_sps(es))
                .expect("an SPS in the re-encoded video")
        }

        /// Defect 5a. A field-coded source sends one PES per field, 1800
        /// ticks apart, but the decoder hands the encoder one frame per
        /// pair: the encoder must open at 25 fps (VUI time_scale 50 with
        /// num_units_in_tick 1), not the 50 the PES DTS step says.
        #[test]
        fn a_field_per_pes_source_encodes_at_the_frame_rate() {
            let aus = x264_aus(20, (320, 240), None, None);
            let mut cc = 0u8;
            let mut ts = ts_of(&aus, 900_000, true, &mut cc);
            // A trailing PES start flushes the last one.
            ts.extend_from_slice(&packetize_ts(0x100, &build_video_pes(&[0, 0, 0, 1, 0x09, 0xF0], 0), &mut cc)[0]);
            let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
            let out = run(&mut r, &ts);
            let sps = first_sps(&out);
            assert_eq!(sps.timing.map(|(n, t, _)| (n, t)), Some((1, 50)), "25 fps, not 50");
            assert_eq!(r.inner.pipeline.fps(), (25, 1));
            // Every encoded frame carries its own source PTS, a frame apart.
            let pts: Vec<u64> = out_pes(&out).iter().map(|(_, p)| *p).collect();
            assert!(pts.len() >= 10, "{} frames encoded", pts.len());
            assert!(pts.windows(2).all(|w| w[1] - w[0] == 3_600), "{pts:?}");
        }

        /// Frames without any timestamp: nothing to measure, so the
        /// encoder opens at the fallback once 60 frames have gone by.
        #[test]
        fn a_source_without_timestamps_opens_at_the_fallback() {
            let aus = x264_aus(70, (320, 240), None, None);
            let mut cc = 0u8;
            let mut ts = Vec::new();
            ts.extend_from_slice(&synth_pat(0x1000));
            ts.extend_from_slice(&synth_pmt(0x1000, 0x100, 0x1B));
            for au in &aus {
                let mut pes = vec![0x00, 0x00, 0x01, 0xE0, 0, 0, 0x80, 0x00, 0x00];
                pes.extend_from_slice(au);
                for p in packetize_ts(0x100, &pes, &mut cc) {
                    ts.extend_from_slice(&p);
                }
            }
            let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
            let out = run(&mut r, &ts);
            assert_eq!(r.inner.pipeline.fps(), (30, 1));
            assert_eq!(first_sps(&out).timing.map(|(n, t, _)| (n, t)), Some((1, 60)));
        }

        /// The source's sample aspect ratio reaches the output VUI:
        /// 720x576 16:9 anamorphic SD (64:45) used to leave SAR-less and
        /// display at 5:4. Scaled, the display aspect ratio is kept.
        #[test]
        fn the_source_sample_aspect_ratio_survives() {
            let aus = x264_aus(10, (720, 576), None, Some((64, 45)));
            let mut cc = 0u8;
            let ts = ts_of(&aus, 900_000, false, &mut cc);
            let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
            let out = run(&mut r, &ts);
            let sps = first_sps(&out);
            assert_eq!((sps.width, sps.height), (720, 576));
            assert_eq!(sps.sample_aspect_ratio, Some((64, 45)));

            let mut scaled = cfg("x264");
            scaled.width = Some(1024);
            scaled.height = Some(576);
            let mut r = TsVideoReplacer::new(&scaled, None).unwrap();
            let out = run(&mut r, &ts_of(&aus, 900_000, false, &mut cc));
            let sps = first_sps(&out);
            assert_eq!((sps.width, sps.height), (1024, 576));
            assert_eq!(sps.sample_aspect_ratio, Some((1, 1)));

            // A square-pixel source that never said so stays unspecified.
            let aus = x264_aus(10, (320, 240), None, None);
            let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
            let out = run(&mut r, &ts_of(&aus, 900_000, false, &mut cc));
            assert_eq!(first_sps(&out).sample_aspect_ratio, None);
        }

        /// Decode the re-encoded video: per frame (interlaced, top field
        /// first, mean luma of even rows, mean luma of odd rows).
        pub(super) fn decode_out(out: &[u8]) -> Vec<(bool, bool, f64, f64)> {
            let mut dec = video_engine::VideoDecoder::open(video_codec::VideoCodec::H264).unwrap();
            let mut frames = Vec::new();
            let mut take = |dec: &mut video_engine::VideoDecoder| {
                while let Ok(f) = dec.receive_frame() {
                    let (y, ys, ..) = f.yuv_planes().expect("planar");
                    let (w, h) = (f.width() as usize, f.height() as usize);
                    let mean = |parity: usize| {
                        let rows: Vec<usize> = (0..h).filter(|r| r % 2 == parity).collect();
                        let sum: u64 = rows
                            .iter()
                            .map(|r| y[r * ys..r * ys + w].iter().map(|&b| b as u64).sum::<u64>())
                            .sum();
                        sum as f64 / (rows.len() * w) as f64
                    };
                    frames.push((f.is_interlaced(), f.top_field_first(), mean(0), mean(1)));
                }
            };
            for (es, pts) in out_pes(out) {
                dec.send_packet_with_pts(&es, pts as i64).unwrap();
                take(&mut dec);
            }
            dec.send_flush().unwrap();
            take(&mut dec);
            frames
        }

        /// The Sky Sports case, end to end: a field-coded 1080i-style
        /// source (one PES per field) with `scan` unset (auto), unscaled,
        /// comes out 25 fps, MBAFF, pic_struct signalled, in the source's
        /// field order — top first here, bottom first for a BFF source.
        #[test]
        fn auto_codes_an_interlaced_source_interlaced_in_its_field_order() {
            for order in [VideoFieldOrder::Tff, VideoFieldOrder::Bff] {
                let aus = x264_aus(16, (320, 240), Some(order), None);
                let mut cc = 0u8;
                let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
                let out = run(&mut r, &ts_of(&aus, 900_000, true, &mut cc));
                let sps = first_sps(&out);
                assert!(!sps.frame_mbs_only && sps.mb_adaptive_frame_field, "{order:?}: MBAFF");
                assert!(sps.pic_struct_present, "{order:?}: pic_struct");
                assert_eq!(sps.timing.map(|(n, t, _)| (n, t)), Some((1, 50)), "{order:?}: 25 fps");
                assert_eq!(r.inner.pipeline.field_order(), Some(order));
                let frames = decode_out(&out);
                assert!(frames.len() >= 8);
                for (interlaced, tff, ..) in frames {
                    assert!(interlaced, "{order:?}: decoded interlaced");
                    assert_eq!(tff, order.is_top_field_first(), "{order:?}: field order kept");
                }
            }
        }

        /// `progressive` keeps today's frame coding; `auto` does too when
        /// the output is scaled vertically, or the source is progressive.
        #[test]
        fn progressive_scaled_or_progressive_source_stays_frame_coded() {
            let interlaced = x264_aus(10, (320, 240), Some(VideoFieldOrder::Tff), None);
            let progressive = x264_aus(10, (320, 240), None, None);
            let mut forced = cfg("x264");
            forced.scan = Some(crate::config::models::VideoScan::Progressive);
            let mut scaled = cfg("x264");
            scaled.height = Some(160);
            for (c, aus) in [(forced, &interlaced), (scaled, &interlaced), (cfg("x264"), &progressive)] {
                let mut cc = 0u8;
                let mut r = TsVideoReplacer::new(&c, None).unwrap();
                let out = run(&mut r, &ts_of(aus, 900_000, false, &mut cc));
                assert!(first_sps(&out).frame_mbs_only, "{:?}", c.scan);
                assert_eq!(r.inner.pipeline.field_order(), None);
            }
        }

        /// A source whose top field is dark and bottom field bright, both
        /// flat: `scan: interlaced` scaled 240 -> 160 lines keeps the two
        /// fields apart (each scaled on its own, then woven), where
        /// scaling the woven frame would have averaged them to grey.
        #[test]
        fn interlaced_scaling_never_blends_the_fields() {
            let (w, h) = (320usize, 240usize);
            let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
                codec: VideoEncoderCodec::X264,
                width: w as u32,
                height: h as u32,
                fps_num: 25,
                fps_den: 1,
                bitrate_kbps: 4_000,
                gop_size: 25,
                preset: VideoPreset::Veryfast,
                global_header: false,
                field_order: Some(VideoFieldOrder::Tff),
                ..VideoEncoderConfig::default()
            })
            .unwrap();
            let y: Vec<u8> = (0..w * h).map(|k| if (k / w) % 2 == 0 { 60 } else { 180 }).collect();
            let c = vec![128u8; w / 2 * h / 2];
            let mut aus = Vec::new();
            for i in 0..10 {
                aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i)).unwrap().into_iter().map(|f| f.data));
            }
            aus.extend(enc.flush().unwrap().into_iter().map(|f| f.data));

            let mut scaled = cfg("x264");
            scaled.height = Some(160);
            scaled.scan = Some(crate::config::models::VideoScan::Interlaced);
            let mut cc = 0u8;
            let mut r = TsVideoReplacer::new(&scaled, None).unwrap();
            let out = run(&mut r, &ts_of(&aus, 900_000, false, &mut cc));
            let sps = first_sps(&out);
            assert_eq!((sps.width, sps.height), (320, 160));
            assert!(!sps.frame_mbs_only);
            let frames = decode_out(&out);
            assert!(frames.len() >= 5);
            for (interlaced, tff, even, odd) in frames {
                assert!(interlaced && tff);
                assert!((even - 60.0).abs() < 8.0 && (odd - 180.0).abs() < 8.0, "{even} / {odd}");
            }
        }

        /// Joined mid-GOP, the decoder opens on the IDR that carries the
        /// SPS, not the first PES: libx264 declares its reorder depth (0),
        /// which the seed then leaves to libavcodec — zero added latency.
        /// Opened on a P picture instead, it would have been seeded 1 and
        /// held a frame for good.
        #[test]
        fn the_decoder_opens_on_the_pes_that_carries_the_sps() {
            let aus = x264_aus(40, (320, 240), None, None);
            let has_sps = |au: &Vec<u8>| video_engine::find_h264_sps(au).is_some();
            let start = (1..aus.len()).find(|&i| !has_sps(&aus[i])).expect("a P picture");
            let skipped = aus[start..].iter().take_while(|au| !has_sps(au)).count();
            assert!(skipped >= 1 && start + skipped < aus.len() - 10);
            let mut cc = 0u8;
            let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
            let out = run(&mut r, &ts_of(&aus[start..], 900_000, false, &mut cc));
            assert_eq!(
                r.inner.pes_awaiting_sps as usize, skipped,
                "the P pictures before the next SPS are passed over"
            );
            let dec = r.inner.decoder.as_ref().expect("opened");
            assert_eq!(dec.reorder_depth(), 0);
            assert!(out_pes(&out).len() >= 10);
        }

        /// A frame-coded source keeps its rate: one PES per frame.
        #[test]
        fn a_frame_per_pes_source_is_unchanged() {
            let aus = x264_aus(12, (320, 240), None, None);
            let mut cc = 0u8;
            let ts = ts_of(&aus, 900_000, false, &mut cc);
            let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
            let out = run(&mut r, &ts);
            assert_eq!(first_sps(&out).timing.map(|(n, t, _)| (n, t)), Some((1, 50)));
        }

        /// After an input switch the encoder is already open at a fixed
        /// rate, so the new source's frames flow at once — nothing waits
        /// for a rate the encoder can no longer take.
        #[test]
        fn an_input_switch_with_the_encoder_open_drops_no_frames() {
            let aus = x264_aus(12, (320, 240), None, None);
            let mut cc = 0u8;
            let mut r = TsVideoReplacer::new(&cfg("x264"), None).unwrap();
            let _ = run(&mut r, &ts_of(&aus, 900_000, false, &mut cc));
            assert!(r.inner.pipeline.is_open());
            r.external_reset_handle().store(true, Ordering::Relaxed);
            let before = r.stats_handle().output_frames.load(Ordering::Relaxed);
            let decoded_before = r.decode_stats_handle().output_frames.load(Ordering::Relaxed);
            let _ = run(&mut r, &ts_of(&aus, 5_000_000, false, &mut cc));
            let encoded = r.stats_handle().output_frames.load(Ordering::Relaxed) - before;
            let decoded =
                r.decode_stats_handle().output_frames.load(Ordering::Relaxed) - decoded_before;
            assert!(decoded >= 8, "{decoded} decoded");
            assert!(r.inner.source_fps_locked);
            assert_eq!(encoded, decoded, "every decoded frame of the new source encoded");
        }
    }
}
