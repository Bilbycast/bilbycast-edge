// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
#[cfg(feature = "webrtc")]
use super::audio_silence::SilenceGenerator;
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
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
    ///
    /// When `silent_fallback` is set, the encoder is built eagerly at
    /// session startup (before any source audio arrives) and the decoder
    /// is filled lazily on the first real AAC frame — hence it is an
    /// `Option`; the stage, pinned to the encoder's format, is built with
    /// the encoder.
    Active {
        /// `None` until the first real AAC frame arrives (silent-fallback
        /// builds the encoder ahead of any source audio).
        decoder: Option<AacDecoder>,
        encoder: AudioEncoder,
        decode_stats: Arc<DecodeStats>,
        /// The channel / rate stage between the decoder and the Opus
        /// encoder (`audio_transcode::encoder_stage`): the `transcode`
        /// block, or `audio_encode.sample_rate` / `channels` alone when
        /// they differ from the source, converting to the format the
        /// encoder was opened at, in streaming mode (its constant delay is
        /// taken off the encoder's stamps). An MP2 / AC-3 / E-AC-3 source
        /// goes through it too.
        stage: super::audio_transcode::EncoderStage,
        /// Silent-PCM generator + audio-drop watchdog. `Some` iff
        /// `audio_encode.silent_fallback = true`.
        silence: Option<SilenceGenerator>,
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
    /// source access unit that can open them — for H.264, the first that
    /// carries the SPS (`SpsOpenGate`), so the decoder's reorder depth is
    /// seeded from it.
    #[cfg(feature = "media-codecs")]
    Lazy {
        cfg: VideoEncodeConfig,
        sps_gate: crate::engine::video_encode_util::SpsOpenGate,
    },
    /// Decode → re-encode pipeline is live.
    #[cfg(feature = "media-codecs")]
    Active(Box<WebrtcVideoActive>),
    /// Decoder or encoder construction failed once. Drop video for the
    /// rest of the session's lifetime.
    Failed,
}

#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
struct WebrtcVideoActive {
    decoder: video_engine::VideoDecoder,
    /// Shared encoder pipeline — wraps `VideoEncoder` + optional
    /// `VideoScaler`. WebRTC uses `global_header = false` because every
    /// IDR already carries in-band SPS/PPS that the RFC 6184 packetizer
    /// forwards verbatim; no out-of-band codec config channel is
    /// needed. Lazy-opens on the first decoded frame.
    pipeline: crate::engine::video_encode_util::ScaledVideoEncoder,
    /// Monotonic PTS counter in encoder time base. We pass the source
    /// 90 kHz PTS to `write_media` for correct lip-sync, but the encoder
    /// itself gets a monotonic `out_frame_count` so libx264 / NVENC rate
    /// control behaves predictably.
    out_frame_count: i64,
    /// The encoder rate — pinned, or measured from the decoded frames before
    /// the encoder opens (it used to open at a flat 30/1).
    rate: crate::engine::video_encode_util::EncoderRateLock,
    /// The source PTS of each frame in the encoder, by `out_frame_count`:
    /// what an encoded frame's RTP timestamp is.
    in_flight: crate::engine::video_encode_util::EncodedPtsMap,
    stats: Arc<VideoEncodeStats>,
}

/// Resolve a `video_encode.codec` string into a [`VideoEncoderCodec`] for
/// WebRTC. Only H.264 backends are allowed; validation rejects HEVC at
/// config-load, but guard against it at runtime too. Honours `h264_auto`
/// → resolver-picked H.264 backend on the host.
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn resolve_webrtc_video_backend(
    cfg: &VideoEncodeConfig,
    output_id: &str,
    flow_id: &str,
    event_sender: &EventSender,
) -> Option<video_codec::VideoEncoderCodec> {
    let codec = cfg.codec.as_str();
    // Auto path: route through the encoder resolver but cap the result
    // at H.264 — if the resolver picked an HEVC backend (because Auto
    // got hevc_auto / auto), reject with the same not-supported event
    // shape. WebRTC browsers only decode H.264.
    if (codec == "h264_auto" || codec == "auto")
        && crate::engine::hardware_probe::static_capabilities().is_some() {
            match crate::engine::hardware_probe::resolve_for_video_encode_config(cfg) {
                Ok(r) if r.family() == crate::engine::hardware_probe::EncoderFamily::H264 => {
                    tracing::info!(
                        "WebRTC output '{}': video_encode auto-resolved '{}' → {}",
                        output_id,
                        codec,
                        r.ffmpeg_name(),
                    );
                    return Some(r.as_video_encoder_codec());
                }
                Ok(other) => {
                    let msg = format!(
                        "WebRTC output '{}': video_encode auto-resolved to {} but WebRTC browsers only decode H.264",
                        output_id,
                        other.ffmpeg_name(),
                    );
                    tracing::error!("{msg}");
                    event_sender.emit_flow(
                        EventSeverity::Critical,
                        category::VIDEO_ENCODE,
                        msg,
                        flow_id,
                    );
                    return None;
                }
                Err(e) => {
                    let msg = format!(
                        "WebRTC output '{}': video_encode unavailable: {}",
                        output_id,
                        e.message()
                    );
                    tracing::error!("{msg}");
                    event_sender.emit_flow(
                        EventSeverity::Critical,
                        category::VIDEO_ENCODE,
                        msg,
                        flow_id,
                    );
                    return None;
                }
            }
        }
        // No probe snapshot → fall through to legacy mapping (which
        // doesn't recognise auto, so reports unknown_codec). Production
        // always has caps installed; this is just a test-path fallback.
    match codec {
        "x264" => Some(video_codec::VideoEncoderCodec::X264),
        "h264_nvenc" => Some(video_codec::VideoEncoderCodec::H264Nvenc),
        "h264_qsv" => Some(video_codec::VideoEncoderCodec::H264Qsv),
        "h264_vaapi" => Some(video_codec::VideoEncoderCodec::H264Vaapi),
        // h264_rkmpp produces baseline/main/high H.264 — browser-decodable.
        // (hevc_rkmpp is intentionally omitted; WebRTC is H.264-only here.)
        "h264_rkmpp" => Some(video_codec::VideoEncoderCodec::H264Rkmpp),
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
        #[cfg(feature = "media-codecs")]
        Some(cfg) => WebrtcVideoEncoderState::Lazy {
            cfg: cfg.clone(),
            sps_gate: crate::engine::video_encode_util::SpsOpenGate::new(),
        },
        #[cfg(not(feature = "media-codecs"))]
        Some(_) => WebrtcVideoEncoderState::Failed,
    }
}

/// Open the decoder+stats for a WebRTC video_encode pipeline and flip
/// the state into `Active`. Mirrors RTMP's `open_video_active` but
/// specialised to WebRTC's H.264-only output constraint.
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn open_webrtc_video_active(
    cfg: &VideoEncodeConfig,
    source_is_h264: bool,
    first_au: &[u8],
    output_id: &str,
    flow_id: &str,
    output_stats: &Arc<OutputStatsAccumulator>,
    event_sender: &EventSender,
) -> WebrtcVideoEncoderState {
    let Some(backend) = resolve_webrtc_video_backend(cfg, output_id, flow_id, event_sender)
    else {
        return WebrtcVideoEncoderState::Failed;
    };
    let source_codec = if source_is_h264 {
        video_codec::VideoCodec::H264
    } else {
        video_codec::VideoCodec::Hevc
    };
    // Seeded from the access unit that triggered the open: an H.264
    // decoder's reorder depth comes from its SPS (`ReorderSeed`).
    let decoder = match video_engine::VideoDecoder::open_opts(
        source_codec,
        video_engine::DecoderOptions {
            reorder_seed: video_engine::ReorderSeed::FromAccessUnit(first_au),
            ..Default::default()
        },
    ) {
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
        video_codec::VideoEncoderCodec::H264Qsv => "qsv",
        video_codec::VideoEncoderCodec::H264Vaapi => "vaapi",
        video_codec::VideoEncoderCodec::H264Rkmpp => "rkmpp",
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
    // Unpinned, the encoder opens at the source's measured rate: the
    // placeholder here is replaced when `EncoderRateLock` locks.
    let (pinned, (fps_num, fps_den)) = match (cfg.fps_num, cfg.fps_den) {
        (Some(n), Some(d)) => (true, (n, d)),
        _ => (false, crate::engine::video_encode_util::RATE_LOCK_FALLBACK),
    };
    let mut pipeline = crate::engine::video_encode_util::ScaledVideoEncoder::new(
        cfg.clone(),
        backend,
        fps_num,
        fps_den,
        false,
        format!("WebRTC output '{}'", output_id),
    );
    pipeline.set_resolved_backend_sink(stats_handle.resolved_backend.clone());
    // An interlaced frame from this decoder is a woven frame (H.264) or a
    // single field (HEVC field_seq) — which sets the geometry its sample
    // aspect ratio describes.
    pipeline.set_source_codec(source_codec);
    WebrtcVideoEncoderState::Active(Box::new(WebrtcVideoActive {
        decoder,
        pipeline,
        out_frame_count: 0,
        rate: crate::engine::video_encode_util::EncoderRateLock::new(pinned),
        in_flight: Default::default(),
        stats: stats_handle,
    }))
}

/// Concatenate source NAL units (no start codes) back into an Annex-B
/// byte stream suitable for `VideoDecoder::send_packet`.
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn nalus_to_annex_b_webrtc(nalus: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = nalus.iter().map(|n| 4 + n.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nalu in nalus {
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(nalu);
    }
    out
}

/// Push one source access unit (stamped `pts`) through the decoder +
/// encoder and return every encoded frame — Annex B, with any emitted
/// SPS/PPS inline on IDRs — with the source PTS of the picture it codes.
/// Flips `video_state` to `Failed` on a terminal error; returns an empty
/// vec while the encoder opens (same convention as RTMP).
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn encode_one_video_frame_webrtc(
    video_state: &mut WebrtcVideoEncoderState,
    nalus: &[Vec<u8>],
    pts: u64,
    output_id: &str,
) -> Vec<(Vec<u8>, u64)> {
    let active = match video_state {
        WebrtcVideoEncoderState::Active(a) => a,
        _ => return Vec::new(),
    };
    let annex_b = nalus_to_annex_b_webrtc(nalus);
    let block_result: Result<Vec<(Vec<u8>, u64)>, String> = crate::timed_block_in_place!(
        "output_webrtc.video_encoder",
        crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
        {
            active.stats.input_frames.fetch_add(1, Ordering::Relaxed);
            // The access unit's PTS goes in so each decoded frame carries its
            // own — what the rate lock measures the source's cadence on.
            if let Err(e) = active.decoder.send_packet_with_pts(&annex_b, pts as i64) {
                tracing::debug!("WebRTC output '{}': decoder send_packet: {e:?}", output_id);
            }
            let mut out: Vec<(Vec<u8>, u64)> = Vec::new();
            loop {
                let frame = match active.decoder.receive_frame() {
                    Ok(f) => f,
                    Err(_) => break,
                };
                // Until the rate is known the encoder cannot open (its time
                // base is fixed at open): the frame is dropped.
                if !active.rate.admit(frame.pts(), &mut active.pipeline) {
                    continue;
                }
                // The picture's own source PTS (display order), falling back
                // to the access unit's when the decoder lost it.
                let frame_pts = frame.pts().filter(|p| *p >= 0).map_or(pts, |p| p as u64);
                active.in_flight.push(active.out_frame_count, Some(frame_pts));
                let encoded_frames = match active.pipeline.encode(&frame, Some(active.out_frame_count)) {
                    Ok(frames) => frames,
                    Err(e) => {
                        active.in_flight.cancel_last();
                        if !active.pipeline.is_open() {
                            return Err(format!("encoder open failed: {e}"));
                        }
                        tracing::debug!("WebRTC output '{}': encode error: {e}", output_id);
                        active.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                active.out_frame_count += 1;
                for ef in encoded_frames {
                    let coded_pts = active.in_flight.take(ef.pts).unwrap_or(frame_pts);
                    out.push((ef.data, coded_pts));
                    active.stats.output_frames.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok::<_, String>(out)
        }
    );

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
    #[cfg_attr(not(feature = "media-codecs"), allow(unused_variables))]
    flow_id: &str,
    #[cfg_attr(not(feature = "media-codecs"), allow(unused_variables))]
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
    // access unit that can open them (an H.264 one waits for the SPS,
    // which seeds the decoder's reorder depth). Falls through to the
    // encode path on the same frame.
    #[cfg(feature = "media-codecs")]
    if let WebrtcVideoEncoderState::Lazy { cfg, sps_gate } = video_state {
        let au = nalus_to_annex_b_webrtc(nalus);
        let codec = if source_is_h264 {
            video_codec::VideoCodec::H264
        } else {
            video_codec::VideoCodec::Hevc
        };
        if !sps_gate.admits(codec, &au) {
            return;
        }
        let cfg = cfg.clone();
        *video_state = open_webrtc_video_active(
            &cfg,
            source_is_h264,
            &au,
            output_id,
            flow_id,
            stats,
            events,
        );
        if matches!(video_state, WebrtcVideoEncoderState::Failed) {
            return;
        }
    }

    // The frames to packetize, each with its RTP timestamp — the source
    // NALUs (passthrough), or every frame the encoder handed back, split
    // back into NAL units, each on its own picture's source PTS. They used
    // to be run together as one frame stamped with the access unit being
    // fed: a pipeline's depth late, out of order on a source with B-frames
    // (the receiver saw -80 ms steps on Sky Sports), and one marker bit
    // over several pictures when the encoder handed back more than one.
    #[cfg(feature = "media-codecs")]
    let frames: Vec<(std::borrow::Cow<'_, [Vec<u8>]>, u64)> =
        if matches!(video_state, WebrtcVideoEncoderState::Active(_)) {
            let encoded = encode_one_video_frame_webrtc(video_state, nalus, pts, output_id);
            if matches!(video_state, WebrtcVideoEncoderState::Failed) {
                return;
            }
            encoded
                .into_iter()
                .map(|(annex_b, frame_pts)| {
                    (std::borrow::Cow::Owned(super::ts_demux::split_annex_b_nalus(&annex_b)), frame_pts)
                })
                .collect()
        } else {
            vec![(std::borrow::Cow::Borrowed(nalus), pts)]
        };
    #[cfg(not(feature = "media-codecs"))]
    let frames: Vec<(std::borrow::Cow<'_, [Vec<u8>]>, u64)> = vec![(std::borrow::Cow::Borrowed(nalus), pts)];

    for (send_nalus, frame_pts) in &frames {
        let nalu_count = send_nalus.len();
        for (i, nalu) in send_nalus.iter().enumerate() {
            let is_last = i == nalu_count - 1;
            let rtp_payloads = H264Packetizer::packetize(nalu, is_last);
            for rtp_payload in &rtp_payloads {
                let media_time = MediaTime::new(*frame_pts, Frequency::NINETY_KHZ);
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

    let mut egress_static = crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("webrtc".to_string()),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
        ..Default::default()
    };
    if let Some(ve) = config.video_encode.as_ref() {
        egress_static = egress_static.with_video_encode_target(ve);
    }
    if let Some(ae) = config.audio_encode.as_ref() {
        egress_static = egress_static.with_audio_encode_target(ae);
    }
    output_stats.set_egress_static(egress_static);

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
        // Per-viewer cancel token, rooted at the output task's parent. The
        // API layer holds a clone so DELETE /whep/<session_id> tears down
        // exactly this viewer without affecting other concurrent viewers.
        let viewer_cancel = cancel.child_token();
        let _ = msg.reply.send(Ok((answer, session_id.clone(), viewer_cancel.clone())));

        // Spawn a per-viewer send task
        let viewer_rx = broadcast_tx.subscribe();
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
            whep_viewer_loop(&output_id, &session_id, session, viewer_rx, WhepViewerCfg { stats: viewer_stats, cancel: viewer_cancel, video_only, program_number: viewer_program, events: &viewer_events, flow_id: &viewer_flow_id, audio_encode: viewer_audio_encode, transcode: viewer_transcode, compressed_audio_input: viewer_compressed, video_encode: viewer_video_encode }).await;
        });
    }

    tracing::info!("WHEP server output '{}' stopped", config.id);
}

/// Per-viewer send loop: demux TS → packetize H.264 → send via WebRTC to one viewer.
#[cfg(feature = "webrtc")]
/// Fixed per-viewer settings for a WHEP session: identity, the program it
/// carries, and the audio/video transcode selection.
struct WhepViewerCfg<'a> {
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    video_only: bool,
    program_number: Option<u16>,
    events: &'a EventSender,
    flow_id: &'a str,
    audio_encode: Option<crate::config::models::AudioEncodeConfig>,
    transcode: Option<super::audio_transcode::TranscodeJson>,
    compressed_audio_input: bool,
    video_encode: Option<VideoEncodeConfig>,
}

async fn whep_viewer_loop(
    output_id: &str,
    session_id: &str,
    mut session: super::webrtc::session::WebrtcSession,
    mut rx: broadcast::Receiver<RtpPacket>,
    params: WhepViewerCfg<'_>,
) {
    // Destructured back into the original bindings so the body is unchanged.
    let WhepViewerCfg {
        stats,
        cancel,
        video_only,
        program_number,
        events,
        flow_id,
        audio_encode,
        transcode,
        compressed_audio_input,
        video_encode,
    } = params;

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
    let mut encoder_state: WebrtcEncoderState = match audio_encode.as_ref() {
        Some(cfg) if cfg.silent_fallback && audio_mid.is_some() && audio_pt.is_some() => {
            build_webrtc_encoder_state_eager_for_silent_fallback(
                audio_encode.as_ref(),
                transcode.as_ref(),
                &cancel,
                &stats,
                flow_id,
                output_id,
                events,
            )
        }
        Some(_) if audio_mid.is_some() && audio_pt.is_some() => WebrtcEncoderState::Lazy,
        _ => WebrtcEncoderState::Disabled,
    };
    let mut silence_interval: Option<tokio::time::Interval> =
        if let WebrtcEncoderState::Active { silence: Some(sg), .. } = &encoder_state {
            let mut iv = tokio::time::interval(sg.chunk_duration());
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            Some(iv)
        } else {
            None
        };
    // Lazy FFmpeg-backed decoder for non-AAC sources (MP2 / AC-3 /
    // E-AC-3). Mirrors `decoder` inside `WebrtcEncoderState::Active`
    // for the AAC path.
    #[cfg(feature = "media-codecs")]
    let mut ff_audio_decoder: Option<video_engine::AudioDecoder> = None;
    #[cfg(feature = "media-codecs")]
    let mut ff_audio_codec: Option<video_codec::AudioDecoderCodec> = None;
    let mut video_encoder_state: WebrtcVideoEncoderState =
        init_webrtc_video_encoder_state(video_encode.as_ref());

    // Send loop: demux TS → packetize H.264 → send via str0m.
    // Also processes incoming RTCP/STUN via drive_udp_io() to keep
    // the session alive (same pattern as whip_client_loop).
    let mut demuxer = TsDemuxer::new(program_number);

    loop {
        let silence_tick = async {
            match silence_interval.as_mut() {
                Some(iv) => { iv.tick().await; }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => break,

            _ = silence_tick => {
                emit_webrtc_silence_if_needed(
                    &mut encoder_state,
                    &mut session,
                    audio_mid,
                    audio_pt,
                    &stats,
                    output_id,
                ).await;
                continue;
            }

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
                                super::webrtc::ts_demux::DemuxedFrame::Opus { .. } => {
                                    // Native Opus passthrough (input already
                                    // publishing Opus-in-TS) is not yet wired
                                    // through the str0m audio path. The frame
                                    // is dropped — a rate-limited warning is
                                    // emitted once per output so operators see
                                    // the loss instead of silent degradation.
                                    static OPUS_PASSTHROUGH_WARN: std::sync::atomic::AtomicBool =
                                        std::sync::atomic::AtomicBool::new(false);
                                    if !OPUS_PASSTHROUGH_WARN.swap(true, std::sync::atomic::Ordering::Relaxed) {
                                        events.emit_flow(
                                            EventSeverity::Warning,
                                            category::WEBRTC,
                                            "Opus-in-TS passthrough is not yet wired on WHEP output — dropping audio frames",
                                            flow_id,
                                        );
                                    }
                                }
                                super::webrtc::ts_demux::DemuxedFrame::Aac { data, pts } => {
                                    if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                        encoder_state = build_webrtc_encoder_state(
                                            audio_encode.as_ref(),
                                            transcode.as_ref(),
                                            &demuxer,
                                            Some(&data),
                                            compressed_audio_input,
                                            &cancel,
                                            &stats,
                                            flow_id,
                                            output_id,
                                            events,
                                        );
                                    }
                                    if let (
                                        WebrtcEncoderState::Active {
                                            decoder,
                                            encoder,
                                            decode_stats,
                                            stage,
                                            silence,
                                        },
                                        Some(audio_mid),
                                        Some(audio_pt),
                                    ) = (&mut encoder_state, audio_mid, audio_pt)
                                    {
                                        if decoder.is_none()
                                            && let Some(c) = demuxer.cached_aac_config() {
                                                webrtc_lazy_build_decoder(decoder, c, output_id);
                                            }
                                        if let Some(sg) = silence.as_mut() {
                                            sg.mark_real_audio(pts);
                                        }
                                        let Some(dec) = decoder.as_mut() else {
                                            continue;
                                        };
                                        decode_stats.inc_input();
                                        match dec.decode_frame(&data) {
                                            Ok(planar) => {
                                                decode_stats.inc_output();
                                                match encoder.submit_through(stage, &planar, dec.sample_rate(), pts) {
                                                    Ok(_) => {}
                                                    Err(e) => {
                                                        tracing::debug!(
                                                            "WHEP viewer '{}' transcode failed: {}",
                                                            session_id, e
                                                        );
                                                    }
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
                                #[cfg(feature = "media-codecs")]
                                super::webrtc::ts_demux::DemuxedFrame::OtherAudio {
                                    stream_type, data, pts,
                                } => {
                                    if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                        encoder_state = build_webrtc_encoder_state(
                                            audio_encode.as_ref(),
                                            transcode.as_ref(),
                                            &demuxer,
                                            None,
                                            compressed_audio_input,
                                            &cancel,
                                            &stats,
                                            flow_id,
                                            output_id,
                                            events,
                                        );
                                    }
                                    let (
                                        WebrtcEncoderState::Active {
                                            encoder, silence, stage, ..
                                        },
                                        Some(audio_mid),
                                        Some(audio_pt),
                                    ) = (&mut encoder_state, audio_mid, audio_pt)
                                    else {
                                        continue;
                                    };
                                    let Some(codec) =
                                        crate::engine::audio_decode::ff_codec_for_stream_type(
                                            stream_type,
                                        )
                                    else {
                                        continue;
                                    };
                                    if ff_audio_codec != Some(codec) {
                                        ff_audio_decoder = crate::engine::audio_decode::open_ff_decoder(codec).ok();
                                        ff_audio_codec = Some(codec);
                                    }
                                    let Some(dec) = ff_audio_decoder.as_mut() else {
                                        continue;
                                    };
                                    if let Some(sg) = silence.as_mut() {
                                        sg.mark_real_audio(pts);
                                    }
                                    for au in
                                        crate::engine::audio_decode::split_audio_codec_frames(
                                            &data, codec,
                                        )
                                    {
                                        if dec.send_packet(au, pts as i64).is_err() {
                                            continue;
                                        }
                                        while let Ok(frame) = dec.receive_frame() {
                                            // To the encoder's format (a 5.1
                                            // AC-3 source was refused, MP2 at
                                            // another rate mislabelled).
                                            let _ = encoder.submit_through(stage, &frame.planar, frame.sample_rate, pts);
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
                                #[cfg(not(feature = "media-codecs"))]
                                super::webrtc::ts_demux::DemuxedFrame::OtherAudio { .. } => {}
                                // MPEG-2 video on a WebRTC output requires a
                                // transcode hop we don't have today (WebRTC is
                                // H.264 only). Drop the AU.
                                super::webrtc::ts_demux::DemuxedFrame::Mpeg2 { .. } => {}
                                // Stream discontinuity is metadata for stateful
                                // decoders; the WebRTC packetizer re-anchors on
                                // the next IDR independently.
                                super::webrtc::ts_demux::DemuxedFrame::Discontinuity
                                | super::webrtc::ts_demux::DemuxedFrame::Scte35(_) => {}
                            }
                        }

                        // Drive str0m: process incoming RTCP/STUN + send queued output.
                        if let Some(ev) = session.drive_udp_io().await
                            && let SessionEvent::Disconnected = ev {
                                tracing::info!("WHEP viewer '{}' disconnected during send", session_id);
                                break;
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
        let tls = crate::util::tls::TlsTrust {
            accept_self_signed: config.accept_self_signed_cert.unwrap_or(true),
            fingerprint: config.cert_fingerprint.clone(),
        };
        // Cancel-guard the signaling POST: with a bounded connect_timeout it
        // can still take ~10 s against a filtered endpoint, and without this
        // guard a flow-stop would stall on that connect. The tokio::select!
        // evaluates to the whip_post result, or diverges out of the loop on
        // cancellation.
        let post_result = tokio::select! {
            _ = cancel.cancelled() => break 'outer,
            r = super::webrtc::signaling::whip_post(
                &whip_url,
                &offer_sdp,
                config.bearer_token.as_deref(),
                &tls,
            ) => r,
        };
        let (answer_sdp, _resource_url) = match post_result {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("WHIP signaling '{}' failed: {}", config.id, e);
                // Surface the failure in the manager UI — the error string
                // carries the HTTP status + body from whip_post, or the
                // connect error for a filtered/blackholed endpoint. Without
                // this the output sits silently in the benign "waiting" state.
                events.emit_flow(
                    EventSeverity::Warning,
                    category::WEBRTC,
                    format!("WHIP signaling failed: {e}"),
                    flow_id,
                );
                tokio::select! {
                    _ = cancel.cancelled() => break 'outer,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        if let Err(e) = session.apply_answer(&answer_sdp, pending) {
            tracing::error!("WHIP client '{}': SDP answer error: {}", config.id, e);
            // Back off before retrying — a bare `continue` here spins the
            // session/offer/POST loop with no delay on a persistent SDP fault.
            tokio::select! {
                _ = cancel.cancelled() => break 'outer,
                _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
            }
            backoff_secs = (backoff_secs * 2).min(30);
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
                    // Positive lifecycle signal, symmetric with the WHEP
                    // client input's "WHEP connected" event.
                    events.emit_flow(
                        EventSeverity::Info,
                        category::WEBRTC,
                        "WHIP session established",
                        flow_id,
                    );
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
        let mut encoder_state: WebrtcEncoderState = match audio_encode.as_ref() {
            Some(cfg) if cfg.silent_fallback && audio_mid.is_some() && audio_pt.is_some() => {
                build_webrtc_encoder_state_eager_for_silent_fallback(
                    audio_encode.as_ref(),
                    transcode.as_ref(),
                    &cancel,
                    &stats,
                    flow_id,
                    &config.id,
                    events,
                )
            }
            Some(_) if audio_mid.is_some() && audio_pt.is_some() => WebrtcEncoderState::Lazy,
            _ => WebrtcEncoderState::Disabled,
        };
        // Lazy FFmpeg-backed decoder for non-AAC sources on the WHIP
        // path. Sibling to `decoder` inside `WebrtcEncoderState::Active`.
        #[cfg(feature = "media-codecs")]
        let mut ff_audio_decoder: Option<video_engine::AudioDecoder> = None;
        #[cfg(feature = "media-codecs")]
        let mut ff_audio_codec: Option<video_codec::AudioDecoderCodec> = None;
        let mut silence_interval: Option<tokio::time::Interval> =
            if let WebrtcEncoderState::Active { silence: Some(sg), .. } = &encoder_state {
                let mut iv = tokio::time::interval(sg.chunk_duration());
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                Some(iv)
            } else {
                None
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
            let silence_tick = async {
                match silence_interval.as_mut() {
                    Some(iv) => { iv.tick().await; }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => break 'outer,

                _ = silence_tick => {
                    emit_webrtc_silence_if_needed(
                        &mut encoder_state,
                        &mut session,
                        audio_mid,
                        audio_pt,
                        &stats,
                        &config.id,
                    ).await;
                    continue;
                }

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
                                    super::webrtc::ts_demux::DemuxedFrame::Opus { .. } => {
                                        // Native Opus passthrough not yet implemented
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::Aac { data, pts } => {
                                        if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                            encoder_state = build_webrtc_encoder_state(
                                                audio_encode.as_ref(),
                                                transcode.as_ref(),
                                                &demuxer,
                                                Some(&data),
                                                compressed_audio_input,
                                                &cancel,
                                                &stats,
                                                flow_id,
                                                &config.id,
                                                events,
                                            );
                                        }
                                        if let (
                                            WebrtcEncoderState::Active {
                                                decoder,
                                                encoder,
                                                decode_stats,
                                                stage,
                                                silence,
                                            },
                                            Some(audio_mid),
                                            Some(audio_pt),
                                        ) = (&mut encoder_state, audio_mid, audio_pt)
                                        {
                                            if decoder.is_none()
                                                && let Some(c) = demuxer.cached_aac_config() {
                                                    webrtc_lazy_build_decoder(decoder, c, &config.id);
                                                }
                                            if let Some(sg) = silence.as_mut() {
                                                sg.mark_real_audio(pts);
                                            }
                                            let Some(dec) = decoder.as_mut() else {
                                                continue;
                                            };
                                            decode_stats.inc_input();
                                            match dec.decode_frame(&data) {
                                                Ok(planar) => {
                                                    decode_stats.inc_output();
                                                    match encoder.submit_through(stage, &planar, dec.sample_rate(), pts) {
                                                        Ok(_) => {}
                                                        Err(e) => {
                                                            tracing::debug!(
                                                                "WHIP '{}' transcode failed: {}",
                                                                config.id, e
                                                            );
                                                        }
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
                                    #[cfg(feature = "media-codecs")]
                                    super::webrtc::ts_demux::DemuxedFrame::OtherAudio {
                                        stream_type, data, pts,
                                    } => {
                                        if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                            encoder_state = build_webrtc_encoder_state(
                                                audio_encode.as_ref(),
                                                transcode.as_ref(),
                                                &demuxer,
                                                None,
                                                compressed_audio_input,
                                                &cancel,
                                                &stats,
                                                flow_id,
                                                &config.id,
                                                events,
                                            );
                                        }
                                        let (
                                            WebrtcEncoderState::Active {
                                                encoder, silence, stage, ..
                                            },
                                            Some(audio_mid),
                                            Some(audio_pt),
                                        ) = (&mut encoder_state, audio_mid, audio_pt)
                                        else {
                                            continue;
                                        };
                                        let Some(codec) =
                                            crate::engine::audio_decode::ff_codec_for_stream_type(
                                                stream_type,
                                            )
                                        else {
                                            continue;
                                        };
                                        if ff_audio_codec != Some(codec) {
                                            ff_audio_decoder = crate::engine::audio_decode::open_ff_decoder(codec).ok();
                                            ff_audio_codec = Some(codec);
                                        }
                                        let Some(dec) = ff_audio_decoder.as_mut() else {
                                            continue;
                                        };
                                        if let Some(sg) = silence.as_mut() {
                                            sg.mark_real_audio(pts);
                                        }
                                        for au in
                                            crate::engine::audio_decode::split_audio_codec_frames(
                                                &data, codec,
                                            )
                                        {
                                            if dec.send_packet(au, pts as i64).is_err() {
                                                continue;
                                            }
                                            while let Ok(frame) = dec.receive_frame() {
                                                let _ = encoder.submit_through(stage, &frame.planar, frame.sample_rate, pts);
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
                                                    "WHIP '{}' audio write error: {}",
                                                    config.id, e
                                                );
                                            }
                                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                            stats.bytes_sent.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                                            stats.record_latency(recv_time_us);
                                        }
                                    }
                                    #[cfg(not(feature = "media-codecs"))]
                                    super::webrtc::ts_demux::DemuxedFrame::OtherAudio { .. } => {}
                                    // MPEG-2 video on a WebRTC output requires
                                    // a transcode hop we don't have today
                                    // (WebRTC is H.264 only). Drop the AU.
                                    super::webrtc::ts_demux::DemuxedFrame::Mpeg2 { .. } => {}
                                    // Stream discontinuity is metadata for
                                    // stateful decoders; the WebRTC packetizer
                                    // re-anchors on the next IDR independently.
                                    super::webrtc::ts_demux::DemuxedFrame::Discontinuity
                                    | super::webrtc::ts_demux::DemuxedFrame::Scte35(_) => {}
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
    first_frame: Option<&[u8]>,
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

    if sample_rate_from_index(sr_idx).is_none() {
        tracing::error!(
            "WebRTC output '{}': audio_encode rejected unsupported AAC sample_rate_index={sr_idx}",
            output_id
        );
        return WebrtcEncoderState::Failed;
    }
    if ch_cfg == 0 || ch_cfg > 2 {
        tracing::error!(
            "WebRTC output '{}': audio_encode rejected unsupported AAC channel_config={ch_cfg}",
            output_id
        );
        return WebrtcEncoderState::Failed;
    }
    // What the decoder really hands out — see the RTMP output's
    // `build_encoder_state`: an HE-AAC header gives the core's rate and
    // v2's mono core, which SBR and PS double.
    let Some((input_sr, input_ch)) = first_frame
        .and_then(|f| super::audio_decode::aac_decoded_format(profile, sr_idx, ch_cfg, f))
    else {
        return WebrtcEncoderState::Lazy;
    };

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

    // The channel / rate stage converts the source to what the Opus encoder
    // ingests: the transcode block when set (audio_encode's sample_rate /
    // channels folded in for the fields it leaves unset — transcode.channels
    // wins over the Opus encoder's channel count, transcode.sample_rate
    // chooses the PCM rate it ingests), otherwise those two alone. Without a
    // block, audio_encode.channels used to open the encoder at a channel
    // count the decoded PCM did not have, and every frame was dropped.
    // Opus on the wire is always 48 kHz regardless of either block.
    let mut stage = super::audio_transcode::EncoderStage::new(
        transcode.cloned(),
        enc_cfg.sample_rate,
        enc_cfg.channels,
    );
    let (enc_in_sr, target_ch) = match stage.prepare(input_sr, input_ch) {
        Ok(out) => out,
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
        opus_vbr_mode: enc_cfg.opus_vbr_mode.clone(),
        opus_fec: enc_cfg.opus_fec,
        opus_dtx: enc_cfg.opus_dtx,
        opus_frame_duration_ms: enc_cfg.opus_frame_duration_ms,
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

    let mut encoder = match AudioEncoder::spawn(
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

    // The stage's resampler delay comes off the stamps with the codec's.
    encoder.set_upstream_delay(stage.delay(), enc_in_sr);

    tracing::info!(
        "WebRTC output '{}': audio_encode active (Opus, source {} Hz {} ch -> 48000 Hz {} ch, {} kbps)",
        output_id, input_sr, input_ch, target_ch, target_br,
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

    WebrtcEncoderState::Active {
        decoder: Some(decoder),
        encoder,
        decode_stats,
        stage,
        silence: None,
    }
}

/// Eager encoder construction for `silent_fallback = true` on a WebRTC
/// session. Mirrors [`super::output_rtmp::build_encoder_state_eager_for_silent_fallback`]
/// but tailored to Opus (48 kHz, stereo only).
///
/// Fine details:
/// - WebRTC always emits Opus on the wire; the encoder is opened with
///   input PCM at 48 kHz (the Opus internal clock) and the declared
///   channel count (defaulting to stereo).
/// - A real source AAC frame arriving later triggers lazy construction
///   of the AAC decoder ([`webrtc_lazy_build_decoder`]); the encoder's
///   stage, pinned to 48 kHz and the declared channel count, converts its
///   native format so the encoder sees a consistent PCM format.
#[cfg(feature = "webrtc")]
fn build_webrtc_encoder_state_eager_for_silent_fallback(
    audio_encode: Option<&crate::config::models::AudioEncodeConfig>,
    pending_transcode_cfg: Option<&super::audio_transcode::TranscodeJson>,
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    output_id: &str,
    events: &EventSender,
) -> WebrtcEncoderState {
    let Some(enc_cfg) = audio_encode else {
        return WebrtcEncoderState::Disabled;
    };

    let codec = match AudioCodec::parse(&enc_cfg.codec) {
        Some(c) => c,
        None => {
            tracing::error!(
                "WebRTC output '{}': audio_encode unknown codec '{}'", output_id, enc_cfg.codec
            );
            return WebrtcEncoderState::Failed;
        }
    };

    // Browsers require Opus; the validator catches bad configs, but
    // guard again here so an eager build doesn't start a non-Opus
    // encoder on the back of a misconfigured silent_fallback block.
    if !matches!(codec, AudioCodec::Opus) {
        tracing::error!(
            "WebRTC output '{}': silent_fallback requires codec=opus (got {})",
            output_id, enc_cfg.codec
        );
        return WebrtcEncoderState::Failed;
    }

    // Opus is always 48 kHz on the wire regardless of the declared
    // sample_rate; the channel count is honoured — a transcode block's
    // first, as on every other build of this stage. Taken from
    // `audio_encode` alone, a block's `channels: 1` met an encoder opened in
    // stereo and the pinned stage fell back to the default conversion,
    // dropping the block's routing.
    let target_sr = 48_000_u32;
    let target_ch = pending_transcode_cfg
        .and_then(|b| b.channels)
        .or(enc_cfg.channels)
        .unwrap_or(2)
        .clamp(1, 2);
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    let params = EncoderParams {
        codec,
        sample_rate: target_sr,
        channels: target_ch,
        target_bitrate_kbps: target_br,
        target_sample_rate: target_sr,
        target_channels: target_ch,
        opus_vbr_mode: enc_cfg.opus_vbr_mode.clone(),
        opus_fec: enc_cfg.opus_fec,
        opus_dtx: enc_cfg.opus_dtx,
        opus_frame_duration_ms: enc_cfg.opus_frame_duration_ms,
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
                "WebRTC output '{}': audio_encode(silent_fallback) requires ffmpeg in PATH but it is not installed",
                output_id
            );
            tracing::error!("{msg}");
            events.emit_flow(EventSeverity::Critical, category::AUDIO_ENCODE, msg, flow_id);
            return WebrtcEncoderState::Failed;
        }
        Err(e) => {
            let msg = format!(
                "WebRTC output '{}': audio_encode(silent_fallback) encoder spawn failed: {e}",
                output_id
            );
            tracing::error!("{msg}");
            events.emit_flow(EventSeverity::Critical, category::AUDIO_ENCODE, msg, flow_id);
            return WebrtcEncoderState::Failed;
        }
    };

    let decode_stats = Arc::new(DecodeStats::new());
    stats.set_encode_stats(
        encoder.stats_handle(),
        encoder.params().codec.as_str().to_string(),
        encoder.params().target_sample_rate,
        encoder.params().target_channels,
        encoder.params().target_bitrate_kbps,
    );

    let silence = SilenceGenerator::new(target_sr, target_ch, 0);

    tracing::info!(
        "WebRTC output '{}': audio_encode(silent_fallback) active (Opus 48000 Hz {} ch, {} kbps)",
        output_id, target_ch, target_br,
    );

    // Real audio, when it arrives, is converted to the format the silence
    // opened the encoder at (48 kHz, the declared channel count).
    let mut stage = super::audio_transcode::EncoderStage::new(
        pending_transcode_cfg.cloned(),
        enc_cfg.sample_rate,
        enc_cfg.channels,
    );
    stage.pin_output(target_sr, target_ch);

    WebrtcEncoderState::Active {
        decoder: None,
        encoder,
        decode_stats,
        stage,
        silence: Some(silence),
    }
}

/// Shared silence-tick handler for WebRTC sessions. Submits one
/// zero-filled planar chunk to the encoder, drains any encoded Opus
/// frames, and writes them to the audio MID. Early-returns when:
/// - the encoder state is not Active with a silence generator, or
/// - real audio has arrived within the grace window, or
/// - the session has no audio MID / payload type (SDP negotiated
///   video-only).
#[cfg(feature = "webrtc")]
async fn emit_webrtc_silence_if_needed(
    state: &mut WebrtcEncoderState,
    session: &mut super::webrtc::session::WebrtcSession,
    audio_mid: Option<str0m::media::Mid>,
    audio_pt: Option<str0m::media::Pt>,
    stats: &Arc<OutputStatsAccumulator>,
    output_id: &str,
) {
    use std::time::Instant;
    use str0m::media::MediaTime;

    let (Some(audio_mid), Some(audio_pt)) = (audio_mid, audio_pt) else {
        return;
    };
    let WebrtcEncoderState::Active {
        encoder,
        silence: Some(sg),
        ..
    } = state
    else {
        return;
    };
    if !sg.should_emit() {
        return;
    }
    if !sg.is_emitting() {
        tracing::info!(
            "WebRTC output '{}': source audio absent/stalled — starting silent-Opus injection",
            output_id
        );
    }
    let (planar, pts) = sg.next_chunk();
    encoder.submit_planar(planar, pts);
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
            tracing::debug!("WebRTC output '{}' silent audio write error: {}", output_id, e);
            continue;
        }
        session.drain_outputs().await;
        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        stats.bytes_sent.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
    }
}

/// Lazy build of the AAC decoder on the first real source AAC frame
/// observed while the WebRTC encoder is already running (the
/// silent-fallback path). The encoder's stage, pinned to its format (48 kHz
/// Opus input, the declared channel count), converts whatever the source
/// turns out to be.
#[cfg(feature = "webrtc")]
fn webrtc_lazy_build_decoder(
    decoder: &mut Option<AacDecoder>,
    cached_aac: (u8, u8, u8),
    output_id: &str,
) {
    let (profile, sr_idx, ch_cfg) = cached_aac;
    if profile != 1 {
        tracing::warn!(
            "WebRTC output '{}': silent-fallback ignoring non-AAC-LC source frame (profile={profile})",
            output_id
        );
        return;
    }
    match AacDecoder::from_adts_config(profile, sr_idx, ch_cfg) {
        Ok(d) => *decoder = Some(d),
        Err(e) => {
            tracing::warn!(
                "WebRTC output '{}': silent-fallback AacDecoder build failed: {e}",
                output_id
            );
        }
    }
}

#[cfg(all(test, feature = "webrtc", feature = "fdk-aac"))]
mod stage_tests {
    use super::*;

    /// **B1.** `audio_encode.channels` without a `transcode` block on a
    /// WebRTC output goes through the shared channel / rate stage. The Opus
    /// encoder used to be opened at the override's channel count while the
    /// decoded stereo PCM went to it unconverted, and every frame was
    /// dropped (`planar channel count != configured`).
    #[test]
    fn a_channel_override_without_a_transcode_block_converts() {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let enc: crate::config::models::AudioEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "opus", "channels": 1 })).unwrap();
        let mut demux = super::super::ts_demux::TsDemuxer::new(None);
        demux.demux(&crate::engine::ts_test_fixtures::aac_program_ts(ADTS));
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("w1".into(), "w1".into(), "webrtc".into()));
        let first = demux
            .demux(&crate::engine::ts_test_fixtures::aac_program_ts(ADTS))
            .into_iter()
            .find_map(|f| match f {
                super::super::ts_demux::DemuxedFrame::Aac { data, .. } => Some(data),
                _ => None,
            })
            .unwrap();
        let st = build_webrtc_encoder_state(
            Some(&enc),
            None,
            &demux,
            Some(&first),
            true,
            &CancellationToken::new(),
            &stats,
            "f",
            "w1",
            &events,
        );
        let WebrtcEncoderState::Active { mut encoder, mut stage, .. } = st else {
            panic!("the session must re-encode");
        };
        assert_eq!((encoder.params().channels, encoder.params().target_channels), (1, 1));
        let pcm = stage.process(&[vec![0.1f32; 1024], vec![0.1f32; 1024]], 48_000).unwrap();
        assert_eq!(pcm.len(), 1, "the stage mixes to mono");
        assert!(encoder.submit_planar(&pcm, 90_000), "and the encoder takes it");
    }

    /// An HE-AAC v2 source (header: 24 kHz mono core) goes to the Opus
    /// encoder at what it decodes to, 48 kHz stereo — not converted down to
    /// the core's format the header describes.
    #[test]
    fn an_he_aac_v2_source_is_encoded_at_its_decoded_format() {
        let enc: crate::config::models::AudioEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "opus" })).unwrap();
        let ts = crate::engine::ts_test_fixtures::aac_program_ts(
            &crate::engine::ts_test_fixtures::he_aac_adts(true),
        );
        let mut demux = super::super::ts_demux::TsDemuxer::new(None);
        let first = demux
            .demux(&ts)
            .into_iter()
            .find_map(|f| match f {
                super::super::ts_demux::DemuxedFrame::Aac { data, .. } => Some(data),
                _ => None,
            })
            .unwrap();
        assert_eq!(demux.cached_aac_config(), Some((1, 6, 1)));
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("w1".into(), "w1".into(), "webrtc".into()));
        let st = build_webrtc_encoder_state(
            Some(&enc),
            None,
            &demux,
            Some(&first),
            true,
            &CancellationToken::new(),
            &stats,
            "f",
            "w1",
            &events,
        );
        let WebrtcEncoderState::Active { encoder, stage, .. } = st else {
            panic!("the session must re-encode");
        };
        assert_eq!((encoder.params().sample_rate, encoder.params().channels), (48_000, 2));
        assert_eq!(stage.output(), Some((48_000, 2)));
    }

    /// The silent-fallback Opus encoder takes the transcode block's channel
    /// count (and so keeps its routing) before `audio_encode`'s.
    #[test]
    fn a_silent_fallback_session_takes_the_transcode_blocks_channels() {
        let enc: crate::config::models::AudioEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "opus", "silent_fallback": true })).unwrap();
        let block: super::super::audio_transcode::TranscodeJson =
            serde_json::from_value(serde_json::json!({ "channels": 1, "channel_map_with_gain": [[[0, 1.0]]] }))
                .unwrap();
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("w1".into(), "w1".into(), "webrtc".into()));
        let st = build_webrtc_encoder_state_eager_for_silent_fallback(
            Some(&enc),
            Some(&block),
            &CancellationToken::new(),
            &stats,
            "f",
            "w1",
            &events,
        );
        let WebrtcEncoderState::Active { encoder, mut stage, .. } = st else {
            panic!("silent fallback builds eagerly");
        };
        assert_eq!(encoder.params().channels, 1);
        let out = stage.process(&[vec![0.5f32; 1024], vec![0.0f32; 1024]], 48_000).unwrap();
        assert_eq!(out.len(), 1);
        assert!((out[0][512] - 0.5).abs() < 1e-6, "left only: {}", out[0][512]);
    }
}

#[cfg(all(test, feature = "webrtc", feature = "video-encoder-x264"))]
mod rate_tests {
    use super::*;

    /// Unpinned, the WebRTC encoder opens at the source's measured rate — it
    /// used to open at a flat 30/1 — which needs the decoder to be handed
    /// each access unit's PTS (it used to get none).
    #[test]
    fn an_unpinned_webrtc_encode_opens_at_the_source_rate() {
        let cfg: VideoEncodeConfig =
            serde_json::from_value(serde_json::json!({"codec": "x264", "preset": "veryfast"})).unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "webrtc".into()));
        let (events, _rx) = crate::manager::events::event_channel();
        let aus = crate::engine::output_rtmp::x264_test_source(40, 3_600);
        let mut state = open_webrtc_video_active(&cfg, true, &aus[0].0, "o", "f", &stats, &events);
        let mut sps = None;
        let mut frames = 0;
        for (au, pts) in &aus {
            let nalus = crate::engine::ts_demux::split_annex_b_nalus(au);
            for (out, _) in encode_one_video_frame_webrtc(&mut state, &nalus, *pts, "o") {
                frames += 1;
                sps = sps.or_else(|| video_engine::find_h264_sps(&out));
            }
        }
        let WebrtcVideoEncoderState::Active(active) = &state else { panic!("not active") };
        assert_eq!(active.pipeline.fps(), (25, 1));
        assert_eq!(sps.and_then(|s| s.timing).map(|(n, t, _)| (n, t)), Some((1, 50)), "VUI 25 fps");
        assert_eq!(frames, 40 - 4);
    }

    /// Every encoded frame goes out on its own picture's source PTS. A
    /// source with B-frames arrives I P B B (decode order); the decoder
    /// hands pictures back I B B P, and they used to be stamped with the
    /// access unit being fed — RTP timestamps stepping back and forth.
    #[test]
    fn encoded_frames_keep_their_own_pictures_pts() {
        use video_codec::{VideoEncoderCodec, VideoEncoderConfig, VideoPreset};
        let (w, h, n) = (320usize, 240usize, 40usize);
        let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: w as u32,
            height: h as u32,
            fps_num: 25,
            fps_den: 1,
            gop_size: 25,
            max_b_frames: 2,
            // `zerolatency` would turn the B-frames off.
            tune: String::new(),
            preset: VideoPreset::Veryfast,
            global_header: false,
            ..VideoEncoderConfig::default()
        })
        .unwrap();
        let mut aus = Vec::new();
        for i in 0..n {
            // A texture panning a pixel a frame: x264 codes it with B-frames.
            let y: Vec<u8> = (0..w * h)
                .map(|k| (((k % w + i) * 7919 + (k / w) * 104_729) % 181) as u8 + 30)
                .collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(enc.flush().unwrap());
        assert!(aus.windows(2).any(|a| a[1].pts < a[0].pts), "the source reorders");
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "preset": "veryfast", "fps_num": 25, "fps_den": 1
        }))
        .unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "webrtc".into()));
        let (events, _rx) = crate::manager::events::event_channel();
        let mut state = open_webrtc_video_active(&cfg, true, &aus[0].data, "o", "f", &stats, &events);
        let mut stamps = Vec::new();
        for au in &aus {
            let nalus = crate::engine::ts_demux::split_annex_b_nalus(&au.data);
            let pts = 900_000 + au.pts as u64 * 3_600;
            stamps.extend(encode_one_video_frame_webrtc(&mut state, &nalus, pts, "o").into_iter().map(|(_, p)| p));
        }
        assert!(stamps.len() >= n - 3, "{} frames", stamps.len());
        assert!(stamps.windows(2).all(|w| w[1] == w[0] + 3_600), "{stamps:?}");
    }
}
