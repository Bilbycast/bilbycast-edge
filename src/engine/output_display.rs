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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::display::audio_bars::{new_shared_meter, SharedMeter, StreamHeader};
use crate::display::audio_meter::spawn_audio_meter;
use crate::display::{audio::AudioBackend, clock::AudioClock, kms::KmsDisplay};
use crate::engine::audio_decode::DecodeStats;
use crate::engine::packet::RtpPacket;
use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};
use crate::engine::video_decode_stats::VideoDecodeStats;
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
/// CPU-blit ceiling: sysmem frames larger than this are refused by the
/// display loop (a 4K libswscale YUV→BGRA convert into a
/// write-combining dumb buffer measured ≈7 s/frame). Zero-copy PRIME
/// frames are exempt. Shared by the top-of-loop gate and the catch-up
/// drain's park condition.
const SW_BLIT_MAX_W: u32 = 1920;
const SW_BLIT_MAX_H: u32 = 1080;

const MPSC_VIDEO_DEPTH: usize = 24;
const MPSC_AUDIO_DEPTH: usize = 64;

/// Grace period the orchestrator waits — AFTER cancellation — for its
/// blocking render children to drain and drop the live `KmsDisplay`
/// (closing the card fd, which releases DRM master). If they don't drain
/// within this window (a child wedged in a blocking syscall the cancel
/// token can't interrupt — `spawn_blocking` threads are detached from
/// `JoinHandle::abort`), the orchestrator force-releases DRM master
/// through a dup'd fd so switching this output to another connector on
/// the same GPU isn't blocked until a process restart. Kept under
/// `FlowRuntime::remove_output`'s 5 s teardown-await so the orchestrator
/// returns on its own (emitting a clean `display_stopped`) instead of
/// being `abort()`ed mid-drain.
const DISPLAY_DRAIN_GRACE: Duration = Duration::from_secs(3);

/// Number of consecutive failed `KmsDisplay::open` attempts (each spaced
/// ~500 ms) after which the open-retry loop escalates ONCE from the
/// initial Warning to a Critical alarm. The connector is being held by
/// another DRM master (a desktop compositor, another edge, or a prior
/// display task wedged on the same GPU) and the picture will stay dark
/// until that clears — the operator needs a persistent, actionable
/// signal rather than a single easy-to-miss Warning. ~10 s at 500 ms.
const KMS_BUSY_CRITICAL_ATTEMPTS: u32 = 20;

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
    claim_registry: Arc<crate::display::claim_registry::DisplayClaimRegistry>,
    master_clock: crate::engine::master_clock::MasterClockHandle,
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
            claim_registry,
            master_clock,
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
    claim_registry: Arc<crate::display::claim_registry::DisplayClaimRegistry>,
    master_clock: crate::engine::master_clock::MasterClockHandle,
) -> Result<()> {
    use crate::display::claim_registry::{ClaimKey, ClaimOutcome};

    // 0. Acquire the per-edge runtime claim on this `(device, audio_device)`
    //    pair. KMS only lets one master lease a connector and ALSA only
    //    lets one writer hold the PCM device, so two outputs targeting
    //    the same pair must serialise. The first to start wins; the
    //    others park here in FCFS order until the holder releases or
    //    their per-output cancel token fires. The slot is bound BEFORE
    //    `KmsDisplay::open` so a failed open also releases it for the
    //    next waiter (`Drop` of the guard hands the slot off atomically
    //    under the per-key shard lock).
    let claim_key = ClaimKey::new(config.device.clone(), config.audio_device.clone());
    let _claim_guard = match claim_registry.try_claim(
        claim_key.clone(),
        flow_id.clone(),
        config.id.clone(),
        cancel.child_token(),
    ) {
        ClaimOutcome::Granted(g) => g,
        ClaimOutcome::Queued(ticket) => {
            let info = ticket.info.clone();
            event_sender.emit_with_details(
                EventSeverity::Info,
                "display",
                format!(
                    "display output '{}' waiting for {}: held by flow '{}', output '{}' \
                     (queue position {})",
                    config.id,
                    config.device,
                    info.holder_flow_id.as_deref().unwrap_or("?"),
                    info.holder_output_id.as_deref().unwrap_or("?"),
                    info.queue_position,
                ),
                Some(&flow_id),
                serde_json::json!({
                    "error_code": "display_output_waiting",
                    "output_id": config.id,
                    "device": config.device,
                    "audio_device": config.audio_device.clone().unwrap_or_default(),
                    "holder_flow_id": info.holder_flow_id,
                    "holder_output_id": info.holder_output_id,
                    "queue_position": info.queue_position,
                }),
            );
            match ticket.wait_for_grant().await {
                Ok(g) => {
                    event_sender.emit_with_details(
                        EventSeverity::Info,
                        "display",
                        format!(
                            "display output '{}' acquired {} (was queued behind '{}')",
                            config.id,
                            config.device,
                            info.holder_output_id.as_deref().unwrap_or("?"),
                        ),
                        Some(&flow_id),
                        serde_json::json!({
                            "error_code": "display_output_acquired",
                            "output_id": config.id,
                            "device": config.device,
                            "audio_device": config.audio_device.clone().unwrap_or_default(),
                            "previous_holder_flow_id": info.holder_flow_id,
                            "previous_holder_output_id": info.holder_output_id,
                        }),
                    );
                    g
                }
                Err(_) => {
                    // Cancelled while parked — the per-output cancel token
                    // fired (flow stop, RemoveOutput, edge shutdown). Clean
                    // exit; no `display_started` was emitted, so no
                    // `display_stopped` event is needed either.
                    return Ok(());
                }
            }
        }
    };

    // 1. Resolve the operator's hardware-decode preference against the
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

    // Surface the resolved video-decode backend at startup so an operator can
    // tell hardware from software decode at a glance. A silent CPU fallback on
    // a host that *should* be doing VAAPI — GPU present but the libva backend
    // driver missing, or this binary built without `video-decoder-vaapi` — was
    // previously invisible: the display output just ran hot on CPU and grabbed
    // frames under load with nothing in the logs to say why (issue #70). An
    // explicit `hw_decode` that can't be honoured already emits the
    // `display_hw_decode_unavailable_falling_back` Warning above; here we cover
    // the `Auto → CPU` case (which is otherwise silent) and the success case.
    if matches!(
        resolved,
        crate::engine::hardware_probe::ResolvedDisplayDecoder::Cpu
    ) && matches!(pref, crate::config::models::HwDecodePreference::Auto)
    {
        let any_hw_decoder_compiled = cfg!(feature = "video-decoder-vaapi")
            || cfg!(feature = "video-decoder-nvdec")
            || cfg!(feature = "video-decoder-qsv")
            || cfg!(feature = "video-decoder-rkmpp");
        let hint = if !any_hw_decoder_compiled {
            "no hardware video-decoder compiled into this build — use a `full` \
             release variant (or build with e.g. `video-decoder-vaapi`)"
        } else {
            "no usable hardware decoder detected at startup — on Intel/AMD \
             install a VAAPI driver (`va-driver-all`, plus `intel-media-va-driver` \
             on modern Intel); on NVIDIA the proprietary driver"
        };
        tracing::warn!(
            output_id = %config.id,
            "display '{}' video decode backend: cpu (auto) — {hint}",
            config.id,
        );
    } else {
        tracing::info!(
            output_id = %config.id,
            "display '{}' video decode backend: {}",
            config.id,
            backend_name(backend),
        );
    }

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
    //
    //    **Cross-process serialisation**: the per-edge `DisplayClaimRegistry`
    //    above only serialises display outputs *within this edge process*.
    //    Multiple edges on the same host can also contend for the same
    //    KMS connector — KMS only lets one drm-master hold the CRTC, so
    //    the second edge's `drmSetMaster` / `drmModeSetCrtc` fails until
    //    the holder releases. We treat any KMS open failure as a
    //    transient busy condition and retry with a 500 ms backoff,
    //    listening on the per-output cancel token. The first attempt
    //    failure emits `display_output_waiting` with `reason: "kms_busy"`
    //    so the operator sees the wait through the same UI surface as
    //    the in-process queue. A successful open after retries emits
    //    `display_output_acquired`. Cancellation cleanly exits without
    //    a `display_started`/`display_stopped` pair.
    let mut kms = {
        let mut attempt: u32 = 0;
        let mut emitted_waiting = false;
        let mut emitted_critical = false;
        loop {
            let device = config.device.clone();
            let open_res = tokio::task::spawn_blocking(move || {
                KmsDisplay::open(&device, None, None, None)
            })
            .await
            .map_err(|e| anyhow::anyhow!("kms join: {e}"))?;
            match open_res {
                Ok(k) => {
                    if emitted_waiting {
                        event_sender.emit_with_details(
                            EventSeverity::Info,
                            "display",
                            format!(
                                "display output '{}' acquired {} after {} retry attempt(s)",
                                config.id, config.device, attempt,
                            ),
                            Some(&flow_id),
                            serde_json::json!({
                                "error_code": "display_output_acquired",
                                "output_id": config.id,
                                "device": config.device,
                                "audio_device": config.audio_device.clone().unwrap_or_default(),
                                "reason": "kms_busy_cleared",
                                "attempts": attempt,
                            }),
                        );
                    }
                    break k;
                }
                Err(e) => {
                    let msg = e.to_string();
                    let code = classify_kms_error(&msg);
                    if !emitted_waiting {
                        // First failure — could be either "busy" (another
                        // process holds the connector) or a genuine
                        // configuration problem (wrong connector name,
                        // unplugged cable). Either way we retry: if the
                        // operator misconfigured the device they can
                        // either fix the config (UpdateConfig will
                        // remove + re-add this output cleanly) or stop
                        // the flow (cancel arm wins). Emitting a single
                        // Warning here lets the manager UI flag the
                        // condition without spamming events on every
                        // retry tick.
                        event_sender.emit_with_details(
                            EventSeverity::Warning,
                            "display",
                            format!(
                                "display output '{}' could not open {}: {} — retrying",
                                config.id, config.device, msg,
                            ),
                            Some(&flow_id),
                            serde_json::json!({
                                "error_code": "display_output_waiting",
                                "output_id": config.id,
                                "device": config.device,
                                "audio_device": config.audio_device.clone().unwrap_or_default(),
                                "reason": "kms_busy",
                                "kms_error_code": code,
                                "kms_error_message": msg,
                            }),
                        );
                        emitted_waiting = true;
                    } else {
                        if !emitted_critical && attempt >= KMS_BUSY_CRITICAL_ATTEMPTS {
                            // The connector still won't open ~10 s in. The
                            // lone Warning above is easy to miss and the loop
                            // would otherwise retry silently forever, so
                            // escalate ONCE to Critical: the picture stays
                            // dark until the holder (a desktop compositor,
                            // another edge, or a prior display task wedged on
                            // the same GPU) releases the connector — often a
                            // restart is required. `code` carries the precise
                            // classified cause (`display_master_busy` /
                            // `display_device_invalid` / …).
                            event_sender.emit_with_details(
                                EventSeverity::Critical,
                                "display",
                                format!(
                                    "display output '{}' still cannot open {} after {} attempts: \
                                     {} — the picture will stay dark until the connector is freed; \
                                     if it is held by a desktop compositor or a prior display task \
                                     on the same GPU, stop that holder or restart the edge",
                                    config.id, config.device, attempt, msg,
                                ),
                                Some(&flow_id),
                                serde_json::json!({
                                    "error_code": code,
                                    "output_id": config.id,
                                    "device": config.device,
                                    "audio_device": config.audio_device.clone().unwrap_or_default(),
                                    "reason": "kms_busy_persistent",
                                    "kms_error_message": msg,
                                    "attempts": attempt,
                                }),
                            );
                            emitted_critical = true;
                        } else if attempt % 60 == 0 {
                            // Throttled progress log every ~30 s so an
                            // operator who left the wait running sees that
                            // we're still trying, without spamming the event
                            // log.
                            tracing::debug!(
                                "display output '{}' still waiting on {} after {} attempts: {}",
                                config.id, config.device, attempt, msg,
                            );
                        }
                    }
                    attempt = attempt.saturating_add(1);
                    // Sleep with cancel — exit immediately if the flow
                    // is being torn down.
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                        _ = cancel.cancelled() => {
                            return Ok(());
                        }
                    }
                }
            }
        }
    };

    let chosen_resolution = format!("{}x{}", kms.width(), kms.height());
    let chosen_refresh = kms.refresh_hz();

    // 2. Build the lock-free counters. Stats-handle registration is
    //    deferred to the display child so an `auto`-resolution output
    //    publishes the post-auto-match resolution rather than the
    //    placeholder we opened KMS with.
    let counters = Arc::new(DisplayStatsCounters::default());
    // Seed the audio codec label: `none` for outputs without an
    // `audio_device` (so the manager UI shows "no audio configured"
    // rather than "decoder hasn't latched yet"), `unknown` otherwise.
    // The demux task overwrites this with the real codec discriminant
    // on the first audio frame.
    counters.set_audio_codec_label(
        if config.audio_device.as_deref().unwrap_or("").is_empty() {
            crate::stats::collector::DisplayCodecLabel::None
        } else {
            crate::stats::collector::DisplayCodecLabel::Unknown
        },
    );

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

    // 3. Wire up channels + audio clock.
    let clock = Arc::new(AudioClock::new());
    // Clock reference for the audio resampler. Default (`vsync_to_display`)
    // is audio-master: the resampler holds the DAC buffer and video paces to
    // the measured playout. `genlock` instead locks the measured playout to
    // the flow master clock so the panel stays rate-coherent with the flow's
    // wire outputs; video still follows the playout, so lip-sync is the same.
    let display_master_clock: Option<crate::engine::master_clock::MasterClockHandle> =
        if config.sync_mode == "genlock" {
            Some(master_clock)
        } else {
            None
        };
    let (vtx, vrx) = mpsc::channel::<VideoFrame>(MPSC_VIDEO_DEPTH);
    let (atx, arx) = mpsc::channel::<AudioBlock>(MPSC_AUDIO_DEPTH);
    // Shared switch / Lagged / pts_jump generation. Demux is the sole
    // writer (via `flush_decoders_for_switch`); display + audio loops
    // read on every recv to drop pre-switch decoded blocks without
    // paying their wall time. Eliminates the multi-second "frozen on
    // the previous stream" symptom on hosts where blit + vsync take
    // longer than one source frame period (e.g. 4K-display + sysmem
    // readback iGPU paths).
    let frame_gen = Arc::new(AtomicU64::new(0));

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
            Arc::clone(&counters),
        )
    });

    // Pre-allocate the audio-bars overlay plane up-front — before the
    // demux task spawns — so we can plumb the result into the demux
    // loop's VAAPI download decision. On hosts that lack a usable
    // multi-plane KMS configuration (no atomic, no Overlay / Cursor
    // plane drivable from the CRTC in ARGB8888, etc.) `enable_bars_overlay`
    // returns an error. Previously an operator who set both
    // `show_audio_bars: true` and `hw_decode: vaapi` saw no bars at
    // all on those hosts — the VAAPI zero-copy path produces
    // `VideoFrame { prime: Some(..) }` frames which the display loop
    // scans straight out via `present_prime`, with no dumb buffer to
    // bake bars into and no second plane to compose them onto.
    //
    // The fallback path: force the demux loop to download every VAAPI
    // surface to sysmem (`av_hwframe_transfer_data`) when bars are
    // requested but the overlay plane is unavailable. That makes
    // `frame.is_vaapi()` flip to false, the demux loop builds a
    // planar / semi-planar `VideoFrame`, and the display loop's
    // CPU-blit path bakes the bars into the primary dumb buffer
    // alongside the video. Same cost the HDR-on-SDR path already
    // pays (one PCIe transfer per frame on Intel iHD; pointer-alias
    // on AMD radeonsi) — small price for a useful confidence
    // display when the host can't compose multiple planes.
    let bars_overlay_active = if meter_snapshot.is_some() {
        match kms.enable_bars_overlay() {
            Ok(()) => true,
            Err(e) => {
                tracing::info!(
                    flow_id = %flow_id,
                    output_id = %config.id,
                    "audio-bars overlay plane unavailable — VAAPI frames will demote to CPU-blit so bars stay visible: {e:#}"
                );
                false
            }
        }
    } else {
        false
    };
    counters
        .bars_overlay_enabled
        .store(bars_overlay_active, Ordering::Relaxed);
    // When bars are configured but the overlay plane can't be programmed,
    // the demux loop forces VAAPI surfaces through the CPU-blit path so
    // the display loop can bake bars into the primary dumb buffer.
    let force_cpu_blit_for_bars = Arc::new(AtomicBool::new(
        meter_snapshot.is_some() && !bars_overlay_active,
    ));
    // Sticky cross-task demotion: set by the display task the first time a
    // zero-copy (PRIME) framebuffer import is rejected by the kernel (addfb
    // EINVAL — e.g. an NV12 tiling modifier the scanout plane won't accept,
    // classic on Intel Gen9 Yf_TILED on kernel >= 6.2). Once set, the decode
    // task downloads VAAPI surfaces to sysmem so every subsequent frame takes
    // the working CPU-blit path instead of black-screening on a doomed prime
    // present. Mirrors the `force_cpu_blit_for_bars` sharing pattern.
    let prime_scanout_failed = Arc::new(AtomicBool::new(false));
    // Set by the decode task the first time `rga_rs::nv12_dmabuf_to_sysmem`
    // successfully engages. Read by the display task's pacer to gate a
    // one-time A/V latency calibration — the RGA-accelerated transfer
    // path was measured (2026-07-19, bilby-pir6s) to carry a large,
    // *fixed* extra pipeline latency (roughly 340-400 ms, stable per session,
    // not drifting) that the raw audio-vs-video drift calculation has
    // no way to tell apart from a real backlog: left uncorrected, the
    // catch-up drain fires continuously and the drop rate lands worse
    // than the plain CPU transfer path it was meant to improve on
    // (~31 % vs ~11 %) despite RGA's per-frame cost genuinely being
    // lower. Same class of fix already proven correct for the (KMS-
    // side, since-disabled) Esmart-plane zero-copy attempt earlier
    // this session — mirrors `KmsDisplay::yuv_overlay_active()`'s role
    // there, just for a transfer-path optimization instead of a
    // scanout-plane one, so it carries none of that attempt's
    // plane-compositing risk.
    #[cfg(feature = "rga-transfer")]
    let rga_transfer_active = Arc::new(AtomicBool::new(false));

    // Display is a terminal consumer — it decodes the flow's broadcast
    // channel and renders to KMS + ALSA. No encoder runs, so register a
    // passthrough-true egress descriptor: `build_pipeline_summary` will
    // therefore suppress the `audio_encode` / `video_encode` tags. The
    // `audio_decode` and `video_decode` handles below cause those tags
    // to surface so the manager UI labels the leg as "Audio Decode" /
    // "Video Decode" — never "Transcode".
    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: None,
        video_passthrough: true,
        audio_passthrough: true,
        audio_only: false,
        ..Default::default()
    });
    let audio_decode_counters = Arc::new(DecodeStats::new());
    let video_decode_counters = Arc::new(VideoDecodeStats::new());
    // Register the decode handles up-front so the very first stats
    // snapshot already carries the badge — even before the first frame
    // is decoded. The handles are kept in sync with the live codec /
    // geometry by the demux loop on every successful decode.
    output_stats.set_decode_stats(
        Arc::clone(&audio_decode_counters),
        "",   // codec label lands when the first audio frame is decoded
        0, 0, // sample rate / channels fill in on first decode
    );
    output_stats.set_video_decode_stats(
        Arc::clone(&video_decode_counters),
        "",   // codec label lands on first decoded video frame
        0, 0, 0.0,
    );

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
    let demux_frame_gen = Arc::clone(&frame_gen);
    // Snapshot the panel's HDR signalling capability now (kms is moved
    // into the display task below). The decode loop reads this to
    // decide whether to download HDR-flagged VAAPI surfaces to sysmem
    // for CPU tonemap (panel can't do HDR) or pass them through as
    // zero-copy prime frames + let the display task program
    // `HDR_OUTPUT_METADATA` on the connector (panel does HDR).
    let demux_panel_hdr_capable = kms.panel_hdr_capable();
    let demux_force_cpu_blit_for_bars = Arc::clone(&force_cpu_blit_for_bars);
    let demux_prime_scanout_failed = Arc::clone(&prime_scanout_failed);
    #[cfg(feature = "rga-transfer")]
    let demux_rga_transfer_active = Arc::clone(&rga_transfer_active);
    let demux_output_stats = Arc::clone(&output_stats);
    let demux_audio_decode_counters = Arc::clone(&audio_decode_counters);
    let demux_video_decode_counters = Arc::clone(&video_decode_counters);
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
            demux_frame_gen,
            demux_panel_hdr_capable,
            demux_force_cpu_blit_for_bars,
            demux_prime_scanout_failed,
            #[cfg(feature = "rga-transfer")]
            demux_rga_transfer_active,
            demux_output_stats,
            demux_audio_decode_counters,
            demux_video_decode_counters,
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
    // active mode (e.g. "display (3840x2160@60Hz · vaapi-zerocopy)").
    // Static strings — the resolver runs once per output.
    //
    // VAAPI starts on the zero-copy DMA-BUF + KMS-PRIME scanout path: the
    // decoder produces VAAPI surfaces, the demux loop maps each to an
    // `AVDRMFrameDescriptor`, and the display task atomic-page-flips onto
    // the imported framebuffer without any libswscale or dumb-buffer copy.
    // The label below is therefore the *initial* mode. If the kernel later
    // rejects the PRIME framebuffer import (addfb EINVAL on a tiling
    // modifier the scanout plane won't accept — e.g. Intel Gen9 NV12
    // Yf_TILED on kernel >= 6.2), the display task sets `prime_scanout_failed`
    // and the decode loop demotes to a VAAPI-decode-then-sysmem-blit path for
    // the rest of the run (download_to_sysmem + libswscale + dumb buffer). The
    // static label stays `vaapi-zerocopy`; the one-shot
    // `display_prime_fallback_engaged` Warning event is the operator's signal
    // that the demotion engaged. (A dynamic `vaapi (sysmem)` label could be
    // surfaced off `prime_scanout_failed` later if the UI needs it.)
    //
    // RKMPP mirrors this exactly: `open_video_decoder_with_retry` calls
    // `set_rkmpp_zero_copy(true)` on every display-path RKMPP decoder,
    // so the demux loop's `drain_video_frames` maps native
    // `AV_PIX_FMT_DRM_PRIME` frames straight to the same PRIME scanout
    // path VAAPI uses (no `av_hwframe_map` needed — RKMPP frames are
    // already DRM_PRIME) — same sticky `prime_scanout_failed`
    // fallback, same static label semantics.
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
        (crate::engine::hardware_probe::ResolvedDisplayDecoder::Vaapi, _) => {
            "vaapi-zerocopy"
        }
        (crate::engine::hardware_probe::ResolvedDisplayDecoder::Rkmpp, _) => {
            "rkmpp-zerocopy"
        }
    };
    let display_frame_gen = Arc::clone(&frame_gen);
    let display_force_cpu_blit = Arc::clone(&force_cpu_blit_for_bars);
    let display_prime_scanout_failed = Arc::clone(&prime_scanout_failed);
    #[cfg(feature = "rga-transfer")]
    let display_rga_transfer_active = Arc::clone(&rga_transfer_active);
    // Independent DRM-master release handle (a dup of the card fd) captured
    // before the live `kms` moves into the render thread below. Lets the
    // orchestrator free the connector on teardown even if that thread later
    // wedges and never drops `kms` itself (see DISPLAY_DRAIN_GRACE).
    let master_release = kms.master_release_handle();
    let display_handle = tokio::task::spawn_blocking(move || {
        display_loop(
            kms,
            vrx,
            display_clock,
            display_counters,
            display_cancel,
            display_output_stats,
            decoder_kind_label,
            display_event_sender,
            display_flow_id,
            display_output_id,
            display_meter,
            display_scaling_mode,
            display_frame_gen,
            display_force_cpu_blit,
            display_prime_scanout_failed,
            #[cfg(feature = "rga-transfer")]
            display_rga_transfer_active,
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
    let audio_frame_gen = Arc::clone(&frame_gen);
    let audio_handle = tokio::task::spawn_blocking(move || {
        audio_loop(
            audio_device,
            arx,
            audio_clock,
            audio_counters,
            audio_pair,
            // Stage 1 runs audio-master (None); Stage 2 passes the flow
            // master clock here to switch the renderer into genlock.
            display_master_clock,
            audio_cancel,
            audio_event_sender,
            audio_flow_id,
            audio_output_id,
            audio_frame_gen,
        );
    });

    // Wait for all children to drain (cancellation cascade). On exit each
    // child drops its share of the pipeline; the display child drops the
    // live `KmsDisplay`, closing the card fd and releasing DRM master so
    // the next opener of this connector/CRTC can take over with no process
    // restart.
    //
    // A `spawn_blocking` child can wedge in a syscall the cancel token
    // can't interrupt (ALSA `snd_pcm_writei` on a yanked sink is the
    // classic case) and `JoinHandle::abort` does not reap blocking threads.
    // If that thread is the one holding the card fd it would keep DRM
    // master forever, so an operator who switches this output to another
    // connector on the same GPU sees the new connector stay dark until the
    // edge process exits — exactly the "had to restart the node" symptom.
    // Once cancellation is requested, give the children a bounded grace
    // period to drain cleanly; if they don't, force-release DRM master
    // through the dup'd fd so the connector is freed for the next opener.
    // The wedged thread is abandoned (it dies with the process) but no
    // longer holds the hardware hostage.
    let drain = async {
        let _ = tokio::join!(demux_handle, display_handle, audio_handle);
        if let Some(handle) = meter_handle {
            let _ = handle.await;
        }
    };
    tokio::pin!(drain);
    tokio::select! {
        _ = &mut drain => {}
        _ = cancel.cancelled() => {
            if tokio::time::timeout(DISPLAY_DRAIN_GRACE, &mut drain).await.is_err() {
                if let Some(mr) = master_release.as_ref() {
                    mr.force_drop_master();
                }
                tracing::warn!(
                    "display output '{}' on {}: render threads did not drain within {}s of \
                     cancel — force-released DRM master so the connector is free for the next \
                     opener (a wedged blocking thread is abandoned and dies with the process)",
                    config.id,
                    config.device,
                    DISPLAY_DRAIN_GRACE.as_secs(),
                );
                event_sender.emit_with_details(
                    EventSeverity::Warning,
                    "display",
                    format!(
                        "display output '{}' teardown wedged on {}; force-released DRM master to \
                         free the connector for the next opener",
                        config.id, config.device,
                    ),
                    Some(&flow_id),
                    serde_json::json!({
                        "error_code": "display_teardown_forced_release",
                        "output_id": config.id,
                        "device": config.device,
                        "audio_device": config.audio_device.clone().unwrap_or_default(),
                        "grace_seconds": DISPLAY_DRAIN_GRACE.as_secs(),
                    }),
                );
            }
        }
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
    /// No system-memory chroma — `VideoFrame::prime` carries a DMA-BUF
    /// descriptor instead. The display loop branches on `prime.is_some()`
    /// before reading any of the planar fields.
    None,
}

/// Carries the bits of a VAAPI-decoded surface that the display loop
/// hands to `KmsDisplay::present_prime`. Owns the AVFrame keepalive so
/// the source VA surface stays valid until the page-flip event arrives.
struct PrimePayload {
    descriptor: crate::display::kms::DrmPrimeDescriptor,
    keepalive: std::sync::Arc<dyn crate::display::kms::PrimeKeepalive>,
}

/// Snapshot of the demuxer's currently-locked stream identifiers,
/// captured at the moment a video AU is handed off for decode.
/// Carried alongside each [`VideoFrame`] purely so the local-display
/// confidence overlay can surface them — none of the decode / scale /
/// blit / present code reads these fields.
#[derive(Debug, Clone, Copy, Default)]
struct StreamIds {
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    program_number: Option<u16>,
}

struct VideoFrame {
    /// Y plane (luma) — raw bytes from the decoder, owned so we can
    /// move them across the mpsc out of the decoder's lifetime.
    /// Empty on the VAAPI zero-copy path (where `prime` carries the
    /// scanout-ready DMA-BUF descriptor instead of system-memory pixel
    /// data).
    y: Vec<u8>,
    y_stride: usize,
    /// Chroma — planar (3 planes) or semi-planar (interleaved UV).
    /// See [`VideoFrameChroma`]. Empty on the VAAPI zero-copy path.
    chroma: VideoFrameChroma,
    /// VAAPI zero-copy payload — when `Some`, the display loop calls
    /// `KmsDisplay::present_prime` directly and skips the libswscale
    /// blit + dumb-buffer page-flip. The keepalive holds the source
    /// VA surface alive through the kernel's scanout commit.
    prime: Option<PrimePayload>,
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
    /// Stream generation at production time. Incremented by
    /// `flush_decoders_for_switch` on every operator switch / Lagged /
    /// pts_jump. The display loop reads the shared counter on every
    /// recv; frames whose `frame_gen` is older than the current value
    /// are dropped without blit so the mpsc backlog clears in
    /// microseconds instead of bleeding the previous stream's last
    /// second of decoded video onto the panel after a switch.
    frame_gen: u64,
    /// Source video codec the demuxer locked onto for this AU. Used by
    /// the display loop to populate `DisplayStats.video_codec` so the
    /// manager UI shows the active codec rather than the literal
    /// `"unknown"` string.
    codec: VideoCodec,
    /// Video / audio PID currently locked by the demuxer, plus the
    /// active program number. Carried purely so the local-display
    /// overlay can surface them on the confidence header — none of the
    /// blit / scale / present code touches these fields.
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    program_number: Option<u16>,
}

struct AudioBlock {
    planar: Vec<Vec<f32>>,
    pts_90k: u64,
    sample_rate: u32,
    channels: u8,
    /// Same semantics as [`VideoFrame::frame_gen`] — lets the audio
    /// task drop pre-switch decoded blocks without paying the wall
    /// time of an ALSA `writei`.
    frame_gen: u64,
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
    frame_gen: Arc<AtomicU64>,
    panel_hdr_capable: bool,
    force_cpu_blit_for_bars: Arc<AtomicBool>,
    prime_scanout_failed: Arc<AtomicBool>,
    #[cfg(feature = "rga-transfer")] rga_transfer_active: Arc<AtomicBool>,
    output_stats: Arc<OutputStatsAccumulator>,
    audio_decode_counters: Arc<DecodeStats>,
    video_decode_counters: Arc<VideoDecodeStats>,
) {
    // Track the codec last reported on the audio-decode handle so we
    // only touch the Mutex<String> when the source codec actually
    // changes (cheap, but no point taking the lock on every frame).
    // Video codec / dimensions are updated inside `drain_video_frames`
    // off the decoded frame's own descriptors.
    let mut last_audio_codec_label: Option<&'static str> = None;
    let mut demuxer = TsDemuxer::with_audio_track(program_number, audio_track_index);
    let mut video_decoder: Option<VideoDecoder> = None;
    let mut current_video_codec: Option<VideoCodec> = None;
    let mut deint_state = DeintState::default();
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
        hdr_on_sdr_event_emitted: false,
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
    // Keyframe gate — armed at startup, re-armed by every decoder flush
    // in `flush_decoders_for_switch`. See [`KeyframeGate`].
    let mut keyframe_gate = KeyframeGate::new();
    // Operator-visible "input switch in flight" tracking. Set when the
    // demuxer emits `DemuxedFrame::Discontinuity` (PMT version_number
    // changed — the monotonic stamp `TsContinuityFixer::on_switch`
    // injects on every switch). Cleared on the first decoded video frame
    // that follows. The elapsed_ms goes into `display_input_switch_acquired`
    // so operators can tell whether a multi-second freeze was a long-GOP
    // source (informational) or a genuinely broken pipeline (actionable).
    let mut acquiring_since: Option<Instant> = None;
    // One-shot gate so `display_input_switch_slow_gop` fires at most once
    // per acquiring window — re-armed on the next `Discontinuity` event.
    let mut slow_gop_emitted = false;
    // Re-arming guard: the display path supports planar YUV 4:2:0/4:2:2/
    // 4:4:4 (8/10/12-bit) and semi-planar NV12 / NV16 / P010LE / P016LE
    // / P210LE / P216LE. Any other pixel format from the decoder is
    // silently dropped. Emit a Warning-level event when we first see
    // an unsupported format and then again every
    // `UNSUPPORTED_PIXFMT_RE_EMIT_S` seconds — the per-frame counter
    // (`frames_dropped_unsupported_pixfmt`) gives the manager UI the
    // continuous-rate signal between event emissions.
    let mut last_unsupported_pixfmt_at: Option<Instant> = None;
    // One-shot guard so the operator gets a clear "AC-4 cannot be
    // decoded" event the first time AC-4 audio is observed on this
    // output, but the per-frame demux loop doesn't spam the event
    // ring on every PES packet.
    let mut ac4_audio_warned = false;

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
                flush_decoders_for_switch(
                    &mut video_decoder,
                    &mut aac_decoder,
                    &mut ff_audio_decoder,
                    &mut hw_open_state,
                    &mut deint_state,
                    &counters,
                    &mut last_video_pts,
                    &mut keyframe_gate,
                    &frame_gen,
                    "lagged",
                );
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
        // Snapshot the frame counter before this datagram. After we drain
        // the demuxed AUs, any increment above this value means the new
        // stream successfully produced its first decoded frame — which is
        // what clears `acquiring_since` and fires the `_acquired` event.
        let frames_before = counters.frames_received_since_open.load(Ordering::Relaxed);
        for frame in frames {
            match frame {
                DemuxedFrame::H264 { nalus, pts, is_keyframe } => {
                    let stream_ids = StreamIds {
                        video_pid: demuxer.video_pid(),
                        audio_pid: demuxer.audio_pid(),
                        program_number: demuxer.target_program(),
                    };
                    handle_video_au(
                        &nalus,
                        pts,
                        is_keyframe,
                        VideoCodec::H264,
                        &mut last_video_pts,
                        &mut keyframe_gate,
                        &mut video_decoder,
                        &mut current_video_codec,
                        &mut aac_decoder,
                        &mut ff_audio_decoder,
                        &mut hw_open_state,
                        &mut deint_state,
                        &mut last_unsupported_pixfmt_at,
                        &counters,
                        &vtx,
                        &event_sender,
                        &flow_id,
                        &output_id,
                        &frame_gen,
                        panel_hdr_capable,
                        &force_cpu_blit_for_bars,
                        &prime_scanout_failed,
                        #[cfg(feature = "rga-transfer")]
                        &rga_transfer_active,
                        stream_ids,
                        &video_decode_counters,
                        &output_stats,
                    );
                }
                DemuxedFrame::H265 { nalus, pts, is_keyframe } => {
                    let stream_ids = StreamIds {
                        video_pid: demuxer.video_pid(),
                        audio_pid: demuxer.audio_pid(),
                        program_number: demuxer.target_program(),
                    };
                    handle_video_au(
                        &nalus,
                        pts,
                        is_keyframe,
                        VideoCodec::Hevc,
                        &mut last_video_pts,
                        &mut keyframe_gate,
                        &mut video_decoder,
                        &mut current_video_codec,
                        &mut aac_decoder,
                        &mut ff_audio_decoder,
                        &mut hw_open_state,
                        &mut deint_state,
                        &mut last_unsupported_pixfmt_at,
                        &counters,
                        &vtx,
                        &event_sender,
                        &flow_id,
                        &output_id,
                        &frame_gen,
                        panel_hdr_capable,
                        &force_cpu_blit_for_bars,
                        &prime_scanout_failed,
                        #[cfg(feature = "rga-transfer")]
                        &rga_transfer_active,
                        stream_ids,
                        &video_decode_counters,
                        &output_stats,
                    );
                }
                DemuxedFrame::Mpeg2 { es, pts, is_keyframe } => {
                    // MPEG-2 has no NAL framing — wrap the ES in a single
                    // synthetic "NALU" so we reuse the existing
                    // PTS-jump / decoder-ensure / drain pipeline. The
                    // Annex-B prefix the helper prepends is harmless to
                    // the libavcodec mpeg2video decoder — it scans for
                    // its own start codes (`0x000001B3` etc.) inside the
                    // payload and ignores leading bytes that don't
                    // match.
                    let synthetic = vec![es];
                    let stream_ids = StreamIds {
                        video_pid: demuxer.video_pid(),
                        audio_pid: demuxer.audio_pid(),
                        program_number: demuxer.target_program(),
                    };
                    handle_video_au(
                        &synthetic,
                        pts,
                        is_keyframe,
                        VideoCodec::Mpeg2,
                        &mut last_video_pts,
                        &mut keyframe_gate,
                        &mut video_decoder,
                        &mut current_video_codec,
                        &mut aac_decoder,
                        &mut ff_audio_decoder,
                        &mut hw_open_state,
                        &mut deint_state,
                        &mut last_unsupported_pixfmt_at,
                        &counters,
                        &vtx,
                        &event_sender,
                        &flow_id,
                        &output_id,
                        &frame_gen,
                        panel_hdr_capable,
                        &force_cpu_blit_for_bars,
                        &prime_scanout_failed,
                        #[cfg(feature = "rga-transfer")]
                        &rga_transfer_active,
                        stream_ids,
                        &video_decode_counters,
                        &output_stats,
                    );
                }
                DemuxedFrame::Aac { data, pts } => {
                    counters.set_audio_codec_label(
                        crate::stats::collector::DisplayCodecLabel::Aac,
                    );
                    if last_audio_codec_label != Some("AAC-LC") {
                        if let Some(h) = output_stats.audio_decode_stats_handle() {
                            h.set_input_codec("AAC-LC");
                        }
                        last_audio_codec_label = Some("AAC-LC");
                    }
                    if let Some(asc) = demuxer.cached_aac_config() {
                        if aac_decoder.is_none() {
                            if let Ok(d) = aac_decoder_from_adts_config(asc) {
                                aac_decoder = Some(d);
                            }
                        }
                        if let Some(decoder) = aac_decoder.as_mut() {
                            audio_decode_counters.inc_input();
                            match decoder.decode_frame(&data) {
                                Ok(decoded) => {
                                    audio_decode_counters.inc_output();
                                    let sr = decoder.sample_rate().unwrap_or(48_000);
                                    let ch = decoder.channels().unwrap_or(2);
                                    if let Some(h) = output_stats.audio_decode_stats_handle() {
                                        h.set_output_shape(sr, ch as u8);
                                    }
                                    if atx.try_send(AudioBlock {
                                        planar: decoded.planar,
                                        pts_90k: pts,
                                        sample_rate: sr,
                                        channels: ch,
                                        frame_gen: frame_gen.load(Ordering::Relaxed),
                                    }).is_err()
                                    {
                                        counters
                                            .audio_dropped_mpsc_full
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Err(_) => {
                                    audio_decode_counters.inc_error();
                                }
                            }
                        } else {
                            audio_decode_counters.inc_dropped_uninit();
                        }
                    } else {
                        audio_decode_counters.inc_dropped_uninit();
                    }
                }
                DemuxedFrame::Opus { data, pts } => {
                    counters.set_audio_codec_label(
                        crate::stats::collector::DisplayCodecLabel::Opus,
                    );
                    if last_audio_codec_label != Some("Opus") {
                        if let Some(h) = output_stats.audio_decode_stats_handle() {
                            h.set_input_codec("Opus");
                        }
                        last_audio_codec_label = Some("Opus");
                    }
                    // Opus rides on `stream_type = 0x06` (private_data) +
                    // an Opus registration descriptor. The PES payload is
                    // a sequence of Opus access units, each prefixed by
                    // the Opus-in-MPEG-TS control header (an 11-bit
                    // sync, flags, and the `au_size` length); strip the
                    // headers and feed each Opus packet to libopus via
                    // `FfAudioDecoder` so the ALSA path renders audio
                    // exactly the way it does for AAC / AC-3 / MP2.
                    if current_ff_codec != Some(AudioDecoderCodec::Opus) {
                        ff_audio_decoder = FfAudioDecoder::open(AudioDecoderCodec::Opus).ok();
                        current_ff_codec = Some(AudioDecoderCodec::Opus);
                    }
                    if let Some(decoder) = ff_audio_decoder.as_mut() {
                        // Per-frame PTS interpolation across the PES — see
                        // the `OtherAudio` arm below. An Opus PES commonly
                        // packs ~9 20 ms packets (180 ms), so PES-PTS
                        // stamping sawtoothed the AudioClock hardest here.
                        let mut samples_out: u64 = 0;
                        let mut last_frame_samples: u64 = 0;
                        for frame_bytes in
                            crate::engine::audio_decode::split_opus_frames(&data)
                        {
                            audio_decode_counters.inc_input();
                            if decoder.send_packet(frame_bytes, pts as i64).is_ok() {
                                while let Ok(decoded) = decoder.receive_frame() {
                                    audio_decode_counters.inc_output();
                                    if let Some(h) = output_stats.audio_decode_stats_handle() {
                                        h.set_output_shape(
                                            decoded.sample_rate,
                                            decoded.channels as u8,
                                        );
                                    }
                                    let frame_pts = if decoded.sample_rate > 0 {
                                        pts + samples_out * 90_000
                                            / decoded.sample_rate as u64
                                    } else {
                                        pts
                                    };
                                    last_frame_samples =
                                        decoded.samples_per_channel as u64;
                                    samples_out += last_frame_samples;
                                    if atx.try_send(AudioBlock {
                                        planar: decoded.planar,
                                        pts_90k: frame_pts,
                                        sample_rate: decoded.sample_rate,
                                        channels: decoded.channels,
                                        frame_gen: frame_gen.load(Ordering::Relaxed),
                                    }).is_err()
                                    {
                                        counters
                                            .audio_dropped_mpsc_full
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            } else {
                                audio_decode_counters.inc_error();
                                // See the `OtherAudio` arm — a lost frame
                                // still occupies its mux-schedule slot.
                                samples_out += last_frame_samples;
                            }
                        }
                    } else {
                        audio_decode_counters.inc_dropped_uninit();
                    }
                }
                DemuxedFrame::Discontinuity => {
                    // Operator switched the active input. The fixer's
                    // monotonic PMT version_number bump propagated through
                    // the broadcast channel and the demuxer surfaced it as
                    // this frame variant. Flush every decoder so the next
                    // AU from the new stream becomes the fresh anchor —
                    // without this the libavcodec context keeps trying to
                    // reference frames from the previous stream and either
                    // emits nothing or emits glitched output until its
                    // internal queue drains, which on a long-GOP source can
                    // take many seconds.
                    flush_decoders_for_switch(
                        &mut video_decoder,
                        &mut aac_decoder,
                        &mut ff_audio_decoder,
                        &mut hw_open_state,
                        &mut deint_state,
                        &counters,
                        &mut last_video_pts,
                        &mut keyframe_gate,
                        &frame_gen,
                        "switch",
                    );
                    acquiring_since = Some(std::time::Instant::now());
                    slow_gop_emitted = false;
                    event_sender.emit_flow_with_details(
                        EventSeverity::Info,
                        crate::manager::events::category::FLOW,
                        format!("display output '{output_id}': input switch detected — acquiring video"),
                        &flow_id,
                        serde_json::json!({
                            "error_code": "display_input_switch_acquiring",
                            "output_id": output_id,
                        }),
                    );
                }
                DemuxedFrame::Scte35(_) => {}
                DemuxedFrame::OtherAudio { stream_type, data, pts } => {
                    // AC-4 (synthetic stream_type 0xAC) has no
                    // open-source decoder — surface the codec label so
                    // the manager UI shows "AC-4", emit a one-shot
                    // Warning event explaining why audio is silent, and
                    // drop the PES bytes (video keeps playing).
                    if stream_type == crate::engine::ts_demux::SYNTHETIC_STREAM_TYPE_AC4 {
                        counters.set_audio_codec_label(
                            crate::stats::collector::DisplayCodecLabel::Ac4,
                        );
                        if !ac4_audio_warned {
                            ac4_audio_warned = true;
                            event_sender.emit_flow_with_details(
                                EventSeverity::Warning,
                                crate::manager::events::category::FLOW,
                                format!(
                                    "display output '{output_id}': audio is AC-4 — \
                                     no decoder available, video continues with \
                                     silenced audio"
                                ),
                                &flow_id,
                                serde_json::json!({
                                    "error_code": "display_audio_ac4_undecodable",
                                    "output_id": output_id,
                                }),
                            );
                        }
                        let _ = data;
                        let _ = pts;
                        continue;
                    }
                    let Some(codec) = crate::engine::audio_decode::ff_codec_for_stream_type(
                        stream_type,
                    ) else {
                        continue;
                    };
                    counters.set_audio_codec_label(match codec {
                        AudioDecoderCodec::Mp2 => crate::stats::collector::DisplayCodecLabel::Mp2,
                        AudioDecoderCodec::Ac3 => crate::stats::collector::DisplayCodecLabel::Ac3,
                        AudioDecoderCodec::Eac3 => crate::stats::collector::DisplayCodecLabel::Eac3,
                        AudioDecoderCodec::Opus => crate::stats::collector::DisplayCodecLabel::Opus,
                        AudioDecoderCodec::AacLatm => crate::stats::collector::DisplayCodecLabel::Aac,
                    });
                    let codec_label: &'static str = match codec {
                        AudioDecoderCodec::Mp2 => "MP2",
                        AudioDecoderCodec::Ac3 => "AC-3",
                        AudioDecoderCodec::Eac3 => "E-AC-3",
                        AudioDecoderCodec::Opus => "Opus",
                        AudioDecoderCodec::AacLatm => "AAC-LATM",
                    };
                    if last_audio_codec_label != Some(codec_label) {
                        if let Some(h) = output_stats.audio_decode_stats_handle() {
                            h.set_input_codec(codec_label);
                        }
                        last_audio_codec_label = Some(codec_label);
                    }
                    if current_ff_codec != Some(codec) {
                        ff_audio_decoder = FfAudioDecoder::open(codec).ok();
                        current_ff_codec = Some(codec);
                    }
                    if let Some(decoder) = ff_audio_decoder.as_mut() {
                        // Per-frame PTS interpolation across a multi-frame
                        // PES, mirroring `extract_aac_frames` for ADTS: the
                        // j-th decoded frame is stamped `pes_pts + decoded
                        // samples so far / rate`, not the PES PTS. A DVB
                        // mux packs 2–6 MP2/AC-3 frames per PES; stamping
                        // them all with one PTS made the AudioClock — the
                        // video pacing master — under-read true playout by
                        // (N−1)×frame_duration, a 50–150 ms sawtooth that
                        // presented as motion judder on non-AAC feeds.
                        let mut samples_out: u64 = 0;
                        let mut last_frame_samples: u64 = 0;
                        for frame_bytes in
                            crate::engine::audio_decode::split_audio_codec_frames(&data, codec)
                        {
                            audio_decode_counters.inc_input();
                            if decoder.send_packet(frame_bytes, pts as i64).is_ok() {
                                while let Ok(decoded) = decoder.receive_frame() {
                                    audio_decode_counters.inc_output();
                                    if let Some(h) = output_stats.audio_decode_stats_handle() {
                                        h.set_output_shape(
                                            decoded.sample_rate,
                                            decoded.channels as u8,
                                        );
                                    }
                                    let frame_pts = if decoded.sample_rate > 0 {
                                        pts + samples_out * 90_000
                                            / decoded.sample_rate as u64
                                    } else {
                                        pts
                                    };
                                    last_frame_samples =
                                        decoded.samples_per_channel as u64;
                                    samples_out += last_frame_samples;
                                    if atx.try_send(AudioBlock {
                                        planar: decoded.planar,
                                        pts_90k: frame_pts,
                                        sample_rate: decoded.sample_rate,
                                        channels: decoded.channels,
                                        frame_gen: frame_gen.load(Ordering::Relaxed),
                                    }).is_err()
                                    {
                                        counters
                                            .audio_dropped_mpsc_full
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            } else {
                                audio_decode_counters.inc_error();
                                // The undecodable frame still occupied
                                // its slot in the mux schedule — advance
                                // the interpolation accumulator so the
                                // PES's remaining frames aren't stamped
                                // early (the AudioClock is the video
                                // pacing master; a skipped slot showed
                                // up as ±1-frame pacing wobble).
                                samples_out += last_frame_samples;
                            }
                        }
                    } else {
                        audio_decode_counters.inc_dropped_uninit();
                    }
                }
            }
        }

        // Post-datagram visibility for in-flight input switches.
        if let Some(start) = acquiring_since {
            let frames_now = counters.frames_received_since_open.load(Ordering::Relaxed);
            if frames_now > frames_before {
                // First decoded frame on the new stream landed — switch
                // is visually acquired.
                let elapsed_ms = start.elapsed().as_millis() as u64;
                event_sender.emit_flow_with_details(
                    EventSeverity::Info,
                    crate::manager::events::category::FLOW,
                    format!(
                        "display output '{output_id}': input switch acquired in {elapsed_ms} ms",
                    ),
                    &flow_id,
                    serde_json::json!({
                        "error_code": "display_input_switch_acquired",
                        "output_id": output_id,
                        "elapsed_ms": elapsed_ms,
                    }),
                );
                acquiring_since = None;
                slow_gop_emitted = false;
            } else if !slow_gop_emitted
                && start.elapsed() >= std::time::Duration::from_secs(5)
            {
                // Still waiting after 5 s. Source GOP is long, or the
                // pipeline is genuinely broken — operator's call.
                slow_gop_emitted = true;
                let elapsed_ms = start.elapsed().as_millis() as u64;
                event_sender.emit_flow_with_details(
                    EventSeverity::Warning,
                    crate::manager::events::category::FLOW,
                    format!(
                        "display output '{output_id}': input switch still acquiring after \
                         {elapsed_ms} ms — long source GOP or pipeline issue",
                    ),
                    &flow_id,
                    serde_json::json!({
                        "error_code": "display_input_switch_slow_gop",
                        "output_id": output_id,
                        "elapsed_ms": elapsed_ms,
                    }),
                );
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
    /// One-shot gate so the HDR-on-SDR-panel tonemap warning fires
    /// exactly once per output on the first HDR frame that lands on an
    /// SDR-only connector. Reset whenever the decoder is re-opened
    /// (back to `false` in `HwOpenState::default`) so a flow that
    /// switches sources mid-run lights up the warning again on the
    /// new HDR source.
    hdr_on_sdr_event_emitted: bool,
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
/// good frame proves the decode path is live. Errors accumulated while
/// the keyframe gate is timeout-opened (speculative feed of
/// non-keyframe-anchored AUs) are exempt — see
/// [`KeyframeGate::opened_by_timeout`] and [`SPECULATIVE_DEMOTE_GRACE`].
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
            Ok(mut d) => {
                if attempt_idx > 0 {
                    tracing::info!(
                        "display HW decoder opened on attempt {} (flow='{flow_id}', output='{output_id}')",
                        attempt_idx + 1,
                    );
                }
                if matches!(state.backend, DecoderBackend::Rkmpp) {
                    // Every consumer of this decoder is the display
                    // output — safe to always request the raw
                    // DRM_PRIME frame here and route it through the
                    // zero-copy KMS scanout path below instead of
                    // paying a sysmem download on every frame.
                    d.set_rkmpp_zero_copy(true);
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
/// (`pts_jump`, broadcast `Lagged`, operator switch via
/// `DemuxedFrame::Discontinuity`) — all are conceptually a re-open from
/// the watchdog's POV.
fn reset_decoder_open_window(state: &mut HwOpenState, counters: &DisplayStatsCounters) {
    state.consecutive_send_errors = 0;
    state.first_send_after_open = None;
    counters
        .frames_received_since_open
        .store(0, Ordering::Relaxed);
}

/// Post-flush keyframe gate for the demux loop's video path.
///
/// While armed (startup, and re-armed by every decoder flush in
/// [`flush_decoders_for_switch`]), video AUs are shed until the first
/// keyframe: a freshly-opened/flushed decoder has no reference frames, so
/// pre-IDR slices are guaranteed `send_packet` rejections. Feeding them
/// anyway produced the sporadic AVERROR_INVALIDDATA / EINVAL warnings on
/// every mid-GOP join AND left a latent false HW→CPU demotion — a
/// long-GOP source whose decoder rejects every pre-IDR slice runs
/// `consecutive_send_errors` past [`RUNTIME_FAIL_DEMOTE_THRESHOLD`]
/// before the IDR arrives, demoting a healthy HW backend for the rest of
/// the run.
///
/// The escape hatch for IDR-less H.264 sources (gradual intra refresh
/// signals decodability via a recovery-point SEI the demuxer doesn't
/// parse — `is_keyframe` would stay `false` forever) is **time-based**:
/// [`KEYFRAME_GATE_MAX_WAIT`] after arming, the gate opens
/// unconditionally, restoring the pre-gate behaviour (libavcodec
/// error-conceals its way to a clean picture). Time, not AU count, so
/// the worst-case black period is fps-independent — a 5 fps image
/// source and a 50 fps contribution feed both recover within the same
/// wall-clock bound, close to the pre-gate ~1 intra-refresh-cycle
/// latency. The AU budget remains as a belt-and-braces secondary bound.
/// Broadcast outputs must come back to picture.
struct KeyframeGate {
    waiting: bool,
    skipped: u32,
    armed_at: Instant,
    /// `true` when the gate opened via the time / AU budget rather than
    /// a real keyframe — from that point the demux loop is
    /// **speculatively** feeding non-keyframe-anchored AUs. CPU
    /// (libavcodec) error-conceals its way to a picture from those; a
    /// HW decoder (VAAPI / QSV / NVDEC) legitimately rejects every one
    /// of them with INVALIDDATA. Those rejections say nothing about
    /// the backend's health, so while this flag is set (bounded by
    /// [`SPECULATIVE_DEMOTE_GRACE`]) they must not count toward the
    /// HW→CPU demotion threshold — counting them demoted a perfectly
    /// healthy VAAPI backend on *every* join of a long-GOP source
    /// (gate budget 3 s < GOP, observed 2026-06-11 with a ~4.4 s GOP:
    /// `hw_decode: vaapi` silently ran CPU on every session). Cleared
    /// by the first real keyframe that flows through [`Self::admit`],
    /// which re-anchors the stream and restarts error accounting.
    opened_by_timeout: bool,
}

/// Wall-clock budget per gate episode before giving up on seeing a
/// keyframe — past any sane broadcast GOP (≤ 5 s would be unusual; most
/// are ≤ 2 s), and close to the pre-gate recovery latency of an
/// intra-refresh source.
const KEYFRAME_GATE_MAX_WAIT: Duration = Duration::from_secs(3);

/// Secondary AU-count bound (≈ 10 s at 30 fps) in case the wall clock
/// misbehaves; first bound to trip wins.
const KEYFRAME_GATE_MAX_SKIPPED_AUS: u32 = 300;

/// Upper bound on the speculative-feed demotion immunity. While the
/// gate is timeout-opened, HW `send_packet` rejections are expected and
/// don't count toward [`RUNTIME_FAIL_DEMOTE_THRESHOLD`]; past this
/// budget (measured from the gate arming) the immunity lapses so a
/// genuinely IDR-less source on a HW backend still demotes to CPU —
/// libavcodec's error concealment is the only path to a picture there.
/// 15 s comfortably covers real broadcast GOPs (almost always ≤ 5 s,
/// rarely up to ~10 s) while keeping the worst-case black period on an
/// intra-refresh source bounded.
const SPECULATIVE_DEMOTE_GRACE: Duration = Duration::from_secs(15);

impl KeyframeGate {
    fn new() -> Self {
        Self {
            waiting: true,
            skipped: 0,
            armed_at: Instant::now(),
            opened_by_timeout: false,
        }
    }

    /// Re-arm the gate (decoder was just flushed).
    fn arm(&mut self) {
        self.waiting = true;
        self.skipped = 0;
        self.armed_at = Instant::now();
        self.opened_by_timeout = false;
    }

    /// `true` when the AU should be fed to the decoder. The first
    /// admitted keyframe opens the gate until the next [`Self::arm`]
    /// (and ends a speculative-feed window if the gate had previously
    /// opened by timeout).
    fn admit(&mut self, is_keyframe: bool) -> bool {
        if !self.waiting {
            if is_keyframe && self.opened_by_timeout {
                // First real keyframe after a timeout-open: the stream
                // is now properly anchored — subsequent send errors are
                // meaningful again.
                self.opened_by_timeout = false;
                tracing::debug!(
                    "display keyframe gate: keyframe arrived after timeout-open — \
                     stream re-anchored, HW demotion accounting resumes",
                );
            }
            return true;
        }
        if is_keyframe {
            self.waiting = false;
            self.opened_by_timeout = false;
            return true;
        }
        self.skipped += 1;
        if self.armed_at.elapsed() >= KEYFRAME_GATE_MAX_WAIT
            || self.skipped >= KEYFRAME_GATE_MAX_SKIPPED_AUS
        {
            self.waiting = false;
            self.opened_by_timeout = true;
            tracing::info!(
                skipped = self.skipped,
                waited_ms = self.armed_at.elapsed().as_millis() as u64,
                "display keyframe gate: no keyframe within the budget \
                 (IDR-less intra-refresh source?) — feeding the decoder \
                 anyway",
            );
            return true;
        }
        false
    }

    /// `true` while the gate is timeout-opened and the demux loop is
    /// feeding non-keyframe-anchored AUs the decoder may legitimately
    /// reject. Read by the demotion check in `handle_video_au`.
    fn speculative_feed(&self) -> bool {
        self.opened_by_timeout
    }

    /// Wall-clock since the gate was last armed. Bounds the
    /// speculative-feed demotion immunity.
    fn armed_elapsed(&self) -> Duration {
        self.armed_at.elapsed()
    }
}

/// Shared decoder flush. Used by every "the upstream stream just changed"
/// trigger:
///   - `RecvError::Lagged` (subscriber fell behind, packets were dropped)
///   - `pts_jump` (PTS delta past 5 s — heuristic switch detection)
///   - `DemuxedFrame::Discontinuity` (PMT version_number changed — the
///     monotonic stamp `TsContinuityFixer::on_switch` injects on every
///     operator switch, including dead-input switches)
///
/// `last_video_pts` is cleared so the *next* PTS becomes the fresh anchor
/// without the pts_jump path firing a redundant flush on the same event.
/// The keyframe gate is re-armed so the demux loop sheds pre-IDR AUs
/// instead of feeding a reference-frame-less decoder guaranteed
/// `send_packet` rejections. `reason` rides into the trace so the field
/// is greppable across log lines for the three triggers.
#[allow(clippy::too_many_arguments)]
fn flush_decoders_for_switch(
    video_decoder: &mut Option<VideoDecoder>,
    aac_decoder: &mut Option<AacDecoder>,
    ff_audio_decoder: &mut Option<FfAudioDecoder>,
    state: &mut HwOpenState,
    deint: &mut DeintState,
    counters: &DisplayStatsCounters,
    last_video_pts: &mut Option<u64>,
    keyframe_gate: &mut KeyframeGate,
    frame_gen: &AtomicU64,
    reason: &'static str,
) {
    if let Some(d) = video_decoder.as_mut() {
        d.flush();
        reset_decoder_open_window(state, counters);
    }
    if let Some(d) = aac_decoder.as_mut() {
        d.reset();
    }
    if let Some(d) = ff_audio_decoder.as_mut() {
        d.flush();
    }
    *last_video_pts = None;
    // Drop the deinterlacer's PTS tracking across the epoch boundary so
    // a cross-stream delta can't corrupt the field-duration estimate;
    // the estimate itself and the one-shot event gate survive (same
    // panel, likely the same format family — and re-emitting the
    // "deinterlace engaged" Info on every switch would be spam).
    deint.last_pts_90k = None;
    deint.pending_delta_90k = None;
    keyframe_gate.arm();
    // Bump the shared generation counter so any decoded frames already
    // queued in the demux→display + demux→audio mpscs get dropped on
    // arrival instead of bleeding the previous stream's last second of
    // video onto the panel after a switch. Order: increment AFTER the
    // libavcodec flushes so the first new-stream frame stamps the new
    // gen value (frames decoded from packets fed BEFORE the flush carry
    // the old gen and get dropped by the display/audio loops).
    frame_gen.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(reason = reason, "display decoder flush");
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
#[allow(clippy::too_many_arguments)]
fn handle_video_au(
    nalus: &[Vec<u8>],
    pts: u64,
    is_keyframe: bool,
    codec: VideoCodec,
    last_video_pts: &mut Option<u64>,
    keyframe_gate: &mut KeyframeGate,
    video_decoder: &mut Option<VideoDecoder>,
    current_video_codec: &mut Option<VideoCodec>,
    aac_decoder: &mut Option<AacDecoder>,
    ff_audio_decoder: &mut Option<FfAudioDecoder>,
    hw_open_state: &mut HwOpenState,
    deint: &mut DeintState,
    last_unsupported_pixfmt_at: &mut Option<Instant>,
    counters: &DisplayStatsCounters,
    vtx: &mpsc::Sender<VideoFrame>,
    event_sender: &EventSender,
    flow_id: &str,
    output_id: &str,
    frame_gen: &AtomicU64,
    panel_hdr_capable: bool,
    force_cpu_blit_for_bars: &AtomicBool,
    prime_scanout_failed: &AtomicBool,
    #[cfg(feature = "rga-transfer")] rga_transfer_active: &AtomicBool,
    stream_ids: StreamIds,
    video_decode_counters: &VideoDecodeStats,
    output_stats: &OutputStatsAccumulator,
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
        flush_decoders_for_switch(
            video_decoder,
            aac_decoder,
            ff_audio_decoder,
            hw_open_state,
            deint,
            counters,
            last_video_pts,
            keyframe_gate,
            frame_gen,
            "pts_jump",
        );
    }
    *last_video_pts = Some(pts);
    // Keyframe gate (armed at startup + re-armed by every flush above,
    // including the pts_jump one this very AU may have triggered): shed
    // AUs until the stream's next keyframe — a flushed decoder has no
    // reference frames, so feeding it pre-IDR slices only manufactures
    // send_packet errors. A keyframe AU that itself trips pts_jump
    // flushes and is then fed immediately, so a clean cut switch costs
    // zero extra frames. `last_video_pts` is still advanced for skipped
    // AUs (above) so the jump detector stays anchored to the live stream
    // while the gate is closed.
    let was_speculative = keyframe_gate.speculative_feed();
    if !keyframe_gate.admit(is_keyframe) {
        counters
            .aus_skipped_awaiting_keyframe
            .fetch_add(1, Ordering::Relaxed);
        return;
    }
    if was_speculative && is_keyframe {
        // The stream just re-anchored on a real keyframe after a
        // timeout-opened (speculative) feed window. Whatever
        // send_packet rejections the garbage AUs accumulated say
        // nothing about the backend — start the demotion window fresh
        // from this anchor so only post-keyframe failures count. The
        // no-frames watchdog timer re-arms too: it may have started on
        // an *accepted* garbage send seconds ago, and letting that
        // stale anchor count down would demote a healthy HW backend
        // during the IDR's legitimate first-frame latency.
        hw_open_state.consecutive_send_errors = 0;
        hw_open_state.first_send_after_open = None;
    }
    // Count the access unit as a decode-stage input frame the moment we
    // resolve a decoder for it. Output frames are bumped per
    // `receive_frame` success inside `drain_video_frames`. Gated AUs
    // (above) are tracked on `aus_skipped_awaiting_keyframe` instead —
    // they never reach the decoder.
    video_decode_counters.inc_input();
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
        deint,
        frame_gen,
        codec,
        panel_hdr_capable,
        force_cpu_blit_for_bars,
        prime_scanout_failed,
        #[cfg(feature = "rga-transfer")]
        rga_transfer_active,
        stream_ids,
        video_decode_counters,
        output_stats,
    );
    let decode_us = decode_start.elapsed().as_micros() as u64;
    counters.decode_count.fetch_add(1, Ordering::Relaxed);
    counters
        .decode_us_total
        .fetch_add(decode_us, Ordering::Relaxed);
    counters
        .decode_us_max
        .fetch_max(decode_us, Ordering::Relaxed);

    // Section 1: sustained send_packet errors → demote. Suspended while
    // the keyframe gate is timeout-opened (speculative feed): a HW
    // decoder rejecting non-keyframe-anchored AUs is behaving
    // correctly, and demoting on those rejections permanently kicked
    // `hw_decode: vaapi` to CPU on every join of a long-GOP source
    // (gate budget < GOP length). The immunity is bounded by
    // SPECULATIVE_DEMOTE_GRACE so an IDR-less intra-refresh source on
    // a HW backend still falls back to CPU (which can error-conceal)
    // rather than staying black forever.
    let speculative_grace = keyframe_gate.speculative_feed()
        && keyframe_gate.armed_elapsed() < SPECULATIVE_DEMOTE_GRACE;
    if hw_open_state.consecutive_send_errors >= RUNTIME_FAIL_DEMOTE_THRESHOLD
        && !matches!(hw_open_state.backend, DecoderBackend::Cpu)
        && !speculative_grace
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

    // Section 3: watchdog for "decoder opened, no frames". Also
    // suspended during the bounded speculative-feed window: a HW
    // decoder that accepts non-keyframe-anchored AUs without producing
    // frames is behaving correctly (nothing decodable has been fed
    // yet) — pre-gate builds demoted healthy VAAPI through exactly
    // this path on every mid-GOP join ("accepted packets but produced
    // no frame in ~2.5 s").
    if !matches!(hw_open_state.backend, DecoderBackend::Cpu)
        && !hw_open_state.fell_back_to_cpu
        && !speculative_grace
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

/// Classify a `blit_and_present` error string (a zero-copy PRIME frame's
/// `present_prime` failure): `true` when it is a PERMANENT scanout
/// rejection that will fail identically on every future frame — the kernel
/// refused the PRIME framebuffer import (`addfb` EINVAL on a tiling
/// modifier the scanout plane won't accept, e.g. Intel Gen9 NV12 Yf_TILED)
/// or the descriptor was malformed. The display task demotes to the
/// CPU-blit (sysmem-download) path on `true`.
///
/// Also covers `display_prime_page_flip_failed` — the framebuffer import
/// succeeded but the kernel refused to actually scan it out (`atomic_commit`
/// or legacy `set_crtc` rejected the plane update). Confirmed on real
/// RK3588 hardware: a well-formed linear NV12 PRIME framebuffer was
/// rejected with EINVAL on every single frame, forever, because this
/// function originally treated every `display_prime_page_flip_failed`
/// message as transient (assuming EBUSY) — the display sat black/frozen
/// with no automatic recovery until manually rolled back. The one case
/// that IS genuinely transient and self-heals — `EBUSY` (a previous
/// nonblocking commit still in flight) — carries "busy" in its message
/// (see the `AtomicCommitError::Busy` arm in `KmsDisplay::present_prime`)
/// and is excluded explicitly so a one-off pipeline collision doesn't
/// falsely tear down zero-copy for the rest of the session.
fn prime_scanout_permanently_rejected(err_msg: &str) -> bool {
    err_msg.contains("display_prime_addfb_failed")
        || err_msg.contains("display_prime_invalid")
        || (err_msg.contains("display_prime_page_flip_failed") && !err_msg.contains("busy"))
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

/// Deinterlace state for one demux-decode run: a one-shot gate for the
/// operator event plus the measured field duration used to offset the
/// second bob field's PTS. Fresh per flow run.
struct DeintState {
    engaged_event_emitted: bool,
    /// Display-order PTS of the previous *interlaced* decoded frame —
    /// consecutive deltas measure the source frame period.
    last_pts_90k: Option<u64>,
    /// Candidate frame-period delta awaiting confirmation. A delta is
    /// adopted into `field_dur_90k` only when two consecutive deltas
    /// agree (±12.5 %) — an isolated gap (one lost/undecodable frame,
    /// a mixed film/video cadence run) would otherwise stamp the next
    /// frame's second field a full frame period late.
    pending_delta_90k: Option<u64>,
    /// One field period in 90 kHz ticks (half the measured frame
    /// period). Defaults to 1800 (20 ms — 50i) until measured.
    field_dur_90k: u64,
}

impl Default for DeintState {
    fn default() -> Self {
        Self {
            engaged_event_emitted: false,
            last_pts_90k: None,
            pending_delta_90k: None,
            field_dur_90k: 1_800,
        }
    }
}

/// Row-replication bob deinterlace of one plane: rebuild a full-height
/// plane using only the rows of one field (`parity` 0 = top-field rows
/// 0,2,4…; 1 = bottom-field rows 1,3,5…), replicating each field row
/// over its missing neighbour. Copies whole rows, so it is
/// element-width agnostic — safe for every 8/10/12-bit planar and
/// semi-planar layout the sysmem dispatch produces (including
/// interleaved-UV rows, whose per-row field parity matches luma).
fn bob_plane(src: &[u8], stride: usize, parity: usize) -> Vec<u8> {
    let mut out = src.to_vec();
    if stride == 0 {
        return out;
    }
    let rows = src.len() / stride;
    if rows < 2 {
        return out;
    }
    for j in 0..rows {
        let mut src_row = (j & !1usize) + parity;
        if src_row >= rows {
            src_row = rows - 1;
        }
        if src_row != j {
            let (dst_off, src_off) = (j * stride, src_row * stride);
            out.copy_within(src_off..src_off + stride, dst_off);
        }
    }
    out
}

/// Bob one field (luma + chroma) out of a prepared sysmem frame. See
/// [`bob_plane`] for the row semantics.
fn bob_field(
    y: &[u8],
    y_stride: usize,
    chroma: &VideoFrameChroma,
    parity: usize,
) -> (Vec<u8>, VideoFrameChroma) {
    let by = bob_plane(y, y_stride, parity);
    let bc = match chroma {
        VideoFrameChroma::Planar { u, u_stride, v, v_stride } => VideoFrameChroma::Planar {
            u: bob_plane(u, *u_stride, parity),
            u_stride: *u_stride,
            v: bob_plane(v, *v_stride, parity),
            v_stride: *v_stride,
        },
        VideoFrameChroma::SemiPlanar { uv, uv_stride } => VideoFrameChroma::SemiPlanar {
            uv: bob_plane(uv, *uv_stride, parity),
            uv_stride: *uv_stride,
        },
        VideoFrameChroma::None => VideoFrameChroma::None,
    };
    (by, bc)
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
    deint: &mut DeintState,
    frame_gen: &AtomicU64,
    codec: VideoCodec,
    panel_hdr_capable: bool,
    force_cpu_blit_for_bars: &AtomicBool,
    prime_scanout_failed: &AtomicBool,
    #[cfg(feature = "rga-transfer")] rga_transfer_active: &AtomicBool,
    stream_ids: StreamIds,
    video_decode_counters: &VideoDecodeStats,
    output_stats: &OutputStatsAccumulator,
) {
    while let Ok(frame) = decoder.receive_frame() {
        video_decode_counters.inc_output();
        // Temporary diagnostic instrumentation (RKMPP display-output
        // stutter investigation): isolate MPP's own frame-fetch cost
        // from our DRM_PRIME→sysmem transfer cost, since the existing
        // decode_us_max/avg counters wrap both plus send_packet.
        let receive_us = decoder.last_receive_frame_us();
        counters
            .receive_frame_us_max
            .fetch_max(receive_us, Ordering::Relaxed);
        counters
            .receive_frame_us_total
            .fetch_add(receive_us, Ordering::Relaxed);
        counters.receive_frame_count.fetch_add(1, Ordering::Relaxed);
        if let Some(transfer_us) = decoder.last_transfer_us() {
            counters
                .rkmpp_transfer_us_max
                .fetch_max(transfer_us, Ordering::Relaxed);
            counters
                .rkmpp_transfer_us_total
                .fetch_add(transfer_us, Ordering::Relaxed);
            counters.rkmpp_transfer_count.fetch_add(1, Ordering::Relaxed);
        }
        // Refresh the registered handle's codec / geometry whenever the
        // resolution changes. Cheap: at most one Mutex<String> lock per
        // resolution change (typically just once per flow run).
        if let Some(h) = output_stats.video_decode_stats_handle() {
            let codec_label: &'static str = match codec {
                VideoCodec::H264 => "h264",
                VideoCodec::Hevc => "hevc",
                VideoCodec::Mpeg2 => "mpeg2",
            };
            h.set_input_codec(codec_label);
            let w = frame.width() as u32;
            let hght = frame.height() as u32;
            if h.output_width.load(Ordering::Relaxed) != w
                || h.output_height.load(Ordering::Relaxed) != hght
            {
                h.set_geometry(w, hght, 0.0);
            }
        }
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
        let interlaced = frame.is_interlaced();
        let top_field_first = frame.top_field_first();

        if interlaced {
            // Track the source frame period off display-order PTS
            // deltas; half of it offsets the second bob field below.
            if let Some(prev) = deint.last_pts_90k {
                let delta = pts_90k.wrapping_sub(prev);
                // 10–100 ms of frame period covers 10–100 fps; a delta
                // outside that is a PTS jump / reorder artefact — keep
                // the previous estimate. Within range, adopt only after
                // two consecutive agreeing deltas (see
                // `pending_delta_90k`).
                if (900..=9_000).contains(&delta) {
                    if let Some(prev_delta) = deint.pending_delta_90k
                        && delta.abs_diff(prev_delta) <= prev_delta / 8
                    {
                        deint.field_dur_90k = delta / 2;
                    }
                    deint.pending_delta_90k = Some(delta);
                } else {
                    deint.pending_delta_90k = None;
                }
            }
            deint.last_pts_90k = Some(pts_90k);
            // One-shot operator visibility: interlaced content is
            // rendered by splitting each frame into its two fields
            // (bob), not by weaving — say so once so a 50-fields/s
            // presentation of a "25 fps" source isn't a surprise.
            if !deint.engaged_event_emitted {
                deint.engaged_event_emitted = true;
                event_sender.emit_output_with_details(
                    EventSeverity::Info,
                    crate::manager::events::category::DISPLAY,
                    format!(
                        "Display output '{output_id}': interlaced source detected \
                         ({width}x{height}) — bob deinterlacing engaged, fields \
                         presented at 2x frame rate"
                    ),
                    output_id,
                    serde_json::json!({
                        "error_code": "display_deinterlace_engaged",
                        "width": width,
                        "height": height,
                        "top_field_first": top_field_first,
                    }),
                );
            }
        }

        // ── HDR routing for VAAPI sources ───────────────────────────
        //
        // Two paths depending on whether the connected panel can
        // signal HDR (kernel + driver expose `HDR_OUTPUT_METADATA`
        // on the connector — checked at `KmsDisplay::open` and
        // captured in `panel_hdr_capable`):
        //
        // * **HDR panel**: leave the frame as a VAAPI surface and
        //   let the display task program `HDR_OUTPUT_METADATA` on
        //   the connector before the next atomic flip. The HDMI / DP
        //   sink transmits the Dynamic Range and Mastering
        //   InfoFrame, the panel switches into HDR mode, and the
        //   panel's own EOTF maps PQ / HLG to its native
        //   luminance. Zero-copy stays on, no CPU tonemap.
        //
        // * **SDR panel**: the only correct option is a CPU
        //   tonemap. Download the VAAPI surface to sysmem
        //   (`av_hwframe_transfer_data`) so `is_vaapi()` flips to
        //   false and the planar / semi-planar codepath below
        //   builds a sysmem `VideoFrame` — the display task's
        //   CPU-blit path then runs the PQ/HLG → SDR tonemap LUT
        //   in `hdr_tonemap::apply_bgra` before scanout. Cost: one
        //   PCIe transfer per HDR frame on Intel iHD; pointer-
        //   alias on AMD radeonsi.
        //
        // PQ = `AVCOL_TRC_SMPTE2084` (16); HLG = `AVCOL_TRC_ARIB_STD_B67` (18).
        let source_is_hdr = color_transfer == 16 || color_transfer == 18;

        // One-shot warning the first time an HDR source lands on an
        // SDR-only connector. The CPU LUT tonemap path that follows is
        // ~1 PCIe transfer per frame on Intel iHD plus the
        // `hdr_tonemap::apply_bgra` overhead — ~25-40 % of a core at
        // 1080p60 — and the operator can't see it from elsewhere in the
        // UI. Surfacing it here lets them decide whether to swap the
        // panel for an HDR-capable one or accept the cost.
        if source_is_hdr && !panel_hdr_capable && !state.hdr_on_sdr_event_emitted {
            let transfer_label = if color_transfer == 16 { "PQ (HDR10)" } else { "HLG" };
            event_sender.emit_output_with_details(
                EventSeverity::Warning,
                crate::manager::events::category::DISPLAY,
                format!(
                    "Display output '{output_id}' received an HDR ({transfer_label}) source on an SDR-only panel — running CPU tonemap on every frame. Swap to an HDR-capable panel to drop CPU back."
                ),
                output_id,
                serde_json::json!({
                    "error_code": "display_hdr_tonemap_active",
                    "color_transfer": color_transfer,
                    "transfer_label": transfer_label,
                    "panel_hdr_capable": false,
                }),
            );
            state.hdr_on_sdr_event_emitted = true;
        }

        // Download VAAPI / RKMPP HW surfaces to sysmem when:
        //   1. HDR source + SDR panel — the display task needs CPU
        //      pixels for the PQ/HLG → SDR tonemap LUT.
        //   2. Bars are configured but the overlay plane couldn't be
        //      programmed (`force_cpu_blit_for_bars`) — the display
        //      task's CPU-blit path is the only way bars reach the
        //      panel; the zero-copy scanout owns the primary plane
        //      and there's no dumb buffer to bake into.
        // Either way: download flips `is_vaapi()`/`is_drm_prime()` to
        // false, the planar / semi-planar codepath below builds a
        // sysmem `VideoFrame`, and the display task's CPU-blit path
        // takes over.
        let is_hw_prime_frame = frame.is_vaapi() || frame.is_drm_prime();
        let over_sw_ceiling =
            frame.width() > 1920 || frame.height() > 1080;
        // Sticky demotion wins over the SW-blit ceiling: once the display
        // task has reported that zero-copy PRIME scanout is rejected on this
        // host, sysmem download + CPU-blit is the ONLY way to a picture, so
        // we pay the libswscale cost even above 1080p rather than black-screen.
        let prime_dead = prime_scanout_failed.load(Ordering::Relaxed);
        // (3. — interlaced source: the PRIME scanout would weave the
        // two fields into one combing progressive frame; the CPU path
        // below bob-deinterlaces instead. Interlaced content tops out
        // at 1080i so the `!over_sw_ceiling` guard never bites in
        // practice.)
        let need_sysmem = is_hw_prime_frame
            && (prime_dead
                || (!over_sw_ceiling
                    && ((source_is_hdr && !panel_hdr_capable)
                        || interlaced
                        || force_cpu_blit_for_bars.load(Ordering::Relaxed))));
        // RGA-accelerated fast path (`rga-transfer` feature, ARM
        // Rockchip only): FFmpeg's generic `download_to_sysmem()`
        // (`av_hwframe_transfer_data`) does an unaccelerated CPU
        // mmap+DMA_BUF_SYNC+memcpy — measured as the dominant per-frame
        // cost behind HDMI display-output stutter on bilby-pir6s
        // (RK3588), spikes up to ~106 ms. Try the Rockchip 2D
        // accelerator first (benchmarked on the same hardware at
        // ~3 ms/frame, 1080p) — restricted to the plain RKMPP-native
        // NV12 case (not HDR/P010LE, which needs the CPU path's
        // tonemap-aware handling downstream) so it never touches the
        // HDR or bars-without-overlay reasons `need_sysmem` can also
        // fire for. On any failure (feature not compiled in, frame not
        // RKMPP-native, RGA call failed), `rga_prepared` stays `None`
        // and every existing code path below runs completely
        // unchanged.
        #[cfg(feature = "rga-transfer")]
        let rga_prepared: Option<(Vec<u8>, usize, VideoFrameChroma, i32)> = 'rga: {
            // `DRM_FORMAT_NV12` fourcc ("NV12" little-endian, matching
            // the value already seen on this exact path's atomic_commit
            // error logs: `fourcc="0x3231564e"`). NOT the same check as
            // `pixel_format == NV12` — `frame.pixel_format()` on a
            // DRM_PRIME frame always reports `AV_PIX_FMT_DRM_PRIME`
            // itself (that's what `is_drm_prime()` tests), never the
            // underlying sw format, so gating on it here would silently
            // reject every frame. The DRM_PRIME descriptor's own fourcc
            // is the correct source for "is this actually 8-bit NV12".
            const DRM_FOURCC_NV12: u32 = 0x3231564e;
            if !need_sysmem || !frame.is_drm_prime() {
                break 'rga None;
            }
            let Ok(prime_frame) = frame.map_drm_prime() else {
                break 'rga None;
            };
            if prime_frame.fourcc != DRM_FOURCC_NV12 {
                break 'rga None;
            }
            let Some(p0) = prime_frame.planes.first() else {
                break 'rga None;
            };
            let uv_offset_rows = prime_frame
                .planes
                .get(1)
                .map(|p1| p1.offset / p0.pitch.max(1))
                .unwrap_or(height);
            match rga_rs::nv12_dmabuf_to_sysmem(
                p0.fd,
                width,
                height,
                p0.pitch,
                uv_offset_rows,
            ) {
                Ok((y, y_stride, uv, uv_stride)) => {
                    // First engagement: log once and flip the shared
                    // flag the display task's pacer reads to gate its
                    // one-time A/V latency calibration (see
                    // `rga_transfer_active`'s doc comment at its
                    // declaration for why that's needed).
                    if !rga_transfer_active.swap(true, Ordering::Relaxed) {
                        tracing::info!(
                            output_id = %output_id,
                            "rga_transfer: hardware transfer engaged (one-shot confirmation)"
                        );
                    }
                    Some((
                        y,
                        y_stride,
                        VideoFrameChroma::SemiPlanar { uv, uv_stride },
                        AVPixelFormat_AV_PIX_FMT_NV12_VAL,
                    ))
                }
                Err(e) => {
                    tracing::debug!(
                        output_id = %output_id,
                        "rga_transfer failed, falling back to CPU download: {e:#}"
                    );
                    None
                }
            }
        };
        #[cfg(not(feature = "rga-transfer"))]
        let rga_prepared: Option<(Vec<u8>, usize, VideoFrameChroma, i32)> = None;
        let frame =
            if need_sysmem && rga_prepared.is_none() {
                match frame.download_to_sysmem() {
                    Ok(sysmem) => sysmem,
                    Err(e) => {
                        // Download failed (rare — driver / format
                        // mismatch). Pass the HW frame through and
                        // let the prime path run — the operator
                        // either sees un-tonemapped HDR (HDR-on-SDR
                        // case) or no bars (bars-without-overlay
                        // case). Sustained failures trip the existing
                        // zero-copy watchdog.
                        tracing::warn!(
                            output_id = %output_id,
                            color_transfer,
                            force_cpu_blit_for_bars = force_cpu_blit_for_bars.load(Ordering::Relaxed),
                            "HW → sysmem download failed ({e:?}) — falling back to zero-copy prime path"
                        );
                        frame
                    }
                }
            } else {
                frame
            };

        // ── VAAPI / RKMPP zero-copy fast path ──────────────────────
        //
        // `is_vaapi()` is true exactly when the decoder was opened on
        // the VAAPI backend AND the `get_format` callback negotiated
        // AV_PIX_FMT_VAAPI for the bitstream profile; `is_drm_prime()`
        // is true when it's RKMPP's native DRM_PRIME output (only
        // reachable when `set_rkmpp_zero_copy(true)` was set on this
        // decoder — see `open_video_decoder_with_retry`). Either way,
        // map the frame to a DRM PRIME descriptor and ship it through
        // the mpsc with the AVFrame keepalive. The display task
        // imports the DMA-BUF as a KMS framebuffer and atomic-page-
        // flips straight onto it — eliminating both the libswscale
        // YUV→BGRA blit and the system-memory copy through the dumb
        // buffer (and, for RKMPP, the `av_hwframe_transfer_data`
        // DRM_PRIME→sysmem download that was previously the dominant
        // per-frame cost — see the `rkmpp_transfer_us_*` stats this
        // fix was built to eliminate).
        //
        // On mapping failure (typical: FFmpeg without CONFIG_LIBDRM,
        // or driver refused to export this profile) VAAPI drops the
        // frame and bumps a counter — the runtime watchdog further
        // upstream will demote the whole decoder to CPU after a
        // window of sustained zero-copy failures, a path proven on
        // Intel/AMD hardware over many prior releases. RKMPP's native
        // DRM_PRIME export is new code with no equivalent hardware
        // track record yet, so rather than risk a black/frozen panel
        // on a bad assumption about the AVDRMFrameDescriptor layout
        // (see `wrap_rkmpp_drm_prime`'s doc comment), a per-frame
        // mapping failure falls back to a sysmem download and rides
        // the existing CPU-blit path below instead of being dropped —
        // worst case this is exactly the pre-zero-copy behaviour.
        //
        // Re-check the frame's format fresh here rather than reuse
        // `is_hw_prime_frame` (computed before the `need_sysmem` block
        // above) — a successful `download_to_sysmem()` there already
        // flipped `is_vaapi()`/`is_drm_prime()` to false, and entering
        // this block on the stale flag would call `map_drm_prime()` on
        // an already-sysmem frame, which always fails
        // (`HwFrameNotOnDevice`, not the RKMPP-specific arm since
        // `is_drm_prime()` is also false by then) and silently drops
        // every frame forever — `frames_displayed` stuck at 0 with no
        // error loop to notice by. Confirmed on real hardware
        // (2026-07-19): the sticky `prime_scanout_failed` demotion
        // correctly engaged after the first present failure, but this
        // stale check then dropped every subsequent frame anyway.
        let frame = if rga_prepared.is_none() && (frame.is_vaapi() || frame.is_drm_prime()) {
            let rkmpp_native = frame.is_drm_prime();
            let zerocopy_kind = if rkmpp_native { "rkmpp-zerocopy" } else { "vaapi-zerocopy" };
            match frame.map_drm_prime() {
                Ok(prime_frame) => {
                    let descriptor = crate::display::kms::DrmPrimeDescriptor {
                        width: prime_frame.width,
                        height: prime_frame.height,
                        fourcc: prime_frame.fourcc,
                        modifier: prime_frame.modifier,
                        planes: prime_frame
                            .planes
                            .iter()
                            .map(|p| crate::display::kms::DrmPrimePlaneDesc {
                                fd: p.fd,
                                offset: p.offset,
                                pitch: p.pitch,
                            })
                            .collect(),
                    };
                    let keepalive = prime_frame.keepalive();
                    let out_frame = VideoFrame {
                        y: Vec::new(),
                        y_stride: 0,
                        chroma: VideoFrameChroma::None,
                        prime: Some(PrimePayload {
                            descriptor,
                            keepalive,
                        }),
                        width,
                        height,
                        pixel_format,
                        colorspace,
                        color_transfer,
                        full_range,
                        pts_90k,
                        frame_gen: frame_gen.load(Ordering::Relaxed),
                        codec,
                        video_pid: stream_ids.video_pid,
                        audio_pid: stream_ids.audio_pid,
                        program_number: stream_ids.program_number,
                    };
                    if vtx.try_send(out_frame).is_err() {
                        counters
                            .frames_dropped_mpsc_full
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                Err(e) if rkmpp_native => {
                    // See the "RKMPP's native DRM_PRIME export" doc
                    // comment above — new, hardware-unproven code path:
                    // recover to sysmem instead of dropping.
                    counters
                        .frames_dropped_unsupported_pixfmt
                        .fetch_add(1, Ordering::Relaxed);
                    let now = Instant::now();
                    let due = last_unsupported_pixfmt_at
                        .map(|t| {
                            now.duration_since(t)
                                >= std::time::Duration::from_secs(
                                    UNSUPPORTED_PIXFMT_RE_EMIT_S,
                                )
                        })
                        .unwrap_or(true);
                    if due {
                        *last_unsupported_pixfmt_at = Some(now);
                        event_sender.emit_flow_with_details(
                            EventSeverity::Warning,
                            crate::manager::events::category::SYSTEM_RESOURCES,
                            format!(
                                "display output '{output_id}': RKMPP DRM PRIME export \
                                 failed ({e}) — falling back to sysmem CPU-blit for this frame"
                            ),
                            flow_id,
                            serde_json::json!({
                                "error_code": "display_rkmpp_prime_export_failed",
                                "output_id": output_id,
                                "decoder_kind": zerocopy_kind,
                                "width": width,
                                "height": height,
                                "ffmpeg_error": format!("{e}"),
                            }),
                        );
                    }
                    // Recover to sysmem and fall through to the
                    // ordinary semi-planar codepath below with it —
                    // NOT a `continue`, unlike every other arm here.
                    match frame.download_to_sysmem() {
                        Ok(sysmem) => sysmem,
                        Err(_) => continue,
                    }
                }
                Err(e) => {
                    counters
                        .frames_dropped_unsupported_pixfmt
                        .fetch_add(1, Ordering::Relaxed);
                    let now = Instant::now();
                    let due = last_unsupported_pixfmt_at
                        .map(|t| {
                            now.duration_since(t)
                                >= std::time::Duration::from_secs(
                                    UNSUPPORTED_PIXFMT_RE_EMIT_S,
                                )
                        })
                        .unwrap_or(true);
                    if due {
                        *last_unsupported_pixfmt_at = Some(now);
                        event_sender.emit_flow_with_details(
                            EventSeverity::Warning,
                            crate::manager::events::category::SYSTEM_RESOURCES,
                            format!(
                                "display output '{output_id}': VAAPI DRM PRIME export \
                                 failed ({e}) — frames dropped, demote pending"
                            ),
                            flow_id,
                            serde_json::json!({
                                "error_code": "display_vaapi_prime_export_failed",
                                "output_id": output_id,
                                "decoder_kind": zerocopy_kind,
                                "width": width,
                                "height": height,
                                "ffmpeg_error": format!("{e}"),
                            }),
                        );
                    }
                    continue;
                }
            }
        } else {
            frame
        };

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
            if let Some(rga) = rga_prepared {
                Some(rga)
            } else if let Some((y, ys, u, us, v, vs)) = frame.yuv_planes() {
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

        // Interlaced sysmem frame → bob deinterlace: present each field
        // as its own full-height frame (missing rows replicated from
        // the field's own rows) at 2× the frame rate, the second field
        // offset by one field duration. Weaving both fields into one
        // progressive scanout — the pre-fix behaviour — combed every
        // moving edge and halved the motion rate on 1080i/576i, still
        // the dominant broadcast emission formats. The display loop's
        // frame-period EMA measures the field cadence, so the
        // auto-match mode picker naturally locks the panel to the
        // *field* rate (50 Hz for 1080i25).
        if interlaced {
            let first_parity: usize = if top_field_first { 0 } else { 1 };
            let cur_gen = frame_gen.load(Ordering::Relaxed);
            for (i, parity) in [first_parity, 1 - first_parity].into_iter().enumerate() {
                let (by, bc) = bob_field(&y, y_stride, &chroma, parity);
                let out_frame = VideoFrame {
                    y: by,
                    y_stride,
                    chroma: bc,
                    prime: None,
                    width,
                    height,
                    pixel_format: out_pix_fmt,
                    colorspace,
                    color_transfer,
                    full_range,
                    pts_90k: pts_90k.wrapping_add(i as u64 * deint.field_dur_90k),
                    frame_gen: cur_gen,
                    codec,
                    video_pid: stream_ids.video_pid,
                    audio_pid: stream_ids.audio_pid,
                    program_number: stream_ids.program_number,
                };
                if vtx.try_send(out_frame).is_err() {
                    counters
                        .frames_dropped_mpsc_full
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            continue;
        }

        let out_frame = VideoFrame {
            y,
            y_stride,
            chroma,
            prime: None,
            width,
            height,
            pixel_format: out_pix_fmt,
            colorspace,
            color_transfer,
            full_range,
            pts_90k,
            frame_gen: frame_gen.load(Ordering::Relaxed),
            codec,
            video_pid: stream_ids.video_pid,
            audio_pid: stream_ids.audio_pid,
            program_number: stream_ids.program_number,
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

// Absolute-time monotonic-clock sleep used by the per-frame pacing
// branch of `display_loop`. Replaces `std::thread::sleep(Duration::from_millis)`
// — that primitive runs on SCHED_OTHER with typical ±1–2 ms wake-up
// slop, which at 60 Hz (16.7 ms vblank) can push the kernel page-flip
// a full vblank late on frames whose drift target sits near a vblank
// boundary, tipping the smoothed drift toward the late-frame hysteresis.
//
// `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)` is the same syscall
// the data-plane wire pacer uses (`engine::wire_emit::sleep_until_monotonic_ns`,
// just on CLOCK_TAI there to keep the kernel etf qdisc happy). Display
// has no ETF / PTP requirement, so CLOCK_MONOTONIC is the natural
// choice — leap-second / system-clock adjustments don't perturb it.
//
// Inlined here rather than reusing the `wire_emit` helper to keep the
// optional, feature-gated display module decoupled from the always-on
// data-plane hot path. If a third caller ever appears, lift both into
// `util/time.rs`.
fn display_monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64)
}

fn display_sleep_until_monotonic_ns(target_ns: u64) {
    let ts = libc::timespec {
        tv_sec: (target_ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (target_ns % 1_000_000_000) as i64,
    };
    // EINTR re-arms with the same absolute deadline so signals can't
    // shorten the sleep below its target.
    loop {
        let rc = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        };
        if rc != libc::EINTR {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn display_loop(
    mut kms: KmsDisplay,
    mut vrx: mpsc::Receiver<VideoFrame>,
    clock: Arc<AudioClock>,
    counters: Arc<DisplayStatsCounters>,
    cancel: CancellationToken,
    output_stats: Arc<OutputStatsAccumulator>,
    decoder_kind_label: &'static str,
    event_sender: EventSender,
    flow_id: String,
    output_id: String,
    meter: Option<SharedMeter>,
    scaling_mode: DisplayScalingMode,
    frame_gen: Arc<AtomicU64>,
    force_cpu_blit_signal: Arc<AtomicBool>,
    prime_scanout_failed: Arc<AtomicBool>,
    #[cfg(feature = "rga-transfer")] rga_transfer_active: Arc<AtomicBool>,
) {
    // Frame period derived from the observed PTS deltas — used to size
    // the late-drop threshold. 33 ms (30 fps) until we've seen enough
    // frames to estimate.
    let mut frame_period_ms: f64 = 33.0;
    let mut last_pts: Option<u64> = None;
    let mut scaler: Option<CachedScaler> = None;
    // Reusable scratch buffer for the per-frame stream-info header
    // (codec, dims, fps, HDR signal, video / audio PIDs, program).
    // Lives across iterations so we never alloc on the per-frame path
    // — `String::clear` keeps the heap allocation; the formatted line
    // is short (<64 chars) so the buffer settles to a tiny single
    // allocation after the first frame.
    let mut header_buf: String = String::with_capacity(96);
    // Track the most-recent video codec discriminant we wrote to the
    // shared `counters.video_codec_label` atomic. Used to skip the
    // store on every frame when the codec didn't change.
    let mut last_video_codec_label_disc: u8 =
        crate::stats::collector::DisplayCodecLabel::Unknown as u8;

    // Resolution autodetect state. Re-armed on PTS jump (input switch)
    // or on any mid-stream source resolution change.
    let mut matched_dims: Option<(u32, u32)> = None;
    // When the auto-match modeset fails (EBUSY race with an in-flight
    // flip, transient EDID re-probe returning no modes), re-arm the
    // match at this instant instead of latching the failed attempt as
    // "matched" forever — the pre-fix behaviour pinned the panel at the
    // old mode until the next source-dims change (2026-06-11: 4K Take
    // left the panel at 1080p for the rest of the session).
    let mut automatch_retry_at: Option<Instant> = None;
    let mut stats_registered = false;
    // Debounce timestamp for the CPU-blit-ceiling Warning event: emit
    // at most once per 30 s while the condition holds, otherwise the
    // log floods with one Critical per dropped frame on a 4K stream
    // running on a CPU-only host.
    let mut last_sw_ceiling_warn_at: Option<Instant> = None;
    // Throttle for the per-frame blit/present failure warn below. The
    // prime path logs its own failures inside `blit_and_present`; the
    // CPU-blit path's errors (scaler init, libswscale, atomic_commit,
    // page_flip) previously vanished into `.is_ok()` — a host where
    // every CPU present fails showed a frozen panel with zero log
    // evidence. One line per second keeps a hard-broken present path
    // visible without flooding.
    let mut last_blit_err_warn_at: Option<Instant> = None;
    // Wedge tracking for the dedicated `display_flip_timeout` event. Only
    // the FIRST frame after the GPU stops posting completions carries the
    // `display_flip_timeout` string; while the flip stays pending every
    // later frame fails with EBUSY instead. `flip_wedged` latches the
    // condition (cleared by the next successful present) so the event
    // emits as a throttled heartbeat for the whole wedge, not a one-shot.
    let mut flip_wedged = false;
    let mut last_flip_timeout_warn_at: Option<Instant> = None;
    // Tracks whether the panel mode currently in force was picked with
    // a stabilised source-fps hint. The first frame fires a modeset
    // with `frame_period_ms = 33` (default) — fine for picking the
    // smallest dims-covering mode but not yet able to choose between
    // 50 Hz / 60 Hz / 100 Hz panel-refresh candidates. After ~5 source
    // frames the EMA-smoothed period reflects the real fps, and we
    // re-fire the auto-match once so the panel locks to a refresh
    // that's an integer multiple of the source. Stays `true` from then
    // on until the resolution / input switch path resets it.
    let mut fps_locked: bool = false;
    let mut frames_since_period_reset: u32 = 0;
    // The `(width, height, fps)` hint from the last *fresh*
    // (EMA-stabilised) fps-locked modeset. Carried across PTS jumps /
    // input switches as a stale fallback hint: switching between
    // same-format sources then picks the same mode on the first try and
    // `match_source_resolution` no-ops — previously every switch paid a
    // hintless re-pick (often hopping a 50 Hz panel back to 60 Hz)
    // followed by a second fps-locked re-modeset 40 frames later: two
    // full HDMI link retrains (0.5–2 s of black each) between identical
    // 25/50 fps sources. Scoped to the dims it was measured at — a
    // cross-format switch (720p50 → 1080p29.97) must fall back to the
    // hintless highest-refresh pick, or the old stream's rate steers
    // the new dims onto a wrong-cadence mode and buys an extra retrain.
    let mut last_fps_hint: Option<(u32, u32, f32)> = None;

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

    // One-time A/V latency calibration for the RGA-accelerated
    // DRM_PRIME→sysmem transfer path (`rga-transfer` feature). That
    // path was measured on bilby-pir6s (2026-07-19) to carry a large,
    // *fixed* extra pipeline latency (roughly 340-400 ms, stable per session,
    // not drifting) that the raw audio-vs-video drift below has no
    // way to tell apart from a real backlog — left uncorrected, the
    // catch-up drain (further below) fires continuously and the
    // measured drop rate lands *worse* than the plain CPU transfer
    // path RGA was meant to improve on (~31% vs ~11%), despite RGA's
    // own per-frame cost genuinely being lower. Sample the raw drift
    // for the first `RGA_CALIBRATION_MS` after engagement, lock in the
    // median as a standing correction, and never touch it again — a
    // fixed pipeline property, not something that drifts over a
    // session. Same technique already proven correct for the (KMS-
    // side, since-disabled) Esmart-plane zero-copy attempt earlier
    // this session; this is a pacing-only correction with none of
    // that attempt's plane-compositing risk.
    #[cfg(feature = "rga-transfer")]
    const RGA_CALIBRATION_MS: u128 = 1500;
    #[cfg(feature = "rga-transfer")]
    const RGA_CALIBRATION_MIN_SAMPLES: usize = 20;
    #[cfg(feature = "rga-transfer")]
    let mut rga_calibration_samples: Vec<i64> = Vec::new();
    #[cfg(feature = "rga-transfer")]
    let mut rga_calibration_started_at: Option<Instant> = None;
    #[cfg(feature = "rga-transfer")]
    let mut rga_latency_comp_ms: i64 = 0;
    #[cfg(feature = "rga-transfer")]
    let mut rga_calibration_done = false;

    // One-frame stash for the catch-up drain below: when the drain
    // pulls a frame that crossed a generation bump or a resolution
    // change, that frame must run the full per-frame checks at the top
    // of the loop (stale-gen gate, SW-blit ceiling, mode re-arm), so it
    // parks here and becomes `next` on the following iteration.
    let mut pending: Option<VideoFrame> = None;

    // Video is paced directly to the measured audio-playout position
    // (`AudioClock`, now driven by `snd_pcm_delay()` in `audio.rs`). We
    // present each frame when the audio the operator is *hearing* reaches
    // that frame's PTS — i.e. we drive the raw V−A offset toward zero,
    // rather than absorbing whatever baseline the stream happened to anchor
    // with. That is the whole fix for the historical "+700 ms, video leads
    // sound" offset: the old EMA folded the mux delivery interleave (~1 s of
    // audio buffered ahead of the matching video) into "drift ≈ 0" and
    // presented video immediately, so the picture ran a second ahead of the
    // sound. With a measured clock the per-frame offset is smooth, so no EMA
    // baseline, no consecutive-late/early snap, and no catch-up are needed —
    // the loop self-aligns over the first ~second and then paces at source
    // rate. `av_sync_offset_ms` reports this raw V−A offset and should now
    // sit near zero.

    // The audio-bars overlay plane is pre-allocated up-front in
    // `run_display_output` (before the demux task spawns) so the demux
    // loop can plumb the result into its VAAPI-download decision.
    // `kms.bars_overlay_dims()` returning Some on the hot path is the
    // signal that the overlay plane is live and the rasterise should
    // paint into it; returning None means run_display_output found no
    // suitable plane and the demux loop is downloading VAAPI surfaces
    // so the CPU-blit path below bakes bars into the primary dumb
    // buffer instead.

    while !cancel.is_cancelled() {
        let mut next = match pending.take() {
            Some(f) => f,
            None => match vrx.blocking_recv() {
                Some(f) => f,
                None => break,
            },
        };

        // Drop frames decoded before the most recent switch / Lagged /
        // pts_jump. Without this gate, the up-to-`MPSC_VIDEO_DEPTH`
        // pre-switch frames already in the queue would each pay full
        // blit + vsync time before the new stream gets a slot — on a
        // 4K-display + slow-blit host that bleeds the previous stream's
        // last second of decoded video onto the panel after every input
        // switch (the user-visible "frozen on the old picture" symptom).
        // Drops here are O(µs) per frame — just `continue`, no scaler /
        // present / page-flip work — so the queue clears almost
        // instantly and the new stream's first frame renders on the
        // next iteration.
        if next.frame_gen < frame_gen.load(Ordering::Relaxed) {
            counters
                .frames_dropped_stale_gen
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let cur_video_label = match next.codec {
            VideoCodec::H264 => crate::stats::collector::DisplayCodecLabel::H264,
            VideoCodec::Hevc => crate::stats::collector::DisplayCodecLabel::Hevc,
            VideoCodec::Mpeg2 => crate::stats::collector::DisplayCodecLabel::Mpeg2Video,
        };
        if cur_video_label as u8 != last_video_codec_label_disc {
            counters.set_video_codec_label(cur_video_label);
            last_video_codec_label_disc = cur_video_label as u8;
        }

        // CPU-blit ceiling: refuse > 1080p sources unless the zero-copy
        // VAAPI path is engaged for this frame. Every CPU-decoded frame
        // walks libswscale to convert YUV → BGRA into a write-combining
        // KMS dumb buffer — at 3840×2160 that's a ~33 MB write per
        // frame and the observed blit time is ≈ 7 s, producing 0.14 fps.
        // A black panel with a Critical event in the manager UI is
        // honest; silent 0.14 fps playout is not. The ceiling lifts
        // automatically when `next.prime.is_some()` — the zero-copy
        // path skips libswscale entirely, so 4K HEVC scans out at the
        // panel's native cadence.
        const SW_CEILING_WARN_INTERVAL: std::time::Duration =
            std::time::Duration::from_secs(30);
        let zero_copy = next.prime.is_some();
        // CPU-blit ceiling: a 4K CPU-decoded frame would walk libswscale
        // through a ~33 MB BGRA convert + write-combining KMS dumb-buffer
        // copy — measured at ~7 s per frame on the testbed's iGPU, i.e.
        // 0.14 fps with a fully-saturated CPU. Refuse to pay that cost.
        // The condition fires when a non-zero-copy frame arrives with
        // dims past the ceiling — this is exactly the post-VAAPI-demote
        // case (decoder fell back to libavcodec CPU on `EAGAIN` /
        // `INVALIDDATA` bursts and now produces sysmem frames).
        //
        // **Drop the frame, don't kill the thread.** The previous
        // implementation `break`-ed out of `display_loop` here, exiting
        // the OS thread permanently — when VAAPI later re-engaged
        // (which the existing runtime promotion machinery does
        // automatically on a clean `send_packet`), there was no
        // consumer left for the video mpsc and the display silently
        // froze with `frames_displayed` stuck at the demote moment
        // and `frames_dropped_mpsc_full` climbing at decoder rate.
        // Continuing the loop keeps the thread alive so the *next*
        // zero-copy frame after re-promote presents normally.
        if !zero_copy
            && (next.width > SW_BLIT_MAX_W || next.height > SW_BLIT_MAX_H)
        {
            let now = Instant::now();
            let due = last_sw_ceiling_warn_at
                .map(|t| now.duration_since(t) >= SW_CEILING_WARN_INTERVAL)
                .unwrap_or(true);
            if due {
                last_sw_ceiling_warn_at = Some(now);
                event_sender.emit_flow_with_details(
                    EventSeverity::Warning,
                    "display",
                    format!(
                        "display output '{output_id}': dropping {}x{} CPU-decoded \
                         frames (over {SW_BLIT_MAX_W}x{SW_BLIT_MAX_H} CPU-blit ceiling); \
                         display will resume automatically when video-decoder-vaapi \
                         zero-copy frames arrive again",
                        next.width, next.height,
                    ),
                    &flow_id,
                    serde_json::json!({
                        "error_code": "display_resolution_unsupported_for_sw_blit",
                        "output_id": output_id,
                        "source_width": next.width,
                        "source_height": next.height,
                        "max_supported_width": SW_BLIT_MAX_W,
                        "max_supported_height": SW_BLIT_MAX_H,
                    }),
                );
            }
            counters.frames_dropped_unsupported_pixfmt.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Re-arm autodetect if the source resolution shifted (operator
        // switched from 1080p to 720p, etc).
        if let Some((mw, mh)) = matched_dims {
            if mw != next.width || mh != next.height {
                tracing::info!(
                    output_id = %output_id,
                    "display source resolution changed {mw}x{mh} → {}x{} — re-arming panel mode match",
                    next.width,
                    next.height,
                );
                matched_dims = None;
                automatch_retry_at = None;
                wall_anchor = None;
                fps_locked = false;
                frames_since_period_reset = 0;
            }
        }
        // A previous auto-match attempt failed — retry once the backoff
        // elapses (the failure was transient more often than not:
        // SETCRTC-vs-flip EBUSY, EDID probe flake).
        if matched_dims.is_some()
            && automatch_retry_at.is_some_and(|t| Instant::now() >= t)
        {
            matched_dims = None;
            automatch_retry_at = None;
        }
        // Re-fire the auto-match exactly once after the source fps has
        // stabilised. Without this, a 25 fps source on a panel that
        // offers both 50 Hz and 60 Hz at the source resolution stays
        // on the 60 Hz mode KMS picked at first-frame time (when our
        // `frame_period_ms` was still the 33 ms default), and the
        // operator sees 2:3 pulldown judder for the rest of the run.
        if matched_dims.is_some() && !fps_locked && frames_since_period_reset >= 40 {
            matched_dims = None;
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
            // Convert the demuxer's running frame-period estimate into
            // an fps hint. Until we've seen ≥ 40 frames `frame_period_ms`
            // is still settling from the 33 ms (30 fps) default; once
            // the EMA stabilises, the real source fps drives a clean
            // panel-refresh choice (50 Hz for 25/50 fps, 60 Hz for
            // 30/60 fps). While the EMA is still warming, fall back to
            // the hint from the last fps-locked modeset (`last_fps_hint`)
            // — an input switch between same-format sources then picks
            // the same mode immediately and skips the modeset entirely.
            // Only a genuinely fresh hint sets `fps_locked`, so the
            // 40-frame re-fire still verifies (and, when the stale hint
            // was right, no-ops inside `match_source_resolution`).
            let fresh_fps_hint = frames_since_period_reset >= 40;
            let src_fps_hint = if fresh_fps_hint {
                Some(1000.0 / frame_period_ms as f32)
            } else {
                last_fps_hint
                    .filter(|(w, h, _)| *w == next.width && *h == next.height)
                    .map(|(_, _, fps)| fps)
            };
            let (modeset, ok_code, err_code, ok_verb, err_verb) = match scaling_mode {
                DisplayScalingMode::MatchSource => (
                    kms.match_source_resolution(next.width, next.height, src_fps_hint),
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
                Ok(()) => {
                    // Mirror onto tracing — manager events are
                    // best-effort (dropped while the WS is down), and a
                    // mode decision is exactly what an operator greps
                    // the local log for when the panel looks wrong.
                    tracing::info!(
                        output_id = %output_id,
                        "display {} for source {}x{} → {}x{}@{}Hz",
                        ok_verb,
                        next.width,
                        next.height,
                        kms.width(),
                        kms.height(),
                        kms.refresh_hz(),
                    );
                    emit_event(
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
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        output_id = %output_id,
                        "display {err_verb} (source {}x{}, panel stays {}x{}@{}Hz, retrying in 2 s): {e:#}",
                        next.width,
                        next.height,
                        kms.width(),
                        kms.height(),
                        kms.refresh_hz(),
                    );
                    emit_event(
                        &event_sender,
                        EventSeverity::Warning,
                        err_code,
                        &flow_id,
                        &output_id,
                        &format!("display {err_verb}: {e}"),
                    );
                    automatch_retry_at =
                        Some(Instant::now() + std::time::Duration::from_secs(2));
                }
            }
            // Register / refresh the stats handle with the post-modeset
            // resolution so the manager UI shows the active mode.
            output_stats.set_display_stats(
                Arc::clone(&counters),
                format!("{}x{}", kms.width(), kms.height()),
                kms.refresh_hz(),
                "XRGB8888",
                decoder_kind_label,
            );
            stats_registered = true;
            matched_dims = Some((next.width, next.height));
            // Only a *fresh* (EMA-stabilised) hint locks the fps and is
            // remembered for the next switch — a stale carried hint
            // leaves `fps_locked` false so the 40-frame re-fire still
            // verifies the new stream's real rate.
            if fresh_fps_hint {
                fps_locked = true;
                last_fps_hint =
                    src_fps_hint.map(|fps| (next.width, next.height, fps));
            }
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
                frame_period_ms = 33.0;
                matched_dims = None;
                wall_anchor = None;
                fps_locked = false;
                frames_since_period_reset = 0;
            } else if (10..=200).contains(&dms) {
                // Feed the EMA with *fractional* milliseconds: the
                // integer-truncated delta biases NTSC-family rates
                // (59.94 fps = 1501.5 ticks → 16 ms → a 62.5 fps hint)
                // far past the mode-picker's ±0.5 Hz cadence tolerance,
                // steering e.g. a 59.94 fps source onto a 75 Hz mode.
                let dms_f = forward as f64 / 90.0;
                frame_period_ms = frame_period_ms * 0.875 + dms_f * 0.125;
                frames_since_period_reset = frames_since_period_reset.saturating_add(1);
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
        // boundary.
        //
        // On the *late* side, a frame is skipped only when a fresher
        // frame is already queued behind it (the catch-up drain below)
        // — a late frame with nothing behind it is still the freshest
        // content we have, and presenting it beats holding the panel on
        // the previous (even staler) frame. The old policy dropped late
        // frames unconditionally behind a 4×-period/160 ms floor with
        // no queue look-ahead: because the present path is serial
        // (sleep → blit → blocking page-flip), any source whose
        // delivery rate persistently exceeded the sustainable present
        // rate (60.00 fps on a 59.94 Hz panel mode, CPU blit + tonemap
        // over one vblank) latched video 143–160 ms behind audio
        // *forever* — each drop recovered one period while the backlog
        // kept refilling.
        //
        // `catchup_cap_ms` caps the forward wait below so a large
        // startup mux interleave (audio buffered ~1 s ahead of the
        // matching video) converges over ~a second of gentle slow-in
        // rather than a single long freeze. 4× source period, floored
        // at 160 ms.
        let catchup_cap_ms: i64 = ((frame_period_ms as i64) * 4).max(160);
        // Arms the catch-up drain: a frame further behind the reference
        // than this, with a fresher frame already queued, is skipped in
        // O(µs) instead of paying a present slot. 2× source period,
        // floored at 50 ms — tight enough to bound the worst-case V−A
        // error at ~2 periods under sustained overload, loose enough
        // that decoder burst jitter (B-frame reorder) never trips it.
        let late_skip_ms: i64 = ((frame_period_ms as i64) * 2).max(50);
        // Hand the buffer to KMS a hair before its target vblank — the
        // page-flip blocks for up to one vblank inside `kms.present()`, so
        // over-sleeping by even a millisecond pushes the scan-out a full
        // frame late at 60 Hz.
        const PRESENT_MARGIN_MS: i64 = 2;

        // Raw video−audio offset in ms. With audio configured the reference
        // is the *measured* audio-playout position (`AudioClock`, driven by
        // `snd_pcm_delay()`); muted outputs fall back to a wall-clock seeded
        // by the first frame's PTS.
        let raw_drift_ms_opt: Option<i64> = if let Some(audio_pts) =
            clock.current_pts_90k_smoothed()
        {
            wall_anchor = None;
            let raw = (next.pts_90k as i64 - audio_pts as i64) / 90;
            #[cfg(feature = "rga-transfer")]
            {
                if rga_transfer_active.load(Ordering::Relaxed) {
                    if rga_calibration_done {
                        Some(raw + rga_latency_comp_ms)
                    } else {
                        let started = *rga_calibration_started_at.get_or_insert_with(Instant::now);
                        rga_calibration_samples.push(raw);
                        if started.elapsed().as_millis() >= RGA_CALIBRATION_MS
                            && rga_calibration_samples.len() >= RGA_CALIBRATION_MIN_SAMPLES
                        {
                            rga_calibration_samples.sort_unstable();
                            let median = rga_calibration_samples[rga_calibration_samples.len() / 2];
                            rga_latency_comp_ms = -median;
                            rga_calibration_done = true;
                            tracing::info!(
                                output_id = %output_id,
                                measured_offset_ms = median,
                                comp_ms = rga_latency_comp_ms,
                                "display: calibrated fixed A/V latency on the RGA transfer \
                                 path — applying standing correction"
                            );
                        }
                        // Still calibrating — let this frame's raw
                        // drift through uncorrected rather than guess.
                        Some(raw)
                    }
                } else {
                    Some(raw)
                }
            }
            #[cfg(not(feature = "rga-transfer"))]
            {
                Some(raw)
            }
        } else {
            // Audio muted — pace on wall-clock seeded by the first frame.
            let now = Instant::now();
            let (anchor_pts, anchor_at) =
                wall_anchor.get_or_insert_with(|| (next.pts_90k, now));
            let pts_delta_ms = (next.pts_90k.wrapping_sub(*anchor_pts) as i64) / 90;
            let wall_delta_ms = now.duration_since(*anchor_at).as_millis() as i64;
            let drift_ms = pts_delta_ms - wall_delta_ms;
            // Anchor servo. The source's PTS clock and the host wall
            // clock are different crystals (MPEG allows ±30 ppm; cheap
            // encoders exceed it), so against a *fixed* anchor the
            // offset accumulates without bound — at 30 ppm every frame
            // fell past the old late-drop band after ~90 min and the
            // panel silently froze for hours until the drift crossed
            // the −2 s rebase limit. Outside a one-period deadband,
            // slew the anchor toward zero drift at ≤ 250 µs per frame:
            // ~60× the worst-case accumulation rate (100 ppm × 40 ms
            // = 4 µs/frame), yet far below anything visible. A muted
            // output has no audio to stay aligned with — a standing
            // sub-period offset is meaningless; boundedness is what
            // matters.
            let deadband_ms = frame_period_ms as i64;
            if drift_ms.unsigned_abs() as i64 > deadband_ms {
                let excess_ms = (drift_ms.unsigned_abs() as i64 - deadband_ms) as u64;
                let step = std::time::Duration::from_micros(250)
                    .min(std::time::Duration::from_millis(excess_ms));
                if drift_ms > 0 {
                    // Frame runs ahead of the wall timeline — pull the
                    // anchor back so future targets land earlier.
                    if let Some(adj) = anchor_at.checked_sub(step) {
                        *anchor_at = adj;
                    }
                } else {
                    *anchor_at += step;
                }
            }
            Some(drift_ms)
        };

        if let Some(mut raw_drift_ms) = raw_drift_ms_opt {
            // Catch-up drain: this frame is already behind the reference
            // clock AND fresher frames are queued behind it — fast-forward
            // to the freshest one instead of paying one present slot per
            // stale frame. EXCEPT when the frame is so far behind that
            // this is a clock re-base, not a late frame: on an input
            // switch the new stream's PTS epoch can sit seconds below the
            // (still-old-stream) audio playout for the brief window before
            // the audio task re-anchors. Draining or dropping there would
            // blank the whole new stream's video; instead present it and
            // let the measured clock re-converge over the next few frames.
            const REBASE_LIMIT_MS: i64 = 2_000;
            let mut drift_is_stale = false;
            while raw_drift_ms < -late_skip_ms && raw_drift_ms >= -REBASE_LIMIT_MS {
                let Ok(newer) = vrx.try_recv() else {
                    // Nothing fresher queued — the late frame is still the
                    // newest content we have; present it rather than hold
                    // the panel on the previous (even staler) frame.
                    break;
                };
                // Park-and-break cases: frames the drain must NOT
                // fast-forward through, because the full per-frame checks
                // at the top of the loop own them.
                //  * generation bump / dims change — stale-gen gate +
                //    mode re-arm;
                //  * a sysmem frame over the SW-blit ceiling (a 4K
                //    prime→CPU demotion mid-backlog) — presenting it here
                //    would wedge the thread in a multi-second libswscale
                //    blit the top-of-loop gate exists to refuse;
                //  * a two-sided PTS discontinuity > 1 s — the loop-top
                //    stream-change detector must see it to reset
                //    wall_anchor / frame-period EMA / mode match;
                //    swallowing it here left a muted output pacing
                //    against a stale anchor epoch for minutes.
                let jump_fwd = newer.pts_90k.wrapping_sub(next.pts_90k);
                let jump_back = next.pts_90k.wrapping_sub(newer.pts_90k);
                if newer.frame_gen != next.frame_gen
                    || newer.width != next.width
                    || newer.height != next.height
                    || (newer.prime.is_none()
                        && (newer.width > SW_BLIT_MAX_W
                            || newer.height > SW_BLIT_MAX_H))
                    || (jump_fwd > 90_000 && jump_back > 90_000)
                {
                    pending = Some(newer);
                    break;
                }
                // Keep the frame-period EMA fed with real frame-to-frame
                // deltas across the skip so the thresholds stay honest.
                let dms = (newer.pts_90k.wrapping_sub(next.pts_90k) as i64) / 90;
                if (10..=200).contains(&dms) {
                    let dms_f = newer.pts_90k.wrapping_sub(next.pts_90k) as f64 / 90.0;
                    frame_period_ms = frame_period_ms * 0.875 + dms_f * 0.125;
                    frames_since_period_reset =
                        frames_since_period_reset.saturating_add(1);
                }
                last_pts = Some(newer.pts_90k);
                counters.frames_dropped_late.fetch_add(1, Ordering::Relaxed);
                next = newer;
                raw_drift_ms = if let Some(audio_pts) =
                    clock.current_pts_90k_smoothed()
                {
                    let raw = (next.pts_90k as i64 - audio_pts as i64) / 90;
                    #[cfg(feature = "rga-transfer")]
                    let raw = if rga_transfer_active.load(Ordering::Relaxed) && rga_calibration_done {
                        raw + rga_latency_comp_ms
                    } else {
                        raw
                    };
                    raw
                } else if let Some((anchor_pts, anchor_at)) = wall_anchor.as_ref() {
                    ((next.pts_90k.wrapping_sub(*anchor_pts) as i64) / 90)
                        - anchor_at.elapsed().as_millis() as i64
                } else {
                    // Pacing reference vanished mid-drain (audio task
                    // stalled past the staleness cutoff) — present what
                    // we have and let the next iteration re-anchor.
                    // `raw_drift_ms` still describes the *skipped*
                    // frame, so don't report it as this frame's offset.
                    drift_is_stale = true;
                    break;
                };
            }
            // Surface the raw V−A offset to the operator. We drive this
            // toward zero by present-to-playout (below), so a sustained
            // non-zero value is a real lip-sync problem — not an absorbed
            // baseline as in the previous EMA design.
            if !drift_is_stale {
                counters.store_av_offset_ms(
                    raw_drift_ms.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                );
            }
            // Positive → this frame is ahead of the audio playout; wait
            // until the sound reaches it so the picture is shown exactly
            // when its audio plays. Capped at `catchup_cap_ms` so a large
            // startup mux interleave (audio buffered ~1 s ahead of the
            // matching video) converges over ~a second of gentle slow-in
            // rather than a single long freeze.
            if raw_drift_ms > PRESENT_MARGIN_MS {
                let sleep_ms =
                    (raw_drift_ms - PRESENT_MARGIN_MS).min(catchup_cap_ms) as u64;
                // Absolute CLOCK_MONOTONIC sleep — eliminates the ±1–2 ms
                // slop of `std::thread::sleep` on SCHED_OTHER, which at
                // 60 Hz can push the next page-flip a full vblank late on
                // frames whose target sits near a vblank boundary.
                let target_ns = display_monotonic_now_ns()
                    .saturating_add(sleep_ms.saturating_mul(1_000_000));
                display_sleep_until_monotonic_ns(target_ns);
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
        compose_stream_header(&mut header_buf, &next, frame_period_ms);
        let blit_start = Instant::now();
        let blit_ok = match blit_and_present(
            &mut kms,
            &next,
            &mut scaler,
            meter.as_ref(),
            &header_buf,
        ) {
            Ok(()) => {
                // A successful flip clears any wedge latch so a later
                // recurrence re-fires the heartbeat immediately (rather than
                // waiting out a stale 30 s throttle).
                if flip_wedged {
                    flip_wedged = false;
                    last_flip_timeout_warn_at = None;
                }
                true
            }
            Err(e) => {
                let msg = format!("{e:#}");
                // A zero-copy (PRIME) frame whose framebuffer import the
                // kernel rejected (addfb EINVAL on a tiling modifier the
                // scanout plane won't accept — classic on Intel Gen9 NV12
                // Yf_TILED, or a malformed descriptor) can never scan out on
                // this host. Demote ONCE: signal the decode task to download
                // VAAPI surfaces to sysmem from here on, so the next frame
                // arrives as a CPU frame and takes the libswscale blit path.
                // Transient page-flip failures (EBUSY) are NOT a demotion
                // trigger — they self-heal on the next frame.
                let import_rejected =
                    next.prime.is_some() && prime_scanout_permanently_rejected(&msg);
                if import_rejected
                    && !prime_scanout_failed.swap(true, Ordering::Relaxed)
                {
                    emit_event(
                        &event_sender,
                        EventSeverity::Warning,
                        "display_prime_fallback_engaged",
                        &flow_id,
                        &output_id,
                        &format!(
                            "display zero-copy (PRIME) scanout rejected by the kernel — \
                             demoting to CPU-blit (sysmem download) for the rest of this run: {msg}"
                        ),
                    );
                }
                // A bounded page-flip wait that elapsed with no completion
                // event means the GPU/driver has stopped posting vblank
                // completions (wedged scanout — e.g. broken nvidia-drm flip
                // IRQs). Only the FIRST such frame carries the
                // `display_flip_timeout` string; while that flip stays pending
                // every later frame fails with EBUSY ("page_flip queue" /
                // "atomic_commit busy") instead — so latch the wedge in
                // `flip_wedged` (cleared by the next successful present above)
                // and emit a throttled (~one / 30 s) `display_flip_timeout`
                // heartbeat for its whole duration, not a single blip. The
                // loop keeps trying and stays cancel-responsive; it recovers
                // automatically once the driver resumes firing completions.
                let now = Instant::now();
                if msg.contains("display_flip_timeout") {
                    flip_wedged = true;
                }
                if flip_wedged {
                    let timeout_due = last_flip_timeout_warn_at
                        .map(|t| now.duration_since(t) >= std::time::Duration::from_secs(30))
                        .unwrap_or(true);
                    if timeout_due {
                        last_flip_timeout_warn_at = Some(now);
                        emit_event(
                            &event_sender,
                            EventSeverity::Warning,
                            "display_flip_timeout",
                            &flow_id,
                            &output_id,
                            "display page flips are not completing — the GPU/driver stopped \
                             posting vblank completions (a black or frozen panel). On NVIDIA, \
                             verify `nvidia-drm.modeset=1` is set and the driver version is \
                             current; see docs/installation.md (Local-display output)",
                        );
                    }
                }
                let due = last_blit_err_warn_at
                    .map(|t| now.duration_since(t) >= std::time::Duration::from_secs(1))
                    .unwrap_or(true);
                if due {
                    last_blit_err_warn_at = Some(now);
                    tracing::warn!(
                        output_id = %output_id,
                        zero_copy = next.prime.is_some(),
                        bars_overlay = kms.bars_overlay_dims().is_some(),
                        "display blit/present failed: {msg}",
                    );
                }
                false
            }
        };
        if meter.is_some() && kms.bars_overlay_dims().is_none()
            && !force_cpu_blit_signal.load(Ordering::Relaxed)
        {
            force_cpu_blit_signal.store(true, Ordering::Relaxed);
            counters
                .bars_overlay_enabled
                .store(false, Ordering::Relaxed);
        }
        // Self-heal: while degraded (overlay torn down mid-session →
        // CPU-bake fallback engaged above), periodically re-attempt the
        // overlay enable. KMS gates the retry on a cooldown so this
        // per-frame call is a cheap flag check almost always. A mode
        // change heals through `resync_bars_overlay_to_panel` instead;
        // this arm catches it either way because it keys on the live
        // overlay state, not on who restored it. Without the heal, the
        // CPU bake kept bars on ≤1080p sources but 4K zero-copy sources
        // (over the SW-blit ceiling) showed no bars for the rest of the
        // session.
        if meter.is_some() && force_cpu_blit_signal.load(Ordering::Relaxed)
            && kms.maybe_reheal_bars_overlay(false)
        {
            force_cpu_blit_signal.store(false, Ordering::Relaxed);
            counters
                .bars_overlay_enabled
                .store(true, Ordering::Relaxed);
            tracing::info!(
                output_id = %output_id,
                "audio-bars overlay restored — leaving CPU-blit fallback, hardware composition re-engaged"
            );
        }
        let blit_us = blit_start.elapsed().as_micros() as u64;
        counters.blit_count.fetch_add(1, Ordering::Relaxed);
        counters.blit_us_total.fetch_add(blit_us, Ordering::Relaxed);
        counters
            .blit_us_max
            .fetch_max(blit_us, Ordering::Relaxed);
        if blit_ok {
            counters.frames_displayed.fetch_add(1, Ordering::Relaxed);
        }

        // One-shot Warning if the atomic-commit page-flip path was
        // refused by the kernel and we permanently fell back to the
        // legacy `set_crtc` per-frame plane reconfigure. Operators see
        // this once per `KmsDisplay` lifetime so they can tell apart
        // "10 fps because atomic was refused" from "10 fps because the
        // host can't keep up with the source".
        if let Some(reason) = kms.take_atomic_fallback_reason() {
            emit_event(
                &event_sender,
                EventSeverity::Warning,
                "display_atomic_unavailable",
                &flow_id,
                &output_id,
                &format!(
                    "display atomic commit unavailable — falling back to per-frame set_crtc: {reason}"
                ),
            );
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
    header_text: &str,
) -> Result<()> {
    let stream_header = StreamHeader { text: header_text };
    // ── VAAPI zero-copy fast path ──────────────────────────────────
    //
    // Decoded straight into a VAAPI surface; the descriptor + Arc
    // keepalive ride the mpsc instead of system-memory plane copies.
    // We hand both to `KmsDisplay::present_prime`, which imports the
    // DMA-BUF as a KMS framebuffer and atomic-page-flips onto it. No
    // libswscale work, no dumb-buffer write — the prime FB scans out
    // straight from the VAAPI surface.
    //
    // Audio-bars overlay rasterise: when the operator has
    // `show_audio_bars: true` AND `KmsDisplay::enable_bars_overlay`
    // succeeded at startup, we paint the meter snapshot into the
    // dedicated ARGB8888 overlay-plane buffer so the next atomic
    // commit composes both planes (primary = video, overlay = bars)
    // at vblank. This keeps the zero-copy path on for `show_audio_bars`
    // workflows. Hosts where the overlay couldn't be enabled (no
    // suitable plane / driver lacks atomic) fall through with no bars
    // — the operator opted into VAAPI-on-no-overlay-host explicitly
    // (forced `hw_decode: vaapi`), and the legacy `set_crtc` path
    // can't compose multi-plane.
    if let Some(prime) = frame.prime.as_ref() {
        if let (Some(snapshot), Some((dst_w, dst_h))) = (meter, kms.bars_overlay_dims()) {
            if let Some(mut map) = kms.bars_overlay_buffer() {
                let snap = snapshot.load();
                let pitch = map.pitch() as usize;
                crate::display::audio_bars::rasterise_overlay(
                    &snap,
                    map.as_mut(),
                    pitch,
                    dst_w,
                    dst_h,
                );
                crate::display::audio_bars::rasterise_header_overlay(
                    &snap,
                    &stream_header,
                    map.as_mut(),
                    pitch,
                    dst_w,
                    dst_h,
                );
            }
        }
        // HDR signalling on the prime path. When the source carries
        // PQ / HLG transfer AND the panel can do HDR (the decode
        // task only routes HDR sources here when both are true), set
        // the connector's `HDR_OUTPUT_METADATA` so the next atomic
        // commit transmits the Dynamic Range and Mastering InfoFrame
        // alongside the FB flip — single vblank, both metadata and
        // pixels move together. SDR sources (or HDR sources whose
        // panel can't signal HDR but whose download to sysmem
        // failed) clear the metadata so the connector reverts to
        // SDR signalling. Both calls are idempotent when the EOTF
        // hasn't changed.
        let eotf = match frame.color_transfer {
            16 => Some(crate::display::kms::HdrEotf::Pq),
            18 => Some(crate::display::kms::HdrEotf::Hlg),
            _ => None,
        };
        match eotf {
            Some(e) if kms.panel_hdr_capable() => {
                if let Err(err) = kms.set_hdr_output_metadata(e, None, None, None, None) {
                    tracing::warn!(
                        "HDR_OUTPUT_METADATA programming failed: {err:#}"
                    );
                }
            }
            _ => kms.clear_hdr_output_metadata(),
        }
        if let Err(e) = kms.present_prime(&prime.descriptor, prime.keepalive.clone()) {
            // Log the underlying KMS / DRM error so a runtime probe-vs-
            // present mismatch is debuggable. Without this, the display
            // task silently drops every frame and `frames_displayed`
            // sits at zero — the symptom we hit on Intel iHD when
            // `add_planar_framebuffer` rejected the modifier.
            tracing::warn!(
                width = prime.descriptor.width,
                height = prime.descriptor.height,
                fourcc = format!("0x{:08x}", prime.descriptor.fourcc),
                modifier = format!("0x{:016x}", prime.descriptor.modifier),
                planes = prime.descriptor.planes.len(),
                "display zero-copy present failed: {e:#}"
            );
            return Err(anyhow::anyhow!("display zero-copy present failed: {e}"));
        }
        return Ok(());
    }

    // CPU-blit path. Drop any retained PRIME state from a prior
    // zero-copy run — if the previous flow was VAAPI and the source
    // demoted to a SW-decoded codec mid-stream, the kernel still
    // holds an FB pointing at a VAAPI surface in the scanout slot.
    // `release_prime_state` is a no-op when no state is held, so the
    // steady-state CPU path pays nothing. Also drop any HDR
    // signalling — the CPU-blit + tonemap path produces SDR BGRA, so
    // the panel must come out of HDR mode before the next flip.
    kms.release_prime_state();
    kms.clear_hdr_output_metadata();

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
        VideoFrameChroma::None => {
            // Should only land here if a VAAPI frame escapes the
            // zero-copy fast path AND has empty plane data — drop it
            // rather than silently mis-rendering.
            return Ok(());
        }
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
        // Bake bars into the dumb buffer as a defensive fallback —
        // when the overlay plane is unavailable (no atomic, no
        // suitable plane, driver detached the overlay), this is the
        // only path the bars survive on for a CPU-blit frame. When
        // the overlay plane IS available, the overlay buffer below
        // composites on top (zpos > primary) so the visible result is
        // unchanged; the dumb-buffer bars are then a hidden backstop.
        crate::display::audio_bars::rasterise(&snap, dst, pitch, dst_w, dst_h);
        crate::display::audio_bars::rasterise_header(
            &snap,
            &stream_header,
            dst,
            pitch,
            dst_w,
            dst_h,
        );
    }

    drop(map);

    // Refresh the overlay-plane bars buffer so the next atomic commit
    // (via `present_cpu_atomic` below) re-arms the plane with fresh
    // content. Without this, a CPU-blit frame followed by legacy
    // `page_flip` can leave the overlay plane detached on some
    // drivers — bars + header vanish until the next VAAPI prime
    // frame re-programs the plane via `present_prime`. The
    // double-rasterise above (bake-into-dumb + paint-overlay) means
    // the bars stay visible whichever plane the kernel ends up
    // scanning out.
    if let Some(snapshot) = meter {
        if let (Some((dst_w_ov, dst_h_ov)), Some(mut ov_map)) =
            (kms.bars_overlay_dims(), kms.bars_overlay_buffer())
        {
            let snap = snapshot.load();
            let ov_pitch = ov_map.pitch() as usize;
            crate::display::audio_bars::rasterise_overlay(
                &snap,
                ov_map.as_mut(),
                ov_pitch,
                dst_w_ov,
                dst_h_ov,
            );
            crate::display::audio_bars::rasterise_header_overlay(
                &snap,
                &stream_header,
                ov_map.as_mut(),
                ov_pitch,
                dst_w_ov,
                dst_h_ov,
            );
        }
    }

    // Atomic commit when available — keeps the bars overlay plane
    // programmed every frame. Falls back to legacy `page_flip`
    // internally on hosts where atomic isn't usable.
    kms.present_cpu_atomic()?;
    Ok(())
}

/// Compose the per-frame stream-info header (codec + dims + fps + HDR
/// signal + video / audio PIDs + program) into `buf`. Reuses the
/// caller's `String` allocation (`String::clear` keeps the heap; the
/// formatted line settles to a single small allocation after the
/// first frame). Single line — the strip's existing top label row is
/// the only place left of the audio meter blocks where it can sit
/// without trampling either the bars or the per-block labels.
///
/// The format is tuned for broadcast-pro confidence monitoring:
/// `"HEVC 1920x1080 50p HDR-PQ V:0x100 A:0x101 PROG 1"`. Fields drop
/// out cleanly when the demuxer hasn't locked them yet (PID = `None`,
/// fps still on the 30 fps default before PTS deltas stabilise) so a
/// freshly-armed flow shows what it knows without `0x0` / `0p`
/// placeholders.
fn compose_stream_header(buf: &mut String, frame: &VideoFrame, frame_period_ms: f64) {
    use std::fmt::Write;
    buf.clear();
    let codec_name = match frame.codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::Hevc => "HEVC",
        VideoCodec::Mpeg2 => "MPEG-2",
    };
    let _ = write!(buf, "{} {}x{}", codec_name, frame.width, frame.height);
    // fps from the running frame-period EMA. The 33 ms initial value
    // is the same default the mode picker uses; suppress it so we
    // don't show "30p" before the source frame rate has stabilised.
    if frame_period_ms > 0.0 && (frame_period_ms - 33.0).abs() > 1.0 {
        let fps = (1000.0_f32 / frame_period_ms as f32).round() as u32;
        let _ = write!(buf, " {fps}p");
    }
    // HDR transfer (PQ = SMPTE 2084 = 16, HLG = ARIB STD-B67 = 18).
    // Skip BT.709 (1) / unspecified (0/2) — those are SDR and the
    // header would just be repeating the implicit default.
    match frame.color_transfer {
        16 => { let _ = write!(buf, " HDR-PQ"); }
        18 => { let _ = write!(buf, " HDR-HLG"); }
        _ => {}
    }
    // Wide gamut signal (BT.2020 NCL = 9, BT.2020 CL = 10). Most
    // broadcast contribution still rides BT.709 — surfacing only the
    // exceptions keeps the header short.
    if matches!(frame.colorspace, 9 | 10) {
        let _ = write!(buf, " BT.2020");
    }
    if let Some(pid) = frame.video_pid {
        let _ = write!(buf, " V:0x{pid:X}");
    }
    if let Some(pid) = frame.audio_pid {
        let _ = write!(buf, " A:0x{pid:X}");
    }
    if let Some(prog) = frame.program_number {
        let _ = write!(buf, " PROG {prog}");
    }
}

// ── Audio child ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn audio_loop(
    device: String,
    mut arx: mpsc::Receiver<AudioBlock>,
    clock: Arc<AudioClock>,
    counters: Arc<DisplayStatsCounters>,
    channel_pair: [u8; 2],
    master_clock: Option<crate::engine::master_clock::MasterClockHandle>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    output_id: String,
    frame_gen: Arc<AtomicU64>,
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
    // Genlock mode (Stage 2): attach the flow master clock so the resampler
    // drives the measured audio playout toward `master.now_90khz()`. Absent
    // (Stage 1 / muted) the backend runs audio-master and holds buffer fill.
    if let Some(m) = master_clock {
        backend.set_master_clock(m);
    }
    // Graceful-degradation state for repeated open failures (Bonus-K).
    //
    // Without this guard, a permanently misconfigured `audio_device`
    // (e.g. `hw:1,3` on a host with only card 0, or a host where the
    // running user isn't in the `audio` group so /dev/snd/* opens
    // EACCES) puts the loop into a tight retry: every blocking_recv
    // returns immediately because the upstream mpsc is full, every
    // backend.write() fails on snd_pcm_open, the 200 ms sleep makes
    // each iteration cost ~200 ms wall, and every iteration emits a
    // Critical event. On the edge1 testbed this was observed to fire
    // ~5 Critical events per second indefinitely and drop ~87 % of
    // decoded audio frames upstream from mpsc-full pressure.
    //
    // The fix: after `AUDIO_DEGRADE_THRESHOLD` consecutive same-code
    // failures, flip to silent-drain — emit ONE Critical "audio
    // disabled for output X" event, then keep consuming AudioBlocks
    // from the channel without calling backend.write(). The upstream
    // decode pipeline stops dropping (channel drains at full rate)
    // and the operator gets one actionable event instead of a flood.
    // The watchdog also stops dragging the loop to 5 Hz so the
    // demux task on the other side of the mpsc doesn't see
    // back-pressure-induced frame loss.
    //
    // Counter resets on the first successful write, so a transient
    // ALSA error (xrun + recover, USB-audio reseat) doesn't latch
    // the loop into silent mode.
    const AUDIO_DEGRADE_THRESHOLD: u32 = 10;
    let mut consecutive_failures: u32 = 0;
    let mut last_error_code: &'static str = "";
    let mut degraded: bool = false;
    while !cancel.is_cancelled() {
        let block = match arx.blocking_recv() {
            Some(b) => b,
            None => break,
        };
        // Drop pre-switch audio (see the matching gate in `display_loop`
        // for the rationale). ALSA `writei` blocks for the full block
        // duration on the master clock, so without this gate the
        // queued-but-stale audio after a switch would push the audio
        // clock that many ms into the previous stream's PTS base — a
        // sustained negative `av_sync_offset_ms` and a subsequent burst
        // of `frames_dropped_late` on the video side until the clock
        // re-anchors. Dropping here keeps the audio clock in lock-step
        // with the video gate so the post-switch first-frame `av_offset`
        // capture is meaningful.
        if block.frame_gen < frame_gen.load(Ordering::Relaxed) {
            continue;
        }
        // Degraded state: drain silently. No writes to backend (which
        // would just fail again), no events, no sleep. Upstream decode
        // pipeline drains the mpsc at full rate.
        if degraded {
            continue;
        }
        match backend.write(
            &block.planar,
            block.pts_90k,
            block.sample_rate,
            block.channels,
            &clock,
            channel_pair,
        ) {
            Ok(_) => {
                // Successful write — reset the failure counter so a
                // future xrun doesn't latch us into degraded mode.
                consecutive_failures = 0;
                last_error_code = "";
            }
            Err(e) => {
                let msg = e.to_string();
                let code: &'static str =
                    if msg.contains("display_audio_open_failed")
                        || msg.contains("snd_pcm_open")
                    {
                        "display_audio_device_invalid"
                    } else {
                        counters.audio_underruns.fetch_add(1, Ordering::Relaxed);
                        "display_audio_open_failed"
                    };
                if code == last_error_code {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                } else {
                    consecutive_failures = 1;
                    last_error_code = code;
                }
                if consecutive_failures >= AUDIO_DEGRADE_THRESHOLD {
                    // Latch into degraded mode. Emit the final
                    // Critical that names the persistent error so the
                    // operator can fix the underlying problem.
                    degraded = true;
                    emit_event(
                        &event_sender,
                        EventSeverity::Critical,
                        "display_audio_disabled_persistent_failure",
                        &flow_id,
                        &output_id,
                        &format!(
                            "display audio disabled — {consecutive_failures} consecutive \
                             '{code}' failures: {msg}. Channel drains silently; \
                             video continues. Fix the audio_device or user-group \
                             permission and restart the flow."
                        ),
                    );
                } else {
                    emit_event(
                        &event_sender,
                        EventSeverity::Critical,
                        code,
                        &flow_id,
                        &output_id,
                        &format!("display audio write failed: {msg}"),
                    );
                    // Reset and back off briefly — don't kill the whole
                    // output; video continues. (Retry budget still
                    // counts down; once it hits the threshold, we flip
                    // to silent-drain above.)
                    backend.reset();
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────

fn classify_kms_error(msg: &str) -> &'static str {
    if msg.contains("display_resolution_unsupported") {
        "display_resolution_unsupported"
    } else if msg.contains("display_master_busy") {
        // A compositor / display manager holds the DRM master — checked
        // before display_mode_set_failed because both ride the set_crtc
        // path; the EACCES variant is the master-busy one.
        "display_master_busy"
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

    /// The KMS-open error classifier must map a compositor-held DRM master
    /// (the `display_master_busy` context string emitted by `KmsDisplay::open`
    /// on EACCES) to its own code — checked BEFORE `display_mode_set_failed`
    /// because both ride the `set_crtc` path — so the operator-facing
    /// `display_output_waiting` event lifts the actionable "stop the
    /// compositor" code rather than the misleading "rejected the mode".
    #[test]
    fn classify_kms_error_distinguishes_master_busy_from_mode_set_failed() {
        assert_eq!(
            classify_kms_error(
                "display_master_busy: another DRM master (a desktop compositor / display \
                 manager such as GDM) already owns connector 'HDMI-A-1' — stop it"
            ),
            "display_master_busy"
        );
        assert_eq!(
            classify_kms_error("display_mode_set_failed: drmModeSetCrtc rejected the mode"),
            "display_mode_set_failed"
        );
        assert_eq!(
            classify_kms_error("display_resolution_unsupported: 4096x2160@60"),
            "display_resolution_unsupported"
        );
        // A master-busy string that also happens to mention the mode must
        // still classify as master_busy (ordering guarantee).
        assert_eq!(
            classify_kms_error(
                "display_master_busy: ... — see display_mode_set_failed troubleshooting"
            ),
            "display_master_busy"
        );
    }

    /// The PRIME-scanout demotion (option 2, Gen9 NV12 Yf_TILED black-screen
    /// fix) must engage on a permanent framebuffer-import rejection but stay
    /// dormant for a transient page-flip EBUSY — otherwise a one-frame EBUSY
    /// collision would permanently downgrade a working zero-copy output to
    /// the CPU-blit path. Error strings mirror the `context()` / `bail!`
    /// messages produced by `KmsDisplay::present_prime` in `display::kms`.
    #[test]
    fn prime_scanout_demotes_on_addfb_einval_not_on_page_flip_ebusy() {
        // Permanent — the modifier / descriptor is rejected on every frame.
        assert!(prime_scanout_permanently_rejected(
            "display zero-copy present failed: display_prime_addfb_failed: \
             add_planar_framebuffer: Invalid argument (os error 22)"
        ));
        assert!(prime_scanout_permanently_rejected(
            "display_prime_invalid: descriptor has no planes"
        ));
        // Transient — a previous nonblocking commit is still in flight; the
        // next frame retries. Must NOT demote.
        assert!(!prime_scanout_permanently_rejected(
            "display_prime_page_flip_failed: atomic_commit busy (frame dropped): \
             Device or resource busy (os error 16)"
        ));
        // Unrelated CPU-blit-path errors are not a zero-copy signal either.
        assert!(!prime_scanout_permanently_rejected(
            "display back-buffer unavailable"
        ));
    }

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
            prime: None,
            width: 4,
            height: 4,
            pixel_format: 0, // YUV420P
            colorspace: AVCOL_SPC_BT709,
            color_transfer: 0,
            full_range: false,
            pts_90k: 0,
            frame_gen: 0,
            codec: VideoCodec::H264,
            video_pid: None,
            audio_pid: None,
            program_number: None,
        };
        match planar.chroma {
            VideoFrameChroma::Planar { .. } => {}
            VideoFrameChroma::SemiPlanar { .. } | VideoFrameChroma::None => {
                panic!("expected Planar arm")
            }
        }

        let semi = VideoFrame {
            y: vec![0; 16],
            y_stride: 4,
            chroma: VideoFrameChroma::SemiPlanar {
                uv: vec![0; 8],
                uv_stride: 4,
            },
            prime: None,
            width: 4,
            height: 4,
            pixel_format: AVPixelFormat_AV_PIX_FMT_NV12_VAL,
            colorspace: AVCOL_SPC_BT709,
            color_transfer: 0,
            full_range: false,
            pts_90k: 0,
            frame_gen: 0,
            codec: VideoCodec::Hevc,
            video_pid: None,
            audio_pid: None,
            program_number: None,
        };
        match semi.chroma {
            VideoFrameChroma::SemiPlanar { .. } => {}
            VideoFrameChroma::Planar { .. } | VideoFrameChroma::None => {
                panic!("expected SemiPlanar arm")
            }
        }
    }

    /// The keyframe gate starts armed: pre-IDR AUs are shed, the first
    /// keyframe is admitted and opens the gate for everything after it.
    #[test]
    fn keyframe_gate_sheds_until_first_keyframe() {
        let mut gate = KeyframeGate::new();
        assert!(!gate.admit(false));
        assert!(!gate.admit(false));
        assert!(gate.admit(true));
        // Open — non-keyframes flow through.
        assert!(gate.admit(false));
        assert!(gate.admit(false));
    }

    /// `arm()` (decoder flush) closes an open gate and resets the skip
    /// budget; a keyframe AU presented while armed is admitted
    /// immediately (clean-cut switch costs zero extra frames).
    #[test]
    fn keyframe_gate_rearms_on_flush() {
        let mut gate = KeyframeGate::new();
        assert!(gate.admit(true));
        assert!(gate.admit(false));
        gate.arm();
        assert!(!gate.admit(false));
        assert!(gate.admit(true));
        assert!(gate.admit(false));
        // Flush landing exactly on a keyframe: admitted straight away.
        gate.arm();
        assert!(gate.admit(true));
    }

    /// Escape hatch: an IDR-less source (gradual intra refresh — the
    /// demuxer never flags `is_keyframe`) must not stay black forever.
    /// Past the skip budget the gate opens unconditionally.
    #[test]
    fn keyframe_gate_gives_up_after_skip_budget() {
        let mut gate = KeyframeGate::new();
        for _ in 0..KEYFRAME_GATE_MAX_SKIPPED_AUS - 1 {
            assert!(!gate.admit(false));
        }
        // The budget-exhausting AU is fed, and the gate stays open.
        assert!(gate.admit(false));
        assert!(gate.admit(false));
        // A later flush re-arms with a fresh budget.
        gate.arm();
        assert!(!gate.admit(false));
        assert!(gate.admit(true));
    }

    /// A timeout-opened gate marks the feed as speculative (HW
    /// send_packet rejections are expected and must not demote); the
    /// first real keyframe re-anchors the stream and clears the flag.
    #[test]
    fn keyframe_gate_speculative_flag_set_by_timeout_cleared_by_keyframe() {
        let mut gate = KeyframeGate::new();
        assert!(!gate.speculative_feed());
        for _ in 0..KEYFRAME_GATE_MAX_SKIPPED_AUS - 1 {
            assert!(!gate.admit(false));
        }
        assert!(gate.admit(false)); // budget exhausted — opens speculatively
        assert!(gate.speculative_feed());
        assert!(gate.admit(false)); // still speculative across non-keyframes
        assert!(gate.speculative_feed());
        assert!(gate.admit(true)); // real keyframe → anchored
        assert!(!gate.speculative_feed());
    }

    /// A gate opened by a real keyframe is never speculative, and a
    /// flush (`arm`) clears a lingering speculative flag.
    #[test]
    fn keyframe_gate_speculative_flag_clean_paths() {
        let mut gate = KeyframeGate::new();
        assert!(gate.admit(true));
        assert!(!gate.speculative_feed());
        assert!(gate.admit(false));
        assert!(!gate.speculative_feed());

        // Timeout-open, then flush before any keyframe: arm() resets.
        let mut gate = KeyframeGate::new();
        for _ in 0..KEYFRAME_GATE_MAX_SKIPPED_AUS {
            let _ = gate.admit(false);
        }
        assert!(gate.speculative_feed());
        gate.arm();
        assert!(!gate.speculative_feed());
    }

    /// The primary escape hatch is wall-clock based so the worst-case
    /// black period is fps-independent: 3 s after arming, the next AU is
    /// fed regardless of how few AUs a low-fps source produced.
    #[test]
    fn keyframe_gate_gives_up_after_wall_clock_budget() {
        let mut gate = KeyframeGate::new();
        assert!(!gate.admit(false), "fresh gate sheds non-keyframes");
        // Backdate the arm time past the budget — a 5 fps source has
        // produced only ~15 AUs by now, far under the AU budget.
        gate.armed_at = Instant::now() - KEYFRAME_GATE_MAX_WAIT - Duration::from_millis(1);
        assert!(gate.admit(false), "time budget opens the gate");
        assert!(gate.admit(false), "gate stays open");
        gate.arm();
        assert!(!gate.admit(false), "re-arm restores a fresh time budget");
    }

    /// Bob deinterlace row semantics: parity 0 rebuilds the plane from
    /// top-field rows (0,2,4…), parity 1 from bottom-field rows
    /// (1,3,5…), each field row replicated over its missing neighbour.
    #[test]
    fn bob_plane_replicates_field_rows() {
        // 4 rows × 2 bytes: rows [0,0], [1,1], [2,2], [3,3].
        let src: Vec<u8> = vec![0, 0, 1, 1, 2, 2, 3, 3];
        assert_eq!(bob_plane(&src, 2, 0), vec![0, 0, 0, 0, 2, 2, 2, 2]);
        assert_eq!(bob_plane(&src, 2, 1), vec![1, 1, 1, 1, 3, 3, 3, 3]);
    }

    /// Odd row counts clamp the replicated source row to the last row
    /// instead of reading past the plane.
    #[test]
    fn bob_plane_clamps_on_odd_row_count() {
        // 3 rows × 1 byte: [0], [1], [2].
        let src: Vec<u8> = vec![0, 1, 2];
        assert_eq!(bob_plane(&src, 1, 0), vec![0, 0, 2]);
        // Bottom field, row 2 wants row 3 — clamps to row 2.
        assert_eq!(bob_plane(&src, 1, 1), vec![1, 1, 2]);
    }

    /// Degenerate shapes must pass through untouched rather than panic:
    /// zero stride (defensive) and single-row planes.
    #[test]
    fn bob_plane_degenerate_shapes_pass_through() {
        assert_eq!(bob_plane(&[7, 8], 0, 0), vec![7, 8]);
        assert_eq!(bob_plane(&[7, 8], 2, 1), vec![7, 8]);
    }

    /// Semi-planar chroma (interleaved UV rows) bobs with the same row
    /// parity as luma — one UV row per chroma line, so field parity is
    /// row parity there too.
    #[test]
    fn bob_field_covers_semiplanar_chroma() {
        let y: Vec<u8> = vec![10, 11, 12, 13]; // 4 rows × 1
        let chroma = VideoFrameChroma::SemiPlanar {
            uv: vec![20, 21, 22, 23], // 2 rows × 2 (interleaved U,V)
            uv_stride: 2,
        };
        let (by, bc) = bob_field(&y, 1, &chroma, 0);
        assert_eq!(by, vec![10, 10, 12, 12]);
        match bc {
            VideoFrameChroma::SemiPlanar { uv, uv_stride } => {
                // 2 chroma rows: parity 0 keeps row 0 for both.
                assert_eq!(uv, vec![20, 21, 20, 21]);
                assert_eq!(uv_stride, 2);
            }
            _ => panic!("chroma layout must be preserved"),
        }
    }
}
