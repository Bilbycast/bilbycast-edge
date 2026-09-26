// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
use super::input_post_process::{InputPostProcess, InputPostProcessConfig};
use super::input_transcode::{publish_input_packet_with_post, InputTranscoder};
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
    force_idr: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("WHIP input started for flow '{}', waiting for publisher", flow_id);
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
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
            transcoder.as_mut(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
            &event_sender,
        );
        // Synthetic-TS input — TsMuxer handles pid_overrides; the post-
        // process only does program_filter (no-op) + pid_map. No
        // `passthrough_clock` on WebrtcInputConfig — TsMuxer already
        // controls PTS.
        let av_skew_for_post = stats.as_ref().av_skew_reporter_for_input(&input_id);
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: None,
            pid_map: config.pid_map.as_ref(),
            passthrough_clock: false,
            av_sync_pacer: None,
            av_skew: Some(&av_skew_for_post),
        });
        if let Some(ref _p) = post {
            tracing::info!("WHIP input: ingress post-process active");
        }
        whip_input_loop(config, &flow_id, broadcast_tx, stats, cancel, session_rx, &event_sender, &mut transcoder, &mut post).await;
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
    post: &mut Option<InputPostProcess>,
) {
    let public_ip: Option<std::net::IpAddr> =
        config.public_ip.as_ref().and_then(|ip| ip.parse().ok());
    // Bind precedence (BUG-006):
    //   1. `bind_addr` — explicit operator pin (e.g. `0.0.0.0:8000`).
    //      Required for fixed-port deployments behind a firewall / NAT.
    //   2. `public_ip` set → bind that IP, OS-chosen port. The local
    //      address must match the host ICE candidate we advertise, or
    //      str0m's per-packet destination check rejects the binding
    //      request ("Discarding STUN request on unknown interface").
    //   3. Neither set → `0.0.0.0:0` (legacy auto-bind).
    let bind_addr: std::net::SocketAddr = if let Some(addr_str) = config.bind_addr.as_ref() {
        match addr_str.parse() {
            Ok(addr) => addr,
            Err(e) => {
                tracing::error!("Invalid WebRTC bind_addr '{}': {}", addr_str, e);
                events.emit_flow(EventSeverity::Critical, category::WEBRTC,
                    format!("WebRTC input invalid bind_addr '{addr_str}': {e}"), flow_id);
                return;
            }
        }
    } else {
        match public_ip {
            Some(ip) => std::net::SocketAddr::new(ip, 0),
            None => "0.0.0.0:0".parse().unwrap(),
        }
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
        // Per-session cancel token, rooted at the input task's parent. The
        // API layer holds a clone so DELETE /whip/... tears down exactly
        // this session without affecting the rest of the input task or
        // other concurrent sessions.
        let child_cancel = cancel.child_token();
        let _ = msg.reply.send(Ok((answer, session_id.clone(), child_cancel.clone())));

        // Create TS muxer for converting H.264 NALUs and Opus audio to
        // MPEG-TS. WebRTC audio is always Opus; we carry it per the
        // FFmpeg-compatible Opus-in-MPEG-TS convention (stream_type 0x06
        // + "Opus" registration descriptor in the PMT, per-AU control
        // header in the private PES).
        let mut ts_muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
        if let Some(po) = config.pid_overrides.as_ref()
            && let Some(entry) = po.get(&1) {
                ts_muxer.set_pids(entry.pmt_pid, entry.video_pid, entry.audio_pid, entry.pcr_pid);
            }
        ts_muxer.set_audio_stream(0x06, Some(*b"Opus"));
        let mut seq_num: u16 = 0;
        let mut last_audio_pts_90khz: u64 = 0;
        // One timeline for both tracks (`WhipClock`): each track's RTP
        // timestamps start at an unrelated random base.
        let mut clock = WhipClock::default();
        // The publish's layout — which of video and audio its offer carried
        // — is settled from the session's tracks on the first media
        // (`WhipLayout`). The muxer used to assume video and no audio: an
        // audio-only publish named an absent video PID as PCR_PID and
        // carried no PCR at all, and an A/V publish's Opus never reached the
        // PMT.
        let mut layout = WhipLayout::default();
        let mut param_sets = H264ParamSets::default();

        // Receive media from the WebRTC session
        loop {
            let event = session.poll_event(&child_cancel).await;

            match event {
                SessionEvent::MediaData { mid, data, rtp_time, network_time, .. } => {
                    // Determine if this is video or audio
                    let is_video = session.video_mid == Some(mid);
                    let is_audio_stream = session.audio_mid == Some(mid);
                    layout.apply(
                        &mut ts_muxer,
                        session.video_mid.is_some(),
                        session.audio_mid.is_some(),
                        flow_id,
                    );

                    if is_audio_stream {
                        // Opus-over-WHIP audio: the RTP timestamp on the
                        // Opus clock (48 kHz, carried in MediaTime.denom)
                        // onto the publish's 90 kHz timeline (`WhipClock`),
                        // then muxed via the FFmpeg-compatible Opus-in-TS
                        // path.
                        let denom = rtp_time.denom() as u64;
                        let pts_90khz = if denom == 0 {
                            last_audio_pts_90khz
                        } else {
                            clock.pts_90k(false, rtp_time.numer(), denom, network_time)
                        };
                        last_audio_pts_90khz = pts_90khz;
                        let ts_chunks = ts_muxer.mux_audio_opus(&data, pts_90khz);
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
                                upstream_seq: None,
                                upstream_leg_id: None,
                                sender_timestamp_us: None,
                            };
                            seq_num = seq_num.wrapping_add(1);
                            stats.input_packets.fetch_add(1, Ordering::Relaxed);
                            stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                            if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                publish_input_packet_with_post(transcoder, post, &broadcast_tx, pkt);
                            } else {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else if is_video {
                        // A depayloaded access unit, Annex B, with the
                        // parameter sets ahead of every IDR
                        // (`H264ParamSets`).
                        let pts_90khz = clock.pts_90k(true, rtp_time.numer(), rtp_time.denom() as u64, network_time);
                        let (annex_b, is_keyframe) = param_sets.access_unit(&data);
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
                                upstream_seq: None,
                                upstream_leg_id: None,
                                sender_timestamp_us: None,
                            };
                            seq_num = seq_num.wrapping_add(1);
                            stats.input_packets.fetch_add(1, Ordering::Relaxed);
                            stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                            if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                publish_input_packet_with_post(transcoder, post, &broadcast_tx, pkt);
                            } else {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                SessionEvent::MediaAdded { .. } => {
                    // A track negotiated after media began: the layout
                    // follows (a PMT version bump, PCR moving with video).
                    layout.renegotiated(
                        &mut ts_muxer,
                        session.video_mid.is_some(),
                        session.audio_mid.is_some(),
                        flow_id,
                    );
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
    force_idr: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("WHEP input started, connecting to {}", config.whep_url);
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
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
            transcoder.as_mut(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
            &event_sender,
        );
        let av_skew_for_post = stats.as_ref().av_skew_reporter_for_input(&input_id);
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: None,
            pid_map: config.pid_map.as_ref(),
            passthrough_clock: false,
            av_sync_pacer: None,
            av_skew: Some(&av_skew_for_post),
        });
        if let Some(ref _p) = post {
            tracing::info!("WHEP input: ingress post-process active");
        }
        whep_input_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder, &mut post).await;
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
    post: &mut Option<InputPostProcess>,
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
        let tls = crate::util::tls::TlsTrust {
            accept_self_signed: config.accept_self_signed_cert.unwrap_or(true),
            fingerprint: config.cert_fingerprint.clone(),
        };
        // Cancel-guard the signaling POST so a flow-stop doesn't stall on a
        // slow/filtered connect (bounded to ~10 s by connect_timeout). The
        // select! evaluates to the whep_post result, or returns on cancel.
        let post_result = tokio::select! {
            _ = cancel.cancelled() => return,
            r = crate::engine::webrtc::signaling::whep_post(
                &config.whep_url,
                &offer_sdp,
                config.bearer_token.as_deref(),
                &tls,
            ) => r,
        };
        let (answer_sdp, _resource_url) = match post_result {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("WHEP signaling failed: {}", e);
                // Surface the failure in the manager UI — carries the HTTP
                // status + body, or the connect error for a filtered endpoint.
                events.emit_flow(
                    EventSeverity::Warning,
                    category::WEBRTC,
                    format!("WHEP signaling failed: {e}"),
                    flow_id,
                );
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
            // Back off before retrying — a bare `continue` spins the loop.
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
            }
            backoff_secs = (backoff_secs * 2).min(30);
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
        if let Some(po) = config.pid_overrides.as_ref()
            && let Some(entry) = po.get(&1) {
                ts_muxer.set_pids(entry.pmt_pid, entry.video_pid, entry.audio_pid, entry.pcr_pid);
            }
        let mut seq_num: u16 = 0;
        let mut param_sets = H264ParamSets::default();

        // Receive loop
        loop {
            let event = session.poll_event(&cancel).await;
            match event {
                SessionEvent::MediaData { mid, data, rtp_time, .. } => {
                    if session.video_mid == Some(mid) {
                        let pts_90khz = rtp_time.numer();
                        let (annex_b, is_keyframe) = param_sets.access_unit(&data);
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
                                upstream_seq: None,
                                upstream_leg_id: None,
                                sender_timestamp_us: None,
                            };
                            seq_num = seq_num.wrapping_add(1);
                            stats.input_packets.fetch_add(1, Ordering::Relaxed);
                            stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                            if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                publish_input_packet_with_post(transcoder, post, &broadcast_tx, pkt);
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

/// Which tracks a WHIP publish carries, applied to its TS muxer once, on the
/// first media — by then the offer's tracks have all been added
/// (`SessionEvent::MediaAdded` comes out of the answer, before ICE).
#[cfg(feature = "webrtc")]
#[derive(Debug, Default)]
struct WhipLayout {
    applied: bool,
}

#[cfg(feature = "webrtc")]
impl WhipLayout {
    /// Set the muxer's layout from the session's tracks, once. With no video
    /// the PMT lists the Opus alone, names it as PCR_PID, and it carries the
    /// PCR (`TsMuxer::audio_pcr`).
    fn apply(
        &mut self,
        muxer: &mut crate::engine::rtmp::ts_mux::TsMuxer,
        has_video: bool,
        has_audio: bool,
        flow_id: &str,
    ) {
        if self.applied {
            return;
        }
        self.applied = true;
        muxer.set_has_video(has_video);
        muxer.set_has_audio(has_audio);
        tracing::info!(
            flow_id,
            has_video,
            has_audio,
            "WHIP: publish carries {}",
            match (has_video, has_audio) {
                (true, true) => "video and audio",
                (true, false) => "video only",
                (false, true) => "audio only — the PMT names the audio PID as PCR_PID",
                (false, false) => "no media track",
            }
        );
    }

    /// A track added after the layout was applied: video moves the PCR
    /// (`TsMuxer::change_has_video` bumps the PMT version); audio joins the
    /// PMT on its next emission.
    fn renegotiated(
        &mut self,
        muxer: &mut crate::engine::rtmp::ts_mux::TsMuxer,
        has_video: bool,
        has_audio: bool,
        flow_id: &str,
    ) {
        if !self.applied {
            return;
        }
        if has_audio {
            muxer.set_has_audio(true);
        }
        if muxer.change_has_video(has_video) {
            tracing::info!(flow_id, has_video, "WHIP: the publish renegotiated its video track");
        }
    }
}

/// One 90 kHz timeline for a WHIP publish's video and audio.
///
/// Each track's RTP timestamps start at a random base of the sender's
/// choosing (RFC 3550 §5.1; libwebrtc picks one per SSRC), so the video's
/// raw 90 kHz timestamp and the Opus's scaled to 90 kHz were an arbitrary
/// distance apart — up to hours. Once the Opus track reached the PMT an
/// A/V publish carried its audio that far from the video and the PCR: an
/// RTMP restream clamped every audio tag to the first one's time, and a TS
/// or CMAF receiver found the audio hours off its clock.
///
/// Each track is anchored at its first frame's arrival instead: the first
/// frame of any track is placed at the muxer's PCR lead, a track whose
/// first frame arrives later starts that much later, and each track runs on
/// its own RTP clock from there. Audio and video are then as far apart as
/// their first frames' arrival — the sender's capture-to-send latency
/// difference and the network's, tens of milliseconds — not an arbitrary
/// distance. (The RTCP sender reports' NTP ↔ RTP mapping would take out
/// even that; str0m exposes them and doing so is a follow-up.)
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Default)]
struct WhipClock {
    /// When the publish's first frame of any track arrived.
    origin: Option<std::time::Instant>,
    /// Each track's first RTP timestamp and the PTS (90 kHz) it maps to.
    video: Option<(u64, u64)>,
    audio: Option<(u64, u64)>,
}

#[cfg(any(feature = "webrtc", test))]
impl WhipClock {
    /// The PTS (90 kHz, 33 bits) of a frame of the video or audio track:
    /// its RTP timestamp `rtp` (unwrapped) on a `rate` Hz clock, arrived at
    /// `arrival`.
    fn pts_90k(&mut self, video: bool, rtp: u64, rate: u64, arrival: std::time::Instant) -> u64 {
        let origin = *self.origin.get_or_insert(arrival);
        let slot = if video { &mut self.video } else { &mut self.audio };
        let (first, base) = *slot.get_or_insert_with(|| {
            let late_us = arrival.saturating_duration_since(origin).as_micros() as u64;
            (rtp, crate::engine::rtmp::ts_mux::PCR_LEAD_90K + late_us * 9 / 100)
        });
        let ticks = (rtp as i128 - first as i128) * 90_000 / rate.max(1) as i128;
        ((base as i128 + ticks).max(0) as u64) & 0x1_FFFF_FFFF
    }
}

/// The SPS and PPS an H.264 WebRTC stream carried in-band, put back ahead of
/// every IDR that lacks them.
///
/// A WHIP / WHEP sender need not repeat its parameter sets with each IDR
/// (only libwebrtc does so reliably), and the TS carries no out-of-band copy:
/// a receiver that joined an output after the first IDR never decoded a
/// picture. The frame str0m hands over is already Annex B, start codes
/// included — the input used to prefix one more, and to read the IDR from the
/// first byte, which is a start code's zero: no access unit was ever marked a
/// keyframe, so PAT/PMT never led an IDR and no random-access flag was set.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Default)]
struct H264ParamSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

#[cfg(any(feature = "webrtc", test))]
impl H264ParamSets {
    /// `data` (one depayloaded access unit) as Annex B, with the cached SPS /
    /// PPS ahead of an IDR that carries neither of its own, and whether it
    /// holds an IDR slice. The unit's own parameter sets refresh the cache.
    fn access_unit(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        const START: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
        let starts_coded = data.starts_with(&[0, 0, 1]) || data.starts_with(&START);
        let nalus = if starts_coded {
            crate::engine::ts_demux::split_annex_b_nalus(data)
        } else if data.is_empty() {
            Vec::new()
        } else {
            vec![data.to_vec()]
        };
        let (mut idr, mut has_sps, mut has_pps) = (false, false, false);
        for n in &nalus {
            match n.first().map(|b| b & 0x1F) {
                Some(5) => idr = true,
                Some(7) => {
                    has_sps = true;
                    self.sps = Some(n.clone());
                }
                Some(8) => {
                    has_pps = true;
                    self.pps = Some(n.clone());
                }
                _ => {}
            }
        }
        let mut out = Vec::with_capacity(data.len() + 64);
        let mut rest = nalus.iter().peekable();
        if let Some(aud) = rest.next_if(|n| n.first().map(|b| b & 0x1F) == Some(9)) {
            out.extend_from_slice(&START);
            out.extend_from_slice(aud);
        }
        if idr {
            let sps = self.sps.as_ref().filter(|_| !has_sps);
            let pps = self.pps.as_ref().filter(|_| !has_pps);
            for ps in [sps, pps].into_iter().flatten() {
                out.extend_from_slice(&START);
                out.extend_from_slice(ps);
            }
        }
        for n in rest {
            out.extend_from_slice(&START);
            out.extend_from_slice(n);
        }
        (out, idr)
    }
}

#[cfg(test)]
mod tests {
    use super::H264ParamSets;

    /// A WHIP publish's tracks share one timeline, set by when each track's
    /// first frame arrived: the RTP bases (random per SSRC) drop out. The
    /// video's raw RTP timestamp and the Opus's scaled one used to be muxed
    /// as they were — an arbitrary distance apart.
    #[test]
    fn whip_tracks_share_one_timeline_from_their_first_arrival() {
        use super::WhipClock;
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut c = WhipClock::default();
        assert_eq!(c.pts_90k(true, 3_000_000_000, 90_000, t0), LEAD, "the first frame at the lead");
        // The audio's first frame 20 ms later, on an unrelated base.
        assert_eq!(c.pts_90k(false, 123_456, 48_000, t0 + ms(20)), LEAD + 1_800);
        // A second of each: a second on, each on its own clock.
        assert_eq!(c.pts_90k(true, 3_000_090_000, 90_000, t0 + ms(1_000)), LEAD + 90_000);
        assert_eq!(c.pts_90k(false, 123_456 + 48_000, 48_000, t0 + ms(1_020)), LEAD + 1_800 + 90_000);
        // Arrival jitter after the first frame moves nothing.
        assert_eq!(c.pts_90k(false, 123_456 + 96_000, 48_000, t0 + ms(2_500)), LEAD + 1_800 + 180_000);
        // An audio frame a little behind its first (a late reordered one)
        // stays on the timeline.
        assert_eq!(c.pts_90k(false, 123_456 - 960, 48_000, t0 + ms(30)), LEAD + 1_800 - 1_800);
    }

    /// The PMT of `ts` as `(PCR_PID, [(stream_type, PID)])`, and the PIDs
    /// whose packets carry a PCR.
    #[cfg(feature = "webrtc")]
    fn layout_of(ts: &[bytes::Bytes]) -> ((u16, Vec<(u8, u16)>), Vec<u16>) {
        use crate::engine::ts_parse::{extract_pcr, ts_pid};
        let pmt = ts.iter().find(|p| ts_pid(p) == 0x1000).expect("a PMT");
        let sec = &pmt[5..];
        let len = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
        let mut es = Vec::new();
        let mut pos = 12;
        while pos + 5 <= 3 + len - 4 {
            es.push((sec[pos], (((sec[pos + 1] & 0x1F) as u16) << 8) | sec[pos + 2] as u16));
            pos += 5 + ((((sec[pos + 3] & 0x0F) as usize) << 8) | sec[pos + 4] as usize);
        }
        let pcr_pid = (((sec[8] & 0x1F) as u16) << 8) | sec[9] as u16;
        let mut pcr_pids: Vec<u16> = ts.iter().filter(|p| extract_pcr(p).is_some()).map(|p| ts_pid(p)).collect();
        pcr_pids.dedup();
        ((pcr_pid, es), pcr_pids)
    }

    /// An audio-only WHIP publish: the PMT lists the Opus alone and names it
    /// as PCR_PID, and the Opus carries the PCR. The muxer assumed video and
    /// no audio: the PMT named an absent video PID as PCR_PID and listed no
    /// audio, and no packet carried a PCR. An A/V publish lists both, with
    /// the PCR on the video.
    #[cfg(feature = "webrtc")]
    #[test]
    fn a_whip_publishs_layout_follows_its_tracks() {
        let mut mux = crate::engine::rtmp::ts_mux::TsMuxer::new();
        mux.set_audio_stream(0x06, Some(*b"Opus"));
        let mut layout = super::WhipLayout::default();
        layout.apply(&mut mux, false, true, "f");
        let mut ts = Vec::new();
        for k in 0..5u64 {
            ts.extend(mux.mux_audio_opus(&[0xFC; 80], 90_000 + k * 1_800));
        }
        assert_eq!(layout_of(&ts), ((0x0101, vec![(0x06, 0x0101)]), vec![0x0101]));

        let mut mux = crate::engine::rtmp::ts_mux::TsMuxer::new();
        mux.set_audio_stream(0x06, Some(*b"Opus"));
        let mut layout = super::WhipLayout::default();
        layout.apply(&mut mux, true, true, "f");
        let mut ts = mux.mux_video(&[0, 0, 0, 1, 0x65, 0x88], 90_000, 90_000, true);
        ts.extend(mux.mux_audio_opus(&[0xFC; 80], 90_000));
        assert_eq!(layout_of(&ts), ((0x0100, vec![(0x1B, 0x0100), (0x06, 0x0101)]), vec![0x0100]));
    }

    fn nal_types(annex_b: &[u8]) -> Vec<u8> {
        crate::engine::ts_demux::split_annex_b_nalus(annex_b).iter().map(|n| n[0] & 0x1F).collect()
    }

    fn unit(types: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for t in types {
            out.extend_from_slice(&[0, 0, 0, 1, 0x60 | t, 0x11, 0x22]);
        }
        out
    }

    /// Every IDR goes out behind an SPS and a PPS — the last ones the stream
    /// sent — whether or not the sender repeated them; a non-IDR gets none, an
    /// access unit delimiter stays first, and the IDR is found past the start
    /// code (it was read from the start code's first byte, so none was).
    #[test]
    fn every_idr_goes_out_behind_the_parameter_sets() {
        let mut ps = H264ParamSets::default();
        let (au, key) = ps.access_unit(&unit(&[7, 8, 5]));
        assert!(key);
        assert_eq!(nal_types(&au), vec![7, 8, 5], "carried its own: not doubled");
        let (au, key) = ps.access_unit(&unit(&[1]));
        assert!(!key);
        assert_eq!(nal_types(&au), vec![1]);
        let (au, key) = ps.access_unit(&unit(&[5]));
        assert!(key);
        assert_eq!(nal_types(&au), vec![7, 8, 5], "the cached sets go back in");
        let (au, _) = ps.access_unit(&unit(&[9, 5]));
        assert_eq!(nal_types(&au), vec![9, 7, 8, 5]);
        assert!(!au.starts_with(&[0, 0, 0, 1, 0, 0]), "no doubled start code");
    }

    /// A unit handed over without a start code (one bare NAL) is framed once.
    #[test]
    fn a_bare_nal_is_framed() {
        let mut ps = H264ParamSets::default();
        let (au, key) = ps.access_unit(&[0x65, 0x11, 0x22]);
        assert!(key);
        assert_eq!(au, vec![0, 0, 0, 1, 0x65, 0x11, 0x22]);
    }
}
