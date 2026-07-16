//! SDI capture I/O via Blackmagic DeckLink (mirrors `mxl_video_io.rs`).
//!
//! Input: captures packed 4:2:2 video (+ embedded PCM audio) from a DeckLink
//! card via `decklink-rs` (the Blackmagic SDK), unpacks UYVY422 to planar YUV,
//! encodes to H.264/HEVC, muxes into MPEG-TS, and publishes onto the flow's
//! broadcast channel. Mirrors the MXL / ST 2110-20 ingress encoder path. Unlike
//! MXL, a single device handle carries both video and audio, so this task owns
//! the whole A+V mux.
//!
//! # Resilience
//!
//! Three distinct failure modes, deliberately handled differently:
//!
//! * **Signal loss** (cable pulled, source rebooted, format mismatch). The card
//!   keeps delivering frames, flagged `signal_present = false`. We keep encoding
//!   them — holding the transport stream up is what downstream wants — but raise
//!   a Warning, and an Info when the signal returns. This is the common case and
//!   it does *not* restart anything.
//! * **Format change** (raster or frame rate), or the device disappearing
//!   entirely. The session is torn down, a Warning is raised, and the device is
//!   re-opened (with `format: "auto"` that re-detects the new format). The
//!   MPEG-TS muxer and the video/audio clocks persist across re-opens so
//!   downstream never sees the PCR or continuity counters step backwards.
//! * **Unrecoverable config** — an unsupported pixel format or chroma, or an
//!   encoder that will not open. These stop the input, since re-opening would
//!   just spin.
//!
//! Playout (SDI output) lives in `output_sdi.rs`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::models::SdiInputConfig;
use crate::engine::packet::RtpPacket;
use crate::manager::events::{Event, EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

#[cfg(feature = "media-codecs")]
use std::time::Instant;

#[cfg(feature = "media-codecs")]
use crate::stats::collector::SdiCaptureStats;
#[cfg(feature = "media-codecs")]
use bytes::Bytes;
#[cfg(feature = "media-codecs")]
use decklink_rs::{CapturedFrame, DecklinkCapture, DecklinkCaptureConfig, DecklinkPixelFormat};

#[cfg(feature = "media-codecs")]
use super::audio_encode::{AudioCodec, AudioEncoder, EncoderParams};
#[cfg(feature = "media-codecs")]
use crate::stats::collector::OutputStatsAccumulator;

/// Emit an SDI-input event.
///
/// Emitted under `category::SDI` — the same category the SDI *output* path uses
/// and the one the `category::SDI` doc contract promises — so a category-scoped
/// `sdi` alarm rule catches capture events (signal lost/restored, raster change,
/// capture-open failure) as well as playout events. `details` carries the
/// `error_code`; the manager keys triggers on **both** `category` and
/// `error_code`, so the two must agree (a mismatched category silently drops the
/// alarm). The flow *and* input scope are both set so the UI can attach the
/// alarm to the input's row.
fn emit_sdi(
    event_sender: &EventSender,
    severity: EventSeverity,
    input_id: &str,
    flow_id: &str,
    message: String,
    details: serde_json::Value,
) {
    event_sender.send(Event {
        severity,
        category: category::SDI.to_string(),
        message,
        details: Some(details),
        flow_id: Some(flow_id.to_string()),
        input_id: Some(input_id.to_string()),
        output_id: None,
    });
}

/// 90 kHz presentation timestamp of capture frame `frame_idx`, on a timeline
/// based at `base`.
///
/// Derived from the frame index against the exact rational frame period rather
/// than accumulated from a per-frame step: `90_000 * den / num` is not an
/// integer at 23.976 (3753.75) or 59.94 (1501.5), and a truncated step
/// accumulates the residue forever — 17.3 s/day and 28.8 s/day respectively.
/// Nothing downstream of an SDI input re-anchors video, while the AAC encoder
/// anchors once and self-advances by exact sample count, so every lost tick
/// lands as permanent lip-sync error. The u128 intermediate cannot overflow at
/// any reachable frame count.
#[cfg(feature = "media-codecs")]
fn frame_pts_90k(base: i64, frame_idx: u64, fr_num: u32, fr_den: u32) -> i64 {
    let num = fr_num.max(1) as u128;
    let den = fr_den as u128;
    base + ((frame_idx as u128 * 90_000 * den) / num) as i64
}

/// 90 kHz timestamp of the PCM frame at index `samples_total` in a session
/// based at `base`.
///
/// Recomputed from the running sample total for the same reason video is
/// recomputed from the frame index: at 48 kHz the per-sample factor is 15/8, so
/// a per-block step is exact only when the block is a multiple of 8 samples.
/// DeckLink delivers 1920 samples/frame at 25 fps (exact) but the SDK's block
/// size *must* vary at the NTSC rates (48000/29.97 = 1601.6), and the residue
/// would seed the next session's encoder anchor via `audio_pts_90khz`.
#[cfg(feature = "media-codecs")]
fn audio_pts_90k(base: u64, samples_total: u64, sample_rate: u32) -> u64 {
    base.wrapping_add((samples_total as u128 * 90_000 / sample_rate.max(1) as u128) as u64)
}

/// Publish a batch of freshly-muxed 188-byte TS packets onto the flow's
/// broadcast channel. Shared by the video and audio mux paths so both essences
/// ride the same A+V transport stream.
#[cfg(feature = "media-codecs")]
fn publish_ts(
    ts_packets: Vec<Bytes>,
    rtp_timestamp: u32,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
) {
    for ts in ts_packets {
        let ts_len = ts.len() as u64;
        let pkt = RtpPacket {
            data: ts,
            sequence_number: 0,
            rtp_timestamp,
            recv_time_us: crate::util::time::now_us(),
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        };
        flow_stats.input_packets.fetch_add(1, Ordering::Relaxed);
        flow_stats.input_bytes.fetch_add(ts_len, Ordering::Relaxed);
        let _ = broadcast_tx.send(pkt);
    }
}

/// Embedded-audio encode state for an SDI input.
///
/// The DeckLink device delivers interleaved 32-bit PCM alongside video from a
/// single handle, so this task owns the audio encode + mux too (unlike MXL,
/// where audio is a separate flow input).
#[cfg(feature = "media-codecs")]
struct SdiAudio {
    encoder: AudioEncoder,
    /// Per-channel planar f32 scratch, reused every audio block.
    planar: Vec<Vec<f32>>,
    /// Running audio presentation timestamp in 90 kHz ticks.
    pts_90khz: u64,
    /// Timeline origin this session's PCM is counted from.
    pts_base: u64,
    /// PCM frames submitted to the encoder this session.
    samples_total: u64,
    sample_rate: u32,
    channels: u8,
}

/// Unpack a packed UYVY422 row-major frame into planar 8-bit YUV.
///
/// UYVY byte order per 2-pixel macropixel is `U Y0 V Y1`.
///
/// `chroma_420` selects the destination chroma layout:
/// * `false` → **YUV422P**: chroma planes are `w/2 × h` (1:1 row copy).
/// * `true`  → **YUV420P**: chroma planes are `w/2 × h/2`, produced by
///   averaging each vertical pair of chroma rows.
///
/// Unpacking straight into the encoder's own input layout means
/// [`ScaledVideoEncoder`] passes the planes through verbatim — no libswscale
/// pass — and, critically, a 4:2:0 encoder never receives 4:2:2 planes (which
/// it would silently misread, producing ghosted chroma).
///
/// The caller must have validated that `data` holds `height * stride` bytes and
/// that `stride >= width * 2` (see the frame-size guard in the capture loop);
/// this function slices rows directly and would otherwise panic.
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn unpack_uyvy422(
    data: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    chroma_420: bool,
    y_out: &mut [u8],
    cb_out: &mut [u8],
    cr_out: &mut [u8],
) {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let row_bytes = w * 2;

    // Row slices are taken with exact lengths and walked with `chunks_exact`
    // so the bounds checks fold away and the loops vectorise.
    if !chroma_420 {
        // 4:2:2 — one chroma sample per macropixel, on every row.
        for row in 0..h {
            let src = &data[row * stride..row * stride + row_bytes];
            let y_row = &mut y_out[row * w..row * w + w];
            let cb_row = &mut cb_out[row * cw..row * cw + cw];
            let cr_row = &mut cr_out[row * cw..row * cw + cw];
            for (px, ((y2, cb), cr)) in src.chunks_exact(4).zip(
                y_row
                    .chunks_exact_mut(2)
                    .zip(cb_row.iter_mut())
                    .zip(cr_row.iter_mut()),
            ) {
                *cb = px[0];
                y2[0] = px[1];
                *cr = px[2];
                y2[1] = px[3];
            }
        }
        return;
    }

    // 4:2:0 — luma at full rate, chroma averaged over each vertical row pair.
    for row in 0..h {
        let src = &data[row * stride..row * stride + row_bytes];
        let y_row = &mut y_out[row * w..row * w + w];
        for (px, y2) in src.chunks_exact(4).zip(y_row.chunks_exact_mut(2)) {
            y2[0] = px[1];
            y2[1] = px[3];
        }
    }
    for ry in 0..h / 2 {
        let top = ry * 2 * stride;
        let bot = (ry * 2 + 1) * stride;
        let r0 = &data[top..top + row_bytes];
        let r1 = &data[bot..bot + row_bytes];
        let cb_row = &mut cb_out[ry * cw..ry * cw + cw];
        let cr_row = &mut cr_out[ry * cw..ry * cw + cw];
        for (((a, b), cb), cr) in r0
            .chunks_exact(4)
            .zip(r1.chunks_exact(4))
            .zip(cb_row.iter_mut())
            .zip(cr_row.iter_mut())
        {
            *cb = ((a[0] as u16 + b[0] as u16) >> 1) as u8;
            *cr = ((a[2] as u16 + b[2] as u16) >> 1) as u8;
        }
    }
}

/// Test-only re-export so `output_sdi`'s round-trip test can pin
/// `pack_uyvy422` as the exact inverse of this module's unpacker.
#[cfg(all(test, feature = "media-codecs"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn unpack_uyvy422_for_test(
    data: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    chroma_420: bool,
    y_out: &mut [u8],
    cb_out: &mut [u8],
    cr_out: &mut [u8],
) {
    unpack_uyvy422(
        data, stride, width, height, chroma_420, y_out, cb_out, cr_out,
    )
}

pub fn spawn_sdi_input(
    config: SdiInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_sdi_input(
            config,
            input_id,
            broadcast_tx,
            flow_stats,
            cancel,
            event_sender,
            flow_id,
        )
        .await;
    })
}

async fn run_sdi_input(
    config: SdiInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) {
    let ctx = format!(
        "SDI input '{input_id}' (flow={flow_id} device='{}')",
        config.device
    );

    #[cfg(feature = "media-codecs")]
    {
        tokio::task::block_in_place(|| {
            sdi_input_blocking_loop(
                &ctx,
                &config,
                &input_id,
                &flow_id,
                &broadcast_tx,
                &flow_stats,
                &cancel,
                &event_sender,
            );
        });
    }
    #[cfg(not(feature = "media-codecs"))]
    {
        let _ = (&config, &broadcast_tx, &flow_stats);
        emit_sdi(
            &event_sender,
            EventSeverity::Critical,
            &input_id,
            &flow_id,
            format!("{ctx}: SDI input requires the media-codecs feature"),
            serde_json::json!({ "error_code": "sdi_no_media_codecs" }),
        );
        cancel.cancelled().await;
    }

    info!(target: "sdi.in", "{ctx}: capture loop exited");
}

/// Delay before re-opening the device after a capture session ends. The
/// DeckLink driver needs a moment to release the input before it can be
/// re-acquired, and this also bounds the retry rate on a dead device.
#[cfg(feature = "media-codecs")]
const SDI_REOPEN_BACKOFF: Duration = Duration::from_millis(500);

/// How long one frame read waits before the loop checks its cancellation token
/// again. The read has to be bounded here rather than parked on the card: a
/// device that has gone away is exactly the case where no frame ever arrives to
/// return control, and it is also when a flow stop most wants to complete.
#[cfg(feature = "media-codecs")]
const SDI_READ_POLL_MS: u32 = 250;

/// Silence this long means the *device* went away, not the signal — a live
/// input delivers frames continuously, and even an unlocked card keeps emitting
/// frames flagged no-signal. Re-open on it.
#[cfg(feature = "media-codecs")]
const SDI_DEVICE_SILENCE: Duration = Duration::from_secs(5);

/// Build the per-session embedded-audio encode state.
///
/// Best-effort by design: an unsupported codec or a failed encoder init warns
/// and returns `None` so the flow continues video-only rather than dying.
///
/// `start_pts_90khz` lets audio timestamps continue across a device re-open, so
/// downstream never sees the audio clock jump backwards.
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn setup_audio(
    ctx: &str,
    config: &SdiInputConfig,
    input_id: &str,
    flow_id: &str,
    cap: &DecklinkCapture,
    cancel: &CancellationToken,
    event_sender: &EventSender,
    ts_mux: &mut crate::engine::rtmp::ts_mux::TsMuxer,
    ts_audio_configured: &mut bool,
    start_pts_90khz: u64,
) -> Option<SdiAudio> {
    if config.audio_channels == 0 || cap.audio_channels() == 0 || cap.audio_sample_rate() == 0 {
        return None;
    }

    let acodec = config
        .audio_encode
        .as_ref()
        .and_then(|a| AudioCodec::parse(&a.codec))
        .unwrap_or(AudioCodec::AacLc);

    if !matches!(
        acodec,
        AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2
    ) {
        emit_sdi(
            event_sender,
            EventSeverity::Warning,
            input_id,
            flow_id,
            format!(
                "{ctx}: audio_encode.codec '{}' is not supported on an SDI input \
                 (AAC family only) — continuing video-only",
                acodec.as_str()
            ),
            serde_json::json!({
                "error_code": "sdi_audio_codec_unsupported",
                "codec": acodec.as_str(),
            }),
        );
        return None;
    }

    let ach = cap.audio_channels();
    let ahz = cap.audio_sample_rate();
    let bitrate = config
        .audio_encode
        .as_ref()
        .and_then(|a| a.bitrate_kbps)
        .unwrap_or_else(|| acodec.default_bitrate_kbps());

    let params = EncoderParams {
        codec: acodec,
        sample_rate: ahz,
        channels: ach,
        target_bitrate_kbps: bitrate,
        target_sample_rate: ahz,
        target_channels: ach,
        opus_vbr_mode: None,
        opus_fec: false,
        opus_dtx: false,
        opus_frame_duration_ms: None,
    };
    // `AudioEncoder::spawn` wants an OutputStatsAccumulator; this input has no
    // output identity, so give it a synthetic one (as `input_pcm_encode` does).
    let encoder_stats = Arc::new(OutputStatsAccumulator::new(
        format!("sdi-input-encode:{input_id}"),
        "sdi-input-encode".to_string(),
        "synthetic".to_string(),
    ));

    match AudioEncoder::spawn(
        params,
        cancel.clone(),
        flow_id.to_string(),
        input_id.to_string(),
        encoder_stats,
        None,
    ) {
        Ok(encoder) => {
            // AAC in ADTS => stream_type 0x0F, no registration descriptor. Only
            // stamp the PMT once: re-declaring it on every re-open would churn
            // the PMT version for no reason.
            if !*ts_audio_configured {
                ts_mux.set_has_audio(true);
                ts_mux.set_audio_stream(0x0F, None);
                *ts_audio_configured = true;
            }
            emit_sdi(
                event_sender,
                EventSeverity::Info,
                input_id,
                flow_id,
                format!(
                    "{ctx}: embedded audio {ach}ch @ {ahz} Hz -> {} {bitrate} kbps",
                    acodec.as_str()
                ),
                serde_json::json!({
                    "error_code": "sdi_audio_started",
                    "codec": acodec.as_str(),
                    "channels": ach,
                    "sample_rate": ahz,
                    "bitrate_kbps": bitrate,
                }),
            );
            Some(SdiAudio {
                encoder,
                planar: vec![Vec::new(); ach as usize],
                pts_90khz: start_pts_90khz,
                pts_base: start_pts_90khz,
                samples_total: 0,
                sample_rate: ahz,
                channels: ach,
            })
        }
        Err(e) => {
            tracing::error!(target: "sdi.in", "{ctx}: audio encoder init failed: {e}");
            emit_sdi(
                event_sender,
                EventSeverity::Warning,
                input_id,
                flow_id,
                format!("{ctx}: audio encoder init failed: {e} — continuing video-only"),
                serde_json::json!({
                    "error_code": "sdi_audio_encoder_init_failed",
                    "error": e.to_string(),
                }),
            );
            None
        }
    }
}

/// Why a capture session ended.
#[cfg(feature = "media-codecs")]
enum SessionEnd {
    /// Flow is shutting down — do not re-open.
    Cancelled,
    /// The device errored, or the source raster changed. Re-open and carry on.
    Retry,
    /// Unrecoverable (encoder cannot open). Give up; a re-open would spin.
    Fatal,
}

#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn sdi_input_blocking_loop(
    ctx: &str,
    config: &SdiInputConfig,
    input_id: &str,
    flow_id: &str,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
    cancel: &CancellationToken,
    event_sender: &EventSender,
) {
    // ── configuration checks (fatal — re-opening would never fix these) ──
    let pixel_format = match config.pixel_format.as_str() {
        "v210" => DecklinkPixelFormat::V210,
        _ => DecklinkPixelFormat::Uyvy422,
    };
    // v210 (10-bit) unpack is not wired yet — only UYVY422 8-bit is unpacked
    // below. Refuse v210 loudly rather than emit garbage planes.
    if matches!(pixel_format, DecklinkPixelFormat::V210) {
        emit_sdi(
            event_sender,
            EventSeverity::Critical,
            input_id,
            flow_id,
            format!("{ctx}: pixel_format=v210 not yet supported; use uyvy422"),
            serde_json::json!({
                "error_code": "sdi_pixfmt_unsupported",
                "pixel_format": config.pixel_format,
            }),
        );
        return;
    }

    let enc = &config.video_encode;

    // Unpack UYVY422 straight into the encoder's own input layout so the planes
    // pass through verbatim (no libswscale) AND a 4:2:0 encoder never silently
    // misreads 4:2:2 chroma planes.
    let target_chroma = crate::engine::video_encode_util::resolve_chroma(enc.chroma.as_deref());
    let bit_depth = enc.bit_depth.unwrap_or(8);
    let chroma_420 = match (target_chroma, bit_depth) {
        (video_codec::VideoChroma::Yuv420, 8) => true,
        (video_codec::VideoChroma::Yuv422, 8) => false,
        _ => {
            let msg = format!(
                "{ctx}: SDI capture is 8-bit 4:2:2 (uyvy422); video_encode chroma={target_chroma:?} \
                 bit_depth={bit_depth} is unsupported — use yuv420p or yuv422p at 8-bit"
            );
            tracing::error!(target: "sdi.in", "{msg}");
            emit_sdi(
                event_sender,
                EventSeverity::Critical,
                input_id,
                flow_id,
                msg,
                serde_json::json!({
                    "error_code": "sdi_encode_chroma_unsupported",
                    "chroma": format!("{target_chroma:?}"),
                    "bit_depth": bit_depth,
                }),
            );
            return;
        }
    };
    let Some(src_pix_fmt) = video_engine::av_pix_fmt_for_yuv(target_chroma, bit_depth) else {
        let msg = format!(
            "{ctx}: no FFmpeg pixel format for chroma={target_chroma:?} bit_depth={bit_depth}"
        );
        tracing::error!(target: "sdi.in", "{msg}");
        emit_sdi(
            event_sender,
            EventSeverity::Critical,
            input_id,
            flow_id,
            msg,
            serde_json::json!({
                "error_code": "sdi_encode_chroma_unsupported",
                "chroma": format!("{target_chroma:?}"),
                "bit_depth": bit_depth,
            }),
        );
        return;
    };

    let cap_cfg = DecklinkCaptureConfig {
        device: config.device.clone(),
        format: config.format.clone(),
        pixel_format,
        audio_channels: config.audio_channels,
        audio_sample_rate: 48_000,
    };

    // ── state that must survive a device re-open ──
    // Re-creating the muxer or resetting timestamps on every reconnect would
    // step the PCR/PTS backwards and reset continuity counters, which receivers
    // see as a stream fault. Keep them across sessions.
    let mut ts_mux = crate::engine::rtmp::ts_mux::TsMuxer::new();
    ts_mux.set_has_video(true);
    if config.scte35_extraction {
        ts_mux.set_scte35_stream(crate::engine::rtmp::ts_mux::DEFAULT_SCTE35_PID);
    }
    let mut ts_audio_configured = false;
    let mut pts: i64 = 0;
    let mut audio_pts_90khz: u64 = 0;
    let mut session: u64 = 0;

    // Telemetry the byte stream cannot express: signal lock, shim frame drops,
    // session count. Registered before the first open so the manager sees an
    // honest `signal_present: false` while a device is still being retried.
    let sdi_stats = Arc::new(SdiCaptureStats::default());
    flow_stats.set_sdi_capture_stats(input_id, sdi_stats.clone());

    // The shim's drop counter is per-capture-handle and restarts at zero on
    // every re-open, so carry a base forward to keep the reported figure
    // cumulative across sessions.
    let mut dropped_base: u64 = 0;

    // ── supervision loop: one iteration == one capture session ──
    // A cable pull, a source reboot or a raster change are routine in a
    // broadcast plant, so they must not kill the input task.
    loop {
        if cancel.is_cancelled() {
            break;
        }
        session += 1;
        sdi_stats.sessions.store(session, Ordering::Relaxed);

        let Some(cap) = open_capture(
            ctx,
            &cap_cfg,
            input_id,
            flow_id,
            session,
            cancel,
            event_sender,
        ) else {
            break; // cancelled while retrying
        };

        let end = run_capture_session(
            ctx,
            config,
            input_id,
            flow_id,
            cap,
            session,
            chroma_420,
            src_pix_fmt,
            &mut ts_mux,
            &mut ts_audio_configured,
            &mut pts,
            &mut audio_pts_90khz,
            broadcast_tx,
            flow_stats,
            &sdi_stats,
            dropped_base,
            cancel,
            event_sender,
        );
        dropped_base = sdi_stats.frames_dropped.load(Ordering::Relaxed);

        // The card is gone or re-opening; it is not locked to a signal until a
        // frame proves otherwise.
        sdi_stats.signal_present.store(false, Ordering::Relaxed);
        // Caption presence and timecode are per-session telemetry ("fresh on
        // every re-open"). The session-local dedup bools reset each session,
        // but the *reported* stats are sticky atomics that are only ever set
        // true — without this reset a switch to an uncaptioned / no-timecode
        // source keeps reporting the previous session's values forever.
        sdi_stats.set_captions_present(false, false);
        sdi_stats.store_timecode(None);

        match end {
            SessionEnd::Cancelled | SessionEnd::Fatal => break,
            SessionEnd::Retry => {
                if cancel.is_cancelled() {
                    break;
                }
                std::thread::sleep(SDI_REOPEN_BACKOFF);
            }
        }
    }
}

/// Open the DeckLink input, retrying until it succeeds or the flow is cancelled.
///
/// Emits one Warning on the first failure of a session (so an operator sees a
/// pulled cable) and then stays quiet, to avoid flooding the event bus while a
/// device is unplugged.
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn open_capture(
    ctx: &str,
    cap_cfg: &DecklinkCaptureConfig,
    input_id: &str,
    flow_id: &str,
    session: u64,
    cancel: &CancellationToken,
    event_sender: &EventSender,
) -> Option<DecklinkCapture> {
    let mut announced = false;
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        match DecklinkCapture::open(cap_cfg.clone()) {
            Ok(c) => {
                let (w, h) = c.video_dimensions();
                let (n, d) = c.video_frame_rate();
                info!(
                    target: "sdi.in",
                    "{ctx}: capture opened {w}x{h} @ {n}/{d} (session {session})"
                );
                emit_sdi(
                    event_sender,
                    EventSeverity::Info,
                    input_id,
                    flow_id,
                    format!("{ctx}: capture opened {w}x{h} @ {n}/{d} (session {session})"),
                    serde_json::json!({
                        "error_code": "sdi_capture_opened",
                        "width": w,
                        "height": h,
                        "frame_rate_num": n,
                        "frame_rate_den": d,
                        "session": session,
                    }),
                );
                return Some(c);
            }
            Err(e) => {
                if !announced {
                    announced = true;
                    tracing::warn!(target: "sdi.in", "{ctx}: capture open failed: {e}; retrying");
                    emit_sdi(
                        event_sender,
                        EventSeverity::Warning,
                        input_id,
                        flow_id,
                        format!(
                            "{ctx}: capture open failed: {e} — retrying until the device returns"
                        ),
                        serde_json::json!({
                            "error_code": "sdi_capture_open_failed",
                            "error": e.to_string(),
                            "session": session,
                        }),
                    );
                } else {
                    debug!(target: "sdi.in", "{ctx}: open failed: {e}, retrying");
                }
                std::thread::sleep(SDI_REOPEN_BACKOFF);
            }
        }
    }
}

/// Decide whether a decoded caption packet is a **new** detection for its
/// type this session, updating the caller's sticky presence flags. Pure
/// (no I/O) so the transition logic — fire once, never re-fire, independent
/// per type — is unit-testable without a capture session.
///
/// Returns `Some(kind_label)` exactly on the transition from absent to
/// present for that type; `None` on every subsequent packet of the same
/// type, or if `kind` was already present.
#[cfg(feature = "media-codecs")]
fn caption_newly_detected(
    kind: crate::engine::st2110::captions::CaptionType,
    cea608_present: &mut bool,
    cea708_present: &mut bool,
) -> Option<&'static str> {
    let present = match kind {
        crate::engine::st2110::captions::CaptionType::Cea608 => cea608_present,
        crate::engine::st2110::captions::CaptionType::Cea708 => cea708_present,
    };
    if *present {
        return None;
    }
    *present = true;
    Some(match kind {
        crate::engine::st2110::captions::CaptionType::Cea608 => "CEA-608",
        crate::engine::st2110::captions::CaptionType::Cea708 => "CEA-708",
    })
}

/// Drive one capture session to completion, returning why it ended.
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn run_capture_session(
    ctx: &str,
    config: &SdiInputConfig,
    input_id: &str,
    flow_id: &str,
    mut cap: DecklinkCapture,
    session: u64,
    chroma_420: bool,
    src_pix_fmt: i32,
    ts_mux: &mut crate::engine::rtmp::ts_mux::TsMuxer,
    ts_audio_configured: &mut bool,
    pts: &mut i64,
    audio_pts_90khz: &mut u64,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
    sdi_stats: &Arc<SdiCaptureStats>,
    dropped_base: u64,
    cancel: &CancellationToken,
    event_sender: &EventSender,
) -> SessionEnd {
    use crate::engine::video_encode_util::ScaledVideoEncoder;

    let (width, height) = cap.video_dimensions();
    let (fr_num, fr_den) = cap.video_frame_rate();
    let enc = &config.video_encode;

    let enc_cfg = match crate::engine::st2110_video_io::build_encoder_config(
        enc, width, height, fr_num, fr_den,
    ) {
        Ok(c) => c,
        Err(e) => {
            // Log as well as emit: the event only reaches a connected manager,
            // so a standalone edge would otherwise fail silently here.
            tracing::error!(target: "sdi.in", "{ctx}: encoder config failed: {e}");
            emit_sdi(
                event_sender,
                EventSeverity::Critical,
                input_id,
                flow_id,
                format!("{ctx}: encoder config failed: {e}"),
                serde_json::json!({
                    "error_code": "sdi_encode_config_failed",
                    "error": e.to_string(),
                }),
            );
            return SessionEnd::Fatal;
        }
    };

    let mut pipeline = ScaledVideoEncoder::new(
        enc.clone(),
        enc_cfg.codec,
        fr_num,
        fr_den,
        false,
        format!("SDI input:{ctx}"),
    );
    // The pts fed below are 90 kHz ticks (they flow straight into the TS
    // mux), not a frame counter. Without this, libx264 reads them against a
    // 1/fps timebase and its VBV rate control segfaults ~one lookahead-depth
    // of frames in. NVENC merely tolerated the same mistake.
    pipeline.set_pts_90k();

    let mut audio = setup_audio(
        ctx,
        config,
        input_id,
        flow_id,
        &cap,
        cancel,
        event_sender,
        ts_mux,
        ts_audio_configured,
        *audio_pts_90khz,
    );

    // Video timestamps are derived from a frame index against the session's
    // rational frame period (see `frame_pts_90k`). `pts_base` carries the
    // timeline in from the previous session so a re-open continues it.
    let pts_base: i64 = *pts;
    let mut frame_idx: u64 = 0;

    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let ch = if chroma_420 { h / 2 } else { h };
    let mut y_plane = vec![0u8; w * h];
    let mut cb_plane = vec![0u8; cw * ch];
    let mut cr_plane = vec![0u8; cw * ch];

    emit_sdi(
        event_sender,
        EventSeverity::Info,
        input_id,
        flow_id,
        format!(
            "{ctx}: capture→encode pipeline started (session {session}, codec={})",
            enc.codec
        ),
        serde_json::json!({
            "error_code": "sdi_pipeline_started",
            "session": session,
            "codec": enc.codec,
            "width": width,
            "height": height,
            "frame_rate_num": fr_num,
            "frame_rate_den": fr_den,
        }),
    );

    // Make sure the audio clock is carried back out even on an early return.
    macro_rules! save_audio_pts {
        () => {
            if let Some(a) = audio.as_ref() {
                *audio_pts_90khz = a.pts_90khz;
            }
        };
    }

    // `None` until the first frame tells us whether the input is locked.
    let mut signal_present: Option<bool> = None;
    // Caption presence is tracked per session (fresh on every re-open, same
    // as every other per-session state here) so `sdi_captions_detected`
    // fires once per type per session, not once per packet.
    let mut cea608_present = false;
    let mut cea708_present = false;
    let mut last_frame_at = Instant::now();

    loop {
        if cancel.is_cancelled() {
            save_audio_pts!();
            return SessionEnd::Cancelled;
        }

        // Device error / stream ended / gone silent — most often a pulled cable
        // or a source reboot. Surface it and re-open rather than dying.
        let mut lost: Option<String> = None;
        let frame = match cap.read_frame_timeout(SDI_READ_POLL_MS) {
            Ok(Some(f)) => {
                last_frame_at = Instant::now();
                Some(f)
            }
            Ok(None) => {
                if last_frame_at.elapsed() >= SDI_DEVICE_SILENCE {
                    lost = Some(format!("no frame for {} s", SDI_DEVICE_SILENCE.as_secs()));
                }
                None
            }
            Err(e) => {
                lost = Some(e.to_string());
                None
            }
        };
        if let Some(reason) = lost {
            save_audio_pts!();
            tracing::warn!(target: "sdi.in", "{ctx}: capture ended: {reason}; reopening");
            emit_sdi(
                event_sender,
                EventSeverity::Warning,
                input_id,
                flow_id,
                format!("{ctx}: capture ended: {reason} — reopening device"),
                serde_json::json!({
                    "error_code": "sdi_capture_lost",
                    "error": reason,
                    "session": session,
                }),
            );
            return SessionEnd::Retry;
        }
        let Some(frame) = frame else { continue };

        match frame {
            CapturedFrame::Video(vf) => {
                // This frame's PTS on the session timeline — the same value the
                // encoder is handed below, so a cue extracted from this frame's
                // VANC is anchored to the picture it rode in on. Every path that
                // abandons the frame leaves `frame_idx` alone, so the slot
                // passes to the next frame and the anchor stays truthful.
                let frame_pts = frame_pts_90k(pts_base, frame_idx, fr_num, fr_den);

                // VANC rides the frame, so this has to run before any guard
                // below can abandon it: a frame carrying an unusable raster or
                // a short video payload can still carry a perfectly good cue.
                if config.scte35_extraction
                    || config.timecode_extraction
                    || config.captions_extraction
                {
                    for captured in &vf.ancillary {
                        let anc = crate::engine::st2110::ancillary::AncPacket::simple(
                            captured.did,
                            captured.sdid,
                            captured.line_number,
                            captured.data.clone(),
                        );
                        if config.scte35_extraction
                            && crate::engine::st2110::scte104::is_scte104_packet(&anc)
                        {
                            match crate::engine::st2110::scte104::parse_scte104(&anc.user_data) {
                                Ok(msg)
                                    if matches!(msg.opcode,
                                    crate::engine::st2110::scte104::SpliceOpcode::SpliceStartNormal
                                    | crate::engine::st2110::scte104::SpliceOpcode::SpliceEndNormal
                                    | crate::engine::st2110::scte104::SpliceOpcode::SpliceCancel) =>
                                {
                                    let section =
                                        crate::engine::scte35_encode::build_splice_insert_section(
                                            &msg,
                                            frame_pts.max(0) as u64,
                                        );
                                    let packets = ts_mux.mux_scte35(&section);
                                    if !packets.is_empty() {
                                        publish_ts(
                                            packets,
                                            frame_pts.max(0) as u32,
                                            broadcast_tx,
                                            flow_stats,
                                        );
                                        sdi_stats
                                            .scte35_cues_emitted
                                            .fetch_add(1, Ordering::Relaxed);
                                        let event_id = msg.splice_event_id.unwrap_or(0);
                                        emit_sdi(
                                            event_sender,
                                            EventSeverity::Info,
                                            input_id,
                                            flow_id,
                                            format!(
                                                "{ctx}: emitted SCTE-35 cue event_id={event_id} \
                                                 opcode={:?}",
                                                msg.opcode
                                            ),
                                            serde_json::json!({
                                                "error_code": "sdi_scte35_emitted",
                                                "session": session,
                                                "splice_event_id": event_id,
                                                "opcode": format!("{:?}", msg.opcode),
                                            }),
                                        );
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    debug!(target: "sdi.in", "{ctx}: invalid SCTE-104 VANC packet: {e}")
                                }
                            }
                        } else if config.timecode_extraction
                            && crate::engine::st2110::timecode::is_atc_packet(&anc)
                        {
                            match crate::engine::st2110::timecode::parse_timecode(&anc.user_data) {
                                Ok(tc) => sdi_stats.store_timecode(Some(tc)),
                                Err(e) => {
                                    debug!(target: "sdi.in", "{ctx}: invalid ATC VANC packet: {e}")
                                }
                            }
                        } else if config.captions_extraction
                            && crate::engine::st2110::captions::is_caption_packet(&anc)
                        {
                            match crate::engine::st2110::captions::parse_caption(&anc) {
                                Ok(summary) => {
                                    if let Some(kind_str) = caption_newly_detected(
                                        summary.kind,
                                        &mut cea608_present,
                                        &mut cea708_present,
                                    ) {
                                        emit_sdi(
                                            event_sender,
                                            EventSeverity::Info,
                                            input_id,
                                            flow_id,
                                            format!(
                                                "{ctx}: {kind_str} captions detected \
                                                 (cc_count={})",
                                                summary.cc_count
                                            ),
                                            serde_json::json!({
                                                "error_code": "sdi_captions_detected",
                                                "session": session,
                                                "caption_type": kind_str,
                                                "cc_count": summary.cc_count,
                                            }),
                                        );
                                    }
                                    // Reflect cumulative session presence (both
                                    // types independently), not just this one
                                    // packet's kind.
                                    sdi_stats.set_captions_present(cea608_present, cea708_present);
                                }
                                Err(e) => {
                                    debug!(target: "sdi.in", "{ctx}: invalid caption VANC packet: {e}")
                                }
                            }
                        }
                    }
                }

                // A format change means the scratch planes, the encoder and the
                // PTS cadence are all built for the wrong format. Tear the
                // session down and re-open: with `format: "auto"` that
                // re-detects it. The frame rate has to be part of the test —
                // 1080i50 and 1080p50 are both 1920x1080, so a rate-only change
                // (a router crosspoint, a source rebooting into another format,
                // a camera swap) is invisible to a dimension check while every
                // later timestamp is spaced for the wrong cadence.
                let (cur_num, cur_den) = cap.video_frame_rate();
                if vf.width != width
                    || vf.height != height
                    || cur_num != fr_num
                    || cur_den != fr_den
                {
                    save_audio_pts!();
                    tracing::warn!(
                        target: "sdi.in",
                        "{ctx}: source format changed {width}x{height} @ {fr_num}/{fr_den} -> \
                         {}x{} @ {cur_num}/{cur_den}; restarting capture",
                        vf.width, vf.height,
                    );
                    emit_sdi(
                        event_sender,
                        EventSeverity::Warning,
                        input_id,
                        flow_id,
                        format!(
                            "{ctx}: source format changed {width}x{height} @ {fr_num}/{fr_den} -> \
                             {}x{} @ {cur_num}/{cur_den} — restarting capture",
                            vf.width, vf.height,
                        ),
                        serde_json::json!({
                            "error_code": "sdi_raster_changed",
                            "width": vf.width,
                            "height": vf.height,
                            "frame_rate_num": cur_num,
                            "frame_rate_den": cur_den,
                            "previous_width": width,
                            "previous_height": height,
                            "previous_frame_rate_num": fr_num,
                            "previous_frame_rate_den": fr_den,
                        }),
                    );
                    return SessionEnd::Retry;
                }

                // Signal presence comes straight from the card
                // (`bmdFrameHasNoInputSource`). Alarm on the transition, but keep
                // encoding: the card substitutes bars/black, and holding the TS up
                // is what downstream decoders and sockets want.
                match (signal_present, vf.signal_present) {
                    (Some(true), false) | (None, false) => {
                        sdi_stats.signal_losses.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(target: "sdi.in", "{ctx}: SDI input signal lost");
                        emit_sdi(
                            event_sender,
                            EventSeverity::Warning,
                            input_id,
                            flow_id,
                            format!(
                                "{ctx}: SDI input signal lost — no source locked; continuing to \
                                 emit the card's bars/black"
                            ),
                            serde_json::json!({
                                "error_code": "sdi_signal_lost",
                                "session": session,
                                "signal_losses": sdi_stats.signal_losses.load(Ordering::Relaxed),
                            }),
                        );
                    }
                    (Some(false), true) => {
                        info!(target: "sdi.in", "{ctx}: SDI input signal restored");
                        emit_sdi(
                            event_sender,
                            EventSeverity::Info,
                            input_id,
                            flow_id,
                            format!("{ctx}: SDI input signal restored"),
                            serde_json::json!({
                                "error_code": "sdi_signal_restored",
                                "session": session,
                            }),
                        );
                    }
                    _ => {}
                }
                signal_present = Some(vf.signal_present);
                sdi_stats
                    .signal_present
                    .store(vf.signal_present, Ordering::Relaxed);
                // The shim counts frames it dropped because we fell behind the
                // SDI cadence — the one drop cause no transport counter sees.
                sdi_stats
                    .frames_dropped
                    .store(dropped_base + cap.dropped_frames(), Ordering::Relaxed);

                // Defensive: `unpack_uyvy422` slices rows directly, so a short or
                // unexpectedly-strided frame would panic. Skip it — a dropped
                // frame is always preferable to a dead flow.
                let need = vf.stride.saturating_mul(height as usize);
                if vf.stride < w * 2 || vf.data.len() < need {
                    debug!(
                        target: "sdi.in",
                        "{ctx}: skipping malformed frame (stride={} len={}, need stride>={} len>={need})",
                        vf.stride, vf.data.len(), w * 2,
                    );
                    continue;
                }

                unpack_uyvy422(
                    &vf.data,
                    vf.stride,
                    width,
                    height,
                    chroma_420,
                    &mut y_plane,
                    &mut cb_plane,
                    &mut cr_plane,
                );

                match pipeline.encode_raw_planes(
                    width,
                    height,
                    src_pix_fmt,
                    &y_plane,
                    w,
                    &cb_plane,
                    cw,
                    &cr_plane,
                    cw,
                    Some(frame_pts),
                ) {
                    Ok(frames) => {
                        for ef in frames {
                            let ts_packets = ts_mux.mux_video(
                                &ef.data,
                                ef.pts as u64,
                                ef.dts as u64,
                                ef.keyframe,
                            );
                            publish_ts(ts_packets, ef.pts as u32, broadcast_tx, flow_stats);
                        }
                    }
                    Err(e) => {
                        if !pipeline.is_open() {
                            // The encoder itself will not open (bad params, no
                            // GPU session). Re-opening the device would spin.
                            save_audio_pts!();
                            tracing::error!(target: "sdi.in", "{ctx}: encoder open failed: {e}");
                            emit_sdi(
                                event_sender,
                                EventSeverity::Critical,
                                input_id,
                                flow_id,
                                format!("{ctx}: encoder open failed: {e}"),
                                serde_json::json!({
                                    "error_code": "sdi_encode_failed",
                                    "error": e.to_string(),
                                }),
                            );
                            return SessionEnd::Fatal;
                        }
                        debug!(target: "sdi.in", "{ctx}: encode error: {e}");
                    }
                }

                frame_idx += 1;
                *pts = frame_pts_90k(pts_base, frame_idx, fr_num, fr_den);
            }
            CapturedFrame::Audio(af) => {
                let Some(a) = audio.as_mut() else { continue };
                let ch = a.channels as usize;
                if ch == 0 {
                    continue;
                }
                let n_frames = af.samples.len() / ch;
                if n_frames == 0 {
                    continue;
                }

                // De-interleave 32-bit PCM into per-channel planar f32, which is
                // the layout the shared audio encoder consumes.
                const S32_SCALE: f32 = 1.0 / 2_147_483_648.0;
                for p in a.planar.iter_mut() {
                    p.clear();
                    p.reserve(n_frames);
                }
                for f in 0..n_frames {
                    let base = f * ch;
                    for (c, plane) in a.planar.iter_mut().enumerate() {
                        plane.push(af.samples[base + c] as f32 * S32_SCALE);
                    }
                }

                a.encoder.submit_planar(&a.planar, a.pts_90khz);
                a.samples_total = a.samples_total.wrapping_add(n_frames as u64);
                a.pts_90khz = audio_pts_90k(a.pts_base, a.samples_total, a.sample_rate);

                // `ef.data` is a complete ADTS frame; the muxer wraps it in a PES.
                for ef in a.encoder.drain() {
                    let ts_packets = ts_mux.mux_audio_pre_adts(&ef.data, ef.pts);
                    publish_ts(ts_packets, ef.pts as u32, broadcast_tx, flow_stats);
                }
            }
        }
    }
}

#[cfg(all(test, feature = "media-codecs"))]
mod tests {
    use super::{audio_pts_90k, frame_pts_90k, unpack_uyvy422};
    use crate::engine::video_encode_util::resolve_chroma;

    /// ~4.6 h at 59.94 — long enough that a per-frame residue of half a tick is
    /// seconds of lip-sync, which is the whole point.
    const LONG_RUN: u64 = 1_000_000;

    /// Exact 90 kHz tick counts at frame `LONG_RUN`, computed by hand from
    /// `idx * 90_000 * den / num` (every case divides exactly at 1e6 frames).
    /// The frame rates come off the card as `(scale, duration)` — 59.94 arrives
    /// as `(60000, 1001)`, not `(60, 1)`.
    #[test]
    fn frame_pts_is_exact_over_a_long_run() {
        let cases: &[(u32, u32, i64)] = &[
            (24000, 1001, 3_753_750_000), // 23.976
            (24, 1, 3_750_000_000),
            (25, 1, 3_600_000_000),
            (30000, 1001, 3_003_000_000), // 29.97
            (30, 1, 3_000_000_000),
            (50, 1, 1_800_000_000),
            (60000, 1001, 1_501_500_000), // 59.94
            (60, 1, 1_500_000_000),
        ];
        for &(num, den, expect) in cases {
            assert_eq!(
                frame_pts_90k(0, LONG_RUN, num, den),
                expect,
                "{num}/{den} at frame {LONG_RUN}"
            );
        }
    }

    /// The regression this guards: `90_000 * den / num` is 3753.75 at 23.976 and
    /// 1501.5 at 59.94, and accumulating the truncated step loses the residue on
    /// every frame forever. 500 000 ticks over `LONG_RUN` frames at 59.94 is
    /// 5.6 s in 4.6 h — 28.8 s/day of lip-sync, since nothing re-anchors SDI
    /// video and the AAC encoder self-advances by exact sample count.
    #[test]
    fn accumulating_a_truncated_step_drifts_where_the_index_does_not() {
        // (num, den, ticks the accumulating step loses over LONG_RUN frames)
        let ntsc: &[(u32, u32, i64)] = &[(24000, 1001, 750_000), (60000, 1001, 500_000)];
        for &(num, den, lost) in ntsc {
            let step = (90_000u64 * den as u64 / num as u64) as i64;
            let accumulated = step * LONG_RUN as i64;
            let exact = frame_pts_90k(0, LONG_RUN, num, den);
            assert_eq!(exact - accumulated, lost, "{num}/{den}");
        }

        // The integer rates divide exactly, which is why this only ever bit the
        // NTSC family — and why it survived a bring-up done at 1080i50.
        for &(num, den) in &[(24u32, 1u32), (25, 1), (30, 1), (50, 1), (60, 1)] {
            let step = (90_000u64 * den as u64 / num as u64) as i64;
            assert_eq!(
                step * LONG_RUN as i64,
                frame_pts_90k(0, LONG_RUN, num, den),
                "{num}/{den} is exact either way"
            );
        }
    }

    /// Consecutive frames stay within a tick of the nominal period: the index
    /// derivation must not trade accumulated drift for per-frame jitter (the TS
    /// muxer builds PCR off these timestamps).
    #[test]
    fn frame_pts_spacing_never_wanders() {
        for &(num, den) in &[(24000u32, 1001u32), (30000, 1001), (60000, 1001), (25, 1)] {
            let nominal = 90_000i64 * den as i64 / num as i64;
            for i in 0..1_000u64 {
                let delta = frame_pts_90k(0, i + 1, num, den) - frame_pts_90k(0, i, num, den);
                assert!(
                    (delta - nominal).abs() <= 1,
                    "{num}/{den} frame {i}: delta {delta} vs nominal {nominal}"
                );
            }
        }
    }

    /// A device re-open re-bases the timeline on the PTS handed back, so the
    /// only error a session boundary can add is the one-off truncation of the
    /// frame it stopped on — never more than a tick, and it cannot compound
    /// inside the new session.
    #[test]
    fn re_open_rebase_costs_at_most_one_tick() {
        for stop in 0..64u64 {
            let base = frame_pts_90k(0, stop, 60000, 1001);
            for after in 0..64u64 {
                let split = frame_pts_90k(base, after, 60000, 1001);
                let continuous = frame_pts_90k(0, stop + after, 60000, 1001);
                assert!(
                    (split - continuous).abs() <= 1,
                    "stop={stop} after={after}: {split} vs {continuous}"
                );
            }
        }
        // Frame 0 of the new session lands exactly on the handed-back PTS.
        let base = frame_pts_90k(0, 900, 60000, 1001);
        assert_eq!(frame_pts_90k(base, 0, 60000, 1001), base);
    }

    /// At 48 kHz the per-sample factor is 15/8 — exact only on multiples of 8
    /// samples. DeckLink hands over 1920/frame at 25 fps (exact) but the SDK's
    /// block size must vary at the NTSC rates (48000/29.97 = 1601.6).
    #[test]
    fn audio_pts_is_exact_over_a_long_run() {
        let samples: u64 = (0..LONG_RUN)
            .map(|i| if i % 2 == 0 { 1601 } else { 1602 })
            .sum();
        assert_eq!(samples, 1_601_500_000);
        assert_eq!(audio_pts_90k(0, samples, 48_000), 3_002_812_500);
    }

    /// Within a session the drifted accumulator was inert (the encoder anchors
    /// once and self-advances), but it seeded the *next* session's anchor via
    /// `audio_pts_90khz` — so every re-open stepped audio backwards by the
    /// accumulated residue. ~9 s over `LONG_RUN` blocks at 29.97 (≈ 9.3 h).
    #[test]
    fn accumulating_a_truncated_audio_step_drifts() {
        let mut accumulated: u64 = 0;
        let mut samples: u64 = 0;
        for i in 0..LONG_RUN {
            let n: u64 = if i % 2 == 0 { 1601 } else { 1602 };
            accumulated = accumulated.wrapping_add(n * 90_000 / 48_000);
            samples += n;
        }
        assert_eq!(audio_pts_90k(0, samples, 48_000) - accumulated, 812_500);

        // 1920-sample blocks (25 fps) are a multiple of 8, so the old step was
        // exact there. Bring-up ran at 1080i50 and never saw this either.
        let pal: u64 = (0..LONG_RUN).map(|_| 1920 * 90_000 / 48_000).sum();
        assert_eq!(pal, audio_pts_90k(0, 1920 * LONG_RUN, 48_000));
    }

    /// The next session's AAC encoder anchors on the PTS handed back, so a
    /// session must start exactly on its base.
    #[test]
    fn audio_pts_carries_the_session_anchor() {
        assert_eq!(audio_pts_90k(12_345, 0, 48_000), 12_345);
        // 8 samples == 15 ticks exactly, from any base.
        assert_eq!(audio_pts_90k(12_345, 8, 48_000), 12_360);
    }

    /// A card that reports nothing must not divide by zero.
    #[test]
    fn degenerate_rates_do_not_panic() {
        assert_eq!(frame_pts_90k(0, 10, 0, 1), 900_000);
        assert_eq!(audio_pts_90k(0, 10, 0), 900_000);
    }

    /// Build a UYVY422 buffer with `stride` padding after each row, so the
    /// tests exercise the real capture case where stride > width * 2.
    fn uyvy(rows: &[[u8; 4]], stride: usize) -> Vec<u8> {
        let mut buf = vec![0xAA; rows.len() * stride]; // padding must be ignored
        for (r, row) in rows.iter().enumerate() {
            buf[r * stride..r * stride + 4].copy_from_slice(row);
        }
        buf
    }

    // Two 2x2 frames' worth of macropixels: [U, Y0, V, Y1] per two pixels.
    const ROW0: [u8; 4] = [10, 100, 20, 101];
    const ROW1: [u8; 4] = [30, 102, 40, 103];

    #[test]
    fn unpacks_422_preserving_chroma_per_row() {
        let data = uyvy(&[ROW0, ROW1], 8);
        let (mut y, mut cb, mut cr) = ([0u8; 4], [0u8; 2], [0u8; 2]);
        unpack_uyvy422(&data, 8, 2, 2, false, &mut y, &mut cb, &mut cr);

        assert_eq!(y, [100, 101, 102, 103], "luma is full-rate and in order");
        // 4:2:2 keeps one chroma sample per macropixel on *every* row.
        assert_eq!(cb, [10, 30]);
        assert_eq!(cr, [20, 40]);
    }

    #[test]
    fn unpacks_420_averaging_vertical_chroma_pairs() {
        let data = uyvy(&[ROW0, ROW1], 8);
        let (mut y, mut cb, mut cr) = ([0u8; 4], [0u8; 1], [0u8; 1]);
        unpack_uyvy422(&data, 8, 2, 2, true, &mut y, &mut cb, &mut cr);

        // Luma is identical to the 4:2:2 case — only chroma is subsampled.
        assert_eq!(y, [100, 101, 102, 103]);
        // Averaged over the vertical row pair, truncating: (10+30)>>1, (20+40)>>1.
        assert_eq!(cb, [20], "Cb averaged down the row pair");
        assert_eq!(cr, [30], "Cr averaged down the row pair");
    }

    /// Regression guard for the bug that produced ghosted chroma: taking the
    /// top row's chroma instead of averaging silently yields plausible-looking
    /// output, so assert the two layouts genuinely differ on the same input.
    #[test]
    fn four_two_zero_is_not_four_two_two_truncated() {
        let data = uyvy(&[ROW0, ROW1], 8);

        let (mut y2, mut cb2, mut cr2) = ([0u8; 4], [0u8; 2], [0u8; 2]);
        unpack_uyvy422(&data, 8, 2, 2, false, &mut y2, &mut cb2, &mut cr2);

        let (mut y0, mut cb0, mut cr0) = ([0u8; 4], [0u8; 1], [0u8; 1]);
        unpack_uyvy422(&data, 8, 2, 2, true, &mut y0, &mut cb0, &mut cr0);

        assert_ne!(cb0[0], cb2[0], "4:2:0 must average, not copy the top row");
    }

    #[test]
    fn ignores_stride_padding() {
        // Same pixels, wider stride. Output must be byte-identical.
        let tight = uyvy(&[ROW0, ROW1], 4);
        let padded = uyvy(&[ROW0, ROW1], 16);

        let (mut ya, mut cba, mut cra) = ([0u8; 4], [0u8; 2], [0u8; 2]);
        unpack_uyvy422(&tight, 4, 2, 2, false, &mut ya, &mut cba, &mut cra);
        let (mut yb, mut cbb, mut crb) = ([0u8; 4], [0u8; 2], [0u8; 2]);
        unpack_uyvy422(&padded, 16, 2, 2, false, &mut yb, &mut cbb, &mut crb);

        assert_eq!((ya, cba, cra), (yb, cbb, crb));
    }

    /// `sdi_input_blocking_loop` derives `chroma_420` from the encoder's
    /// resolved chroma, then unpacks straight into that layout. If the
    /// resolver ever grew a chroma the unpacker cannot emit, planes would be
    /// fed to the encoder in the wrong shape — the exact upstream bug this
    /// path was written to sidestep. Pin the mapping.
    #[test]
    fn chroma_resolution_agrees_with_the_unpacker() {
        use video_codec::VideoChroma;

        // Config validation rejects everything except these two for SDI, so
        // these are the only chromas `unpack_uyvy422` must ever service.
        assert_eq!(resolve_chroma(Some("yuv420p")), VideoChroma::Yuv420);
        assert_eq!(resolve_chroma(Some("yuv422p")), VideoChroma::Yuv422);
        // An unset chroma defaults to 4:2:0, which the unpacker supports.
        assert_eq!(resolve_chroma(None), VideoChroma::Yuv420);

        // And the layouts the unpacker branches on are exactly those two.
        for (chroma, expect_420) in [("yuv420p", true), ("yuv422p", false)] {
            let is_420 = matches!(resolve_chroma(Some(chroma)), VideoChroma::Yuv420);
            assert_eq!(
                is_420, expect_420,
                "chroma {chroma} maps to the right branch"
            );
        }
    }

    mod caption_presence {
        use super::super::caption_newly_detected;
        use crate::engine::st2110::captions::CaptionType;

        /// The first packet of a type is the only one that reports a
        /// detection — everything after is `None`, no matter how many more
        /// packets of the same type arrive in the session.
        #[test]
        fn fires_once_per_type_then_stays_silent() {
            let (mut cea608, mut cea708) = (false, false);
            assert_eq!(
                caption_newly_detected(CaptionType::Cea608, &mut cea608, &mut cea708),
                Some("CEA-608")
            );
            assert_eq!(
                caption_newly_detected(CaptionType::Cea608, &mut cea608, &mut cea708),
                None,
                "second CEA-608 packet in the same session must not re-fire"
            );
            assert_eq!(
                caption_newly_detected(CaptionType::Cea608, &mut cea608, &mut cea708),
                None,
                "third CEA-608 packet must not re-fire either"
            );
        }

        /// CEA-608 and CEA-708 are independent signals — one being present
        /// must not mask a later detection of the other.
        #[test]
        fn types_are_independent() {
            let (mut cea608, mut cea708) = (false, false);
            assert_eq!(
                caption_newly_detected(CaptionType::Cea608, &mut cea608, &mut cea708),
                Some("CEA-608")
            );
            // CEA-708 arriving afterward is still a fresh detection.
            assert_eq!(
                caption_newly_detected(CaptionType::Cea708, &mut cea608, &mut cea708),
                Some("CEA-708")
            );
            // Both now present; neither re-fires.
            assert_eq!(
                caption_newly_detected(CaptionType::Cea608, &mut cea608, &mut cea708),
                None
            );
            assert_eq!(
                caption_newly_detected(CaptionType::Cea708, &mut cea608, &mut cea708),
                None
            );
            assert!(cea608 && cea708, "both sticky flags end up set");
        }

        /// CEA-708-only source: CEA-608 never fires, CEA-708 fires exactly
        /// once. Guards against a shared/aliased state bug between the two
        /// independent flags.
        #[test]
        fn cea708_only_source_never_touches_cea608() {
            let (mut cea608, mut cea708) = (false, false);
            for _ in 0..5 {
                caption_newly_detected(CaptionType::Cea708, &mut cea608, &mut cea708);
            }
            assert!(!cea608, "CEA-608 was never seen, must stay false");
            assert!(cea708);
        }
    }
}
