// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

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
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;

/// Spawn a WebRTC output task (WHIP client or WHEP server depending on config mode).
pub fn spawn_webrtc_output(
    config: WebrtcOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();

    #[cfg(feature = "webrtc")]
    {
        use crate::config::models::WebrtcOutputMode;
        match config.mode {
            WebrtcOutputMode::WhipClient => {
                tokio::spawn(async move {
                    whip_client_loop(config, rx, output_stats, cancel).await;
                })
            }
            WebrtcOutputMode::WhepServer => {
                // WHEP server mode: the actual session creation is driven by
                // API POST requests. This task just keeps the output alive
                // and tracks stats. Individual viewer sessions are spawned
                // by the session registry.
                tokio::spawn(async move {
                    tracing::info!(
                        "WebRTC/WHEP server output '{}' started, waiting for viewers at /api/v1/flows/.../whep",
                        config.id,
                    );
                    webrtc_stub_loop(&config, rx, output_stats, cancel).await;
                })
            }
        }
    }

    #[cfg(not(feature = "webrtc"))]
    {
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

/// WHIP client output loop — pushes media to an external WHIP endpoint.
#[cfg(feature = "webrtc")]
async fn whip_client_loop(
    config: WebrtcOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
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
        let mut connected = false;

        // Wait for connected event before sending media
        loop {
            let event = session.poll_event(&child_cancel).await;
            match event {
                SessionEvent::Connected => {
                    connected = true;
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

        if !connected {
            continue;
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

        // Send loop: demux TS → packetize H.264 → send via str0m
        let mut demuxer = TsDemuxer::new();
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
                                        // TODO: Send Opus via audio MID
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::Aac { .. } => {
                                        // AAC not supported in WebRTC — skip
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
