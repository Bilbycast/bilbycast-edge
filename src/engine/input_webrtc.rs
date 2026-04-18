// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! WebRTC input tasks: WHIP server (receive contributions) and WHEP client (pull from server).
//!
//! Both modes receive H.264 video (and optionally Opus audio) via WebRTC,
//! mux it into MPEG-TS, and publish `RtpPacket` into the flow's broadcast channel.

#[cfg(feature = "webrtc")]
use std::sync::Arc;
#[cfg(feature = "webrtc")]
use std::sync::atomic::Ordering;

#[cfg(feature = "webrtc")]
use tokio::sync::broadcast;
#[cfg(feature = "webrtc")]
use tokio::task::JoinHandle;
#[cfg(feature = "webrtc")]
use tokio_util::sync::CancellationToken;

#[cfg(feature = "webrtc")]
use crate::config::models::WebrtcInputConfig;
#[cfg(feature = "webrtc")]
use crate::manager::events::{EventSender, EventSeverity, category};
#[cfg(feature = "webrtc")]
use crate::stats::collector::FlowStatsAccumulator;

#[cfg(feature = "webrtc")]
use super::input_transcode::{publish_input_packet, InputTranscoder};
#[cfg(feature = "webrtc")]
use super::packet::RtpPacket;
#[cfg(feature = "webrtc")]
use super::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};

/// Spawn a WHIP server input task.
///
/// Waits for a WHIP publisher to connect via the API endpoint. When a
/// publisher sends an SDP offer (through the session registry), this task
/// creates a WebRTC session, receives H.264/Opus media, muxes it into
/// MPEG-TS, and publishes packets to the broadcast channel.
#[cfg(feature = "webrtc")]
pub fn spawn_whip_input(
    config: WebrtcInputConfig,
    flow_id: String,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    session_rx: tokio::sync::mpsc::Receiver<crate::api::webrtc::registry::NewSessionMsg>,
    event_sender: EventSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("WHIP input started for flow '{}', waiting for publisher", flow_id);
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("WHIP input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("WHIP input: transcode setup failed, passthrough: {e}");
                None
            }
        };
        super::input_transcode::register_ingress_stats(
            stats.as_ref(),
            &input_id,
            transcoder.as_ref(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
        );
        whip_input_loop(config, &flow_id, broadcast_tx, stats, cancel, session_rx, &event_sender, &mut transcoder).await;
        tracing::info!("WHIP input stopped for flow '{}'", flow_id);
    })
}

#[cfg(feature = "webrtc")]
async fn whip_input_loop(
    config: WebrtcInputConfig,
    flow_id: &str,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    mut session_rx: tokio::sync::mpsc::Receiver<crate::api::webrtc::registry::NewSessionMsg>,
    events: &EventSender,
    transcoder: &mut Option<InputTranscoder>,
) {
    let public_ip: Option<std::net::IpAddr> =
        config.public_ip.as_ref().and_then(|ip| ip.parse().ok());
    // Bind the WebRTC UDP socket to the public_ip when set, so the
    // socket's local address (which we report as the destination on every
    // incoming packet) matches the host candidate we advertise to the
    // peer. Without this the agent's per-packet destination check inside
    // the `is` ICE state machine sees `0.0.0.0:<port>` and refuses to
    // pair the incoming STUN binding request with the local candidate
    // (`Discarding STUN request on unknown interface: 0.0.0.0:<port>`),
    // and the connection silently goes straight to Disconnected.
    //
    // When `public_ip` is unset we still bind to 0.0.0.0 (legacy
    // behaviour) so existing LAN deployments that auto-detected an
    // interface IP via the route-discovery probe keep working — but in
    // that mode the destination-mismatch issue may still bite. Operators
    // who hit it should set `public_ip` explicitly.
    let bind_addr: std::net::SocketAddr = match public_ip {
        Some(ip) => std::net::SocketAddr::new(ip, 0),
        None => "0.0.0.0:0".parse().unwrap(),
    };

    loop {
        // Wait for a WHIP publisher to connect
        let msg = tokio::select! {
            _ = cancel.cancelled() => break,
            msg = session_rx.recv() => match msg {
                Some(m) => m,
                None => break, // Channel closed
            }
        };

        tracing::info!("WHIP publisher connecting to flow '{}'", flow_id);

        // Create WebRTC session. WHIP input is the server side — ICE-Lite.
        let session_config = SessionConfig { bind_addr, public_ip, ice_lite: true };
        let mut session = match WebrtcSession::new(&session_config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to create WebRTC session: {}", e);
                events.emit_flow(EventSeverity::Warning, category::WEBRTC, format!("WebRTC session failed: {e}"), flow_id);
                let _ = msg.reply.send(Err(e));
                continue;
            }
        };

        // Accept the SDP offer
        let answer = match session.accept_offer(&msg.offer_sdp) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Failed to accept SDP offer: {}", e);
                let _ = msg.reply.send(Err(e));
                continue;
            }
        };

        let session_id = uuid::Uuid::new_v4().to_string();
        let _ = msg.reply.send(Ok((answer, session_id.clone())));

        // Create TS muxer for converting H.264 NALUs to MPEG-TS
        let mut ts_muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
        let mut seq_num: u16 = 0;
        let child_cancel = cancel.child_token();

        // Receive media from the WebRTC session
        loop {
            let event = session.poll_event(&child_cancel).await;

            match event {
                SessionEvent::MediaData { mid, data, rtp_time, .. } => {
                    // Determine if this is video or audio
                    let is_video = session.video_mid == Some(mid);
                    let _is_audio = session.audio_mid == Some(mid);

                    if is_video {
                        // H.264 data from str0m is already depayloaded (complete NALUs)
                        // Convert to Annex B and mux into TS
                        let pts_90khz = rtp_time.numer() as u64;

                        // Prepend Annex B start codes and feed to TS muxer
                        let mut annex_b = Vec::new();
                        annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                        annex_b.extend_from_slice(&data);

                        let is_keyframe = !data.is_empty() && (data[0] & 0x1F) == 5;
                        let ts_chunks = ts_muxer.mux_video(&annex_b, pts_90khz, pts_90khz, is_keyframe);

                        for ts_data in ts_chunks {
                            let pkt = RtpPacket {
                                data: ts_data,
                                sequence_number: seq_num,
                                rtp_timestamp: pts_90khz as u32,
                                recv_time_us: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_micros() as u64,
                                is_raw_ts: true,
                            };
                            seq_num = seq_num.wrapping_add(1);
                            stats.input_packets.fetch_add(1, Ordering::Relaxed);
                            stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                            if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                publish_input_packet(transcoder, &broadcast_tx, pkt);
                            } else {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    // TODO: Handle Opus audio — mux into TS with stream_type 0x06
                }
                SessionEvent::Connected => {
                    tracing::info!("WHIP publisher connected on flow '{}'", flow_id);
                    events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHIP publisher connected", flow_id);
                }
                SessionEvent::Disconnected => {
                    tracing::info!("WHIP publisher disconnected from flow '{}', waiting for next", flow_id);
                    events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHIP publisher disconnected", flow_id);
                    break; // Go back to waiting for next publisher
                }
                SessionEvent::KeyframeRequest { .. } => {
                    // We're receiving, not sending — ignore
                }
                _ => {}
            }
        }
    }
}

/// Spawn a WHEP client input task.
///
/// Connects to an external WHEP server, receives H.264/Opus media,
/// muxes to TS, and publishes to the broadcast channel.
#[cfg(feature = "webrtc")]
pub fn spawn_whep_input(
    config: crate::config::models::WhepInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("WHEP input started, connecting to {}", config.whep_url);
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("WHEP input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("WHEP input: transcode setup failed, passthrough: {e}");
                None
            }
        };
        super::input_transcode::register_ingress_stats(
            stats.as_ref(),
            &input_id,
            transcoder.as_ref(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
        );
        whep_input_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder).await;
    })
}

#[cfg(feature = "webrtc")]
async fn whep_input_loop(
    config: crate::config::models::WhepInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) {
    let bind_addr: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
    // WHEP input is the client side — full ICE (not ICE-Lite).
    let session_config = SessionConfig { bind_addr, public_ip: None, ice_lite: false };

    let mut backoff_secs = 1u64;

    loop {
        // Create session and SDP offer
        let mut session = match WebrtcSession::new(&session_config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("WHEP: failed to create session: {}", e);
                events.emit_flow(EventSeverity::Warning, category::WEBRTC, format!("WebRTC session failed: {e}"), flow_id);
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        let (offer_sdp, pending) = match session.create_offer(true, !config.video_only, false) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("WHEP: failed to create SDP offer: {}", e);
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        // POST to WHEP endpoint
        let (answer_sdp, _resource_url) = match crate::engine::webrtc::signaling::whep_post(
            &config.whep_url,
            &offer_sdp,
            config.bearer_token.as_deref(),
        ).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("WHEP signaling failed: {}", e);
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        if let Err(e) = session.apply_answer(&answer_sdp, pending) {
            tracing::error!("WHEP: failed to apply SDP answer: {}", e);
            continue;
        }

        backoff_secs = 1; // Reset on successful connection
        tracing::info!("WHEP connected to {}", config.whep_url);
        events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHEP connected", flow_id);

        // Wait for ICE + DTLS handshake to complete before entering the
        // receive loop. Without this, the active-DTLS side of the WHEP
        // client may stall: incoming RTP arrives before DTLS finishes and
        // poll_event's tokio::select races between the recv arm and the
        // sleep arm in a way that starves the DTLS handshake state machine.
        // Explicitly draining poll_event until Connected mirrors what the
        // WHIP client output does and lets ICE/DTLS reliably complete first.
        let mut connected = false;
        loop {
            match session.poll_event(&cancel).await {
                SessionEvent::Connected => {
                    connected = true;
                    break;
                }
                SessionEvent::Disconnected => {
                    tracing::warn!("WHEP disconnected during handshake, will retry");
                    break;
                }
                _ => continue,
            }
        }
        if !connected {
            continue;
        }

        // str0m may emit MediaAdded *after* Connected. Flush any pending
        // events so video_mid is populated before media starts arriving.
        session.drain_pending_events();

        let mut ts_muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
        let mut seq_num: u16 = 0;

        // Receive loop
        loop {
            let event = session.poll_event(&cancel).await;
            match event {
                SessionEvent::MediaData { mid, data, rtp_time, .. } => {
                    if session.video_mid == Some(mid) {
                        let pts_90khz = rtp_time.numer() as u64;
                        let mut annex_b = Vec::new();
                        annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                        annex_b.extend_from_slice(&data);

                        let is_keyframe = !data.is_empty() && (data[0] & 0x1F) == 5;
                        let ts_chunks = ts_muxer.mux_video(&annex_b, pts_90khz, pts_90khz, is_keyframe);

                        for ts_data in ts_chunks {
                            let pkt = RtpPacket {
                                data: ts_data,
                                sequence_number: seq_num,
                                rtp_timestamp: pts_90khz as u32,
                                recv_time_us: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_micros() as u64,
                                is_raw_ts: true,
                            };
                            seq_num = seq_num.wrapping_add(1);
                            stats.input_packets.fetch_add(1, Ordering::Relaxed);
                            stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                            if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                publish_input_packet(transcoder, &broadcast_tx, pkt);
                            } else {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                SessionEvent::Disconnected => {
                    tracing::warn!("WHEP disconnected, reconnecting...");
                    events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHEP disconnected", flow_id);
                    break; // Reconnect
                }
                _ => {}
            }
        }
    }
}
