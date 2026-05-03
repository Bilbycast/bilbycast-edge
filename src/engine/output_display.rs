// Local-display output — Linux-only. Subscribes to the flow's broadcast
// bus, demuxes the chosen program / audio PID, decodes video + audio,
// and renders to a KMS connector + ALSA device.
//
// Architecture (one task graph per running output):
//
//     RtpPacket broadcast::Receiver
//          │ .subscribe()
//          ▼
//     run_display_output (top tokio task — orchestrates lifecycle)
//          ├── demux_decode_loop (block_in_place — TS in, frames out)
//          │       ├── TsDemuxer
//          │       ├── VideoDecoder (persistent, H.264/HEVC)
//          │       ├── AacDecoder (persistent, fdk-aac)
//          │       └── AudioDecoder (persistent, libavcodec for MP2/AC3/EAC3/Opus)
//          ├── display_loop (block_in_place — KMS page-flip)
//          └── audio_loop (block_in_place — ALSA blocking writes; *master clock*)
//
// A/V sync = audio is master. ALSA `writei` block-time advances
// `AudioClock`; the display loop dup/drops video to track that clock.
//
// Drop semantics: broadcast `Lagged(n)` increments `packets_dropped`
// and flushes both decoders so the next IDR / sync frame is the new
// anchor. Display task drops video frames whose PTS is more than one
// frame-period behind the audio clock (`frames_dropped_late++`) and
// holds the previous frame for one more vsync when the next decoded
// frame is too far ahead (`frames_repeated++`). Audio xrun → ALSA
// `prepare()` and continue (`audio_underruns++`). The flow input is
// **never** blocked.

#![cfg(all(feature = "display", target_os = "linux"))]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use aac_audio::AacDecoder;
use video_codec::AudioDecoderCodec;
use video_engine::{
    AudioDecoder as FfAudioDecoder, ScalerDstFormat, VideoCodec, VideoDecoder, VideoScaler,
};

use crate::config::models::{DisplayOutputConfig, DisplayScalingMode};
use crate::display::audio_bars::{new_shared_meter, MeterSnapshot, SharedMeter};
use crate::display::audio_meter::spawn_audio_meter;
use crate::display::{audio::AudioBackend, clock::AudioClock, kms::KmsDisplay};
use crate::engine::packet::RtpPacket;
use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::{DisplayStatsCounters, OutputStatsAccumulator};

const MPSC_VIDEO_DEPTH: usize = 8;
const MPSC_AUDIO_DEPTH: usize = 64;

// ── Public spawner ────────────────────────────────────────────────

/// Spawn the orchestrator task for one `display` output. The returned
/// `JoinHandle` exits when either the parent flow's cancellation token
/// fires or the orchestrator hits a fatal error (modeset rejected,
/// connector vanished, ALSA refused to open).
pub fn spawn_display_output(
    config: DisplayOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();
    tokio::spawn(async move {
        if let Err(e) = run_display_output(
            config,
            &mut rx,
            output_stats,
            cancel,
            event_sender,
            flow_id,
        )
        .await
        {
            tracing::error!("display output exited with error: {e}");
        }
    })
}

// ── Top orchestrator ──────────────────────────────────────────────

async fn run_display_output(
    config: DisplayOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> Result<()> {
    // 1. Open KMS at the connector's preferred mode. The actual mode the
    //    panel runs at is decided by `config.scaling_mode`:
    //    - `MatchSource` (default): the display loop re-modesets to the
    //      smallest mode covering the source on the first decoded frame
    //      (and again whenever the source dims change).
    //    - `MonitorNative`: the display loop holds the panel at the
    //      preferred mode opened here and lets libswscale upscale the
    //      source.
    //    The deprecated `config.resolution` / `config.refresh_hz` are
    //    accepted by the deserializer for backward-compat round-trip but
    //    no longer drive the mode-set.
    let kms = match tokio::task::spawn_blocking({
        let device = config.device.clone();
        move || KmsDisplay::open(&device, None, None, None)
    })
    .await
    .map_err(|e| anyhow::anyhow!("kms join: {e}"))?
    {
        Ok(k) => k,
        Err(e) => {
            let msg = e.to_string();
            let code = classify_kms_error(&msg);
            emit_event(
                &event_sender,
                EventSeverity::Critical,
                code,
                &flow_id,
                &config.id,
                &format!("display open failed: {msg}"),
            );
            return Err(e);
        }
    };

    let chosen_resolution = format!("{}x{}", kms.width(), kms.height());
    let chosen_refresh = kms.refresh_hz();

    // 2. Build the lock-free counters. Stats-handle registration is
    //    deferred to the display child so an `auto`-resolution output
    //    publishes the post-auto-match resolution rather than the
    //    placeholder we opened KMS with.
    let counters = Arc::new(DisplayStatsCounters::default());
    let audio_codec_label: &'static str = if config.audio_device.as_deref().unwrap_or("").is_empty()
    {
        "none"
    } else {
        "unknown"
    };

    let scaling_label = match config.scaling_mode {
        DisplayScalingMode::MatchSource => "auto-match pending first frames",
        DisplayScalingMode::MonitorNative => "monitor-native, source scaled to panel",
    };
    emit_event(
        &event_sender,
        EventSeverity::Info,
        "display_started",
        &flow_id,
        &config.id,
        &format!(
            "display started on {} ({}@{}Hz, {}){}",
            config.device,
            chosen_resolution,
            chosen_refresh,
            scaling_label,
            config
                .audio_device
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|a| format!(" + audio '{a}'"))
                .unwrap_or_default(),
        ),
    );

    // 3. Wire up channels + audio clock + program-start anchor.
    let clock = Arc::new(AudioClock::new());
    let program_start = Instant::now();
    let (vtx, vrx) = mpsc::channel::<VideoFrame>(MPSC_VIDEO_DEPTH);
    let (atx, arx) = mpsc::channel::<AudioBlock>(MPSC_AUDIO_DEPTH);

    // Optional metering child — independent multi-PID audio decoder
    // that updates a shared `MeterSnapshot` consumed by `display_loop`.
    let meter_snapshot: Option<SharedMeter> = if config.show_audio_bars {
        Some(new_shared_meter())
    } else {
        None
    };
    let meter_handle = meter_snapshot.as_ref().map(|snap| {
        spawn_audio_meter(
            rx.resubscribe(),
            config.program_number,
            Arc::clone(snap),
            cancel.child_token(),
        )
    });

    // Demux + decode child — owns the broadcast subscriber.
    let demux_cancel = cancel.child_token();
    let demux_counters = Arc::clone(&counters);
    let demux_event_sender = event_sender.clone();
    let demux_flow_id = flow_id.clone();
    let demux_output_id = config.id.clone();
    let mut demux_rx = rx.resubscribe();
    let demux_program = config.program_number;
    let demux_track = config.audio_track_index;
    let demux_handle = tokio::task::spawn_blocking(move || {
        demux_decode_loop(
            &mut demux_rx,
            vtx,
            atx,
            demux_program,
            demux_track,
            demux_counters,
            demux_cancel,
            demux_event_sender,
            demux_flow_id,
            demux_output_id,
        );
    });

    // Display child — owns the KMS card and the back/front framebuffers.
    let display_cancel = cancel.child_token();
    let display_counters = Arc::clone(&counters);
    let display_clock = Arc::clone(&clock);
    let display_output_stats = Arc::clone(&output_stats);
    let display_event_sender = event_sender.clone();
    let display_flow_id = flow_id.clone();
    let display_output_id = config.id.clone();
    let display_meter = meter_snapshot.as_ref().map(Arc::clone);
    let display_scaling_mode = config.scaling_mode;
    let display_handle = tokio::task::spawn_blocking(move || {
        display_loop(
            kms,
            vrx,
            display_clock,
            display_counters,
            display_cancel,
            display_output_stats,
            audio_codec_label,
            display_event_sender,
            display_flow_id,
            display_output_id,
            display_meter,
            display_scaling_mode,
        );
    });

    // Audio child — owns the ALSA PCM. Skipped entirely when muted.
    let audio_cancel = cancel.child_token();
    let audio_counters = Arc::clone(&counters);
    let audio_clock = Arc::clone(&clock);
    let audio_device = config.audio_device.clone().unwrap_or_default();
    let audio_pair = config.audio_channel_pair;
    let audio_event_sender = event_sender.clone();
    let audio_flow_id = flow_id.clone();
    let audio_output_id = config.id.clone();
    let audio_handle = tokio::task::spawn_blocking(move || {
        audio_loop(
            audio_device,
            arx,
            audio_clock,
            audio_counters,
            audio_pair,
            program_start,
            audio_cancel,
            audio_event_sender,
            audio_flow_id,
            audio_output_id,
        );
    });

    // Wait for all children to drain (cancellation cascade). The audio
    // meter is optional; await it only when it was spawned.
    let _ = tokio::join!(demux_handle, display_handle, audio_handle);
    if let Some(handle) = meter_handle {
        let _ = handle.await;
    }

    emit_event(
        &event_sender,
        EventSeverity::Info,
        "display_stopped",
        &flow_id,
        &config.id,
        &format!(
            "display stopped: frames_displayed={} late_drops={} underruns={}",
            counters.frames_displayed.load(Ordering::Relaxed),
            counters.frames_dropped_late.load(Ordering::Relaxed),
            counters.audio_underruns.load(Ordering::Relaxed),
        ),
    );
    Ok(())
}

// ── Frame channels ────────────────────────────────────────────────

struct VideoFrame {
    /// Raw YUV planes in source format. The display task hands these to
    /// libswscale (via `VideoScaler::scale_raw_planes_into_packed`) for
    /// the YUV → BGRA conversion + scale, writing straight into the KMS
    /// dumb buffer.
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    y_stride: usize,
    u_stride: usize,
    v_stride: usize,
    width: u32,
    height: u32,
    /// FFmpeg `AVPixelFormat` integer (e.g. `AV_PIX_FMT_YUV420P`). Threads
    /// through to the scaler so libswscale knows the source planes' chroma
    /// subsampling and bit depth.
    pixel_format: i32,
    /// FFmpeg `AVColorSpace` (`AVCOL_SPC_*`). Drives the YUV→RGB matrix
    /// libswscale uses — BT.709 for HD, BT.601 for SD, BT.2020 for UHD.
    /// `AVCOL_SPC_UNSPECIFIED` (2) means the bitstream didn't tell us;
    /// the display loop falls back to BT.709 for ≥720p sources, BT.601
    /// otherwise.
    colorspace: i32,
    full_range: bool,
    pts_90k: u64,
}

struct AudioBlock {
    planar: Vec<Vec<f32>>,
    pts_90k: u64,
    sample_rate: u32,
    channels: u8,
}

// ── Demux + decode child ──────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn demux_decode_loop(
    rx: &mut broadcast::Receiver<RtpPacket>,
    vtx: mpsc::Sender<VideoFrame>,
    atx: mpsc::Sender<AudioBlock>,
    program_number: Option<u16>,
    audio_track_index: Option<u8>,
    counters: Arc<DisplayStatsCounters>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    output_id: String,
) {
    let mut demuxer = TsDemuxer::with_audio_track(program_number, audio_track_index);
    let mut video_decoder: Option<VideoDecoder> = None;
    let mut current_video_codec: Option<VideoCodec> = None;
    let mut aac_decoder: Option<AacDecoder> = None;
    let mut ff_audio_decoder: Option<FfAudioDecoder> = None;
    let mut current_ff_codec: Option<AudioDecoderCodec> = None;
    let mut last_lag_log = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(2))
        .unwrap_or_else(std::time::Instant::now);
    // Track the previous video PTS so a large discontinuity (operator
    // input switch — new stream has an unrelated PTS base) flushes the
    // persistent video decoder. Without this, the decoder keeps
    // referencing the old stream's reference frames and either emits
    // glitched output or stalls until its internal IDR-anchor recycles.
    let mut last_video_pts: Option<u64> = None;

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let packet = match rx.blocking_recv() {
            Ok(p) => p,
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // Flush both decoders — the next IDR / sync frame is the
                // new anchor. Reuse the demuxer's cached PSI.
                if let Some(d) = video_decoder.as_mut() {
                    d.flush();
                }
                if let Some(d) = aac_decoder.as_mut() {
                    d.reset();
                }
                if let Some(d) = ff_audio_decoder.as_mut() {
                    d.flush();
                }
                let now = std::time::Instant::now();
                if now.duration_since(last_lag_log).as_secs_f32() > 1.0 {
                    last_lag_log = now;
                    emit_event(
                        &event_sender,
                        EventSeverity::Warning,
                        "display_subscriber_lagged",
                        &flow_id,
                        &output_id,
                        &format!("display subscriber lagged, dropped {n} packets"),
                    );
                }
                continue;
            }
        };

        let ts_data: &[u8] = if packet.is_raw_ts {
            &packet.data
        } else {
            // Strip 12-byte RTP header. Bonded / extension headers get
            // re-stripped inside the demuxer (it scans for 0x47 sync).
            if packet.data.len() < 12 {
                continue;
            }
            &packet.data[12..]
        };

        let frames = demuxer.demux(ts_data);
        for frame in frames {
            match frame {
                DemuxedFrame::H264 { nalus, pts, .. } => {
                    if pts_jump(last_video_pts, pts) {
                        if let Some(d) = video_decoder.as_mut() {
                            d.flush();
                        }
                        if let Some(d) = aac_decoder.as_mut() {
                            d.reset();
                        }
                        if let Some(d) = ff_audio_decoder.as_mut() {
                            d.flush();
                        }
                    }
                    last_video_pts = Some(pts);
                    ensure_video_decoder(
                        &mut video_decoder,
                        &mut current_video_codec,
                        VideoCodec::H264,
                    );
                    if let Some(decoder) = video_decoder.as_mut() {
                        feed_video_decoder(decoder, &nalus, pts);
                        drain_video_frames(decoder, pts, &vtx, &counters);
                    }
                }
                DemuxedFrame::H265 { nalus, pts, .. } => {
                    if pts_jump(last_video_pts, pts) {
                        if let Some(d) = video_decoder.as_mut() {
                            d.flush();
                        }
                        if let Some(d) = aac_decoder.as_mut() {
                            d.reset();
                        }
                        if let Some(d) = ff_audio_decoder.as_mut() {
                            d.flush();
                        }
                    }
                    last_video_pts = Some(pts);
                    ensure_video_decoder(
                        &mut video_decoder,
                        &mut current_video_codec,
                        VideoCodec::Hevc,
                    );
                    if let Some(decoder) = video_decoder.as_mut() {
                        feed_video_decoder(decoder, &nalus, pts);
                        drain_video_frames(decoder, pts, &vtx, &counters);
                    }
                }
                DemuxedFrame::Aac { data, pts } => {
                    if let Some(asc) = demuxer.cached_aac_config() {
                        if aac_decoder.is_none() {
                            if let Ok(d) = aac_decoder_from_adts_config(asc) {
                                aac_decoder = Some(d);
                            }
                        }
                        if let Some(decoder) = aac_decoder.as_mut() {
                            if let Ok(decoded) = decoder.decode_frame(&data) {
                                let sr = decoder.sample_rate().unwrap_or(48_000);
                                let ch = decoder.channels().unwrap_or(2);
                                let _ = atx.try_send(AudioBlock {
                                    planar: decoded.planar,
                                    pts_90k: pts,
                                    sample_rate: sr,
                                    channels: ch,
                                });
                            }
                        }
                    }
                }
                DemuxedFrame::Opus => {
                    // The demuxer surfaces Opus discovery without a
                    // payload accessor in v1; once Opus packetization
                    // lands (planned alongside the v2 hardware-decode
                    // path) the same `try_send` pattern feeds
                    // `ff_audio_decoder` configured for Opus.
                    let _ = (&mut ff_audio_decoder, &mut current_ff_codec);
                }
                DemuxedFrame::OtherAudio { stream_type, data, pts } => {
                    let Some(codec) = crate::engine::audio_decode::ff_codec_for_stream_type(
                        stream_type,
                    ) else {
                        continue;
                    };
                    if current_ff_codec != Some(codec) {
                        ff_audio_decoder = FfAudioDecoder::open(codec).ok();
                        current_ff_codec = Some(codec);
                    }
                    if let Some(decoder) = ff_audio_decoder.as_mut() {
                        for frame_bytes in
                            crate::engine::audio_decode::split_audio_codec_frames(&data, codec)
                        {
                            if decoder.send_packet(frame_bytes, pts as i64).is_ok() {
                                while let Ok(decoded) = decoder.receive_frame() {
                                    let _ = atx.try_send(AudioBlock {
                                        planar: decoded.planar,
                                        pts_90k: pts,
                                        sample_rate: decoded.sample_rate,
                                        channels: decoded.channels,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn aac_decoder_from_adts_config(
    config: (u8, u8, u8),
) -> Result<AacDecoder, aac_audio::AacError> {
    let asc = aac_audio::decoder::build_audio_specific_config(config.0, config.1, config.2);
    AacDecoder::open_raw(&asc)
}

fn ensure_video_decoder(
    slot: &mut Option<VideoDecoder>,
    current: &mut Option<VideoCodec>,
    desired: VideoCodec,
) {
    if *current == Some(desired) && slot.is_some() {
        return;
    }
    *current = Some(desired);
    *slot = VideoDecoder::open(desired).ok();
}

/// True when `pts` is far enough from `prev` that the upstream stream
/// almost certainly changed (operator input switch). 90 kHz × 1 s =
/// 90 000 — a real continuous stream's frame-to-frame delta is well
/// under that. We also wrap-around-tolerate by computing the minimum of
/// forward and backward distance, since 33-bit PTS roll-over is a real
/// stream event we don't want to mistake for a switch.
fn pts_jump(prev: Option<u64>, pts: u64) -> bool {
    let Some(p) = prev else {
        return false;
    };
    let forward = pts.wrapping_sub(p);
    let backward = p.wrapping_sub(pts);
    forward.min(backward) > 90_000
}

fn feed_video_decoder(decoder: &mut VideoDecoder, nalus: &[Vec<u8>], pts_90k: u64) {
    // Concatenate NAL units back into Annex-B form (start codes between
    // each NALU). The demuxer already strips ADTS / start codes, so we
    // re-add the standard `0x00 0x00 0x00 0x01` prefix. PTS is attached
    // to the input packet so FFmpeg's reorder queue can hand the
    // matching display-order PTS back on `receive_frame`.
    let total = nalus.iter().map(|n| n.len() + 4).sum::<usize>();
    let mut buf = Vec::with_capacity(total);
    for n in nalus {
        buf.extend_from_slice(&[0, 0, 0, 1]);
        buf.extend_from_slice(n);
    }
    let _ = decoder.send_packet_with_pts(&buf, pts_90k as i64);
}

fn drain_video_frames(
    decoder: &mut VideoDecoder,
    fallback_pts_90k: u64,
    vtx: &mpsc::Sender<VideoFrame>,
    counters: &DisplayStatsCounters,
) {
    while let Ok(frame) = decoder.receive_frame() {
        let Some((y, ys, u, us, v, vs)) = frame.yuv_planes() else {
            continue;
        };
        // Prefer the decoder-propagated display-order PTS. With
        // B-frame H.264 / HEVC, the input-feed PTS we held in the
        // outer loop matches the *most recent fed* access unit, not
        // this particular decoded frame — using it would place every
        // frame in a GOP at the same audio-clock offset and the
        // dup/drop logic would misfire on every B-frame. Falling back
        // to the input PTS is fine for I-only streams where the
        // decoder has no chance to reorder.
        let pts_90k = frame.pts().map(|p| p as u64).unwrap_or(fallback_pts_90k);
        let frame = VideoFrame {
            y: y.to_vec(),
            u: u.to_vec(),
            v: v.to_vec(),
            y_stride: ys,
            u_stride: us,
            v_stride: vs,
            width: frame.width(),
            height: frame.height(),
            pixel_format: frame.pixel_format(),
            colorspace: frame.colorspace(),
            full_range: frame.is_full_range(),
            pts_90k,
        };
        if vtx.try_send(frame).is_err() {
            counters.frames_dropped_late.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ── Display child ─────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn display_loop(
    mut kms: KmsDisplay,
    mut vrx: mpsc::Receiver<VideoFrame>,
    clock: Arc<AudioClock>,
    counters: Arc<DisplayStatsCounters>,
    cancel: CancellationToken,
    output_stats: Arc<OutputStatsAccumulator>,
    audio_codec_label: &'static str,
    event_sender: EventSender,
    flow_id: String,
    output_id: String,
    meter: Option<SharedMeter>,
    scaling_mode: DisplayScalingMode,
) {
    // Drift threshold = full source frame period, derived from the
    // observed PTS deltas. 33 ms (30 fps) until we've seen enough
    // frames to estimate. The previous half-frame threshold dropped
    // ~2 % of 25 fps frames because typical PCR-paced sources jitter
    // beyond it.
    let mut frame_period_ms: i64 = 33;
    let mut last_pts: Option<u64> = None;
    let mut last_frame: Option<VideoFrame> = None;
    let mut scaler: Option<CachedScaler> = None;

    // Resolution autodetect state. Re-armed on PTS jump (input switch)
    // or on any mid-stream source resolution change.
    let mut matched_dims: Option<(u32, u32)> = None;
    let mut stats_registered = false;

    while !cancel.is_cancelled() {
        let next = match vrx.blocking_recv() {
            Some(f) => f,
            None => break,
        };

        // Re-arm autodetect if the source resolution shifted (operator
        // switched from 1080p to 720p, etc).
        if let Some((mw, mh)) = matched_dims {
            if mw != next.width || mh != next.height {
                matched_dims = None;
                last_frame = None;
            }
        }

        // First frame after open or after re-arm: pick the panel mode
        // according to `scaling_mode`.
        // - `MatchSource`: re-modeset to the smallest mode whose dims
        //   cover the source. Refresh stays at the panel's preferred
        //   rate — desktop monitors that advertise low-refresh modes
        //   (24 / 25 / 30 Hz) typically can't drive them without
        //   flicker, and the audio-master dup/drop logic already
        //   handles source-fps-vs-panel-Hz cadence cleanly.
        // - `MonitorNative`: hold the panel at the connector's
        //   preferred mode (already set at open) and let libswscale
        //   upscale the source. The `set_monitor_native_mode` call is
        //   defensive — KMS opened at the preferred mode already, so
        //   it's a no-op on the steady-state path.
        if matched_dims.is_none() {
            let (modeset, ok_code, err_code, ok_verb, err_verb) = match scaling_mode {
                DisplayScalingMode::MatchSource => (
                    kms.match_source_resolution(next.width, next.height),
                    "display_auto_matched",
                    "display_auto_match_failed",
                    "auto-matched to source",
                    "auto-match fell back to startup mode",
                ),
                DisplayScalingMode::MonitorNative => (
                    kms.set_monitor_native_mode(),
                    "display_monitor_native_set",
                    "display_monitor_native_set_failed",
                    "set to monitor-native (panel-preferred mode)",
                    "monitor-native modeset fell back to startup mode",
                ),
            };
            match modeset {
                Ok(()) => emit_event(
                    &event_sender,
                    EventSeverity::Info,
                    ok_code,
                    &flow_id,
                    &output_id,
                    &format!(
                        "display {} for source {}x{} → {}x{}@{}Hz",
                        ok_verb,
                        next.width,
                        next.height,
                        kms.width(),
                        kms.height(),
                        kms.refresh_hz(),
                    ),
                ),
                Err(e) => emit_event(
                    &event_sender,
                    EventSeverity::Warning,
                    err_code,
                    &flow_id,
                    &output_id,
                    &format!("display {err_verb}: {e}"),
                ),
            }
            // Register / refresh the stats handle with the post-modeset
            // resolution so the manager UI shows the active mode.
            output_stats.set_display_stats(
                Arc::clone(&counters),
                format!("{}x{}", kms.width(), kms.height()),
                kms.refresh_hz(),
                "XRGB8888",
                "sw",
                "unknown",
                audio_codec_label,
            );
            stats_registered = true;
            matched_dims = Some((next.width, next.height));
        }

        // Track the running source frame period so the drift threshold
        // adapts to 25 / 30 / 50 / 60 fps content. A frame-to-frame
        // delta beyond ±1 s is treated as a stream change — drop the
        // cached previous frame and re-arm the resolution match for
        // the new source.
        if let Some(prev) = last_pts {
            let forward = next.pts_90k.wrapping_sub(prev) as i64;
            let backward = (prev.wrapping_sub(next.pts_90k)) as i64;
            let dms = forward / 90;
            if forward.unsigned_abs() > 90_000 && backward.unsigned_abs() > 90_000 {
                last_frame = None;
                frame_period_ms = 33;
                matched_dims = None;
            } else if (10..=200).contains(&dms) {
                // Light EMA so a one-off long frame doesn't move the
                // window. α = 1/8 is plenty for ≤ 60 fps content.
                frame_period_ms = (frame_period_ms * 7 + dms) / 8;
            }
        }
        last_pts = Some(next.pts_90k);
        let _ = stats_registered;

        // Compute drift against the audio clock, dropping frames that
        // fell behind / repeating frames that ran too far ahead. The
        // threshold is **1.5×** source frame period — at 25 fps that
        // is 60 ms, well clear of typical ALSA per-period jitter
        // (~10-20 ms) so a single sample-quanta wobble doesn't trip
        // a false repeat every few hundred ms (the visible "skipping"
        // the user saw on 1080p25 confidence playback). We never
        // **drop** unless behind by more than 2× period — losing
        // a frame is more visible than a single-frame stale repeat.
        if let Some(audio_pts) = clock.current_pts_90k() {
            let drift_pts: i64 = next.pts_90k as i64 - audio_pts as i64;
            // 90 PTS units = 1 ms.
            let drift_ms = drift_pts / 90;
            counters.store_av_offset_ms(drift_ms.clamp(i32::MIN as i64, i32::MAX as i64) as i32);

            let repeat_threshold = (frame_period_ms * 3) / 2;
            let drop_threshold = frame_period_ms * 2;
            if drift_ms < -drop_threshold {
                counters.frames_dropped_late.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if drift_ms > repeat_threshold {
                if let Some(prev) = last_frame.as_ref() {
                    if blit_and_present(&mut kms, prev, &mut scaler, meter.as_ref()).is_ok() {
                        counters.frames_repeated.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
            }
        }
        if blit_and_present(&mut kms, &next, &mut scaler, meter.as_ref()).is_ok() {
            counters.frames_displayed.fetch_add(1, Ordering::Relaxed);
        }
        last_frame = Some(next);
    }
}

/// Cached libswscale context. Held across frames and rebuilt only when
/// the source shape changes — every parameter change costs a fresh
/// `sws_getContext` (heavy) and a fresh `sws_setColorspaceDetails` to
/// reapply the YUV→RGB matrix.
struct CachedScaler {
    inner: VideoScaler,
    src_w: u32,
    src_h: u32,
    src_pix_fmt: i32,
    dst_w: u32,
    dst_h: u32,
    src_colorspace: i32,
    src_full_range: bool,
}

/// FFmpeg `AVCOL_SPC_*` integers we care about. Repeated here as plain
/// constants so this file doesn't have to depend on libffmpeg-video-sys
/// directly — `VideoScaler::set_yuv_to_rgb_colorspace` accepts any
/// integer libswscale recognises.
const AVCOL_SPC_BT709: i32 = 1;
const AVCOL_SPC_UNSPECIFIED: i32 = 2;
const AVCOL_SPC_SMPTE170M: i32 = 6;

fn effective_colorspace(signalled: i32, src_h: u32) -> i32 {
    // BT.709 for HD and above, BT.601 (SMPTE 170M) for SD when the
    // bitstream didn't tell us. This matches what every modern decoder
    // assumes when VUI is missing.
    if signalled == AVCOL_SPC_UNSPECIFIED {
        if src_h >= 720 {
            AVCOL_SPC_BT709
        } else {
            AVCOL_SPC_SMPTE170M
        }
    } else {
        signalled
    }
}

fn ensure_scaler(
    cache: &mut Option<CachedScaler>,
    src_w: u32,
    src_h: u32,
    src_pix_fmt: i32,
    dst_w: u32,
    dst_h: u32,
    src_colorspace: i32,
    src_full_range: bool,
) -> Result<&mut CachedScaler> {
    let needs_rebuild = match cache.as_ref() {
        Some(c) => {
            c.src_w != src_w
                || c.src_h != src_h
                || c.src_pix_fmt != src_pix_fmt
                || c.dst_w != dst_w
                || c.dst_h != dst_h
                || c.src_colorspace != src_colorspace
                || c.src_full_range != src_full_range
        }
        None => true,
    };
    if needs_rebuild {
        let inner = VideoScaler::new_with_dst_format(
            src_w,
            src_h,
            src_pix_fmt,
            dst_w,
            dst_h,
            ScalerDstFormat::Bgra8,
        )
        .map_err(|e| anyhow::anyhow!("display scaler init failed: {e}"))?;
        inner.set_yuv_to_rgb_colorspace(src_colorspace, src_full_range);
        *cache = Some(CachedScaler {
            inner,
            src_w,
            src_h,
            src_pix_fmt,
            dst_w,
            dst_h,
            src_colorspace,
            src_full_range,
        });
    }
    Ok(cache.as_mut().expect("scaler just inserted"))
}

fn blit_and_present(
    kms: &mut KmsDisplay,
    frame: &VideoFrame,
    scaler: &mut Option<CachedScaler>,
    meter: Option<&SharedMeter>,
) -> Result<()> {
    let mut map = kms.back_buffer()?;
    let pitch = map.pitch() as usize;
    let dst_w = map.width();
    let dst_h = map.height();
    let dst = map.as_mut();

    let src_w = frame.width;
    let src_h = frame.height;
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Ok(());
    }

    let colorspace = effective_colorspace(frame.colorspace, src_h);
    let cached = ensure_scaler(
        scaler,
        src_w,
        src_h,
        frame.pixel_format,
        dst_w,
        dst_h,
        colorspace,
        frame.full_range,
    )?;
    cached
        .inner
        .scale_raw_planes_into_packed(
            src_w,
            src_h,
            frame.pixel_format,
            &frame.y,
            frame.y_stride,
            &frame.u,
            frame.u_stride,
            &frame.v,
            frame.v_stride,
            dst,
            pitch,
        )
        .map_err(|e| anyhow::anyhow!("display scale failed: {e}"))?;

    if let Some(snapshot) = meter {
        // Snapshot is small (≤ ~16 PIDs × 8 channels × 16 B); clone the
        // current snapshot and rasterise without holding the mutex
        // across the per-pixel writes.
        let snap: MeterSnapshot = snapshot
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        crate::display::audio_bars::rasterise(&snap, dst, pitch, dst_w, dst_h);
    }

    drop(map);
    kms.present()?;
    Ok(())
}

// ── Audio child ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn audio_loop(
    device: String,
    mut arx: mpsc::Receiver<AudioBlock>,
    clock: Arc<AudioClock>,
    counters: Arc<DisplayStatsCounters>,
    channel_pair: [u8; 2],
    program_start: Instant,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    output_id: String,
) {
    if device.is_empty() {
        // Audio muted — drain the channel until the demux child closes
        // it on shutdown so the bounded mpsc never wedges.
        while !cancel.is_cancelled() {
            if arx.blocking_recv().is_none() {
                break;
            }
        }
        return;
    }
    let mut backend = AudioBackend::new(device);
    while !cancel.is_cancelled() {
        let block = match arx.blocking_recv() {
            Some(b) => b,
            None => break,
        };
        match backend.write(
            &block.planar,
            block.pts_90k,
            block.sample_rate,
            block.channels,
            &clock,
            program_start,
            channel_pair,
        ) {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                let code = if msg.contains("display_audio_open_failed")
                    || msg.contains("snd_pcm_open")
                {
                    "display_audio_device_invalid"
                } else {
                    counters.audio_underruns.fetch_add(1, Ordering::Relaxed);
                    "display_audio_open_failed"
                };
                emit_event(
                    &event_sender,
                    EventSeverity::Critical,
                    code,
                    &flow_id,
                    &output_id,
                    &format!("display audio write failed: {msg}"),
                );
                // Reset and back off briefly — don't kill the whole
                // output; video continues.
                backend.reset();
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────

fn classify_kms_error(msg: &str) -> &'static str {
    if msg.contains("display_resolution_unsupported") {
        "display_resolution_unsupported"
    } else if msg.contains("display_mode_set_failed") {
        "display_mode_set_failed"
    } else if msg.contains("display_device_invalid") {
        "display_device_invalid"
    } else {
        "display_device_invalid"
    }
}

fn emit_event(
    sender: &EventSender,
    severity: EventSeverity,
    error_code: &str,
    flow_id: &str,
    output_id: &str,
    message: &str,
) {
    let details = serde_json::json!({
        "error_code": error_code,
        "output_id": output_id,
    });
    sender.emit_with_details(severity, "display", message, Some(flow_id), details);
    let _ = Bytes::new(); // suppress unused-import warning
}
