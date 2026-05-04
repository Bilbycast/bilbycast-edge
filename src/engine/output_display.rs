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
// anchor. The display task paces each decoded frame to its
// audio-clock display time — sleeping when the frame arrived early
// (the previous frame stays scanned-out by the panel, no flip needed),
// dropping when it arrived more than 2× period late
// (`frames_dropped_late++`). Audio xrun → ALSA `prepare()` and
// continue (`audio_underruns++`) without nudging the anchor. The flow
// input is **never** blocked. The legacy `frames_repeated` counter
// stays on `DisplayStatsCounters` for dashboard back-compat but no
// longer increments — sleep-pacing makes the previous frame stay
// scanned-out naturally between flips.

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
    AudioDecoder as FfAudioDecoder, DecoderBackend, ScalerDstFormat, VideoCodec, VideoDecoder,
    VideoScaler,
};

use crate::config::models::{DisplayOutputConfig, DisplayScalingMode};
use crate::display::audio_bars::{new_shared_meter, SharedMeter};
use crate::display::audio_meter::spawn_audio_meter;
use crate::display::{audio::AudioBackend, clock::AudioClock, kms::KmsDisplay};
use crate::engine::packet::RtpPacket;
use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::{DisplayStatsCounters, OutputStatsAccumulator};

// Display feeder depth. 8 was too tight on 1080p H.264 CPU decode —
// a single heavy frame whose blit+present overran the source frame
// period (e.g. a busy P/B-frame at 25 fps where the 40 ms budget got
// consumed by libswscale + audio-bars overlay + vblank wait) left no
// slack for the next frame, so the demux child saw `try_send` fail
// and dropped a perfectly good decoded frame. 24 slots = ~1 second
// of pre-decoded video on a 25 fps source — long enough to ride out
// the worst per-frame spike we measured without ballooning latency
// (the display task still paces against the audio clock, so a deep
// queue doesn't translate into visible lag).
const MPSC_VIDEO_DEPTH: usize = 24;
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
    // 0. Resolve the operator's hardware-decode preference against the
    //    edge's startup-probed capabilities. **Broadcast invariant**:
    //    the display output never goes dark for HW-availability
    //    reasons. `Auto` picks the best HW backend the host can do and
    //    falls back to CPU; an explicit `Nvdec` / `Qsv` / `Vaapi` that
    //    the host can't satisfy emits a Warning
    //    (`display_hw_decode_unavailable_falling_back`) and silently
    //    runs CPU instead so the picture stays on screen — the
    //    operator sees the alarm in the UI and can choose to leave
    //    the dropdown alone or switch it to `cpu` permanently. The
    //    cost-plan resolver in `derive_cost_plan` already does the
    //    same `.unwrap_or(Cpu)` so cost accounting matches what we
    //    actually run here. The Critical
    //    `display_hw_decode_unavailable` code is reserved for a future
    //    "neither HW nor CPU works" path (e.g. CPU decode disabled at
    //    build); not reachable today.
    let pref = config.hw_decode.unwrap_or_default();
    let (resolved, hw_unavailable_reason) =
        match crate::engine::hardware_probe::resolve_display_decoder(
            &pref,
            crate::engine::hardware_probe::static_capabilities().as_deref(),
        ) {
            Ok(r) => (r, None),
            Err(reason) => {
                let reason_tag = reason.as_reason();
                let msg = format!(
                    "display output '{}': hw_decode '{:?}' unavailable on this host ({}); \
                     falling back to CPU decode",
                    config.id, pref, reason_tag,
                );
                event_sender.emit_flow_with_details(
                    EventSeverity::Warning,
                    crate::manager::events::category::SYSTEM_RESOURCES,
                    msg,
                    &flow_id,
                    serde_json::json!({
                        "error_code": "display_hw_decode_unavailable_falling_back",
                        "output_id": config.id,
                        "preference": format!("{:?}", pref).to_lowercase(),
                        "reason": reason_tag,
                        "fell_back_to": "cpu",
                    }),
                );
                (
                    crate::engine::hardware_probe::ResolvedDisplayDecoder::Cpu,
                    Some(reason_tag),
                )
            }
        };
    let backend = resolved.as_backend();

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
    let demux_backend = backend;
    // True when the open-time soft-fallback (Section 2 above) already
    // emitted its Warning event — keeps the runtime fallback machinery
    // from double-warning since `state.backend` is already `Cpu` and
    // the operator's preference can't be honoured anyway.
    let demux_hw_already_unavailable = hw_unavailable_reason.is_some();
    // Snapshot the operator's original preference (before soft-fallback
    // forced it to Cpu) so the runtime fallback events name the right
    // backend in the manager UI.
    let demux_requested_backend = match pref {
        crate::config::models::HwDecodePreference::Auto
        | crate::config::models::HwDecodePreference::Cpu => backend,
        crate::config::models::HwDecodePreference::Nvdec => DecoderBackend::Nvdec,
        crate::config::models::HwDecodePreference::Qsv => DecoderBackend::Qsv,
        crate::config::models::HwDecodePreference::Vaapi => DecoderBackend::Vaapi,
    };
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
            demux_backend,
            demux_requested_backend,
            demux_hw_already_unavailable,
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
    // Surface the resolved decoder backend on `DisplayStats.decoder_kind`
    // so the manager UI's flow card and Resources card can render the
    // active mode (e.g. "display (1920x1080@60Hz · NVDEC)"). Static
    // strings — the resolver runs once per output.
    let decoder_kind_label: &'static str = match (resolved, hw_unavailable_reason) {
        // Soft-fallback path: operator asked for HW but the host can't
        // do it. We're running CPU but the UI must reflect *why* — so
        // the flow card reads "cpu (hw unavailable)" rather than just
        // "cpu", which would make the dropdown choice look correct.
        (crate::engine::hardware_probe::ResolvedDisplayDecoder::Cpu, Some(_)) => {
            "cpu (hw unavailable)"
        }
        (crate::engine::hardware_probe::ResolvedDisplayDecoder::Cpu, None) => "cpu",
        (crate::engine::hardware_probe::ResolvedDisplayDecoder::Nvdec, _) => "nvdec",
        (crate::engine::hardware_probe::ResolvedDisplayDecoder::Qsv, _) => "qsv",
        (crate::engine::hardware_probe::ResolvedDisplayDecoder::Vaapi, _) => "vaapi",
    };
    let display_handle = tokio::task::spawn_blocking(move || {
        display_loop(
            kms,
            vrx,
            display_clock,
            display_counters,
            display_cancel,
            display_output_stats,
            audio_codec_label,
            decoder_kind_label,
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

/// Chroma layout carried alongside the Y plane through the demux →
/// display mpsc. CPU planar YUV (`yuv420p` / `yuv422p` / `yuv444p` and
/// the 10/12-bit LE planar siblings) lands in the `Planar` arm; HW
/// decoder output (NV12 / NV16 / P010LE / P016LE / P210LE / P216LE)
/// lands in the `SemiPlanar` arm. The display loop dispatches on the
/// arm at blit time — libswscale handles the format conversion +
/// matrix + scale natively for both, so no per-pixel reformat runs on
/// the demux thread (an earlier revision shifted P0xx high-bit data
/// down to YUV420P10LE in pure Rust and couldn't sustain 4K 50 fps —
/// the broadcast subscriber kept lagging and the decoder flushed on
/// every `RecvError::Lagged`, leaving the operator looking at one
/// frame every few seconds).
enum VideoFrameChroma {
    Planar {
        u: Vec<u8>,
        u_stride: usize,
        v: Vec<u8>,
        v_stride: usize,
    },
    SemiPlanar {
        uv: Vec<u8>,
        uv_stride: usize,
    },
}

struct VideoFrame {
    /// Y plane (luma) — raw bytes from the decoder, owned so we can
    /// move them across the mpsc out of the decoder's lifetime.
    y: Vec<u8>,
    y_stride: usize,
    /// Chroma — planar (3 planes) or semi-planar (interleaved UV).
    /// See [`VideoFrameChroma`].
    chroma: VideoFrameChroma,
    width: u32,
    height: u32,
    /// FFmpeg `AVPixelFormat` integer of the source layout — what
    /// the decoder actually produced. The display scaler is keyed on
    /// this, so libswscale picks up the correct semi-planar / planar
    /// reader (and bit-depth) without us reformatting first.
    pixel_format: i32,
    /// FFmpeg `AVColorSpace` (`AVCOL_SPC_*`). Drives the YUV→RGB matrix
    /// libswscale uses — BT.709 for HD, BT.601 for SD, BT.2020 for UHD.
    /// `AVCOL_SPC_UNSPECIFIED` (2) means the bitstream didn't tell us;
    /// the display loop falls back to BT.709 for ≥720p sources, BT.601
    /// otherwise.
    colorspace: i32,
    /// FFmpeg `AVColorTransferCharacteristic` (`AVCOL_TRC_*`). Drives
    /// the EOTF — `BT709` (1) for SDR HD, `SMPTE2084` (16) for PQ
    /// HDR, `ARIB_STD_B67` (18) for HLG HDR. The display loop reads
    /// this to decide whether to apply an HDR-to-SDR tonemap LUT
    /// after libswscale produces 8-bit BGRA, so a UHD HDR contribution
    /// feed shows recognisable colours on a Rec.709 confidence panel
    /// instead of a dim, washed-out frame.
    color_transfer: i32,
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
    backend: DecoderBackend,
    requested_backend: DecoderBackend,
    hw_already_unavailable: bool,
) {
    let mut demuxer = TsDemuxer::with_audio_track(program_number, audio_track_index);
    let mut video_decoder: Option<VideoDecoder> = None;
    let mut current_video_codec: Option<VideoCodec> = None;
    // HW-decoder open lifecycle. Fresh per flow run, so each flow
    // restart re-attempts the operator's chosen HW backend before
    // falling back to CPU. See `open_video_decoder_with_retry` for
    // the retry budget + event semantics. When the open-time
    // soft-fallback (Section 2 in `run_display_output`) already fired,
    // we start with `fell_back_to_cpu = true` so the runtime fallback
    // machinery doesn't double-warn.
    let mut hw_open_state = HwOpenState {
        backend,
        requested_backend,
        fell_back_to_cpu: hw_already_unavailable,
        fallback_event_emitted: hw_already_unavailable,
        consecutive_send_errors: 0,
        last_send_error_log_at: None,
        first_send_after_open: None,
    };
    let mut aac_decoder: Option<AacDecoder> = None;
    let mut ff_audio_decoder: Option<FfAudioDecoder> = None;
    let mut current_ff_codec: Option<AudioDecoderCodec> = None;
    let mut last_lag_log = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(2))
        .unwrap_or_else(std::time::Instant::now);
    let mut last_lagged_at: Option<Instant> = None;
    // Track the previous video PTS so a large discontinuity (operator
    // input switch — new stream has an unrelated PTS base) flushes the
    // persistent video decoder. Without this, the decoder keeps
    // referencing the old stream's reference frames and either emits
    // glitched output or stalls until its internal IDR-anchor recycles.
    let mut last_video_pts: Option<u64> = None;
    // Re-arming guard: the display path supports planar YUV 4:2:0/4:2:2/
    // 4:4:4 (8/10/12-bit) and semi-planar NV12 / NV16 / P010LE / P016LE
    // / P210LE / P216LE. Any other pixel format from the decoder is
    // silently dropped. Emit a Warning-level event when we first see
    // an unsupported format and then again every
    // `UNSUPPORTED_PIXFMT_RE_EMIT_S` seconds — the per-frame counter
    // (`frames_dropped_unsupported_pixfmt`) gives the manager UI the
    // continuous-rate signal between event emissions.
    let mut last_unsupported_pixfmt_at: Option<Instant> = None;

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
                    reset_decoder_open_window(&mut hw_open_state, &counters);
                }
                if let Some(d) = aac_decoder.as_mut() {
                    d.reset();
                }
                if let Some(d) = ff_audio_decoder.as_mut() {
                    d.flush();
                }
                let now = std::time::Instant::now();
                counters.subscriber_lag_events.fetch_add(1, Ordering::Relaxed);
                let elapsed_since_prev = last_lagged_at.map(|t| now.duration_since(t));
                last_lagged_at = Some(now);
                tracing::debug!(
                    output_id = %output_id,
                    dropped = n,
                    since_prev_ms = elapsed_since_prev.map(|d| d.as_millis() as u64).unwrap_or(0),
                    "display broadcast subscriber lagged",
                );
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
                    handle_video_au(
                        &nalus,
                        pts,
                        VideoCodec::H264,
                        &mut last_video_pts,
                        &mut video_decoder,
                        &mut current_video_codec,
                        &mut aac_decoder,
                        &mut ff_audio_decoder,
                        &mut hw_open_state,
                        &mut last_unsupported_pixfmt_at,
                        &counters,
                        &vtx,
                        &event_sender,
                        &flow_id,
                        &output_id,
                    );
                }
                DemuxedFrame::H265 { nalus, pts, .. } => {
                    handle_video_au(
                        &nalus,
                        pts,
                        VideoCodec::Hevc,
                        &mut last_video_pts,
                        &mut video_decoder,
                        &mut current_video_codec,
                        &mut aac_decoder,
                        &mut ff_audio_decoder,
                        &mut hw_open_state,
                        &mut last_unsupported_pixfmt_at,
                        &counters,
                        &vtx,
                        &event_sender,
                        &flow_id,
                        &output_id,
                    );
                }
                DemuxedFrame::Mpeg2 { es, pts, .. } => {
                    // MPEG-2 has no NAL framing — wrap the ES in a single
                    // synthetic "NALU" so we reuse the existing
                    // PTS-jump / decoder-ensure / drain pipeline. The
                    // Annex-B prefix the helper prepends is harmless to
                    // the libavcodec mpeg2video decoder — it scans for
                    // its own start codes (`0x000001B3` etc.) inside the
                    // payload and ignores leading bytes that don't
                    // match.
                    let synthetic = vec![es];
                    handle_video_au(
                        &synthetic,
                        pts,
                        VideoCodec::Mpeg2,
                        &mut last_video_pts,
                        &mut video_decoder,
                        &mut current_video_codec,
                        &mut aac_decoder,
                        &mut ff_audio_decoder,
                        &mut hw_open_state,
                        &mut last_unsupported_pixfmt_at,
                        &counters,
                        &vtx,
                        &event_sender,
                        &flow_id,
                        &output_id,
                    );
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

/// HW-decode lifecycle state for one demux loop run. Tracks the active
/// backend (mutates on fallback), a one-shot "we already warned about
/// the fallback" gate, and the operator's original requested backend
/// for the diagnostic event detail. Constructed once at the top of
/// `demux_decode_loop`; a fresh struct on the next flow restart gives
/// HW another chance, matching the "toggle recording off → picture
/// returns" recovery path operators already rely on.
struct HwOpenState {
    /// Backend currently used for `open_with_backend`. Demotes from a
    /// HW backend to `Cpu` once the retry budget is exhausted.
    backend: DecoderBackend,
    /// Snapshot of the operator's chosen backend at task bring-up.
    /// Stable across the loop's lifetime — only used for the warning
    /// event detail so the operator knows which HW path was attempted.
    requested_backend: DecoderBackend,
    /// `true` once we've degraded to CPU on this run. Set by either
    /// the open-time soft-fallback (Section 2 of the display fix), the
    /// open-retry exhaustion path in `open_video_decoder_with_retry`,
    /// or the runtime fallback (`force_cpu_fallback`).
    fell_back_to_cpu: bool,
    /// One-shot gate so the warning event fires exactly once per
    /// fallback, never per access unit.
    fallback_event_emitted: bool,
    /// Sustained `send_packet_with_pts` failure counter. Bumped from
    /// `feed_video_decoder` on every `Err`; reset to 0 from
    /// `drain_video_frames` on the first frame after a failure run
    /// (proves the decode path recovered). Crossing
    /// `RUNTIME_FAIL_DEMOTE_THRESHOLD` while still on a HW backend
    /// triggers `force_cpu_fallback` — catches the "QSV opens but
    /// every send_packet returns EINVAL on Arrow Lake / fresh iHD"
    /// pattern that today's silent `let _ = ...` swallowing hides.
    consecutive_send_errors: u32,
    /// Last wall-clock at which `feed_video_decoder` emitted a
    /// `tracing::warn!` for a send_packet error. One-per-second
    /// throttle so a hard-broken HW backend doesn't flood the log
    /// before the demotion threshold trips.
    last_send_error_log_at: Option<Instant>,
    /// Wall-clock of the first **successful** `send_packet_with_pts`
    /// after the current decoder was opened. `None` means we haven't
    /// fed the decoder a real packet yet (warm-up window). Section 3
    /// watchdog reads this — once it's set + 2500 ms have passed +
    /// `frames_received_since_open == 0` while still on a HW backend,
    /// we emit `display_hw_decode_no_frames` and demote to CPU.
    first_send_after_open: Option<Instant>,
}

/// Sustained run of `send_packet_with_pts` errors after which the
/// runtime path treats the active HW backend as broken and demotes to
/// CPU. ≈ 1 s at 25–30 fps; long enough to ride out legitimate
/// broadcast packet errors (SRT-FEC repair, momentary stream
/// corruption) without flapping back to CPU on every transient. The
/// reset in `drain_video_frames` is what makes that work — a single
/// good frame proves the decode path is live.
const RUNTIME_FAIL_DEMOTE_THRESHOLD: u32 = 30;

/// Watchdog deadline for "decoder accepted packets but never produced
/// a frame". Picked so that legitimate first-frame latency on QSV /
/// VAAPI on Arrow Lake (vendor docs: 100–300 ms) sits comfortably
/// inside the gate, while a hard-broken HW backend trips it within
/// the operator's reaction time. `first_send_after_open` only sets
/// after a successful `send_packet`, so we're already past the
/// "waiting for SPS/PPS / first IDR" gate when the timer arms.
const WATCHDOG_NO_FRAMES_MS: u64 = 2_500;

/// Re-arm period for the unsupported-pix-fmt warning event in
/// `drain_video_frames`. The original 1-shot gate hid sustained drops
/// after the first warning; re-emitting once a minute keeps the
/// operator's event feed legible without flooding it. The accompanying
/// `frames_dropped_unsupported_pixfmt` counter tracks the per-frame
/// rate independently.
const UNSUPPORTED_PIXFMT_RE_EMIT_S: u64 = 60;

/// Open a `VideoDecoder` with bounded HW retry + CPU fallback.
///
/// On a flow restart that overlaps the previous flow's HW context
/// (typical when the operator toggles `recording.enabled`, since that
/// triggers a `destroy_flow` → `create_flow` round-trip), the GPU
/// driver may return "no free session" / "device busy" for a brief
/// window. We retry up to 3 times at 50 / 100 / 200 ms before giving
/// up on HW and switching this run to CPU decode — broadcast outputs
/// must come back to picture, even at the cost of operator-chosen HW.
///
/// On the CPU branch (operator-chosen or post-fallback) we open once
/// with no sleep; CPU decoder open is cheap and never contends.
fn open_video_decoder_with_retry(
    codec: VideoCodec,
    state: &mut HwOpenState,
    counters: &DisplayStatsCounters,
    event_sender: &EventSender,
    flow_id: &str,
    output_id: &str,
) -> Option<VideoDecoder> {
    if matches!(state.backend, DecoderBackend::Cpu) {
        return VideoDecoder::open_with_backend(codec, DecoderBackend::Cpu).ok();
    }

    const ATTEMPT_DELAYS_MS: [u64; 3] = [50, 100, 200];
    let mut last_err: Option<String> = None;
    for (attempt_idx, delay_ms) in
        std::iter::once(0u64).chain(ATTEMPT_DELAYS_MS.iter().copied()).enumerate()
    {
        if delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        match VideoDecoder::open_with_backend(codec, state.backend) {
            Ok(d) => {
                if attempt_idx > 0 {
                    tracing::info!(
                        "display HW decoder opened on attempt {} (flow='{flow_id}', output='{output_id}')",
                        attempt_idx + 1,
                    );
                }
                return Some(d);
            }
            Err(e) => {
                last_err = Some(e.to_string());
            }
        }
    }

    // Retry budget exhausted. Demote to CPU for the rest of this run.
    state.backend = DecoderBackend::Cpu;
    state.fell_back_to_cpu = true;
    counters.decoder_demotions.fetch_add(1, Ordering::Relaxed);
    if !state.fallback_event_emitted {
        state.fallback_event_emitted = true;
        let requested = backend_name(state.requested_backend);
        let last_error = last_err.clone().unwrap_or_else(|| "unknown".to_string());
        event_sender.emit_flow_with_details(
            EventSeverity::Warning,
            crate::manager::events::category::SYSTEM_RESOURCES,
            format!(
                "Display HW decode unavailable on flow '{flow_id}' — fell back to CPU after 3 attempts"
            ),
            flow_id,
            serde_json::json!({
                "error_code": "display_hw_decode_unavailable",
                "output_id": output_id,
                "requested_backend": requested,
                "fell_back_to": "cpu",
                "attempts": ATTEMPT_DELAYS_MS.len() + 1,
                "last_error": last_error,
            }),
        );
    }
    VideoDecoder::open_with_backend(codec, DecoderBackend::Cpu).ok()
}

fn ensure_video_decoder(
    slot: &mut Option<VideoDecoder>,
    current: &mut Option<VideoCodec>,
    desired: VideoCodec,
    state: &mut HwOpenState,
    counters: &DisplayStatsCounters,
    event_sender: &EventSender,
    flow_id: &str,
    output_id: &str,
) {
    if *current == Some(desired) && slot.is_some() {
        return;
    }
    *current = Some(desired);
    *slot = open_video_decoder_with_retry(desired, state, counters, event_sender, flow_id, output_id);
    if slot.is_some() {
        // Fresh decoder — re-arm the watchdog/error window. The watchdog
        // gates on `first_send_after_open` being set, so leaving it None
        // here is correct — the next successful send_packet will arm it.
        reset_decoder_open_window(state, counters);
    }
}

/// Stable short name for an HW backend, used in event details and log
/// fields. Mirrors the JSON wire shape that the manager UI's flow card
/// matches against.
fn backend_name(backend: DecoderBackend) -> &'static str {
    match backend {
        DecoderBackend::Cpu => "cpu",
        DecoderBackend::Nvdec => "nvdec",
        DecoderBackend::Qsv => "qsv",
        DecoderBackend::Vaapi => "vaapi",
    }
}

/// Reset the per-open accounting on `HwOpenState` + `DisplayStatsCounters`.
/// Called on every fresh decoder open and on every decoder flush
/// (`pts_jump`, broadcast `Lagged`) — both are conceptually a re-open
/// from the watchdog's POV.
fn reset_decoder_open_window(state: &mut HwOpenState, counters: &DisplayStatsCounters) {
    state.consecutive_send_errors = 0;
    state.first_send_after_open = None;
    counters
        .frames_received_since_open
        .store(0, Ordering::Relaxed);
}

/// Switch the active decoder from a HW backend to CPU mid-flight.
/// Called by both the runtime send-error threshold (Section 1) and the
/// watchdog (Section 3). Idempotent on repeat calls — subsequent calls
/// are no-ops because `state.backend` is already `Cpu`. Emits one
/// Warning event with the supplied `trigger` so the manager UI can
/// distinguish "send_packet kept failing" from "decoder accepted
/// packets but never produced a frame". Single-shot via
/// `state.fallback_event_emitted`.
#[allow(clippy::too_many_arguments)]
fn force_cpu_fallback(
    slot: &mut Option<VideoDecoder>,
    current: &mut Option<VideoCodec>,
    state: &mut HwOpenState,
    counters: &DisplayStatsCounters,
    event_sender: &EventSender,
    flow_id: &str,
    output_id: &str,
    trigger: &'static str,
    last_error: Option<String>,
) {
    if matches!(state.backend, DecoderBackend::Cpu) {
        // Already on CPU — nothing to demote. This branch is hit by
        // the watchdog when a previous fallback already moved us off
        // HW, plus by the send-error threshold path on a re-entry.
        return;
    }
    state.backend = DecoderBackend::Cpu;
    state.fell_back_to_cpu = true;
    *slot = None;
    *current = None;
    reset_decoder_open_window(state, counters);
    counters.decoder_demotions.fetch_add(1, Ordering::Relaxed);
    if !state.fallback_event_emitted {
        state.fallback_event_emitted = true;
        let requested = backend_name(state.requested_backend);
        event_sender.emit_flow_with_details(
            EventSeverity::Warning,
            crate::manager::events::category::SYSTEM_RESOURCES,
            format!(
                "display output '{output_id}': HW decode failed at runtime ({trigger}); \
                 fell back to CPU"
            ),
            flow_id,
            serde_json::json!({
                "error_code": "display_hw_decode_runtime_failed",
                "output_id": output_id,
                "requested_backend": requested,
                "fell_back_to": "cpu",
                "trigger": trigger,
                "last_error": last_error.unwrap_or_else(|| "unknown".to_string()),
            }),
        );
    }
}

/// One iteration of the demux → decode → drain → watchdog pipeline for
/// a single H.264 / HEVC access unit. Centralised here so the H264 and
/// H265 arms in `demux_decode_loop` stay one-line dispatches and every
/// piece of book-keeping (pts_jump flush, decoder ensure, send,
/// drain, watchdog) lives in lock-step in one place.
#[allow(clippy::too_many_arguments)]
fn handle_video_au(
    nalus: &[Vec<u8>],
    pts: u64,
    codec: VideoCodec,
    last_video_pts: &mut Option<u64>,
    video_decoder: &mut Option<VideoDecoder>,
    current_video_codec: &mut Option<VideoCodec>,
    aac_decoder: &mut Option<AacDecoder>,
    ff_audio_decoder: &mut Option<FfAudioDecoder>,
    hw_open_state: &mut HwOpenState,
    last_unsupported_pixfmt_at: &mut Option<Instant>,
    counters: &DisplayStatsCounters,
    vtx: &mpsc::Sender<VideoFrame>,
    event_sender: &EventSender,
    flow_id: &str,
    output_id: &str,
) {
    if pts_jump(*last_video_pts, pts) {
        // Section 5: count + log every PTS jump so the operator can
        // tell whether the Reolink "degraded picture" is the
        // SRT-FEC-repair-out-of-order-PTS hypothesis.
        let prev = (*last_video_pts).unwrap_or(0);
        let forward = pts.wrapping_sub(prev);
        let backward = prev.wrapping_sub(pts);
        counters.pts_jumps_observed.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            output_id = %output_id,
            prev_pts = prev,
            new_pts = pts,
            delta_forward = forward,
            delta_backward = backward,
            "display pts_jump → decoder flush",
        );
        if let Some(d) = video_decoder.as_mut() {
            d.flush();
            reset_decoder_open_window(hw_open_state, counters);
        }
        if let Some(d) = aac_decoder.as_mut() {
            d.reset();
        }
        if let Some(d) = ff_audio_decoder.as_mut() {
            d.flush();
        }
    }
    *last_video_pts = Some(pts);
    ensure_video_decoder(
        video_decoder,
        current_video_codec,
        codec,
        hw_open_state,
        counters,
        event_sender,
        flow_id,
        output_id,
    );
    let Some(decoder) = video_decoder.as_mut() else {
        return;
    };
    // Time the full per-AU decode pipeline: synchronous `send_packet`
    // into libavcodec, plus the drain loop that pulls every reorder-
    // buffer frame the AU made available, plus the per-plane `to_vec`
    // copies that move pixels out of the decoder's lifetime. Sustained
    // values past one source frame period on motion-heavy segments are
    // the signature of "decode is slower than real-time on this
    // content" — the failure mode that produces stutter even when
    // blit + queue depth are healthy.
    let decode_start = Instant::now();
    feed_video_decoder(
        decoder,
        nalus,
        pts,
        codec,
        hw_open_state,
        counters,
        flow_id,
        output_id,
    );
    drain_video_frames(
        decoder,
        pts,
        vtx,
        counters,
        event_sender,
        flow_id,
        output_id,
        hw_open_state.backend,
        last_unsupported_pixfmt_at,
        hw_open_state,
    );
    let decode_us = decode_start.elapsed().as_micros() as u64;
    counters.decode_count.fetch_add(1, Ordering::Relaxed);
    counters
        .decode_us_total
        .fetch_add(decode_us, Ordering::Relaxed);
    counters
        .decode_us_max
        .fetch_max(decode_us, Ordering::Relaxed);

    // Section 1: sustained send_packet errors → demote.
    if hw_open_state.consecutive_send_errors >= RUNTIME_FAIL_DEMOTE_THRESHOLD
        && !matches!(hw_open_state.backend, DecoderBackend::Cpu)
    {
        force_cpu_fallback(
            video_decoder,
            current_video_codec,
            hw_open_state,
            counters,
            event_sender,
            flow_id,
            output_id,
            "send_packet_errors",
            None,
        );
        return;
    }

    // Section 3: watchdog for "decoder opened, no frames".
    if !matches!(hw_open_state.backend, DecoderBackend::Cpu)
        && !hw_open_state.fell_back_to_cpu
        && counters.frames_received_since_open.load(Ordering::Relaxed) == 0
    {
        if let Some(first_send) = hw_open_state.first_send_after_open {
            let elapsed = first_send.elapsed();
            if elapsed >= std::time::Duration::from_millis(WATCHDOG_NO_FRAMES_MS) {
                let backend = backend_name(hw_open_state.backend);
                event_sender.emit_flow_with_details(
                    EventSeverity::Warning,
                    crate::manager::events::category::SYSTEM_RESOURCES,
                    format!(
                        "display output '{output_id}': HW decoder ({backend}) accepted \
                         packets but produced no frame in {} ms — falling back to CPU",
                        elapsed.as_millis(),
                    ),
                    flow_id,
                    serde_json::json!({
                        "error_code": "display_hw_decode_no_frames",
                        "output_id": output_id,
                        "backend": backend,
                        "ms_since_first_send": elapsed.as_millis() as u64,
                    }),
                );
                force_cpu_fallback(
                    video_decoder,
                    current_video_codec,
                    hw_open_state,
                    counters,
                    event_sender,
                    flow_id,
                    output_id,
                    "watchdog_no_frames",
                    None,
                );
            }
        }
    }
}

/// True when `pts` is far enough from `prev` that the upstream stream
/// almost certainly changed (operator input switch). 90 kHz × 5 s =
/// 450 000 — a real continuous stream's frame-to-frame delta is well
/// under that even after SRT-FEC heals a multi-second loss; **operator
/// input switches** drop in a brand-new TS stream with an unrelated
/// PCR/PTS base (the delta is essentially random, almost always far
/// past 5 s). The earlier 1-second threshold tripped on every Reolink
/// 4K HEVC source after an FEC repair, flushing the decoder mid-GOP
/// and resetting the watchdog before it could ever fire — that's what
/// caused the "Reolink picture is no good" symptom under both CPU and
/// QSV decode. We also wrap-around-tolerate by computing the minimum
/// of forward and backward distance, since 33-bit PTS roll-over is a
/// real stream event we don't want to mistake for a switch.
const PTS_JUMP_THRESHOLD_90K: u64 = 450_000;

fn pts_jump(prev: Option<u64>, pts: u64) -> bool {
    let Some(p) = prev else {
        return false;
    };
    let forward = pts.wrapping_sub(p);
    let backward = p.wrapping_sub(pts);
    forward.min(backward) > PTS_JUMP_THRESHOLD_90K
}

fn feed_video_decoder(
    decoder: &mut VideoDecoder,
    nalus: &[Vec<u8>],
    pts_90k: u64,
    codec: VideoCodec,
    state: &mut HwOpenState,
    counters: &DisplayStatsCounters,
    flow_id: &str,
    output_id: &str,
) {
    // H.264 / HEVC: concatenate NAL units back into Annex-B form
    // (start codes between each NALU). The demuxer already strips
    // start codes, so we re-add the standard `0x00 0x00 0x00 0x01`
    // prefix.
    //
    // MPEG-2: the demuxer surfaces the elementary stream verbatim
    // already framed by `0x000001XX` start codes — feeding it through
    // the Annex-B prefix would inject a synthetic empty slice
    // (`0x000001 + 0x01 = slice_start_code 0x01`) which the libavcodec
    // mpeg2video decoder mis-parses. Pass the bytes through unchanged.
    //
    // PTS is attached to the input packet so FFmpeg's reorder queue
    // can hand the matching display-order PTS back on `receive_frame`.
    let buf = match codec {
        VideoCodec::Mpeg2 => {
            let total = nalus.iter().map(|n| n.len()).sum::<usize>();
            let mut buf = Vec::with_capacity(total);
            for n in nalus {
                buf.extend_from_slice(n);
            }
            buf
        }
        VideoCodec::H264 | VideoCodec::Hevc => {
            let total = nalus.iter().map(|n| n.len() + 4).sum::<usize>();
            let mut buf = Vec::with_capacity(total);
            for n in nalus {
                buf.extend_from_slice(&[0, 0, 0, 1]);
                buf.extend_from_slice(n);
            }
            buf
        }
    };
    match decoder.send_packet_with_pts(&buf, pts_90k as i64) {
        Ok(()) => {
            if state.first_send_after_open.is_none() {
                // Watchdog arms only after the FIRST successful send,
                // so legitimate startup latency (waiting for SPS/PPS,
                // first IDR) doesn't trip it.
                state.first_send_after_open = Some(Instant::now());
            }
        }
        Err(e) => {
            counters.send_packet_errors.fetch_add(1, Ordering::Relaxed);
            state.consecutive_send_errors = state.consecutive_send_errors.saturating_add(1);
            // Throttle log lines to once per second so a hard-broken
            // backend doesn't flood the log before the demotion
            // threshold trips. Counter still increments per call.
            let now = Instant::now();
            let log_due = state
                .last_send_error_log_at
                .map(|t| now.duration_since(t) >= std::time::Duration::from_secs(1))
                .unwrap_or(true);
            if log_due {
                state.last_send_error_log_at = Some(now);
                tracing::warn!(
                    flow_id = %flow_id,
                    output_id = %output_id,
                    backend = backend_name(state.backend),
                    consecutive_errors = state.consecutive_send_errors,
                    "display decoder send_packet failed: {e}",
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn drain_video_frames(
    decoder: &mut VideoDecoder,
    fallback_pts_90k: u64,
    vtx: &mpsc::Sender<VideoFrame>,
    counters: &DisplayStatsCounters,
    event_sender: &EventSender,
    flow_id: &str,
    output_id: &str,
    backend: DecoderBackend,
    last_unsupported_pixfmt_at: &mut Option<Instant>,
    state: &mut HwOpenState,
) {
    while let Ok(frame) = decoder.receive_frame() {
        // A successful frame proves the decode path is live — reset
        // the per-error window so the runtime fallback only triggers
        // on *sustained* failure, not the legit packet errors a
        // broadcast stream produces.
        state.consecutive_send_errors = 0;
        // Bump the watchdog counter (must happen before the decode
        // continues — the watchdog reads it on every AU iteration).
        counters
            .frames_received_since_open
            .fetch_add(1, Ordering::Relaxed);
        // Prefer the decoder-propagated display-order PTS. With
        // B-frame H.264 / HEVC, the input-feed PTS we held in the
        // outer loop matches the *most recent fed* access unit, not
        // this particular decoded frame — using it would place every
        // frame in a GOP at the same audio-clock offset and the
        // dup/drop logic would misfire on every B-frame. Falling back
        // to the input PTS is fine for I-only streams where the
        // decoder has no chance to reorder.
        let pts_90k = match frame.pts() {
            Some(p) => p as u64,
            None => {
                // Section 5: count + log — sustained growth points at
                // the B-frame display-PTS commit interacting badly
                // with the source.
                counters.frame_pts_fallbacks.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    output_id = %output_id,
                    fallback_pts = fallback_pts_90k,
                    "display decoder produced frame with no PTS — falling back to input PTS",
                );
                fallback_pts_90k
            }
        };
        let width = frame.width();
        let height = frame.height();
        let colorspace = frame.colorspace();
        let color_transfer = frame.color_transfer();
        let full_range = frame.is_full_range();
        let pixel_format = frame.pixel_format();

        // Two arms — each just copies the planes out of the decoder's
        // lifetime and stamps the **decoder's own** pixel format on
        // the outgoing frame. libswscale handles every conversion
        // (NV12 → BGRA, P010LE → BGRA, etc.) natively in its SIMD
        // paths, so no per-pixel reformat runs on the demux thread.
        //
        //   1. CPU decode produces planar YUV (`yuv420p` / `yuv422p`
        //      / `yuv444p` and the 10/12-bit LE planar siblings) —
        //      `yuv_planes()` returns Some, we copy three planes.
        //   2. HW decode produces a semi-planar layout (NV12 / NV16 /
        //      P010LE / P016LE / P210LE / P216LE) — the four
        //      semi-planar accessors are tried in order; the first
        //      `Some` decides the pixel format we stamp.
        let prepared: Option<(Vec<u8>, usize, VideoFrameChroma, i32)> =
            if let Some((y, ys, u, us, v, vs)) = frame.yuv_planes() {
                Some((
                    y.to_vec(),
                    ys,
                    VideoFrameChroma::Planar {
                        u: u.to_vec(),
                        u_stride: us,
                        v: v.to_vec(),
                        v_stride: vs,
                    },
                    pixel_format,
                ))
            } else if let Some((y, ys, uv, uvs)) = frame.nv12_planes() {
                Some((
                    y.to_vec(),
                    ys,
                    VideoFrameChroma::SemiPlanar {
                        uv: uv.to_vec(),
                        uv_stride: uvs,
                    },
                    AVPixelFormat_AV_PIX_FMT_NV12_VAL,
                ))
            } else if let Some((y, ys, uv, uvs)) = frame.nv16_planes() {
                Some((
                    y.to_vec(),
                    ys,
                    VideoFrameChroma::SemiPlanar {
                        uv: uv.to_vec(),
                        uv_stride: uvs,
                    },
                    AVPixelFormat_AV_PIX_FMT_NV16_VAL,
                ))
            } else if let Some((y, ys, uv, uvs, _planar_pix_fmt)) = frame.p01x_planes() {
                // The accessor's `_planar_pix_fmt` hint is unused — we
                // hand the semi-planar P010LE / P016LE straight to
                // libswscale, no planar conversion on our side. Pick
                // the source format off the original `pixel_format`.
                Some((
                    y.to_vec(),
                    ys,
                    VideoFrameChroma::SemiPlanar {
                        uv: uv.to_vec(),
                        uv_stride: uvs,
                    },
                    pixel_format,
                ))
            } else if let Some((y, ys, uv, uvs, _planar_pix_fmt)) = frame.p21x_planes() {
                Some((
                    y.to_vec(),
                    ys,
                    VideoFrameChroma::SemiPlanar {
                        uv: uv.to_vec(),
                        uv_stride: uvs,
                    },
                    pixel_format,
                ))
            } else {
                // Section 5: per-frame counter feeds the manager UI's
                // continuous rate; the event below fires on first
                // sighting and re-arms every UNSUPPORTED_PIXFMT_RE_EMIT_S
                // seconds so a sustained rate stays visible without
                // flooding the event feed.
                counters
                    .frames_dropped_unsupported_pixfmt
                    .fetch_add(1, Ordering::Relaxed);
                let now = Instant::now();
                let due = last_unsupported_pixfmt_at
                    .map(|t| {
                        now.duration_since(t)
                            >= std::time::Duration::from_secs(UNSUPPORTED_PIXFMT_RE_EMIT_S)
                    })
                    .unwrap_or(true);
                if due {
                    *last_unsupported_pixfmt_at = Some(now);
                    let decoder_kind = backend_name(backend);
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning,
                        crate::manager::events::category::SYSTEM_RESOURCES,
                        format!(
                            "display output '{output_id}': unsupported decoded pixel format \
                             {pixel_format} from {decoder_kind} decoder ({width}x{height}) — \
                             frames will be dropped"
                        ),
                        flow_id,
                        serde_json::json!({
                            "error_code": "display_unsupported_pixfmt",
                            "output_id": output_id,
                            "pixel_format": pixel_format,
                            "decoder_kind": decoder_kind,
                            "width": width,
                            "height": height,
                        }),
                    );
                }
                None
            };

        let Some((y, y_stride, chroma, out_pix_fmt)) = prepared else {
            continue;
        };

        let out_frame = VideoFrame {
            y,
            y_stride,
            chroma,
            width,
            height,
            pixel_format: out_pix_fmt,
            colorspace,
            color_transfer,
            full_range,
            pts_90k,
        };
        if vtx.try_send(out_frame).is_err() {
            // Distinct from the display-thread `frames_dropped_late` —
            // this is the demux→display mpsc backing up because per-frame
            // blit/present is taking longer than one source frame period
            // on average. The diagnostic split lets the manager UI show
            // operators "blit is too slow" vs "decode is too slow" vs
            // "frame arrived too late" as three separate signals.
            counters
                .frames_dropped_mpsc_full
                .fetch_add(1, Ordering::Relaxed);
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
    decoder_kind_label: &'static str,
    event_sender: EventSender,
    flow_id: String,
    output_id: String,
    meter: Option<SharedMeter>,
    scaling_mode: DisplayScalingMode,
) {
    // Frame period derived from the observed PTS deltas — used to size
    // the late-drop threshold. 33 ms (30 fps) until we've seen enough
    // frames to estimate.
    let mut frame_period_ms: i64 = 33;
    let mut last_pts: Option<u64> = None;
    let mut scaler: Option<CachedScaler> = None;

    // Resolution autodetect state. Re-armed on PTS jump (input switch)
    // or on any mid-stream source resolution change.
    let mut matched_dims: Option<(u32, u32)> = None;
    let mut stats_registered = false;

    // Wall-clock pacer used when audio is muted (no `AudioClock` to
    // pace against). Anchors on the first frame's PTS + the wall-clock
    // at that moment, then sleeps each subsequent frame until its
    // wall-clock-equivalent display time. Without this, a muted output
    // would consume decoded frames at decoder rate (often a burst
    // followed by idle) and present at the panel's vblank cadence —
    // exactly the "blast then pause" stutter the audio path used to
    // suffer from before the dup/drop logic was replaced. Reset on a
    // resolution change or large PTS jump so a stream switch starts
    // fresh.
    let mut wall_anchor: Option<(u64, Instant)> = None;

    // Audio↔video PTS offset captured on the first video frame after
    // the audio clock arms. The audio task anchors on its very first
    // ALSA `writei`, which fires before the video decoder has produced
    // its first frame — for H.264 with B-frames + IDR-wait latency
    // that's typically 80–150 ms. Without compensation, the very first
    // drift reading is `-decoder_warmup_ms`, and every subsequent
    // frame measures the same baseline drift forever. With our drop
    // threshold at 4× source period (160 ms at 25 fps), even modest
    // motion-jitter pushed past the drop edge — the operator saw
    // stutter at the same spots in the file every loop because that's
    // where the encoder happened to place its heaviest frames relative
    // to the constant offset.
    //
    // We capture the offset once (audio_pts − video_pts at the first
    // recv where audio is armed) and subtract it from drift forever
    // after, zeroing the steady-state baseline. Reset on resolution
    // change / PTS jump (input switch) so the offset re-captures for
    // the new stream.
    let mut av_offset_pts: Option<i64> = None;

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
                wall_anchor = None;
                av_offset_pts = None;
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
                decoder_kind_label,
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
                frame_period_ms = 33;
                matched_dims = None;
                wall_anchor = None;
                av_offset_pts = None;
            } else if (10..=200).contains(&dms) {
                // Light EMA so a one-off long frame doesn't move the
                // window. α = 1/8 is plenty for ≤ 60 fps content.
                frame_period_ms = (frame_period_ms * 7 + dms) / 8;
            }
        }
        last_pts = Some(next.pts_90k);
        let _ = stats_registered;

        // Pace this frame against the audio master clock (or against a
        // wall-clock seeded by the first frame's PTS when audio is
        // muted). The previous revision used a dup/drop scheme on the
        // raw stepped audio clock (which only ticks once per ALSA
        // period, ~20 ms in our config): when a frame was "ahead" we
        // re-presented the previous frame **and dropped the new one**
        // via `continue`, then the next decoded frame measured even
        // further ahead and was dropped the same way. Decoders emit
        // frames in bursts (B-frame reorder buffers, post-Lagged
        // recovery, post-IDR catch-up), so the loop drained the burst
        // at vblank cadence (~16 ms each), drift accumulated past the
        // 1.5× threshold, and the operator saw motion stutter — short
        // spurts of motion separated by held-frame pauses — across
        // every codec / decode backend / source resolution.
        //
        // The new model: never throw a decoded frame away because it
        // arrived early — sleep until its display time and present it.
        // The smoothed clock reader ([`AudioClock::current_pts_90k_smoothed`])
        // interpolates the audio PTS forward by wall-clock between
        // ALSA writes so the per-frame drift estimate is steady to
        // sub-ms instead of swinging ±20 ms across each period
        // boundary. Frames so far behind they can't catch up are
        // still dropped at 2× period — that case still shows up on a
        // genuinely overloaded host or after a long Lagged event and
        // displaying a stale frame is more visible than dropping it.
        // Drop only frames so far behind the audio clock that catching
        // up by re-pacing isn't realistic. 4× source period (160 ms at
        // 25 fps, 100 ms at 60 fps) — slightly more lipsync slack than
        // the lossless target, but the previous 2× threshold tripped
        // on motion-heavy segments where the CPU decoder briefly fell
        // behind real-time and audio (paced on wall-clock) momentarily
        // outran video PTS. Each visible drop reads to the operator as
        // motion stutter, which is more disruptive on a confidence
        // monitor than a few frames of lipsync drift the decoder
        // catches up on naturally.
        let drop_threshold_ms: i64 = (frame_period_ms * 4).max(160);
        let drift_ms_opt: Option<i64> = if let Some(audio_pts) =
            clock.current_pts_90k_smoothed()
        {
            wall_anchor = None;
            // Capture the audio-vs-video baseline once on first frame
            // after audio is armed (and after every reset above). The
            // offset accounts for the decoder's first-frame warmup
            // latency that has no business showing up as a sustained
            // negative drift. After capture, drift = 0 in steady
            // state; only real motion-jitter perturbs it.
            let offset = *av_offset_pts.get_or_insert(
                audio_pts as i64 - next.pts_90k as i64,
            );
            Some(((next.pts_90k as i64 + offset) - audio_pts as i64) / 90)
        } else {
            // Audio muted — pace on wall-clock seeded by the first
            // post-anchor frame.
            let now = Instant::now();
            let (anchor_pts, anchor_at) =
                wall_anchor.get_or_insert_with(|| (next.pts_90k, now));
            let pts_delta_ms = (next.pts_90k.wrapping_sub(*anchor_pts) as i64) / 90;
            let wall_delta_ms = now.duration_since(*anchor_at).as_millis() as i64;
            Some(pts_delta_ms - wall_delta_ms)
        };

        if let Some(drift_ms) = drift_ms_opt {
            counters.store_av_offset_ms(
                drift_ms.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            );
            if drift_ms < -drop_threshold_ms {
                counters.frames_dropped_late.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // Subtract a small margin so we hand the buffer to KMS a hair
            // before its target vblank — the page-flip itself blocks for
            // up to one vblank inside `kms.present()`, so over-sleeping
            // by even a millisecond pushes the actual scan-out a full
            // frame late at 60 Hz.
            const PRESENT_MARGIN_MS: i64 = 2;
            if drift_ms > PRESENT_MARGIN_MS {
                let cap_ms = drop_threshold_ms;
                let sleep_ms = (drift_ms - PRESENT_MARGIN_MS).min(cap_ms) as u64;
                std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
            }
        }

        // Time the blit + page-flip so operators can see at a glance
        // whether display work is keeping up with the source frame
        // period. Includes libswscale colour-convert, the optional HDR
        // tonemap LUT, the audio-bars overlay, and the kernel's vblank
        // wait inside `kms.present()`. On a 60 Hz panel one vblank is
        // ~16.7 ms, so values above ~33 ms mean we missed a vblank
        // slot and the next iteration's pacing has already slipped a
        // frame.
        let blit_start = Instant::now();
        let blit_ok = blit_and_present(&mut kms, &next, &mut scaler, meter.as_ref()).is_ok();
        let blit_us = blit_start.elapsed().as_micros() as u64;
        counters.blit_count.fetch_add(1, Ordering::Relaxed);
        counters.blit_us_total.fetch_add(blit_us, Ordering::Relaxed);
        counters
            .blit_us_max
            .fetch_max(blit_us, Ordering::Relaxed);
        if blit_ok {
            counters.frames_displayed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Cached libswscale context. Held across frames and rebuilt only when
/// the source shape changes — every parameter change costs a fresh
/// `sws_getContext` (heavy) and a fresh `sws_setColorspaceDetails` to
/// reapply the YUV→RGB matrix.
///
/// Also caches the post-libswscale HDR-to-SDR tonemap LUT (PQ or HLG
/// → Rec.709 sRGB), built lazily when the source carries an HDR
/// transfer characteristic. The LUT is 256 bytes — held in L1 across
/// every per-pixel lookup in `apply_bgra`.
struct CachedScaler {
    inner: VideoScaler,
    src_w: u32,
    src_h: u32,
    src_pix_fmt: i32,
    dst_w: u32,
    dst_h: u32,
    src_colorspace: i32,
    src_full_range: bool,
    src_color_transfer: i32,
    /// `Some(lut)` when `src_color_transfer` is PQ (16) or HLG (18).
    /// `None` for SDR transfers (BT.709, BT.601, unspecified) — no
    /// per-pixel work runs in that path.
    hdr_tonemap: Option<crate::display::hdr_tonemap::HdrTonemap>,
}

/// FFmpeg `AVCOL_SPC_*` integers we care about. Repeated here as plain
/// constants so this file doesn't have to depend on libffmpeg-video-sys
/// directly — `VideoScaler::set_yuv_to_rgb_colorspace` accepts any
/// integer libswscale recognises.
const AVCOL_SPC_BT709: i32 = 1;
const AVCOL_SPC_UNSPECIFIED: i32 = 2;
const AVCOL_SPC_SMPTE170M: i32 = 6;

/// `AV_PIX_FMT_*` integers mirrored as plain consts so the dispatch
/// branches in `drain_video_frames` don't have to depend on
/// `libffmpeg-video-sys` directly. Values match the bindgen output
/// for the FFmpeg n7.x line we vendor (`AVPixelFormat_AV_PIX_FMT_*`
/// in the bindings — stable across n7.0 / n7.1). Naming preserves
/// the bindgen original so it's grep-able against the bindings.
#[allow(non_upper_case_globals)]
const AVPixelFormat_AV_PIX_FMT_NV12_VAL: i32 = 23;
#[allow(non_upper_case_globals)]
const AVPixelFormat_AV_PIX_FMT_NV16_VAL: i32 = 101;

/// `AVColorTransferCharacteristic` integer for SMPTE 2084 (PQ / HDR10)
/// — the only transfer that engages the HDR-to-SDR tonemap LUT.
/// Anything else (BT.709, BT.601, `UNSPECIFIED`, ARIB STD-B67 / HLG)
/// bypasses the LUT and presents libswscale's BGRA output unchanged.
/// HLG is omitted on purpose: ARIB STD-B67 was designed to produce a
/// sensible picture on a vanilla sRGB display without any tonemap
/// (the panel's own EOTF approximately inverts the HLG OETF), so
/// applying a LUT to it would only darken midtones.
const AVCOL_TRC_SMPTE2084: i32 = 16;

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

#[allow(clippy::too_many_arguments)]
fn ensure_scaler(
    cache: &mut Option<CachedScaler>,
    src_w: u32,
    src_h: u32,
    src_pix_fmt: i32,
    dst_w: u32,
    dst_h: u32,
    src_colorspace: i32,
    src_full_range: bool,
    src_color_transfer: i32,
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
                || c.src_color_transfer != src_color_transfer
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
        // Build the HDR → SDR tonemap LUT once per source-shape change
        // when the bitstream signals PQ. HLG (`ARIB_STD_B67`) is
        // backward-compatible with sRGB display by design — the panel's
        // sRGB EOTF approximately inverts the HLG OETF without any
        // per-pixel intervention from us. SDR sources (BT.709 / BT.601
        // / unspecified) and HLG both fall through to `None` here, so
        // `blit_and_present` does no per-pixel work beyond what
        // libswscale already produced.
        let hdr_tonemap = match src_color_transfer {
            AVCOL_TRC_SMPTE2084 => Some(crate::display::hdr_tonemap::HdrTonemap::for_pq()),
            // ARIB STD-B67 (HLG) and everything else: pass through
            // libswscale's BGRA unchanged.
            _ => None,
        };
        *cache = Some(CachedScaler {
            inner,
            src_w,
            src_h,
            src_pix_fmt,
            dst_w,
            dst_h,
            src_colorspace,
            src_full_range,
            src_color_transfer,
            hdr_tonemap,
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
        frame.color_transfer,
    )?;
    match &frame.chroma {
        VideoFrameChroma::Planar {
            u,
            u_stride,
            v,
            v_stride,
        } => cached
            .inner
            .scale_raw_planes_into_packed(
                src_w,
                src_h,
                frame.pixel_format,
                &frame.y,
                frame.y_stride,
                u,
                *u_stride,
                v,
                *v_stride,
                dst,
                pitch,
            )
            .map_err(|e| anyhow::anyhow!("display scale failed: {e}"))?,
        VideoFrameChroma::SemiPlanar { uv, uv_stride } => cached
            .inner
            .scale_semi_planar_into_packed(
                src_w,
                src_h,
                &frame.y,
                frame.y_stride,
                uv,
                *uv_stride,
                dst,
                pitch,
            )
            .map_err(|e| anyhow::anyhow!("display scale failed: {e}"))?,
    }

    // HDR → SDR tonemap. libswscale's YUV→RGB matrix gives BGRA values
    // that are still PQ- or HLG-encoded; without this LUT a UHD HDR
    // contribution feed displays as a dim, low-contrast frame on a
    // Rec.709 confidence panel. SDR sources skip the loop body
    // entirely — `hdr_tonemap` is `None` for `BT709` /
    // `UNSPECIFIED` / `SMPTE170M` transfers. **Apply before** audio
    // bars so the operator's overlay stays at fixed sRGB-correct
    // brightness instead of getting flattened into the tonemap.
    if let Some(tonemap) = cached.hdr_tonemap.as_ref() {
        tonemap.apply_bgra(dst, pitch, dst_w as usize, dst_h as usize);
    }

    if let Some(snapshot) = meter {
        // Lock-free read: `ArcSwap::load` is one atomic acquire +
        // refcount bump. The display loop never blocks the meter task
        // and never allocates on the hot path.
        let snap = snapshot.load();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test for the chroma discriminator. The display blit
    /// matches on `VideoFrameChroma`, so the enum must keep its two
    /// arms intact and round-trip through a `VideoFrame` literal.
    /// Real format-conversion correctness is covered downstream by
    /// `video-engine`'s `scale_semi_planar_nv12_to_bgra_writes_pixels`
    /// scaler test — there's no per-pixel logic in this file to
    /// unit-test now that the bit-shift / deinterleave helpers are
    /// gone.
    #[test]
    fn video_frame_chroma_arms_round_trip() {
        let planar = VideoFrame {
            y: vec![0; 16],
            y_stride: 4,
            chroma: VideoFrameChroma::Planar {
                u: vec![0; 4],
                u_stride: 2,
                v: vec![0; 4],
                v_stride: 2,
            },
            width: 4,
            height: 4,
            pixel_format: 0, // YUV420P
            colorspace: AVCOL_SPC_BT709,
            color_transfer: 0,
            full_range: false,
            pts_90k: 0,
        };
        match planar.chroma {
            VideoFrameChroma::Planar { .. } => {}
            VideoFrameChroma::SemiPlanar { .. } => panic!("expected Planar arm"),
        }

        let semi = VideoFrame {
            y: vec![0; 16],
            y_stride: 4,
            chroma: VideoFrameChroma::SemiPlanar {
                uv: vec![0; 8],
                uv_stride: 4,
            },
            width: 4,
            height: 4,
            pixel_format: AVPixelFormat_AV_PIX_FMT_NV12_VAL,
            colorspace: AVCOL_SPC_BT709,
            color_transfer: 0,
            full_range: false,
            pts_90k: 0,
        };
        match semi.chroma {
            VideoFrameChroma::SemiPlanar { .. } => {}
            VideoFrameChroma::Planar { .. } => panic!("expected SemiPlanar arm"),
        }
    }
}
