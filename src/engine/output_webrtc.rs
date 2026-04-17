// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! WebRTC output tasks: WHIP client (push to server) and WHEP server (serve viewers).
//!
//! Both modes extract H.264 NALUs from MPEG-TS broadcast channel packets,
//! packetize per RFC 6184, and send via str0m WebRTC sessions.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::WebrtcOutputConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;

#[cfg(feature = "webrtc")]
use super::audio_decode::{AacDecoder, DecodeStats, sample_rate_from_index};
#[cfg(feature = "webrtc")]
use super::audio_encode::{AudioCodec, AudioEncoder, AudioEncoderError, EncoderParams};
#[cfg(all(feature = "webrtc", feature = "video-thumbnail"))]
use super::ts_video_replace::VideoEncodeStats;
#[cfg(feature = "webrtc")]
use crate::config::models::VideoEncodeConfig;
use super::packet::RtpPacket;

/// Per-output encoder state for the WebRTC audio_encode bridge. Mirrors the
/// pattern in [`super::output_rtmp`] but tailored to Opus output: there is
/// no Transparent same-codec fast path because the source is always AAC and
/// WebRTC always emits Opus.
#[cfg(feature = "webrtc")]
enum WebrtcEncoderState {
    /// audio_encode unset, or video_only=true. Drop audio frames silently
    /// (preserves the existing pre-Phase B behavior).
    Disabled,
    /// audio_encode set; encoder will be built on first AAC frame.
    Lazy,
    /// Decoder + encoder running; each AAC frame goes through decode → encode
    /// → write_media to the WebRTC audio MID.
    Active {
        decoder: AacDecoder,
        encoder: AudioEncoder,
        decode_stats: Arc<DecodeStats>,
        /// Optional planar PCM shuffle / sample-rate stage between the
        /// decoder and the Opus encoder. `None` unless `config.transcode`
        /// was set. When `transcode.channels` is set it wins over the Opus
        /// encoder's channel count (the encoder reconfigures to match).
        transcoder: Option<super::audio_transcode::PlanarAudioTranscoder>,
    },
    /// Decoder or encoder construction failed once. Drop audio for the
    /// rest of the session's lifetime.
    Failed,
}

/// Per-WebRTC-session video transcoding state. Parallels
/// [`WebrtcEncoderState`] but for video: opens the decoder+encoder on
/// the first video access unit and transitions to `Active` for the
/// rest of the session. RTMP's `VideoEncoderState` is the closest
/// analogue, except RTMP builds an out-of-band FLV sequence header
/// (`global_header = true`) while WebRTC emits SPS/PPS inline on every
/// IDR (`global_header = false`) so H264Packetizer can feed them into
/// RTP as ordinary NAL units.
#[cfg(feature = "webrtc")]
enum WebrtcVideoEncoderState {
    /// `video_encode` unset. Passthrough H.264, drop HEVC (pre-Phase 4d).
    Disabled,
    /// `video_encode` set; decoder+encoder will be built on the first
    /// source access unit.
    #[cfg(feature = "video-thumbnail")]
    Lazy { cfg: VideoEncodeConfig },
    /// Decode → re-encode pipeline is live.
    #[cfg(feature = "video-thumbnail")]
    Active(Box<WebrtcVideoActive>),
    /// Decoder or encoder construction failed once. Drop video for the
    /// rest of the session's lifetime.
    Failed,
}

#[cfg(all(feature = "webrtc", feature = "video-thumbnail"))]
struct WebrtcVideoActive {
    decoder: video_engine::VideoDecoder,
    /// Opened on the first decoded frame so we can pick up the source
    /// resolution when the operator didn't specify one.
    encoder: Option<video_engine::VideoEncoder>,
    backend: video_codec::VideoEncoderCodec,
    requested_width: Option<u32>,
    requested_height: Option<u32>,
    fps_num: Option<u32>,
    fps_den: Option<u32>,
    bitrate_kbps: u32,
    gop_size: Option<u32>,
    preset: video_codec::VideoPreset,
    profile: video_codec::VideoProfile,
    /// Monotonic PTS counter in encoder time base. We pass the source
    /// 90 kHz PTS to `write_media` for correct lip-sync, but the encoder
    /// itself gets a monotonic `out_frame_count` so libx264 / NVENC rate
    /// control behaves predictably.
    out_frame_count: i64,
    stats: Arc<VideoEncodeStats>,
}

/// Resolve a `video_encode.codec` string into a [`VideoEncoderCodec`] for
/// WebRTC. Only H.264 backends are allowed; validation rejects HEVC at
/// config-load, but guard against it at runtime too.
#[cfg(all(feature = "webrtc", feature = "video-thumbnail"))]
fn resolve_webrtc_video_backend(
    codec: &str,
    output_id: &str,
    flow_id: &str,
    event_sender: &EventSender,
) -> Option<video_codec::VideoEncoderCodec> {
    match codec {
        "x264" => Some(video_codec::VideoEncoderCodec::X264),
        "h264_nvenc" => Some(video_codec::VideoEncoderCodec::H264Nvenc),
        other => {
            let msg = format!(
                "WebRTC output '{}': video_encode codec '{other}' not supported — WebRTC browsers only decode H.264",
                output_id
            );
            tracing::error!("{msg}");
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::VIDEO_ENCODE,
                msg,
                flow_id,
            );
            None
        }
    }
}

#[cfg(all(feature = "webrtc", feature = "video-thumbnail"))]
fn resolve_webrtc_video_preset(s: Option<&str>) -> video_codec::VideoPreset {
    match s {
        Some("ultrafast") => video_codec::VideoPreset::Ultrafast,
        Some("superfast") => video_codec::VideoPreset::Superfast,
        Some("veryfast") => video_codec::VideoPreset::Veryfast,
        Some("faster") => video_codec::VideoPreset::Faster,
        Some("fast") => video_codec::VideoPreset::Fast,
        Some("slow") => video_codec::VideoPreset::Slow,
        Some("slower") => video_codec::VideoPreset::Slower,
        Some("veryslow") => video_codec::VideoPreset::Veryslow,
        _ => video_codec::VideoPreset::Medium,
    }
}

#[cfg(all(feature = "webrtc", feature = "video-thumbnail"))]
fn resolve_webrtc_video_profile(s: Option<&str>) -> video_codec::VideoProfile {
    match s {
        Some("baseline") => video_codec::VideoProfile::Baseline,
        Some("main") => video_codec::VideoProfile::Main,
        Some("high") => video_codec::VideoProfile::High,
        _ => video_codec::VideoProfile::Auto,
    }
}

/// Initialise the per-session video encoder state. Called once per WHEP
/// viewer loop / WHIP client loop at startup. Returns `Disabled` when
/// `video_encode` is unset so the hot path can passthrough H.264 without
/// any extra branches.
#[cfg(feature = "webrtc")]
fn init_webrtc_video_encoder_state(
    video_encode: Option<&VideoEncodeConfig>,
) -> WebrtcVideoEncoderState {
    match video_encode {
        None => WebrtcVideoEncoderState::Disabled,
        #[cfg(feature = "video-thumbnail")]
        Some(cfg) => WebrtcVideoEncoderState::Lazy { cfg: cfg.clone() },
        #[cfg(not(feature = "video-thumbnail"))]
        Some(_) => WebrtcVideoEncoderState::Failed,
    }
}

/// Open the decoder+stats for a WebRTC video_encode pipeline and flip
/// the state into `Active`. Mirrors RTMP's `open_video_active` but
/// specialised to WebRTC's H.264-only output constraint.
#[cfg(all(feature = "webrtc", feature = "video-thumbnail"))]
fn open_webrtc_video_active(
    cfg: &VideoEncodeConfig,
    source_is_h264: bool,
    output_id: &str,
    flow_id: &str,
    output_stats: &Arc<OutputStatsAccumulator>,
    event_sender: &EventSender,
) -> WebrtcVideoEncoderState {
    let Some(backend) = resolve_webrtc_video_backend(&cfg.codec, output_id, flow_id, event_sender)
    else {
        return WebrtcVideoEncoderState::Failed;
    };
    let source_codec = if source_is_h264 {
        video_codec::VideoCodec::H264
    } else {
        video_codec::VideoCodec::Hevc
    };
    let decoder = match video_engine::VideoDecoder::open(source_codec) {
        Ok(d) => d,
        Err(e) => {
            let msg = format!(
                "WebRTC output '{}': video_encode failed to open decoder for {:?}: {e}",
                output_id, source_codec
            );
            tracing::error!("{msg}");
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::VIDEO_ENCODE,
                msg,
                flow_id,
            );
            return WebrtcVideoEncoderState::Failed;
        }
    };
    let stats_handle = Arc::new(VideoEncodeStats::default());
    let backend_tag = match backend {
        video_codec::VideoEncoderCodec::X264 => "x264",
        video_codec::VideoEncoderCodec::H264Nvenc => "nvenc",
        _ => "unknown",
    };
    output_stats.set_video_encode_stats(
        stats_handle.clone(),
        String::new(),
        "h264".to_string(),
        cfg.width.unwrap_or(0),
        cfg.height.unwrap_or(0),
        match (cfg.fps_num, cfg.fps_den) {
            (Some(n), Some(d)) if d > 0 => n as f32 / d as f32,
            _ => 0.0,
        },
        cfg.bitrate_kbps.unwrap_or(4000),
        backend_tag.to_string(),
    );
    tracing::info!(
        "WebRTC output '{}': video_encode active ({} @ {} kbps, source {:?})",
        output_id,
        backend_tag,
        cfg.bitrate_kbps.unwrap_or(4000),
        source_codec,
    );
    event_sender.emit_flow(
        EventSeverity::Info,
        category::VIDEO_ENCODE,
        format!("Video encoder started: output '{}'", output_id),
        flow_id,
    );
    WebrtcVideoEncoderState::Active(Box::new(WebrtcVideoActive {
        decoder,
        encoder: None,
        backend,
        requested_width: cfg.width,
        requested_height: cfg.height,
        fps_num: cfg.fps_num,
        fps_den: cfg.fps_den,
        bitrate_kbps: cfg.bitrate_kbps.unwrap_or(4000),
        gop_size: cfg.gop_size,
        preset: resolve_webrtc_video_preset(cfg.preset.as_deref()),
        profile: resolve_webrtc_video_profile(cfg.profile.as_deref()),
        out_frame_count: 0,
        stats: stats_handle,
    }))
}

/// Concatenate source NAL units (no start codes) back into an Annex-B
/// byte stream suitable for `VideoDecoder::send_packet`.
#[cfg(all(feature = "webrtc", feature = "video-thumbnail"))]
fn nalus_to_annex_b_webrtc(nalus: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = nalus.iter().map(|n| 4 + n.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nalu in nalus {
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(nalu);
    }
    out
}

/// Push one source access unit through the decoder + encoder and return
/// the encoder's Annex-B output (with any emitted SPS/PPS inline on
/// IDRs). Flips `video_state` to `Failed` on a terminal error; returns
/// an empty vec while the encoder opens (same convention as RTMP).
#[cfg(all(feature = "webrtc", feature = "video-thumbnail"))]
fn encode_one_video_frame_webrtc(
    video_state: &mut WebrtcVideoEncoderState,
    nalus: &[Vec<u8>],
    output_id: &str,
) -> Vec<Vec<u8>> {
    let active = match video_state {
        WebrtcVideoEncoderState::Active(a) => a,
        _ => return Vec::new(),
    };
    let annex_b = nalus_to_annex_b_webrtc(nalus);
    let block_result: Result<Vec<Vec<u8>>, String> = tokio::task::block_in_place(|| -> Result<Vec<Vec<u8>>, String> {
        active.stats.input_frames.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = active.decoder.send_packet(&annex_b) {
            tracing::debug!("WebRTC output '{}': decoder send_packet: {e:?}", output_id);
        }
        let mut out = Vec::new();
        loop {
            let frame = match active.decoder.receive_frame() {
                Ok(f) => f,
                Err(_) => break,
            };
            if active.encoder.is_none() {
                let src_w = frame.width();
                let src_h = frame.height();
                if let (Some(rw), Some(rh)) = (active.requested_width, active.requested_height) {
                    if rw != src_w || rh != src_h {
                        tracing::warn!(
                            "WebRTC output '{}': video_encode resolution change {}x{} -> {}x{} requested \
                             but scaling is not implemented yet; using source resolution",
                            output_id, src_w, src_h, rw, rh
                        );
                    }
                }
                let (fps_num, fps_den) = match (active.fps_num, active.fps_den) {
                    (Some(n), Some(d)) => (n, d),
                    _ => (30, 1),
                };
                let gop_size = active.gop_size.unwrap_or(fps_num.max(1) * 2);
                let enc_cfg = video_codec::VideoEncoderConfig {
                    codec: active.backend,
                    width: src_w,
                    height: src_h,
                    fps_num,
                    fps_den,
                    bitrate_kbps: active.bitrate_kbps,
                    gop_size,
                    preset: active.preset,
                    profile: active.profile,
                    // WebRTC has no out-of-band codec-config channel;
                    // emit SPS/PPS in-band on every IDR so late-joining
                    // browsers can decode as soon as they see a keyframe.
                    global_header: false,
                };
                let enc = match video_engine::VideoEncoder::open(&enc_cfg) {
                    Ok(e) => e,
                    Err(e) => return Err(format!("encoder open failed: {e}")),
                };
                active.encoder = Some(enc);
            }
            let (y, y_s, u, u_s, v, v_s) = match frame.yuv_planes() {
                Some(p) => p,
                None => continue,
            };
            let enc = active.encoder.as_mut().unwrap();
            let encoded_frames = match enc.encode_frame(
                y, y_s, u, u_s, v, v_s,
                Some(active.out_frame_count),
            ) {
                Ok(frames) => frames,
                Err(e) => {
                    tracing::debug!("WebRTC output '{}': encode error: {e}", output_id);
                    active.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            active.out_frame_count += 1;
            for ef in encoded_frames {
                out.push(ef.data);
                active.stats.output_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(out)
    });

    // Only encoder *open* failure flips us to Failed. Decoder priming
    // (no frames produced yet on the first few input access units)
    // returns Ok(vec![]) — keep state Active and try again next frame.
    match block_result {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("WebRTC output '{}': video_encode: {e}", output_id);
            *video_state = WebrtcVideoEncoderState::Failed;
            Vec::new()
        }
    }
}

/// Handle one demuxed video access unit: passthrough H.264, encode HEVC
/// (or H.264 if `video_encode` is set), then RFC 6184 packetize and hand
/// to str0m. Shared by the WHIP client loop and the WHEP per-viewer loop.
///
/// `source_is_h264` distinguishes the source codec; HEVC sources without
/// `video_encode` are dropped here (pre-Phase 4d behaviour). With
/// `video_encode`, both source codecs are decoded and re-encoded as
/// H.264 so every WebRTC browser can decode the output.
#[cfg(feature = "webrtc")]
#[allow(clippy::too_many_arguments)]
async fn handle_webrtc_video_frame(
    source_is_h264: bool,
    nalus: &[Vec<u8>],
    pts: u64,
    recv_time_us: u64,
    video_state: &mut WebrtcVideoEncoderState,
    session: &mut super::webrtc::session::WebrtcSession,
    video_mid: str0m::media::Mid,
    video_pt: str0m::media::Pt,
    stats: &Arc<OutputStatsAccumulator>,
    output_id: &str,
    #[cfg_attr(not(feature = "video-thumbnail"), allow(unused_variables))]
    flow_id: &str,
    #[cfg_attr(not(feature = "video-thumbnail"), allow(unused_variables))]
    events: &EventSender,
) {
    use super::webrtc::rtp_h264::H264Packetizer;
    use str0m::media::{Frequency, MediaTime};
    use std::time::Instant;

    // Disabled + HEVC source → drop (pre-Phase 4d behaviour).
    if matches!(video_state, WebrtcVideoEncoderState::Disabled) && !source_is_h264 {
        return;
    }
    // Failed → drop video for the rest of the session.
    if matches!(video_state, WebrtcVideoEncoderState::Failed) {
        return;
    }

    // Lazy-open the decoder + encoder scaffolding on the first video
    // access unit. Falls through to the encode path on the same frame.
    #[cfg(feature = "video-thumbnail")]
    if matches!(video_state, WebrtcVideoEncoderState::Lazy { .. }) {
        let cfg = match video_state {
            WebrtcVideoEncoderState::Lazy { cfg } => cfg.clone(),
            _ => unreachable!(),
        };
        *video_state = open_webrtc_video_active(
            &cfg, source_is_h264, output_id, flow_id, stats, events,
        );
        if matches!(video_state, WebrtcVideoEncoderState::Failed) {
            return;
        }
    }

    // Decide the final NALU stream to packetize — either the source
    // NALUs (passthrough) or the encoder's Annex-B output split back
    // into NAL units.
    //
    // `owned` keeps the encoder-produced NALUs alive when we go down the
    // encode path; `send_nalus` borrows either from `nalus` (passthrough)
    // or from `owned`.
    let owned: Vec<Vec<u8>>;
    let send_nalus: &[Vec<u8>];

    #[cfg(feature = "video-thumbnail")]
    {
        if matches!(video_state, WebrtcVideoEncoderState::Active(_)) {
            let encoded = encode_one_video_frame_webrtc(video_state, nalus, output_id);
            if matches!(video_state, WebrtcVideoEncoderState::Failed) {
                return;
            }
            if encoded.is_empty() {
                // Encoder still priming — no output yet.
                return;
            }
            let mut collected: Vec<Vec<u8>> = Vec::new();
            for annex_b in &encoded {
                let mut parts = super::ts_demux::split_annex_b_nalus(annex_b);
                collected.append(&mut parts);
            }
            if collected.is_empty() {
                return;
            }
            owned = collected;
            send_nalus = &owned;
        } else {
            send_nalus = nalus;
            owned = Vec::new();
            let _ = &owned;
        }
    }

    #[cfg(not(feature = "video-thumbnail"))]
    {
        send_nalus = nalus;
        owned = Vec::new();
        let _ = &owned;
    }

    let nalu_count = send_nalus.len();
    for (i, nalu) in send_nalus.iter().enumerate() {
        let is_last = i == nalu_count - 1;
        let rtp_payloads = H264Packetizer::packetize(nalu, is_last);
        for rtp_payload in &rtp_payloads {
            let media_time = MediaTime::new(pts, Frequency::NINETY_KHZ);
            if let Err(e) = session.write_media(
                video_mid,
                video_pt,
                Instant::now(),
                media_time,
                &rtp_payload.data,
            ) {
                tracing::debug!("WebRTC output '{}' write error: {}", output_id, e);
            }
            // str0m requires poll_output between consecutive writes —
            // drain or the next write_media is silently rejected.
            session.drain_outputs().await;
            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
            stats.bytes_sent.fetch_add(rtp_payload.data.len() as u64, Ordering::Relaxed);
            stats.record_latency(recv_time_us);
        }
    }
}

/// Spawn a WebRTC output task (WHIP client or WHEP server depending on config mode).
///
/// For WHEP server mode, `session_rx` must be provided — it receives SDP offers
/// from the HTTP handler (via `WebrtcSessionRegistry`) and spawns per-viewer
/// send tasks. The corresponding sender is stored in `FlowRuntime::whep_session_tx`
/// and registered with the session registry after flow creation.
pub fn spawn_webrtc_output(
    config: WebrtcOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    #[cfg(feature = "webrtc")]
    session_rx: Option<tokio::sync::mpsc::Receiver<crate::api::webrtc::registry::NewSessionMsg>>,
    event_sender: EventSender,
    flow_id: String,
    compressed_audio_input: bool,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();

    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("webrtc".to_string()),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
    });

    #[cfg(feature = "webrtc")]
    {
        use crate::config::models::WebrtcOutputMode;
        match config.mode {
            WebrtcOutputMode::WhipClient => {
                tokio::spawn(async move {
                    whip_client_loop(config, rx, output_stats, cancel, &event_sender, &flow_id, compressed_audio_input).await;
                })
            }
            WebrtcOutputMode::WhepServer => {
                let broadcast_tx_clone = broadcast_tx.clone();
                tokio::spawn(async move {
                    tracing::info!(
                        "WebRTC/WHEP server output '{}' started, waiting for viewers at /api/v1/flows/.../whep",
                        config.id,
                    );
                    if let Some(session_rx) = session_rx {
                        whep_server_loop(config, broadcast_tx_clone, rx, output_stats, cancel, session_rx, &event_sender, &flow_id, compressed_audio_input).await;
                    } else {
                        tracing::warn!(
                            "WHEP server output '{}' has no session channel — viewers cannot connect",
                            config.id,
                        );
                        webrtc_stub_loop(&config, rx, output_stats, cancel).await;
                    }
                })
            }
        }
    }

    #[cfg(not(feature = "webrtc"))]
    {
        let _ = (event_sender, flow_id, compressed_audio_input); // Suppress unused warnings
        tokio::spawn(async move {
            tracing::warn!(
                "WebRTC output '{}' is a stub: the `webrtc` cargo feature is not enabled. \
                 Packets will be consumed but not transmitted.",
                config.id,
            );
            webrtc_stub_loop(&config, rx, output_stats, cancel).await;
        })
    }
}

/// Stub receive loop — consumes packets without transmitting.
/// Used when webrtc feature is disabled, or for WHEP server mode
/// (where actual sending happens in per-viewer session tasks).
async fn webrtc_stub_loop(
    config: &WebrtcOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("WebRTC output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(_packet) => {
                        // Consume silently
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("WebRTC output '{}' broadcast channel closed", config.id);
                        break;
                    }
                }
            }
        }
    }
}

/// WHEP server loop — listens for viewer session requests from the HTTP handler
/// and spawns per-viewer send tasks that subscribe to the broadcast channel.
#[cfg(feature = "webrtc")]
async fn whep_server_loop(
    config: WebrtcOutputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    mut session_rx: tokio::sync::mpsc::Receiver<crate::api::webrtc::registry::NewSessionMsg>,
    events: &EventSender,
    flow_id: &str,
    compressed_audio_input: bool,
) {
    use super::webrtc::session::{SessionConfig, WebrtcSession};

    let public_ip: Option<std::net::IpAddr> = config.public_ip.as_ref().and_then(|ip| ip.parse().ok());
    let bind_addr: std::net::SocketAddr = match public_ip {
        Some(ip) => std::net::SocketAddr::new(ip, 0),
        None => "0.0.0.0:0".parse().unwrap(),
    };
    // WHEP output is the server side — ICE-Lite.
    let session_config = SessionConfig { bind_addr, public_ip, ice_lite: true };

    loop {
        // Wait for a viewer to connect via WHEP
        let msg = tokio::select! {
            _ = cancel.cancelled() => break,
            msg = session_rx.recv() => match msg {
                Some(m) => m,
                None => break, // Channel closed
            },
            // Keep consuming broadcast packets while waiting so we don't lag
            result = rx.recv() => {
                match result {
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };

        tracing::info!("WHEP viewer connecting to output '{}'", config.id);

        // Create WebRTC session for this viewer
        let mut session = match WebrtcSession::new(&session_config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("WHEP output '{}': failed to create session: {}", config.id, e);
                events.emit_flow(EventSeverity::Warning, category::WEBRTC, format!("WebRTC session creation failed: {e}"), flow_id);
                let _ = msg.reply.send(Err(e));
                continue;
            }
        };

        // Accept the viewer's SDP offer (recvonly from viewer's perspective)
        let answer = match session.accept_offer(&msg.offer_sdp) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("WHEP output '{}': failed to accept SDP offer: {}", config.id, e);
                let _ = msg.reply.send(Err(e));
                continue;
            }
        };

        let session_id = uuid::Uuid::new_v4().to_string();
        let _ = msg.reply.send(Ok((answer, session_id.clone())));

        // Spawn a per-viewer send task
        let viewer_rx = broadcast_tx.subscribe();
        let viewer_cancel = cancel.child_token();
        let viewer_stats = stats.clone();
        let output_id = config.id.clone();
        let video_only = config.video_only;
        let viewer_program = config.program_number;
        let viewer_events = events.clone();
        let viewer_flow_id = flow_id.to_string();
        let viewer_audio_encode = config.audio_encode.clone();
        let viewer_transcode = config.transcode.clone();
        let viewer_compressed = compressed_audio_input;
        let viewer_video_encode = config.video_encode.clone();

        tokio::spawn(async move {
            whep_viewer_loop(
                &output_id,
                &session_id,
                session,
                viewer_rx,
                viewer_stats,
                viewer_cancel,
                video_only,
                viewer_program,
                &viewer_events,
                &viewer_flow_id,
                viewer_audio_encode,
                viewer_transcode,
                viewer_compressed,
                viewer_video_encode,
            ).await;
        });
    }

    tracing::info!("WHEP server output '{}' stopped", config.id);
}

/// Per-viewer send loop: demux TS → packetize H.264 → send via WebRTC to one viewer.
#[cfg(feature = "webrtc")]
async fn whep_viewer_loop(
    output_id: &str,
    session_id: &str,
    mut session: super::webrtc::session::WebrtcSession,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    video_only: bool,
    program_number: Option<u16>,
    events: &EventSender,
    flow_id: &str,
    audio_encode: Option<crate::config::models::AudioEncodeConfig>,
    transcode: Option<super::audio_transcode::TranscodeJson>,
    compressed_audio_input: bool,
    video_encode: Option<VideoEncodeConfig>,
) {
    use super::ts_parse::strip_rtp_header;
    use super::webrtc::ts_demux::TsDemuxer;
    use super::webrtc::session::SessionEvent;
    use str0m::media::MediaTime;
    use std::time::Instant;

    // Wait for ICE+DTLS to complete
    loop {
        let event = session.poll_event(&cancel).await;
        match event {
            SessionEvent::Connected => {
                tracing::info!("WHEP viewer '{}' connected on output '{}'", session_id, output_id);
                events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHEP viewer connected", flow_id);
                break;
            }
            SessionEvent::Disconnected => {
                tracing::info!("WHEP viewer '{}' disconnected during setup", session_id);
                events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHEP viewer disconnected", flow_id);
                return;
            }
            _ => continue,
        }
    }

    // str0m may emit MediaAdded *after* Connected. Flush any pending
    // events so video_mid / audio_mid are populated before we read them.
    session.drain_pending_events();

    // Get the video MID and PT
    let video_mid = match session.video_mid {
        Some(mid) => mid,
        None => {
            tracing::error!("WHEP viewer '{}': no video MID negotiated", session_id);
            return;
        }
    };
    let video_pt = match session.get_pt(video_mid) {
        Some(pt) => pt,
        None => {
            tracing::error!("WHEP viewer '{}': no video PT negotiated", session_id);
            return;
        }
    };

    // Extract the audio MID + payload type if SDP negotiated audio.
    // video_only=true skips this entirely (no audio MID was negotiated).
    let (audio_mid, audio_pt) = if !video_only {
        let mid = session.audio_mid;
        let pt = mid.and_then(|m| session.get_pt(m));
        (mid, pt)
    } else {
        (None, None)
    };

    // Build encoder state lazily on first AAC frame. The Lazy state only
    // makes sense when audio_encode is set AND audio MID was negotiated.
    let mut encoder_state: WebrtcEncoderState =
        if audio_encode.is_some() && audio_mid.is_some() && audio_pt.is_some() {
            WebrtcEncoderState::Lazy
        } else {
            WebrtcEncoderState::Disabled
        };
    let mut video_encoder_state: WebrtcVideoEncoderState =
        init_webrtc_video_encoder_state(video_encode.as_ref());

    // Send loop: demux TS → packetize H.264 → send via str0m.
    // Also processes incoming RTCP/STUN via drive_udp_io() to keep
    // the session alive (same pattern as whip_client_loop).
    let mut demuxer = TsDemuxer::new(program_number);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,

            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        let recv_time_us = packet.recv_time_us;
                        let payload = strip_rtp_header(&packet);
                        if payload.is_empty() { continue; }

                        let frames = demuxer.demux(payload);
                        for frame in frames {
                            match frame {
                                super::webrtc::ts_demux::DemuxedFrame::H264 { nalus, pts, .. } => {
                                    handle_webrtc_video_frame(
                                        true,
                                        &nalus,
                                        pts,
                                        recv_time_us,
                                        &mut video_encoder_state,
                                        &mut session,
                                        video_mid,
                                        video_pt,
                                        &stats,
                                        output_id,
                                        flow_id,
                                        events,
                                    ).await;
                                }
                                super::webrtc::ts_demux::DemuxedFrame::H265 { nalus, pts, .. } => {
                                    handle_webrtc_video_frame(
                                        false,
                                        &nalus,
                                        pts,
                                        recv_time_us,
                                        &mut video_encoder_state,
                                        &mut session,
                                        video_mid,
                                        video_pt,
                                        &stats,
                                        output_id,
                                        flow_id,
                                        events,
                                    ).await;
                                }
                                super::webrtc::ts_demux::DemuxedFrame::Opus => {
                                    // TODO: handle native Opus passthrough
                                    // (rare — would require an OBS source
                                    // already publishing Opus-in-TS).
                                }
                                super::webrtc::ts_demux::DemuxedFrame::Aac { data, pts } => {
                                    if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                        encoder_state = build_webrtc_encoder_state(
                                            audio_encode.as_ref(),
                                            transcode.as_ref(),
                                            &demuxer,
                                            compressed_audio_input,
                                            &cancel,
                                            &stats,
                                            flow_id,
                                            output_id,
                                            events,
                                        );
                                    }
                                    if let (
                                        WebrtcEncoderState::Active { decoder, encoder, decode_stats, transcoder },
                                        Some(audio_mid),
                                        Some(audio_pt),
                                    ) = (&mut encoder_state, audio_mid, audio_pt)
                                    {
                                        decode_stats.inc_input();
                                        match decoder.decode_frame(&data) {
                                            Ok(planar) => {
                                                decode_stats.inc_output();
                                                if let Some(tc) = transcoder.as_mut() {
                                                    match tc.process(&planar) {
                                                        Ok(shuffled) => {
                                                            encoder.submit_planar(&shuffled, pts);
                                                        }
                                                        Err(e) => {
                                                            tracing::debug!(
                                                                "WHEP viewer '{}' transcode failed: {}",
                                                                session_id, e
                                                            );
                                                        }
                                                    }
                                                } else {
                                                    encoder.submit_planar(&planar, pts);
                                                }
                                            }
                                            Err(_) => {
                                                decode_stats.inc_error();
                                            }
                                        }
                                        for frame in encoder.drain() {
                                            // Convert 90k PTS → 48k for Opus
                                            let media_time = MediaTime::new(
                                                frame.pts * 48_000 / 90_000,
                                                str0m::media::Frequency::FORTY_EIGHT_KHZ,
                                            );
                                            if let Err(e) = session.write_media(
                                                audio_mid,
                                                audio_pt,
                                                Instant::now(),
                                                media_time,
                                                &frame.data,
                                            ) {
                                                tracing::debug!(
                                                    "WHEP viewer '{}' audio write error: {}",
                                                    session_id, e
                                                );
                                            }
                                            session.drain_outputs().await;
                                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                            stats.bytes_sent.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                                            stats.record_latency(recv_time_us);
                                        }
                                    }
                                }
                            }
                        }

                        // Drive str0m: process incoming RTCP/STUN + send queued output.
                        if let Some(ev) = session.drive_udp_io().await {
                            match ev {
                                SessionEvent::Disconnected => {
                                    tracing::info!("WHEP viewer '{}' disconnected during send", session_id);
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    tracing::info!("WHEP viewer '{}' disconnected from output '{}'", session_id, output_id);
    events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHEP viewer disconnected", flow_id);
}

/// WHIP client output loop — pushes media to an external WHIP endpoint.
#[cfg(feature = "webrtc")]
async fn whip_client_loop(
    config: WebrtcOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    compressed_audio_input: bool,
) {
    use std::time::Instant;
    use super::ts_parse::strip_rtp_header;
    use super::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
    use super::webrtc::ts_demux::TsDemuxer;
    use str0m::media::MediaTime;

    let whip_url = match &config.whip_url {
        Some(url) => url.clone(),
        None => {
            tracing::error!("WebRTC output '{}': no whip_url configured", config.id);
            return;
        }
    };

    let public_ip: Option<std::net::IpAddr> = config.public_ip.as_ref().and_then(|ip| ip.parse().ok());
    // When public_ip is pinned we also bind the UDP socket to that address,
    // so the destination IP on every incoming packet matches the local ICE
    // candidate. Without this, str0m's ICE state machine discards STUN
    // binding requests as "unknown interface" and the connection silently
    // fails to complete. Same fix as input_webrtc.
    let bind_addr: std::net::SocketAddr = match public_ip {
        Some(ip) => std::net::SocketAddr::new(ip, 0),
        None => "0.0.0.0:0".parse().unwrap(),
    };
    // WHIP output is the client side — full ICE (not ICE-Lite).
    let session_config = SessionConfig { bind_addr, public_ip, ice_lite: false };
    let mut backoff_secs = 1u64;

    'outer: loop {
        // Create session
        let mut session = match WebrtcSession::new(&session_config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("WHIP client '{}': session error: {}", config.id, e);
                events.emit_flow(EventSeverity::Warning, category::WEBRTC, format!("WebRTC session creation failed: {e}"), flow_id);
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        // Create SDP offer (sendonly video + optional audio)
        let (offer_sdp, pending) = match session.create_offer(true, !config.video_only, true) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("WHIP client '{}': SDP offer error: {}", config.id, e);
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        // POST to WHIP endpoint
        let (answer_sdp, _resource_url) = match super::webrtc::signaling::whip_post(
            &whip_url,
            &offer_sdp,
            config.bearer_token.as_deref(),
        ).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("WHIP signaling '{}' failed: {}", config.id, e);
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        if let Err(e) = session.apply_answer(&answer_sdp, pending) {
            tracing::error!("WHIP client '{}': SDP answer error: {}", config.id, e);
            continue;
        }

        backoff_secs = 1;
        tracing::info!("WHIP client '{}' signaling complete, waiting for ICE/DTLS", config.id);

        // Wait for ICE+DTLS to complete
        let child_cancel = cancel.child_token();

        // Wait for connected event before sending media
        loop {
            let event = session.poll_event(&child_cancel).await;
            match event {
                SessionEvent::Connected => {
                    tracing::info!("WHIP client '{}' session established", config.id);
                    break;
                }
                SessionEvent::Disconnected => {
                    tracing::warn!("WHIP client '{}' disconnected during setup", config.id);
                    continue 'outer;
                }
                _ => continue,
            }
        }

        // str0m may emit MediaAdded *after* Connected. Flush any pending
        // events so video_mid / audio_mid are populated before we read them.
        session.drain_pending_events();

        // Get the video PT
        let video_mid = match session.video_mid {
            Some(mid) => mid,
            None => {
                tracing::error!("WHIP client '{}': no video MID", config.id);
                continue;
            }
        };
        let video_pt = match session.get_pt(video_mid) {
            Some(pt) => pt,
            None => {
                tracing::error!("WHIP client '{}': no video PT negotiated", config.id);
                continue;
            }
        };

        // Optional audio MID + PT (only present when video_only=false).
        let (audio_mid, audio_pt) = if !config.video_only {
            let mid = session.audio_mid;
            let pt = mid.and_then(|m| session.get_pt(m));
            (mid, pt)
        } else {
            (None, None)
        };

        let audio_encode = config.audio_encode.clone();
        let transcode = config.transcode.clone();
        let mut encoder_state: WebrtcEncoderState =
            if audio_encode.is_some() && audio_mid.is_some() && audio_pt.is_some() {
                WebrtcEncoderState::Lazy
            } else {
                WebrtcEncoderState::Disabled
            };
        let mut video_encoder_state: WebrtcVideoEncoderState =
            init_webrtc_video_encoder_state(config.video_encode.as_ref());

        // Send loop: demux TS → packetize H.264 → send via str0m.
        //
        // We must also process incoming UDP (RTCP receiver reports, STUN
        // keepalives) so str0m can maintain the connection. Without this
        // the remote peer never receives RTCP feedback, timers expire,
        // and the session silently dies.
        let mut demuxer = TsDemuxer::new(config.program_number);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break 'outer,

                result = rx.recv() => {
                    match result {
                        Ok(packet) => {
                            let recv_time_us = packet.recv_time_us;
                            let payload = strip_rtp_header(&packet);
                            if payload.is_empty() { continue; }

                            let frames = demuxer.demux(payload);
                            for frame in frames {
                                match frame {
                                    super::webrtc::ts_demux::DemuxedFrame::H264 { nalus, pts, .. } => {
                                        handle_webrtc_video_frame(
                                            true,
                                            &nalus,
                                            pts,
                                            recv_time_us,
                                            &mut video_encoder_state,
                                            &mut session,
                                            video_mid,
                                            video_pt,
                                            &stats,
                                            &config.id,
                                            flow_id,
                                            events,
                                        ).await;
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::H265 { nalus, pts, .. } => {
                                        handle_webrtc_video_frame(
                                            false,
                                            &nalus,
                                            pts,
                                            recv_time_us,
                                            &mut video_encoder_state,
                                            &mut session,
                                            video_mid,
                                            video_pt,
                                            &stats,
                                            &config.id,
                                            flow_id,
                                            events,
                                        ).await;
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::Opus => {
                                        // Native Opus passthrough not yet implemented
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::Aac { data, pts } => {
                                        if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                            encoder_state = build_webrtc_encoder_state(
                                                audio_encode.as_ref(),
                                                transcode.as_ref(),
                                                &demuxer,
                                                compressed_audio_input,
                                                &cancel,
                                                &stats,
                                                flow_id,
                                                &config.id,
                                                events,
                                            );
                                        }
                                        if let (
                                            WebrtcEncoderState::Active { decoder, encoder, decode_stats, transcoder },
                                            Some(audio_mid),
                                            Some(audio_pt),
                                        ) = (&mut encoder_state, audio_mid, audio_pt)
                                        {
                                            decode_stats.inc_input();
                                            match decoder.decode_frame(&data) {
                                                Ok(planar) => {
                                                    decode_stats.inc_output();
                                                    if let Some(tc) = transcoder.as_mut() {
                                                        match tc.process(&planar) {
                                                            Ok(shuffled) => {
                                                                encoder.submit_planar(&shuffled, pts);
                                                            }
                                                            Err(e) => {
                                                                tracing::debug!(
                                                                    "WHIP '{}' transcode failed: {}",
                                                                    config.id, e
                                                                );
                                                            }
                                                        }
                                                    } else {
                                                        encoder.submit_planar(&planar, pts);
                                                    }
                                                }
                                                Err(_) => {
                                                    decode_stats.inc_error();
                                                }
                                            }
                                            for frame in encoder.drain() {
                                                let media_time = MediaTime::new(
                                                    frame.pts * 48_000 / 90_000,
                                                    str0m::media::Frequency::FORTY_EIGHT_KHZ,
                                                );
                                                if let Err(e) = session.write_media(
                                                    audio_mid,
                                                    audio_pt,
                                                    Instant::now(),
                                                    media_time,
                                                    &frame.data,
                                                ) {
                                                    tracing::debug!(
                                                        "WHIP audio write error: {}", e
                                                    );
                                                }
                                                session.drain_outputs().await;
                                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                                stats.bytes_sent.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                                                stats.record_latency(recv_time_us);
                                            }
                                        }
                                    }
                                }
                            }

                            // Drive str0m: process incoming RTCP/STUN + send queued output.
                            if let Some(ev) = session.drive_udp_io().await {
                                match ev {
                                    super::webrtc::session::SessionEvent::Disconnected => {
                                        tracing::warn!("WHIP client '{}' disconnected during send", config.id);
                                        events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHIP client disconnected", flow_id);
                                        continue 'outer;
                                    }
                                    super::webrtc::session::SessionEvent::KeyframeRequest { .. } => {
                                        // We can't generate keyframes — log and ignore.
                                        tracing::debug!("WHIP client '{}': received PLI/FIR (ignored, passthrough mode)", config.id);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        }
                        Err(broadcast::error::RecvError::Closed) => break 'outer,
                    }
                }
            }
        }
    }

    tracing::info!("WebRTC output '{}' stopped", config.id);
}

/// Lazy build of the WebRTC encoder state on the first AAC frame. Mirrors
/// `super::output_rtmp::build_encoder_state` but specialised for the
/// AAC → Opus path. Returns Failed (with a clear log) if the input is
/// non-AAC-LC, ffmpeg is missing, or anything in the decode/encode chain
/// rejects the source.
#[cfg(feature = "webrtc")]
fn build_webrtc_encoder_state(
    audio_encode: Option<&crate::config::models::AudioEncodeConfig>,
    transcode: Option<&super::audio_transcode::TranscodeJson>,
    demuxer: &super::ts_demux::TsDemuxer,
    compressed_audio_input: bool,
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    output_id: &str,
    events: &EventSender,
) -> WebrtcEncoderState {
    let Some(enc_cfg) = audio_encode else {
        return WebrtcEncoderState::Disabled;
    };

    if !compressed_audio_input {
        let msg = format!(
            "WebRTC output '{}': audio_encode is set but the flow input cannot carry TS audio (PCM-only source); audio will be dropped",
            output_id
        );
        tracing::error!("{msg}");
        events.emit_flow(
            EventSeverity::Critical,
            crate::manager::events::category::AUDIO_ENCODE,
            msg,
            flow_id,
        );
        return WebrtcEncoderState::Failed;
    }

    let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() else {
        return WebrtcEncoderState::Lazy;
    };

    if profile != 1 {
        let msg = format!(
            "WebRTC output '{}': audio_encode requires AAC-LC input (ADTS profile=1, AOT=2), got profile={profile} (AOT={}); audio will be dropped",
            output_id,
            profile + 1
        );
        tracing::error!("{msg}");
        events.emit_flow(
            EventSeverity::Critical,
            crate::manager::events::category::AUDIO_ENCODE,
            msg,
            flow_id,
        );
        return WebrtcEncoderState::Failed;
    }

    let Some(input_sr) = sample_rate_from_index(sr_idx) else {
        tracing::error!(
            "WebRTC output '{}': audio_encode rejected unsupported AAC sample_rate_index={sr_idx}",
            output_id
        );
        return WebrtcEncoderState::Failed;
    };
    if ch_cfg == 0 || ch_cfg > 2 {
        tracing::error!(
            "WebRTC output '{}': audio_encode rejected unsupported AAC channel_config={ch_cfg}",
            output_id
        );
        return WebrtcEncoderState::Failed;
    }

    // Validation guarantees codec=opus for WebRTC; no need to handle others.
    let Some(codec) = AudioCodec::parse(&enc_cfg.codec) else {
        tracing::error!(
            "WebRTC output '{}': audio_encode unknown codec '{}'",
            output_id, enc_cfg.codec
        );
        return WebrtcEncoderState::Failed;
    };
    if codec != AudioCodec::Opus {
        tracing::error!(
            "WebRTC output '{}': audio_encode codec must be 'opus', got '{}'",
            output_id, enc_cfg.codec
        );
        return WebrtcEncoderState::Failed;
    }

    // When a transcode block is set it wins: transcode.channels overrides
    // the Opus encoder's channel count, and transcode.sample_rate (if set)
    // chooses the PCM rate the encoder ingests. When unset the Opus encoder
    // follows the source, matching the pre-transcode behaviour.
    // Opus on the wire is always 48 kHz regardless of either block.
    let (enc_in_sr, target_ch, transcoder) = if let Some(tj_in) = transcode {
        let merged = super::audio_transcode::TranscodeJson {
            sample_rate: tj_in.sample_rate,
            channels: tj_in.channels.or(enc_cfg.channels),
            ..tj_in.clone()
        };
        match super::audio_transcode::PlanarAudioTranscoder::new(
            input_sr, ch_cfg, &merged,
        ) {
            Ok(tc) => (tc.out_sample_rate(), tc.out_channels(), Some(tc)),
            Err(e) => {
                let msg = format!(
                    "WebRTC output '{output_id}': audio_encode transcode build failed: {e}"
                );
                tracing::error!("{msg}");
                events.emit_flow(
                    EventSeverity::Critical,
                    crate::manager::events::category::AUDIO_ENCODE,
                    msg,
                    flow_id,
                );
                return WebrtcEncoderState::Failed;
            }
        }
    } else {
        (input_sr, enc_cfg.channels.unwrap_or(ch_cfg), None)
    };
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    let params = EncoderParams {
        codec,
        sample_rate: enc_in_sr,
        channels: target_ch,
        target_bitrate_kbps: target_br,
        // Opus is always 48 kHz on the wire regardless of operator input.
        target_sample_rate: 48_000,
        target_channels: target_ch,
    };

    let decoder = match AacDecoder::from_adts_config(profile, sr_idx, ch_cfg) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                "WebRTC output '{}': audio_encode AacDecoder build failed: {e}",
                output_id
            );
            return WebrtcEncoderState::Failed;
        }
    };

    let encoder = match AudioEncoder::spawn(
        params,
        cancel.child_token(),
        flow_id.to_string(),
        output_id.to_string(),
        stats.clone(),
        Some(events.clone()),
    ) {
        Ok(e) => e,
        Err(AudioEncoderError::FfmpegNotFound) => {
            let msg = format!(
                "WebRTC output '{}': audio_encode requires ffmpeg in PATH but it is not installed; audio will be dropped",
                output_id
            );
            tracing::error!("{msg}");
            events.emit_flow(
                EventSeverity::Critical,
                crate::manager::events::category::AUDIO_ENCODE,
                msg,
                flow_id,
            );
            return WebrtcEncoderState::Failed;
        }
        Err(e) => {
            let msg = format!(
                "WebRTC output '{}': audio_encode spawn failed: {e}",
                output_id
            );
            tracing::error!("{msg}");
            events.emit_flow(
                EventSeverity::Critical,
                crate::manager::events::category::AUDIO_ENCODE,
                msg,
                flow_id,
            );
            return WebrtcEncoderState::Failed;
        }
    };

    tracing::info!(
        "WebRTC output '{}': audio_encode active (Opus, source {} Hz {} ch -> 48000 Hz {} ch, {} kbps)",
        output_id, input_sr, ch_cfg, target_ch, target_br,
    );

    // Register decode + encode stats with the shared per-output accumulator.
    let decode_stats = Arc::new(DecodeStats::new());
    stats.set_decode_stats(
        decode_stats.clone(),
        decoder.codec_name(),
        decoder.sample_rate(),
        decoder.channels(),
    );
    stats.set_encode_stats(
        encoder.stats_handle(),
        encoder.params().codec.as_str().to_string(),
        encoder.params().target_sample_rate,
        encoder.params().target_channels,
        encoder.params().target_bitrate_kbps,
    );

    WebrtcEncoderState::Active { decoder, encoder, decode_stats, transcoder }
}
