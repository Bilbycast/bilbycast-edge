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
//! Requires compile-time features `media-codecs` (for the
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
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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
    #[cfg(not(all(feature = "media-codecs", feature = "fdk-aac")))]
    {
        events.emit_flow(
            EventSeverity::Critical,
            category::FLOW,
            format!(
                "Test-pattern input '{input_id}' requires the 'media-codecs' and 'fdk-aac' features — rebuild the edge with those enabled"
            ),
            &flow_id,
        );
        cancel.cancelled().await;
    }
}

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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

    // Build the ingress transcoder. All three fields None ⇒ passthrough
    // (no codec state, no scratch buffers). Errors degrade to passthrough
    // with a Critical event so the operator notices.
    let mut transcoder = match crate::engine::input_transcode::InputTranscoder::new(
        config.audio_encode.as_ref(),
        config.transcode.as_ref(),
        config.video_encode.as_ref(),
        None,
    ) {
        Ok(t) => {
            if let Some(ref t) = t {
                tracing::info!("Test-pattern input: ingress transcode active — {}", t.describe());
            }
            t
        }
        Err(e) => {
            events.emit_flow(
                EventSeverity::Critical,
                category::FLOW,
                format!("Test-pattern input transcode disabled: {e}"),
                flow_id,
            );
            None
        }
    };
    crate::engine::input_transcode::register_ingress_stats(
        stats.as_ref(),
        input_id,
        transcoder.as_mut(),
        config.audio_encode.as_ref(),
        config.video_encode.as_ref(),
        &events,
    );
    // Synthetic-TS — TsMuxer handles pid_overrides. No `passthrough_clock`
    // on TestPatternInputConfig: the pattern generator already controls
    // every PTS it emits, so there's no source clock to re-anchor against.
    let av_skew_for_post = stats.as_ref().av_skew_reporter_for_input(input_id);
    let mut post = crate::engine::input_post_process::InputPostProcess::from_config(
        &crate::engine::input_post_process::InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: None,
            pid_map: config.pid_map.as_ref(),
            passthrough_clock: false,
            av_sync_pacer: None,
            pcr_jump_signal: None,
            av_skew: Some(&av_skew_for_post),
        },
    );
    if let Some(ref _p) = post {
        tracing::info!("Test-pattern input: ingress post-process active");
    }

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
        source_video_pid: None,
        hw_decode: None,
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

    // Build the audio encoder if enabled. Channel-ident announcements only
    // apply when the A/V-sync beep isn't claiming the audio (av_sync_marker
    // takes precedence — a sync test needs the pip on every channel).
    let channel_ident = !config.av_sync_marker
        && matches!(
            config.audio_content,
            crate::config::models::TestPatternAudioContent::ChannelIdent
        );
    let audio_ctx = if config.audio_enabled {
        Some(build_audio_encoder(
            config.tone_hz,
            config.tone_dbfs,
            config.audio_channels.max(1) as usize,
            channel_ident,
        )?)
    } else {
        None
    };

    let mut ts_muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
    if let Some(po) = config.pid_overrides.as_ref() {
        if let Some(entry) = po.get(&1) {
                ts_muxer.set_pids(entry.pmt_pid, entry.video_pid, entry.audio_pid, entry.pcr_pid);
            }
    }
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

    // A/V sync marker geometry. Burst length ≈ 80 ms (1 video frame on
    // 12 fps and floor; the .max(3) keeps it visible at low rates and
    // gives the AAC encoder enough samples to land at least one frame
    // boundary inside the burst regardless of phase).
    let burst_frames: u64 = ((fps as u64 * 80) / 1000).max(3);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                return Ok(());
            }
            _ = ticker.tick() => {}
        }

        let pts_90khz = frame_idx.saturating_mul(90_000) / fps as u64;
        let frame_in_second = frame_idx % fps as u64;
        let burst_active = config.av_sync_marker && frame_in_second < burst_frames;

        // Refresh Y plane from the static bars and overlay motion so
        // downstream viewers can tell the feed is live, not frozen.
        y_plane.copy_from_slice(&y_base);
        draw_bouncing_box(&mut y_plane, width as usize, height as usize, frame_idx);
        draw_timecode(&mut y_plane, width as usize, height as usize, frame_idx, fps);
        if let Some(id) = config.screen_id.as_deref() {
            if !id.is_empty() {
                draw_screen_id(&mut y_plane, width as usize, height as usize, id);
            }
        }
        if config.av_sync_marker {
            match config.av_sync_style {
                crate::config::models::TestPatternAvSyncStyle::Flash => {
                    draw_av_sync_marker(&mut y_plane, width as usize, height as usize, burst_active);
                }
                crate::config::models::TestPatternAvSyncStyle::Sweep => {
                    draw_av_sync_sweep(
                        &mut y_plane,
                        width as usize,
                        height as usize,
                        frame_in_second,
                        fps,
                        burst_active,
                    );
                }
            }
        }

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
            publish_chunks(ts_chunks, &mut seq_num, stats, per_input_tx, pts_90khz, &mut transcoder, &mut post);
        }

        // ── Audio ── generate + encode ~1 frame-duration's worth of tone.
        if let Some(ref mut ctx) = audio_state {
            let samples_needed = ctx.sample_rate / fps as usize;
            // Burst envelope: when av_sync_marker is on, gate the sine
            // into the same window the flash uses, with a raised-cosine
            // ramp at each edge so the burst doesn't add broadband click
            // energy (which would taint LUFS / true-peak measurements
            // downstream).
            let total_burst_samples = (burst_frames as usize) * samples_needed;
            let ramp_samples = (ctx.sample_rate * 2 / 1000).min(total_burst_samples / 4).max(1);
            let pos_base = (frame_in_second as usize) * samples_needed;
            let mut planar: Vec<Vec<f32>> = vec![Vec::with_capacity(samples_needed); ctx.channels];
            for i in 0..samples_needed {
                if config.av_sync_marker {
                    // Sync mode: gated sine burst, identical on every
                    // channel — the pip the operator aligns to the flash /
                    // sweep dot.
                    let env: f32 = if !burst_active {
                        0.0
                    } else {
                        let pos = pos_base + i;
                        if pos < ramp_samples {
                            0.5 * (1.0 - (std::f32::consts::PI * pos as f32 / ramp_samples as f32).cos())
                        } else if pos + ramp_samples >= total_burst_samples {
                            let tail = total_burst_samples.saturating_sub(pos);
                            0.5 * (1.0 - (std::f32::consts::PI * tail as f32 / ramp_samples as f32).cos())
                        } else {
                            1.0
                        }
                    };
                    let v = (ctx.phase.sin() as f32) * ctx.amplitude * env;
                    for ch in planar.iter_mut() {
                        ch.push(v);
                    }
                    ctx.phase += ctx.step;
                    if ctx.phase > std::f64::consts::TAU {
                        ctx.phase -= std::f64::consts::TAU;
                    }
                } else if let Some(ident) = ctx.ident.as_mut() {
                    // Channel-ident: each channel reads from its own looped
                    // announcement buffer (voice digit or counted beeps),
                    // all sharing one period so they stay phase-aligned.
                    let idx = ident.pos % ident.period;
                    for (ch, buf) in planar.iter_mut().zip(ident.buffers.iter()) {
                        ch.push(buf.get(idx).copied().unwrap_or(0.0));
                    }
                    ident.pos = ident.pos.wrapping_add(1);
                } else {
                    // Continuous line-up tone, identical on every channel.
                    let v = (ctx.phase.sin() as f32) * ctx.amplitude;
                    for ch in planar.iter_mut() {
                        ch.push(v);
                    }
                    ctx.phase += ctx.step;
                    if ctx.phase > std::f64::consts::TAU {
                        ctx.phase -= std::f64::consts::TAU;
                    }
                }
            }
            ctx.accumulate(planar);
            let frames_ready = ctx.encode_pending()?;
            for (adts, apts) in frames_ready {
                let ts_chunks = ts_muxer.mux_audio_pre_adts(&adts, apts);
                publish_chunks(ts_chunks, &mut seq_num, stats, per_input_tx, apts, &mut transcoder, &mut post);
            }
        }

        frame_idx = frame_idx.saturating_add(1);
    }
}

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn publish_chunks(
    ts_chunks: Vec<Bytes>,
    seq_num: &mut u16,
    stats: &Arc<FlowStatsAccumulator>,
    per_input_tx: &broadcast::Sender<RtpPacket>,
    pts_90khz: u64,
    transcoder: &mut Option<crate::engine::input_transcode::InputTranscoder>,
    post: &mut Option<crate::engine::input_post_process::InputPostProcess>,
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
        sender_timestamp_us: None,
    };
    *seq_num = seq_num.wrapping_add(1);
    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(total_len as u64, Ordering::Relaxed);
    // Route through the optional ingress transcoder. None ⇒ passthrough
    // (single broadcast send, no copy). Some ⇒ block_in_place re-encode
    // before publish. A lagging subscriber sees RecvError::Lagged inside
    // the helper — not our problem.
    crate::engine::input_transcode::publish_input_packet_with_post(transcoder, post, per_input_tx, pkt);
}

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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
        VideoEncoderCodec::H264Vaapi => "h264_vaapi",
        VideoEncoderCodec::HevcVaapi => "hevc_vaapi",
    }
}

/// Build SMPTE 75% colour bars as YUV420P planes (BT.601 TV range).
///
/// Returns `(y_plane, u_plane, v_plane, y_stride, u_stride, v_stride)`.
/// Chroma planes are width/2 × height/2 per YUV420P.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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

/// 5×7 bitmap font for A–Z. Same row encoding as [`TC_FONT`] (low 5 bits,
/// MSB = leftmost pixel). Used by the screen-ID overlay.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
const LETTER_FONT: [[u8; 7]; 26] = [
    [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001], // A
    [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110], // B
    [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110], // C
    [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110], // D
    [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111], // E
    [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000], // F
    [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111], // G
    [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001], // H
    [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110], // I
    [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100], // J
    [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001], // K
    [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111], // L
    [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001], // M
    [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001], // N
    [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110], // O
    [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000], // P
    [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101], // Q
    [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001], // R
    [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110], // S
    [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100], // T
    [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110], // U
    [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100], // V
    [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001], // W
    [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001], // X
    [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100], // Y
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111], // Z
];

/// Resolve a character to its 5×7 glyph rows. Covers digits, `:`, A–Z, and
/// a few symbols; unknown characters return `None` (caller skips them).
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn glyph_5x7(ch: char) -> Option<[u8; 7]> {
    match ch {
        '0'..='9' => Some(TC_FONT[(ch as u8 - b'0') as usize]),
        ':' => Some(TC_FONT[10]),
        'A'..='Z' => Some(LETTER_FONT[(ch as u8 - b'A') as usize]),
        ' ' => Some([0; 7]),
        '-' => Some([0, 0, 0, 0b11111, 0, 0, 0]),
        '.' => Some([0, 0, 0, 0, 0, 0b00110, 0b00110]),
        _ => None,
    }
}

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn draw_glyph(y: &mut [u8], stride: usize, height: usize, ch: char, x0: usize, y0: usize, scale: usize, luma: u8) {
    let Some(glyph) = glyph_5x7(ch) else { return; };
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
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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

/// A/V-sync marker patch. Always paints a black "well" in the upper-
/// right corner so the eye / scope knows where to look; on burst frames
/// the inner square goes bright. Paired with the audio tone burst, the
/// offset between the audible pip and the visible flash reads off
/// directly as A/V skew.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn draw_av_sync_marker(y: &mut [u8], width: usize, height: usize, flash_on: bool) {
    let size = (height / 8).max(48);
    if size >= width || size >= height { return; }
    let pad = (height / 100).max(4);
    let x0 = width.saturating_sub(size + pad);
    let y0 = pad;
    fill_rect_y(y, width, height, x0, y0, size, size, 16);
    let border = (size / 12).max(2);
    let inner_size = size.saturating_sub(2 * border);
    let inner_luma = if flash_on { 235 } else { 16 };
    fill_rect_y(y, width, height, x0 + border, y0 + border, inner_size, inner_size, inner_luma);
}

/// Burned-in identifier label, drawn large + centred near the top of the
/// frame with a black backing well. Uppercased; characters the 5×7 font
/// can't draw are skipped. Scale shrinks to keep the label inside the frame.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn draw_screen_id(y: &mut [u8], width: usize, height: usize, label: &str) {
    let text: String = label
        .chars()
        .flat_map(|c| c.to_uppercase())
        .filter(|c| glyph_5x7(*c).is_some())
        .collect();
    if text.is_empty() {
        return;
    }
    let n = text.chars().count();
    // Aim for a prominent label (~1/10 of frame height) but shrink to fit.
    let mut scale = (height / 72).max(3);
    while scale > 1 {
        let total_w = n * 5 * scale + n.saturating_sub(1) * scale;
        if total_w + scale * 8 <= width {
            break;
        }
        scale -= 1;
    }
    let glyph_w = 5 * scale;
    let glyph_h = 7 * scale;
    let gap = scale;
    let total_w = n * glyph_w + n.saturating_sub(1) * gap;
    if total_w >= width {
        return;
    }
    let x0 = (width - total_w) / 2;
    let y0 = (height / 12).max(scale * 2);
    let pad = scale * 2;
    let bg_x0 = x0.saturating_sub(pad);
    let bg_y0 = y0.saturating_sub(pad);
    let bg_w = (total_w + 2 * pad).min(width.saturating_sub(bg_x0));
    let bg_h = (glyph_h + 2 * pad).min(height.saturating_sub(bg_y0));
    fill_rect_y(y, width, height, bg_x0, bg_y0, bg_w, bg_h, 16);
    let mut cx = x0;
    for ch in text.chars() {
        draw_glyph(y, width, height, ch, cx, y0, scale, 235);
        cx += glyph_w + gap;
    }
}

/// A/V-sync "sweep" marker: a dot orbits a ring once per second, and the
/// beep fires as it crosses the 12 o'clock notch. The operator reads skew
/// from where the dot sits when the pip is heard — more intuitive than a
/// flash. Drawn upper-right so it stays clear of the centre box, the bottom
/// timecode, and the top screen-ID label.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn draw_av_sync_sweep(
    y: &mut [u8],
    width: usize,
    height: usize,
    frame_in_second: u64,
    fps: u32,
    flash_on: bool,
) {
    let r = (height.min(width) / 6).max(20) as i64;
    let w = width as i64;
    let h = height as i64;
    if r * 2 + 8 >= w || r * 2 + 8 >= h {
        return;
    }
    let cx = (w * 3 / 4).clamp(r + 4, w - r - 4);
    let cy = (h / 2).clamp(r + 4, h - r - 4);
    let plot = |buf: &mut [u8], px: i64, py: i64, luma: u8| {
        if px >= 0 && px < w && py >= 0 && py < h {
            buf[(py as usize) * width + (px as usize)] = luma;
        }
    };
    // Ring outline (2 px) + a bright notch at 12 o'clock (the beep mark).
    let steps = ((2.0 * std::f64::consts::PI * r as f64) as i64).max(64);
    for s in 0..steps {
        let a = 2.0 * std::f64::consts::PI * (s as f64) / (steps as f64);
        let px = cx + (r as f64 * a.sin()) as i64;
        let py = cy - (r as f64 * a.cos()) as i64;
        plot(y, px, py, 180);
        plot(y, px + 1, py, 180);
    }
    for d in 0..(r / 4).max(1) {
        plot(y, cx, cy - r + d, 235);
        plot(y, cx + 1, cy - r + d, 235);
    }
    // Dot sweeps once per second; angle 0 = 12 o'clock, clockwise.
    let theta = 2.0 * std::f64::consts::PI * (frame_in_second as f64) / (fps.max(1) as f64);
    let dx = cx + (r as f64 * theta.sin()) as i64;
    let dy = cy - (r as f64 * theta.cos()) as i64;
    let dot_r = (r / 7).max(3);
    let luma = if flash_on { 235 } else { 200 };
    for oy in -dot_r..=dot_r {
        for ox in -dot_r..=dot_r {
            if ox * ox + oy * oy <= dot_r * dot_r {
                plot(y, dx + ox, dy + oy, luma);
            }
        }
    }
}

/// Solid white box that ping-pongs diagonally. Triangle wave in X and Y
/// with coprime-ish speeds so the trajectory covers the frame rather
/// than cycling on a short orbit.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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
    /// Per-channel looped number-announcement buffers. `Some` only in
    /// channel-ident mode (see [`build_ident_bank`]).
    ident: Option<IdentBank>,
}

/// Per-channel "say the channel number" loop sources. Every channel's
/// buffer is the same `period` length and starts its announcement at the
/// top of the period, so all channels stay phase-aligned and a single
/// running `pos` indexes them all.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
struct IdentBank {
    buffers: Vec<Vec<f32>>,
    period: usize,
    pos: usize,
}

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
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

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn build_audio_encoder(
    tone_hz: f32,
    tone_dbfs: f32,
    channels: usize,
    channel_ident: bool,
) -> Result<AudioContext, String> {
    let sample_rate = 48_000usize;
    // Validation already constrains this to {1,2,6,8}; clamp defensively so
    // a stray value can never reach the encoder's channel-mode mapping.
    let channels = channels.clamp(1, 8);
    // Scale bitrate with channel count (128 kbps for stereo line-up; more
    // for 5.1 / 7.1 so each bed channel keeps a sane allocation).
    let bitrate = 64_000u32
        .saturating_mul(channels as u32)
        .clamp(96_000, 512_000);
    let cfg = aac_codec::EncoderConfig {
        profile: aac_codec::AacProfile::AacLc,
        sample_rate: sample_rate as u32,
        channels: channels as u8,
        bitrate,
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

    let ident = if channel_ident {
        Some(build_ident_bank(channels, sample_rate, amplitude))
    } else {
        None
    };

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
        ident,
    })
}

/// Resolve the directory operators drop per-digit voice clips into.
/// `BILBYCAST_TESTGEN_VOICE_DIR` overrides; otherwise `<media_dir>/testgen_voice/`.
/// Clips are named `1.wav` … `8.wav` (the channel number). Missing clips
/// fall back to counted beeps, so the feature works before any are added.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn testgen_voice_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("BILBYCAST_TESTGEN_VOICE_DIR") {
        if !d.trim().is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    crate::media::media_dir().join("testgen_voice")
}

/// Build the per-channel announcement loop. Channel `i` (0-based) announces
/// the number `i + 1`: a spoken-digit voice clip if `<dir>/<n>.wav` exists,
/// otherwise `n` counted beeps. All channels share one period (longest
/// utterance + a gap, rounded to whole seconds, ≥ 2 s) so they loop in lock-step.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn build_ident_bank(channels: usize, sample_rate: usize, amplitude: f32) -> IdentBank {
    let dir = testgen_voice_dir();
    let mut contents: Vec<Vec<f32>> = Vec::with_capacity(channels);
    let mut voice_hits = 0usize;
    let mut override_hits = 0usize;
    for ch in 0..channels {
        let digit = ch + 1;
        // Resolution: runtime override dir → compiled-in default → beeps.
        let clip = match load_voice_clip(&dir, digit, sample_rate, amplitude) {
            Some(v) => {
                override_hits += 1;
                Some(v)
            }
            None => embedded_voice_clip(digit, sample_rate, amplitude),
        };
        match clip {
            Some(v) => {
                voice_hits += 1;
                contents.push(v);
            }
            None => contents.push(render_counted_beeps(digit, sample_rate, amplitude)),
        }
    }
    tracing::info!(
        "Test-pattern channel-ident: {channels} channel(s); {voice_hits} voice clip(s) ({override_hits} from override dir {}, rest built-in), {} channel(s) on counted-beep fallback",
        dir.display(),
        channels - voice_hits
    );

    let gap = (sample_rate as f64 * 0.6) as usize;
    let one_sec = sample_rate.max(1);
    let max_len = contents.iter().map(|c| c.len()).max().unwrap_or(0) + gap;
    let mut period = max_len.div_ceil(one_sec) * one_sec;
    if period < 2 * one_sec {
        period = 2 * one_sec;
    }
    let buffers: Vec<Vec<f32>> = contents
        .into_iter()
        .map(|mut c| {
            c.resize(period, 0.0);
            c
        })
        .collect();
    IdentBank {
        buffers,
        period,
        pos: 0,
    }
}

/// `digit` short 1 kHz beeps (raised-cosine edges to avoid clicks),
/// separated by short gaps — the counted-beep channel identifier.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn render_counted_beeps(digit: usize, sample_rate: usize, amplitude: f32) -> Vec<f32> {
    let beep_n = (sample_rate * 150 / 1000).max(1);
    let gap_n = sample_rate * 120 / 1000;
    let ramp = (sample_rate * 5 / 1000).max(1);
    let freq = 1000.0f64;
    let mut out: Vec<f32> = Vec::with_capacity(digit * (beep_n + gap_n));
    for b in 0..digit {
        for i in 0..beep_n {
            let env = if i < ramp {
                0.5 * (1.0 - (std::f64::consts::PI * i as f64 / ramp as f64).cos())
            } else if i + ramp >= beep_n {
                let tail = beep_n - i;
                0.5 * (1.0 - (std::f64::consts::PI * tail as f64 / ramp as f64).cos())
            } else {
                1.0
            };
            let t = i as f64 / sample_rate as f64;
            out.push(((2.0 * std::f64::consts::PI * freq * t).sin() * env) as f32 * amplitude);
        }
        if b + 1 < digit {
            out.extend(std::iter::repeat(0.0f32).take(gap_n));
        }
    }
    out
}

/// Peak-normalise a clip to the configured level so the voice sits at
/// roughly the same loudness as the tone / beeps regardless of how it was
/// recorded.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn peak_normalize(pcm: &mut [f32], amplitude: f32) {
    let peak = pcm.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    if peak > 1e-6 {
        let g = amplitude / peak;
        for s in pcm.iter_mut() {
            *s *= g;
        }
    }
}

/// Load a per-digit **override** clip from the runtime voice directory,
/// decoded + resampled to 48 kHz mono and peak-normalised. Returns `None`
/// (→ built-in voice, then beeps) when the file is absent or unreadable.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn load_voice_clip(
    dir: &std::path::Path,
    digit: usize,
    sample_rate: usize,
    amplitude: f32,
) -> Option<Vec<f32>> {
    let path = dir.join(format!("{digit}.wav"));
    let bytes = std::fs::read(&path).ok()?;
    match decode_wav(&bytes, sample_rate) {
        Ok(mut pcm) => {
            peak_normalize(&mut pcm, amplitude);
            Some(pcm)
        }
        Err(e) => {
            tracing::warn!(
                "Test-pattern: voice clip {} ignored ({e}); falling back to the built-in voice / beeps for that channel",
                path.display()
            );
            None
        }
    }
}

/// Built-in spoken-digit voice prompts, compiled into the binary so
/// channel-ident works with zero setup on a fresh install. Digits 1–8 (the
/// max channel count); operators override any of them by dropping `<n>.wav`
/// into the runtime voice directory.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn embedded_voice_bytes(digit: usize) -> Option<&'static [u8]> {
    let b: &'static [u8] = match digit {
        1 => include_bytes!("../../assets/testgen_voice/1.wav"),
        2 => include_bytes!("../../assets/testgen_voice/2.wav"),
        3 => include_bytes!("../../assets/testgen_voice/3.wav"),
        4 => include_bytes!("../../assets/testgen_voice/4.wav"),
        5 => include_bytes!("../../assets/testgen_voice/5.wav"),
        6 => include_bytes!("../../assets/testgen_voice/6.wav"),
        7 => include_bytes!("../../assets/testgen_voice/7.wav"),
        8 => include_bytes!("../../assets/testgen_voice/8.wav"),
        _ => return None,
    };
    Some(b)
}

/// Decode + normalise the compiled-in voice prompt for `digit`.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn embedded_voice_clip(digit: usize, sample_rate: usize, amplitude: f32) -> Option<Vec<f32>> {
    let bytes = embedded_voice_bytes(digit)?;
    match decode_wav(bytes, sample_rate) {
        Ok(mut pcm) => {
            peak_normalize(&mut pcm, amplitude);
            Some(pcm)
        }
        Err(e) => {
            // Our own asset — a decode failure here is a build/asset bug.
            tracing::error!("Test-pattern: built-in voice clip {digit} failed to decode ({e})");
            None
        }
    }
}

/// Minimal RIFF/WAVE decoder → 48 kHz mono f32. Handles 16/24-bit PCM and
/// 32-bit float, mono or multi (downmixed by averaging), any sample rate
/// (linear-resampled). Deliberately tiny — voice idents are tiny files.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
fn decode_wav(bytes: &[u8], target_sr: usize) -> Result<Vec<f32>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, sr, bits)
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let sz = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]])
            as usize;
        let body_start = pos + 8;
        let body_end = body_start.saturating_add(sz).min(bytes.len());
        if id == b"fmt " && body_end - body_start >= 16 {
            let b = &bytes[body_start..body_end];
            fmt = Some((
                u16::from_le_bytes([b[0], b[1]]),
                u16::from_le_bytes([b[2], b[3]]),
                u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                u16::from_le_bytes([b[14], b[15]]),
            ));
        } else if id == b"data" {
            data = Some(&bytes[body_start..body_end]);
        }
        // Chunks are word-aligned (pad byte when size is odd).
        pos = body_start + sz + (sz & 1);
    }
    let (af, channels, sr, bits) = fmt.ok_or("missing fmt chunk")?;
    let data = data.ok_or("missing data chunk")?;
    let channels = (channels.max(1)) as usize;
    let mut mono: Vec<f32> = Vec::new();
    match (af, bits) {
        (1, 16) => {
            for f in data.chunks_exact(2 * channels) {
                let mut acc = 0.0f32;
                for c in 0..channels {
                    acc += i16::from_le_bytes([f[c * 2], f[c * 2 + 1]]) as f32 / 32768.0;
                }
                mono.push(acc / channels as f32);
            }
        }
        (1, 24) => {
            for f in data.chunks_exact(3 * channels) {
                let mut acc = 0.0f32;
                for c in 0..channels {
                    let mut v = (f[c * 3] as i32) | ((f[c * 3 + 1] as i32) << 8) | ((f[c * 3 + 2] as i32) << 16);
                    if v & 0x0080_0000 != 0 {
                        v |= !0x00FF_FFFF;
                    }
                    acc += v as f32 / 8_388_608.0;
                }
                mono.push(acc / channels as f32);
            }
        }
        (3, 32) => {
            for f in data.chunks_exact(4 * channels) {
                let mut acc = 0.0f32;
                for c in 0..channels {
                    acc += f32::from_le_bytes([f[c * 4], f[c * 4 + 1], f[c * 4 + 2], f[c * 4 + 3]]);
                }
                mono.push(acc / channels as f32);
            }
        }
        _ => {
            return Err(format!(
                "unsupported WAV (format={af}, bits={bits}); use 16/24-bit PCM or 32-bit float"
            ));
        }
    }
    if mono.is_empty() {
        return Err("empty audio data".into());
    }
    let src_sr = sr.max(1) as usize;
    if src_sr == target_sr {
        return Ok(mono);
    }
    // Linear resample to the target rate.
    let out_len = mono.len() * target_sr / src_sr;
    let mut out = Vec::with_capacity(out_len);
    let last = mono.len() - 1;
    for i in 0..out_len {
        let p = i as f64 * src_sr as f64 / target_sr as f64;
        let idx = p.floor() as usize;
        let frac = (p - idx as f64) as f32;
        let a = mono[idx.min(last)];
        let b = mono[(idx + 1).min(last)];
        out.push(a + (b - a) * frac);
    }
    Ok(out)
}
