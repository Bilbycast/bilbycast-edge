// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

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

use std::sync::atomic::{AtomicU64, Ordering};
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
}

impl TsVideoReplacer {
    /// Build a new replacer from a `video_encode` block. Codec state is
    /// opened lazily on the first decoded frame.
    #[cfg(feature = "video-thumbnail")]
    pub fn new(cfg: &VideoEncodeConfig) -> Result<Self, TsVideoReplaceError> {
        let stats = Arc::new(VideoEncodeStats::default());
        let inner = inner::Inner::from_config(cfg, stats.clone())?;
        let description = inner.target_description();
        Ok(Self {
            inner,
            description,
            stats,
        })
    }

    /// Stub constructor when `video-thumbnail` is compiled out — returns
    /// `VideoEngineMissing` so callers fail cleanly at output startup.
    #[cfg(not(feature = "video-thumbnail"))]
    pub fn new(_cfg: &VideoEncodeConfig) -> Result<Self, TsVideoReplaceError> {
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
    use video_codec::{
        VideoCodec, VideoEncoderCodec, VideoEncoderConfig, VideoPreset, VideoProfile,
    };
    use video_engine::{VideoDecoder, VideoEncoder};

    pub struct Inner {
        backend: VideoEncoderCodec,
        #[allow(dead_code)]
        target_family: VideoCodec,
        requested_width: Option<u32>,
        requested_height: Option<u32>,
        fps_num: Option<u32>,
        fps_den: Option<u32>,
        bitrate_kbps: u32,
        gop_size: Option<u32>,
        preset: VideoPreset,
        profile: VideoProfile,

        pmt_pid: Option<u16>,
        video_pid: Option<u16>,
        source_stream_type: u8,
        target_stream_type: u8,

        pes_buffer: Vec<u8>,
        pes_started: bool,
        pending_pts: Option<u64>,

        decoder: Option<VideoDecoder>,
        encoder: Option<VideoEncoder>,

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
    }

    impl Inner {
        pub fn from_config(
            cfg: &VideoEncodeConfig,
            stats: Arc<VideoEncodeStats>,
        ) -> Result<Self, TsVideoReplaceError> {
            let backend = parse_codec(&cfg.codec)?;
            let target_family = backend.family();
            let target_stream_type = target_family.stream_type();

            let preset = cfg
                .preset
                .as_deref()
                .map(parse_preset)
                .unwrap_or(VideoPreset::Medium);
            let profile = cfg
                .profile
                .as_deref()
                .map(parse_profile)
                .unwrap_or(VideoProfile::Auto);

            let description = format!(
                "{} @ {} kbps",
                backend.ffmpeg_name(),
                cfg.bitrate_kbps.unwrap_or(4000),
            );

            Ok(Self {
                backend,
                target_family,
                requested_width: cfg.width,
                requested_height: cfg.height,
                fps_num: cfg.fps_num,
                fps_den: cfg.fps_den,
                bitrate_kbps: cfg.bitrate_kbps.unwrap_or(4000),
                gop_size: cfg.gop_size,
                preset,
                profile,
                pmt_pid: None,
                video_pid: None,
                source_stream_type: 0,
                target_stream_type,
                pes_buffer: Vec::with_capacity(256 * 1024),
                pes_started: false,
                pending_pts: None,
                decoder: None,
                encoder: None,
                out_video_cc: 0,
                out_frame_count: 0,
                pts_90k: 0,
                pts_anchored: false,
                pts_step_90k: 3000, // 30 fps default until we learn otherwise
                description,
                stats,
            })
        }

        pub fn target_description(&self) -> String {
            self.description.clone()
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

                if pid == PAT_PID && ts_pusi(pkt) && self.pmt_pid.is_none() {
                    let mut programs = parse_pat_programs(pkt);
                    if !programs.is_empty() {
                        programs.sort_by_key(|(num, _)| *num);
                        self.pmt_pid = Some(programs[0].1);
                    }
                }

                if let Some(pmt_pid) = self.pmt_pid {
                    if pid == pmt_pid && ts_pusi(pkt) {
                        if self.video_pid.is_none() {
                            if let Some((vpid, vst)) = parse_pmt_video(pkt) {
                                self.video_pid = Some(vpid);
                                self.source_stream_type = vst;
                            }
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
            if let Some(enc) = self.encoder.as_mut() {
                if let Ok(frames) = enc.flush() {
                    let vpid = match self.video_pid {
                        Some(p) => p,
                        None => return,
                    };
                    for ef in frames {
                        let pes = build_video_pes(&ef.data, self.pts_90k);
                        let pkts = packetize_ts(vpid, &pes, &mut self.out_video_cc);
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
            loop {
                let frame = match self.decoder.as_mut().unwrap().receive_frame() {
                    Ok(f) => f,
                    Err(_) => break,
                };

                if self.encoder.is_none() {
                    let src_w = frame.width();
                    let src_h = frame.height();
                    if let (Some(rw), Some(rh)) =
                        (self.requested_width, self.requested_height)
                    {
                        if rw != src_w || rh != src_h {
                            tracing::warn!(
                                "ts_video_replace: resolution change {}x{} -> {}x{} requested \
                                 but scaling is not implemented yet; using source resolution",
                                src_w, src_h, rw, rh
                            );
                        }
                    }
                    let (fps_num, fps_den) = match (self.fps_num, self.fps_den) {
                        (Some(n), Some(d)) => (n, d),
                        _ => (30, 1),
                    };
                    self.pts_step_90k =
                        (90_000u64 * fps_den as u64) / fps_num.max(1) as u64;

                    let gop_size = self.gop_size.unwrap_or(fps_num.max(1) * 2);

                    let enc_cfg = VideoEncoderConfig {
                        codec: self.backend,
                        width: src_w,
                        height: src_h,
                        fps_num,
                        fps_den,
                        bitrate_kbps: self.bitrate_kbps,
                        gop_size,
                        preset: self.preset,
                        profile: self.profile,
                        global_header: false, // emit SPS/PPS inline
                    };
                    match VideoEncoder::open(&enc_cfg) {
                        Ok(e) => self.encoder = Some(e),
                        Err(e) => {
                            tracing::error!(
                                "ts_video_replace: failed to open encoder: {e}"
                            );
                            return Err(());
                        }
                    }
                }

                // Pull the three YUV planes straight out of the decoded
                // frame. Non-planar pixel formats (unlikely for broadcast
                // H.264/HEVC) are skipped.
                let (y_data, y_stride, u_data, u_stride, v_data, v_stride) =
                    match frame.yuv_planes() {
                        Some(p) => p,
                        None => continue,
                    };

                let enc = self.encoder.as_mut().unwrap();
                let encoded = match enc.encode_frame(
                    y_data, y_stride, u_data, u_stride, v_data, v_stride,
                    Some(self.out_frame_count),
                ) {
                    Ok(frames) => frames,
                    Err(e) => {
                        tracing::debug!("ts_video_replace: encode error: {e}");
                        self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                self.out_frame_count += 1;

                let vpid = self.video_pid.unwrap();
                for ef in encoded {
                    let pes = build_video_pes(&ef.data, self.pts_90k);
                    let pkts = packetize_ts(vpid, &pes, &mut self.out_video_cc);
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
            other => Err(TsVideoReplaceError::UnknownCodec(other.to_string())),
        }
    }

    fn parse_preset(s: &str) -> VideoPreset {
        match s {
            "ultrafast" => VideoPreset::Ultrafast,
            "superfast" => VideoPreset::Superfast,
            "veryfast" => VideoPreset::Veryfast,
            "faster" => VideoPreset::Faster,
            "fast" => VideoPreset::Fast,
            "medium" => VideoPreset::Medium,
            "slow" => VideoPreset::Slow,
            "slower" => VideoPreset::Slower,
            "veryslow" => VideoPreset::Veryslow,
            _ => VideoPreset::Medium,
        }
    }

    fn parse_profile(s: &str) -> VideoProfile {
        match s {
            "baseline" => VideoProfile::Baseline,
            "main" => VideoProfile::Main,
            "high" => VideoProfile::High,
            _ => VideoProfile::Auto,
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
        }
    }

    #[test]
    fn rejects_unknown_codec() {
        assert!(TsVideoReplacer::new(&cfg("vp9")).is_err());
    }

    #[test]
    fn accepts_x264_and_x265_and_nvenc() {
        assert!(TsVideoReplacer::new(&cfg("x264")).is_ok());
        assert!(TsVideoReplacer::new(&cfg("x265")).is_ok());
        assert!(TsVideoReplacer::new(&cfg("h264_nvenc")).is_ok());
        assert!(TsVideoReplacer::new(&cfg("hevc_nvenc")).is_ok());
    }

    #[test]
    fn process_empty_input_is_noop() {
        let mut r = TsVideoReplacer::new(&cfg("x264")).unwrap();
        let mut out = Vec::new();
        r.process(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn process_misaligned_input_is_passthrough() {
        let mut r = TsVideoReplacer::new(&cfg("x264")).unwrap();
        let mut out = Vec::new();
        let input = vec![0u8; 100];
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn process_unknown_pid_passes_through_verbatim() {
        let mut r = TsVideoReplacer::new(&cfg("x264")).unwrap();
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x1F;
        pkt[2] = 0xFF;
        pkt[3] = 0x10;

        let mut out = Vec::new();
        r.process(&pkt, &mut out);
        assert_eq!(&out[..], &pkt[..]);
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
