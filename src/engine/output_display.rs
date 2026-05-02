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
use video_engine::{AudioDecoder as FfAudioDecoder, VideoCodec, VideoDecoder};

use crate::config::models::DisplayOutputConfig;
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
    // 1. Open KMS first so we fail fast with `display_device_invalid` /
    //    `display_resolution_unsupported` / `display_mode_set_failed`
    //    before doing any other work.
    let (req_w, req_h, is_auto_resolution) = parse_resolution(config.resolution.as_deref());
    let kms = match tokio::task::spawn_blocking({
        let device = config.device.clone();
        let refresh = config.refresh_hz;
        move || KmsDisplay::open(&device, req_w, req_h, refresh)
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

    emit_event(
        &event_sender,
        EventSeverity::Info,
        "display_started",
        &flow_id,
        &config.id,
        &format!(
            "display started on {} ({}@{}Hz{}){}",
            config.device,
            chosen_resolution,
            chosen_refresh,
            if is_auto_resolution {
                ", auto-match pending first frame"
            } else {
                ""
            },
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
    let display_handle = tokio::task::spawn_blocking(move || {
        display_loop(
            kms,
            vrx,
            display_clock,
            display_counters,
            display_cancel,
            is_auto_resolution,
            display_output_stats,
            audio_codec_label,
            display_event_sender,
            display_flow_id,
            display_output_id,
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

    // Wait for all three to drain (cancellation cascade).
    let _ = tokio::join!(demux_handle, display_handle, audio_handle);

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
    /// Raw YUV planes in source format. The display task does the
    /// `YUV → XRGB8888` conversion inline.
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    y_stride: usize,
    u_stride: usize,
    v_stride: usize,
    width: u32,
    height: u32,
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
                    ensure_video_decoder(
                        &mut video_decoder,
                        &mut current_video_codec,
                        VideoCodec::H264,
                    );
                    if let Some(decoder) = video_decoder.as_mut() {
                        feed_video_decoder(decoder, &nalus);
                        drain_video_frames(decoder, pts, &vtx, &counters);
                    }
                }
                DemuxedFrame::H265 { nalus, pts, .. } => {
                    ensure_video_decoder(
                        &mut video_decoder,
                        &mut current_video_codec,
                        VideoCodec::Hevc,
                    );
                    if let Some(decoder) = video_decoder.as_mut() {
                        feed_video_decoder(decoder, &nalus);
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

fn feed_video_decoder(decoder: &mut VideoDecoder, nalus: &[Vec<u8>]) {
    // Concatenate NAL units back into Annex-B form (start codes between
    // each NALU). The demuxer already strips ADTS / start codes, so we
    // re-add the standard `0x00 0x00 0x00 0x01` prefix.
    let total = nalus.iter().map(|n| n.len() + 4).sum::<usize>();
    let mut buf = Vec::with_capacity(total);
    for n in nalus {
        buf.extend_from_slice(&[0, 0, 0, 1]);
        buf.extend_from_slice(n);
    }
    let _ = decoder.send_packet(&buf);
}

fn drain_video_frames(
    decoder: &mut VideoDecoder,
    pts_90k: u64,
    vtx: &mpsc::Sender<VideoFrame>,
    counters: &DisplayStatsCounters,
) {
    while let Ok(frame) = decoder.receive_frame() {
        let Some((y, ys, u, us, v, vs)) = frame.yuv_planes() else {
            continue;
        };
        let frame = VideoFrame {
            y: y.to_vec(),
            u: u.to_vec(),
            v: v.to_vec(),
            y_stride: ys,
            u_stride: us,
            v_stride: vs,
            width: frame.width(),
            height: frame.height(),
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
    is_auto_resolution: bool,
    output_stats: Arc<OutputStatsAccumulator>,
    audio_codec_label: &'static str,
    event_sender: EventSender,
    flow_id: String,
    output_id: String,
) {
    let frame_period_ms_at_60hz: i64 = 16;
    let mut last_frame: Option<VideoFrame> = None;
    let mut stats_registered = false;

    while !cancel.is_cancelled() {
        let next = match vrx.blocking_recv() {
            Some(f) => f,
            None => break,
        };

        // First decoded frame: if the operator picked `auto`, re-modeset
        // to the smallest mode whose dimensions cover the source. This
        // is the fix for the 4K-on-1080p-source CPU-blit jitter — the
        // monitor's preferred mode is no longer the default.
        if !stats_registered {
            if is_auto_resolution {
                match kms.match_source_resolution(next.width, next.height) {
                    Ok(()) => {
                        emit_event(
                            &event_sender,
                            EventSeverity::Info,
                            "display_auto_matched",
                            &flow_id,
                            &output_id,
                            &format!(
                                "display auto-matched to source {}x{} → {}x{}@{}Hz",
                                next.width,
                                next.height,
                                kms.width(),
                                kms.height(),
                                kms.refresh_hz(),
                            ),
                        );
                        // Buffer dimensions changed — drop any cached
                        // previous frame so we don't blit from a stale
                        // size into the new framebuffers.
                        last_frame = None;
                    }
                    Err(e) => {
                        emit_event(
                            &event_sender,
                            EventSeverity::Warning,
                            "display_auto_match_failed",
                            &flow_id,
                            &output_id,
                            &format!("display auto-match fell back to startup mode: {e}"),
                        );
                    }
                }
            }
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
        }
        // Compute drift against the audio clock, dropping frames that
        // fell behind by more than ±half a frame period.
        if let Some(audio_pts) = clock.current_pts_90k() {
            let drift_pts: i64 = next.pts_90k as i64 - audio_pts as i64;
            // 90 PTS units = 1 ms.
            let drift_ms = drift_pts / 90;
            counters.store_av_offset_ms(drift_ms.clamp(i32::MIN as i64, i32::MAX as i64) as i32);

            if drift_ms < -frame_period_ms_at_60hz {
                counters.frames_dropped_late.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if drift_ms > frame_period_ms_at_60hz {
                if let Some(prev) = last_frame.as_ref() {
                    if blit_and_present(&mut kms, prev).is_ok() {
                        counters.frames_repeated.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
            }
        }
        if blit_and_present(&mut kms, &next).is_ok() {
            counters.frames_displayed.fetch_add(1, Ordering::Relaxed);
        }
        last_frame = Some(next);
    }
}

fn blit_and_present(kms: &mut KmsDisplay, frame: &VideoFrame) -> Result<()> {
    let mut map = kms.back_buffer()?;
    let pitch = map.pitch() as usize;
    let dst_w = map.width() as usize;
    let dst_h = map.height() as usize;
    let dst = map.as_mut();

    // Fast YUV420 → XRGB8888 nearest-neighbour blit. Good enough for
    // a 1080p confidence monitor; v2 routes through libswscale or a
    // hardware overlay plane for true broadcast quality.
    let src_w = frame.width as usize;
    let src_h = frame.height as usize;
    if src_w == 0 || src_h == 0 {
        return Ok(());
    }
    let scale_x = (src_w as f32) / (dst_w as f32).max(1.0);
    let scale_y = (src_h as f32) / (dst_h as f32).max(1.0);
    for y in 0..dst_h {
        let sy = ((y as f32) * scale_y) as usize;
        let sy = sy.min(src_h - 1);
        let row_base = y * pitch;
        for x in 0..dst_w {
            let sx = ((x as f32) * scale_x) as usize;
            let sx = sx.min(src_w - 1);
            let yval = frame.y[sy * frame.y_stride + sx] as i32;
            let uy = sy / 2;
            let ux = sx / 2;
            let uval = frame.u[uy * frame.u_stride + ux] as i32 - 128;
            let vval = frame.v[uy * frame.v_stride + ux] as i32 - 128;
            // BT.601 conversion (good enough for the v1 confidence
            // monitor; v2 picks the right matrix from VUI).
            let c = yval - 16;
            let r = ((298 * c + 409 * vval + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * uval - 208 * vval + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * uval + 128) >> 8).clamp(0, 255) as u8;
            let off = row_base + x * 4;
            if off + 4 <= dst.len() {
                dst[off] = b;
                dst[off + 1] = g;
                dst[off + 2] = r;
                dst[off + 3] = 0xFF;
            }
        }
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

/// Returns `(width, height, is_auto)`. `is_auto` is true when the
/// operator left resolution unset or picked "auto" — in that case
/// `(None, None)` opens KMS at the connector's preferred mode and the
/// display loop re-modesets to the source flow's dimensions on first
/// decoded frame (avoiding the 4K-on-a-1080p-source CPU-blit jitter).
fn parse_resolution(res: Option<&str>) -> (Option<u32>, Option<u32>, bool) {
    let Some(r) = res else { return (None, None, true) };
    if r.is_empty() || r == "auto" {
        return (None, None, true);
    }
    let Some((w, h)) = r.split_once('x') else {
        return (None, None, true);
    };
    (w.parse().ok(), h.parse().ok(), false)
}

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
