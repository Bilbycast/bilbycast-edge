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
use super::webrtc::session::{
    BACKOFF_RESET_AFTER, NegotiationPanic, OfferRefused, SessionConfig, SessionEvent, WebrtcSession,
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

        // The tracks' frames onto one TS (`WhipMux`): one timeline for
        // both, the parameter sets ahead of every IDR, the Opus in the
        // FFmpeg-compatible carriage, no payload-less frame.
        let mut whip = WhipMux::new(opus_ts_muxer(config.pid_overrides.as_ref()));
        let mut seq_num: u16 = 0;
        // The publish's layout — which of video and audio its offer carried
        // — is settled from the session's tracks on the first media
        // (`WhipLayout`). The muxer used to assume video and no audio: an
        // audio-only publish named an absent video PID as PCR_PID and
        // carried no PCR at all, and an A/V publish's Opus never reached the
        // PMT.
        let mut layout = WhipLayout::new("WHIP publish");
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
                SessionEvent::MediaData { mid, data, rtp_time, network_time, .. } => {
                    // Determine if this is video or audio
                    let is_video = session.video_mid == Some(mid);
                    let is_audio_stream = session.audio_mid == Some(mid);
                    if !is_video && !is_audio_stream {
                        continue;
                    }
                    let (has_video, has_audio) = tracks(&session);
                    layout.apply(&mut whip.muxer, has_video, has_audio, flow_id);
                    // Then what the publisher sends: a camera it offered
                    // and never sends leaves the PMT, the PCR moving onto
                    // the Opus (it named the silent video PID as PCR_PID,
                    // and no packet carried a PCR). A payload-less frame
                    // (a bandwidth probe) shows nothing is sent.
                    if !data.is_empty() {
                        layout.frame(&mut whip.muxer, !is_audio_stream, network_time, flow_id);
                    }
                    let Some((ts_chunks, pts_90khz)) = whip.frame(
                        !is_audio_stream,
                        &data,
                        rtp_time.numer(),
                        rtp_time.denom() as u64,
                        network_time,
                    ) else {
                        continue;
                    };
                    publish_frame(ts_chunks, pts_90khz, &mut seq_num, &stats, &broadcast_tx, transcoder, post);
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

/// The TS muxer a WebRTC input's H.264 and Opus go onto: program 1 on
/// `pid_overrides`' PIDs, and the Opus in the FFmpeg-compatible Opus-in-TS
/// carriage (stream_type 0x06 with the "Opus" registration descriptor in the
/// PMT, a control header per access unit in the private PES). WebRTC audio
/// is always Opus. Shared by the WHIP and WHEP inputs.
#[cfg(feature = "webrtc")]
fn opus_ts_muxer(
    pid_overrides: Option<&crate::config::models::TsPidOverridesMap>,
) -> crate::engine::rtmp::ts_mux::TsMuxer {
    let mut ts_muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
    if let Some(entry) = pid_overrides.and_then(|po| po.get(&1)) {
        ts_muxer.set_pids(entry.pmt_pid, entry.video_pid, entry.audio_pid, entry.pcr_pid);
    }
    ts_muxer.set_audio_stream(0x06, Some(*b"Opus"));
    ts_muxer
}

/// Publish one frame's TS packets, stamped `pts_90khz`, onto the flow —
/// seven to an item, not one (edge#166) — through the ingress transcode and
/// post-process, counted on the input's stats. Shared by the WHIP and WHEP
/// inputs.
#[cfg(feature = "webrtc")]
fn publish_frame(
    ts_chunks: Vec<bytes::Bytes>,
    pts_90khz: u64,
    seq_num: &mut u16,
    stats: &FlowStatsAccumulator,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) {
    for ts_data in crate::engine::packet::bundle_ts_packets(ts_chunks) {
        let pkt = RtpPacket {
            data: ts_data,
            sequence_number: *seq_num,
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
        *seq_num = seq_num.wrapping_add(1);
        stats.input_packets.fetch_add(1, Ordering::Relaxed);
        stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
        if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
            publish_input_packet_with_post(transcoder, post, broadcast_tx, pkt);
        } else {
            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
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
    // The unusable answer last reported (`report_whep_answer`), until a pull
    // connects.
    let mut answer_fault: Option<&'static str> = None;

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
        let (answer_sdp, resource_url) = match post_result {
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

        // Whatever becomes of this pull from here, the server holds a
        // resource for it, released once the pull is over (`whep_release`).
        let release = || whep_release(resource_url.clone(), config.bearer_token.clone(), tls.clone());

        if let Err(e) = session.apply_answer(&answer_sdp, pending) {
            tracing::error!("WHEP: failed to apply SDP answer: {}", e);
            report_negotiation_panic(&e, events, flow_id, "WHEP server");
            // A panic has just been reported; anything else was a log line
            // only, so a server answering what this input cannot apply
            // looked, from the manager, like a pull that never started.
            if e.downcast_ref::<NegotiationPanic>().is_none() {
                report_whep_answer(
                    events,
                    flow_id,
                    &mut answer_fault,
                    "not_applied",
                    format!("WHEP server's answer could not be applied: {e}"),
                    Some(e.to_string()),
                );
            }
            release();
            // Back off before retrying — a bare `continue` spins the loop.
            if !whep_back_off(&mut backoff_secs, &cancel).await {
                return;
            }
            continue;
        }

        // The tracks the answer accepted: H.264 video, Opus audio (unless
        // `video_only`, which offers none), or both. A server that took
        // neither would connect and send nothing this input reads, the flow
        // stalled on NULL keepalives for as long as the session lasted.
        let (video_pt, audio_pt) = session.answered_pts(&answer_sdp);
        let tracks = (
            session.video_mid.is_some() && video_pt.is_some(),
            session.audio_mid.is_some() && audio_pt.is_some(),
        );
        if tracks == (false, false) {
            let msg = "WHEP server's answer accepted no H.264 video and no Opus audio, \
                       the only media this input takes";
            tracing::warn!("{msg} ({}); retrying after the backoff", config.whep_url);
            report_whep_answer(events, flow_id, &mut answer_fault, "no_h264_or_opus", msg.to_string(), None);
            release();
            if !whep_back_off(&mut backoff_secs, &cancel).await {
                return;
            }
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
            release();
            // Backed off like any other failed attempt: it used to retry at
            // once, against a server whose handshake always fails, without
            // end.
            if !whep_back_off(&mut backoff_secs, &cancel).await {
                return;
            }
            continue;
        }
        let connected_at = std::time::Instant::now();
        // The answer faults of the episode this pull ends are reported again
        // should they recur.
        answer_fault = None;

        // str0m may emit MediaAdded *after* Connected. Flush any pending
        // events so video_mid is populated before media starts arriving.
        session.drain_pending_events();

        // Both tracks onto one TS as the WHIP input muxes a publish
        // (`WhipMux`), laid out from the answer and then from what the
        // server actually sends (`WhipLayout`). Only the video was muxed: the
        // Opus was dropped, and a pull of a server sending no video
        // published nothing at all.
        let mut whep = WhipMux::new(opus_ts_muxer(config.pid_overrides.as_ref()));
        let mut layout = WhipLayout::new("WHEP pull");
        let mut seq_num: u16 = 0;

        // Receive loop
        loop {
            let event = session.poll_event(&cancel).await;
            match event {
                SessionEvent::MediaData { mid, data, rtp_time, network_time, .. } => {
                    let video = session.video_mid == Some(mid);
                    if !video && session.audio_mid != Some(mid) {
                        continue;
                    }
                    layout.apply(&mut whep.muxer, tracks.0, tracks.1, flow_id);
                    // A payload-less frame (RTP padding, a sender's
                    // bandwidth probe) is no access unit (see `WhipMux`),
                    // nor evidence that its track is sent.
                    if !data.is_empty() {
                        layout.frame(&mut whep.muxer, video, network_time, flow_id);
                    }
                    let Some((ts_chunks, pts_90khz)) = whep.frame(
                        video,
                        &data,
                        rtp_time.numer(),
                        rtp_time.denom() as u64,
                        network_time,
                    ) else {
                        continue;
                    };
                    publish_frame(ts_chunks, pts_90khz, &mut seq_num, &stats, &broadcast_tx, transcoder, post);
                }
                SessionEvent::Disconnected => {
                    tracing::warn!("WHEP disconnected, reconnecting...");
                    events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHEP disconnected", flow_id);
                    break; // Reconnect
                }
                _ => {}
            }
        }

        release();
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

/// Raise the Warning for a WHEP server's answer this input cannot use —
/// `reason`: `not_applied` (it does not parse, or str0m refuses it) or
/// `no_h264_or_opus` (it accepts neither track) — once per episode: not again
/// for the same reason (`reported`) until a pull connects, since the input
/// asks again every backoff, at most 30 s apart, and a server that answers
/// so will go on answering so.
#[cfg(feature = "webrtc")]
fn report_whep_answer(
    events: &EventSender,
    flow_id: &str,
    reported: &mut Option<&'static str>,
    reason: &'static str,
    message: String,
    error: Option<String>,
) {
    if *reported == Some(reason) {
        return;
    }
    *reported = Some(reason);
    events.emit_flow_with_details(
        EventSeverity::Warning,
        category::WEBRTC,
        message,
        flow_id,
        serde_json::json!({
            "error_code": "whep_answer_unusable",
            "reason": reason,
            "error": error,
        }),
    );
}

/// Release a WHEP pull's resource on the server — a DELETE to the answer's
/// `Location` (`resource`) — in a task of its own, so neither the retry
/// behind it nor a stopping input waits on it (`delete_session` bounds it to
/// 10 s). Best effort: a failure is a log line. The input used to leave every
/// pull's resource to the server's own timeout, so a server that counts its
/// viewers, or caps them, kept a dead one for each reconnect.
#[cfg(feature = "webrtc")]
fn whep_release(
    resource: Option<String>,
    bearer_token: Option<String>,
    tls: crate::util::tls::TlsTrust,
) {
    let Some(resource) = resource else {
        return;
    };
    tokio::spawn(async move {
        match crate::engine::webrtc::signaling::delete_session(&resource, bearer_token.as_deref(), &tls)
            .await
        {
            Ok(()) => tracing::debug!("WHEP: released {resource}"),
            Err(e) => tracing::warn!("WHEP: DELETE {resource} failed: {e}"),
        }
    });
}

/// How long a track a WebRTC session negotiated may carry nothing while the
/// other track runs before it leaves the TS's layout: before its first frame
/// and after its last alike. 3 s: a server that sends a viewer video only
/// from the source's next IDR (the edge's own WHEP output, re-encoding)
/// sends its first picture up to a source GOP after the audio, and 2 s is
/// the common GOP, so a merely late first picture neither leaves the PMT
/// and comes back with a second version bump and PCR move at session start
/// (it did at 1 s), nor does a screen share whose static picture repeats
/// once a second leave between repeats. The cost is the wait: while a
/// listed video carries nothing the PCR_PID names it and no PCR goes out.
#[cfg(feature = "webrtc")]
const TRACK_ABSENT_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

/// A gap this long in the frames of the track that runs is a pause of the
/// whole session (a network outage), not the other track gone: the other's
/// absence clock starts again at the running track's next frame, so the
/// pause counts nothing against it.
#[cfg(feature = "webrtc")]
const SESSION_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

/// Which tracks a WHIP publish or a WHEP pull carries on its TS: the tracks
/// the session negotiated, then those the peer actually sends.
///
/// The negotiated tracks are applied to the muxer once, on the first media
/// ([`Self::apply`]): by then the session's tracks have all been added (a
/// publish's `SessionEvent::MediaAdded` comes out of the answer, before
/// ICE; a pull's tracks are its own offer's). A negotiated track need not
/// be sent, though: a WHEP server's answer accepts a track whether or not
/// it has anything to send on it — the edge's own WHEP output accepts H.264
/// on a flow with no video, and Opus with no Opus to send (no
/// `audio_encode`, and a source whose audio is not Opus) — and a publisher
/// may offer a camera it never sends. Nor need a track that has been sent
/// go on: a server's source may turn audio-only, a publisher may drop its
/// camera. So a listed track that carries no frame while the other runs
/// [`TRACK_ABSENT_AFTER`], before its first frame or since its last, leaves
/// the PMT (a version bump; the video leaving moves the PCR onto the audio
/// PID), and its next frame brings it back ([`Self::frame`]). Laid out from
/// the negotiation alone, a session sending audio named a silent video PID
/// as PCR_PID and carried no PCR, and one sending video listed an Opus
/// stream nothing was sent on; and a track kept once it had carried a
/// frame, as an RTMP publish's video is, left a video that stopped named as
/// PCR_PID with no PCR on any PID for the rest of the session.
#[cfg(feature = "webrtc")]
#[derive(Debug)]
struct WhipLayout {
    /// What the layout is of, for the log: `"WHIP publish"`, `"WHEP pull"`.
    session: &'static str,
    applied: bool,
    /// Per track, video then audio: negotiated by the session.
    negotiated: [bool; 2],
    /// Listed in the muxer's PMT.
    listed: [bool; 2],
    /// When the first frame of the other track arrived since the track's own
    /// last frame (or, before its first, since it was listed): its absence
    /// clock. Cleared by each of its own frames, so it runs only while the
    /// other track does.
    absent_since: [Option<std::time::Instant>; 2],
    /// When the track's last frame arrived.
    last_frame: [Option<std::time::Instant>; 2],
}

#[cfg(feature = "webrtc")]
impl WhipLayout {
    fn new(session: &'static str) -> Self {
        Self {
            session,
            applied: false,
            negotiated: [false; 2],
            listed: [false; 2],
            absent_since: [None; 2],
            last_frame: [None; 2],
        }
    }

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
        self.negotiated = [has_video, has_audio];
        self.listed = [has_video, has_audio];
        muxer.set_has_video(has_video);
        muxer.set_has_audio(has_audio);
        tracing::info!(
            flow_id,
            has_video,
            has_audio,
            "{} carries {}",
            self.session,
            match (has_video, has_audio) {
                (true, true) => "video and audio",
                (true, false) => "video only",
                (false, true) => "audio only — the PMT names the audio PID as PCR_PID",
                (false, false) => "no media track",
            }
        );
    }

    /// The session's tracks after the layout was applied: a track it now
    /// negotiates for the first time joins the PMT with a version bump — the
    /// video moving the PCR (`TsMuxer::change_has_video`), the audio too
    /// (`TsMuxer::change_has_audio`; it used to join without one, so a
    /// receiver holding the PMT by its version never saw it) — and is then
    /// held to [`TRACK_ABSENT_AFTER`] like the others.
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
        for (track, now) in [has_video, has_audio].into_iter().enumerate() {
            if !now || self.negotiated[track] {
                continue;
            }
            self.negotiated[track] = true;
            self.absent_since[track] = None;
            if self.list(muxer, track, true) {
                tracing::info!(
                    flow_id,
                    "{}: renegotiated, adding its {} track",
                    self.session,
                    TRACK_NAMES[track]
                );
            }
        }
    }

    /// A frame with a payload of the video (`video`) or the audio track,
    /// arrived at `arrival`, about to be muxed: its track back into the PMT
    /// if it had left, and the other one out once this one has run
    /// [`TRACK_ABSENT_AFTER`] without a frame of it — before its first or
    /// since its last. The clock starts at this track's first frame after
    /// the other's last, and again at its first after a pause of its own
    /// ([`SESSION_PAUSE`]), so a pause of both (a network outage) does not
    /// count.
    fn frame(
        &mut self,
        muxer: &mut crate::engine::rtmp::ts_mux::TsMuxer,
        video: bool,
        arrival: std::time::Instant,
        flow_id: &str,
    ) {
        let (this, other) = if video { (0, 1) } else { (1, 0) };
        self.absent_since[this] = None;
        if self.last_frame[this].is_some_and(|last| arrival.saturating_duration_since(last) >= SESSION_PAUSE) {
            self.absent_since[other] = None;
        }
        self.last_frame[this] = Some(arrival);
        if self.list(muxer, this, true) {
            tracing::info!(
                flow_id,
                "{}: sends {} again; the PMT lists it",
                self.session,
                TRACK_NAMES[this]
            );
        }
        if !self.listed[other] {
            return;
        }
        let since = *self.absent_since[other].get_or_insert(arrival);
        if arrival.saturating_duration_since(since) < TRACK_ABSENT_AFTER {
            return;
        }
        if self.list(muxer, other, false) {
            tracing::info!(
                flow_id,
                "{}: sends no {} ({} s of {} without it); the PMT drops it{}",
                self.session,
                TRACK_NAMES[other],
                TRACK_ABSENT_AFTER.as_secs(),
                TRACK_NAMES[this],
                if video { "" } else { " and names the audio PID as PCR_PID" },
            );
        }
    }

    /// List `track` in the muxer's PMT, or take it out: whether that
    /// changed anything.
    fn list(&mut self, muxer: &mut crate::engine::rtmp::ts_mux::TsMuxer, track: usize, on: bool) -> bool {
        self.listed[track] = on;
        if track == 0 { muxer.change_has_video(on) } else { muxer.change_has_audio(on) }
    }
}

/// The tracks [`WhipLayout`] indexes, video then audio.
#[cfg(feature = "webrtc")]
const TRACK_NAMES: [&str; 2] = ["video", "audio"];

/// A WHIP publish's — or a WHEP pull's — depayloaded frames onto its TS:
/// one timeline for both tracks ([`WhipClock`]), the parameter sets ahead of
/// every IDR ([`H264ParamSets`]), Opus in the FFmpeg-compatible carriage.
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
    /// RTP timestamp `rtp` on a `rate` Hz clock, arrived at `arrival` — as
    /// TS packets and the PTS it was stamped at; `None` for a frame with no
    /// payload.
    fn frame(
        &mut self,
        video: bool,
        data: &[u8],
        rtp: u64,
        rate: u64,
        arrival: std::time::Instant,
    ) -> Option<(Vec<bytes::Bytes>, u64)> {
        if data.is_empty() {
            return None;
        }
        if video {
            // A depayloaded access unit, Annex B, with the parameter sets
            // ahead of every IDR (`H264ParamSets`).
            let pts = self.clock.pts_90k(true, rtp, rate, arrival);
            let (annex_b, is_keyframe) = self.param_sets.access_unit(data);
            Some((self.muxer.mux_video(&annex_b, pts, pts, is_keyframe), pts))
        } else {
            // Opus: the RTP timestamp on the Opus clock (48 kHz, carried in
            // MediaTime.denom) onto the publish's 90 kHz timeline.
            let pts = if rate == 0 {
                self.last_audio_pts_90khz
            } else {
                self.clock.pts_90k(false, rtp, rate, arrival)
            };
            self.last_audio_pts_90khz = pts;
            Some((self.muxer.mux_audio_opus(data, pts), pts))
        }
    }
}

/// One 90 kHz timeline for a WHIP publish's (or a WHEP pull's) video and
/// audio.
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
    #[cfg(feature = "webrtc")]
    use std::sync::{Arc, atomic::AtomicUsize};

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
        use crate::engine::ts_parse::ts_pid;
        let pmt = ts.iter().find(|p| ts_pid(p) == 0x1000).expect("a PMT");
        let (_, pcr_pid, es) = pmt_layout(pmt);
        ((pcr_pid, es), pcr_pids_of(ts))
    }

    /// A PMT packet as `(version, PCR_PID, [(stream_type, PID)])`.
    #[cfg(feature = "webrtc")]
    fn pmt_layout(pmt: &[u8]) -> (u8, u16, Vec<(u8, u16)>) {
        let sec = &pmt[5..];
        let len = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
        let mut es = Vec::new();
        let mut pos = 12;
        while pos + 5 <= 3 + len - 4 {
            es.push((sec[pos], (((sec[pos + 1] & 0x1F) as u16) << 8) | sec[pos + 2] as u16));
            pos += 5 + ((((sec[pos + 3] & 0x0F) as usize) << 8) | sec[pos + 4] as usize);
        }
        let pcr_pid = (((sec[8] & 0x1F) as u16) << 8) | sec[9] as u16;
        ((sec[5] >> 1) & 0x1F, pcr_pid, es)
    }

    /// Each PMT layout `ts` went through, in turn (`pmt_layout`).
    #[cfg(feature = "webrtc")]
    fn pmts_of(ts: &[bytes::Bytes]) -> Vec<(u8, u16, Vec<(u8, u16)>)> {
        let mut pmts = Vec::new();
        for p in ts.iter().filter(|p| crate::engine::ts_parse::ts_pid(p) == 0x1000) {
            let pmt = pmt_layout(p);
            if pmts.last() != Some(&pmt) {
                pmts.push(pmt);
            }
        }
        pmts
    }

    /// The PIDs whose packets carry a PCR, in turn.
    #[cfg(feature = "webrtc")]
    fn pcr_pids_of(ts: &[bytes::Bytes]) -> Vec<u16> {
        use crate::engine::ts_parse::{extract_pcr, ts_pid};
        let mut pcr_pids: Vec<u16> = ts.iter().filter(|p| extract_pcr(p).is_some()).map(|p| ts_pid(p)).collect();
        pcr_pids.dedup();
        pcr_pids
    }

    /// Whether `ts` carries an Opus access unit in the Opus-in-TS carriage —
    /// a PES on the audio PID opening with the control header `0x7F 0xE0`.
    #[cfg(feature = "webrtc")]
    fn carries_opus(ts: &[bytes::Bytes]) -> bool {
        ts.iter().any(|p| {
            (p[1] & 0x40) != 0
                && crate::engine::ts_parse::ts_pid(p) == 0x0101
                && crate::engine::ts_parse::pes_payload_offset(p)
                    .is_some_and(|es| p[es..].starts_with(&[0x7F, 0xE0]))
        })
    }

    /// A WHEP pull is laid out from the answer's tracks, then from what the
    /// server sends: a track the answer accepted that carries nothing while
    /// the other runs 3 s — before its first frame or since its last —
    /// leaves the PMT, the video taking the PCR with it onto the Opus, and
    /// its next frame brings it back. The edge's own WHEP output accepts both
    /// tracks whatever its flow carries, so laid out from the answer alone a
    /// pull of its audio named a silent video PID as PCR_PID and carried no
    /// PCR, and a pull of its video alone listed an Opus stream nothing was
    /// sent on. A video that stopped once it had sent a frame used to stay,
    /// named as PCR_PID with no PCR on any PID; and with a 1 s window a first
    /// picture merely late (a server that sends from the source's next IDR,
    /// a 2 s GOP) left the PMT and came back, two version bumps and two PCR
    /// moves at session start.
    #[cfg(feature = "webrtc")]
    #[test]
    fn a_whep_pulls_layout_follows_what_the_server_sends() {
        use std::time::{Duration, Instant};
        const BOTH: [(u8, u16); 2] = [(0x1B, 0x0100), (0x06, 0x0101)];
        let window = super::TRACK_ABSENT_AFTER.as_millis() as u64;
        assert_eq!(window, 3_000);

        /// A pull whose answer accepted video and audio, fed frames as its
        /// receive loop feeds them.
        struct Pull {
            mux: super::WhipMux,
            layout: super::WhipLayout,
            t0: Instant,
            ts: Vec<bytes::Bytes>,
        }
        impl Pull {
            fn new() -> Self {
                Self {
                    mux: super::WhipMux::new(super::opus_ts_muxer(None)),
                    layout: super::WhipLayout::new("WHEP pull"),
                    t0: Instant::now(),
                    ts: Vec::new(),
                }
            }

            /// A frame of the video (`video`) or the audio, `at_ms` in.
            fn frame(&mut self, video: bool, at_ms: u64) {
                self.layout.apply(&mut self.mux.muxer, true, true, "f");
                let at = self.t0 + Duration::from_millis(at_ms);
                self.layout.frame(&mut self.mux.muxer, video, at, "f");
                let (data, rtp, rate): (&[u8], _, _) = if video {
                    (&[0, 0, 0, 1, 0x65, 0x88], at_ms * 90, 90_000)
                } else {
                    (&[0xFC; 80], at_ms * 48, 48_000)
                };
                self.ts.extend(self.mux.frame(video, data, rtp, rate, at).unwrap().0);
            }

            /// Audio every 20 ms and, where `video` says so, video every
            /// 40 ms, over `from_ms..to_ms`.
            fn run(&mut self, from_ms: u64, to_ms: u64, video: impl Fn(u64) -> bool) {
                for at in (from_ms..to_ms).step_by(20) {
                    self.frame(false, at);
                    if at % 40 == 0 && video(at) {
                        self.frame(true, at);
                    }
                }
            }
        }

        // Audio alone: the video leaves 3 s in, the Opus taking the PCR,
        // and comes back with its first frame, the PCR with it.
        let mut pull = Pull::new();
        pull.run(0, window + 500, |_| false);
        pull.frame(true, window + 500);
        assert_eq!(
            pmts_of(&pull.ts),
            vec![(0, 0x0100, BOTH.to_vec()), (1, 0x0101, vec![BOTH[1]]), (2, 0x0100, BOTH.to_vec())],
        );
        assert_eq!(pcr_pids_of(&pull.ts), vec![0x0101, 0x0100]);

        // Video alone: the Opus leaves the PMT 3 s in.
        let mut pull = Pull::new();
        for at in (0..window + 200).step_by(40) {
            pull.frame(true, at);
        }
        assert_eq!(pmts_of(&pull.ts), vec![(0, 0x0100, BOTH.to_vec()), (1, 0x0100, vec![BOTH[0]])]);
        assert_eq!(pcr_pids_of(&pull.ts), vec![0x0100]);

        // Both, the video's first frame 2.5 s behind the audio's (a source
        // GOP of 2 s and an encoder's latency): nothing leaves, nothing
        // moves.
        let mut pull = Pull::new();
        pull.run(0, window + 500, |at| at >= 2_500);
        assert_eq!(pmts_of(&pull.ts), vec![(0, 0x0100, BOTH.to_vec())]);
        assert_eq!(pcr_pids_of(&pull.ts), vec![0x0100]);
        assert!(carries_opus(&pull.ts));

        // Both, then the video stops a second in while the audio runs on:
        // the video leaves 3 s after its last frame, the PCR moving onto the
        // Opus, and comes back with its next frame.
        let mut pull = Pull::new();
        pull.run(0, 1_000, |_| true);
        pull.run(1_000, 1_000 + window + 500, |_| false);
        pull.frame(true, 1_000 + window + 500);
        assert_eq!(
            pmts_of(&pull.ts),
            vec![(0, 0x0100, BOTH.to_vec()), (1, 0x0101, vec![BOTH[1]]), (2, 0x0100, BOTH.to_vec())],
        );
        assert_eq!(pcr_pids_of(&pull.ts), vec![0x0100, 0x0101, 0x0100]);

        // Both pause together for longer than the window (a network outage)
        // and come back: neither ran alone, so nothing leaves.
        let mut pull = Pull::new();
        pull.run(0, 1_000, |_| true);
        pull.run(1_000 + window + 2_000, 2 * window + 4_000, |_| true);
        assert_eq!(pmts_of(&pull.ts), vec![(0, 0x0100, BOTH.to_vec())]);
        assert_eq!(pcr_pids_of(&pull.ts), vec![0x0100]);
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
        let mut layout = super::WhipLayout::new("WHIP publish");
        layout.apply(&mut mux, false, true, "f");
        let mut ts = Vec::new();
        for k in 0..5u64 {
            ts.extend(mux.mux_audio_opus(&[0xFC; 80], 90_000 + k * 1_800));
        }
        assert_eq!(layout_of(&ts), ((0x0101, vec![(0x06, 0x0101)]), vec![0x0101]));

        let mut mux = crate::engine::rtmp::ts_mux::TsMuxer::new();
        mux.set_audio_stream(0x06, Some(*b"Opus"));
        let mut layout = super::WhipLayout::new("WHIP publish");
        layout.apply(&mut mux, true, true, "f");
        let mut ts = mux.mux_video(&[0, 0, 0, 1, 0x65, 0x88], 90_000, 90_000, true);
        ts.extend(mux.mux_audio_opus(&[0xFC; 80], 90_000));
        assert_eq!(layout_of(&ts), ((0x0100, vec![(0x1B, 0x0100), (0x06, 0x0101)]), vec![0x0100]));
    }

    /// A track a WHIP publish negotiates after its media began joins the PMT
    /// with a version bump: the video moving the PCR, and the audio too. The
    /// audio used to join without one, so a receiver holding the PMT by its
    /// version (as ISO/IEC 13818-1 lets it) never saw the Opus.
    #[cfg(feature = "webrtc")]
    #[test]
    fn a_track_negotiated_late_joins_the_pmt_with_a_version_bump() {
        let video = |mux: &mut crate::engine::rtmp::ts_mux::TsMuxer, pts| {
            mux.mux_video(&[0, 0, 0, 1, 0x41, 0x9A], pts, pts, false)
        };
        let mut mux = super::opus_ts_muxer(None);
        let mut layout = super::WhipLayout::new("WHIP publish");
        layout.apply(&mut mux, true, false, "f");
        let mut ts = video(&mut mux, 90_000);
        layout.renegotiated(&mut mux, true, true, "f");
        ts.extend(video(&mut mux, 93_600));
        ts.extend(mux.mux_audio_opus(&[0xFC; 80], 93_600));
        let both = vec![(0x1B, 0x0100), (0x06, 0x0101)];
        assert_eq!(pmts_of(&ts), vec![(0, 0x0100, vec![(0x1B, 0x0100)]), (1, 0x0100, both.clone())]);

        let mut mux = super::opus_ts_muxer(None);
        let mut layout = super::WhipLayout::new("WHIP publish");
        layout.apply(&mut mux, false, true, "f");
        let mut ts = mux.mux_audio_opus(&[0xFC; 80], 90_000);
        layout.renegotiated(&mut mux, true, true, "f");
        ts.extend(video(&mut mux, 91_800));
        assert_eq!(pmts_of(&ts), vec![(0, 0x0101, vec![(0x06, 0x0101)]), (1, 0x0100, both)]);
    }

    /// A publisher that negotiates H.264 and Opus and then sends only its
    /// Opus — a camera offered and never sent — is laid out audio-only once
    /// its audio has run 3 s alone: the PMT lists the Opus alone as
    /// PCR_PID, and the Opus carries the PCR. Laid out from the negotiation
    /// alone, PCR_PID named the silent video PID and no packet carried a PCR.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_publish_that_sends_no_video_is_laid_out_audio_only() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::engine::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
        use std::time::{Duration, Instant};

        let config: crate::config::models::WebrtcInputConfig =
            serde_json::from_value(serde_json::json!({ "public_ip": "127.0.0.1" })).unwrap();
        let (broadcast_tx, mut published) = tokio::sync::broadcast::channel(4096);
        let stats = Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
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

        // A publisher offering H.264 video and Opus audio, both accepted.
        let peer = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
        let mut publisher = WebrtcSession::new(&peer).await.unwrap();
        let (offer, pending) = publisher.create_offer(true, true, true).unwrap();
        let (reply, answer) = tokio::sync::oneshot::channel();
        session_tx.send(NewSessionMsg { offer_sdp: offer, reply }).await.unwrap();
        let (answer, _session, _cancel) = answer.await.unwrap().unwrap();
        publisher.apply_answer(&answer, pending).unwrap();
        let audio_mid = publisher.audio_mid.unwrap();
        let audio_pt = publisher.get_pt(audio_mid).expect("Opus accepted");
        assert!(publisher.get_pt(publisher.video_mid.unwrap()).is_some(), "H.264 accepted");
        let connected = tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(publisher.poll_event(&cancel).await, SessionEvent::Connected) {}
        })
        .await;
        assert!(connected.is_ok(), "the publisher never connected");

        // Three and a half seconds of Opus (20 ms frames, TOC 0xFC), and no
        // video.
        let mut ts = Vec::new();
        for k in 0..175u64 {
            let at = str0m::media::MediaTime::new(k * 960, str0m::media::Frequency::FORTY_EIGHT_KHZ);
            publisher.write_media(audio_mid, audio_pt, Instant::now(), at, &[0xFC; 80]).unwrap();
            publisher.drain_outputs().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = publisher.drive_udp_io().await;
            while let Ok(pkt) = published.try_recv() {
                ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
            }
        }
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        while let Ok(pkt) = published.try_recv() {
            ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
        }
        assert!(carries_opus(&ts), "no Opus was published ({} packets)", ts.len());
        let (_, pcr_pid, es) = pmts_of(&ts).pop().expect("a PMT");
        assert_eq!((pcr_pid, es), (0x0101, vec![(0x06, 0x0101)]));
        assert_eq!(pcr_pids_of(&ts), vec![0x0101]);
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
        assert!(w.frame(false, &[], 123_456, 48_000, t0).is_none(), "a probe on the audio");
        assert!(w.frame(true, &[], 3_000_000_000, 90_000, t0).is_none(), "and on the video");
        // The media, 40 ms on: the publish's timeline opens at its first
        // frame, not at the probes.
        let at = t0 + Duration::from_millis(40);
        let (ts, pts) = w.frame(true, &[0, 0, 0, 1, 0x65, 0x88], 3_000_003_600, 90_000, at).unwrap();
        assert_eq!(pts, LEAD);
        assert!(!ts.is_empty());
        let (ts, pts) = w.frame(false, &[0xFC; 80], 123_456 + 1_920, 48_000, at).unwrap();
        assert_eq!(pts, LEAD);
        // The Opus AU carries its 80 bytes: control header 0x7F 0xE0, then
        // au_size.
        let pkt = ts.iter().flat_map(|b| b.chunks(188)).find(|p| (p[1] & 0x40) != 0 && ((p[1] as u16 & 0x1F) << 8 | p[2] as u16) == 0x0101);
        let pkt = pkt.expect("the audio PES");
        let es = crate::engine::ts_parse::pes_payload_offset(pkt).expect("a PES header");
        assert_eq!(&pkt[es..es + 3], &[0x7F, 0xE0, 80]);
        assert!(w.frame(false, &[], 123_456 + 3_840, 48_000, at).is_none(), "a probe mid-stream");
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

    /// A local WHEP server (`serve_whep`): its address, how many pulls it has
    /// had, and the resources DELETEd from it.
    #[cfg(feature = "webrtc")]
    struct WhepServer {
        addr: std::net::SocketAddr,
        posts: Arc<AtomicUsize>,
        deletes: Arc<std::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    #[cfg(feature = "webrtc")]
    impl WhepServer {
        /// The resources DELETEd within `within`, once at least `n` have been.
        async fn deleted(&self, n: usize, within: std::time::Duration) -> Vec<String> {
            let deadline = tokio::time::Instant::now() + within;
            while self.deletes.lock().unwrap().len() < n && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            self.deletes.lock().unwrap().clone()
        }
    }

    /// A local WHEP server answering each pull as an edge's WHEP output does
    /// (the answer passed through `edit`), at the resource `/whep/res-{n}`,
    /// then sending what `send` asks — an H.264 IDR every 40 ms and / or a
    /// 20 ms Opus frame every 20 ms — for `for_ms`.
    #[cfg(feature = "webrtc")]
    async fn serve_whep(edit: fn(String) -> String, send: (bool, bool), for_ms: u64) -> WhepServer {
        serve_whep_sending(edit, send, for_ms, None).await
    }

    /// [`serve_whep`] sending `opus` (real Opus packets, in turn) as its
    /// audio, where the placeholder is a TOC 0xFC and 79 bytes of it.
    #[cfg(feature = "webrtc")]
    async fn serve_whep_sending(
        edit: fn(String) -> String,
        send: (bool, bool),
        for_ms: u64,
        opus: Option<Arc<Vec<Vec<u8>>>>,
    ) -> WhepServer {
        use crate::engine::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
        use axum::http::{StatusCode, header};
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        use str0m::media::{Frequency, MediaTime};
        use tokio_util::sync::CancellationToken;
        const IDR: &[u8] = &[
            0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0xDA, 0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, 0, 0, 0, 1,
            0x65, 0x88, 0x84, 0x00, 0x33,
        ];

        // What `main` installs before any TLS client is built.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let posts = Arc::new(AtomicUsize::new(0));
        let deletes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = axum::Router::new().route(
            "/whep/{resource}",
            axum::routing::delete({
                let deletes = deletes.clone();
                move |axum::extract::Path(resource): axum::extract::Path<String>| async move {
                    deletes.lock().unwrap().push(resource);
                    StatusCode::OK
                }
            }),
        );
        let app = app.route(
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
                    let answer = edit(server.accept_offer(&offer).unwrap());
                    let opus = opus.clone();
                    tokio::spawn(async move {
                        let cancel = CancellationToken::new();
                        let connected = tokio::time::timeout(Duration::from_secs(10), async {
                            loop {
                                match server.poll_event(&cancel).await {
                                    SessionEvent::Connected => break true,
                                    SessionEvent::Disconnected => break false,
                                    _ => {}
                                }
                            }
                        })
                        .await;
                        if connected != Ok(true) {
                            return;
                        }
                        server.drain_pending_events();
                        let video = server.video_mid.and_then(|mid| Some((mid, server.get_pt(mid)?)));
                        let audio = server.audio_mid.and_then(|mid| Some((mid, server.get_pt(mid)?)));
                        for k in 0..for_ms / 20 {
                            if let (true, Some((mid, pt))) = (send.1, audio) {
                                let at = MediaTime::new(k * 960, Frequency::FORTY_EIGHT_KHZ);
                                let packet = opus.as_ref().map(|p| &p[k as usize % p.len()][..]);
                                let _ = server.write_media(mid, pt, Instant::now(), at, packet.unwrap_or(&[0xFC; 80]));
                                server.drain_outputs().await;
                            }
                            if let (true, 0, Some((mid, pt))) = (send.0, k % 2, video) {
                                let at = MediaTime::new(k / 2 * 3_600, Frequency::NINETY_KHZ);
                                let _ = server.write_media(mid, pt, Instant::now(), at, IDR);
                                server.drain_outputs().await;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            let _ = server.drive_udp_io().await;
                        }
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
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        WhepServer { addr, posts, deletes, task }
    }

    /// Run the WHEP input against `addr` for `run_ms`: what it published, as
    /// TS packets, and the events it raised.
    #[cfg(feature = "webrtc")]
    async fn pull_whep(
        addr: std::net::SocketAddr,
        run_ms: u64,
    ) -> (Vec<bytes::Bytes>, Vec<crate::manager::events::Event>) {
        pull_whep_with(addr, run_ms, serde_json::json!({})).await
    }

    /// [`pull_whep`] with more of the input's configuration (`extra`), its
    /// ingress transcode built from it as `spawn_whep_input` builds it.
    #[cfg(feature = "webrtc")]
    async fn pull_whep_with(
        addr: std::net::SocketAddr,
        run_ms: u64,
        extra: serde_json::Value,
    ) -> (Vec<bytes::Bytes>, Vec<crate::manager::events::Event>) {
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let mut json = serde_json::json!({ "whep_url": format!("http://{addr}/whep") });
        json.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        let config: crate::config::models::WhepInputConfig = serde_json::from_value(json).unwrap();
        let transcoder =
            super::InputTranscoder::new(config.audio_encode.as_ref(), config.transcode.as_ref(), None, None)
                .unwrap();
        let (broadcast_tx, mut published) = tokio::sync::broadcast::channel(16_384);
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
                let (mut transcoder, mut post) = (transcoder, None);
                super::whep_input_loop(
                    config, broadcast_tx, stats, cancel, &events, "flow-w", &mut transcoder, &mut post,
                )
                .await;
            }
        });
        tokio::time::sleep(Duration::from_millis(run_ms)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        let mut ts = Vec::new();
        while let Ok(pkt) = published.try_recv() {
            ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
        }
        let mut raised_events = Vec::new();
        while let Ok(ev) = raised.try_recv() {
            raised_events.push(ev);
        }
        (ts, raised_events)
    }

    /// An edge-to-edge pull of a WHEP output that sends audio alone — the
    /// output answers H.264 too, whatever its flow carries — publishes the
    /// audio: by the end the PMT lists the Opus alone and names it as
    /// PCR_PID, and the Opus carries the PCR. The input muxed only the video
    /// track: it published nothing, and its flow sent NULL keepalives and
    /// reported the input stalled.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whep_pull_of_audio_alone_publishes_the_audio() {
        let server = serve_whep(|a| a, (false, true), 4_000).await;
        let (ts, _) = pull_whep(server.addr, 5_000).await;
        server.task.abort();
        assert!(carries_opus(&ts), "no Opus was published ({} packets)", ts.len());
        let (_, pcr_pid, es) = pmts_of(&ts).pop().expect("a PMT");
        assert_eq!((pcr_pid, es), (0x0101, vec![(0x06, 0x0101)]));
        assert_eq!(pcr_pids_of(&ts), vec![0x0101]);
    }

    /// The whole chain: a WHEP pull of Opus through an ingress
    /// `audio_encode` (AAC-LC) publishes AAC — the PMT names it on the
    /// audio PID, no longer the Opus, and the audio PES carry ADTS. The TS
    /// audio replacer refused Opus as a source, so a WebRTC input's
    /// `audio_encode` re-encoded nothing and its Opus passed through, the
    /// engage watchdog saying `codec_not_replaceable`.
    #[cfg(all(feature = "webrtc", feature = "media-codecs", feature = "fdk-aac"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_whep_pull_of_opus_is_re_encoded_by_an_ingress_audio_encode() {
        // Real Opus (the input passes it to libavcodec): a second of a tone.
        let mut enc = video_engine::AudioEncoder::open(&video_codec::AudioEncoderConfig {
            codec: video_codec::AudioCodecType::Opus,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 96,
        })
        .unwrap();
        let tone: Vec<f32> = (0..960).map(|k| 0.3 * (k as f32 * 0.13).sin()).collect();
        let packets: Vec<Vec<u8>> = (0..150)
            .flat_map(|_| enc.encode_frame(&[tone.clone(), tone.clone()]).unwrap())
            .map(|f| f.data.to_vec())
            .collect();
        assert!(packets.len() >= 100);
        let server = serve_whep_sending(|a| a, (false, true), 4_000, Some(Arc::new(packets))).await;
        let (ts, _) = pull_whep_with(server.addr, 5_000, serde_json::json!({ "audio_encode": { "codec": "aac_lc" } })).await;
        server.task.abort();
        let (_, _, es) = pmts_of(&ts).pop().expect("a PMT");
        assert_eq!(es, vec![(0x0F, 0x0101)], "AAC on the audio PID");
        let adts = ts.iter().any(|p| {
            (p[1] & 0x40) != 0
                && crate::engine::ts_parse::ts_pid(p) == 0x0101
                && crate::engine::ts_parse::pes_payload_offset(p)
                    .is_some_and(|es| p[es] == 0xFF && p[es + 1] & 0xF6 == 0xF0)
        });
        assert!(adts, "no ADTS on the audio PID ({} packets)", ts.len());
        assert!(!carries_opus(&ts), "the Opus passed through");
    }

    /// An A/V pull publishes both tracks: the PMT lists the H.264 and the
    /// Opus, the PCR on the video, and the Opus is carried. The input muxed
    /// only the video: the PMT listed it alone and the audio went nowhere.
    /// And the input stopping releases the pull's resource on the server (a
    /// DELETE), which it never did.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whep_pull_of_video_and_audio_publishes_both() {
        let server = serve_whep(|a| a, (true, true), 1_500).await;
        let (ts, _) = pull_whep(server.addr, 2_500).await;
        assert!(carries_opus(&ts), "no Opus was published ({} packets)", ts.len());
        let both = vec![(0x1B, 0x0100), (0x06, 0x0101)];
        assert_eq!(pmts_of(&ts), vec![(0, 0x0100, both)]);
        assert_eq!(pcr_pids_of(&ts), vec![0x0100]);
        let deleted = server.deleted(1, std::time::Duration::from_secs(5)).await;
        server.task.abort();
        assert_eq!(deleted, vec!["res-0".to_string()]);
    }

    /// The `whep_answer_unusable` Warnings among `raised`, by `reason`.
    #[cfg(feature = "webrtc")]
    fn unusable_answers(raised: &[crate::manager::events::Event]) -> Vec<String> {
        raised
            .iter()
            .filter_map(|e| {
                let d = e.details.as_ref().filter(|d| d["error_code"] == "whep_answer_unusable")?;
                assert_eq!(e.severity, crate::manager::events::EventSeverity::Warning);
                assert_eq!(e.flow_id.as_deref(), Some("flow-w"));
                Some(d["reason"].as_str().unwrap_or_default().to_string())
            })
            .collect()
    }

    /// A WHEP server whose answer names no codec this input offered — VP8
    /// and G.711 on the offer's payload types — leaves it no track to read:
    /// it is not connected over, its resource is released (DELETE), a Warning
    /// names it once for the episode, and the input pulls again after its
    /// backoff. It used to connect and publish nothing for as long as the
    /// session lasted.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whep_answer_with_nothing_this_input_takes_is_not_waited_on() {
        let off_offer =
            |a: String| a.replace("H264/90000", "VP8/90000").replace("opus/48000/2", "PCMU/8000");
        let server = serve_whep(off_offer, (true, true), 1_000).await;
        let (ts, raised) = pull_whep(server.addr, 1_500).await;
        let deleted = server.deleted(2, std::time::Duration::from_secs(5)).await;
        server.task.abort();
        assert!(ts.is_empty(), "{} packets published", ts.len());
        assert_eq!(server.posts.load(std::sync::atomic::Ordering::SeqCst), 2, "pulled at 0 and 1 s");
        assert_eq!(unusable_answers(&raised), vec!["no_h264_or_opus"], "once for both pulls");
        assert!(raised.iter().any(|e| e.message.contains("no H.264 video and no Opus audio")));
        assert_eq!(deleted, vec!["res-0".to_string(), "res-1".to_string()]);
    }

    /// A WHEP server whose answer this input cannot apply — not SDP at all,
    /// here — raises a Warning saying so, once for the episode however many
    /// pulls it refuses, and each pull's resource is released. It was a log
    /// line only: from the manager, a pull that never started. str0m's parse
    /// error stays in the log: it spans several lines of parser internals and
    /// names a heap address of this process (`PointerOffset(0x…)`), which the
    /// event's message and `details.error` carried to the manager.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whep_answer_that_cannot_be_applied_raises_a_warning_once() {
        let server = serve_whep(|_| "this is not SDP".to_string(), (true, true), 1_000).await;
        let (ts, raised) = pull_whep(server.addr, 1_500).await;
        let deleted = server.deleted(2, std::time::Duration::from_secs(5)).await;
        server.task.abort();
        assert!(ts.is_empty(), "{} packets published", ts.len());
        assert_eq!(server.posts.load(std::sync::atomic::Ordering::SeqCst), 2, "pulled at 0 and 1 s");
        assert_eq!(unusable_answers(&raised), vec!["not_applied"], "once for both pulls");
        let warning = raised
            .iter()
            .find(|e| e.message.starts_with("WHEP server's answer could not be applied: "))
            .unwrap_or_else(|| panic!("{:?}", raised.iter().map(|e| &e.message).collect::<Vec<_>>()));
        let error = warning.details.as_ref().unwrap()["error"].as_str().unwrap().to_string();
        for text in [&warning.message, &error] {
            assert!(text.contains("the answer is not valid SDP"), "{text}");
            assert!(!text.contains("0x") && !text.contains("PointerOffset"), "{text}");
            assert_eq!(text.lines().count(), 1, "{text}");
        }
        assert_eq!(deleted, vec!["res-0".to_string(), "res-1".to_string()]);
    }
}
