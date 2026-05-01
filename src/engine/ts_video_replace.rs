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
        /// source PES PTS.
        pts_90k: u64,
        pts_anchored: bool,
        pts_step_90k: u64,

        description: String,
        stats: Arc<VideoEncodeStats>,
        force_idr: Arc<AtomicBool>,
    }

    impl Inner {
        pub fn from_config(
            cfg: &VideoEncodeConfig,
            stats: Arc<VideoEncodeStats>,
            force_idr: Arc<AtomicBool>,
        ) -> Result<Self, TsVideoReplaceError> {
            let backend = parse_codec(&cfg.codec)?;
            let target_family = backend.family();
            let target_stream_type = target_family.stream_type();

            let description = format!(
                "{} @ {} kbps",
                backend.ffmpeg_name(),
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
            let pipeline = ScaledVideoEncoder::new(
                cfg.clone(),
                backend,
                fps_num,
                fps_den,
                false,
                "ts_video_replace",
            );

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
                description,
                stats,
                force_idr,
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
                        // Only rewrite the PMT when the target codec family
                        // differs from the source. Same-family transcodes
                        // (e.g. H.264 → H.264 bitrate-drop) leave the PMT
                        // untouched so receivers don't see a CRC flap.
                        if self.video_pid.is_some()
                            && self.source_stream_type != self.target_stream_type
                        {
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
                        let pes = build_video_pes(&ef.data, self.pts_90k);
                        let pkts = packetize_ts(
                            vpid,
                            &pes,
                            &mut self.out_video_cc,
                            Some(self.pts_90k.wrapping_mul(300)),
                        );
                        for p in &pkts {
                            output.extend_from_slice(p);
                        }
                        self.pts_90k = self.pts_90k.wrapping_add(self.pts_step_90k);
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
            let (es_data, pts) = match extract_pes_video(pes) {
                Some(x) => {
                    self.stats.input_frames.fetch_add(1, Ordering::Relaxed);
                    x
                }
                None => {
                    self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                    return Err(());
                }
            };
            self.pending_pts = Some(pts);
            let pes_arrived_us = crate::util::time::now_us();

            if !self.pts_anchored {
                self.pts_90k = pts;
                self.pts_anchored = true;
            }

            if self.decoder.is_none() {
                let src_codec = match VideoCodec::from_stream_type(self.source_stream_type) {
                    Some(c) => c,
                    None => {
                        self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        return Err(());
                    }
                };
                match VideoDecoder::open(src_codec) {
                    Ok(d) => self.decoder = Some(d),
                    Err(e) => {
                        tracing::error!("ts_video_replace: failed to open decoder: {e}");
                        self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        return Err(());
                    }
                }
            }

            if let Some(dec) = self.decoder.as_mut() {
                if let Err(e) = dec.send_packet(&es_data) {
                    // Partial/invalid packet — keep going.
                    tracing::debug!("ts_video_replace: send_packet: {e:?}");
                }
            }

            // Drain every frame the decoder can produce right now.
            // Refresh the 90 kHz PTS step on every loop iteration — the
            // pipeline's lazy-open caches the fps it was constructed
            // with, but we re-derive the step here once we know we're
            // about to emit output.
            let (fps_num, fps_den) = match (self.fps_num, self.fps_den) {
                (Some(n), Some(d)) => (n, d),
                _ => (30, 1),
            };
            self.pts_step_90k = (90_000u64 * fps_den as u64) / fps_num.max(1) as u64;

            loop {
                let frame = match self.decoder.as_mut().unwrap().receive_frame() {
                    Ok(f) => f,
                    Err(_) => break,
                };

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
                        continue;
                    }
                };
                self.out_frame_count += 1;

                let vpid = self.video_pid.unwrap();
                for ef in encoded {
                    let pes = build_video_pes(&ef.data, self.pts_90k);
                    let pkts = packetize_ts(
                        vpid,
                        &pes,
                        &mut self.out_video_cc,
                        Some(self.pts_90k.wrapping_mul(300)),
                    );
                    for p in &pkts {
                        output.extend_from_slice(p);
                    }
                    self.pts_90k = self.pts_90k.wrapping_add(self.pts_step_90k);
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
            other => Err(TsVideoReplaceError::UnknownCodec(other.to_string())),
        }
    }

}

// ─────────────────────────── Shared helpers ───────────────────────────

/// Parse the PMT for the first video stream. Returns `(video_pid,
/// stream_type)` or `None` if no video ES is present. Accepts H.264
/// (0x1B) and HEVC (0x24).
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

        if matches!(st, 0x1B | 0x24) {
            return Some((es_pid, st));
        }
        pos += 5 + es_info_len;
    }
    None
}

/// Rewrite the video stream_type in a PMT TS packet in place and
/// recompute the section CRC32.
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
fn extract_pes_video(pes: &[u8]) -> Option<(Vec<u8>, u64)> {
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
    Some((pes[es_start..].to_vec(), pts))
}

/// Decode the 5-byte PTS in a PES optional header.
#[cfg(feature = "video-thumbnail")]
fn parse_pts(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;
    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) << 15)
        | (b3 << 7)
        | (b4 >> 1)
}

/// Wrap an encoded video frame in a PES packet with a PTS header.
/// Video PES packets use stream_id 0xE0 and unbounded length (the
/// 16-bit length field is zero for video).
#[cfg(feature = "video-thumbnail")]
fn build_video_pes(video_data: &[u8], pts: u64) -> Vec<u8> {
    let mut pes = Vec::with_capacity(14 + video_data.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01]);
    pes.push(0xE0); // video stream_id
    pes.extend_from_slice(&[0, 0]); // unbounded length
    pes.push(0x80); // marker bits
    pes.push(0x80); // PTS present, no DTS
    pes.push(5);    // PES header data length

    let pts = pts & 0x1_FFFF_FFFF;
    pes.push(0x21 | (((pts >> 30) as u8) & 0x0E));
    pes.push((pts >> 22) as u8);
    pes.push(0x01 | (((pts >> 15) as u8) & 0xFE));
    pes.push((pts >> 7) as u8);
    pes.push(0x01 | (((pts as u8) & 0x7F) << 1));

    pes.extend_from_slice(video_data);
    pes
}

/// Packetize a PES payload into one or more 188-byte TS packets.
/// Shared with `ts_audio_replace::packetize_ts`, duplicated here to
/// avoid coupling the two modules.
#[cfg(feature = "video-thumbnail")]
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
    fn build_video_pes_has_stream_id_and_pts() {
        let pes = build_video_pes(&[0, 0, 0, 1], 0xABCD_EF12);
        assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01]);
        assert_eq!(pes[3], 0xE0); // video stream_id
        assert_eq!(pes[7], 0x80);
        assert_eq!(pes[8], 5);
        assert_eq!(&pes[pes.len() - 4..], &[0, 0, 0, 1]);
    }
}
