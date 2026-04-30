// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Synthetic test-pattern input.
//!
//! Generates SMPTE 75% colour bars (vertical, 8 bars: white, yellow,
//! cyan, green, magenta, red, blue, black) at the requested resolution
//! and frame rate, optionally plus a 1 kHz sine tone at the requested
//! level, encodes to H.264 + AAC, muxes to MPEG-TS via the shared
//! [`TsMuxer`], and publishes the TS bytes as `RtpPacket { is_raw_ts:
//! true }` onto the per-input broadcast channel.
//!
//! Useful for:
//! - Pre-live validation — bring up the pipeline end-to-end before the
//!   real source is patched in.
//! - Downstream troubleshooting — a known-good source isolates whether
//!   a problem is upstream of this edge or in the transport/decoder.
//! - Idle fill — an unmistakable "this channel is reachable, but the
//!   production feed isn't live" marker.
//!
//! Requires compile-time features `video-thumbnail` (for the
//! `video-engine` encoder wrapper) and `fdk-aac` (for the AAC encoder).
//! At runtime a video encoder backend must be compiled in
//! (`video-encoder-x264` or `-x265` or `-nvenc`) — without one the
//! input emits a critical event and exits cleanly rather than spinning.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, interval_at};
use tokio_util::sync::CancellationToken;

use crate::config::models::TestPatternInputConfig;
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

/// Public entry point. Matches the signature shape of other input
/// spawners (`input_bonded`, `input_rtp_audio`) so the flow runtime can
/// call it through the same `input_handles` / `per_input_tx` pipeline.
pub fn spawn_test_pattern_input(
    config: TestPatternInputConfig,
    per_input_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: EventSender,
    flow_id: String,
    input_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run(config, per_input_tx, stats, cancel, events, flow_id, input_id).await;
    })
}

async fn run(
    config: TestPatternInputConfig,
    per_input_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: EventSender,
    flow_id: String,
    input_id: String,
) {
    #[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
    {
        if let Err(e) = run_inner(&config, &per_input_tx, &stats, &cancel, &events, &flow_id, &input_id).await {
            events.emit_flow(
                EventSeverity::Critical,
                category::FLOW,
                format!("Test-pattern input '{input_id}' failed: {e}"),
                &flow_id,
            );
            // Stay alive until cancel so FlowRuntime can shut us down cleanly.
            cancel.cancelled().await;
        }
    }
    #[cfg(not(all(feature = "video-thumbnail", feature = "fdk-aac")))]
    {
        events.emit_flow(
            EventSeverity::Critical,
            category::FLOW,
            format!(
                "Test-pattern input '{input_id}' requires the 'video-thumbnail' and 'fdk-aac' features — rebuild the edge with those enabled"
            ),
            &flow_id,
        );
        cancel.cancelled().await;
    }
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
async fn run_inner(
    config: &TestPatternInputConfig,
    per_input_tx: &broadcast::Sender<RtpPacket>,
    stats: &Arc<FlowStatsAccumulator>,
    cancel: &CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
) -> Result<(), String> {
    use crate::config::models::VideoEncodeConfig;
    use crate::engine::video_encode_util::ScaledVideoEncoder;

    let width = config.width as u32;
    let height = config.height as u32;
    let fps = config.fps.max(1) as u32;
    let frame_duration = Duration::from_nanos(1_000_000_000 / fps as u64);

    // Select an available video encoder backend. x264 is the most
    // common; fall back to x265 or NVENC if only those are compiled in.
    let backend = select_video_backend()
        .ok_or_else(|| "no video encoder backend compiled in (enable one of video-encoder-x264 / -x265 / -nvenc)".to_string())?;

    let video_cfg = VideoEncodeConfig {
        codec: backend_codec_string(backend).to_string(),
        width: Some(width),
        height: Some(height),
        fps_num: Some(fps),
        fps_den: Some(1),
        bitrate_kbps: Some(config.video_bitrate_kbps),
        gop_size: Some((fps * 2).min(600)),
        preset: Some("ultrafast".to_string()),
        profile: None,
        chroma: None,
        bit_depth: None,
        rate_control: None,
        crf: None,
        max_bitrate_kbps: None,
        bframes: Some(0),
        refs: None,
        level: None,
        tune: None,
        color_primaries: None,
        color_transfer: None,
        color_matrix: None,
        color_range: None,
    };

    let mut encoder = ScaledVideoEncoder::new(
        video_cfg,
        backend,
        fps,
        1,
        false,
        format!("test-pattern:{input_id}"),
    );

    // Build YUV420P colour-bars once as the static base. Each tick we
    // copy the Y plane out and overlay motion (bouncing box + timecode)
    // so the output is visibly live — chroma planes stay shared.
    let (y_base, u_plane, v_plane, y_stride, u_stride, v_stride) =
        build_smpte_bars_yuv420p(width, height);
    let mut y_plane = y_base.clone();
    // AV_PIX_FMT_YUV420P == 0
    const AV_PIX_FMT_YUV420P: i32 = 0;

    // Build the audio encoder if enabled.
    let audio_ctx = if config.audio_enabled {
        Some(build_audio_encoder(config.tone_hz, config.tone_dbfs)?)
    } else {
        None
    };

    let mut ts_muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
    // Without this the PMT never advertises the audio PID and downstream
    // decoders silently drop the muxed AAC PES packets.
    if audio_ctx.is_some() {
        ts_muxer.set_has_audio(true);
    }
    let mut seq_num: u16 = 0;
    let start = Instant::now();
    let mut ticker = interval_at(start, frame_duration);
    // Fire the first tick immediately so there's no startup hiccup.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    events.emit_flow(
        EventSeverity::Info,
        category::FLOW,
        format!(
            "Test-pattern input '{input_id}' started ({width}x{height}@{fps} {} audio)",
            if audio_ctx.is_some() { "+" } else { "no" }
        ),
        flow_id,
    );

    let mut frame_idx: u64 = 0;
    // Audio state: sine phase, pts running counter in 90kHz.
    let mut audio_state = audio_ctx;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                return Ok(());
            }
            _ = ticker.tick() => {}
        }

        let pts_90khz = frame_idx.saturating_mul(90_000) / fps as u64;

        // Refresh Y plane from the static bars and overlay motion so
        // downstream viewers can tell the feed is live, not frozen.
        y_plane.copy_from_slice(&y_base);
        draw_bouncing_box(&mut y_plane, width as usize, height as usize, frame_idx);
        draw_timecode(&mut y_plane, width as usize, height as usize, frame_idx, fps);

        // ── Video ── encode one frame of bars.
        let encoded_frames = tokio::task::block_in_place(|| {
            encoder.encode_raw_planes(
                width,
                height,
                AV_PIX_FMT_YUV420P,
                &y_plane, y_stride,
                &u_plane, u_stride,
                &v_plane, v_stride,
                Some(pts_90khz as i64),
            )
        })
        .map_err(|e| format!("video encode failed: {e}"))?;

        for ef in encoded_frames {
            // TsMuxer expects Annex-B (with start codes). video-engine's
            // in-process encoders already emit Annex-B when global_header
            // is false.
            let ts_chunks = ts_muxer.mux_video(&ef.data, ef.pts as u64, ef.pts as u64, ef.keyframe);
            publish_chunks(ts_chunks, &mut seq_num, stats, per_input_tx, pts_90khz);
        }

        // ── Audio ── generate + encode ~1 frame-duration's worth of tone.
        if let Some(ref mut ctx) = audio_state {
            let samples_needed = ctx.sample_rate / fps as usize;
            let mut planar: Vec<Vec<f32>> = vec![Vec::with_capacity(samples_needed); ctx.channels];
            for _ in 0..samples_needed {
                let v = (ctx.phase.sin() as f32) * ctx.amplitude;
                for ch in planar.iter_mut() {
                    ch.push(v);
                }
                ctx.phase += ctx.step;
                if ctx.phase > std::f64::consts::TAU {
                    ctx.phase -= std::f64::consts::TAU;
                }
            }
            ctx.accumulate(planar);
            let frames_ready = ctx.encode_pending()?;
            for (adts, apts) in frames_ready {
                let ts_chunks = ts_muxer.mux_audio_pre_adts(&adts, apts);
                publish_chunks(ts_chunks, &mut seq_num, stats, per_input_tx, apts);
            }
        }

        frame_idx = frame_idx.saturating_add(1);
    }
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn publish_chunks(
    ts_chunks: Vec<Bytes>,
    seq_num: &mut u16,
    stats: &Arc<FlowStatsAccumulator>,
    per_input_tx: &broadcast::Sender<RtpPacket>,
    pts_90khz: u64,
) {
    // Bundle all TS packets from this video/audio frame into a single
    // RtpPacket — one TsMuxer call per frame produces ~dozens of 188 B
    // chunks, and publishing each one as its own RtpPacket saturates the
    // 2048-slot broadcast channel, starving the thumbnail / analyzer
    // subscribers and making the test pattern invisible downstream.
    // See `input_rtmp.rs` for the same bundling pattern.
    if ts_chunks.is_empty() {
        return;
    }
    let total_len: usize = ts_chunks.iter().map(|c| c.len()).sum();
    let mut combined = bytes::BytesMut::with_capacity(total_len);
    for chunk in &ts_chunks {
        combined.extend_from_slice(chunk);
    }
    let pkt = RtpPacket {
        data: combined.freeze(),
        sequence_number: *seq_num,
        rtp_timestamp: pts_90khz as u32,
        recv_time_us: crate::util::time::now_us(),
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
    };
    *seq_num = seq_num.wrapping_add(1);
    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(total_len as u64, Ordering::Relaxed);
    // Drop into the broadcast — a lagging subscriber sees RecvError::Lagged,
    // not our problem.
    let _ = per_input_tx.send(pkt);
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
#[allow(unused_imports, unreachable_code)]
fn select_video_backend() -> Option<video_codec::VideoEncoderCodec> {
    use video_codec::VideoEncoderCodec;
    // Return the first compiled-in backend. Preference: x264 (CPU,
    // widely available), x265, NVENC (NVIDIA hardware), QSV (Intel
    // hardware).
    #[cfg(feature = "video-encoder-x264")]
    { return Some(VideoEncoderCodec::X264); }
    #[cfg(all(feature = "video-encoder-x265", not(feature = "video-encoder-x264")))]
    { return Some(VideoEncoderCodec::X265); }
    #[cfg(all(
        feature = "video-encoder-nvenc",
        not(feature = "video-encoder-x264"),
        not(feature = "video-encoder-x265")
    ))]
    { return Some(VideoEncoderCodec::H264Nvenc); }
    #[cfg(all(
        feature = "video-encoder-qsv",
        not(feature = "video-encoder-x264"),
        not(feature = "video-encoder-x265"),
        not(feature = "video-encoder-nvenc")
    ))]
    { return Some(VideoEncoderCodec::H264Qsv); }
    None
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn backend_codec_string(codec: video_codec::VideoEncoderCodec) -> &'static str {
    use video_codec::VideoEncoderCodec;
    // Exhaustive matches for feature-gated variants trigger unused-import
    // warnings on some backend combos; the `use` above is intentional.
    match codec {
        VideoEncoderCodec::X264 => "x264",
        VideoEncoderCodec::X265 => "x265",
        VideoEncoderCodec::H264Nvenc => "h264_nvenc",
        VideoEncoderCodec::HevcNvenc => "hevc_nvenc",
        VideoEncoderCodec::H264Qsv => "h264_qsv",
        VideoEncoderCodec::HevcQsv => "hevc_qsv",
    }
}

/// Build SMPTE 75% colour bars as YUV420P planes (BT.601 TV range).
///
/// Returns `(y_plane, u_plane, v_plane, y_stride, u_stride, v_stride)`.
/// Chroma planes are width/2 × height/2 per YUV420P.
#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn build_smpte_bars_yuv420p(width: u32, height: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>, usize, usize, usize) {
    // SMPTE 75% bars, BT.601 TV-range YUV. 8 vertical bars left→right:
    // white (75%), yellow, cyan, green, magenta, red, blue, black.
    // Pre-computed (Y, U, V) triples.
    const BARS: [(u8, u8, u8); 8] = [
        (180, 128, 128), // 75% white
        (162, 44, 142),  // yellow
        (131, 156, 44),  // cyan
        (112, 72, 58),   // green
        (84, 184, 198),  // magenta
        (65, 100, 212),  // red
        (35, 212, 114),  // blue
        (16, 128, 128),  // black
    ];
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let ch = h / 2;
    let bar_w = (w + 7) / 8;

    let mut y = vec![16u8; w * h];
    let mut u = vec![128u8; cw * ch];
    let mut v = vec![128u8; cw * ch];

    // Y plane, full resolution.
    for row in 0..h {
        let y_row = &mut y[row * w..(row + 1) * w];
        for col in 0..w {
            let bar = (col / bar_w).min(7);
            y_row[col] = BARS[bar].0;
        }
    }
    // Chroma planes, width/2 × height/2. Each chroma sample covers a
    // 2×2 luma block; we pick the bar by matching column position.
    for row in 0..ch {
        let u_row = &mut u[row * cw..(row + 1) * cw];
        let v_row = &mut v[row * cw..(row + 1) * cw];
        for col in 0..cw {
            let bar = ((col * 2) / bar_w).min(7);
            u_row[col] = BARS[bar].1;
            v_row[col] = BARS[bar].2;
        }
    }

    (y, u, v, w, cw, cw)
}

/// 5×7 bitmap font for digits 0–9 and `:`. Each row encodes 5 columns in
/// the low 5 bits, MSB = leftmost pixel. Used for the timecode overlay.
#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
const TC_FONT: [[u8; 7]; 11] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110], // 0
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110], // 1
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111], // 2
    [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110], // 3
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010], // 4
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110], // 5
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110], // 6
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000], // 7
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110], // 8
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100], // 9
    [0b00000, 0b00110, 0b00110, 0b00000, 0b00110, 0b00110, 0b00000], // :
];

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn tc_glyph_index(ch: char) -> Option<usize> {
    match ch {
        '0'..='9' => Some((ch as u8 - b'0') as usize),
        ':' => Some(10),
        _ => None,
    }
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn fill_rect_y(y: &mut [u8], stride: usize, height: usize, x0: usize, y0: usize, w: usize, h: usize, luma: u8) {
    let x1 = (x0 + w).min(stride);
    let y1 = (y0 + h).min(height);
    for py in y0..y1 {
        let row = &mut y[py * stride..py * stride + x1];
        for px in &mut row[x0..] {
            *px = luma;
        }
    }
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn draw_glyph(y: &mut [u8], stride: usize, height: usize, ch: char, x0: usize, y0: usize, scale: usize, luma: u8) {
    let Some(idx) = tc_glyph_index(ch) else { return; };
    let glyph = &TC_FONT[idx];
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..5u8 {
            if (bits >> (4 - col)) & 1 != 0 {
                fill_rect_y(y, stride, height, x0 + col as usize * scale, y0 + row * scale, scale, scale, luma);
            }
        }
    }
}

/// Draw `HH:MM:SS:FF` centred near the bottom of the frame. Y plane only
/// — chroma underneath tints the digits, which is fine: the count is
/// still readable and motion proves liveness.
#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn draw_timecode(y: &mut [u8], width: usize, height: usize, frame_idx: u64, fps: u32) {
    let fps_u = fps.max(1) as u64;
    let ff = (frame_idx % fps_u) as u32;
    let total_sec = frame_idx / fps_u;
    let ss = (total_sec % 60) as u32;
    let mm = ((total_sec / 60) % 60) as u32;
    let hh = ((total_sec / 3600) % 100) as u32;
    let text = format!("{:02}:{:02}:{:02}:{:02}", hh, mm, ss, ff);

    let scale = (height / 90).max(2);
    let glyph_w = 5 * scale;
    let glyph_h = 7 * scale;
    let gap = scale;
    let total_w = text.len() * glyph_w + text.len().saturating_sub(1) * gap;
    if total_w >= width { return; }
    let x0 = (width - total_w) / 2;
    let y0 = height.saturating_sub(glyph_h + scale * 4);

    let pad = scale * 2;
    let bg_x0 = x0.saturating_sub(pad);
    let bg_y0 = y0.saturating_sub(pad);
    let bg_w = (total_w + 2 * pad).min(width - bg_x0);
    let bg_h = (glyph_h + 2 * pad).min(height - bg_y0);
    fill_rect_y(y, width, height, bg_x0, bg_y0, bg_w, bg_h, 16);

    let mut cx = x0;
    for ch in text.chars() {
        draw_glyph(y, width, height, ch, cx, y0, scale, 235);
        cx += glyph_w + gap;
    }
}

/// Solid white box that ping-pongs diagonally. Triangle wave in X and Y
/// with coprime-ish speeds so the trajectory covers the frame rather
/// than cycling on a short orbit.
#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn draw_bouncing_box(y: &mut [u8], width: usize, height: usize, frame_idx: u64) {
    let box_size = (height / 10).max(20);
    if box_size >= width || box_size >= height { return; }
    let travel_x = (width - box_size) as u64;
    let travel_y = (height - box_size) as u64;
    let speed_x = 5u64;
    let speed_y = 3u64;

    let tri = |t: u64, span: u64| -> usize {
        if span == 0 { return 0; }
        let m = t % (2 * span);
        (if m > span { 2 * span - m } else { m }) as usize
    };
    let pos_x = tri(frame_idx * speed_x, travel_x);
    let pos_y = tri(frame_idx * speed_y, travel_y);
    fill_rect_y(y, width, height, pos_x, pos_y, box_size, box_size, 235);
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
struct AudioContext {
    encoder: aac_audio::AacEncoder,
    sample_rate: usize,
    channels: usize,
    amplitude: f32,
    phase: f64,
    step: f64,
    frame_size: usize,
    accumulator: Vec<Vec<f32>>,
    accumulated: usize,
    pts_90k: u64,
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
impl AudioContext {
    fn accumulate(&mut self, planar: Vec<Vec<f32>>) {
        let mut added_per_channel = 0;
        for (ch, samples) in planar.into_iter().enumerate() {
            if ch < self.accumulator.len() {
                added_per_channel = samples.len();
                self.accumulator[ch].extend(samples);
            }
        }
        self.accumulated += added_per_channel;
    }

    /// Drain the accumulator frame-by-frame and return (ADTS bytes, pts) tuples.
    fn encode_pending(&mut self) -> Result<Vec<(Vec<u8>, u64)>, String> {
        let mut out = Vec::new();
        while self.accumulated >= self.frame_size {
            let mut frame_planar: Vec<Vec<f32>> = Vec::with_capacity(self.channels);
            for ch_buf in self.accumulator.iter_mut() {
                frame_planar.push(ch_buf.drain(..self.frame_size).collect());
            }
            self.accumulated -= self.frame_size;
            match self.encoder.encode_frame(&frame_planar) {
                Ok(encoded) => {
                    let pts = self.pts_90k;
                    let sr = self.sample_rate as u64;
                    if sr > 0 {
                        self.pts_90k = self
                            .pts_90k
                            .saturating_add((encoded.num_samples as u64) * 90_000 / sr);
                    }
                    out.push((encoded.bytes, pts));
                }
                Err(e) => {
                    return Err(format!("aac encode failed: {e}"));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn build_audio_encoder(tone_hz: f32, tone_dbfs: f32) -> Result<AudioContext, String> {
    let sample_rate = 48_000usize;
    let channels = 2usize;
    let cfg = aac_codec::EncoderConfig {
        profile: aac_codec::AacProfile::AacLc,
        sample_rate: sample_rate as u32,
        channels: channels as u8,
        bitrate: 128_000,
        afterburner: true,
        sbr_signaling: aac_codec::SbrSignaling::default(),
        transport: aac_codec::TransportType::Adts,
    };
    let encoder = aac_audio::AacEncoder::open(&cfg)
        .map_err(|e| format!("fdk-aac open failed: {e}"))?;
    let frame_size = encoder.frame_size() as usize;

    // Convert dBFS → linear amplitude (clamped to avoid full-scale clip).
    let clamped_db = tone_dbfs.clamp(-60.0, 0.0);
    let amplitude = 10f32.powf(clamped_db / 20.0).min(0.95);
    let step = std::f64::consts::TAU * (tone_hz as f64) / sample_rate as f64;

    Ok(AudioContext {
        encoder,
        sample_rate,
        channels,
        amplitude,
        phase: 0.0,
        step,
        frame_size,
        accumulator: vec![Vec::with_capacity(frame_size); channels],
        accumulated: 0,
        pts_90k: 0,
    })
}
