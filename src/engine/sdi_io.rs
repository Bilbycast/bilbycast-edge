//! SDI capture I/O via Blackmagic DeckLink (mirrors `mxl_video_io.rs`).
//!
//! Input: captures packed 4:2:2 video (+ embedded PCM audio) from a DeckLink
//! card via `decklink-rs` (FFmpeg's `decklink` avdevice), unpacks UYVY422 to
//! planar YUV, encodes to H.264/HEVC, muxes into MPEG-TS, and publishes onto
//! the flow's broadcast channel. Mirrors the MXL / ST 2110-20 ingress encoder
//! path. Unlike MXL, a single device handle carries both video and audio, so
//! this task owns the whole A+V mux.
//!
//! Playout (SDI output) lands with `DecklinkPlayout` in a follow-up.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::models::SdiInputConfig;
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

#[cfg(feature = "media-codecs")]
use bytes::Bytes;
#[cfg(feature = "media-codecs")]
use decklink_rs::{CapturedFrame, DecklinkCapture, DecklinkCaptureConfig, DecklinkPixelFormat};

#[cfg(feature = "media-codecs")]
use super::audio_encode::{AudioCodec, AudioEncoder, EncoderParams};
#[cfg(feature = "media-codecs")]
use crate::stats::collector::OutputStatsAccumulator;

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
        event_sender.emit(
            EventSeverity::Critical,
            category::FLOW,
            format!("{ctx}: SDI input requires the media-codecs feature (error_code: sdi_no_media_codecs)"),
        );
        cancel.cancelled().await;
    }

    info!(target: "sdi.in", "{ctx}: capture loop exited");
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
    use crate::engine::video_encode_util::ScaledVideoEncoder;

    let pixel_format = match config.pixel_format.as_str() {
        "v210" => DecklinkPixelFormat::V210,
        _ => DecklinkPixelFormat::Uyvy422,
    };
    // v210 (10-bit) unpack is not wired yet — only UYVY422 8-bit is unpacked
    // below. Refuse v210 loudly rather than emit garbage planes.
    if matches!(pixel_format, DecklinkPixelFormat::V210) {
        event_sender.emit(
            EventSeverity::Critical,
            category::FLOW,
            format!("{ctx}: pixel_format=v210 not yet supported; use uyvy422 (error_code: sdi_pixfmt_unsupported)"),
        );
        return;
    }

    let cap_cfg = DecklinkCaptureConfig {
        device: config.device.clone(),
        format: config.format.clone(),
        pixel_format,
        audio_channels: config.audio_channels,
        audio_sample_rate: 48_000,
    };

    // Retry the device open until a signal locks or the flow is cancelled.
    let mut cap = loop {
        if cancel.is_cancelled() {
            return;
        }
        match DecklinkCapture::open(cap_cfg.clone()) {
            Ok(c) => break c,
            Err(e) => {
                debug!(target: "sdi.in", "{ctx}: open failed: {e}, retrying");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };

    let (width, height) = cap.video_dimensions();
    let (fr_num, fr_den) = cap.video_frame_rate();
    event_sender.emit(
        EventSeverity::Info,
        category::FLOW,
        format!("{ctx}: capture opened {width}x{height} @ {fr_num}/{fr_den} (error_code: sdi_capture_opened)"),
    );

    let enc = &config.video_encode;
    let enc_cfg = match crate::engine::st2110_video_io::build_encoder_config(
        enc, width, height, fr_num, fr_den,
    ) {
        Ok(c) => c,
        Err(e) => {
            // Log as well as emit: the event only reaches a connected manager,
            // so a standalone edge would otherwise fail silently here.
            tracing::error!(target: "sdi.in", "{ctx}: encoder config failed: {e}");
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!("{ctx}: encoder config failed: {e} (error_code: sdi_encode_config_failed)"),
            );
            return;
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

    // Unpack UYVY422 straight into the encoder's own input layout so the
    // planes pass through verbatim (no libswscale) AND a 4:2:0 encoder never
    // silently misreads 4:2:2 chroma planes.
    let target_chroma = crate::engine::video_encode_util::resolve_chroma(enc.chroma.as_deref());
    let bit_depth = enc.bit_depth.unwrap_or(8);
    let chroma_420 = match (target_chroma, bit_depth) {
        (video_codec::VideoChroma::Yuv420, 8) => true,
        (video_codec::VideoChroma::Yuv422, 8) => false,
        _ => {
            let msg = format!(
                "{ctx}: SDI capture is 8-bit 4:2:2 (uyvy422); video_encode chroma={target_chroma:?} \
                 bit_depth={bit_depth} is unsupported — use yuv420p or yuv422p at 8-bit \
                 (error_code: sdi_encode_chroma_unsupported)"
            );
            tracing::error!(target: "sdi.in", "{msg}");
            event_sender.emit(EventSeverity::Critical, category::FLOW, msg);
            return;
        }
    };
    let src_pix_fmt = video_engine::av_pix_fmt_for_yuv(target_chroma, bit_depth);

    let mut ts_mux = crate::engine::rtmp::ts_mux::TsMuxer::new();
    ts_mux.set_has_video(true);

    // ── embedded SDI audio ────────────────────────────────────────────────
    // One DeckLink handle carries video *and* embedded PCM, so this task also
    // encodes the audio and muxes it into the same TS. Audio failures are
    // non-fatal: we warn and continue video-only rather than kill the flow.
    let mut audio: Option<SdiAudio> = None;
    if config.audio_channels > 0 && cap.audio_channels() > 0 && cap.audio_sample_rate() > 0 {
        let acodec = config
            .audio_encode
            .as_ref()
            .and_then(|a| AudioCodec::parse(&a.codec))
            .unwrap_or(AudioCodec::AacLc);

        if !matches!(
            acodec,
            AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2
        ) {
            event_sender.emit(
                EventSeverity::Warning,
                category::FLOW,
                format!(
                    "{ctx}: audio_encode.codec '{}' is not supported on an SDI input \
                     (AAC family only) — continuing video-only \
                     (error_code: sdi_audio_codec_unsupported)",
                    acodec.as_str()
                ),
            );
        } else {
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
            // `AudioEncoder::spawn` wants an OutputStatsAccumulator; this input
            // has no output identity, so give it a synthetic one (same trick
            // `input_pcm_encode` uses).
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
                    // AAC in ADTS => stream_type 0x0F, no registration descriptor.
                    ts_mux.set_has_audio(true);
                    ts_mux.set_audio_stream(0x0F, None);
                    audio = Some(SdiAudio {
                        encoder,
                        planar: vec![Vec::new(); ach as usize],
                        pts_90khz: 0,
                        sample_rate: ahz,
                        channels: ach,
                    });
                    event_sender.emit(
                        EventSeverity::Info,
                        category::FLOW,
                        format!(
                            "{ctx}: embedded audio {ach}ch @ {ahz} Hz -> {} {bitrate} kbps \
                             (error_code: sdi_audio_started)",
                            acodec.as_str()
                        ),
                    );
                }
                Err(e) => {
                    tracing::error!(target: "sdi.in", "{ctx}: audio encoder init failed: {e}");
                    event_sender.emit(
                        EventSeverity::Warning,
                        category::FLOW,
                        format!(
                            "{ctx}: audio encoder init failed: {e} — continuing video-only \
                             (error_code: sdi_audio_encoder_init_failed)"
                        ),
                    );
                }
            }
        }
    }

    let pts_step = 90_000u64 * fr_den as u64 / fr_num.max(1) as u64;
    let mut pts: i64 = 0;

    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let ch = if chroma_420 { h / 2 } else { h };
    let mut y_plane = vec![0u8; w * h];
    let mut cb_plane = vec![0u8; cw * ch];
    let mut cr_plane = vec![0u8; cw * ch];

    event_sender.emit(
        EventSeverity::Info,
        category::FLOW,
        format!(
            "{ctx}: capture→encode pipeline started (codec={})",
            enc.codec
        ),
    );

    loop {
        if cancel.is_cancelled() {
            break;
        }
        match cap.read_frame() {
            Ok(CapturedFrame::Video(vf)) => {
                // Defensive: `unpack_uyvy422` slices rows directly, so a short or
                // unexpectedly-strided frame from the driver would panic and kill
                // this input task. Skip the frame instead — a dropped frame is
                // always preferable to a dead flow.
                let need = vf.stride.saturating_mul(height as usize);
                if vf.width != width
                    || vf.height != height
                    || vf.stride < (width as usize) * 2
                    || vf.data.len() < need
                {
                    debug!(
                        target: "sdi.in",
                        "{ctx}: skipping malformed frame ({}x{} stride={} len={}, expected {width}x{height} stride>={} len>={need})",
                        vf.width, vf.height, vf.stride, vf.data.len(), (width as usize) * 2,
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

                let fmt = match src_pix_fmt {
                    Some(f) => f,
                    None => {
                        debug!(target: "sdi.in", "{ctx}: no src pixel format for YUV422P8");
                        continue;
                    }
                };

                match pipeline.encode_raw_planes(
                    width,
                    height,
                    fmt,
                    &y_plane,
                    w,
                    &cb_plane,
                    cw,
                    &cr_plane,
                    cw,
                    Some(pts),
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
                            tracing::error!(target: "sdi.in", "{ctx}: encoder open failed: {e}");
                            event_sender.emit(
                                EventSeverity::Critical,
                                category::FLOW,
                                format!("{ctx}: encoder open failed: {e} (error_code: sdi_encode_failed)"),
                            );
                            break;
                        }
                        debug!(target: "sdi.in", "{ctx}: encode error: {e}");
                    }
                }

                pts += pts_step as i64;
            }
            Ok(CapturedFrame::Audio(af)) => {
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
                a.pts_90khz = a
                    .pts_90khz
                    .wrapping_add(n_frames as u64 * 90_000 / a.sample_rate.max(1) as u64);

                // `ef.data` is a complete ADTS frame; the muxer wraps it in a PES.
                for ef in a.encoder.drain() {
                    let ts_packets = ts_mux.mux_audio_pre_adts(&ef.data, ef.pts);
                    publish_ts(ts_packets, ef.pts as u32, broadcast_tx, flow_stats);
                }
            }
            Err(e) => {
                debug!(target: "sdi.in", "{ctx}: capture ended: {e}");
                break;
            }
        }
    }
}
