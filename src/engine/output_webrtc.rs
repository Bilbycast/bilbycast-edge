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
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::OutputStatsAccumulator;

#[cfg(feature = "webrtc")]
use super::audio_decode::{AacDecoder, DecodeStats, sample_rate_from_index};
#[cfg(feature = "webrtc")]
use super::audio_encode::{AudioCodec, AudioEncoder, AudioEncoderError, EncoderParams};
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
    },
    /// Decoder or encoder construction failed once. Drop audio for the
    /// rest of the session's lifetime.
    Failed,
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

    let bind_addr: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
    let public_ip = config.public_ip.as_ref().and_then(|ip| ip.parse().ok());
    let session_config = SessionConfig { bind_addr, public_ip };

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
                events.emit_flow(EventSeverity::Warning, "webrtc", format!("WebRTC session creation failed: {e}"), flow_id);
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
        let viewer_compressed = compressed_audio_input;

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
                viewer_compressed,
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
    compressed_audio_input: bool,
) {
    use super::ts_parse::strip_rtp_header;
    use super::webrtc::ts_demux::TsDemuxer;
    use super::webrtc::rtp_h264::H264Packetizer;
    use super::webrtc::session::SessionEvent;
    use str0m::media::MediaTime;
    use std::time::Instant;

    // Wait for ICE+DTLS to complete
    loop {
        let event = session.poll_event(&cancel).await;
        match event {
            SessionEvent::Connected => {
                tracing::info!("WHEP viewer '{}' connected on output '{}'", session_id, output_id);
                events.emit_flow(EventSeverity::Info, "webrtc", "WHEP viewer connected", flow_id);
                break;
            }
            SessionEvent::Disconnected => {
                tracing::info!("WHEP viewer '{}' disconnected during setup", session_id);
                events.emit_flow(EventSeverity::Info, "webrtc", "WHEP viewer disconnected", flow_id);
                return;
            }
            _ => continue,
        }
    }

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

    // Send loop: demux TS → packetize H.264 → send via str0m
    let mut demuxer = TsDemuxer::new(program_number);
    let mut rtp_seq: u16 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,

            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        let payload = strip_rtp_header(&packet);
                        if payload.is_empty() { continue; }

                        let frames = demuxer.demux(payload);
                        for frame in frames {
                            match frame {
                                super::webrtc::ts_demux::DemuxedFrame::H264 { nalus, pts, .. } => {
                                    let nalu_count = nalus.len();
                                    for (i, nalu) in nalus.iter().enumerate() {
                                        let is_last = i == nalu_count - 1;
                                        let rtp_payloads = H264Packetizer::packetize(nalu, is_last);
                                        for rtp_payload in &rtp_payloads {
                                            let media_time = MediaTime::new(pts, str0m::media::Frequency::NINETY_KHZ);
                                            if let Err(e) = session.write_media(
                                                video_mid,
                                                video_pt,
                                                Instant::now(),
                                                media_time,
                                                &rtp_payload.data,
                                            ) {
                                                tracing::debug!("WHEP viewer '{}' write error: {}", session_id, e);
                                            }
                                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                            stats.bytes_sent.fetch_add(rtp_payload.data.len() as u64, Ordering::Relaxed);
                                            rtp_seq = rtp_seq.wrapping_add(1);
                                        }
                                    }
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
                                        WebrtcEncoderState::Active { decoder, encoder, decode_stats },
                                        Some(audio_mid),
                                        Some(audio_pt),
                                    ) = (&mut encoder_state, audio_mid, audio_pt)
                                    {
                                        decode_stats.inc_input();
                                        match decoder.decode_frame(&data) {
                                            Ok(planar) => {
                                                decode_stats.inc_output();
                                                encoder.submit_planar(&planar, pts);
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
                                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                            stats.bytes_sent.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                        }

                        // Drive str0m (send any queued output)
                        while let Ok(str0m::Output::Transmit(t)) = session.rtc_poll_output() {
                            let _ = session.send_udp(&t).await;
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
    events.emit_flow(EventSeverity::Info, "webrtc", "WHEP viewer disconnected", flow_id);
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
    use super::webrtc::rtp_h264::H264Packetizer;
    use str0m::media::MediaTime;

    let whip_url = match &config.whip_url {
        Some(url) => url.clone(),
        None => {
            tracing::error!("WebRTC output '{}': no whip_url configured", config.id);
            return;
        }
    };

    let bind_addr: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
    let public_ip = config.public_ip.as_ref().and_then(|ip| ip.parse().ok());
    let session_config = SessionConfig { bind_addr, public_ip };
    let mut backoff_secs = 1u64;

    'outer: loop {
        // Create session
        let mut session = match WebrtcSession::new(&session_config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("WHIP client '{}': session error: {}", config.id, e);
                events.emit_flow(EventSeverity::Warning, "webrtc", format!("WebRTC session creation failed: {e}"), flow_id);
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
        tracing::info!("WHIP client '{}' connected to {}", config.id, whip_url);

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
        let mut encoder_state: WebrtcEncoderState =
            if audio_encode.is_some() && audio_mid.is_some() && audio_pt.is_some() {
                WebrtcEncoderState::Lazy
            } else {
                WebrtcEncoderState::Disabled
            };

        // Send loop: demux TS → packetize H.264 → send via str0m
        let mut demuxer = TsDemuxer::new(config.program_number);
        let mut rtp_seq: u16 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break 'outer,

                result = rx.recv() => {
                    match result {
                        Ok(packet) => {
                            let payload = strip_rtp_header(&packet);
                            if payload.is_empty() { continue; }

                            let frames = demuxer.demux(payload);
                            for frame in frames {
                                match frame {
                                    super::webrtc::ts_demux::DemuxedFrame::H264 { nalus, pts, .. } => {
                                        let nalu_count = nalus.len();
                                        for (i, nalu) in nalus.iter().enumerate() {
                                            let is_last = i == nalu_count - 1;
                                            let rtp_payloads = H264Packetizer::packetize(nalu, is_last);
                                            for rtp_payload in &rtp_payloads {
                                                let media_time = MediaTime::new(pts, str0m::media::Frequency::NINETY_KHZ);
                                                if let Err(e) = session.write_media(
                                                    video_mid,
                                                    video_pt,
                                                    Instant::now(),
                                                    media_time,
                                                    &rtp_payload.data,
                                                ) {
                                                    tracing::debug!("WHIP write error: {}", e);
                                                }
                                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                                stats.bytes_sent.fetch_add(rtp_payload.data.len() as u64, Ordering::Relaxed);
                                                rtp_seq = rtp_seq.wrapping_add(1);
                                            }
                                        }
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::Opus => {
                                        // Native Opus passthrough not yet implemented
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::Aac { data, pts } => {
                                        if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                            encoder_state = build_webrtc_encoder_state(
                                                audio_encode.as_ref(),
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
                                            WebrtcEncoderState::Active { decoder, encoder, decode_stats },
                                            Some(audio_mid),
                                            Some(audio_pt),
                                        ) = (&mut encoder_state, audio_mid, audio_pt)
                                        {
                                            decode_stats.inc_input();
                                            match decoder.decode_frame(&data) {
                                                Ok(planar) => {
                                                    decode_stats.inc_output();
                                                    encoder.submit_planar(&planar, pts);
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
                                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                                stats.bytes_sent.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                                            }
                                        }
                                    }
                                }
                            }

                            // Drive str0m (send any queued output)
                            // poll_event would block, so just drain transmits
                            while let Ok(str0m::Output::Transmit(t)) = session.rtc_poll_output() {
                                let _ = session.send_udp(&t).await;
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

    let target_ch = enc_cfg.channels.unwrap_or(ch_cfg);
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    let params = EncoderParams {
        codec,
        sample_rate: input_sr,
        channels: ch_cfg,
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
        "AAC-LC",
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

    WebrtcEncoderState::Active { decoder, encoder, decode_stats }
}
