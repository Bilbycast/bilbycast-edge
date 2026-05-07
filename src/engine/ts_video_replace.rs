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
//! - PAT is observed to learn `pmt_pid`.
//! - PMT is observed to learn `video_pid` + source `stream_type`.
//! - PMT is rewritten in-place when the target codec family differs
//!   from the source (H.264 ↔ HEVC), with a recomputed CRC32.
//! - Video PID packets are buffered into PES, flushed on each PUSI,
//!   fed to the decoder, the resulting frames go through the encoder,
//!   and the encoded bitstream is repacketized as fresh TS.
//! - Every other PID (audio, PAT, null, etc.) is forwarded unchanged.
//!
//! # Scope (MVP)
//!
//! This first cut deliberately skips resolution scaling — if the
//! operator sets `video_encode.width` / `height`, the replacer logs and
//! uses the source resolution instead. Scaling will land in a follow-up
//! step that plumbs `VideoScaler` into the pipeline.
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
    mpeg2_crc32, parse_pat_programs, ts_has_adaptation, ts_has_payload, ts_payload_offset, ts_pid,
    ts_pusi, PAT_PID, TS_PACKET_SIZE, TS_SYNC_BYTE,
};

/// PCR pre-roll behind PTS in 27 MHz ticks (80 ms × 27 000 000 / 1000).
///
/// ISO/IEC 13818-1 Annex L (T-STD model) requires PCR to arrive earlier
/// than the corresponding frame's PTS by the decoder's transport-buffer
/// + CPB pre-roll. 80 ms matches FFmpeg's mpegts muxer default for VBR
/// contribution streams. Phase 4 of the sync-mux work moved actual PCR
/// generation to consult `engine::av_sync_mux::pcr_for_emit`, which
/// uses the master clock when a flow-wide pacer is attached and falls
/// back to `pts × 300 − preroll` otherwise. The constant is retained
/// for tests that exercise the legacy derivation directly.
#[allow(dead_code)]
const PCR_PREROLL_27MHZ: u64 = 2_160_000;

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
    /// The dependent `video-thumbnail` feature is disabled, which means
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
                "video-engine is not compiled in (enable the video-thumbnail feature)"
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
    #[cfg(feature = "video-thumbnail")]
    inner: inner::Inner,
    /// Human-readable description for logging ("x264 @ 4000 kbps").
    description: String,
    /// Shared atomic counters surfaced via [`Self::stats_handle`] so the
    /// per-output stats accumulator can register them at startup.
    stats: Arc<VideoEncodeStats>,
    /// One-shot IDR request for the next encoded frame. External callers
    /// (e.g. the input forwarder on a flow switch) flip this to `true`;
    /// the replacer consumes and clears it before the next encode call.
    /// The inner `Inner` holds a clone of this same `Arc`; this field is
    /// only kept so [`Self::force_idr_handle`] can hand it back out.
    #[allow(dead_code)]
    force_idr_on_next_frame: Arc<AtomicBool>,
}

impl TsVideoReplacer {
    /// Attach a per-flow A/V sync pacer. Output PCR is then derived from
    /// the pacer's master clock instead of `pts × 300 − preroll`. Safe
    /// to call zero or one time before `process()` runs; calling twice
    /// silently overwrites the existing pacer.
    pub fn set_av_sync_pacer(
        &mut self,
        pacer: Arc<crate::engine::av_sync_mux::AvSyncPacer>,
    ) {
        #[cfg(feature = "video-thumbnail")]
        {
            self.inner.av_sync_pacer = Some(pacer);
        }
        #[cfg(not(feature = "video-thumbnail"))]
        {
            let _ = pacer;
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
    #[cfg(feature = "video-thumbnail")]
    pub fn new(
        cfg: &VideoEncodeConfig,
        force_idr: Option<Arc<AtomicBool>>,
    ) -> Result<Self, TsVideoReplaceError> {
        let stats = Arc::new(VideoEncodeStats::default());
        let force_idr = force_idr.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let inner = inner::Inner::from_config(cfg, stats.clone(), force_idr.clone())?;
        let description = inner.target_description();
        Ok(Self {
            inner,
            description,
            stats,
            force_idr_on_next_frame: force_idr,
        })
    }

    /// Stub constructor when `video-thumbnail` is compiled out — returns
    /// `VideoEngineMissing` so callers fail cleanly at output startup.
    #[cfg(not(feature = "video-thumbnail"))]
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

    /// Feed one chunk of 188-byte-aligned TS into the replacer. Output
    /// TS is appended to `output`. Mis-aligned input is passed through
    /// verbatim (best-effort for boundary recovery in the caller).
    #[allow(unused_variables)]
    pub fn process(&mut self, input_ts: &[u8], output: &mut Vec<u8>) {
        #[cfg(feature = "video-thumbnail")]
        {
            self.inner.process(input_ts, output);
        }
        #[cfg(not(feature = "video-thumbnail"))]
        {
            output.extend_from_slice(input_ts);
        }
    }

    /// Drain buffered PES / encoder state. Call on graceful shutdown.
    #[allow(dead_code, unused_variables)]
    pub fn flush(&mut self, output: &mut Vec<u8>) {
        #[cfg(feature = "video-thumbnail")]
        {
            self.inner.flush(output);
        }
    }
}

// ─────────────────────────── Implementation ───────────────────────────

#[cfg(feature = "video-thumbnail")]
mod inner {
    use super::*;
    use crate::engine::video_encode_util::ScaledVideoEncoder;
    use video_codec::{VideoCodec, VideoEncoderCodec};
    use video_engine::VideoDecoder;

    pub struct Inner {
        #[allow(dead_code)]
        target_family: VideoCodec,
        fps_num: Option<u32>,
        fps_den: Option<u32>,

        pmt_pid: Option<u16>,
        video_pid: Option<u16>,
        source_stream_type: u8,
        target_stream_type: u8,

        pes_buffer: Vec<u8>,
        pes_started: bool,
        pending_pts: Option<u64>,

        decoder: Option<VideoDecoder>,
        /// Shared encoder pipeline — wraps `VideoEncoder` + optional
        /// `VideoScaler`. Lazy-opens on the first decoded frame and
        /// handles the `video_encode.width` / `.height` override by
        /// scaling the decoded frame to the target resolution instead
        /// of letting libavcodec silently crop.
        pipeline: ScaledVideoEncoder,

        out_video_cc: u8,
        /// PTS anchor in the encoder time base (1 / fps_num).
        out_frame_count: i64,
        /// 90 kHz PTS for the next emitted PES, anchored to the first
        /// source PES PTS. Used as a fallback when the source-PTS queue
        /// is empty (e.g. encoder catch-up bursts).
        pts_90k: u64,
        pts_anchored: bool,
        pts_step_90k: u64,
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
        src_pts_queue: std::collections::VecDeque<u64>,

        /// Last input PES DTS, used to derive the natural source
        /// inter-frame delta in decode order. DTS stays monotonic
        /// across the input PES stream even when the source has
        /// B-frames (PTS reorders, DTS does not), so the delta is
        /// the source frame rate.
        last_input_dts: Option<u64>,
        /// True once we've measured the source fps and pushed it into
        /// the encoder pipeline (a no-op once the encoder has opened).
        source_fps_locked: bool,
        /// PES frames consumed without yet locking the source rate.
        /// After a hard cap we accept the placeholder rate so a
        /// pathological source (no DTS, all-zero PTS) can't stall the
        /// encoder forever.
        unlocked_pes_count: u32,

        description: String,
        stats: Arc<VideoEncodeStats>,
        force_idr: Arc<AtomicBool>,

        /// Operator's hardware-decoder preference for the input decode
        /// side of this transcode. Defaults to `Auto` (VAAPI ≻ NVDEC ≻
        /// QSV ≻ CPU per host capabilities). Resolved on the first
        /// decoded frame so the static-capabilities snapshot is
        /// guaranteed installed.
        hw_decode_pref: crate::config::models::HwDecodePreference,

        /// Optional per-flow A/V sync pacer. When set, output PCR is
        /// `master.now_27mhz() − PCR_PREROLL_27MHZ` (modular-aware)
        /// instead of `pts × 300 − preroll`. PTS values still come
        /// from `src_pts_queue`, so A/V offset versus source is
        /// preserved. Phase 4 of the sync-mux work.
        pub av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    }

    impl Inner {
        pub fn from_config(
            cfg: &VideoEncodeConfig,
            stats: Arc<VideoEncodeStats>,
            force_idr: Arc<AtomicBool>,
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

            Ok(Self {
                target_family,
                fps_num: cfg.fps_num,
                fps_den: cfg.fps_den,
                pmt_pid: None,
                video_pid: None,
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
                pts_step_90k: 3000, // 30 fps default until we learn otherwise
                src_pts_queue: std::collections::VecDeque::with_capacity(64),
                last_input_dts: None,
                source_fps_locked: cfg.fps_num.is_some() && cfg.fps_den.is_some(),
                unlocked_pes_count: 0,
                description,
                stats,
                force_idr,
                hw_decode_pref: cfg.hw_decode.unwrap_or_default(),
                av_sync_pacer: None,
            })
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
            // Re-anchor PTS to the new input's first frame so downstream
            // A/V stays in sync with the audio replacer (which will also
            // re-anchor on the audio-PID codec swap).
            self.pts_anchored = false;
            self.src_pts_queue.clear();
            self.last_input_dts = None;
            self.source_fps_locked = self.fps_num.is_some() && self.fps_den.is_some();
            self.unlocked_pes_count = 0;
            // First post-switch encoded frame must be an IDR so receivers
            // get a clean entry point right at the switch boundary.
            self.force_idr.store(true, Ordering::Relaxed);
        }

        pub fn process(&mut self, input_ts: &[u8], output: &mut Vec<u8>) {
            if input_ts.is_empty() {
                return;
            }
            if input_ts.len() % TS_PACKET_SIZE != 0 {
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
                        let new_pmt_pid = programs[0].1;
                        if self.pmt_pid != Some(new_pmt_pid) {
                            if self.pmt_pid.is_some() {
                                // Input switched and chose a different PMT
                                // PID — anything cached about the old
                                // program is stale.
                                self.video_pid = None;
                                self.source_stream_type = 0;
                                self.reset_source_state("PMT PID changed");
                            }
                            self.pmt_pid = Some(new_pmt_pid);
                        }
                    }
                }

                if let Some(pmt_pid) = self.pmt_pid {
                    if pid == pmt_pid && ts_pusi(pkt) {
                        if let Some((vpid, vst)) = parse_pmt_video(pkt) {
                            let codec_changed =
                                self.source_stream_type != 0 && self.source_stream_type != vst;
                            let pid_changed =
                                self.video_pid.is_some() && self.video_pid != Some(vpid);
                            if codec_changed || pid_changed {
                                self.reset_source_state(&format!(
                                    "source changed: stream_type {:#04x} -> {:#04x}, pid {:?} -> {}",
                                    self.source_stream_type, vst, self.video_pid, vpid
                                ));
                            }
                            self.video_pid = Some(vpid);
                            self.source_stream_type = vst;
                        }
                        // Always run the PMT rewrite once we know the video
                        // PID — the rewrite enforces both the target
                        // stream_type AND PCR_PID = video_pid. When the
                        // source PMT already matches both, the byte-level
                        // edit and CRC recompute produce an output PMT
                        // identical to the input, so receivers see no
                        // version flap. When either differs, the rewrite
                        // is required (e.g. H.264 → HEVC needs the new
                        // stream_type, and any source whose PMT pointed
                        // PCR_PID at a separate dedicated PCR PID needs
                        // PCR_PID re-pointed at the rebuilt video PID).
                        if self.video_pid.is_some() {
                            let mut rewritten = pkt.to_vec();
                            rewrite_pmt_video_stream_type(
                                &mut rewritten,
                                self.video_pid.unwrap(),
                                self.target_stream_type,
                            );
                            output.extend_from_slice(&rewritten);
                        } else {
                            output.extend_from_slice(pkt);
                        }
                        continue;
                    }
                }

                if Some(pid) == self.video_pid {
                    self.feed_video_packet(pkt, output);
                    continue;
                }

                output.extend_from_slice(pkt);
            }
        }

        pub fn flush(&mut self, output: &mut Vec<u8>) {
            if self.pes_started && !self.pes_buffer.is_empty() {
                let pes = std::mem::take(&mut self.pes_buffer);
                let _ = self.consume_pes(&pes, output);
                self.pes_started = false;
            }
            if self.pipeline.is_open() {
                if let Ok(frames) = self.pipeline.flush() {
                    let vpid = match self.video_pid {
                        Some(p) => p,
                        None => return,
                    };
                    for ef in frames {
                        let pts_for_pes = self
                            .src_pts_queue
                            .pop_front()
                            .unwrap_or(self.pts_90k);
                        let pes = build_video_pes(&ef.data, pts_for_pes);
                        // PCR comes from the master clock when a flow-
                        // wide pacer is attached (broadcast-grade emit);
                        // otherwise fall back to the legacy pts*300 −
                        // preroll derivation so unit tests + non-mastered
                        // call sites keep their existing behaviour.
                        let pcr_27mhz = crate::engine::av_sync_mux::pcr_for_emit(
                            self.av_sync_pacer.as_ref(),
                            pts_for_pes,
                        );
                        let pkts = packetize_ts(
                            vpid,
                            &pes,
                            &mut self.out_video_cc,
                            Some(pcr_27mhz),
                        );
                        for p in &pkts {
                            output.extend_from_slice(p);
                        }
                        self.pts_90k = pts_for_pes.wrapping_add(self.pts_step_90k);
                    }
                }
            }
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
                self.pes_buffer.extend_from_slice(payload);
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
            // When source has no DTS (PTS_DTS_flags = 0b10), PTS itself
            // is monotonic — no B-frame reorder — so use it for the rate
            // measurement.
            let pes_dts = pes_dts.or(Some(pts));
            self.pending_pts = Some(pts);
            let pes_arrived_us = crate::util::time::now_us();

            if !self.pts_anchored {
                self.pts_90k = pts;
                self.pts_anchored = true;
            }

            // Measure the natural source frame interval from DTS deltas.
            // DTS is monotonic in the input PES stream even when the
            // source has B-frames (PTS reorders, DTS does not), so the
            // first valid delta = the source frame rate. Lock that into
            // the encoder's fps before it lazy-opens — libavcodec's
            // time-base is immutable post-open — so the encoded output's
            // CFR spacing matches the source. Without this, the encoder
            // ran at a 30 fps default for any source whose rate the
            // operator didn't pin in `video_encode.fps_num`, which is
            // what produced the user-visible jitter and frame skipping
            // on 25 fps and 50 fps DVB feeds.
            if let Some(dts) = pes_dts {
                if let Some(prev_dts) = self.last_input_dts {
                    let delta = dts.wrapping_sub(prev_dts);
                    if delta >= 90 && delta <= 90_000 {
                        self.pts_step_90k = delta;
                        if !self.source_fps_locked {
                            let fps_num = 90_000u32;
                            let fps_den = delta as u32;
                            let locked = self.pipeline.set_fps_if_unopened(fps_num, fps_den);
                            tracing::info!(
                                "ts_video_replace: source fps measured {}/{} ({:.3} fps) from DTS delta {} — encoder lock {}",
                                fps_num,
                                fps_den,
                                fps_num as f64 / fps_den as f64,
                                delta,
                                if locked { "ACQUIRED" } else { "MISSED (encoder already opened)" },
                            );
                            self.source_fps_locked = true;
                        }
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
                match VideoDecoder::open_with_backend(src_codec, decoder_backend) {
                    Ok(d) => {
                        if !matches!(decoder_backend, video_engine::DecoderBackend::Cpu) {
                            tracing::info!(
                                "ts_video_replace: opened HW decoder (backend={:?})",
                                decoder_backend,
                            );
                        }
                        self.decoder = Some(d);
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
                // src_pts_queue field for the rationale.
                if let Err(e) = dec.send_packet_with_pts(&es_data, pts as i64) {
                    // Partial/invalid packet — keep going.
                    tracing::debug!("ts_video_replace: send_packet: {e:?}");
                }
            }

            // If the operator pinned an fps in the config, that wins
            // over the measured delta — keep the step in lock-step
            // with whatever rate the encoder was opened at. (When the
            // operator left fps_num/fps_den unset, `pts_step_90k` was
            // already updated above from the observed source delta.)
            if let (Some(n), Some(d)) = (self.fps_num, self.fps_den) {
                self.pts_step_90k = (90_000u64 * d as u64) / (n.max(1) as u64);
            }

            // Defer the encoder drain until we have either an operator
            // override or two source DTS samples — the second sample is
            // what lets us lock libavcodec's time-base before the
            // encoder lazy-opens (post-open, the time-base is immutable
            // and we'd be stuck at the 30 fps placeholder forever).
            // We still pull frames out of the decoder so its DPB doesn't
            // back up — discarding the first frame or two is fine, the
            // encoder force-IDR flag (already raised on construction +
            // every input switch) ensures the first encoded frame at
            // post-lock time is a clean entry point.
            if !self.source_fps_locked {
                self.unlocked_pes_count = self.unlocked_pes_count.saturating_add(1);
                // Hard fallback: a pathological source with no DTS and
                // bogus PTS would keep us from ever locking. After 60
                // PES (~2 s of typical broadcast video) accept the
                // 30 fps placeholder so the encoder finally opens and
                // the operator gets visible output instead of silent
                // black. Logged loudly so the symptom — "30 fps output
                // for an N fps source" — is searchable in the field.
                if self.unlocked_pes_count >= 60 {
                    tracing::warn!(
                        "ts_video_replace: source rate not measurable from {} input PES (no usable DTS / PTS deltas) — falling back to 30 fps placeholder; output cadence will not track source",
                        self.unlocked_pes_count,
                    );
                    self.source_fps_locked = true;
                } else {
                    if let Some(dec) = self.decoder.as_mut() {
                        while dec.receive_frame().is_ok() {}
                    }
                    return Ok(());
                }
            }

            // Drain every frame the decoder can produce right now.
            loop {
                let frame = match self.decoder.as_mut().unwrap().receive_frame() {
                    Ok(f) => f,
                    Err(_) => break,
                };

                // Push the source PTS into the FIFO queue so the emit
                // path can pop it for each output PES. The decoder
                // propagates `pkt.pts → frame.pts` through its reorder
                // window, so for B-frame source streams we get the
                // display-order PTS automatically. Fall back to the
                // sample-counted anchor when the decoder didn't have a
                // PTS to attach (e.g. an early frame whose source PES
                // had `pts_dts_flags = 0`).
                let src_pts_for_frame = match frame.pts() {
                    Some(p) if p >= 0 => p as u64,
                    _ => self.pts_90k,
                };
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

                let vpid = self.video_pid.unwrap();
                for ef in encoded {
                    // Prefer source PTS from the queue; fall back to the
                    // sample-counted anchor when the queue is exhausted
                    // (encoder catch-up burst emitting more frames than
                    // we've pushed inputs for since the last drain).
                    let pts_for_pes = self
                        .src_pts_queue
                        .pop_front()
                        .unwrap_or(self.pts_90k);
                    let pes = build_video_pes(&ef.data, pts_for_pes);
                    // PCR sits PCR_PREROLL_27MHZ behind the master clock
                    // (or PTS-derived clock if no pacer is attached) so
                    // the receiver's T-STD buffer model has room. With
                    // a pacer attached, PCR cadence is locked to the
                    // master clock — every output of the flow emits an
                    // identical PCR sequence regardless of internal
                    // pipeline depth, and multi-edge plants on the same
                    // PTP/source PCR stay coherent.
                    let pcr_27mhz = crate::engine::av_sync_mux::pcr_for_emit(
                        self.av_sync_pacer.as_ref(),
                        pts_for_pes,
                    );
                    let pkts = packetize_ts(
                        vpid,
                        &pes,
                        &mut self.out_video_cc,
                        Some(pcr_27mhz),
                    );
                    for p in &pkts {
                        output.extend_from_slice(p);
                    }
                    // Keep the fallback anchor monotonic from the latest
                    // emitted PTS so a later queue-exhausted emit still
                    // produces a sensible (non-decreasing) value.
                    self.pts_90k = pts_for_pes.wrapping_add(self.pts_step_90k);
                    self.stats.output_frames.fetch_add(1, Ordering::Relaxed);
                }
                let lat = crate::util::time::now_us().saturating_sub(pes_arrived_us);
                self.stats.last_latency_us.store(lat, Ordering::Relaxed);
            }

            Ok(())
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
            other => Err(TsVideoReplaceError::UnknownCodec(other.to_string())),
        }
    }

}

// ─────────────────────────── Shared helpers ───────────────────────────

/// Parse the PMT for the first video stream. Returns `(video_pid,
/// stream_type)` or `None` if no video ES is present. Accepts MPEG-1
/// (0x01), MPEG-2 (0x02), H.264 (0x1B) and HEVC (0x24).
#[cfg(feature = "video-thumbnail")]
fn parse_pmt_video(pkt: &[u8]) -> Option<(u16, u8)> {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 {
        return None;
    }

    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let st = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

        if matches!(st, 0x01 | 0x02 | 0x1B | 0x24) {
            return Some((es_pid, st));
        }
        pos += 5 + es_info_len;
    }
    None
}

/// Rewrite the video stream_type in a PMT TS packet in place, force the
/// PMT's `PCR_PID` to match the rebuilt video PID (where this module
/// emits PCR fields), and recompute the section CRC32.
///
/// The PCR_PID field lives at section_start + 8..=9 (top 3 bits reserved,
/// bottom 13 bits = PID). Without this rewrite, sources whose PMT pointed
/// PCR_PID at a separate dedicated PCR PID would leave us emitting PCR
/// on the video PID while the PMT advertises PCR somewhere else — a
/// mismatch professional decoders treat as a TR 101 290 P1.6 violation.
#[cfg(feature = "video-thumbnail")]
fn rewrite_pmt_video_stream_type(pkt: &mut [u8], video_pid: u16, new_stream_type: u8) {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return;
    }

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 {
        return;
    }

    let section_start = offset;
    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    // Force PCR_PID = video_pid. Top 3 bits remain reserved (set to 1
    // per ISO 13818-1; the legacy 0xE0 value preserves whatever the
    // source had in those reserved bits' position).
    pkt[section_start + 8] =
        (pkt[section_start + 8] & 0xE0) | (((video_pid >> 8) as u8) & 0x1F);
    pkt[section_start + 9] = (video_pid & 0xFF) as u8;
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        if es_pid == video_pid {
            pkt[pos] = new_stream_type;
        }
        pos += 5 + es_info_len;
    }

    let crc_offset = section_start + 3 + section_length - 4;
    if crc_offset + 4 <= TS_PACKET_SIZE {
        let crc = mpeg2_crc32(&pkt[section_start..crc_offset]);
        pkt[crc_offset] = (crc >> 24) as u8;
        pkt[crc_offset + 1] = (crc >> 16) as u8;
        pkt[crc_offset + 2] = (crc >> 8) as u8;
        pkt[crc_offset + 3] = crc as u8;
    }
}

/// Extract the ES payload and PTS from a complete PES packet.
#[cfg(feature = "video-thumbnail")]
fn extract_pes_video(pes: &[u8]) -> Option<(Vec<u8>, u64, Option<u64>)> {
    if pes.len() < 9 || pes[0] != 0x00 || pes[1] != 0x00 || pes[2] != 0x01 {
        return None;
    }
    let header_data_len = pes[8] as usize;
    let es_start = 9 + header_data_len;
    if es_start >= pes.len() {
        return None;
    }
    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    let pts = if pts_dts_flags >= 2 && pes.len() >= 14 {
        parse_pts(&pes[9..14])
    } else {
        0
    };
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
#[cfg(feature = "video-thumbnail")]
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
#[cfg(feature = "video-thumbnail")]
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
#[cfg(feature = "video-thumbnail")]
fn build_video_pes(video_data: &[u8], pts: u64) -> Vec<u8> {
    build_video_pes_with_dts(video_data, pts, pts)
}

/// Same as [`build_video_pes`] but lets the caller pass an explicit DTS
/// distinct from the PTS. Kept separate so the no-B-frame default path
/// stays a one-argument call.
#[cfg(feature = "video-thumbnail")]
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

/// Encode a 6-byte PCR field per ISO/IEC 13818-1 §2.4.3.5.
///
/// The 42-bit PCR splits into a 33-bit base @ 90 kHz and a 9-bit extension
/// @ 27 MHz: `pcr_27mhz = base * 300 + ext`. Bytes 0..3 carry the high 32
/// bits of base; byte 4 packs the LSB of base + 6 reserved 1-bits + the top
/// bit of ext; byte 5 carries the low 8 bits of ext.
fn write_pcr_field(buf: &mut [u8; 6], pcr_27mhz: u64) {
    let base = (pcr_27mhz / 300) & 0x1_FFFF_FFFF; // 33-bit
    let ext = (pcr_27mhz % 300) as u32; // 9-bit
    buf[0] = ((base >> 25) & 0xFF) as u8;
    buf[1] = ((base >> 17) & 0xFF) as u8;
    buf[2] = ((base >> 9) & 0xFF) as u8;
    buf[3] = ((base >> 1) & 0xFF) as u8;
    buf[4] = (((base & 1) << 7) as u8) | 0x7E | (((ext >> 8) & 0x01) as u8);
    buf[5] = (ext & 0xFF) as u8;
}

/// Pack a PES into 188-byte TS packets on `pid`. When `pcr_27mhz` is `Some`,
/// the PUSI start packet carries an adaptation field with `PCR_flag = 1` —
/// this is what makes the rebuilt video PID a valid PCR carrier so the
/// downstream stream complies with TR 101 290 P1.5 / P1.7. (Without it,
/// software re-mux paths emit a stream with PMT-declared PCR_PID = video
/// PID but no PCR fields anywhere, which professional decoders reject.)
fn packetize_ts(
    pid: u16,
    pes: &[u8],
    cc: &mut u8,
    pcr_27mhz: Option<u64>,
) -> Vec<[u8; 188]> {
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

        // PCR-carrying PUSI start: build adaptation field (8 bytes:
        // 1 length + 1 flags + 6 PCR), then payload fills the rest.
        // af_length = 7 (excludes the length byte itself).
        if is_first && pcr_27mhz.is_some() {
            const AF_BYTES_AFTER_LEN: usize = 7; // flags(1) + PCR(6)
            const AF_TOTAL: usize = AF_BYTES_AFTER_LEN + 1; // + length byte
            let payload_capacity = TS_PACKET_SIZE - 4 - AF_TOTAL;
            pkt[3] = 0x30 | current_cc; // AFC = both
            pkt[4] = AF_BYTES_AFTER_LEN as u8;
            pkt[5] = 0x10; // PCR_flag only
            let mut pcr_buf = [0u8; 6];
            write_pcr_field(&mut pcr_buf, pcr_27mhz.unwrap());
            pkt[6..12].copy_from_slice(&pcr_buf);
            let take = remaining.min(payload_capacity);
            pkt[4 + AF_TOTAL..4 + AF_TOTAL + take]
                .copy_from_slice(&pes[offset..offset + take]);
            // If the PES is short enough to fit entirely in this PCR-carrying
            // packet, pad the trailing bytes back into the adaptation field.
            // We do this by extending af_length and stuffing 0xFF, so the
            // payload still ends at byte 187. This case is rare for video
            // PES (frames are kilobytes) — handled here only to avoid
            // truncation if a tiny frame ever shows up.
            if take < payload_capacity {
                let stuff = payload_capacity - take;
                let new_af_len = AF_BYTES_AFTER_LEN + stuff;
                pkt[4] = new_af_len as u8;
                // Move the (small) payload to the end of the packet.
                let payload_start_old = 4 + AF_TOTAL;
                let payload_start_new = TS_PACKET_SIZE - take;
                if take > 0 {
                    pkt.copy_within(
                        payload_start_old..payload_start_old + take,
                        payload_start_new,
                    );
                }
                // Fill the new stuffing bytes with 0xFF.
                for b in pkt
                    .iter_mut()
                    .take(payload_start_new)
                    .skip(4 + AF_TOTAL)
                {
                    *b = 0xFF;
                }
            }
            offset += take;
        } else {
            let payload_capacity = TS_PACKET_SIZE - 4;
            if remaining >= payload_capacity {
                pkt[3] = 0x10 | current_cc;
                pkt[4..TS_PACKET_SIZE]
                    .copy_from_slice(&pes[offset..offset + payload_capacity]);
                offset += payload_capacity;
            } else {
                let stuff_len = payload_capacity - remaining;
                if stuff_len == 1 {
                    pkt[3] = 0x30 | current_cc;
                    pkt[4] = 0;
                    pkt[5..5 + remaining].copy_from_slice(&pes[offset..]);
                } else {
                    pkt[3] = 0x30 | current_cc;
                    pkt[4] = (stuff_len - 1) as u8;
                    if stuff_len > 1 {
                        pkt[5] = 0x00;
                        for i in 6..4 + stuff_len {
                            pkt[i] = 0xFF;
                        }
                    }
                    pkt[4 + stuff_len..4 + stuff_len + remaining]
                        .copy_from_slice(&pes[offset..]);
                }
                offset += remaining;
            }
        }
        is_first = false;
        packets.push(pkt);
    }
    packets
}

// ─────────────────────────── tests ───────────────────────────

#[cfg(all(test, feature = "video-thumbnail"))]
mod tests {
    use super::*;

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
        assert_eq!(parse_pmt_video(&pkt), Some((0x0100, 0x1B)));
        let pkt2 = synth_pmt(0x1000, 0x0100, 0x24);
        assert_eq!(parse_pmt_video(&pkt2), Some((0x0100, 0x24)));
        let pkt3 = synth_pmt(0x1000, 0x0100, 0x02);
        assert_eq!(parse_pmt_video(&pkt3), Some((0x0100, 0x02)));
        let pkt4 = synth_pmt(0x1000, 0x0100, 0x01);
        assert_eq!(parse_pmt_video(&pkt4), Some((0x0100, 0x01)));
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

    /// PCR pre-roll: the PCR field on a PUSI start packet must be
    /// strictly less than the PES PTS (in 27 MHz units) by
    /// [`PCR_PREROLL_27MHZ`]. This is the bug that caused VLC and Appear
    /// hardware decoders to play a few buffered frames at startup and
    /// then stutter — without the pre-roll, every frame's PTS coincides
    /// with the receiver's STC and the decoder pipeline has zero time
    /// to dequeue + decode + render.
    #[test]
    fn packetize_ts_pcr_trails_pts_by_preroll() {
        let pts_90k: u64 = 90_000; // 1 s into the source clock
        let pcr_27mhz = pts_90k.saturating_mul(300).saturating_sub(PCR_PREROLL_27MHZ);
        // PCR trails PTS by exactly PCR_PREROLL_27MHZ ticks (80 ms × 27 MHz).
        assert_eq!(pcr_27mhz, 90_000 * 300 - PCR_PREROLL_27MHZ);
        assert!(pcr_27mhz < pts_90k * 300);
        assert_eq!(pts_90k * 300 - pcr_27mhz, PCR_PREROLL_27MHZ);

        // Mux a tiny PES with that PCR and verify the PCR field readback.
        let pes = build_video_pes(&[0, 0, 0, 1, 0x09, 0x10], pts_90k);
        let mut cc = 0u8;
        let pkts = packetize_ts(0x100, &pes, &mut cc, Some(pcr_27mhz));
        assert!(!pkts.is_empty());
        let first = &pkts[0];
        // PUSI bit set on first packet.
        assert_eq!(first[1] & 0x40, 0x40);
        // Adaptation+payload (AFC = 0b11).
        assert_eq!(first[3] & 0x30, 0x30);
        // af_length >= 7 (1 byte flags + 6 byte PCR).
        assert!(first[4] >= 7);
        // PCR_flag bit set.
        assert_eq!(first[5] & 0x10, 0x10);

        // Decode the 6-byte PCR field and confirm round-trip equality.
        let base = ((first[6] as u64) << 25)
            | ((first[7] as u64) << 17)
            | ((first[8] as u64) << 9)
            | ((first[9] as u64) << 1)
            | (((first[10] >> 7) as u64) & 0x01);
        let ext = (((first[10] as u64) & 0x01) << 8) | (first[11] as u64);
        let read_pcr_27mhz = base * 300 + ext;
        assert_eq!(read_pcr_27mhz, pcr_27mhz);
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
        pkt[crc_offset] = (new_crc >> 24) as u8;
        pkt[crc_offset + 1] = (new_crc >> 16) as u8;
        pkt[crc_offset + 2] = (new_crc >> 8) as u8;
        pkt[crc_offset + 3] = new_crc as u8;
        // Sanity: confirm the synth setup before exercising the rewrite.
        let pcr_pid_before = ((pkt[5 + 8] as u16 & 0x1F) << 8) | pkt[5 + 9] as u16;
        assert_eq!(pcr_pid_before, 0x1234);

        // Run the rewrite. Target stream_type matches source (0x1B) so
        // only the PCR_PID change drives the edit.
        rewrite_pmt_video_stream_type(&mut pkt, 0x0100, 0x1B);

        let pcr_pid_after = ((pkt[5 + 8] as u16 & 0x1F) << 8) | pkt[5 + 9] as u16;
        assert_eq!(pcr_pid_after, 0x0100, "PCR_PID must be re-pointed at video PID");

        // CRC must still validate after the rewrite.
        let new_section_length = (((pkt[5 + 1] & 0x0F) as usize) << 8) | (pkt[5 + 2] as usize);
        let new_crc_offset = 5 + 3 + new_section_length - 4;
        let computed = mpeg2_crc32(&pkt[5..new_crc_offset]);
        let stored = ((pkt[new_crc_offset] as u32) << 24)
            | ((pkt[new_crc_offset + 1] as u32) << 16)
            | ((pkt[new_crc_offset + 2] as u32) << 8)
            | (pkt[new_crc_offset + 3] as u32);
        assert_eq!(computed, stored, "CRC must validate after rewrite");
    }
}
