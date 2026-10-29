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
#[cfg(any(feature = "webrtc", test))]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
use super::webrtc::session::{
    BACKOFF_RESET_AFTER, OfferRefused, SessionConfig, SessionEvent, WebrtcSession,
    rejected_bundle_tag, report_negotiation_panic,
};

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
        whip_input_loop(config, &flow_id, broadcast_tx, stats, cancel, session_rx, &event_sender, &mut transcoder, &mut post, WHIP_SETUP_DEADLINE).await;
        tracing::info!("WHIP input stopped for flow '{}'", flow_id);
    })
}

/// How long an answered WHIP publisher has to complete ICE + DTLS before the
/// input gives up on it and waits for the next one. The input serves one
/// publisher at a time, so a publisher that never connected — its offer
/// answered, then not one STUN check — used to hold it for good: every later
/// publisher's POST hung unanswered until the flow restarted. 15 s matches
/// how long the input already took to let go of a publisher whose ICE
/// completed before it fell silent (ICE reports it Disconnected).
#[cfg(feature = "webrtc")]
const WHIP_SETUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

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
    setup_deadline: std::time::Duration,
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
                report_negotiation_panic(&e, events, flow_id, "WHIP publisher");
                let _ = msg.reply.send(Err(e));
                continue;
            }
        };
        // The tracks the answer accepted. An m-line it refused (a VP8-only
        // video) still reaches `video_mid` / `audio_mid` as a track, but
        // nothing is published on it: the layout must not count it.
        let (video_pt, audio_pt) = session.answered_pts(&answer);
        // An offer this input cannot serve is refused now, 400, rather than
        // answered 201 and then waited on for the setup deadline while every
        // other publisher's POST queues behind it: one carrying nothing it
        // takes, or one whose BUNDLE tag the answer rejects — no browser
        // applies that answer (RFC 8843 §7.3.3), and browsers put video
        // first, so a VP8-only publisher with Opus hit it.
        let refusal = if video_pt.is_none() && audio_pt.is_none() {
            Some(
                "it offers no H.264 video and no Opus audio, the only media this input takes"
                    .to_string(),
            )
        } else {
            rejected_bundle_tag(&msg.offer_sdp, &answer).map(|mid| {
                // The advice follows the m-line refused: a publisher whose
                // audio leads (G.711, say, beside an accepted H.264) was told
                // to put its audio first.
                let advice = match session.media_kind(&mid) {
                    Some(str0m::media::MediaKind::Audio) => "offer Opus, or put the video m-line first",
                    _ => "offer H.264, or put the audio m-line first",
                };
                format!(
                    "its BUNDLE tag (mid {mid}) offers nothing this input takes (H.264 video, Opus audio), \
                     and an answer may not reject the tag (RFC 8843 section 7.3.3): {advice}"
                )
            })
        };
        if let Some(reason) = refusal {
            tracing::warn!("WHIP publisher on flow '{}' refused: {}", flow_id, reason);
            let _ = msg.reply.send(Err(OfferRefused(reason).into()));
            continue;
        }
        let tracks = |session: &WebrtcSession| {
            (
                session.video_mid.is_some() && video_pt.is_some(),
                session.audio_mid.is_some() && audio_pt.is_some(),
            )
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
        // The tracks' frames onto one TS (`WhipMux`): one timeline for
        // both, the parameter sets ahead of every IDR, no payload-less
        // frame.
        let mut whip = WhipMux::new(ts_muxer);
        let mut seq_num: u16 = 0;
        // The publish's layout — which of video and audio its offer carried
        // — is settled from the session's tracks on the first media
        // (`WhipLayout`). The muxer used to assume video and no audio: an
        // audio-only publish named an absent video PID as PCR_PID and
        // carried no PCR at all, and an A/V publish's Opus never reached the
        // PMT.
        let mut layout = WhipLayout::default();
        // Until it connects, the publisher is held to `setup_deadline`.
        let setup_by = tokio::time::Instant::now() + setup_deadline;
        let mut connected = false;

        // Receive media from the WebRTC session
        loop {
            let event = if connected {
                session.poll_event(&child_cancel).await
            } else {
                tokio::select! {
                    event = session.poll_event(&child_cancel) => event,
                    _ = tokio::time::sleep_until(setup_by) => {
                        let secs = setup_deadline.as_secs();
                        tracing::warn!(
                            "WHIP publisher on flow '{}' did not connect within {} s of its answer, waiting for next",
                            flow_id, secs,
                        );
                        events.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::WEBRTC,
                            format!("WHIP publisher did not connect within {secs} s of its answer; waiting for the next publisher"),
                            flow_id,
                            serde_json::json!({
                                "error_code": "webrtc_setup_timeout",
                                "peer": "WHIP publisher",
                                "timeout_secs": secs,
                            }),
                        );
                        break; // Go back to waiting for next publisher
                    }
                }
            };

            match event {
                SessionEvent::MediaData { mid, data, rtp_time, network_time, sender_info, .. } => {
                    // Determine if this is video or audio
                    let is_video = session.video_mid == Some(mid);
                    let is_audio_stream = session.audio_mid == Some(mid);
                    if !is_video && !is_audio_stream {
                        continue;
                    }
                    let (has_video, has_audio) = tracks(&session);
                    layout.apply(&mut whip.muxer, has_video, has_audio, flow_id);
                    // The track's latest sender report, when it is one the
                    // lip-sync can use (`SenderClock::from_report`).
                    let sr = sender_info.and_then(|s| {
                        SenderClock::from_report(s.ntp_time, s.rtp_time.numer(), s.sender_packet_count)
                    });
                    let framed = whip.frame(
                        !is_audio_stream,
                        &data,
                        rtp_time.numer(),
                        rtp_time.denom() as u64,
                        network_time,
                        sr,
                    );
                    if let Some(outcome) = whip.take_sync_outcome() {
                        report_av_sync(outcome, events, flow_id);
                    }
                    let Some((ts_chunks, pts_90khz)) = framed else {
                        continue;
                    };
                    // Seven TS packets to an item, not one (edge#166).
                    for ts_data in crate::engine::packet::bundle_ts_packets(ts_chunks) {
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
                SessionEvent::MediaAdded { .. } => {
                    // A track negotiated after media began: the layout
                    // follows (a PMT version bump, PCR moving with video).
                    let (has_video, has_audio) = tracks(&session);
                    layout.renegotiated(&mut whip.muxer, has_video, has_audio, flow_id);
                }
                SessionEvent::Connected => {
                    connected = true;
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

/// What the lip-sync of a WHIP publish came to ([`WhipClock`]), told the
/// operator once: a full alignment and a publish with no usable sender report
/// in the log, the rest as a Warning.
#[cfg(feature = "webrtc")]
fn report_av_sync(outcome: SyncOutcome, events: &EventSender, flow_id: &str) {
    let ms = |ticks: i64| ticks / 90;
    match outcome {
        SyncOutcome::Applied { step_90k } => tracing::info!(
            flow_id,
            step_ms = ms(step_90k),
            "WHIP: the publish's audio is lined up with its video by its RTCP sender reports \
             (audio moved {} ms)",
            ms(step_90k),
        ),
        SyncOutcome::NoReports => tracing::info!(
            flow_id,
            "WHIP: the publisher sent no usable RTCP sender report in {} s; its audio and video \
             stay aligned by their first frames' arrival",
            AV_SYNC_WINDOW.as_secs(),
        ),
        SyncOutcome::Partial { step_90k, residual_90k } => {
            let (step_ms, residual_ms) = (ms(step_90k), ms(residual_90k));
            let left = if residual_ms < 0 { "late" } else { "early" };
            tracing::warn!(flow_id, step_ms, residual_ms, "WHIP: lip-sync clamped to the PCR window");
            events.emit_flow_with_details(
                EventSeverity::Warning,
                category::WEBRTC,
                format!(
                    "WHIP publish: audio lined up with video by the publisher's sender reports only as far \
                     as the PCR allows (moved {step_ms} ms); it still plays {} ms {left}",
                    residual_ms.abs()
                ),
                flow_id,
                serde_json::json!({
                    "error_code": "webrtc_av_sync_partial",
                    "step_ms": step_ms,
                    "residual_ms": residual_ms,
                }),
            );
        }
        SyncOutcome::Implausible { offset_90k } => {
            let offset_ms = ms(offset_90k);
            tracing::warn!(flow_id, offset_ms, "WHIP: implausible sender reports ignored");
            events.emit_flow_with_details(
                EventSeverity::Warning,
                category::WEBRTC,
                format!(
                    "WHIP publish: the publisher's sender reports put its audio {offset_ms} ms from its video, \
                     more than {} s; ignored",
                    AV_SYNC_PLAUSIBLE_90K / 90_000
                ),
                flow_id,
                serde_json::json!({
                    "error_code": "webrtc_av_sync_implausible",
                    "offset_ms": offset_ms,
                }),
            );
        }
        SyncOutcome::Unsettled { offset_90k, estimates } => {
            let offset_ms = ms(offset_90k);
            tracing::warn!(flow_id, offset_ms, estimates, "WHIP: sender reports never agreed");
            events.emit_flow_with_details(
                EventSeverity::Warning,
                category::WEBRTC,
                format!(
                    "WHIP publish: no two of the publisher's sender reports agreed within {} s \
                     (the latest put its audio {offset_ms} ms from where it plays); its audio and video \
                     stay aligned by their first frames' arrival",
                    AV_SYNC_WINDOW.as_secs()
                ),
                flow_id,
                serde_json::json!({
                    "error_code": "webrtc_av_sync_unsettled",
                    "offset_ms": offset_ms,
                    "estimates": estimates,
                }),
            );
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
                report_negotiation_panic(&e, events, flow_id, "WHEP server");
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
            report_negotiation_panic(&e, events, flow_id, "WHEP server");
            // Back off before retrying — a bare `continue` spins the loop.
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
            }
            backoff_secs = (backoff_secs * 2).min(30);
            continue;
        }

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
            // Backed off like any other failed attempt: it used to retry at
            // once, against a server whose handshake always fails, without
            // end.
            if !whep_back_off(&mut backoff_secs, &cancel).await {
                return;
            }
            continue;
        }
        let connected_at = std::time::Instant::now();

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
                    // A payload-less frame (RTP padding, a sender's
                    // bandwidth probe) is no access unit (see `WhipMux`).
                    if session.video_mid == Some(mid) && !data.is_empty() {
                        let pts_90khz = rtp_time.numer();
                        let (annex_b, is_keyframe) = param_sets.access_unit(&data);
                        let ts_chunks = ts_muxer.mux_video(&annex_b, pts_90khz, pts_90khz, is_keyframe);

                        for ts_data in crate::engine::packet::bundle_ts_packets(ts_chunks) {
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

        // Reconnect after the backoff, which starts over only if the session
        // lasted (`BACKOFF_RESET_AFTER`). It was reset as soon as the answer
        // applied and nothing slept here, so a server that accepts each pull
        // and closes it (DTLS close_notify) was POSTed to back to back.
        if connected_at.elapsed() >= BACKOFF_RESET_AFTER {
            backoff_secs = 1;
        }
        if !whep_back_off(&mut backoff_secs, &cancel).await {
            return;
        }
    }
}

/// Wait out the WHEP input's `backoff_secs` before its next attempt, then
/// double it (to 30 s at most). `false` when `cancel` fired meanwhile — the
/// input is stopping.
#[cfg(feature = "webrtc")]
async fn whep_back_off(backoff_secs: &mut u64, cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => return false,
        _ = tokio::time::sleep(std::time::Duration::from_secs(*backoff_secs)) => {}
    }
    *backoff_secs = (*backoff_secs * 2).min(30);
    true
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

/// A WHIP publish's depayloaded frames onto its TS: one timeline for both
/// tracks ([`WhipClock`]), the parameter sets ahead of every IDR
/// ([`H264ParamSets`]), Opus in the FFmpeg-compatible carriage.
///
/// A frame with no payload is no access unit and goes nowhere. A browser
/// publisher's RTP padding-only packets — Chromium sends three every 5 s as
/// bandwidth probes on the audio SSRC — reach the input as empty media, and
/// each one was muxed as a zero-length Opus AU: 33 of 2964 AUs on the rig,
/// every one failed by ffmpeg ("Error parsing the packet header"). RFC 6716
/// §3.4 [R1]: an Opus packet is at least one byte. Nor does such a frame
/// anchor the timeline.
#[cfg(any(feature = "webrtc", test))]
struct WhipMux {
    muxer: crate::engine::rtmp::ts_mux::TsMuxer,
    clock: WhipClock,
    param_sets: H264ParamSets,
    /// The last audio PTS: a frame whose RTP clock rate is unknown (0)
    /// takes it.
    last_audio_pts_90khz: u64,
}

#[cfg(any(feature = "webrtc", test))]
impl WhipMux {
    fn new(muxer: crate::engine::rtmp::ts_mux::TsMuxer) -> Self {
        Self {
            muxer,
            clock: WhipClock::default(),
            param_sets: H264ParamSets::default(),
            last_audio_pts_90khz: 0,
        }
    }

    /// One depayloaded frame of the video (`video`) or the audio track —
    /// RTP timestamp `rtp` on a `rate` Hz clock, arrived at `arrival`, with
    /// the track's latest usable sender report `sr` — as TS packets and the
    /// PTS it was stamped at; `None` for a frame with no payload, and for an
    /// audio frame a backward lip-sync step put to start before the end of
    /// the last audio frame sent ([`WhipClock`], [`AvSync::drop_until`]).
    fn frame(
        &mut self,
        video: bool,
        data: &[u8],
        rtp: u64,
        rate: u64,
        arrival: Instant,
        sr: Option<SenderClock>,
    ) -> Option<(Vec<bytes::Bytes>, u64)> {
        if data.is_empty() {
            return None;
        }
        if video {
            // A depayloaded access unit, Annex B, with the parameter sets
            // ahead of every IDR (`H264ParamSets`).
            let pts = self.clock.pts_90k(true, rtp, rate, arrival, sr)?;
            let (annex_b, is_keyframe) = self.param_sets.access_unit(data);
            let ts = self.muxer.mux_video(&annex_b, pts, pts, is_keyframe);
            // While the lip-sync is unsettled, the PCR this frame went out
            // with: the audio is kept ahead of the PCR the muxer wrote, not
            // one re-derived from the muxer's rules.
            if self.clock.syncing()
                && let Some(pcr) = frame_pcr_90k(&ts)
            {
                self.clock.note_pcr(pcr, arrival);
            }
            Some((ts, pts))
        } else {
            // Opus: the RTP timestamp on the Opus clock (48 kHz, carried in
            // MediaTime.denom) onto the publish's 90 kHz timeline.
            let pts = if rate == 0 {
                self.last_audio_pts_90khz
            } else {
                self.clock.pts_90k(false, rtp, rate, arrival, sr)?
            };
            self.last_audio_pts_90khz = pts;
            Some((self.muxer.mux_audio_opus(data, pts), pts))
        }
    }

    /// What the publish's lip-sync came to, once, as it settles.
    fn take_sync_outcome(&mut self) -> Option<SyncOutcome> {
        self.clock.sync.outcome.take()
    }
}

/// The PCR (90 kHz) a video frame's TS packets carry: the frame's own, in
/// its PES's first packet, after any filler PCRs the muxer put ahead of it.
#[cfg(any(feature = "webrtc", test))]
fn frame_pcr_90k(ts: &[bytes::Bytes]) -> Option<u64> {
    let mut pcr = None;
    for pkt in ts.iter().flat_map(|b| b.chunks(188)) {
        if pkt.len() < 12 {
            continue;
        }
        if let Some(pcr_27m) = crate::engine::ts_parse::extract_pcr(pkt) {
            pcr = Some(pcr_27m / 300);
            if crate::engine::ts_parse::ts_pusi(pkt) {
                break;
            }
        }
    }
    pcr
}

/// How long after a WHIP publish's first audio frame its audio may still be
/// moved onto its video's clock ([`WhipClock`]). Past it the publish is on
/// air: what the sender reports say is reported, never acted on.
#[cfg(any(feature = "webrtc", test))]
const AV_SYNC_WINDOW: Duration = Duration::from_secs(30);

/// The furthest the sender reports may move the audio (90 kHz): 2 s. A pair
/// asking for more is taken for a broken report and ignored.
#[cfg(any(feature = "webrtc", test))]
const AV_SYNC_PLAUSIBLE_90K: i128 = 180_000;

/// How close (90 kHz) two estimates of where the audio belongs, from report
/// pairs sharing neither report, must come before the audio is moved: 20 ms.
/// Capture-clock reports (a browser, OBS, GStreamer, ffmpeg) agree to well
/// under a millisecond. An edge's WHIP output takes its reports at its write
/// times (str0m runs each on from its last write), so they carry its
/// source's mux timing. The rig's AC-3 source led its PCR by 604–669 ms with
/// its audio PES and by 664–736 ms with its video's decode time (664–935 ms
/// by presentation time); modelled from those audio and decode-time leads —
/// not measured on the wire — estimates scatter by up to about ±70 ms. The
/// model leaves out a term: the sending edge re-frames each 96 ms AC-3 PES
/// into 20 ms Opus frames, and the last one written, which its report runs
/// on from, starts a different distance into its PES each time, so the
/// scatter may be wider still. Such a publish is moved once two estimates
/// happen to agree — at str0m's cadence (audio reports every 5 s, video
/// every second) about 30 pairs are judged in the window — and one whose
/// reports scatter wider may close it unsettled, keeping the first-arrival
/// alignment.
#[cfg(any(feature = "webrtc", test))]
const AV_SYNC_AGREE_90K: i128 = 1_800;

/// A move (90 kHz) smaller than this is not made: 20 ms, half EBU R37's
/// audio-lead tolerance, rather than a gap or a drop for nothing.
#[cfg(any(feature = "webrtc", test))]
const AV_SYNC_MIN_STEP_90K: i128 = 1_800;

/// The least any audio frame of the last second may lead the PCR by after
/// the move (90 kHz): 40 ms, so an AU is in the decoder's buffer before it
/// is due (ISO/IEC 13818-1 T-STD) with a frame of margin.
#[cfg(any(feature = "webrtc", test))]
const AV_SYNC_PCR_FLOOR_90K: i128 = 3_600;

/// The most any audio frame of the last second may lead the PCR by after the
/// move (90 kHz): 0.9 s, inside the 1 s an AU may sit in the T-STD buffers
/// (ISO/IEC 13818-1 §2.4.2.6).
#[cfg(any(feature = "webrtc", test))]
const AV_SYNC_PCR_CEILING_90K: i128 = 81_000;

/// The longest an Opus packet plays (90 kHz): 120 ms (RFC 6716 §3.2.5). It
/// bounds how far past the last frame sent a backward step drops, should the
/// frame it is judged on follow lost ones ([`AvSync::drop_until`]).
#[cfg(any(feature = "webrtc", test))]
const OPUS_MAX_PACKET_90K: i128 = 10_800;

/// How old a sender report may be, in seconds of its track's RTP clock, and
/// still be read at a frame.
#[cfg(any(feature = "webrtc", test))]
const SR_FRESH_SECS: i128 = 15;

/// The NTP time at or before which a sender report has no wallclock behind
/// it: 2000-01-01T00:00:00Z, as a duration since the Unix epoch.
#[cfg(any(feature = "webrtc", test))]
const SR_NTP_FLOOR: Duration = Duration::from_secs(946_684_800);

/// How many estimates of where the audio belongs are kept to agree with a
/// new one — at most one per audio report, so about the last 20 s of a
/// libwebrtc or str0m sender's (one audio report every 5 s).
#[cfg(any(feature = "webrtc", test))]
const AV_SYNC_ESTIMATES: usize = 4;

/// A track's RTCP sender report as the lip-sync reads it (RFC 3550 §6.4.1):
/// the instant `ntp`, on the sender's one wallclock, at which the track's RTP
/// clock read `rtp`.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SenderClock {
    ntp: SystemTime,
    /// The low 32 bits of the RTP time — what the report carried. str0m
    /// extends a report's RTP time from the previous report's, apart from
    /// the media's, so the two can disagree above them.
    rtp: u32,
}

#[cfg(any(feature = "webrtc", test))]
impl SenderClock {
    /// A report — NTP time `ntp` (as str0m converts it), RTP time `rtp`, sent
    /// after `packets` packets — that maps its track onto a wallclock; `None`
    /// for one that does not:
    ///
    /// - sent before any media (`packets` 0): ffmpeg's WHIP muxer sends its
    ///   first report so, and str0m reports RTP 0 before its first write;
    /// - with no wallclock behind it: RFC 3550 lets a sender with none send
    ///   NTP 0, and str0m turns every NTP time before 1970 — 0 included —
    ///   into exactly `UNIX_EPOCH` (a negative duration becomes zero), so a
    ///   check for "before the epoch" never fires. A report at or before
    ///   2000-01-01 is refused instead.
    fn from_report(ntp: SystemTime, rtp: u64, packets: u32) -> Option<Self> {
        if packets == 0 || ntp <= UNIX_EPOCH + SR_NTP_FLOOR {
            return None;
        }
        Some(Self { ntp, rtp: rtp as u32 })
    }

    /// Nanoseconds from this report's instant to RTP time `rtp` of its track
    /// on a `rate` Hz clock, worked on the low 32 bits both carry; `None`
    /// when that is more than [`SR_FRESH_SECS`] off — a report that old says
    /// nothing of this frame (nor does one a str0m sender took from a
    /// wallclock in the future, which it sends as RTP 0).
    fn offset_ns(&self, rtp: u64, rate: u64) -> Option<i128> {
        let ticks = (rtp as u32).wrapping_sub(self.rtp) as i32 as i128;
        if rate == 0 || ticks.abs() > SR_FRESH_SECS * rate as i128 {
            return None;
        }
        Some(ticks * 1_000_000_000 / rate as i128)
    }

    /// The report's instant, in nanoseconds since the Unix epoch.
    fn ntp_ns(&self) -> i128 {
        self.ntp.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128)
    }
}

/// What the lip-sync of a WHIP publish came to ([`WhipClock`]), for the input
/// to report. Moves are in 90 kHz ticks, positive when the audio goes later.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncOutcome {
    /// Lined up within 20 ms: the audio moved `step_90k`, or nowhere (0)
    /// when it already was.
    Applied { step_90k: i64 },
    /// Moved `step_90k`, as far as the PCR window allows; `residual_90k`
    /// more would have lined it up.
    Partial { step_90k: i64, residual_90k: i64 },
    /// A report pair would have moved the audio `offset_90k`, more than 2 s:
    /// ignored. Once per publish.
    Implausible { offset_90k: i64 },
    /// The window closed on `estimates` plausible estimates no two of which
    /// agreed; the latest would have moved the audio `offset_90k`.
    Unsettled { offset_90k: i64, estimates: u32 },
    /// The window closed on an audio-and-video publish without one usable
    /// report pair.
    NoReports,
}

/// Where one pair of sender reports puts a publish's audio: the audio
/// track's base PTS that lines it up with the video.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Clone, Copy)]
struct SyncEstimate {
    audio: SenderClock,
    video: SenderClock,
    base: i128,
}

/// One track of a WHIP publish on the publish's 90 kHz timeline.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug)]
struct WhipTrack {
    /// The first frame's RTP timestamp.
    first: u64,
    /// The PTS (90 kHz) that frame maps to. The audio's is moved by the
    /// lip-sync step.
    base: i128,
    /// The latest usable sender report.
    sr: Option<SenderClock>,
    /// The last frame: RTP timestamp, clock rate, PTS.
    last: Option<(u64, u64, i128)>,
}

#[cfg(any(feature = "webrtc", test))]
impl WhipTrack {
    fn pts(&self, rtp: u64, rate: u64) -> i128 {
        self.base + (rtp as i128 - self.first as i128) * 90_000 / rate.max(1) as i128
    }
}

/// The audio's lead over the PCR, audio frame by audio frame, as the lowest
/// and highest over the last second or more: two spans of a second each, the
/// current and the one before it. A burst of audio frames written back to
/// back (an edge sender re-encoding a 96 ms AC-3 PES into five Opus frames)
/// leads the PCR by less at its first frame than at its last, so the window
/// keeps its first frame's.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Default)]
struct MarginWindow {
    current: Option<MarginSpan>,
    previous: Option<MarginSpan>,
}

#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Clone, Copy)]
struct MarginSpan {
    from: Instant,
    low: i128,
    high: i128,
}

#[cfg(any(feature = "webrtc", test))]
impl MarginWindow {
    const SPAN: Duration = Duration::from_secs(1);

    fn add(&mut self, margin: i128, at: Instant) {
        match &mut self.current {
            Some(span) if at.saturating_duration_since(span.from) < Self::SPAN => {
                span.low = span.low.min(margin);
                span.high = span.high.max(margin);
            }
            _ => {
                self.previous = self.current.take();
                self.current = Some(MarginSpan { from: at, low: margin, high: margin });
            }
        }
    }

    /// The lowest and highest margin over at least the last second; `None`
    /// until a second has been seen.
    fn range(&self) -> Option<(i128, i128)> {
        let (previous, current) = (self.previous?, self.current?);
        Some((previous.low.min(current.low), previous.high.max(current.high)))
    }
}

/// The lip-sync state of a WHIP publish ([`WhipClock`]).
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Default)]
struct AvSync {
    /// The step is taken or given up: nothing more is judged.
    done: bool,
    /// When the publish's first audio frame arrived: the window opened.
    first_audio: Option<Instant>,
    /// The last (audio, video) pair of reports judged: a pair is judged
    /// once, not at every frame.
    judged: Option<(SenderClock, SenderClock)>,
    /// Plausible estimates of where the audio belongs, oldest first, at most
    /// one per audio report (the latest video report's).
    recent: [Option<SyncEstimate>; AV_SYNC_ESTIMATES],
    /// How many plausible estimates were made.
    estimates: u32,
    implausible_reported: bool,
    /// The PCR (90 kHz) the last video frame was muxed with, and when that
    /// frame arrived.
    pcr: Option<(u64, Instant)>,
    margins: MarginWindow,
    /// The highest audio PTS sent.
    last_audio_pts: Option<i128>,
    /// Audio at or before this PTS is dropped: what a backward step puts
    /// under the last frame sent, up to that frame's end. At or before its
    /// start the muxer would have bunched a frame one tick past the last PTS
    /// (`TsMuxer::clamp_audio_pts`); after it, short of its end, two frames
    /// would have overlapped on the wire — every step is a mean of two
    /// estimates, almost never a whole number of frames. Dropped by whole
    /// frames, a step back leaves a gap under one frame instead.
    drop_until: Option<i128>,
    /// What the lip-sync came to, until the input takes it.
    outcome: Option<SyncOutcome>,
}

#[cfg(any(feature = "webrtc", test))]
impl AvSync {
    /// Record how far an audio frame stamped `pts`, arriving `at`, leads the
    /// PCR then: the last video frame's, run on by the time since it arrived.
    fn sample_margin(&mut self, pts: i128, at: Instant) {
        let Some((pcr, pcr_at)) = self.pcr else {
            return;
        };
        let now = pcr as i128 + at.saturating_duration_since(pcr_at).as_nanos() as i128 * 9 / 100_000;
        self.margins.add(signed_33(pts - now), at);
    }

    /// Keep `estimate`, in place of the newest one when that came from the
    /// same audio report (a later video report is the fresher reading).
    fn remember(&mut self, estimate: SyncEstimate) {
        let newest = &mut self.recent[AV_SYNC_ESTIMATES - 1];
        if newest.is_some_and(|n| n.audio == estimate.audio) {
            *newest = Some(estimate);
            return;
        }
        self.recent.rotate_left(1);
        self.recent[AV_SYNC_ESTIMATES - 1] = Some(estimate);
    }
}

/// A 33-bit clock difference (90 kHz) as the signed distance it stands for.
#[cfg(any(feature = "webrtc", test))]
fn signed_33(ticks: i128) -> i128 {
    let wrapped = ticks.rem_euclid(1 << 33);
    if wrapped >= 1 << 32 { wrapped - (1 << 33) } else { wrapped }
}

/// Nanoseconds as 90 kHz ticks, to the nearest.
#[cfg(any(feature = "webrtc", test))]
fn ns_to_90k(ns: i128) -> i128 {
    let scaled = ns * 9;
    (scaled + 50_000 * scaled.signum()) / 100_000
}

/// One 90 kHz timeline for a WHIP publish's video and audio, the audio lined
/// up with the video by the publisher's RTCP sender reports.
///
/// Each track's RTP timestamps start at a random base of the sender's
/// choosing (RFC 3550 §5.1; libwebrtc picks one per SSRC), so the video's
/// raw 90 kHz timestamp and the Opus's scaled to 90 kHz were an arbitrary
/// distance apart — up to hours. Once the Opus track reached the PMT an
/// A/V publish carried its audio that far from the video and the PCR: an
/// RTMP restream clamped every audio tag to the first one's time, and a TS
/// or CMAF receiver found the audio hours off its clock.
///
/// Each track is anchored at its first frame's arrival: the first frame of
/// any track is placed at the muxer's PCR lead, a track whose first frame
/// arrives later starts that much later, and each track runs on its own RTP
/// clock from there. That leaves the tracks as far apart as their first
/// frames' arrival, which is not how far apart they were captured: a track
/// whose first frame came late — an encoder the sender opened lazily — is
/// late by that much for the whole publish. Edge to edge, a publish's audio
/// sat 228 ms (p50) *behind* the PCR, so a strict decoder underflowed on
/// it: its first video frame ~330 ms later than the video's steady state
/// (edge#155).
///
/// So the audio is then moved, once, onto the video's clock by the sender
/// reports (RFC 3550 §6.4.1): each maps an RTP time of its track to an
/// instant on the sender's one wallclock, so a pair of them says how far
/// apart an audio frame and a video frame were captured. The video, and the
/// PCR it carries, never move — a PCR discontinuity would cost every
/// receiver. The audio takes one step: forward (a gap, which a decoder
/// mutes) or back (the overlap is dropped by whole frames, leaving a gap
/// under one frame, [`AvSync::drop_until`]). It is taken only:
///
/// - on reports that map the track onto a wallclock
///   ([`SenderClock::from_report`]) and are no more than 15 s from the
///   frame;
/// - once two report pairs sharing neither report agree on it within 20 ms
///   ([`AV_SYNC_AGREE_90K`]). A pair is judged once, when either report
///   changes, never again at the next frame — what it says does not depend
///   on when frames arrive, so arrival jitter cannot draw a borderline
///   estimate under a bound — and one bad report, or a sender re-anchoring
///   as it is read, is not acted on;
/// - when it is under 2 s ([`AV_SYNC_PLAUSIBLE_90K`]); a pair asking for
///   more is reported (`webrtc_av_sync_implausible`) and ignored;
/// - within 30 s of the first audio frame ([`AV_SYNC_WINDOW`]), so a
///   programme is never stepped minutes in. Reports that never agreed by
///   then are reported (`webrtc_av_sync_unsettled`) instead;
/// - as far as the PCR allows: every audio frame of the last second or more
///   at least 40 ms ahead of the PCR the muxer wrote
///   ([`AV_SYNC_PCR_FLOOR_90K`]) and none more than 0.9 s
///   ([`AV_SYNC_PCR_CEILING_90K`]) — the least lead over a burst of audio
///   frames ([`MarginWindow`]), not the lead of the frame being judged. A
///   publisher whose audio lags its video by more than the muxer's 100 ms
///   PCR lead allows is moved only that far and the rest reported
///   (`webrtc_av_sync_partial`): lined up in full, its audio would be behind
///   the PCR.
///
/// Publishes with no usable report keep the arrival alignment, as do
/// audio-only and video-only ones. A sender clock step after the alignment,
/// and a drift between a publisher's audio and video clocks over a publish,
/// are not followed. An edge's own WHIP output sends reports taken at its
/// write times, not its source's PTS, so a publish from one is lined up on
/// when it wrote each track: the first-frame outlier goes, its steady encode
/// skew stays. Those reports also scatter with its source's mux timing
/// ([`AV_SYNC_AGREE_90K`]): the step waits for two that agree, lands within
/// their scatter, and a publish that never sees two agree is reported
/// unsettled and keeps the first-arrival alignment.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Default)]
struct WhipClock {
    /// When the publish's first frame of any track arrived.
    origin: Option<Instant>,
    video: Option<WhipTrack>,
    audio: Option<WhipTrack>,
    sync: AvSync,
}

#[cfg(any(feature = "webrtc", test))]
impl WhipClock {
    /// The PTS (90 kHz, 33 bits) of a frame of the video or audio track: its
    /// RTP timestamp `rtp` (unwrapped) on a `rate` Hz clock, arrived at
    /// `arrival`, `sr` the track's latest usable sender report; `None` for an
    /// audio frame a backward lip-sync step left starting before the end of
    /// the last audio frame sent ([`AvSync::drop_until`]).
    fn pts_90k(
        &mut self,
        video: bool,
        rtp: u64,
        rate: u64,
        arrival: Instant,
        sr: Option<SenderClock>,
    ) -> Option<u64> {
        let origin = *self.origin.get_or_insert(arrival);
        let slot = if video { &mut self.video } else { &mut self.audio };
        let track = slot.get_or_insert_with(|| {
            let late_us = arrival.saturating_duration_since(origin).as_micros() as u64;
            WhipTrack {
                first: rtp,
                base: (crate::engine::rtmp::ts_mux::PCR_LEAD_90K + late_us * 9 / 100) as i128,
                sr: None,
                last: None,
            }
        });
        if sr.is_some() {
            track.sr = sr;
        }
        let mut pts = track.pts(rtp, rate);
        if !video {
            if !self.sync.done {
                self.sync.sample_margin(pts, arrival);
                if let Some(step) = self.judge(rtp, rate, arrival, pts) {
                    pts += step;
                }
            }
            if let Some(until) = self.sync.drop_until {
                if pts <= until {
                    return None;
                }
                self.sync.drop_until = None;
            }
            self.sync.last_audio_pts = Some(self.sync.last_audio_pts.map_or(pts, |last| last.max(pts)));
        }
        let slot = if video { &mut self.video } else { &mut self.audio };
        if let Some(track) = slot {
            track.last = Some((rtp, rate, pts));
        }
        Some((pts.max(0) as u64) & 0x1_FFFF_FFFF)
    }

    /// Whether the lip-sync is still to be settled (it wants the PCR): from
    /// the publish's first audio frame until the step is taken or given up.
    /// Only an audio frame settles it, so a publish whose audio never comes
    /// would want it for good; one whose audio stops inside the window still
    /// reads the PCR off every video frame (its PES's first packet: cheap).
    fn syncing(&self) -> bool {
        !self.sync.done && self.audio.is_some()
    }

    /// The last video frame, arrived `at`, went out with PCR `pcr_90k`.
    fn note_pcr(&mut self, pcr_90k: u64, at: Instant) {
        self.sync.pcr = Some((pcr_90k, at));
    }

    /// Judge the sender reports at an audio frame — RTP time `rtp` on a
    /// `rate` Hz clock, stamped `pts` on the audio's timeline as it stands,
    /// arrived `at` — when either has changed since the last judgement, and
    /// move the audio when two pairs agree. Returns the move made.
    fn judge(&mut self, rtp: u64, rate: u64, at: Instant, pts: i128) -> Option<i128> {
        let opened = *self.sync.first_audio.get_or_insert(at);
        if at.saturating_duration_since(opened) >= AV_SYNC_WINDOW {
            self.close_window();
            return None;
        }
        let (Some(audio), Some(video)) = (self.audio.as_ref(), self.video.as_ref()) else {
            return None;
        };
        let (Some(sa), Some(sv), Some((xv, rate_v, pv))) = (audio.sr, video.sr, video.last) else {
            return None;
        };
        if self.sync.judged == Some((sa, sv)) {
            return None;
        }
        self.sync.judged = Some((sa, sv));
        let (Some(oa), Some(ov)) = (sa.offset_ns(rtp, rate), sv.offset_ns(xv, rate_v)) else {
            return None;
        };
        // How far apart, on the sender's wallclock, this audio frame and the
        // video's last frame were captured: where the audio frame belongs on
        // the video's timeline, less where it is.
        let gap_ns = (sa.ntp_ns() + oa) - (sv.ntp_ns() + ov);
        let want = pv + ns_to_90k(gap_ns) - pts;
        if want.abs() > AV_SYNC_PLAUSIBLE_90K {
            if !self.sync.implausible_reported {
                self.sync.implausible_reported = true;
                self.sync.outcome = Some(SyncOutcome::Implausible { offset_90k: want as i64 });
            }
            return None;
        }
        self.sync.estimates += 1;
        let base = audio.base;
        let estimate = SyncEstimate { audio: sa, video: sv, base: base + want };
        let agreed = self
            .sync
            .recent
            .iter()
            .flatten()
            .find(|e| e.audio != sa && e.video != sv && (e.base - estimate.base).abs() <= AV_SYNC_AGREE_90K)
            .map(|e| (e.base + estimate.base) / 2);
        let (Some(target), Some((low, high))) = (agreed, self.sync.margins.range()) else {
            self.sync.remember(estimate);
            return None;
        };
        // The move, kept inside the PCR window; where that cannot hold
        // (a spread of leads over 0.86 s), the floor wins: an audio frame
        // behind the PCR is the underflow this exists to remove.
        let want = target - base;
        let step = want.min(AV_SYNC_PCR_CEILING_90K - high).max(AV_SYNC_PCR_FLOOR_90K - low);
        let step = if step.abs() < AV_SYNC_MIN_STEP_90K { 0 } else { step };
        let residual = want - step;
        self.sync.done = true;
        self.sync.outcome = Some(if residual.abs() < AV_SYNC_MIN_STEP_90K {
            SyncOutcome::Applied { step_90k: step as i64 }
        } else {
            SyncOutcome::Partial { step_90k: step as i64, residual_90k: residual as i64 }
        });
        if step == 0 {
            return None;
        }
        if let Some(audio) = self.audio.as_mut() {
            audio.base += step;
        }
        if step < 0 {
            // The last frame sent ends where this one began on the old
            // timeline (no later than the longest Opus packet past it): what
            // starts before that would overlap it.
            self.sync.drop_until = self
                .sync
                .last_audio_pts
                .map(|last| last.max(pts.min(last + OPUS_MAX_PACKET_90K) - 1));
        }
        Some(step)
    }

    /// The window closed with the audio not moved: say what the reports came
    /// to, and judge no more.
    fn close_window(&mut self) {
        self.sync.done = true;
        let (Some(audio), Some(_)) = (self.audio.as_ref(), self.video.as_ref()) else {
            return;
        };
        self.sync.outcome = match self.sync.recent.iter().rev().flatten().next() {
            Some(latest) => Some(SyncOutcome::Unsettled {
                offset_90k: (latest.base - audio.base) as i64,
                estimates: self.sync.estimates,
            }),
            None if self.sync.implausible_reported => None,
            None => Some(SyncOutcome::NoReports),
        };
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
        let mut pts = |video: bool, rtp: u64, rate: u64, at: Instant| c.pts_90k(video, rtp, rate, at, None);
        assert_eq!(pts(true, 3_000_000_000, 90_000, t0), Some(LEAD), "the first frame at the lead");
        // The audio's first frame 20 ms later, on an unrelated base.
        assert_eq!(pts(false, 123_456, 48_000, t0 + ms(20)), Some(LEAD + 1_800));
        // A second of each: a second on, each on its own clock.
        assert_eq!(pts(true, 3_000_090_000, 90_000, t0 + ms(1_000)), Some(LEAD + 90_000));
        assert_eq!(pts(false, 123_456 + 48_000, 48_000, t0 + ms(1_020)), Some(LEAD + 1_800 + 90_000));
        // Arrival jitter after the first frame moves nothing.
        assert_eq!(pts(false, 123_456 + 96_000, 48_000, t0 + ms(2_500)), Some(LEAD + 1_800 + 180_000));
        // An audio frame a little behind its first (a late reordered one)
        // stays on the timeline.
        assert_eq!(pts(false, 123_456 - 960, 48_000, t0 + ms(30)), Some(LEAD + 1_800 - 1_800));
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

    /// A publisher whose video the answer refused — it offered VP8 alone, with
    /// Opus, its audio m-line first — publishes its audio, laid out as the
    /// audio-only publish it is: the PMT lists the Opus alone, as PCR_PID,
    /// and the Opus carries the PCR. The refused video m-line still reaches
    /// the session as a track (`MediaAdded`), so the layout took it for
    /// video: PCR_PID named a PID nothing was sent on and no packet carried a
    /// PCR. (Its answer was not SDP until 2026-10 — the refused m-line had no
    /// format — so no such publisher got this far. With its video first it is
    /// refused: see `an_offer_the_input_cannot_serve_is_refused_at_once`.)
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_publish_whose_video_was_refused_is_laid_out_audio_only() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::engine::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
        use std::time::{Duration, Instant};

        let config: crate::config::models::WebrtcInputConfig =
            serde_json::from_value(serde_json::json!({ "public_ip": "127.0.0.1" })).unwrap();
        let (broadcast_tx, mut published) = tokio::sync::broadcast::channel(4096);
        let stats = std::sync::Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
            "flow-w".into(),
            "WHIP".into(),
            "webrtc".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
        let (events, _raised) = crate::manager::events::event_channel();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (mut transcoder, mut post) = (None, None);
                super::whip_input_loop(
                    config, "flow-w", broadcast_tx, stats, cancel, session_rx, &events,
                    &mut transcoder, &mut post, Duration::from_secs(10),
                )
                .await;
            }
        });

        // A publisher offering Opus audio, then VP8 video.
        let vp8 = str0m::Rtc::builder().clear_codecs().enable_vp8(true).enable_opus(true, false);
        let peer = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
        let mut publisher = WebrtcSession::with_rtc_config(&peer, vp8).await.unwrap();
        let (offer, pending) = publisher
            .offer_media(&[str0m::media::MediaKind::Audio, str0m::media::MediaKind::Video], true)
            .unwrap();
        let (reply, answer) = tokio::sync::oneshot::channel();
        session_tx.send(NewSessionMsg { offer_sdp: offer, reply }).await.unwrap();
        let (answer, _session, _cancel) = answer.await.unwrap().unwrap();
        publisher.apply_answer(&answer, pending).unwrap();
        let (audio_mid, audio_pt) = (publisher.audio_mid.unwrap(), {
            let mid = publisher.audio_mid.unwrap();
            publisher.get_pt(mid).expect("Opus accepted")
        });
        assert_eq!(publisher.get_pt(publisher.video_mid.unwrap()), None, "VP8 refused");
        let connected = tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(publisher.poll_event(&cancel).await, SessionEvent::Connected) {}
        })
        .await;
        assert!(connected.is_ok(), "the publisher never connected");

        // Half a second of Opus (20 ms frames, TOC 0xFC), then the TS.
        let mut ts = Vec::new();
        for k in 0..25u64 {
            let at = str0m::media::MediaTime::new(k * 960, str0m::media::Frequency::FORTY_EIGHT_KHZ);
            publisher.write_media(audio_mid, audio_pt, Instant::now(), at, &[0xFC; 80]).unwrap();
            publisher.drain_outputs().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = publisher.drive_udp_io().await;
            while let Ok(pkt) = published.try_recv() {
                ts.push(pkt.data);
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while ts.len() < 20 && tokio::time::Instant::now() < deadline {
            if let Ok(Ok(pkt)) = tokio::time::timeout(Duration::from_millis(100), published.recv()).await {
                ts.push(pkt.data);
            }
        }
        let ts: Vec<bytes::Bytes> = ts.iter().flat_map(|c| c.chunks(188).map(bytes::Bytes::copy_from_slice).collect::<Vec<_>>()).collect();
        assert!(!ts.is_empty(), "nothing was published");
        assert_eq!(layout_of(&ts), ((0x0101, vec![(0x06, 0x0101)]), vec![0x0101]));
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// A browser publisher's RTP padding-only packets — Chromium's bandwidth
    /// probes, three every 5 s on the audio SSRC — reach the input as empty
    /// media. Each was muxed as a zero-length Opus AU (33 of 2964 on the
    /// rig, every one failed by ffmpeg) or an empty video PES. They are no
    /// access unit, and a probe ahead of the media anchors no timeline.
    #[test]
    fn a_payload_less_frame_is_no_access_unit() {
        use super::WhipMux;
        use crate::engine::rtmp::ts_mux::{PCR_LEAD_90K as LEAD, TsMuxer};
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut mux = TsMuxer::new();
        mux.set_audio_stream(0x06, Some(*b"Opus"));
        let mut w = WhipMux::new(mux);
        assert!(w.frame(false, &[], 123_456, 48_000, t0, None).is_none(), "a probe on the audio");
        assert!(w.frame(true, &[], 3_000_000_000, 90_000, t0, None).is_none(), "and on the video");
        // The media, 40 ms on: the publish's timeline opens at its first
        // frame, not at the probes.
        let at = t0 + Duration::from_millis(40);
        let (ts, pts) = w.frame(true, &[0, 0, 0, 1, 0x65, 0x88], 3_000_003_600, 90_000, at, None).unwrap();
        assert_eq!(pts, LEAD);
        assert!(!ts.is_empty());
        let (ts, pts) = w.frame(false, &[0xFC; 80], 123_456 + 1_920, 48_000, at, None).unwrap();
        assert_eq!(pts, LEAD);
        // The Opus AU carries its 80 bytes: control header 0x7F 0xE0, then
        // au_size.
        let pkt = ts.iter().flat_map(|b| b.chunks(188)).find(|p| (p[1] & 0x40) != 0 && ((p[1] as u16 & 0x1F) << 8 | p[2] as u16) == 0x0101);
        let pkt = pkt.expect("the audio PES");
        let es = crate::engine::ts_parse::pes_payload_offset(pkt).expect("a PES header");
        assert_eq!(&pkt[es..es + 3], &[0x7F, 0xE0, 80]);
        assert!(w.frame(false, &[], 123_456 + 3_840, 48_000, at, None).is_none(), "a probe mid-stream");
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

    /// A publisher whose offer is answered and which then never connects —
    /// not one STUN check — holds the WHIP input only until the setup
    /// deadline, with a Warning; the next publisher is answered. The input
    /// takes one publisher at a time, and it used to wait on such a one for
    /// good: every later POST hung unanswered until the flow restarted.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_publisher_that_never_connects_frees_the_input() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::manager::events::EventSeverity;
        use std::time::Duration;
        const CHROME_WHIP_OFFER: &str = include_str!("webrtc/testdata/chrome124-whip-sendonly.sdp");

        let config: crate::config::models::WebrtcInputConfig =
            serde_json::from_value(serde_json::json!({ "public_ip": "127.0.0.1" })).unwrap();
        let (broadcast_tx, _keep) = tokio::sync::broadcast::channel(16);
        let stats = std::sync::Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
            "flow-w".into(),
            "WHIP".into(),
            "webrtc".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
        let (events, mut raised) = crate::manager::events::event_channel();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (mut transcoder, mut post) = (None, None);
                super::whip_input_loop(
                    config, "flow-w", broadcast_tx, stats, cancel, session_rx, &events,
                    &mut transcoder, &mut post, Duration::from_secs(1),
                )
                .await;
            }
        });

        // The phantom: its Chrome peer is nowhere, so nothing ever reaches
        // the session.
        let offer = |reply| NewSessionMsg { offer_sdp: CHROME_WHIP_OFFER.to_string(), reply };
        let (reply, answer) = tokio::sync::oneshot::channel();
        session_tx.send(offer(reply)).await.unwrap();
        assert!(answer.await.unwrap().is_ok(), "the phantom is answered");

        // The next publisher is answered once the phantom's second is up.
        let (reply, answer) = tokio::sync::oneshot::channel();
        session_tx.send(offer(reply)).await.unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(5), answer).await;
        assert!(matches!(answered, Ok(Ok(Ok(_)))), "the next publisher's POST went unanswered");

        let mut timeouts = Vec::new();
        while let Ok(ev) = raised.try_recv() {
            if let Some(d) = ev.details.filter(|d| d["error_code"] == "webrtc_setup_timeout") {
                assert_eq!(ev.severity, EventSeverity::Warning);
                assert_eq!(ev.flow_id.as_deref(), Some("flow-w"));
                timeouts.push(d);
            }
        }
        assert_eq!(timeouts.len(), 1, "{timeouts:?}");
        assert_eq!(timeouts[0]["peer"], "WHIP publisher");
        assert_eq!(timeouts[0]["timeout_secs"], 1);
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// An offer the input cannot serve is refused at answer time — an error
    /// its request is answered 400 on, with the reason — and the next
    /// publisher is answered at once. Both real Chrome 124 publishes used to
    /// be answered 201 with something Chrome could not apply, and the input
    /// was then held for the whole setup deadline:
    ///
    /// * VP8 video alone: nothing this input takes (the answer's BUNDLE line
    ///   was left empty, which Chrome rejects);
    /// * VP8 video, then Opus: the video m-line is the BUNDLE tag, which an
    ///   answer may not reject (RFC 8843 §7.3.3) — "Failed to setup RTCP mux".
    ///
    /// The refusal's advice follows the kind of the tag it rejects: a
    /// publisher whose G.711 audio leads its H.264 video is told to offer
    /// Opus or put its video first. It was told to offer H.264 or put the
    /// audio first, whichever m-line was refused — its audio already was.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn an_offer_the_input_cannot_serve_is_refused_at_once() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::engine::webrtc::session::{SessionConfig, WebrtcSession, offer_was_at_fault};
        use str0m::media::MediaKind;
        use std::time::Duration;
        const VP8_ONLY: &str = include_str!("webrtc/testdata/chrome124-whip-vp8-only-noaudio.sdp");
        const VP8_VIDEO_FIRST: &str =
            include_str!("webrtc/testdata/chrome124-whip-vp8-videofirst.sdp");
        const CHROME_WHIP_OFFER: &str = include_str!("webrtc/testdata/chrome124-whip-sendonly.sdp");

        let config: crate::config::models::WebrtcInputConfig =
            serde_json::from_value(serde_json::json!({ "public_ip": "127.0.0.1" })).unwrap();
        let (broadcast_tx, _keep) = tokio::sync::broadcast::channel(16);
        let stats = std::sync::Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
            "flow-w".into(),
            "WHIP".into(),
            "webrtc".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
        let (events, _raised) = crate::manager::events::event_channel();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (mut transcoder, mut post) = (None, None);
                super::whip_input_loop(
                    config,
                    "flow-w",
                    broadcast_tx,
                    stats,
                    cancel,
                    session_rx,
                    &events,
                    &mut transcoder,
                    &mut post,
                    Duration::from_secs(30),
                )
                .await;
            }
        });
        let offer = |sdp: &str| {
            let (reply, answer) = tokio::sync::oneshot::channel();
            (
                NewSessionMsg {
                    offer_sdp: sdp.to_string(),
                    reply,
                },
                answer,
            )
        };

        // A publisher offering G.711 audio first, then H.264 video.
        let g711 = str0m::Rtc::builder().clear_codecs().enable_pcmu(true, false).enable_h264(true);
        let peer = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
        let g711_first = WebrtcSession::with_rtc_config(&peer, g711)
            .await
            .unwrap()
            .offer_media(&[MediaKind::Audio, MediaKind::Video], true)
            .unwrap()
            .0;

        for (sdp, reasons) in [
            (VP8_ONLY, &["no H.264 video and no Opus audio"][..]),
            (
                VP8_VIDEO_FIRST,
                &["BUNDLE tag (mid 0)", "offer H.264, or put the audio m-line first"][..],
            ),
            (
                g711_first.as_str(),
                &["BUNDLE tag (mid ", "offer Opus, or put the video m-line first"][..],
            ),
        ] {
            let (msg, answer) = offer(sdp);
            session_tx.send(msg).await.unwrap();
            let err = tokio::time::timeout(Duration::from_secs(5), answer)
                .await
                .expect("no reply")
                .unwrap()
                .expect_err("this publish cannot be served");
            assert!(offer_was_at_fault(&err), "{err}");
            for reason in reasons {
                assert!(err.to_string().contains(reason), "{err}");
            }
        }

        // The next publisher — a real one — is answered at once, not after
        // the 30 s the refused ones would have held the input for.
        let (msg, answer) = offer(CHROME_WHIP_OFFER);
        session_tx.send(msg).await.unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(5), answer).await;
        assert!(
            matches!(answered, Ok(Ok(Ok(_)))),
            "the next publisher's POST went unanswered"
        );
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// A WHEP server that accepts each pull and closes the session as it
    /// connects (DTLS close_notify) is pulled from again only after the
    /// backoff — 1 s, then doubling, as for any other failed attempt. The
    /// input re-POSTed it back to back: the backoff was reset as soon as the
    /// answer applied, and nothing slept before a reconnect.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_server_that_closes_each_pull_is_backed_off_from() {
        use crate::engine::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
        use axum::http::{StatusCode, header};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        // What `main` installs before any TLS client is built.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let posts = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/whep",
            axum::routing::post({
                let posts = posts.clone();
                move |offer: String| async move {
                    let n = posts.fetch_add(1, Ordering::SeqCst);
                    let config = SessionConfig {
                        bind_addr: "127.0.0.1:0".parse().unwrap(),
                        public_ip: None,
                        ice_lite: true,
                    };
                    let mut server = WebrtcSession::new(&config).await.unwrap();
                    let answer = server.accept_offer(&offer).unwrap();
                    tokio::spawn(async move {
                        let cancel = CancellationToken::new();
                        let _ = tokio::time::timeout(Duration::from_secs(10), async {
                            loop {
                                match server.poll_event(&cancel).await {
                                    SessionEvent::Connected => server.close().await,
                                    SessionEvent::Disconnected => break,
                                    _ => {}
                                }
                            }
                        })
                        .await;
                    });
                    (
                        StatusCode::CREATED,
                        [
                            (header::LOCATION, format!("/whep/res-{n}")),
                            (header::CONTENT_TYPE, "application/sdp".to_string()),
                        ],
                        answer,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let config: crate::config::models::WhepInputConfig =
            serde_json::from_value(serde_json::json!({ "whep_url": format!("http://{addr}/whep") }))
                .unwrap();
        let (broadcast_tx, _keep) = tokio::sync::broadcast::channel(16);
        let stats = Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
            "flow-w".into(),
            "WHEP".into(),
            "webrtc".into(),
        ));
        let cancel = CancellationToken::new();
        let (events, mut raised) = crate::manager::events::event_channel();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (mut transcoder, mut post) = (None, None);
                super::whep_input_loop(
                    config, broadcast_tx, stats, cancel, &events, "flow-w", &mut transcoder, &mut post,
                )
                .await;
            }
        });

        // Pulls at about 0, 1 and 3 s; the fourth would be at 7 s.
        tokio::time::sleep(Duration::from_millis(4_500)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        server.abort();

        let posts = posts.load(Ordering::SeqCst);
        assert!((2..=3).contains(&posts), "{posts} pulls in 4.5 s");
        // The pulls connected, and were then closed: the path that had no
        // backoff at all, not only a failed handshake.
        let mut lost = 0;
        while let Ok(ev) = raised.try_recv() {
            lost += usize::from(ev.message == "WHEP disconnected");
        }
        assert!(lost >= 1, "no pull was closed after it connected");
    }
}

/// The WHIP input's lip-sync by RTCP sender reports (edge#155): the audio
/// step [`WhipClock`] takes, through [`WhipMux`] and the TS it writes.
///
/// The publisher is modelled on one capture clock: capture instant `c` (ms)
/// is RTP `V0 + 90 c` on its video and `a0 + 48 c` on its audio, and NTP
/// `ntp(c)`; its reports map each track onto that clock unless a test skews
/// them. Each frame carries its track's latest report by its arrival.
#[cfg(test)]
mod av_sync_tests {
    use super::*;
    use crate::engine::rtmp::ts_mux::{PCR_LEAD_90K, TsMuxer};
    use crate::engine::ts_parse::{extract_pes_pts, ts_pid, ts_pusi};

    const LEAD: i128 = PCR_LEAD_90K as i128;
    const V0: u64 = 3_000_000_000;
    const A0: u64 = 123_456;

    /// The publisher's NTP time at capture instant `ms`.
    fn ntp(ms: i64) -> SystemTime {
        let epoch = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        if ms >= 0 {
            epoch + Duration::from_millis(ms as u64)
        } else {
            epoch - Duration::from_millis(ms.unsigned_abs())
        }
    }

    /// The video's report taken at capture instant `at`.
    fn video_report(at: u64) -> Option<SenderClock> {
        SenderClock::from_report(ntp(at as i64), V0 + at * 90, 100)
    }

    /// The audio's report taken at capture instant `at`, its NTP time
    /// `skew` ms off the clock.
    fn audio_report(a0: u64, at: u64, skew: i64) -> Option<SenderClock> {
        SenderClock::from_report(ntp(at as i64 + skew), a0 + at * 48, 100)
    }

    /// The latest of a report taken at `start` and every `every` ms after,
    /// by `arrive`.
    fn latest(arrive: u64, start: u64, every: u64) -> Option<u64> {
        (arrive >= start).then(|| start + (arrive - start) / every * every)
    }

    type Reports = Box<dyn Fn(u64) -> (Option<SenderClock>, Option<SenderClock>)>;

    /// Reports from `start`: the audio's every second, `skew(at)` off, the
    /// video's every half second.
    fn reports(start: u64, a0: u64, skew: impl Fn(u64) -> i64 + 'static) -> Reports {
        Box::new(move |arrive| {
            (
                latest(arrive, start, 1_000).and_then(|at| audio_report(a0, at, skew(at))),
                latest(arrive, start, 500).and_then(video_report),
            )
        })
    }

    /// Reports taken at the instants given, on the clock.
    fn reports_at(audio: &'static [u64], video: &'static [u64]) -> Reports {
        Box::new(move |arrive| {
            let latest = |at: &[u64]| at.iter().copied().filter(|t| *t <= arrive).max();
            (latest(audio).and_then(|at| audio_report(A0, at, 0)), latest(video).and_then(video_report))
        })
    }

    /// One frame as the mux handled it: `pts` is `None` for one it dropped.
    #[derive(Debug, Clone, Copy)]
    struct Sent {
        video: bool,
        capture: u64,
        arrive: u64,
        pts: Option<i128>,
    }

    struct Sim {
        mux: WhipMux,
        t0: Instant,
        a0: u64,
        frames: Vec<Sent>,
        /// What the lip-sync came to, and at which frame's arrival (ms).
        outcomes: Vec<(u64, SyncOutcome)>,
        /// Each audio frame sent once there was a PCR: its arrival (ms) and
        /// its lead over the PCR then — the last video frame's PCR as the
        /// muxer wrote it, run on by the time since that frame arrived.
        leads: Vec<(u64, i128)>,
        pcr: Option<(u64, u64)>,
        /// The audio's TS, in order.
        audio_ts: Vec<bytes::Bytes>,
    }

    impl Sim {
        fn new(a0: u64) -> Self {
            let mut muxer = TsMuxer::new();
            muxer.set_audio_stream(0x06, Some(*b"Opus"));
            Self {
                mux: WhipMux::new(muxer),
                t0: Instant::now(),
                a0,
                frames: Vec::new(),
                outcomes: Vec::new(),
                leads: Vec::new(),
                pcr: None,
                audio_ts: Vec::new(),
            }
        }

        fn frame(&mut self, video: bool, capture: u64, arrive: u64, sr: Option<SenderClock>) {
            const IDR: [u8; 6] = [0, 0, 0, 1, 0x65, 0x88];
            let at = self.t0 + Duration::from_millis(arrive);
            let (rtp, rate, data): (u64, u64, &[u8]) = if video {
                (V0 + capture * 90, 90_000, &IDR)
            } else {
                (self.a0 + capture * 48, 48_000, &[0xFC; 80])
            };
            let out = self.mux.frame(video, data, rtp, rate, at, sr);
            if let Some(outcome) = self.mux.take_sync_outcome() {
                self.outcomes.push((arrive, outcome));
            }
            let pts = out.as_ref().map(|(_, pts)| *pts as i128);
            if let Some((ts, pts)) = out {
                if video {
                    self.pcr = frame_pcr_90k(&ts).map(|pcr| (pcr, arrive));
                } else {
                    if let Some((pcr, pcr_at)) = self.pcr {
                        let now = pcr as i128 + (arrive - pcr_at) as i128 * 90;
                        self.leads.push((arrive, pts as i128 - now));
                    }
                    self.audio_ts.extend(ts);
                }
            }
            self.frames.push(Sent { video, capture, arrive, pts });
        }

        fn audio(&self) -> impl Iterator<Item = &Sent> {
            self.frames.iter().filter(|f| !f.video)
        }

        fn pts(&self, video: bool, capture: u64) -> Option<i128> {
            self.frames.iter().find(|f| f.video == video && f.capture == capture).and_then(|f| f.pts)
        }

        /// The audio PES PTS on the wire, in order.
        fn audio_pes_pts(&self) -> Vec<u64> {
            self.audio_ts
                .iter()
                .flat_map(|b| b.chunks(188))
                .filter(|p| ts_pid(p) == 0x0101 && ts_pusi(p))
                .filter_map(extract_pes_pts)
                .collect()
        }

        /// The audio frames' leads over the PCR from `from` (ms) on.
        fn leads_from(&self, from: u64) -> impl Iterator<Item = i128> {
            self.leads.iter().filter(move |(at, _)| *at >= from).map(|(_, lead)| *lead)
        }
    }

    /// A publish of `secs` s: video every 40 ms, captured at `c` and arriving
    /// at `video_at(c)` (ms), audio every 20 ms arriving at `audio_at(c)`;
    /// each frame carries its track's report of `reports(arrival)`. Frames
    /// arriving together go video first, then by capture.
    fn publish(
        secs: u64,
        a0: u64,
        video_at: impl Fn(u64) -> u64,
        audio_at: impl Fn(u64) -> u64,
        reports: Reports,
    ) -> Sim {
        let mut order: Vec<(u64, bool, u64)> = Vec::new();
        for c in (0..secs * 1_000).step_by(40) {
            order.push((video_at(c), false, c));
        }
        for c in (0..secs * 1_000).step_by(20) {
            order.push((audio_at(c), true, c));
        }
        order.sort();
        let mut sim = Sim::new(a0);
        for (arrive, audio, capture) in order {
            let (ra, rv) = reports(arrive);
            sim.frame(!audio, capture, arrive, if audio { ra } else { rv });
        }
        sim
    }

    /// The reported defect: the video's first frame 330 ms later than its
    /// steady state (as from a sender that opens its encoder lazily), so
    /// anchored on first arrival the audio sat 230 ms *behind* the PCR for
    /// the whole publish (the issue measured −228 ms p50). The reports move
    /// it once, 330 ms later, onto the video's clock: audio and video
    /// captured at one instant then carry one PTS, and the audio leads the
    /// PCR by the muxer's 100 ms. The step comes at the first two report
    /// pairs sharing neither report (the video's 1.5 s report rides its
    /// frame at 1.52 s); it is forward, so nothing is dropped, and the audio
    /// runs on 20 ms a frame either side of it.
    #[test]
    fn sender_reports_line_audio_up_with_video() {
        let sim = publish(6, A0, |c| c.max(330), |c| c, reports(500, A0, |_| 0));
        assert_eq!(sim.outcomes, vec![(1_520, SyncOutcome::Applied { step_90k: 29_700 })]);
        for f in sim.audio() {
            let moved = if f.arrive < 1_520 { 0 } else { 29_700 };
            assert_eq!(f.pts, Some(LEAD + moved + f.capture as i128 * 90), "{f:?}");
        }
        for c in (1_520..6_000).step_by(40) {
            assert_eq!(sim.pts(false, c), sim.pts(true, c), "audio and video captured at {c} ms");
        }
        let steps: Vec<i128> = sim.audio().map(|f| f.pts.unwrap()).collect::<Vec<_>>().windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(steps.iter().filter(|s| **s != 1_800).count(), 1, "one step");
        assert!(steps.contains(&(1_800 + 29_700)));
        assert!(sim.leads_from(400).take(10).all(|l| l < -18_000), "behind the PCR before");
        assert!(sim.leads_from(1_520).all(|l| l == LEAD), "the muxer's lead after");
    }

    /// The audio's first frames 300 ms late: the reports move it back. The
    /// overlap — what was already sent past the new timeline — is dropped,
    /// not sent: the muxer would have stamped each such frame one tick past
    /// the last PTS (`TsMuxer::clamp_audio_pts`), 300 ms of audio bunched at
    /// one instant. The reports start late enough that the startup burst is
    /// out of the PCR window by then.
    #[test]
    fn a_backward_step_drops_the_overlap_and_never_bunches() {
        let sim = publish(6, A0, |c| c, |c| c.max(300), reports(2_500, A0, |_| 0));
        assert_eq!(sim.outcomes, vec![(3_520, SyncOutcome::Applied { step_90k: -27_000 })]);
        let dropped: Vec<u64> = sim.audio().filter(|f| f.pts.is_none()).map(|f| f.capture).collect();
        assert_eq!(dropped, (3_520..=3_800).step_by(20).collect::<Vec<_>>(), "300 ms dropped");
        for f in sim.audio().filter(|f| f.pts.is_some()) {
            let moved = if f.arrive < 3_520 { 27_000 } else { 0 };
            assert_eq!(f.pts, Some(LEAD + moved + f.capture as i128 * 90), "{f:?}");
        }
        assert_eq!(sim.pts(false, 4_000), sim.pts(true, 4_000));
        let wire = sim.audio_pes_pts();
        assert_eq!(wire.len(), sim.audio().filter(|f| f.pts.is_some()).count());
        assert!(wire.windows(2).all(|w| w[1] >= w[0] + 1_800), "an audio PES bunched on the wire");

        // A step of no whole number of frames — the reports put the audio 7 ms
        // later than it was captured, 293 ms back: the frame that would start
        // 7 ms into the last one sent, overlapping its last 13 ms, is dropped
        // too, leaving a 7 ms gap rather than two frames overlapping.
        let sim = publish(6, A0, |c| c, |c| c.max(300), reports(2_500, A0, |_| 7));
        assert_eq!(sim.outcomes, vec![(3_520, SyncOutcome::Applied { step_90k: -26_370 })]);
        let dropped: Vec<u64> = sim.audio().filter(|f| f.pts.is_none()).map(|f| f.capture).collect();
        assert_eq!(dropped, (3_520..=3_800).step_by(20).collect::<Vec<_>>());
        for f in sim.audio().filter(|f| f.pts.is_some()) {
            let moved = if f.arrive < 3_520 { 27_000 } else { 630 };
            assert_eq!(f.pts, Some(LEAD + moved + f.capture as i128 * 90), "{f:?}");
        }
        let wire = sim.audio_pes_pts();
        let steps: Vec<u64> = wire.windows(2).map(|w| w[1] - w[0]).filter(|s| *s != 1_800).collect();
        assert_eq!(steps, vec![1_800 + 630], "one 7 ms gap, no overlap");
    }

    /// A publisher whose audio lags its video by 200 ms: lined up in full,
    /// its audio would be 100 ms behind the PCR. It is moved back only as
    /// far as leaves every frame 40 ms ahead of it, 60 ms, and the 140 ms
    /// left reported. One whose video lags by a second: lined up in full,
    /// the audio would lead the PCR by 1.1 s, past what an AU may wait in
    /// the decoder; it is moved 0.8 s, to lead it by 0.9 s.
    #[test]
    fn the_step_keeps_the_audio_inside_the_pcr_window() {
        let sim = publish(6, A0, |c| c, |c| c + 200, reports(500, A0, |_| 0));
        assert_eq!(
            sim.outcomes,
            vec![(1_520, SyncOutcome::Partial { step_90k: -5_400, residual_90k: -12_600 })]
        );
        assert!(sim.leads_from(1_520).all(|l| l == AV_SYNC_PCR_FLOOR_90K), "lined up as far as the PCR floor");
        assert_eq!(sim.audio().filter(|f| f.pts.is_none()).count(), 3, "the 60 ms overlap dropped");

        let sim = publish(6, A0, |c| c + 1_000, |c| c, reports(500, A0, |_| 0));
        assert_eq!(
            sim.outcomes,
            vec![(2_000, SyncOutcome::Partial { step_90k: 72_000, residual_90k: 18_000 })]
        );
        assert!(sim.leads_from(2_000).all(|l| l == AV_SYNC_PCR_CEILING_90K), "lined up as far as the ceiling");
    }

    /// Audio written five frames at a time, a burst every 100 ms (an edge
    /// sender re-encoding 96 ms AC-3 PES into Opus): a burst's first frame
    /// leads the PCR by 80 ms less than its last. The PCR floor is taken from
    /// the least lead over the last second, so every frame of the 2 s after
    /// the step keeps 40 ms ahead of the PCR — and here the step is judged at
    /// a burst's *last* frame, which, taken alone, would have left each
    /// burst's first frames behind the PCR. Clamped, and not.
    #[test]
    fn a_burst_of_audio_stays_ahead_of_the_pcr_after_the_step() {
        // Frames 2 ms apart in a burst; the video report-bearing frames land
        // between a burst's fourth and fifth.
        let burst = |lag: u64| move |c: u64| c / 100 * 100 + 100 + lag + c % 100 / 10;
        let sim = publish(4, A0, |c| c + 7, burst(140), reports_at(&[300, 1_000], &[447, 1_247]));
        assert_eq!(
            sim.outcomes,
            vec![(1_248, SyncOutcome::Partial { step_90k: -5_400, residual_90k: -15_570 })]
        );
        let after: Vec<i128> = sim.leads.iter().filter(|(at, _)| (1_248..3_248).contains(at)).map(|(_, l)| *l).collect();
        assert!(after.len() > 90);
        assert!(after.iter().all(|l| *l >= AV_SYNC_PCR_FLOOR_90K), "{after:?}");
        assert_eq!(after.iter().min(), Some(&AV_SYNC_PCR_FLOOR_90K));

        // The video 100 ms behind its capture, its first frame 337 ms later
        // still: the audio moved 337 ms, unclamped.
        let sim = publish(4, A0, |c| (c + 107).max(437), burst(0), reports_at(&[300, 1_000], &[707, 1_507]));
        assert_eq!(sim.outcomes, vec![(1_508, SyncOutcome::Applied { step_90k: 30_330 })]);
        let after: Vec<i128> = sim.leads.iter().filter(|(at, _)| (1_508..3_508).contains(at)).map(|(_, l)| *l).collect();
        assert!(after.len() > 90);
        assert!(after.iter().all(|l| *l >= AV_SYNC_PCR_FLOOR_90K), "{after:?}");
        assert_eq!(sim.pts(false, 2_000), sim.pts(true, 2_000));
    }

    /// Reports that cannot map a track onto a wallclock move nothing, and a
    /// usable pair still lines the audio up afterwards: sent before any
    /// media (ffmpeg's first, str0m's before its first write — packet count
    /// 0), with NTP 0 (no wallclock: str0m reads it as exactly the Unix
    /// epoch, so a check for "before the epoch" never fires), more than 15 s
    /// from the frame (a stale one, or the RTP 0 a str0m sender reports when
    /// its last write's wallclock is in the future), or moving the audio more
    /// than 2 s.
    #[test]
    fn an_unusable_sender_report_is_ignored() {
        let at = ntp(1_000);
        assert_eq!(SenderClock::from_report(at, A0, 0), None, "sent before any media");
        assert_eq!(SenderClock::from_report(UNIX_EPOCH, A0, 100), None, "NTP 0, as str0m reads it");
        assert_eq!(SenderClock::from_report(UNIX_EPOCH + Duration::from_secs(86_400), A0, 100), None);
        assert_eq!(SenderClock::from_report(UNIX_EPOCH + SR_NTP_FLOOR, A0, 100), None);
        let sr = SenderClock::from_report(UNIX_EPOCH + SR_NTP_FLOOR + Duration::from_secs(1), A0, 100).unwrap();
        assert_eq!(sr.offset_ns(A0 + 15 * 48_000, 48_000), Some(15_000_000_000));
        assert_eq!(sr.offset_ns(A0 + 15 * 48_000 + 1, 48_000), None, "more than 15 s on");
        assert_eq!(sr.offset_ns(A0.wrapping_sub(15 * 48_000 + 1), 48_000), None, "more than 15 s back");
        assert_eq!(sr.offset_ns(A0.wrapping_sub(15 * 48_000), 48_000), Some(-15_000_000_000));
        let future = SenderClock::from_report(at, 0, 100).expect("a report in form");
        assert_eq!(future.offset_ns(V0, 90_000), None, "RTP 0 against a track on a random base");

        // Through the clock, the reported defect's publish: for 2 s NTP 0 on
        // the audio's reports with live RTP, and a packet count of 0 on the
        // video's; then audio reports 20 s from their frames, then ones 3 s
        // off, then good ones.
        let reports: Reports = Box::new(|arrive| {
            let audio = latest(arrive, 500, 1_000).and_then(|at| match at {
                ..2_000 => SenderClock::from_report(UNIX_EPOCH, A0 + at * 48, 100),
                2_000..4_000 => SenderClock::from_report(ntp(at as i64 - 20_000), (A0 + at * 48).wrapping_sub(20 * 48_000), 100),
                4_000..6_000 => audio_report(A0, at, 3_000),
                _ => audio_report(A0, at, 0),
            });
            let video = latest(arrive, 500, 500).and_then(|at| {
                if at < 2_000 { SenderClock::from_report(ntp(at as i64), V0 + at * 90, 0) } else { video_report(at) }
            });
            (audio, video)
        });
        let sim = publish(9, A0, |c| c.max(330), |c| c, reports);
        assert_eq!(
            sim.outcomes,
            vec![
                (4_500, SyncOutcome::Implausible { offset_90k: 299_700 }),
                (7_520, SyncOutcome::Applied { step_90k: 29_700 }),
            ]
        );
        assert!(sim.audio().filter(|f| f.arrive < 7_520).all(|f| f.pts == Some(LEAD + f.capture as i128 * 90)));
        assert_eq!(sim.pts(false, 8_000), sim.pts(true, 8_000));
    }

    /// str0m's own reading of a report with NTP 0 — the one RFC 3550 lets a
    /// sender without a wallclock send — is the Unix epoch itself (every NTP
    /// time before 1970 is), and it is refused; a real one is not.
    #[cfg(feature = "webrtc")]
    #[test]
    fn str0m_reads_ntp_0_as_the_epoch_and_it_is_refused() {
        let report = |ntp: u64| {
            let mut body = Vec::new();
            body.extend_from_slice(&0x1234_5678u32.to_be_bytes());
            body.extend_from_slice(&ntp.to_be_bytes());
            body.extend_from_slice(&96_000u32.to_be_bytes());
            body.extend_from_slice(&100u32.to_be_bytes());
            body.extend_from_slice(&16_000u32.to_be_bytes());
            str0m::rtp::rtcp::SenderInfo::try_from(&body[..]).unwrap()
        };
        for ntp in [0, 1 << 32, 2_208_988_799 << 32] {
            let info = report(ntp);
            assert_eq!(info.ntp_time, UNIX_EPOCH, "NTP {ntp:#x}");
            let clock = SenderClock::from_report(info.ntp_time, info.rtp_time.numer(), info.sender_packet_count);
            assert_eq!(clock, None);
        }
        // 2026-10-29: NTP seconds 3_970_771_200.
        let info = report(3_970_771_200 << 32);
        assert!(SenderClock::from_report(info.ntp_time, info.rtp_time.numer(), info.sender_packet_count).is_some());
    }

    /// A report's RTP time is read on its low 32 bits, which it and the frame
    /// both carry: str0m extends the two apart (a report's from the previous
    /// report's, a frame's from the packets'), so across the 32-bit wrap one
    /// may be extended and the other not.
    #[test]
    fn a_report_across_the_rtp_wrap_maps_correctly() {
        let sr = SenderClock::from_report(ntp(0), u32::MAX as u64 - 959, 100).unwrap();
        assert_eq!(sr.offset_ns((1 << 32) + 2_880, 48_000), Some(80_000_000));
        let sr = SenderClock::from_report(ntp(0), (1 << 32) + 100, 100).unwrap();
        assert_eq!(sr.offset_ns(u32::MAX as u64 - 99, 48_000), Some(-200 * 1_000_000_000 / 48_000));

        // The reported defect's publish with the audio's RTP clock wrapping
        // a second in: the frames carry it extended, the reports not.
        let a0 = u32::MAX as u64 - 47_999;
        let reports: Reports = Box::new(move |arrive| {
            (
                latest(arrive, 500, 1_000).and_then(|at| SenderClock::from_report(ntp(at as i64), (a0 + at * 48) as u32 as u64, 100)),
                latest(arrive, 500, 500).and_then(video_report),
            )
        });
        let sim = publish(4, a0, |c| c.max(330), |c| c, reports);
        assert_eq!(sim.outcomes, vec![(1_520, SyncOutcome::Applied { step_90k: 29_700 })]);
        assert_eq!(sim.pts(false, 2_000), sim.pts(true, 2_000));
    }

    /// Reports that put the audio within 20 ms of where it is move nothing
    /// (a gap or a drop for nothing); and a publish of one track has nothing
    /// to line up, whatever its reports say.
    #[test]
    fn small_offsets_and_single_track_publishes_never_step() {
        let sim = publish(6, A0, |c| c, |c| c + 10, reports(500, A0, |_| 0));
        assert_eq!(sim.outcomes, vec![(1_530, SyncOutcome::Applied { step_90k: 0 })]);
        assert!(sim.audio().all(|f| f.pts == Some(LEAD + 900 + f.capture as i128 * 90)));

        let carried = reports(500, A0, |_| 3_000);
        let mut audio_only = Sim::new(A0);
        let mut video_only = Sim::new(A0);
        for c in (0..40_000).step_by(20) {
            let (ra, rv) = carried(c);
            audio_only.frame(false, c, c, ra);
            if c % 40 == 0 {
                video_only.frame(true, c, c, rv);
            }
        }
        assert_eq!(audio_only.outcomes, vec![]);
        assert!(audio_only.audio().all(|f| f.pts == Some(LEAD + f.capture as i128 * 90)));
        assert_eq!(video_only.outcomes, vec![]);
        assert!(!video_only.mux.clock.syncing(), "no audio: its video frames' PCR is never read");
    }

    /// One audio report 500 ms off: the pairs it is in agree with each other
    /// but with no pair it is not in, so nothing moves on it; the audio moves
    /// once, when two good pairs sharing neither report agree.
    #[test]
    fn sender_reports_must_agree_before_the_audio_moves() {
        let skew = |at: u64| if at == 500 { 500 } else { 0 };
        let sim = publish(6, A0, |c| c.max(330), |c| c, reports(500, A0, skew));
        assert_eq!(sim.outcomes, vec![(2_520, SyncOutcome::Applied { step_90k: 29_700 })]);
        assert!(sim.audio().filter(|f| f.arrive < 2_520).all(|f| f.pts == Some(LEAD + f.capture as i128 * 90)));
        assert_eq!(sim.pts(false, 3_000), sim.pts(true, 3_000));
    }

    /// The reported defect's publish from an edge's WHIP output, whose reports
    /// are taken at its write times: str0m runs each on from its last write,
    /// so each carries when that write left against its media time — the
    /// source's mux timing. The rig's AC-3 source led its PCR by 604–669 ms
    /// with its audio PES and by 664–736 ms with its video's decode time. Its
    /// first five-PES cycle of audio leads (610–664 ms) and five video leads
    /// spread over 664–736 ms are cycled here through every phase of the two;
    /// reports at str0m's cadence, the audio's every 5 s and the video's
    /// every second. A model of such reports, not a capture of them, and one
    /// without the Opus re-framing term ([`AV_SYNC_AGREE_90K`]): in it
    /// estimates scatter by up to ±64 ms, past the 20 ms two must agree
    /// within, and still every phase is moved once inside the window, to
    /// within that scatter of lip sync, with the audio kept ahead of the PCR.
    #[test]
    fn write_time_reports_settle_once_within_their_scatter() {
        const AUDIO_LEAD_MS: [i64; 5] = [610, 625, 628, 653, 664];
        const VIDEO_LEAD_MS: [i64; 5] = [664, 700, 736, 682, 718];
        const SCATTER_90K: i128 = 64 * 90;
        for phase in 0..25 {
            // A write leaves as far ahead of its media time as its PES led
            // the PCR less the source's mean lead (636 ms audio, 700 video).
            let reports: Reports = Box::new(move |arrive| {
                let audio = latest(arrive, 1_000, 5_000).and_then(|at| {
                    audio_report(A0, at, 636 - AUDIO_LEAD_MS[(at / 5_000 + phase) as usize % 5])
                });
                let video = latest(arrive, 300, 1_000).and_then(|at| {
                    let ahead = 700 - VIDEO_LEAD_MS[(at / 1_000 + phase / 5) as usize % 5];
                    SenderClock::from_report(ntp(at as i64 + ahead), V0 + at * 90, 100)
                });
                (audio, video)
            });
            let sim = publish(36, A0, |c| c.max(330), |c| c, reports);
            let &[(at, SyncOutcome::Applied { step_90k })] = &sim.outcomes[..] else {
                panic!("phase {phase}: {:?}", sim.outcomes);
            };
            assert!(at < 30_000, "phase {phase}: moved at {at} ms");
            assert!((step_90k as i128 - 29_700).abs() <= SCATTER_90K, "phase {phase}: {step_90k}");
            assert!(sim.audio().all(|f| f.pts.is_some()), "phase {phase}: a forward step drops nothing");
            let pts: Vec<i128> = sim.audio().map(|f| f.pts.unwrap()).collect();
            let steps: Vec<i128> = pts.windows(2).map(|w| w[1] - w[0]).filter(|s| *s != 1_800).collect();
            assert_eq!(steps, vec![1_800 + step_90k as i128], "phase {phase}: one step");
            assert!(sim.leads_from(at).all(|l| l >= AV_SYNC_PCR_FLOOR_90K), "phase {phase}");
        }
    }

    /// The audio is moved only within 30 s of the first audio frame. Reports
    /// that never agreed by then are reported, with the latest estimate, and
    /// nothing moves after it — not even on good reports. Reports that only
    /// ever asked for more than 2 s are reported once, as such; a publish
    /// with no usable report at all is told of in the log.
    #[test]
    fn the_audio_is_never_moved_past_the_window() {
        // Each audio report 50 ms further off than the last, then good ones
        // from 31 s.
        let skew = |at: u64| if at >= 31_000 { 0 } else { at as i64 / 1_000 * 50 };
        let sim = publish(36, A0, |c| c.max(330), |c| c, reports(500, A0, skew));
        assert_eq!(
            sim.outcomes,
            vec![(30_000, SyncOutcome::Unsettled { offset_90k: 29_700 + 1_450 * 90, estimates: 88 })]
        );
        assert!(sim.audio().all(|f| f.pts == Some(LEAD + f.capture as i128 * 90)));

        let sim = publish(36, A0, |c| c.max(330), |c| c, reports(500, A0, |_| 3_000));
        assert_eq!(sim.outcomes, vec![(520, SyncOutcome::Implausible { offset_90k: 299_700 })]);

        let sim = publish(36, A0, |c| c.max(330), |c| c, Box::new(|_| (None, None)));
        assert_eq!(sim.outcomes, vec![(30_000, SyncOutcome::NoReports)]);
    }

    /// Edge to edge on loopback, through the WHIP input: a str0m publisher
    /// (the edge's own codec set, reports every 250 ms) writes Opus and H.264
    /// captured on one clock, its video 100 ms behind its capture and its
    /// first video frame 330 ms later still. Anchored on first arrival the
    /// audio would sit 230 ms behind the PCR; the input lines it up by the
    /// publisher's reports, so in the TS it publishes audio and video
    /// captured at one instant carry one PTS (to a 48 kHz sample and a
    /// rounding: str0m truncates a report's RTP time to a sample), and the
    /// audio leads the PCR. The publisher passes its writes the capture
    /// instant as their wallclock, which str0m builds its reports from; an
    /// edge's WHIP output passes the write instant, the same thing only as
    /// far as its writes run on time
    /// (`write_time_reports_settle_once_within_their_scatter`).
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn an_av_publish_is_lip_synced_by_its_sender_reports() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::engine::ts_parse::{extract_pcr, pes_payload_offset};
        use crate::engine::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession, rtc_config};
        use str0m::media::{Frequency, MediaKind, MediaTime};

        let config: crate::config::models::WebrtcInputConfig =
            serde_json::from_value(serde_json::json!({ "public_ip": "127.0.0.1" })).unwrap();
        let (broadcast_tx, mut published) = tokio::sync::broadcast::channel(16_384);
        let stats = std::sync::Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
            "flow-s".into(),
            "WHIP".into(),
            "webrtc".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
        let (events, mut raised) = crate::manager::events::event_channel();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (mut transcoder, mut post) = (None, None);
                super::whip_input_loop(
                    config, "flow-s", broadcast_tx, stats, cancel, session_rx, &events,
                    &mut transcoder, &mut post, Duration::from_secs(10),
                )
                .await;
            }
        });

        let reporting = rtc_config(false)
            .set_rtcp_report_interval_audio(Duration::from_millis(250))
            .set_rtcp_report_interval_video(Duration::from_millis(250));
        let peer = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
        let mut publisher = WebrtcSession::with_rtc_config(&peer, reporting).await.unwrap();
        let (offer, pending) = publisher.offer_media(&[MediaKind::Video, MediaKind::Audio], true).unwrap();
        let (reply, answer) = tokio::sync::oneshot::channel();
        session_tx.send(NewSessionMsg { offer_sdp: offer, reply }).await.unwrap();
        let (answer, _session, _cancel) = answer.await.unwrap().unwrap();
        publisher.apply_answer(&answer, pending).unwrap();
        let (video_mid, audio_mid) = (publisher.video_mid.unwrap(), publisher.audio_mid.unwrap());
        let (video_pt, audio_pt) = (publisher.get_pt(video_mid).unwrap(), publisher.get_pt(audio_mid).unwrap());
        let connected = tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(publisher.poll_event(&cancel).await, SessionEvent::Connected) {}
        })
        .await;
        assert!(connected.is_ok(), "the publisher never connected");

        // Frame `n` of each track carries `n` in two bytes of its payload.
        let tag = |n: u64| [0x40 | (n >> 6) as u8, 0x40 | (n & 0x3F) as u8];
        let picture = |j: u64| {
            let mut au = vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x01, 0x41, 0xfb, 0x01, 0x10];
            au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 0, 1, 0x65, 0x88, 0x84]);
            au.extend_from_slice(&tag(j));
            au.extend(std::iter::repeat_n(0x5a, 300));
            au
        };
        let mut ts = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let capture0 = tick.tick().await.into_std();
        let mut next_picture = 0u64;
        for n in 0..200u64 {
            if n > 0 {
                tick.tick().await;
            }
            let now = n * 20;
            let mut opus = vec![0xFC];
            opus.extend_from_slice(&tag(n));
            opus.extend(std::iter::repeat_n(0x11, 60));
            let at = MediaTime::new(n * 960, Frequency::FORTY_EIGHT_KHZ);
            publisher.write_media(audio_mid, audio_pt, capture0 + Duration::from_millis(now), at, &opus).unwrap();
            publisher.drain_outputs().await;
            while now >= 430 && next_picture * 40 + 100 <= now {
                let at = MediaTime::new(next_picture * 3_600, Frequency::NINETY_KHZ);
                let captured = capture0 + Duration::from_millis(next_picture * 40);
                publisher.write_media(video_mid, video_pt, captured, at, &picture(next_picture)).unwrap();
                publisher.drain_outputs().await;
                next_picture += 1;
            }
            let _ = publisher.drive_udp_io().await;
            while let Ok(pkt) = published.try_recv() {
                ts.push(pkt.data);
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(pkt)) = tokio::time::timeout(Duration::from_millis(100), published.recv()).await {
                ts.push(pkt.data);
            }
        }
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        // Each PES's PTS by its frame number, and each audio PES's lead over
        // the last PCR before it in stream order. That PCR is not run on by
        // the time since it, as the unit sims' is (`Sim::frame`), so this
        // lead reads up to a video frame (40 ms) high: a looser bound on the
        // 40 ms floor than theirs, which this test only backs up.
        let untag = |b: &[u8]| (((b[0] & 0x3F) as u64) << 6) | (b[1] & 0x3F) as u64;
        let (mut video, mut audio, mut leads) = (Vec::new(), Vec::new(), Vec::new());
        let mut pcr = None;
        for pkt in ts.iter().flat_map(|b| b.chunks(188)) {
            if let Some(v) = extract_pcr(pkt) {
                pcr = Some(v / 300);
            }
            if !ts_pusi(pkt) {
                continue;
            }
            let (Some(pts), Some(es)) = (extract_pes_pts(pkt), pes_payload_offset(pkt)) else {
                continue;
            };
            let es = &pkt[es..];
            match ts_pid(pkt) {
                0x0100 => {
                    let at = es.windows(7).position(|w| w == [0, 0, 0, 1, 0x65, 0x88, 0x84]).expect("the IDR");
                    video.push((untag(&es[at + 7..]), pts as i128));
                }
                0x0101 => {
                    // Control header 0x7F 0xE0, au_size, then the Opus TOC.
                    let n = untag(&es[4..]);
                    audio.push((n, pts as i128));
                    if let Some(pcr) = pcr {
                        leads.push((n, signed_33(pts as i128 - pcr as i128)));
                    }
                }
                _ => {}
            }
        }
        assert!(video.len() > 80 && audio.len() > 180, "{} pictures, {} audio frames", video.len(), audio.len());
        let video_base = video[0].1 - video[0].0 as i128 * 3_600;
        assert!(video.iter().all(|(j, pts)| *pts == video_base + *j as i128 * 3_600), "the video moved");
        let off = |(n, pts): &(u64, i128)| pts - (video_base + *n as i128 * 1_800);
        // On first arrival, the audio sat 430 ms early against the picture...
        assert!(audio.iter().take(10).all(|a| off(a) < -27_000), "{:?}", &audio[..10]);
        // ... and, lined up by the reports, it carries the picture's PTS.
        let late: Vec<_> = audio.iter().filter(|(n, _)| *n >= 150).collect();
        assert!(late.len() > 40);
        assert!(late.iter().all(|a| off(a).abs() <= 3), "{:?}", late.iter().map(|a| off(a)).collect::<Vec<_>>());
        assert!(
            leads.iter().filter(|(n, _)| *n >= 150).all(|(_, lead)| *lead >= AV_SYNC_PCR_FLOOR_90K),
            "{leads:?}"
        );
        while let Ok(ev) = raised.try_recv() {
            let code = ev.details.as_ref().and_then(|d| d["error_code"].as_str()).unwrap_or_default().to_string();
            assert!(!code.starts_with("webrtc_av_sync"), "{}", ev.message);
        }
    }
}
