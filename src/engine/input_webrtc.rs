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

    // The input's TS, one for every publisher in turn (`WhipMux`): each
    // takes it on from the last — continuity counters, PSI versions and the
    // timeline carried on, the step to its first PCR marked (`WhipMux::
    // session`). A muxer per publisher restarted all of it from zero with no
    // discontinuity_indicator, which a receiver locked across a re-publish
    // took for CC errors and an unflagged backward PCR jump.
    let mut whip = WhipMux::new(opus_ts_muxer(config.pid_overrides.as_ref()));
    let mut seq_num: u16 = 0;

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

        // The tracks' frames onto the input's TS (`WhipMux`): one timeline
        // for both, the parameter sets ahead of every IDR, the Opus in the
        // FFmpeg-compatible carriage, no payload-less frame.
        whip.session(audio_pt.is_some(), sdp_declares_stereo_opus(&msg.offer_sdp));
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
                tokio::select! {
                    event = session.poll_event(&child_cancel) => event,
                    _ = hold_expiry(whip.hold_deadline()) => {
                        for (ts_chunks, pts_90khz) in whip.release_due(std::time::Instant::now()) {
                            publish_frame(ts_chunks, pts_90khz, &mut seq_num, &stats, &broadcast_tx, transcoder, post);
                        }
                        continue;
                    }
                }
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
                    for (ts_chunks, pts_90khz) in whip.frame(
                        !is_audio_stream,
                        &data,
                        rtp_time.numer(),
                        rtp_time.denom() as u64,
                        network_time,
                    ) {
                        publish_frame(ts_chunks, pts_90khz, &mut seq_num, &stats, &broadcast_tx, transcoder, post);
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
        // Whatever the session held for its channel count goes out with it.
        for (ts_chunks, pts_90khz) in whip.end_session() {
            publish_frame(ts_chunks, pts_90khz, &mut seq_num, &stats, &broadcast_tx, transcoder, post);
        }
    }
}

/// Resolves at `deadline` — when a [`WhipMux`] holding video for its
/// session's channel count is due to release it — and never when there is
/// none.
#[cfg(feature = "webrtc")]
async fn hold_expiry(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
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
    // RFC 7587's default; `WhipMux` follows the session.
    ts_muxer.change_opus_channels(1);
    ts_muxer
}

/// Whether `sdp` — the description of the end that sends the Opus: a WHIP
/// publisher's offer, a WHEP server's answer — declares it stereo, with
/// `sprop-stereo=1` on an audio m-line's Opus payload type (RFC 7587 §7.1).
/// Without it RFC 7587 has the sender likely to send mono, "the received
/// signal can be safely downmixed to mono"; Chrome declares nothing and
/// sends mono unless the answer asks for stereo, which this edge's never
/// does. The packets themselves have the last word (`WhipMux::frame`).
#[cfg(feature = "webrtc")]
fn sdp_declares_stereo_opus(sdp: &str) -> bool {
    let (mut audio, mut opus, mut stereo) = (false, Vec::new(), Vec::new());
    for line in sdp.lines().map(str::trim_end) {
        if let Some(media) = line.strip_prefix("m=") {
            audio = media.starts_with("audio ");
        } else if !audio {
        } else if let Some((pt, codec)) = line.strip_prefix("a=rtpmap:").and_then(|r| r.split_once(' ')) {
            if codec.to_ascii_lowercase().starts_with("opus/") {
                opus.push(pt);
            }
        } else if let Some((pt, params)) = line.strip_prefix("a=fmtp:").and_then(|r| r.split_once(' '))
            && params.split(';').any(|p| p.trim().eq_ignore_ascii_case("sprop-stereo=1"))
        {
            stereo.push(pt);
        }
    }
    opus.iter().any(|pt| stereo.contains(pt))
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
    // The input's TS, one for every pull in turn, as the WHIP input's is for
    // every publisher (`WhipMux::session`).
    let mut whep = WhipMux::new(opus_ts_muxer(config.pid_overrides.as_ref()));
    let mut seq_num: u16 = 0;

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
            release().await;
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
            release().await;
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
            release().await;
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
        whep.session(tracks.1, sdp_declares_stereo_opus(&answer_sdp));
        // A server may prime the pull with a keyframe it had cached: the
        // video is then timed from the live pictures (`PrimedIdr`).
        whep.expect_primed_idr();
        let mut layout = WhipLayout::new("WHEP pull");

        // Receive loop
        loop {
            let event = tokio::select! {
                event = session.poll_event(&cancel) => event,
                _ = hold_expiry(whep.hold_deadline()) => {
                    for (ts_chunks, pts_90khz) in whep.release_due(std::time::Instant::now()) {
                        publish_frame(ts_chunks, pts_90khz, &mut seq_num, &stats, &broadcast_tx, transcoder, post);
                    }
                    continue;
                }
            };
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
                    for (ts_chunks, pts_90khz) in whep.frame(
                        video,
                        &data,
                        rtp_time.numer(),
                        rtp_time.denom() as u64,
                        network_time,
                    ) {
                        publish_frame(ts_chunks, pts_90khz, &mut seq_num, &stats, &broadcast_tx, transcoder, post);
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
        // Whatever the pull held for its channel count goes out with it.
        for (ts_chunks, pts_90khz) in whep.end_session() {
            publish_frame(ts_chunks, pts_90khz, &mut seq_num, &stats, &broadcast_tx, transcoder, post);
        }

        release().await;
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

/// How long the WHEP input waits for a server to answer the DELETE that
/// releases a pull's resource ([`whep_release`]). A live server answers in a
/// few round trips; this bounds what a dead one costs. It is waited out in
/// the input's task, so a stopping flow and a node shutting down wait for
/// it: within the 5 s a stopping flow gives each input task
/// (`FlowRuntime::stop`, `remove_input`), and — beside the WHIP output's
/// own 2 s (`WHIP_DELETE_BOUND`) on a flow carrying both — well within the
/// 10 s a shutdown gives its flows (`SHUTDOWN_FLOW_STOP_BUDGET`).
#[cfg(feature = "webrtc")]
const WHEP_DELETE_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

/// Release a WHEP pull's resource on the server — a DELETE to the answer's
/// `Location` (`resource`) — waiting up to [`WHEP_DELETE_BOUND`] for the
/// answer. Best effort: a failure is a log line, and a resource the server
/// no longer has (404: it closed the stream, say) is released already.
/// Awaited rather than spawned, and never cut short by the input's cancel:
/// a spawned DELETE does not outlive a process shutting down (it leaves
/// through `_exit` once the flows have stopped), so a server that counts
/// its viewers, or caps them, kept the dead pull of every edge restart
/// until its own timeout. The input used to leave every pull's resource to
/// that timeout, then spawned the DELETE (until 2026-11).
#[cfg(feature = "webrtc")]
async fn whep_release(
    resource: Option<String>,
    bearer_token: Option<String>,
    tls: crate::util::tls::TlsTrust,
) {
    let Some(resource) = resource else {
        return;
    };
    let delete = crate::engine::webrtc::signaling::delete_session(&resource, bearer_token.as_deref(), &tls);
    match tokio::time::timeout(WHEP_DELETE_BOUND, delete).await {
        Ok(Ok(())) => tracing::debug!("WHEP: released {resource}"),
        Ok(Err(e)) => tracing::warn!("WHEP: DELETE {resource} failed: {e}"),
        Err(_) => tracing::warn!(
            "WHEP: DELETE {resource} unanswered after {} s; left to the server's own timeout",
            WHEP_DELETE_BOUND.as_secs(),
        ),
    }
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
    /// PCR (`TsMuxer::audio_pcr`). A layout other than the one an earlier
    /// session left on the muxer goes out with a PMT version bump.
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
        // A muxer that has gone out with an earlier session's layout bumps
        // its PMT version for a different one.
        muxer.change_has_video(has_video);
        muxer.change_has_audio(has_audio);
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
    /// The TS lists its Opus as stereo (`channel_config_code` 2, else 1),
    /// from the first session that sent or declared stereo on, for the
    /// input's life.
    opus_stereo: bool,
    /// The session's Opus channel count is still to be read off its first
    /// packet ([`Self::session`]): its video is held meanwhile.
    count_open: bool,
    /// The session's video held while its channel count is open: each
    /// access unit (Annex B, its PTS, whether it is a keyframe), in order.
    held: Vec<(Vec<u8>, u64, bool)>,
    held_bytes: usize,
    /// When the first frame held arrived.
    held_since: Option<std::time::Instant>,
    /// Whether the pull's first picture is one its server had cached, while
    /// that is open ([`PrimedIdr`]); `None` once it is settled, and for a WHIP
    /// publish from the start.
    primed: Option<PrimedIdr>,
}

/// One frame's TS packets and the PTS (90 kHz) they were stamped at, as
/// [`WhipMux`] hands them out to publish.
#[cfg(any(feature = "webrtc", test))]
type MuxedFrame = (Vec<bytes::Bytes>, u64);

/// How long a session's video is held for its first Opus packet to settle
/// the channel count before the first PMT goes out ([`WhipMux::session`]).
/// A sender sending both tracks sends a packet each packet time — 20 ms
/// from Chrome, ffmpeg's WHIP muxer and str0m (the edge's and the relay's
/// senders), at most 120 ms under RFC 6716 §3.2.5 — and bilbycast-relay
/// primes a WHEP viewer with its cached IDR before forwarding the live
/// stream: on an edge WHIP output → relay → edge WHEP input rig (debug
/// builds, loopback) the first Opus packet came 62, 109, 126 and 129 ms
/// behind that IDR on four pulls. 300 ms covers the longest Opus packet
/// twice over, and more than twice what the rig measured. What it costs is
/// paid by a sender that negotiated audio and sends none, or sends it later
/// than that: its first pictures go out together 300 ms late, behind a PMT
/// listing the Opus as the TS did before (mono, unless an earlier session
/// sent stereo), and a stereo packet after that lists it so with a version
/// bump, as every undeclared stereo sender did without the hold.
#[cfg(any(feature = "webrtc", test))]
const OPUS_COUNT_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

/// The most video [`WhipMux`] holds for a session's channel count: past it
/// what is held goes out at once, as when [`OPUS_COUNT_WAIT`] passes. 2 MiB
/// is 300 ms of a 50 Mbit/s stream — a WebRTC sender runs at a tenth of
/// that — and bounds what a sender pouring out video can make the input
/// keep.
#[cfg(any(feature = "webrtc", test))]
const HOLD_MAX_BYTES: usize = 2 * 1024 * 1024;

/// How far (90 kHz) every picture that follows a pull's first must run ahead
/// of it — its RTP distance from it less its arrival's — for that first
/// picture to be one its server had cached ([`PrimedIdr`]): 100 ms.
///
/// A server whose first picture is live puts the picture shown next one frame
/// period behind it on the RTP clock and, sent as they come, in arrival as
/// well: it runs ahead only by what the first picture was held up by on its
/// way — encoding and pacing it, tens of milliseconds. Behind a cached IDR
/// they run ahead by its age: with a 2 s GOP 0 to 2 s, less than this on one
/// pull in twenty, which is left as it is.
#[cfg(any(feature = "webrtc", test))]
const PRIMED_LEAD_90K: i128 = 9_000;

/// How long, and for how many pictures, a pull whose first picture is cached
/// is held while the pictures after it show which of them is shown first. A
/// source without B-frames shows it with its third picture, one with them by
/// the end of the mini-GOP the second fell in: with seven B-frames at 25 fps,
/// 320 ms at most.
#[cfg(any(feature = "webrtc", test))]
const PRIMED_WAIT: std::time::Duration = std::time::Duration::from_millis(400);
#[cfg(any(feature = "webrtc", test))]
const PRIMED_MAX_PICTURES: usize = 10;

/// How many pictures of an unfinished mini-GOP hold the one shown first: the
/// reference decoded first, the B-frame in the middle of those before it, the
/// one in the middle of the half before that, and so on — five, of up to
/// fifteen B-frames. An encoder that codes a pyramid level by level instead
/// may put one B-frame a frame period ahead of the IDR.
#[cfg(any(feature = "webrtc", test))]
const PRIMED_FIRST_SHOWN_WITHIN: usize = 5;

/// Whether a WHEP pull's first picture is a keyframe its server had cached,
/// older than the live pictures that follow it: the pull's first pictures,
/// until that is settled.
///
/// bilbycast-relay primes each viewer with the IDR it has cached, up to a
/// source GOP old (minutes, of a browser that sends one only when asked),
/// and then sends the live pictures. The IDR arrives when they do but is
/// stamped when it was taken, and [`WhipClock`] times a track from its first
/// frame: every live picture went out later than it came by the IDR's age,
/// and so that far past the sound sent with it, for the whole pull, the PCR
/// stepping by as much between the IDR and the first of them (past a second,
/// with nothing to bridge it and no `discontinuity_indicator`). So the video
/// is timed from the live pictures instead, as if the first of them had
/// opened the session: once, as they show the IDR stale, the audio not
/// touched. The IDR keeps its place at the session's start, where it arrived;
/// a decoder is shown it until the first live picture.
///
/// It shows on the second picture: that picture's `frame_num` says that
/// reference pictures between the two were not sent ([`H264Order`]), which a
/// live stream's second picture never says unless one was lost on the way, and
/// it runs ahead of the IDR ([`PRIMED_LEAD_90K`]), which it would not for a
/// picture lost. Its timing alone would not do. A live stream's picture runs
/// ahead too where it is a P-frame sent ahead of the B-frames shown before it,
/// or where a server sends what it queued for a connecting viewer in one go,
/// and one of a source with no steady frame period (a screen share) by any
/// amount. So a pull whose second picture's `frame_num` is 1 or 0, or cannot
/// be read, is not looked at: nothing of it is held, and it goes out as a
/// publish does. That leaves a cached IDR as it is where `frame_num` has come
/// round to the IDR's again (of x264's sixteen values, two: one pull in
/// eight), and where the picture behind it is an IDR itself.
///
/// The video is timed from the live picture shown first. Of a stream whose
/// pictures are shown in the order they are sent that is the second one, and
/// it is settled as that one comes. Otherwise the pull's frames are held from
/// it on, in the order they arrive, until
///
/// * one does not run ahead of the first: the first is live, and a picture
///   behind it was lost;
/// * a picture comes that is shown past all the others, so that every picture
///   shown before them has come;
/// * [`PRIMED_WAIT`], [`PRIMED_MAX_PICTURES`] or [`HOLD_MAX_BYTES`] is
///   reached: the first is left as it is where too few pictures of a mini-GOP
///   came to hold the one shown first ([`PRIMED_FIRST_SHOWN_WITHIN`]).
///
/// What is held then goes out as it came, through the same code as any
/// session's frames, and the PCR runs on meanwhile with the pictures' arrival
/// ([`TsMuxer::run_pcr_on`]).
///
/// [`TsMuxer::run_pcr_on`]: crate::engine::rtmp::ts_mux::TsMuxer::run_pcr_on
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Default)]
struct PrimedIdr {
    /// The pull's first picture, an IDR: its RTP time and its arrival.
    first: Option<(u64, std::time::Instant)>,
    /// The stream's pictures are shown in the order they are sent.
    in_order: bool,
    /// The frames held, from the second picture on: reference pictures between
    /// it and the first were not sent, and it ran ahead of the first.
    held: Vec<ArrivedFrame>,
    held_bytes: usize,
}

/// What the pictures held say of a pull's first ([`PrimedIdr::verdict`]).
#[cfg(any(feature = "webrtc", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstPicture {
    /// The live picture shown first is still to come.
    Open,
    /// The video stays timed from it.
    Kept,
    /// Its server had cached it: the video is timed from the one of them shown
    /// first.
    Cached,
}

/// A depayloaded frame as [`WhipMux::frame`] takes it, kept.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug)]
struct ArrivedFrame {
    video: bool,
    data: Vec<u8>,
    rtp: u64,
    rate: u64,
    arrival: std::time::Instant,
}

/// A picture held behind a pull's first: how far (90 kHz) behind the first it
/// is shown, on the RTP clock, how far ahead of it it runs, and the frame it
/// came as.
#[cfg(any(feature = "webrtc", test))]
#[derive(Clone, Copy)]
struct HeldPicture<'a> {
    shown: i128,
    ahead: i128,
    frame: &'a ArrivedFrame,
}

#[cfg(any(feature = "webrtc", test))]
impl PrimedIdr {
    /// How far (90 kHz) a picture is shown behind the pull's first picture,
    /// and how far ahead of it that runs: less what it arrived behind it by.
    fn distance(first: (u64, std::time::Instant), rtp: u64, rate: u64, arrival: std::time::Instant) -> (i128, i128) {
        let shown = (rtp as i128 - first.0 as i128) * 90_000 / rate.max(1) as i128;
        let arrived = arrival.saturating_duration_since(first.1).as_micros() as i128 * 9 / 100;
        (shown, shown - arrived)
    }

    /// The pictures held, as they came.
    fn pictures(&self) -> Vec<HeldPicture<'_>> {
        let Some(first) = self.first else {
            return Vec::new();
        };
        self.held
            .iter()
            .filter(|frame| frame.video)
            .map(|frame| {
                let (shown, ahead) = Self::distance(first, frame.rtp, frame.rate, frame.arrival);
                HeldPicture { shown, ahead, frame }
            })
            .collect()
    }

    /// When what is held is judged as it is, if anything is.
    fn deadline(&self) -> Option<std::time::Instant> {
        self.held.first().map(|f| f.arrival + PRIMED_WAIT)
    }

    /// What the pictures that have come say of the pull's first. `over`: they
    /// are to be judged as they are, however few.
    fn verdict(&self, over: bool) -> FirstPicture {
        let pictures = self.pictures();
        let Some((last, rest)) = pictures.split_last() else {
            return FirstPicture::Kept;
        };
        if pictures.iter().any(|p| p.ahead <= PRIMED_LEAD_90K) {
            return FirstPicture::Kept;
        }
        if self.in_order || !rest.is_empty() && rest.iter().all(|p| p.shown < last.shown) {
            return FirstPicture::Cached;
        }
        match over || pictures.len() >= PRIMED_MAX_PICTURES || self.held_bytes > HOLD_MAX_BYTES {
            true if pictures.len() >= PRIMED_FIRST_SHOWN_WITHIN => FirstPicture::Cached,
            true => FirstPicture::Kept,
            false => FirstPicture::Open,
        }
    }
}

/// What an H.264 stream's SPS says of the order its pictures come in, and how
/// to read a picture's `frame_num` (ITU-T H.264 §7.3.2.1.1, §7.3.3, §7.4.3):
/// it counts the reference pictures from the last IDR, whose is 0, so that
/// the picture sent right behind an IDR has 1 — or 0, the IDR's second field.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug)]
struct H264Order {
    frame_num_bits: u8,
    separate_colour_planes: bool,
    /// The pictures are shown in the order they are sent: Baseline, which has
    /// no B-frames, or picture order count type 2, which allows none.
    in_order: bool,
}

#[cfg(any(feature = "webrtc", test))]
impl H264Order {
    /// Of `sps`, an SPS NAL unit; `None` if it does not parse.
    fn of(sps: &[u8]) -> Option<Self> {
        use crate::engine::content_analysis::bitreader::{BitReader, unescape_rbsp};
        let rbsp = unescape_rbsp(sps.get(1..)?);
        let mut br = BitReader::new(&rbsp);
        let profile_idc = br.read_bits(8)?;
        br.read_bits(16)?; // constraint flags, level_idc
        br.read_ue()?; // seq_parameter_set_id
        let mut separate_colour_planes = false;
        if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile_idc) {
            let chroma_format_idc = br.read_ue()?;
            if chroma_format_idc == 3 {
                separate_colour_planes = br.read_bit()? == 1;
            }
            br.read_ue()?; // bit_depth_luma_minus8
            br.read_ue()?; // bit_depth_chroma_minus8
            br.read_bit()?; // qpprime_y_zero_transform_bypass_flag
            if br.read_bit()? == 1 {
                // The scaling lists: each its deltas, until one ends it.
                for list in 0..if chroma_format_idc == 3 { 12 } else { 8 } {
                    if br.read_bit()? == 0 {
                        continue;
                    }
                    let (mut last, mut next) = (8, 8);
                    for _ in 0..if list < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + br.read_se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
        let frame_num_bits = u8::try_from(br.read_ue()?).ok().filter(|b| *b <= 12)? + 4;
        let pic_order_cnt_type = br.read_ue()?;
        let in_order = profile_idc == 66 || pic_order_cnt_type == 2;
        Some(Self { frame_num_bits, separate_colour_planes, in_order })
    }

    /// The `frame_num` of the access unit `au`, off its first slice; `None`
    /// for an IDR's, and one that does not parse.
    fn frame_num(&self, au: &[u8]) -> Option<u32> {
        use crate::engine::content_analysis::bitreader::{BitReader, unescape_rbsp};
        let slice = nal_units(au).into_iter().find(|n| n.first().is_some_and(|b| matches!(b & 0x1F, 1 | 2 | 5)))?;
        if slice[0] & 0x1F == 5 {
            return None;
        }
        // Four ue(v) and two bits, at most, ahead of it.
        let rbsp = unescape_rbsp(&slice[1..slice.len().min(40)]);
        let mut br = BitReader::new(&rbsp);
        br.read_ue()?; // first_mb_in_slice
        br.read_ue()?; // slice_type
        br.read_ue()?; // pic_parameter_set_id
        if self.separate_colour_planes {
            br.read_bits(2)?; // colour_plane_id
        }
        br.read_bits(self.frame_num_bits)
    }
}

#[cfg(any(feature = "webrtc", test))]
impl WhipMux {
    fn new(muxer: crate::engine::rtmp::ts_mux::TsMuxer) -> Self {
        Self {
            muxer,
            clock: WhipClock::default(),
            param_sets: H264ParamSets::default(),
            last_audio_pts_90khz: 0,
            opus_stereo: false,
            count_open: false,
            held: Vec::new(),
            held_bytes: 0,
            held_since: None,
            primed: None,
        }
    }

    /// A session — a WHIP publisher, a WHEP pull — starts onto this TS,
    /// taking it on from the last one: the muxer's continuity counters and
    /// PSI versions run on, its tracks are timed on the same timeline as the
    /// last session's (`WhipClock::session`), and the step to its first PCR
    /// — the wall-clock gap between the sessions, which no PCR spanned —
    /// carries `discontinuity_indicator` (`TsMuxer::mark_discontinuity`):
    /// TR 101 290 counts any PCR step past 100 ms without one, and the
    /// flow's source-discontinuity watcher put one on the PCR after it.
    ///
    /// Its Opus (`audio`: the session negotiated it) is listed as stereo
    /// when the sender's description declares it (`declared_stereo`,
    /// [`sdp_declares_stereo_opus`]), and otherwise by its first packet: the
    /// session's video is held until then ([`Self::frame`]) — for
    /// [`OPUS_COUNT_WAIT`] and [`HOLD_MAX_BYTES`] at most — so the first PMT
    /// the session sends already says what that packet does. ffmpeg and
    /// GStreamer's tsdemux keep the channel count of the PMT they start on
    /// across a version bump, and a WHEP pull from bilbycast-relay always
    /// starts with video (the relay primes a viewer with its cached IDR),
    /// so a stereo feed listed mono until its first packet, ~0.1 s later, was
    /// read as mono throughout. The TS never goes back to mono (a stereo
    /// decoder plays a mono packet on both channels, where a mono one folds
    /// a stereo packet down), so nothing is held once it lists stereo: a
    /// reconnecting pull of a stereo feed runs on without a version bump,
    /// where it went stereo, mono and stereo again within a frame.
    ///
    /// Each session used to get a muxer and clock of its own: CC, PTS and
    /// PCR started again from zero with no discontinuity_indicator.
    fn session(&mut self, audio: bool, declared_stereo: bool) {
        debug_assert!(self.held.is_empty(), "the last session's video was released");
        self.held.clear();
        self.held_bytes = 0;
        self.held_since = None;
        self.primed = None;
        self.muxer.mark_discontinuity();
        self.clock.session();
        self.param_sets = H264ParamSets::default();
        if declared_stereo && !self.opus_stereo {
            self.opus_stereo = true;
            self.muxer.change_opus_channels(2);
        }
        self.count_open = audio && !self.opus_stereo;
    }

    /// The session just started ([`Self::session`]) is a WHEP pull, whose
    /// server may prime it with a keyframe it had cached ([`PrimedIdr`]). A
    /// WHIP publisher has none: its first picture is the one it has just
    /// taken, however late it came.
    fn expect_primed_idr(&mut self) {
        self.primed = Some(PrimedIdr::default());
    }

    /// Until when the session's frames are held, if any are, for its channel
    /// count or its first picture's age: [`Self::release_due`] then.
    fn hold_deadline(&self) -> Option<std::time::Instant> {
        let count = self.held_since.map(|since| since + OPUS_COUNT_WAIT);
        let primed = self.primed.as_ref().and_then(PrimedIdr::deadline);
        [count, primed].into_iter().flatten().min()
    }

    /// The frames held past their deadline (`now`), muxed: those held while
    /// the pull's live pictures were open, settled as they stand once
    /// [`PRIMED_WAIT`] has passed; and the video held for the session's
    /// channel count, once [`OPUS_COUNT_WAIT`] has since the first of it
    /// arrived — the Opus is listed as it was, and a stereo packet later
    /// lists it so with a version bump.
    fn release_due(&mut self, now: std::time::Instant) -> Vec<MuxedFrame> {
        let mut out = Vec::new();
        if self.primed.as_ref().and_then(PrimedIdr::deadline).is_some_and(|due| now >= due) {
            out = self.settle_primed(now, true);
        }
        if self.held_since.is_some_and(|since| now >= since + OPUS_COUNT_WAIT) {
            out.extend(self.release());
        }
        out
    }

    /// The session has ended: what it held, muxed.
    fn end_session(&mut self) -> Vec<MuxedFrame> {
        let mut out = match &self.primed {
            Some(primed) if !primed.held.is_empty() => self.settle_primed(std::time::Instant::now(), true),
            _ => Vec::new(),
        };
        out.extend(self.release());
        out
    }

    /// Settle what the pull's video is timed from, if what has come by `now`
    /// does ([`PrimedIdr::verdict`]; `over`: it must), and then the frames held
    /// since, muxed as they came. Until it is settled, the PCR run on to `now`,
    /// when the picture that has just come arrived.
    fn settle_primed(&mut self, now: std::time::Instant, over: bool) -> Vec<MuxedFrame> {
        let verdict = self.primed.as_ref().map(|primed| primed.verdict(over));
        let cached = match verdict {
            None => return Vec::new(),
            Some(FirstPicture::Open) => return self.run_pcr_on(now),
            Some(FirstPicture::Cached) => true,
            Some(FirstPicture::Kept) => false,
        };
        let Some(primed) = self.primed.take() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if cached && let Some(live) = primed.pictures().into_iter().min_by_key(|p| p.shown) {
            tracing::info!(
                "WHEP: the pull's first picture is a keyframe its server had cached, {} ms old; \
                 the video is timed from the live pictures",
                live.ahead / 90
            );
            self.clock.retime_video(live.frame.rtp, live.frame.arrival);
            // The PCR runs on from the IDR's by what passed until the live
            // pictures arrived. Past what the muxer bridges with filler PCRs
            // it is a step, and is marked as one. The IDR is ahead of it: the
            // wait for its channel count ran out long since.
            let opens = &primed.held[0];
            if self.held_since.is_some_and(|since| opens.arrival.saturating_duration_since(since) >= OPUS_COUNT_WAIT) {
                out = self.release();
            }
            if self.held.is_empty() {
                let pts = self.clock.pts_90k(true, opens.rtp, opens.rate, opens.arrival);
                self.muxer.mark_unbridged_step(pts);
            }
        }
        for f in primed.held {
            out.extend(self.mux(f.video, &f.data, f.rtp, f.rate, f.arrival));
        }
        out
    }

    /// The PCR run on as a picture arrives (`now`) that is held with the live
    /// pictures of a pull whose first picture was cached, the one shown first
    /// still to come. Not past the PCR the first of them will carry: that is
    /// no earlier than the one shown first of those that have come puts it.
    /// Nothing while the IDR itself is held, for the channel count. Padded
    /// with null packets to whole datagrams: the UDP, RTP and RIST outputs
    /// re-chunk to seven packets, and would hold the PCR until the pictures it
    /// is sent for went out.
    fn run_pcr_on(&mut self, now: std::time::Instant) -> Vec<MuxedFrame> {
        let Some(primed) = self.primed.as_ref().filter(|p| self.held.is_empty() && p.held.last().is_some_and(|f| f.video))
        else {
            return Vec::new();
        };
        let pictures = primed.pictures();
        let (Some(opens), Some(live)) = (pictures.first(), pictures.iter().min_by_key(|p| p.shown)) else {
            return Vec::new();
        };
        let behind = u64::try_from(opens.shown - live.shown).unwrap_or(0);
        let shown_first_at = live.frame.arrival;
        let ts = self.clock.start_90k(now).min(self.clock.start_90k(shown_first_at) + behind) & 0x1_FFFF_FFFF;
        let mut packets = self.muxer.run_pcr_on(ts);
        if packets.is_empty() {
            return Vec::new();
        }
        let null = crate::engine::flow::null_ts_packet().data;
        let whole = packets.len().next_multiple_of(crate::engine::packet::TS_PACKETS_PER_ITEM);
        packets.resize_with(whole, || null.slice(..188));
        vec![(packets, ts)]
    }

    /// The held video muxed, in order, and the channel count closed for the
    /// session.
    fn release(&mut self) -> Vec<MuxedFrame> {
        self.count_open = false;
        self.held_bytes = 0;
        self.held_since = None;
        std::mem::take(&mut self.held)
            .into_iter()
            .map(|(annex_b, pts, is_keyframe)| (self.muxer.mux_video(&annex_b, pts, pts, is_keyframe), pts))
            .collect()
    }

    /// One depayloaded frame of the video (`video`) or the audio track —
    /// RTP timestamp `rtp` on a `rate` Hz clock, arrived at `arrival` — as
    /// the frames to publish now, each as TS packets and the PTS it was
    /// stamped at: none for a frame with no payload, a picture held for the
    /// session's channel count ([`Self::session`]) or a frame held while the
    /// first of a pull's live pictures to be shown is still to come
    /// ([`PrimedIdr`]); the held frames ahead of the one that settles it.
    fn frame(
        &mut self,
        video: bool,
        data: &[u8],
        rtp: u64,
        rate: u64,
        arrival: std::time::Instant,
    ) -> Vec<MuxedFrame> {
        if data.is_empty() {
            return Vec::new();
        }
        let Some(primed) = &mut self.primed else {
            return self.mux(video, data, rtp, rate, arrival);
        };
        match primed.first {
            // The pull's first picture: a keyframe the one after it may show
            // cached, or a picture nothing is to be told by.
            None => {
                if video && holds_idr(data) {
                    primed.first = Some((rtp, arrival));
                } else if video {
                    self.primed = None;
                }
                return self.mux(video, data, rtp, rate, arrival);
            }
            // The second picture, which shows it: nothing is held until then,
            // nor after unless it does.
            Some(_) if primed.held.is_empty() => {
                if !video {
                    return self.mux(video, data, rtp, rate, arrival);
                }
                let order = self.param_sets.sps.as_deref().and_then(H264Order::of);
                let Some(order) = order.filter(|order| order.frame_num(data).is_some_and(|n| n > 1)) else {
                    self.primed = None;
                    return self.mux(video, data, rtp, rate, arrival);
                };
                primed.in_order = order.in_order;
            }
            Some(_) => {}
        }
        primed.held.push(ArrivedFrame { video, data: data.to_vec(), rtp, rate, arrival });
        primed.held_bytes += data.len();
        // The session's first Opus packet settles its channel count whether
        // or not it is held itself: the IDR held for it goes out.
        let mut out = if video { Vec::new() } else { self.settle_count(data[0]) };
        out.extend(self.settle_primed(arrival, false));
        out
    }

    /// An Opus packet whose TOC byte is `toc` has come: the session's first
    /// settles its channel count before its first PMT goes out, and the video
    /// held for it goes out behind that PMT. A stereo packet (the TOC's `s`
    /// bit, RFC 6716 §3.1) lists the Opus as stereo from here on, with a
    /// version bump once a PMT has gone out.
    fn settle_count(&mut self, toc: u8) -> Vec<MuxedFrame> {
        if !self.opus_stereo && toc & 0x04 != 0 {
            self.opus_stereo = true;
            if self.muxer.change_opus_channels(2) {
                tracing::info!("WebRTC Opus is stereo: the PMT lists it so (channel_config_code 2)");
            }
        }
        if self.count_open { self.release() } else { Vec::new() }
    }

    /// One frame ([`Self::frame`]) muxed, or held for the session's channel
    /// count.
    fn mux(
        &mut self,
        video: bool,
        data: &[u8],
        rtp: u64,
        rate: u64,
        arrival: std::time::Instant,
    ) -> Vec<MuxedFrame> {
        if video {
            // A depayloaded access unit, Annex B, with the parameter sets
            // ahead of every IDR (`H264ParamSets`), timed as it arrives.
            let pts = self.clock.pts_90k(true, rtp, rate, arrival);
            let (annex_b, is_keyframe) = self.param_sets.access_unit(data);
            if !self.count_open {
                return vec![(self.muxer.mux_video(&annex_b, pts, pts, is_keyframe), pts)];
            }
            let since = *self.held_since.get_or_insert(arrival);
            self.held_bytes += annex_b.len();
            self.held.push((annex_b, pts, is_keyframe));
            if self.held_bytes > HOLD_MAX_BYTES || arrival.saturating_duration_since(since) >= OPUS_COUNT_WAIT {
                return self.release();
            }
            Vec::new()
        } else {
            let mut out = self.settle_count(data[0]);
            // Opus: the RTP timestamp on the Opus clock (48 kHz, carried in
            // MediaTime.denom) onto the publish's 90 kHz timeline.
            let pts = if rate == 0 {
                self.last_audio_pts_90khz
            } else {
                self.clock.pts_90k(false, rtp, rate, arrival)
            };
            self.last_audio_pts_90khz = pts;
            out.push((self.muxer.mux_audio_opus(data, pts), pts));
            out
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
/// even that; str0m exposes them and doing so is a follow-up.) The video of a
/// pull whose first picture its server had cached is timed from its live
/// pictures instead ([`Self::retime_video`], [`PrimedIdr`]): that first
/// picture's arrival says nothing of them.
///
/// A later session's tracks ([`Self::session`]) are anchored the same way on
/// the same timeline — at their first frames' arrival after the input's
/// first frame — so the timeline runs on across sessions as the wall clock
/// did, never before a PTS the last session issued. It used to start again
/// at the lead, 20 s back after a 20 s publish. When a session would start
/// before that (the last one's sender clock ran fast), the whole session
/// is moved past it, both tracks by the same distance ([`Self::lift`]):
/// their offset is still their first frames' arrival difference — unless a
/// track's first frame arrived before the first frame processed, which
/// can put it behind the floor even so; that track starts on the floor.
#[cfg(any(feature = "webrtc", test))]
#[derive(Debug, Default)]
struct WhipClock {
    /// When the input's first frame of any track arrived: the timeline's
    /// origin, for every session.
    origin: Option<std::time::Instant>,
    /// The session's tracks: each one's first RTP timestamp and the PTS
    /// (90 kHz, unwrapped) it maps to.
    video: Option<(u64, u64)>,
    audio: Option<(u64, u64)>,
    /// The latest PTS issued (unwrapped).
    latest: u64,
    /// The earliest PTS a session's track may start at: past the last
    /// session's latest. A long session whose sender's clock ran fast would
    /// otherwise leave the next one, anchored on the wall clock, behind it.
    floor: u64,
    /// How far the session's tracks are moved past their arrival anchors:
    /// `floor` less the arrival anchor of the session's first frame
    /// *processed*, or 0 when that anchor is on or past the floor. Set by
    /// that frame, and added to both tracks' anchors. Each track's anchor
    /// plus the lift is then held at the floor at least, so no PTS goes
    /// behind one already issued: the first frame processed need not be
    /// the earliest to arrive, since str0m hands a picture over once it is
    /// complete, anchored at the arrival of its first packet, and an Opus
    /// packet at once — one that arrived while a multi-packet IDR was
    /// still arriving is processed first though the IDR's anchor is
    /// earlier. That IDR starts on the floor, closer to the audio than it
    /// arrived by at most the time it arrived ahead of it.
    lift: Option<u64>,
}

#[cfg(any(feature = "webrtc", test))]
impl WhipClock {
    /// The PTS (90 kHz, 33 bits) of a frame of the video or audio track:
    /// its RTP timestamp `rtp` (unwrapped) on a `rate` Hz clock, arrived at
    /// `arrival`.
    fn pts_90k(&mut self, video: bool, rtp: u64, rate: u64, arrival: std::time::Instant) -> u64 {
        let anchored = if video { self.video } else { self.audio };
        let (first, base) = match anchored {
            Some(anchor) => anchor,
            None => {
                let anchor = (rtp, self.start_90k(arrival));
                *if video { &mut self.video } else { &mut self.audio } = Some(anchor);
                anchor
            }
        };
        let ticks = (rtp as i128 - first as i128) * 90_000 / rate.max(1) as i128;
        let pts = (base as i128 + ticks).max(0) as u64;
        self.latest = self.latest.max(pts);
        pts & 0x1_FFFF_FFFF
    }

    /// The PTS (90 kHz, unwrapped) a track whose first frame arrived at
    /// `arrival` starts at: where that is on the timeline, moved past the
    /// last session's end with the rest of the session, and never behind it.
    fn start_90k(&mut self, arrival: std::time::Instant) -> u64 {
        let origin = *self.origin.get_or_insert(arrival);
        let late_us = arrival.saturating_duration_since(origin).as_micros() as u64;
        let at = crate::engine::rtmp::ts_mux::PCR_LEAD_90K + late_us * 9 / 100;
        let lift = *self.lift.get_or_insert(self.floor.saturating_sub(at));
        (at + lift).max(self.floor)
    }

    /// Time the video from the picture with RTP time `rtp`, arrived at
    /// `arrival`, as if it had been the session's first, and past the one
    /// that was ([`PrimedIdr`]).
    fn retime_video(&mut self, rtp: u64, arrival: std::time::Instant) {
        let start = self.start_90k(arrival);
        if let Some((_, first)) = self.video {
            self.video = Some((rtp, start.max(first + 1)));
        }
    }

    /// A new session's tracks start: each is anchored afresh, at its first
    /// frame, past every PTS issued so far.
    fn session(&mut self) {
        self.video = None;
        self.audio = None;
        self.lift = None;
        if self.origin.is_some() {
            self.floor = self.latest + 1;
        }
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

/// The NAL units of `data`, one depayloaded access unit: Annex B, start codes
/// included, or a bare NAL unit.
#[cfg(any(feature = "webrtc", test))]
fn nal_units(data: &[u8]) -> Vec<Vec<u8>> {
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        crate::engine::ts_demux::split_annex_b_nalus(data)
    } else if data.is_empty() {
        Vec::new()
    } else {
        vec![data.to_vec()]
    }
}

/// Whether the access unit `data` holds an IDR slice.
#[cfg(any(feature = "webrtc", test))]
fn holds_idr(data: &[u8]) -> bool {
    nal_units(data).iter().any(|n| n.first().is_some_and(|b| b & 0x1F == 5))
}

#[cfg(any(feature = "webrtc", test))]
impl H264ParamSets {
    /// `data` (one depayloaded access unit) as Annex B, with the cached SPS /
    /// PPS ahead of an IDR that carries neither of its own, and whether it
    /// holds an IDR slice. The unit's own parameter sets refresh the cache.
    fn access_unit(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        const START: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
        let nalus = nal_units(data);
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

    /// A later session's tracks run on from the last session's timeline as
    /// the wall clock did — 3 s after the input's first frame is 3 s on —
    /// and never before a PTS already issued: a session whose sender's clock
    /// ran fast (10 s of RTP time in 5 s) is followed past its own end. A
    /// clock per session started every one at the lead again.
    #[test]
    fn a_sessions_timeline_runs_on_from_the_last() {
        use super::WhipClock;
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut c = WhipClock::default();
        assert_eq!(c.pts_90k(false, 500, 48_000, t0), LEAD);
        assert_eq!(c.pts_90k(false, 48_500, 48_000, t0 + s(1)), LEAD + 90_000);
        c.session();
        assert_eq!(c.pts_90k(false, 77, 48_000, t0 + s(3)), LEAD + 270_000, "3 s on the wall clock");
        assert_eq!(c.pts_90k(true, 9_000, 90_000, t0 + s(3)), LEAD + 270_000, "video likewise");
        assert_eq!(c.pts_90k(false, 77 + 480_000, 48_000, t0 + s(8)), LEAD + 1_170_000);
        c.session();
        assert_eq!(c.pts_90k(false, 3, 48_000, t0 + s(9)), LEAD + 1_170_001, "past the last session's end");
    }

    /// A session the floor lifts — the last one's sender clock ran fast, so
    /// its tracks' arrival anchors fall behind a PTS already issued — is
    /// lifted whole: both tracks by the distance that puts its first frame
    /// on the floor, so the video still starts the 400 ms after the audio
    /// that it arrived. Each track lifted to the floor on its own put the
    /// audio there and left the video, past it, where it was: 300 ms closer
    /// together than they arrived, for the whole session.
    #[test]
    fn a_lifted_session_keeps_its_tracks_apart_as_they_arrived() {
        use super::WhipClock;
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut c = WhipClock::default();
        // Session 1: 10.5 s of the sender's clock in 10 s.
        assert_eq!(c.pts_90k(true, 0, 90_000, t0), LEAD);
        assert_eq!(c.pts_90k(true, 945_000, 90_000, t0 + ms(10_000)), LEAD + 945_000);
        c.session();
        // Session 2: audio at 10.2 s, video at 10.6 s on the wall clock.
        let audio = c.pts_90k(false, 7, 48_000, t0 + ms(10_200));
        assert_eq!(audio, LEAD + 945_001, "the session starts on the floor");
        let video = c.pts_90k(true, 50, 90_000, t0 + ms(10_600));
        assert_eq!(video - audio, 36_000, "400 ms apart, as they arrived");
        assert_eq!(c.pts_90k(false, 7 + 48_000, 48_000, t0 + ms(11_200)), audio + 90_000);
        // A session the floor does not reach is not lifted.
        c.session();
        let at = c.pts_90k(true, 1, 90_000, t0 + ms(60_000));
        assert_eq!(at, LEAD + 5_400_000);
        assert_eq!(c.pts_90k(false, 1, 48_000, t0 + ms(60_020)), at + 1_800);
    }

    /// A lifted session whose first frame processed is not its earliest:
    /// an Opus packet handed over while a multi-packet IDR, whose first
    /// packet arrived 5 ms before it, was still arriving (str0m hands a
    /// picture over once it is complete, anchored at its first packet).
    /// The lift the Opus sets put that IDR 5 ms behind the floor — behind a
    /// PTS the last session issued — and it starts on the floor instead. So
    /// does a track behind the floor in a session the floor did not lift,
    /// its first frame processed having arrived just past it.
    #[test]
    fn a_track_anchored_before_the_first_frame_processed_starts_on_the_floor() {
        use super::WhipClock;
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut c = WhipClock::default();
        // Session 1: 10.5 s of the sender's clock in 10 s.
        assert_eq!(c.pts_90k(true, 0, 90_000, t0), LEAD);
        assert_eq!(c.pts_90k(true, 945_000, 90_000, t0 + ms(10_000)), LEAD + 945_000);
        c.session();
        let floor = LEAD + 945_001;
        // Session 2: the Opus (arrived at 10.2 s) processed first, then the
        // IDR whose first packet arrived at 10.195 s.
        assert_eq!(c.pts_90k(false, 7, 48_000, t0 + ms(10_200)), floor, "the audio on the floor");
        assert_eq!(c.pts_90k(true, 50, 90_000, t0 + ms(10_195)), floor, "on the floor, not 5 ms behind it");
        assert_eq!(c.pts_90k(true, 50 + 3_600, 90_000, t0 + ms(10_235)), floor + 3_600, "then on its own clock");

        // An input whose first session ran on time, to 10.5 s; the next one's
        // Opus, processed first, arrived at 10.6 s, past the floor, so the
        // session is not lifted; its IDR's first packet arrived at 10.49 s.
        let mut c = WhipClock::default();
        assert_eq!(c.pts_90k(true, 0, 90_000, t0), LEAD);
        assert_eq!(c.pts_90k(true, 945_000, 90_000, t0 + ms(10_500)), LEAD + 945_000);
        c.session();
        let floor = LEAD + 945_001;
        assert_eq!(c.pts_90k(false, 7, 48_000, t0 + ms(10_600)), LEAD + 954_000, "not lifted");
        assert_eq!(c.pts_90k(true, 50, 90_000, t0 + ms(10_490)), floor, "on the floor, not 10 ms behind it");
    }

    /// An access unit of the video, at `ms` in, for [`WhipMux::frame`].
    fn video_at(w: &mut super::WhipMux, t0: std::time::Instant, ms: u64, nal: &[u8]) -> Vec<super::MuxedFrame> {
        w.frame(true, nal, 1_000 + ms * 90, 90_000, t0 + std::time::Duration::from_millis(ms))
    }

    /// An Opus packet with TOC `toc`, at `ms` in.
    fn opus_at(w: &mut super::WhipMux, t0: std::time::Instant, ms: u64, toc: u8) -> Vec<super::MuxedFrame> {
        let mut packet = vec![toc];
        packet.resize(40, 0xA5);
        w.frame(false, &packet, 77 + ms * 48, 48_000, t0 + std::time::Duration::from_millis(ms))
    }

    /// The Opus `channel_config_code` of every PMT in `muxed`, in turn.
    fn pmt_codes(muxed: &[super::MuxedFrame]) -> Vec<Option<u8>> {
        let ts: Vec<bytes::Bytes> = muxed.iter().flat_map(|(ts, _)| ts.iter().cloned()).collect();
        ts.iter()
            .filter(|p| crate::engine::ts_parse::ts_pid(p) == 0x1000)
            .map(|pmt| {
                let sec = &pmt[5..];
                let mut pos = 12 + ((((sec[10] & 0x0F) as usize) << 8) | sec[11] as usize);
                let end = 3 + ((((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize) - 4;
                let mut code = None;
                while pos + 5 <= end {
                    let info = (((sec[pos + 3] & 0x0F) as usize) << 8) | sec[pos + 4] as usize;
                    if sec[pos] == 0x06 {
                        code = crate::engine::ts_parse::opus_channel_config_code(&sec[pos + 5..pos + 5 + info]);
                    }
                    pos += 5 + info;
                }
                code
            })
            .collect()
    }

    /// A WebRTC input's muxer as [`super::opus_ts_muxer`] sets it up, laid
    /// out for video and audio.
    fn av_whip_mux() -> super::WhipMux {
        let mut muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
        muxer.set_audio_stream(0x06, Some(*b"Opus"));
        muxer.change_opus_channels(1);
        muxer.set_has_video(true);
        muxer.set_has_audio(true);
        super::WhipMux::new(muxer)
    }

    const IDR: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88];
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A];

    /// A session whose video leads — a WHEP pull from bilbycast-relay, primed
    /// with its cached IDR — holds that video until its first Opus packet
    /// settles the channel count, then sends it behind a PMT that already
    /// says so: one PMT, stereo, ahead of the IDR, nothing lost. The video
    /// went out at once behind a PMT listing mono, stereo following as a
    /// version bump the receivers that started on it ignore.
    #[test]
    fn a_session_led_by_video_holds_it_for_its_channel_count() {
        let t0 = std::time::Instant::now();
        let mut w = av_whip_mux();
        w.session(true, false);
        assert!(video_at(&mut w, t0, 0, IDR).is_empty(), "the IDR is held");
        assert_eq!(w.hold_deadline(), Some(t0 + super::OPUS_COUNT_WAIT));
        assert!(w.release_due(t0 + std::time::Duration::from_millis(15)).is_empty());
        let out = opus_at(&mut w, t0, 20, 0xFC);
        assert_eq!(out.len(), 2, "the IDR, then the Opus");
        assert_eq!(out[0].1, crate::engine::rtmp::ts_mux::PCR_LEAD_90K, "the IDR at the session's start");
        assert_eq!(pmt_codes(&out), [Some(2)], "one PMT, stereo");
        assert_eq!(w.hold_deadline(), None);
        // Then as ever: each frame out as it comes.
        assert_eq!(video_at(&mut w, t0, 40, P).len(), 1);
        assert_eq!(opus_at(&mut w, t0, 40, 0xFC).len(), 1);
        // A mono first packet settles it mono.
        let mut w = av_whip_mux();
        w.session(true, false);
        assert!(video_at(&mut w, t0, 0, IDR).is_empty());
        assert_eq!(pmt_codes(&opus_at(&mut w, t0, 20, 0x78)), [Some(1)]);
    }

    /// A session whose audio does not come within `OPUS_COUNT_WAIT` has its
    /// video sent then, as it was before the hold — on the next picture or
    /// the input's timer (`release_due`), whichever comes first — and one
    /// whose video outgrows `HOLD_MAX_BYTES` at once.
    #[test]
    fn a_held_session_is_released_by_its_bounds() {
        let t0 = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        let mut w = av_whip_mux();
        w.session(true, false);
        assert!(video_at(&mut w, t0, 0, IDR).is_empty());
        assert!(video_at(&mut w, t0, 280, P).is_empty());
        assert!(w.release_due(t0 + ms(299)).is_empty());
        let out = w.release_due(t0 + ms(300));
        assert_eq!(out.iter().map(|f| f.1).collect::<Vec<_>>(), [crate::engine::rtmp::ts_mux::PCR_LEAD_90K, crate::engine::rtmp::ts_mux::PCR_LEAD_90K + 25_200]);
        assert_eq!(pmt_codes(&out), [Some(1)], "listed as before the hold");
        // A stereo packet later bumps it, as it always did.
        assert_eq!(pmt_codes(&opus_at(&mut w, t0, 320, 0xFC)), [Some(2)]);

        // The next picture past the wait releases it too.
        let mut w = av_whip_mux();
        w.session(true, false);
        assert!(video_at(&mut w, t0, 0, IDR).is_empty());
        assert_eq!(video_at(&mut w, t0, 300, P).len(), 2);

        // So does more than `HOLD_MAX_BYTES` of it.
        let mut w = av_whip_mux();
        w.session(true, false);
        let mut big = IDR.to_vec();
        big.resize(super::HOLD_MAX_BYTES / 2, 0x11);
        assert!(video_at(&mut w, t0, 0, &big).is_empty());
        assert!(video_at(&mut w, t0, 40, &big).is_empty());
        assert_eq!(video_at(&mut w, t0, 80, P).len(), 3, "past the byte bound");

        // And the session ending (`end_session`).
        let mut w = av_whip_mux();
        w.session(true, false);
        assert!(video_at(&mut w, t0, 0, IDR).is_empty());
        assert_eq!(w.end_session().len(), 1);
    }

    /// Nothing is held where the count is known or there is no count to
    /// wait for: a sender declaring stereo (`sprop-stereo=1`), a session
    /// negotiating no audio, and a session onto a TS that lists stereo
    /// already — a reconnecting pull of a stereo feed, which runs on with
    /// no version bump. It went stereo, mono and stereo again within a
    /// frame of every reconnect.
    #[test]
    fn nothing_is_held_where_the_count_is_settled() {
        let t0 = std::time::Instant::now();
        let mut w = av_whip_mux();
        w.session(true, true);
        let out = video_at(&mut w, t0, 0, IDR);
        assert_eq!(pmt_codes(&out), [Some(2)], "declared stereo, at once");
        let mut w = av_whip_mux();
        w.session(false, false);
        assert_eq!(pmt_codes(&video_at(&mut w, t0, 0, IDR)), [Some(1)], "no audio: at once");

        let mut w = av_whip_mux();
        w.session(true, false);
        assert!(video_at(&mut w, t0, 0, IDR).is_empty());
        let mut sent = opus_at(&mut w, t0, 20, 0xFC);
        assert!(w.end_session().is_empty());
        w.session(true, false);
        sent.extend(video_at(&mut w, t0, 5_000, IDR));
        sent.extend(opus_at(&mut w, t0, 5_020, 0x78));
        assert_eq!(sent.len(), 4, "the reconnect's IDR out at once");
        assert_eq!(pmt_codes(&sent), [Some(2), Some(2)], "stereo throughout");
        let versions: std::collections::BTreeSet<u8> = sent
            .iter()
            .flat_map(|(ts, _)| ts.iter())
            .filter(|p| crate::engine::ts_parse::ts_pid(p) == 0x1000)
            .map(|p| (p[10] >> 1) & 0x1F)
            .collect();
        assert_eq!(versions.len(), 1, "no version bump: {versions:?}");
    }

    /// The sender's description of its Opus says stereo with
    /// `sprop-stereo=1` on its audio m-line's Opus PT (OBS); Chrome's, and
    /// any other that says nothing, is mono (RFC 7587's default).
    #[cfg(feature = "webrtc")]
    #[test]
    fn a_stereo_opus_sender_is_read_off_its_sdp() {
        let sdp = |fmtp: &str| {
            format!(
                "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 H264/90000\r\n\
                 a=fmtp:111 sprop-stereo=1\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111 63\r\n\
                 a=rtpmap:111 opus/48000/2\r\na=rtpmap:63 red/48000/2\r\na=fmtp:{fmtp}\r\n"
            )
        };
        assert!(super::sdp_declares_stereo_opus(&sdp(
            "111 minptime=10;maxaveragebitrate=96000;stereo=1;sprop-stereo=1;useinbandfec=1"
        )));
        assert!(!super::sdp_declares_stereo_opus(&sdp("111 minptime=10;useinbandfec=1")));
        assert!(!super::sdp_declares_stereo_opus(&sdp("111 stereo=1")), "what it takes, not sends");
        assert!(!super::sdp_declares_stereo_opus(&sdp("63 sprop-stereo=1")), "not the Opus PT");
        assert!(!super::sdp_declares_stereo_opus(&sdp("111 sprop-stereo=0")));
    }

    /// The Opus `channel_config_code` each PMT in `ts` lists, in turn, with
    /// its version.
    #[cfg(feature = "webrtc")]
    fn opus_channels_of(ts: &[bytes::Bytes]) -> Vec<(u8, Option<u8>)> {
        let mut seen = Vec::new();
        for pmt in ts.iter().filter(|p| crate::engine::ts_parse::ts_pid(p) == 0x1000) {
            let sec = &pmt[5..];
            let len = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
            let mut pos = 12;
            while pos + 5 <= 3 + len - 4 {
                let info_len = (((sec[pos + 3] & 0x0F) as usize) << 8) | sec[pos + 4] as usize;
                if sec[pos] == 0x06 {
                    let code = crate::engine::ts_parse::opus_channel_config_code(&sec[pos + 5..pos + 5 + info_len]);
                    let entry = ((sec[5] >> 1) & 0x1F, code);
                    if seen.last() != Some(&entry) {
                        seen.push(entry);
                    }
                }
                pos += 5 + info_len;
            }
        }
        seen
    }

    /// A publisher after another runs on the same TS: every PID's
    /// continuity counter continues, the PCR never steps back, and the step
    /// to the new publisher's first PCR — the gap no PCR spanned — is the
    /// one carrying `discontinuity_indicator`. The PMT lists the first
    /// publisher's mono Opus as mono and the second's stereo as stereo
    /// (`channel_config_code` 1, then 2 with a version bump). Each publisher
    /// used to get a muxer of its own: CC, PTS and PCR from zero with no
    /// DI, and an Opus entry with no channel count.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_republish_runs_on_the_same_ts_with_its_step_marked() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::engine::ts_parse::{extract_pcr, ts_cc, ts_discontinuity_indicator, ts_has_payload, ts_pid};
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

        let mut ts = Vec::new();
        // An audio-only publisher sending a second of Opus whose TOC is
        // `toc` (0x78: 20 ms CELT mono; 0xFC: 20 ms CELT stereo), then
        // leaving (DTLS close_notify).
        for toc in [0x78u8, 0xFC] {
            let peer = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
            let mut publisher = WebrtcSession::new(&peer).await.unwrap();
            let (offer, pending) = publisher.create_offer(false, true, true).unwrap();
            let (reply, answer) = tokio::sync::oneshot::channel();
            session_tx.send(NewSessionMsg { offer_sdp: offer, reply }).await.unwrap();
            let (answer, _session, _cancel) = answer.await.unwrap().unwrap();
            publisher.apply_answer(&answer, pending).unwrap();
            let audio_mid = publisher.audio_mid.unwrap();
            let audio_pt = publisher.get_pt(audio_mid).expect("Opus accepted");
            let connected = tokio::time::timeout(Duration::from_secs(10), async {
                while !matches!(publisher.poll_event(&cancel).await, SessionEvent::Connected) {}
            })
            .await;
            assert!(connected.is_ok(), "the publisher never connected");
            let mut frame = vec![toc];
            frame.resize(80, 0xA5);
            for k in 0..50u64 {
                let at = str0m::media::MediaTime::new(k * 960, str0m::media::Frequency::FORTY_EIGHT_KHZ);
                publisher.write_media(audio_mid, audio_pt, Instant::now(), at, &frame).unwrap();
                publisher.drain_outputs().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = publisher.drive_udp_io().await;
                while let Ok(pkt) = published.try_recv() {
                    ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
                }
            }
            publisher.close().await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        while let Ok(pkt) = published.try_recv() {
            ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
        }

        assert!(carries_opus(&ts), "no Opus was published ({} packets)", ts.len());
        for pid in [0x0000, 0x1000, 0x0101] {
            let ccs: Vec<u8> = ts.iter().filter(|p| ts_pid(p) == pid && ts_has_payload(p)).map(|p| ts_cc(p)).collect();
            assert!(ccs.len() > 4, "PID 0x{pid:04X}: {} packets", ccs.len());
            for w in ccs.windows(2) {
                assert_eq!(w[1], (w[0] + 1) & 0x0F, "PID 0x{pid:04X} CC breaks: {ccs:?}");
            }
        }
        let pcrs: Vec<(u64, bool)> =
            ts.iter().filter_map(|p| extract_pcr(p).map(|pcr| (pcr / 300, ts_discontinuity_indicator(p)))).collect();
        assert!(pcrs.windows(2).all(|w| w[1].0 >= w[0].0), "the PCR stepped back: {pcrs:?}");
        let marked: Vec<usize> = (0..pcrs.len()).filter(|&i| pcrs[i].1).collect();
        let step = (1..pcrs.len()).max_by_key(|&i| pcrs[i].0 - pcrs[i - 1].0).unwrap();
        assert_eq!(marked, [step], "one DI, on the PCR after the gap: {pcrs:?}");
        assert!(pcrs[step].0 - pcrs[step - 1].0 > 9_000, "a gap of {} ticks", pcrs[step].0 - pcrs[step - 1].0);
        let channels = opus_channels_of(&ts);
        assert_eq!(channels.first().map(|c| c.1), Some(Some(1)), "{channels:?}");
        assert_eq!(channels.last().map(|c| c.1), Some(Some(2)), "{channels:?}");
        assert_ne!(channels.first().map(|c| c.0), channels.last().map(|c| c.0), "the version moved: {channels:?}");
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
                self.ts.extend(self.mux.frame(video, data, rtp, rate, at).into_iter().flat_map(|(ts, _)| ts));
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
        assert!(w.frame(false, &[], 123_456, 48_000, t0).is_empty(), "a probe on the audio");
        assert!(w.frame(true, &[], 3_000_000_000, 90_000, t0).is_empty(), "and on the video");
        // The media, 40 ms on: the publish's timeline opens at its first
        // frame, not at the probes.
        let at = t0 + Duration::from_millis(40);
        let [(ts, pts)] = &w.frame(true, &[0, 0, 0, 1, 0x65, 0x88], 3_000_003_600, 90_000, at)[..] else {
            panic!("one frame")
        };
        let (ts, pts) = (ts.clone(), *pts);
        assert_eq!(pts, LEAD);
        assert!(!ts.is_empty());
        let [(ts, pts)] = &w.frame(false, &[0xFC; 80], 123_456 + 1_920, 48_000, at)[..] else {
            panic!("one frame")
        };
        let (ts, pts) = (ts.clone(), *pts);
        assert_eq!(pts, LEAD);
        // The Opus AU carries its 80 bytes: control header 0x7F 0xE0, then
        // au_size.
        let pkt = ts.iter().flat_map(|b| b.chunks(188)).find(|p| (p[1] & 0x40) != 0 && ((p[1] as u16 & 0x1F) << 8 | p[2] as u16) == 0x0101);
        let pkt = pkt.expect("the audio PES");
        let es = crate::engine::ts_parse::pes_payload_offset(pkt).expect("a PES header");
        assert_eq!(&pkt[es..es + 3], &[0x7F, 0xE0, 80]);
        assert!(w.frame(false, &[], 123_456 + 3_840, 48_000, at).is_empty(), "a probe mid-stream");
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
    /// had, the resources DELETEd from it, and when it wrote its first
    /// picture to a pull.
    #[cfg(feature = "webrtc")]
    struct WhepServer {
        addr: std::net::SocketAddr,
        posts: Arc<AtomicUsize>,
        deletes: Arc<std::sync::Mutex<Vec<String>>>,
        first_picture: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
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
        serve_whep_scripted(
            edit,
            WhepScript { video: send.0, audio: send.1, for_ms, ..WhepScript::default() },
            None,
        )
        .await
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
        serve_whep_scripted(
            edit,
            WhepScript { video: send.0, audio: send.1, for_ms, ..WhepScript::default() },
            opus,
        )
        .await
    }

    /// What a [`serve_whep_scripted`] server does with each pull.
    #[cfg(feature = "webrtc")]
    #[derive(Clone, Copy, Default)]
    struct WhepScript {
        /// Send H.264 (a picture every 40 ms) and / or Opus (a 20 ms frame
        /// every 20 ms) once connected, for `for_ms`.
        video: bool,
        audio: bool,
        for_ms: u64,
        /// The Opus starts this far in (the video at once): a server priming
        /// its viewer with a cached IDR, as bilbycast-relay does.
        audio_from_ms: u64,
        /// Only the first picture is an IDR (SPS + PPS + IDR slice), the
        /// rest P slices; else every picture is an IDR.
        gop: bool,
        /// End the session (DTLS close_notify) once `for_ms` is up, as a
        /// server whose stream closes does.
        close: bool,
        /// Send the first picture, an IDR, and no other: a server priming
        /// its viewer with a cached IDR whose source has stalled.
        one_picture: bool,
        /// Stamp the first picture this much older than it goes out: an IDR
        /// the server had cached, as bilbycast-relay primes a viewer with.
        cached_ms: u64,
        /// Answer a DELETE after this long (0: at once).
        delete_after_ms: u64,
        /// Never answer a DELETE (note it, then hang): a dead server.
        delete_hangs: bool,
    }

    /// A WHEP server on loopback answering each pull as `script` says (see
    /// [`serve_whep`]).
    #[cfg(feature = "webrtc")]
    async fn serve_whep_scripted(
        edit: fn(String) -> String,
        script: WhepScript,
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
        let first_picture = Arc::new(std::sync::Mutex::new(None));
        let app = axum::Router::new().route(
            "/whep/{resource}",
            axum::routing::delete({
                let deletes = deletes.clone();
                move |axum::extract::Path(resource): axum::extract::Path<String>| async move {
                    if script.delete_hangs {
                        deletes.lock().unwrap().push(resource.clone());
                        std::future::pending::<()>().await;
                    }
                    tokio::time::sleep(Duration::from_millis(script.delete_after_ms)).await;
                    deletes.lock().unwrap().push(resource);
                    StatusCode::OK
                }
            }),
        );
        let app = app.route(
            "/whep",
            axum::routing::post({
                let posts = posts.clone();
                let first_picture = first_picture.clone();
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
                        for k in 0..script.for_ms / 20 {
                            if let (true, Some((mid, pt))) = (script.audio && k * 20 >= script.audio_from_ms, audio) {
                                let at = MediaTime::new(k * 960, Frequency::FORTY_EIGHT_KHZ);
                                let packet = opus.as_ref().map(|p| &p[k as usize % p.len()][..]);
                                let _ = server.write_media(mid, pt, Instant::now(), at, packet.unwrap_or(&[0xFC; 80]));
                                server.drain_outputs().await;
                            }
                            let more = k == 0 || !script.one_picture;
                            if let (true, 0, true, Some((mid, pt))) = (script.video, k % 2, more, video) {
                                let newer = if k == 0 { 0 } else { script.cached_ms * 90 };
                                let at = MediaTime::new(newer + k / 2 * 3_600, Frequency::NINETY_KHZ);
                                // The pictures missed behind a cached IDR are 25 fps ones.
                                let picture = match k == 0 || !script.gop {
                                    true => IDR.to_vec(),
                                    false => p_slice((script.cached_ms / 40 + k / 2) as u32),
                                };
                                let written = Instant::now();
                                let _ = server.write_media(mid, pt, written, at, &picture);
                                server.drain_outputs().await;
                                first_picture.lock().unwrap().get_or_insert(written);
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            let _ = server.drive_udp_io().await;
                        }
                        if script.close {
                            server.close().await;
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
        WhepServer { addr, posts, deletes, first_picture, task }
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
        let (ts, raised, _) = pull_whep_timed(addr, run_ms, extra).await;
        (ts, raised)
    }

    /// [`pull_whep_with`], and how long the input took to stop: from its
    /// cancel to its task's return (at most 5 s, as a stopping flow waits).
    #[cfg(feature = "webrtc")]
    async fn pull_whep_timed(
        addr: std::net::SocketAddr,
        run_ms: u64,
        extra: serde_json::Value,
    ) -> (Vec<bytes::Bytes>, Vec<crate::manager::events::Event>, std::time::Duration) {
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
        let stopping = std::time::Instant::now();
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        let took = stopping.elapsed();
        let mut ts = Vec::new();
        while let Ok(pkt) = published.try_recv() {
            ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
        }
        let mut raised_events = Vec::new();
        while let Ok(ev) = raised.try_recv() {
            raised_events.push(ev);
        }
        (ts, raised_events, took)
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

    /// Each video PES on PID 0x0100 of `ts`: its PTS and the NAL unit types
    /// of its access unit.
    #[cfg(feature = "webrtc")]
    fn video_pes_of(ts: &[bytes::Bytes]) -> Vec<(u64, Vec<u8>)> {
        use crate::engine::ts_parse::{extract_pes_pts, pes_payload_offset, ts_payload_offset, ts_pid};
        let mut pes: Vec<(u64, Vec<u8>)> = Vec::new();
        for p in ts.iter().filter(|p| ts_pid(p) == 0x0100) {
            if (p[1] & 0x40) != 0 {
                let (Some(pts), Some(at)) = (extract_pes_pts(p), pes_payload_offset(p)) else {
                    continue;
                };
                pes.push((pts, p[at..].to_vec()));
            } else if let Some(last) = pes.last_mut()
                && p[3] & 0x10 != 0
            {
                last.1.extend_from_slice(&p[ts_payload_offset(p)..]);
            }
        }
        pes.into_iter().map(|(pts, au)| (pts, nal_types(&au))).collect()
    }

    /// The WHEP pull of a stereo feed from bilbycast-relay, and of every
    /// server priming a viewer with its cached IDR: video first, the first
    /// Opus 20 ms on. Its first PMT already lists the Opus as stereo
    /// (`channel_config_code` 2), the IDR held for that packet and sent
    /// behind it, not dropped — and after the server closes the stream, the
    /// next pull of it runs on stereo with no version bump, its IDR sent at
    /// once. The first PMT listed mono and stereo followed as a version bump,
    /// which ffmpeg and GStreamer's tsdemux ignore: they read the feed as
    /// mono throughout. Each reconnect then went mono and stereo again.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whep_pull_led_by_video_lists_its_stereo_from_the_first_pmt() {
        let script = WhepScript {
            video: true,
            audio: true,
            for_ms: 800,
            audio_from_ms: 20,
            gop: true,
            close: true,
            ..WhepScript::default()
        };
        let server = serve_whep_scripted(|a| a, script, None).await;
        // Pulls at 0 s and, after the 1 s backoff, at ~1.9 s.
        let (ts, _) = pull_whep(server.addr, 3_300).await;
        server.task.abort();
        let pulls = server.posts.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(pulls, 2, "two pulls");
        assert!(carries_opus(&ts), "no Opus was published ({} packets)", ts.len());
        assert_eq!(opus_channels_of(&ts), [(0, Some(2))], "stereo from the first PMT, one version");
        let pes = video_pes_of(&ts);
        let idrs: Vec<usize> = (0..pes.len()).filter(|&i| pes[i].1.contains(&5)).collect();
        assert_eq!(idrs.len(), pulls, "each pull's IDR published: {pes:?}");
        assert_eq!(idrs[0], 0, "the first picture published is the IDR");
        assert_eq!(pes[0].0, crate::engine::rtmp::ts_mux::PCR_LEAD_90K, "at the timeline's origin");
        let gap = (1..pes.len()).max_by_key(|&i| pes[i].0 - pes[i - 1].0).unwrap();
        assert_eq!(idrs[1], gap, "the second pull's first picture is its IDR: {pes:?}");
    }

    /// A server whose answer declares its Opus stereo (`sprop-stereo=1`)
    /// is listed stereo from the first PMT with nothing held, its video out
    /// as it comes though it sends no audio at all.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whep_server_declaring_stereo_is_listed_so_at_once() {
        let declared = |a: String| a.replace("useinbandfec=1", "useinbandfec=1;sprop-stereo=1");
        let script = WhepScript { video: true, for_ms: 1_000, gop: true, ..WhepScript::default() };
        let server = serve_whep_scripted(declared, script, None).await;
        let (ts, _) = pull_whep(server.addr, 1_300).await;
        server.task.abort();
        assert_eq!(opus_channels_of(&ts), [(0, Some(2))]);
        let pes = video_pes_of(&ts);
        assert!(pes.first().is_some_and(|(pts, nals)| *pts == crate::engine::rtmp::ts_mux::PCR_LEAD_90K
            && nals.contains(&5)), "{pes:?}");
    }

    /// A WHEP input stopping — a flow stop, the input's removal, a node
    /// shutting down: its cancel — has released its pull (the server has
    /// answered the DELETE) by the time its task returns, which is what a
    /// stopping flow and a shutdown wait for. The DELETE was spawned, and the
    /// task returned before it was answered: a shutdown's `_exit` cut it
    /// short, and the server kept the dead pull until its own timeout.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_stopping_whep_input_has_released_its_pull_when_it_returns() {
        let script = WhepScript { video: true, audio: true, for_ms: 5_000, delete_after_ms: 300, ..WhepScript::default() };
        let server = serve_whep_scripted(|a| a, script, None).await;
        let (ts, _, took) = pull_whep_timed(server.addr, 1_000, serde_json::json!({})).await;
        let deleted = server.deletes.lock().unwrap().clone();
        server.task.abort();
        assert!(!ts.is_empty(), "the pull published nothing");
        assert_eq!(deleted, ["res-0"], "answered before the task returned (in {took:?})");
        assert!(took >= std::time::Duration::from_millis(300), "stopped in {took:?}");
    }

    /// A server that never answers the DELETE holds the input's stop no
    /// longer than `WHEP_DELETE_BOUND`: a dead server must not stall a flow
    /// stop or a node shutdown (a stopping flow gives each input task 5 s).
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn an_unanswered_whep_release_holds_the_stop_only_to_its_bound() {
        let script = WhepScript { video: true, audio: true, for_ms: 5_000, delete_hangs: true, ..WhepScript::default() };
        let server = serve_whep_scripted(|a| a, script, None).await;
        let (_, _, took) = pull_whep_timed(server.addr, 1_000, serde_json::json!({})).await;
        let deleted = server.deletes.lock().unwrap().clone();
        server.task.abort();
        assert_eq!(deleted, ["res-0"], "the DELETE was sent");
        assert!(
            took >= super::WHEP_DELETE_BOUND && took < super::WHEP_DELETE_BOUND + std::time::Duration::from_secs(1),
            "stopped after {took:?}"
        );
    }

    /// What a WebRTC input published (`published`) for a session holding its
    /// one picture for the Opus channel count, the picture written by its
    /// sender at `written`: 50 ms on, by the wall clock, nothing — the
    /// picture has reached the input, which the sender and the test share a
    /// thread with, and is held; then, on tokio's paused clock, which moves
    /// only to the next timer due, what went out first, when it went (`at`),
    /// and when the clock was paused, after the picture arrived.
    #[cfg(feature = "webrtc")]
    async fn published_once_the_hold_runs_out(
        published: &mut tokio::sync::broadcast::Receiver<crate::engine::packet::RtpPacket>,
        written: std::time::Instant,
    ) -> (Vec<bytes::Bytes>, tokio::time::Instant, tokio::time::Instant) {
        use tokio::sync::broadcast::error::TryRecvError;
        use tokio::time::Instant;
        tokio::time::sleep_until(Instant::from_std(written + std::time::Duration::from_millis(50))).await;
        assert!(matches!(published.try_recv(), Err(TryRecvError::Empty)), "the picture was not held");
        tokio::time::pause();
        let paused = Instant::now();
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), published.recv()).await;
        let at = Instant::now();
        tokio::time::resume();
        let first = first
            .unwrap_or_else(|_| panic!("nothing published within 1 s of the pause: the held picture was never released"))
            .unwrap();
        let mut ts: Vec<bytes::Bytes> = first.data.chunks(188).map(bytes::Bytes::copy_from_slice).collect();
        while let Ok(pkt) = published.try_recv() {
            ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
        }
        (ts, at, paused)
    }

    /// The held picture went out as its wait ran out ([`published_once_the_hold_runs_out`]):
    /// no sooner than `OPUS_COUNT_WAIT` after it was written, no later than
    /// that after the pause (it had arrived by then), as the IDR at the
    /// timeline's origin behind the first PMT, which lists the Opus as the TS
    /// did before the hold — mono.
    #[cfg(feature = "webrtc")]
    fn assert_sent_as_the_wait_ran_out(
        ts: &[bytes::Bytes],
        written: std::time::Instant,
        at: tokio::time::Instant,
        paused: tokio::time::Instant,
    ) {
        let wait = super::OPUS_COUNT_WAIT;
        let written = tokio::time::Instant::from_std(written);
        assert!(at >= written + wait, "out {:?} after it was written, inside the wait", at - written);
        // A timer fires on the first millisecond tick at or past its deadline.
        let tick = std::time::Duration::from_millis(1);
        assert!(at <= paused + wait + tick, "out {:?} after the pause, past the wait", at - paused);
        let both = vec![(0x1B, 0x0100), (0x06, 0x0101)];
        assert_eq!(pmts_of(ts), vec![(0, 0x0100, both)], "the first PMT");
        assert_eq!(opus_channels_of(ts), [(0, Some(1))], "the Opus listed as before the hold");
        let pes = video_pes_of(ts);
        assert!(
            pes.len() == 1 && pes[0].0 == crate::engine::rtmp::ts_mux::PCR_LEAD_90K && pes[0].1.contains(&5),
            "the IDR at the timeline's origin: {pes:?}"
        );
    }

    /// A WHEP server that negotiates Opus without declaring its channel
    /// count, sends one IDR — a primed cached picture — and then nothing:
    /// the input holds the IDR for the Opus packet that never comes, and its
    /// receive loop's timer sends it, behind the first PMT, as
    /// `OPUS_COUNT_WAIT` runs out. Without that timer the IDR waited for a
    /// next picture, audio or the end of the pull, however long that took.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whep_pull_whose_audio_never_comes_sends_its_held_idr_as_the_wait_runs_out() {
        use std::time::Duration;
        let script = WhepScript { video: true, for_ms: 30_000, one_picture: true, ..WhepScript::default() };
        let server = serve_whep_scripted(|a| a, script, None).await;
        let config: crate::config::models::WhepInputConfig =
            serde_json::from_value(serde_json::json!({ "whep_url": format!("http://{}/whep", server.addr) })).unwrap();
        let (broadcast_tx, mut published) = tokio::sync::broadcast::channel(16_384);
        let stats = Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
            "flow-w".into(),
            "WHEP".into(),
            "webrtc".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (events, _raised) = crate::manager::events::event_channel();
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
        let written = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(at) = *server.first_picture.lock().unwrap() {
                    break at;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the server never sent its picture");
        let (ts, at, paused) = published_once_the_hold_runs_out(&mut published, written).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        server.task.abort();
        assert_sent_as_the_wait_ran_out(&ts, written, at, paused);
    }

    /// Two WHIP publishers in turn, the first's sender clock running four
    /// times fast (4 s of RTP time in 1 s), so the second's arrival anchors
    /// fall behind the last PTS issued and the floor lifts its session. The
    /// second sends its audio first and its video 200 ms later: on the TS
    /// they are still 200 ms apart, and both run on past the first
    /// publisher's last PTS. Each track lifted to the floor on its own put
    /// both on the floor, the video 200 ms early against the audio; a
    /// publisher per muxer started the second again at the lead, behind the
    /// first.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_republish_lifted_past_a_fast_clock_keeps_its_tracks_apart() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::engine::ts_parse::{extract_pes_pts, pes_payload_offset, ts_pid};
        use crate::engine::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
        use std::time::{Duration, Instant};
        use str0m::media::{Frequency, MediaTime};

        let config: crate::config::models::WebrtcInputConfig =
            serde_json::from_value(serde_json::json!({ "public_ip": "127.0.0.1" })).unwrap();
        let (broadcast_tx, mut published) = tokio::sync::broadcast::channel(16_384);
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

        let mut ts = Vec::new();
        // Publisher 1: audio alone, TOC 0x78, 80 ms of RTP time every 20 ms.
        // Publisher 2: audio (TOC 0x68) from the start, an IDR every 40 ms
        // from 200 ms on, on its own clock.
        for second in [false, true] {
            let peer = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
            let mut publisher = WebrtcSession::new(&peer).await.unwrap();
            let (offer, pending) = publisher.create_offer(second, true, true).unwrap();
            let (reply, answer) = tokio::sync::oneshot::channel();
            session_tx.send(NewSessionMsg { offer_sdp: offer, reply }).await.unwrap();
            let (answer, _session, _cancel) = answer.await.unwrap().unwrap();
            publisher.apply_answer(&answer, pending).unwrap();
            let connected = tokio::time::timeout(Duration::from_secs(10), async {
                while !matches!(publisher.poll_event(&cancel).await, SessionEvent::Connected) {}
            })
            .await;
            assert!(connected.is_ok(), "the publisher never connected");
            publisher.drain_pending_events();
            let audio_mid = publisher.audio_mid.unwrap();
            let audio_pt = publisher.get_pt(audio_mid).expect("Opus accepted");
            let video = publisher.video_mid.and_then(|mid| Some((mid, publisher.get_pt(mid)?)));
            let mut opus = vec![if second { 0x68 } else { 0x78 }];
            opus.resize(80, 0xA5);
            for k in 0..50u64 {
                let step = if second { 960 } else { 3_840 };
                let at = MediaTime::new(k * step, Frequency::FORTY_EIGHT_KHZ);
                publisher.write_media(audio_mid, audio_pt, Instant::now(), at, &opus).unwrap();
                if let (true, true, Some((mid, pt))) = (second, k >= 10 && k % 2 == 0, video) {
                    let at = MediaTime::new(5_000 + (k - 10) / 2 * 3_600, Frequency::NINETY_KHZ);
                    publisher.write_media(mid, pt, Instant::now(), at, IDR).unwrap();
                }
                publisher.drain_outputs().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = publisher.drive_udp_io().await;
                while let Ok(pkt) = published.try_recv() {
                    ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
                }
            }
            publisher.close().await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        while let Ok(pkt) = published.try_recv() {
            ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
        }

        // Each audio PES: its PTS and its Opus TOC (after the control
        // header 0x7F 0xE0 and the one-byte size).
        let audio: Vec<(u64, u8)> = ts
            .iter()
            .filter(|p| ts_pid(p) == 0x0101 && (p[1] & 0x40) != 0)
            .filter_map(|p| Some((extract_pes_pts(p)?, p[pes_payload_offset(p)? + 3])))
            .collect();
        let first: Vec<u64> = audio.iter().filter(|a| a.1 == 0x78).map(|a| a.0).collect();
        let second: Vec<u64> = audio.iter().filter(|a| a.1 == 0x68).map(|a| a.0).collect();
        let video = video_pes_of(&ts);
        assert!(first.len() > 40 && second.len() > 40 && video.len() > 10, "{} / {} / {}", first.len(), second.len(), video.len());
        let last = *first.iter().max().unwrap();
        assert!(last > 300_000, "the first publisher ran 4 s of PTS: {last}");
        assert_eq!(second[0], last + 1, "the second session starts on the floor");
        let apart = video[0].0 as i64 - second[0] as i64;
        assert!((14_400..=27_000).contains(&apart), "video {apart} ticks after the audio, sent 200 ms after");
    }

    /// A WHIP publisher that negotiates Opus without declaring its channel
    /// count (as Chrome does), sends one IDR and then nothing: the input
    /// holds the IDR for the Opus packet that never comes, and its receive
    /// loop's timer sends it, behind the first PMT, as `OPUS_COUNT_WAIT` runs
    /// out. Without that timer the IDR waited for a next picture, audio or
    /// the end of the publish, however long that took.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whip_publish_whose_audio_never_comes_sends_its_held_idr_as_the_wait_runs_out() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::engine::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
        use std::time::{Duration, Instant};
        use str0m::media::{Frequency, MediaTime};

        let config: crate::config::models::WebrtcInputConfig =
            serde_json::from_value(serde_json::json!({ "public_ip": "127.0.0.1" })).unwrap();
        let (broadcast_tx, mut published) = tokio::sync::broadcast::channel(16_384);
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

        let peer = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
        let mut publisher = WebrtcSession::new(&peer).await.unwrap();
        let (offer, pending) = publisher.create_offer(true, true, true).unwrap();
        assert!(!super::sdp_declares_stereo_opus(&offer), "the offer declares no channel count");
        let (reply, answer) = tokio::sync::oneshot::channel();
        session_tx.send(NewSessionMsg { offer_sdp: offer, reply }).await.unwrap();
        let (answer, _session, _cancel) = answer.await.unwrap().unwrap();
        publisher.apply_answer(&answer, pending).unwrap();
        let connected = tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(publisher.poll_event(&cancel).await, SessionEvent::Connected) {}
        })
        .await;
        assert!(connected.is_ok(), "the publisher never connected");
        publisher.drain_pending_events();
        assert!(publisher.audio_mid.and_then(|mid| publisher.get_pt(mid)).is_some(), "Opus negotiated");
        let mid = publisher.video_mid.unwrap();
        let pt = publisher.get_pt(mid).expect("H.264 accepted");
        let written = Instant::now();
        publisher.write_media(mid, pt, written, MediaTime::new(5_000, Frequency::NINETY_KHZ), IDR).unwrap();
        publisher.drain_outputs().await;

        let (ts, at, paused) = published_once_the_hold_runs_out(&mut published, written).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        drop(publisher);
        assert_sent_as_the_wait_ran_out(&ts, written, at, paused);
    }

    // ── A pull primed with a cached IDR (`PrimedIdr`) ─────────────────

    /// A frame as it came: when (µs), on the video or the audio track, its
    /// RTP time, its payload.
    type Came = (u64, bool, u64, Vec<u8>);

    /// A lone 25 fps frame period, on the 90 kHz clock and in µs.
    const PERIOD_90K: u64 = 3_600;
    const PERIOD_US: u64 = 40_000;
    /// The video's and the audio's RTP bases: a sender picks each at random.
    const VIDEO_RTP: u64 = 3_456_789_012;
    const AUDIO_RTP: u64 = 77_000;

    /// The pictures of a source coding `b` B-frames between references, in
    /// decode order, each as the number it is shown at: 0, the IDR, then the
    /// pictures behind it — each reference ahead of the B-frames shown before
    /// it, those as a pyramid, whose middle is the `earlier` or the later of
    /// two.
    fn decode_order(b: u64, earlier: bool, pictures: usize) -> Vec<u64> {
        fn between(from: u64, to: u64, earlier: bool, out: &mut Vec<u64>) {
            if to - from > 1 {
                let mid = from + (to - from + u64::from(!earlier)) / 2;
                out.push(mid);
                between(from, mid, earlier, out);
                between(mid, to, earlier, out);
            }
        }
        let mut order = vec![0];
        while order.len() < pictures {
            let from = (order.len() as u64 - 1) / (b + 1) * (b + 1);
            order.push(from + b + 1);
            between(from, from + b + 1, earlier, &mut order);
        }
        order.truncate(pictures);
        order
    }

    /// The pictures `order` (as they are shown), the first at `first_us` and
    /// the rest arriving as `at` says of each one's place behind it.
    fn pictures(order: &[u64], first_us: u64, period_90k: u64, at: impl Fn(u64) -> u64) -> Vec<Came> {
        order
            .iter()
            .enumerate()
            .map(|(k, &shown)| {
                let nal = if shown == 0 { IDR } else { P };
                // Its own number past the slice header, so every picture's
                // bytes are its own.
                let data = [nal, &shown.to_be_bytes()[..]].concat();
                (first_us + at(k as u64), true, VIDEO_RTP + shown * period_90k, data)
            })
            .collect()
    }

    /// The SPS of a stream that says its pictures come as they are shown (x264
    /// Main without B-frames: picture order count type 2), and of one that
    /// does not (with them).
    const SPS_IN_ORDER: &str = "674d400cda05067e7c0440000003004000000c83c50aa8";
    const SPS_REORDERED: &str = "674d400ceca0a0cfcf80880000030008000003019078a14cb0";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// `bits` — each a value and how many bits it has — as bytes, with the
    /// RBSP's trailing bits.
    fn bits(bits: &[(u32, u32)]) -> Vec<u8> {
        let mut all: Vec<u8> = bits.iter().flat_map(|&(v, n)| (0..n).rev().map(move |i| (v >> i & 1) as u8)).collect();
        all.push(1);
        all.resize(all.len().next_multiple_of(8), 0);
        all.chunks(8).map(|c| c.iter().fold(0, |byte, bit| byte << 1 | bit)).collect()
    }

    /// `v` as ue(v).
    fn ue(v: u32) -> (u32, u32) {
        (v + 1, 2 * (v + 1).ilog2() + 1)
    }

    /// A P slice with `frame_num`, of a stream whose SPS gives it four bits.
    fn p_slice(frame_num: u32) -> Vec<u8> {
        [vec![0, 0, 0, 1, 0x41], bits(&[ue(0), ue(5), ue(0), (frame_num % 16, 4)])].concat()
    }

    /// `frames` with their pictures coded as a source of `b` B-frames codes
    /// them, its SPS `sps`: every picture with its `frame_num`.
    fn coded(mut frames: Vec<Came>, b: u64, sps: &str, period_90k: u64) -> Vec<Came> {
        let last = frames.iter().filter(|f| f.1).map(|f| (f.2 - VIDEO_RTP) / period_90k).max().unwrap_or(0);
        // The references: what another picture between two is halved by.
        let mut frame_nums = std::collections::HashMap::new();
        let mut references = 0u64;
        for shown in decode_order(b, true, last as usize + 2 * b as usize + 3) {
            frame_nums.insert(shown, references as u32);
            let at = shown % (b + 1);
            let leaf = |from: u64, to: u64| {
                let (mut from, mut to) = (from, to);
                loop {
                    let mid = from + (to - from) / 2;
                    match at.cmp(&mid) {
                        std::cmp::Ordering::Equal => break to - from <= 2,
                        std::cmp::Ordering::Less => to = mid,
                        std::cmp::Ordering::Greater => from = mid,
                    }
                }
            };
            if at == 0 || !leaf(0, b + 1) {
                references += 1;
            }
        }
        for f in frames.iter_mut().filter(|f| f.1) {
            let shown = (f.2 - VIDEO_RTP) / period_90k;
            f.3 = if shown == 0 {
                [&[0, 0, 0, 1], &hex(sps)[..], &[0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88]].concat()
            } else {
                [p_slice(frame_nums[&shown]), shown.to_be_bytes().to_vec()].concat()
            };
        }
        frames
    }

    /// `packets` Opus packets of 20 ms, the first at `first_us`, each arriving
    /// as its time comes.
    fn opus(first_us: u64, packets: u64, toc: u8) -> Vec<Came> {
        (0..packets)
            .map(|k| {
                let mut data = vec![toc];
                data.extend((0..40u64).map(|i| (k + i) as u8));
                (first_us + k * 20_000, false, AUDIO_RTP + k * 960, data)
            })
            .collect()
    }

    /// `frames` in the order they arrive.
    fn arriving(mut frames: Vec<Came>) -> Vec<Came> {
        frames.sort_by_key(|f| f.0);
        frames
    }

    /// `frames` through `w` as its input's loop puts them, its holds' timer
    /// run out on the way, and the session's end.
    fn fed(w: &mut super::WhipMux, t0: std::time::Instant, frames: &[Came]) -> Vec<super::MuxedFrame> {
        let mut out = Vec::new();
        for (us, video, rtp, data) in frames {
            let arrival = t0 + std::time::Duration::from_micros(*us);
            while let Some(due) = w.hold_deadline().filter(|due| *due < arrival) {
                out.extend(w.release_due(due));
            }
            out.extend(w.frame(*video, data, *rtp, if *video { 90_000 } else { 48_000 }, arrival));
        }
        out.extend(w.end_session());
        out
    }

    /// `frames` as a session of a fresh input: a WHEP pull (`pull`), whose
    /// first picture may be a cached one, or a WHIP publish, whose frames are
    /// timed as every session's were before that was looked for.
    fn one_session(pull: bool, frames: &[Came]) -> Vec<super::MuxedFrame> {
        let mut w = av_whip_mux();
        w.session(frames.iter().any(|f| !f.1), false);
        if pull {
            w.expect_primed_idr();
        }
        fed(&mut w, std::time::Instant::now(), frames)
    }

    /// The PES of `muxed` on `pid`, as each one's PTS.
    fn pts_on(muxed: &[super::MuxedFrame], pid: u16) -> Vec<u64> {
        use crate::engine::ts_parse::{extract_pes_pts, ts_pid, ts_pusi};
        muxed
            .iter()
            .flat_map(|(ts, _)| ts)
            .filter(|p| ts_pid(p) == pid && ts_pusi(p))
            .filter_map(|p| extract_pes_pts(p))
            .collect()
    }

    /// The PCRs of `muxed` (90 kHz), each with its `discontinuity_indicator`.
    fn pcrs_in(muxed: &[super::MuxedFrame]) -> Vec<(u64, bool)> {
        use crate::engine::ts_parse::{extract_pcr, ts_discontinuity_indicator};
        muxed
            .iter()
            .flat_map(|(ts, _)| ts)
            .filter_map(|p| Some((extract_pcr(p)? / 300, ts_discontinuity_indicator(p))))
            .collect()
    }

    /// Every step between consecutive `pcrs` (90 kHz, modulo the 33-bit
    /// wrap; a step back comes out huge).
    fn pcr_steps(pcrs: &[(u64, bool)]) -> Vec<u64> {
        pcrs.windows(2).map(|w| w[1].0.wrapping_sub(w[0].0) & 0x1_FFFF_FFFF).collect()
    }

    /// `muxed` without its PAT and PMT, which repeat by the wall clock: the
    /// TS packets, and the PTS each frame was published at.
    fn essence(muxed: &[super::MuxedFrame]) -> (Vec<bytes::Bytes>, Vec<u64>) {
        use crate::engine::ts_parse::ts_pid;
        let ts = muxed.iter().flat_map(|(ts, _)| ts).filter(|p| ![0, 0x1000].contains(&ts_pid(p))).cloned().collect();
        (ts, muxed.iter().map(|m| m.1).collect())
    }

    /// A pull primed with a cached IDR and then sent the live pictures has
    /// its video and its sound as far apart as the live pictures and the
    /// sound arrived, whatever the IDR's age (and the PCR their pace, with no
    /// step to mark): every live picture is timed from the first of them. The
    /// IDR stays where it arrived, at the start, ahead of them. Every live
    /// picture used to be timed from the IDR instead, and so went out its age
    /// late — the PCR stepping by that at the first of them.
    #[test]
    fn a_pull_primed_with_a_cached_idr_is_timed_from_its_live_pictures() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        // 160 ms old to ten minutes (a browser's only IDR so far).
        for missed in [4u64, 5, 12, 37, 50, 15_000] {
            for (stereo, sps) in [(0x78, SPS_IN_ORDER), (0xFC, SPS_IN_ORDER), (0x78, SPS_REORDERED), (0xFC, SPS_REORDERED)] {
                // The IDR at 0; the live pictures from 23 ms on, a period
                // apart; the sound from 31 ms on.
                let mut frames = pictures(&[0], 0, PERIOD_90K, |_| 0);
                let live: Vec<u64> = (missed..missed + 100).collect();
                frames.extend(pictures(&live, 23_000, PERIOD_90K, |k| k * PERIOD_US));
                let mut frames = coded(frames, 0, sps, PERIOD_90K);
                frames.extend(opus(31_000, 200, stereo));
                let muxed = one_session(true, &arriving(frames));

                let video = pts_on(&muxed, 0x0100);
                let expected: Vec<u64> =
                    std::iter::once(LEAD).chain((0..100).map(|k| LEAD + 23 * 90 + k * PERIOD_90K)).collect();
                assert_eq!(video, expected, "{missed} pictures missed");
                let audio = pts_on(&muxed, 0x0101);
                assert_eq!(audio, (0..200).map(|k| LEAD + 31 * 90 + k * 1_800).collect::<Vec<_>>());
                // Nothing steps: 23 ms from the IDR's PCR to the next.
                let pcrs = pcrs_in(&muxed);
                assert!(pcrs.iter().all(|p| !p.1), "a DI: {pcrs:?}");
                let steps = pcr_steps(&pcrs);
                assert_eq!(steps[0], 23 * 90);
                assert!(steps[1..].iter().all(|s| *s == PERIOD_90K), "{steps:?}");
                // One PMT layout throughout, the channel count with it.
                let codes: std::collections::BTreeSet<Option<u8>> = pmt_codes(&muxed).into_iter().collect();
                assert_eq!(codes.len(), 1, "{codes:?}");
            }
        }
        // 100 ms ahead or less is within what holds up a live first picture.
        for (from_us, timed_from) in [(20_000u64, LEAD + 3 * PERIOD_90K), (19_000, LEAD + 19 * 90)] {
            let mut frames = pictures(&[0], 0, PERIOD_90K, |_| 0);
            frames.extend(pictures(&(3..40).collect::<Vec<_>>(), from_us, PERIOD_90K, |k| k * PERIOD_US));
            let frames = coded(frames, 0, SPS_IN_ORDER, PERIOD_90K);
            assert_eq!(pts_on(&one_session(true, &frames), 0x0100)[1], timed_from);
        }
    }

    /// The same of a source with B-frames, from whichever picture of a
    /// mini-GOP the pull's live pictures start on: they keep the distances
    /// from each other their RTP times say, to the tick — moved whole, none
    /// clamped, reordered or dropped, only their distance from the IDR taken
    /// out — and the first of them to be shown goes out as it arrived. They
    /// are all past the IDR, where timing the video from the picture after
    /// the IDR put the B-frames shown before that one ahead of it: before
    /// the start of the timeline, the PCR wrapping.
    #[test]
    fn b_frames_behind_a_cached_idr_keep_their_order_and_times() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        let (mut known, mut unknown) = (0, 0);
        for b in [1u64, 2, 3, 7] {
            let order = decode_order(b, b != 2, 400);
            for start in 120..=120 + b as usize {
                // As they come, and the first few in one go: what the server
                // queued for the viewer while it connected.
                for queued in [0u64, 4, 12] {
                    let live = &order[start..start + 60];
                    let mut frames = pictures(&[0], 0, PERIOD_90K, |_| 0);
                    frames.extend(pictures(live, 9_000, PERIOD_90K, |k| k.min(queued) * 300 + k.saturating_sub(queued) * PERIOD_US));
                    let mut frames = coded(frames, b, SPS_REORDERED, PERIOD_90K);
                    if super::H264Order::of(&hex(SPS_REORDERED)).unwrap().frame_num(&frames[1].3) < Some(2) {
                        unknown += 1;
                        continue;
                    }
                    known += 1;
                    frames.extend(opus(5_000, 120, 0xFC));
                    let frames = arriving(frames);
                    let muxed = one_session(true, &frames);
                    let as_before = one_session(false, &frames);

                    let (now, before) = (pts_on(&muxed, 0x0100), pts_on(&as_before, 0x0100));
                    assert_eq!(now.len(), 61, "every picture, the IDR first");
                    assert_eq!((now[0], before[0]), (LEAD, LEAD), "the IDR where it arrived");
                    let moved: std::collections::BTreeSet<u64> =
                        now[1..].iter().zip(&before[1..]).map(|(n, b)| b - n).collect();
                    assert_eq!(moved.len(), 1, "moved whole (b {b}, start {start}, queued {queued}): {moved:?}");
                    assert_eq!(pts_on(&muxed, 0x0101), pts_on(&as_before, 0x0101), "the sound not moved");
                    // The first shown, of the mini-GOP they start in.
                    let first = (0..=b as usize + 1).min_by_key(|&k| live[k]).unwrap();
                    let arrived = frames.iter().filter(|f| f.1).nth(first + 1).unwrap().0;
                    assert_eq!(now[1 + first], LEAD + arrived * 9 / 100, "as it arrived (b {b}, start {start}, queued {queued})");
                    assert!(now[1..].iter().all(|pts| *pts > LEAD), "past the IDR: {now:?}");
                    // The PCR follows them, as they step back and forward, and
                    // the first of them is past all it had reached.
                    let steps = pcr_steps(&pcrs_in(&muxed));
                    assert!(steps.iter().all(|s| *s <= PERIOD_90K || *s > 1 << 32));
                    let back = steps.iter().position(|s| *s > 1 << 32).unwrap();
                    let reference = (1..).find(|&k| live[k] < live[k - 1]).unwrap();
                    let at = pcrs_in(&muxed)[back].0;
                    assert_eq!(at, now[reference] - LEAD, "the first step back is the first B-frame's");
                }
            }
        }
        assert!(known >= 30, "{known} starts told cached, {unknown} where `frame_num` has come round");
    }

    /// A pull whose first picture is live goes out as it always did, to the
    /// byte, each frame as it comes — B-frames or none, at their pace,
    /// jittered, in one go, the first picture held up on its way, a frame a
    /// fifth of a second or a second apart, no steady frame period, no sound,
    /// no keyframe first, the sound first — and so do one whose cached IDR
    /// `frame_num` does not tell and a WHIP publish of anything, one with a
    /// cached IDR included: every timestamp is the track's first frame's
    /// arrival plus the RTP time since.
    #[test]
    fn a_session_whose_first_picture_is_live_is_timed_as_it_was() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        // Arrival jitter, 0-60 ms, the same at every run.
        let jitter = |k: u64| (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) % 60_000;
        let mut sessions: Vec<(String, Vec<Came>)> = Vec::new();
        let mut uneven: Vec<(String, Vec<Came>)> = Vec::new();
        for (b, earlier) in [0u64, 1, 2, 3, 7].into_iter().flat_map(|b| [(b, true), (b, false)]) {
            let order = decode_order(b, earlier, 120);
            let av = |video: Vec<Came>| arriving([video, opus(12_000, 240, 0x78)].concat());
            sessions.push((format!("at its pace, b {b}"), av(pictures(&order, 0, PERIOD_90K, |k| k * PERIOD_US))));
            sessions.push((
                format!("jittered, b {b}"),
                av(pictures(&order, 60_000, PERIOD_90K, |k| k * PERIOD_US + jitter(k))),
            ));
            for queued in [2u64, 5, 30] {
                // What the server queued while its viewer connected, an IDR
                // first, in one go.
                let at = move |k: u64| k.min(queued) * 250 + k.saturating_sub(queued) * PERIOD_US;
                sessions.push((format!("{queued} in one go, b {b}"), av(pictures(&order, 0, PERIOD_90K, at))));
            }
            for late_ms in [15u64, 60, 95, 250] {
                // The first picture held up on its way (encoding, pacing).
                let at = move |k: u64| (k * PERIOD_US).max(late_ms * 1_000 + k * 500);
                sessions.push((format!("first {late_ms} ms late, b {b}"), av(pictures(&order, 0, PERIOD_90K, at))));
            }
            sessions.push((
                format!("no sound, b {b}"),
                pictures(&order, 0, PERIOD_90K, |k| k.min(6) * 200 + k.saturating_sub(6) * PERIOD_US),
            ));
            // Joined part-way: no keyframe first, or the sound.
            sessions.push((format!("mid-GOP, b {b}"), av(pictures(&order[7..], 4_000, PERIOD_90K, |k| k * 300))));
            sessions.push((
                format!("the sound first, b {b}"),
                arriving([pictures(&order, 400_000, PERIOD_90K, |k| k * PERIOD_US), opus(0, 240, 0xFC)].concat()),
            ));
        }
        for (fps, b) in [(1u64, 0u64), (2, 0), (5, 0), (10, 0), (15, 0), (15, 2), (10, 3), (5, 2)] {
            let order = decode_order(b, b != 2, 40);
            for queued in [0u64, 1, 2, 3, 6] {
                let period = 1_000_000 / fps;
                let at = move |k: u64| k.min(queued) * 400 + k.saturating_sub(queued) * period;
                let video = pictures(&order, 0, 90_000 / fps, at);
                let what = format!("{fps} fps, {queued} in one go, b {b}");
                sessions.push((what, arriving([video, opus(3_000, 240, 0xFC)].concat())));
            }
        }
        // No steady frame period, its first gap the longest — a screen share,
        // a webcam with a hiccup at its start — as its server sends what it
        // queued in one go, as it comes, or the first held up on its way: each
        // picture right behind the last, as its `frame_num` says where its SPS
        // is known. A first gap of a second ran ahead of the IDR by all of it.
        for (first_gap_ms, then_ms) in [(400u64, 33u64), (150, 33), (1_199, 16), (250, 100), (900, 900)] {
            let shown_ms = |k: u64| if k == 0 { 0 } else { first_gap_ms + (k - 1) * then_ms };
            for (queued, late_ms) in [(0u64, 0u64), (3, 0), (40, 0), (0, 350), (6, 120)] {
                for sps in ["", SPS_IN_ORDER, SPS_REORDERED] {
                    let video = (0..40u64).map(|k| {
                        let at = (shown_ms(k).saturating_sub(shown_ms(k.min(queued))) * 1_000).max(late_ms * 1_000) + k * 300;
                        let data = match (k, sps) {
                            (_, "") => [if k == 0 { IDR } else { P }, &k.to_be_bytes()].concat(),
                            (0, sps) => [&[0, 0, 0, 1], &hex(sps)[..], &[0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88]].concat(),
                            _ => [p_slice(k as u32), k.to_be_bytes().to_vec()].concat(),
                        };
                        (at, true, VIDEO_RTP + shown_ms(k) * 90, data)
                    });
                    let what = format!("{first_gap_ms} ms, then {then_ms}, {queued} in one go, {late_ms} ms late, {sps:?}");
                    uneven.push((what, arriving([video.collect(), opus(2_000, 240, 0xFC)].concat())));
                }
            }
        }
        // No keyframe first, and a jump behind it.
        let mut frames = pictures(&[5], 0, PERIOD_90K, |_| 0);
        frames.extend(pictures(&(60..90).collect::<Vec<_>>(), 20_000, PERIOD_90K, |k| k * PERIOD_US));
        sessions.push(("no keyframe first".into(), frames));
        // Two B-frames at 15 fps, the later sent first, the one shown first
        // lost, the IDR 60 ms late: the reference runs 193 ms ahead of it, the
        // B-frame that came 60 ms.
        let order: Vec<u64> = decode_order(2, false, 60).into_iter().filter(|shown| *shown != 1).collect();
        let video = pictures(&order, 0, 6_000, |k| (k * 66_667).max(60_000));
        sessions.push(("15 fps, the picture shown first lost, b 2".into(), video));
        sessions.push(("no picture".into(), opus(0, 100, 0x78)));
        sessions.push(("one picture".into(), pictures(&[0], 0, PERIOD_90K, |_| 0)));
        sessions.push(("one picture and sound".into(), arriving([pictures(&[0], 0, PERIOD_90K, |_| 0), opus(9_000, 99, 0xFC)].concat())));

        // Each as it is, and coded so that its SPS and slices parse.
        let coded_too: Vec<(String, Vec<Came>)> = sessions
            .iter()
            .filter(|(_, frames)| frames.iter().any(|f| f.1 && f.2 == VIDEO_RTP))
            .map(|(what, frames)| {
                let b = what.split("b ").nth(1).and_then(|b| b.parse().ok()).unwrap_or(0);
                let fps: u64 = what.split(" fps").next().and_then(|f| f.parse().ok()).unwrap_or(25);
                let sps = if b == 0 { SPS_IN_ORDER } else { SPS_REORDERED };
                (format!("{what}, coded"), coded(frames.clone(), b, sps, 90_000 / fps))
            })
            .collect();
        sessions.extend(coded_too);
        sessions.extend(uneven);
        // A cached IDR that `frame_num` does not tell: the second picture has
        // the IDR's again or one past it (16 and 17 missed), an IDR, or no
        // SPS or slice that parses.
        for missed in [16u64, 17, 32, 49] {
            let mut frames = pictures(&[0], 0, PERIOD_90K, |_| 0);
            frames.extend(pictures(&(missed..missed + 60).collect::<Vec<_>>(), 23_000, PERIOD_90K, |k| k * PERIOD_US));
            for sps in [SPS_IN_ORDER, SPS_REORDERED] {
                sessions.push((format!("{missed} missed"), coded(frames.clone(), 0, sps, PERIOD_90K)));
            }
            sessions.push((format!("{missed} missed, not told"), frames.clone()));
            let mut frames = coded(frames, 0, SPS_IN_ORDER, PERIOD_90K);
            frames[1].3 = frames[0].3.clone();
            sessions.push((format!("{missed} missed, an IDR next"), frames));
        }

        for (what, frames) in &sessions {
            let muxed = one_session(true, frames);
            assert_eq!(essence(&muxed), essence(&one_session(false, frames)), "{what}: not as a publish's");
            // Nothing held, so each frame as it came, PSI and all.
            let mut w = av_whip_mux();
            w.session(frames.iter().any(|f| !f.1), true);
            w.expect_primed_idr();
            let t0 = std::time::Instant::now();
            for (us, video, rtp, data) in frames {
                let arrival = t0 + std::time::Duration::from_micros(*us);
                let out = w.frame(*video, data, *rtp, if *video { 90_000 } else { 48_000 }, arrival);
                assert_eq!((out.len(), w.hold_deadline()), (1, None), "{what}: held");
            }
            for (video, pid, rate) in [(true, 0x0100u16, 90_000u64), (false, 0x0101, 48_000)] {
                let track: Vec<&Came> = frames.iter().filter(|f| f.1 == video).collect();
                let Some(first) = track.first() else { continue };
                let base = LEAD as i128 + (first.0 - frames[0].0) as i128 * 9 / 100;
                let expected: Vec<u64> = track
                    .iter()
                    .map(|f| (base + (f.2 as i128 - first.2 as i128) * 90_000 / rate as i128) as u64)
                    .collect();
                assert_eq!(pts_on(&muxed, pid), expected, "{what}");
            }
        }

        // A WHIP publish is not looked at: its IDR, however old, opens it.
        let mut frames = pictures(&[0], 0, PERIOD_90K, |_| 0);
        frames.extend(pictures(&(40..140).collect::<Vec<_>>(), 23_000, PERIOD_90K, |k| k * PERIOD_US));
        let published = pts_on(&one_session(false, &frames), 0x0100);
        assert_eq!(published[1], LEAD + 40 * PERIOD_90K);
    }

    /// The order an H.264 stream's pictures come in, off the SPS, and their
    /// `frame_num`, off their first slice: x264's (Main, Baseline, High with a
    /// scaling matrix in its PPS, 4:2:2 and 4:4:4), against ffmpeg's
    /// `trace_headers`; an SPS with scaling lists of its own and separate
    /// colour planes; and nothing of what does not parse.
    #[test]
    fn an_h264_streams_order_is_read_off_its_sps_and_slices() {
        use super::H264Order;
        for (what, sps, in_order, slices) in [
            (
                "Main, no B-frames",
                SPS_IN_ORDER,
                true,
                [("419a26223fb54cb25889147d", 1), ("419a66223fb91f76f85e7a31", 3), ("419a86223f2d73cda3d46e08", 4)],
            ),
            (
                "Main, B-frames",
                SPS_REORDERED,
                false,
                [("419a231888ffbab921f13817", 1), ("019e62442dfffeed7c9a6489", 3), ("419bb6393111ff2d73cd98ce", 13)],
            ),
            (
                "Baseline",
                "6742c00cda05067e7c0440000003004000000c83c50aa8",
                true,
                [("419a2047f0f569f29046d91e", 1), ("419a6047f0e67443718b6302", 3), ("419a8047297f7c2082fe00db", 4)],
            ),
            (
                "High",
                "6764000cacd941419f9f011000000300100000030320f1429960",
                false,
                [("419a24188affb7d2add76722", 1), ("019e61442dfffeed75ef6138", 3), ("419b77344c457f3bc0fec67e", 11)],
            ),
            (
                "High 4:2:2",
                "677a000cb6cd941419f8dc0440000003004000000c83c50a6580",
                false,
                [("419a231888ffbab911e58418", 1), ("019e62442dfffeed82f21378", 3), ("419bb6344c447f2d73cda3d3", 13)],
            ),
            (
                "High 4:4:4",
                "67f4000c919b282833f1b808800000030080000019078a14cb",
                false,
                [("419a23188affb4aa9fa281ab", 1), ("019e62442dfffe9db01bcf4b", 3), ("419bb6393115ff3bc0d6c8fd", 13)],
            ),
        ] {
            let order = H264Order::of(&hex(sps)).expect(what);
            assert_eq!(order.in_order, in_order, "{what}");
            for (slice, frame_num) in slices {
                let au = [&[0, 0, 0, 1, 0x06, 0x01, 0, 0, 1][..], &hex(slice)].concat();
                assert_eq!(order.frame_num(&au), Some(frame_num), "{what}: {slice}");
            }
            assert_eq!(order.frame_num(&[0, 0, 0, 1, 0x65, 0x88, 0x84]), None, "{what}: an IDR");
        }

        // High 4:4:4 Predictive, separate colour planes, twelve scaling lists:
        // absent, the default (its first delta ends it), one of two deltas and
        // one spelt out; frame_num of 9 bits, picture order count type 0.
        let mut sps: Vec<(u32, u32)> = vec![(244, 8), (0, 8), (40, 8), ue(0), ue(3), (1, 1), ue(0), ue(0), (0, 1), (1, 1)];
        for list in 0..12 {
            let size = if list < 6 { 16 } else { 64 };
            match list % 4 {
                0 => sps.push((0, 1)),
                // delta_scale -8 (se): the next scale 0.
                1 => sps.extend([(1, 1), ue(16)]),
                // +4, then -12: 12, then 0.
                2 => sps.extend([(1, 1), ue(7), ue(24)]),
                // All of them the last again: 8.
                _ => {
                    sps.push((1, 1));
                    sps.extend(std::iter::repeat_n(ue(0), size));
                }
            }
        }
        sps.extend([ue(5), ue(0), ue(4)]);
        let order = H264Order::of(&[vec![0x67], bits(&sps)].concat()).unwrap();
        assert_eq!((order.frame_num_bits, order.separate_colour_planes, order.in_order), (9, true, false));
        let slice = [vec![0x41], bits(&[ue(0), ue(5), ue(0), (2, 2), (300, 9)])].concat();
        assert_eq!(order.frame_num(&slice), Some(300), "a bare NAL unit");

        for cut in 1..5 {
            assert!(H264Order::of(&hex(SPS_IN_ORDER)[..cut]).is_none());
        }
        let wide = bits(&[(77, 8), (0, 8), (40, 8), ue(0), ue(13), ue(2)]);
        assert!(H264Order::of(&[vec![0x67], wide].concat()).is_none(), "a frame_num of 17 bits");
        assert_eq!(order.frame_num(&[0x41, 0x88]), None);
        assert_eq!(order.frame_num(&[0, 0, 0, 1, 0x09, 0xF0]), None, "no slice");
    }

    /// Where the second picture's `frame_num` says that reference pictures
    /// between it and the first were not sent, the first is cached, and a
    /// stream shown as it is sent is timed from the second as it comes:
    /// nothing is held. One that may reorder is held for the picture shown
    /// first, the PCR running on as each picture comes. Where `frame_num`
    /// says nothing — the picture right behind an IDR, or its second field —
    /// nothing is held, and the video stays timed from the first.
    #[test]
    fn a_cached_idr_shows_on_the_picture_behind_it() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        let ms = std::time::Duration::from_millis;
        let t0 = std::time::Instant::now();
        let pull = |sps: &str| {
            let mut w = av_whip_mux();
            w.session(true, true);
            w.expect_primed_idr();
            let idr = [&[0, 0, 0, 1], &hex(sps)[..], &[0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88]].concat();
            assert_eq!(w.frame(true, &idr, 1_000, 90_000, t0).len(), 1, "the IDR at once");
            w
        };
        let put = |w: &mut super::WhipMux, at: u64, shown: u64, frame_num: u32| {
            w.frame(true, &p_slice(frame_num), 1_000 + shown * 90, 90_000, t0 + ms(at))
        };
        let stamped = |out: Vec<super::MuxedFrame>| out.iter().map(|m| m.1).collect::<Vec<_>>();

        let mut w = pull(SPS_IN_ORDER);
        assert_eq!(stamped(put(&mut w, 20, 1_200, 14)), [LEAD + 1_800], "as it comes");
        assert_eq!(w.hold_deadline(), None);
        assert_eq!(stamped(put(&mut w, 60, 1_240, 15)), [LEAD + 1_800 + 3_600]);

        // 100 ms ahead or less is within what holds up a live first picture,
        // though one behind it was lost.
        let mut w = pull(SPS_IN_ORDER);
        assert_eq!(stamped(put(&mut w, 20, 120, 2)), [LEAD + 120 * 90]);

        // A source a fifth of a second apart, in one go: the picture right
        // behind the IDR, or its second field. And one the IDR's `frame_num`
        // came round to again: 16 pictures missed.
        for (sps, frame_num, shown) in [
            (SPS_IN_ORDER, 1, [200u64, 400, 600]),
            (SPS_IN_ORDER, 0, [200, 400, 600]),
            (SPS_REORDERED, 1, [200, 400, 600]),
            (SPS_IN_ORDER, 1, [680, 720, 760]),
        ] {
            let mut w = pull(sps);
            for (k, shown) in shown.into_iter().enumerate() {
                assert_eq!(stamped(put(&mut w, 20 + k as u64, shown, frame_num + k as u32)), [LEAD + shown * 90]);
                assert_eq!(w.hold_deadline(), None);
            }
        }
        // A picture lost behind a live one that came late: what is behind it
        // does not all run ahead of it, so it is live, and goes out so.
        let mut w = pull(SPS_REORDERED);
        assert_eq!(pts_on(&put(&mut w, 40, 160, 2), 0x0100), [0u64; 0]);
        assert_eq!(pts_on(&put(&mut w, 80, 120, 3), 0x0100), [160, 120].map(|at| LEAD + at * 90));
        assert_eq!(w.hold_deadline(), None);

        // Reordered: the reference, the two B-frames shown before it, and the
        // next. The PCR keeps the pictures' pace, 20 ms behind the IDR's.
        let mut w = pull(SPS_REORDERED);
        let mut pcrs = Vec::new();
        for (at, shown, frame_num) in [(20u64, 1_200u64, 9), (60, 1_120, 10), (100, 1_160, 11)] {
            let out = put(&mut w, at, shown, frame_num);
            assert_eq!(pts_on(&out, 0x0100), [0u64; 0], "held");
            assert!(pmt_codes(&out).is_empty());
            pcrs.extend(pcrs_in(&out));
            // A datagram of its own, which an output sends at once.
            let pids: Vec<u16> = out[0].0.iter().map(|p| crate::engine::ts_parse::ts_pid(p)).collect();
            assert_eq!(pids, [0x0100, 0x1FFF, 0x1FFF, 0x1FFF, 0x1FFF, 0x1FFF, 0x1FFF]);
            assert!(opus_at(&mut w, t0, at + 5, 0xFC).is_empty(), "and the sound, no PCR with it");
        }
        assert_eq!(pcrs, [20, 60, 100].map(|at| (at * 90, false)));
        let out = put(&mut w, 140, 1_320, 11);
        // Timed from the first shown, at 60 ms: the reference 80 ms past it.
        let video = [140u64, 60, 100, 260].map(|at| LEAD + at * 90);
        assert_eq!(pts_on(&out, 0x0100), video);
        assert_eq!(pcrs_in(&out)[0], (140 * 90, false), "the first of them past it");

        // The PCR is not run on past the reference's, though a B-frame comes
        // late: 40 ms past the one shown first, which came 40 ms in.
        let mut w = pull(SPS_REORDERED);
        let mut pcrs = Vec::new();
        for (at, shown, frame_num) in [(20u64, 1_200u64, 9), (40, 1_160, 10), (95, 1_180, 11)] {
            pcrs.extend(pcrs_in(&put(&mut w, at, shown, frame_num)));
        }
        assert_eq!(pcrs, [20, 40, 80].map(|at| (at * 90, false)));
        let out = put(&mut w, 120, 1_260, 11);
        assert_eq!(pcrs_in(&out)[0], (80 * 90, false));
    }

    /// The live pictures come in the same instant as the cached IDR: it goes
    /// out a tick ahead of them. And where they come a second behind it with
    /// the input's timer yet to run — the IDR still held for a channel count
    /// that never came — it goes out first, and the step to them is marked.
    #[test]
    fn a_cached_idr_goes_out_ahead_of_its_live_pictures() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        for (sps, b) in [(SPS_IN_ORDER, 0u64), (SPS_REORDERED, 2)] {
            let live: Vec<u64> = decode_order(b, true, 200)[60..100].to_vec();
            let mut frames = pictures(&[0], 0, PERIOD_90K, |_| 0);
            frames.extend(pictures(&live, 0, PERIOD_90K, |k| k * PERIOD_US));
            let frames = coded(frames, b, sps, PERIOD_90K);
            let video = pts_on(&one_session(true, &frames), 0x0100);
            assert_eq!(video[0], LEAD);
            let first = video[1..].iter().min().unwrap();
            assert!((LEAD + 1..=LEAD + b * PERIOD_90K + 1).contains(first), "b {b}: {video:?}");
            assert!(b > 0 || *first == LEAD + 1);
        }

        let mut w = av_whip_mux();
        w.session(true, false);
        w.expect_primed_idr();
        let t0 = std::time::Instant::now();
        let mut out = Vec::new();
        let live = pictures(&(50..60).collect::<Vec<_>>(), 1_500_000, PERIOD_90K, |k| k * PERIOD_US);
        for (us, _, rtp, data) in coded([pictures(&[0], 0, PERIOD_90K, |_| 0), live].concat(), 0, SPS_IN_ORDER, PERIOD_90K) {
            out.extend(w.frame(true, &data, rtp, 90_000, t0 + std::time::Duration::from_micros(us)));
        }
        let video = pts_on(&out, 0x0100);
        assert_eq!(video[..2], [LEAD, LEAD + 135_000]);
        assert_eq!(pcrs_in(&out)[..3], [(0, false), (135_000, true), (135_000 + PERIOD_90K, false)]);
    }

    /// The frames of a pull held behind a cached IDR come out in the
    /// order they arrived, and no later than their bounds: with the third
    /// picture (a source without B-frames), at 400 ms or at the end of the
    /// pull, whose frames are not lost.
    #[test]
    fn what_a_pulls_first_picture_holds_up_comes_out_within_its_bounds() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        let ms = std::time::Duration::from_millis;
        let pull = |declared: bool| {
            let mut w = av_whip_mux();
            w.session(true, declared);
            w.expect_primed_idr();
            w
        };
        // Declared stereo, so nothing is held for the count.
        let t0 = std::time::Instant::now();
        let mut w = pull(true);
        let idr = [&[0, 0, 0, 1], &hex(SPS_REORDERED)[..], &[0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88]].concat();
        let p = p_slice(7);
        assert_eq!(video_at(&mut w, t0, 0, &idr).len(), 1, "the IDR at once");
        let later = |w: &mut super::WhipMux, at: u64, shown: u64| w.frame(true, &p, 1_000 + shown * 90, 90_000, t0 + ms(at));
        assert_eq!(pcrs_in(&later(&mut w, 20, 1_200)), [(20 * 90, false)], "the second picture is held, the PCR not");
        let held = |out: Vec<super::MuxedFrame>| pts_on(&out, 0x0100).is_empty() && pts_on(&out, 0x0101).is_empty();
        assert!(opus_at(&mut w, t0, 25, 0xFC).is_empty(), "and what comes behind it");
        assert_eq!(w.hold_deadline(), Some(t0 + ms(420)));
        assert!(w.release_due(t0 + ms(419)).is_empty());
        let out = later(&mut w, 60, 1_240);
        assert_eq!(w.hold_deadline(), None, "the third settles it");
        assert_eq!(out.iter().map(|m| m.1).collect::<Vec<_>>(), [LEAD + 1_800, LEAD + 2_250, LEAD + 5_400]);
        assert_eq!(later(&mut w, 100, 1_280).len(), 1, "then each as it comes");

        // The Opus settles the channel count though it is held itself: the
        // IDR held for that goes out behind a PMT that says stereo.
        let mut w = pull(false);
        assert!(video_at(&mut w, t0, 0, &idr).is_empty());
        assert!(later(&mut w, 10, 1_500).is_empty());
        let out = opus_at(&mut w, t0, 30, 0xFC);
        assert_eq!(out.iter().map(|m| m.1).collect::<Vec<_>>(), [LEAD], "the IDR");
        assert_eq!(pmt_codes(&out), [Some(2)]);
        assert_eq!(later(&mut w, 50, 1_540).len(), 3);

        // No third picture: 400 ms, then as it always was.
        let mut w = pull(true);
        assert_eq!(video_at(&mut w, t0, 0, &idr).len(), 1);
        assert!(held(later(&mut w, 20, 1_200)));
        assert!(w.release_due(t0 + ms(419)).is_empty());
        let out = w.release_due(t0 + ms(420));
        assert_eq!(out.iter().map(|m| m.1).collect::<Vec<_>>(), [LEAD + 1_200 * 90]);
        assert_eq!(w.hold_deadline(), None);
        // A mini-GOP cut short: judged by its pictures where they are enough
        // to hold the one shown first, as a fifth is here (of seven B-frames
        // at 10 fps, the reference first), and left alone where they are not.
        for (came, moved) in [(4, false), (5, true)] {
            let mut w = pull(true);
            assert_eq!(video_at(&mut w, t0, 0, &idr).len(), 1);
            for (k, shown) in [2_300u64, 1_900, 1_700, 1_600, 1_800].into_iter().take(came).enumerate() {
                assert!(pts_on(&later(&mut w, 20 + k as u64, shown), 0x0100).is_empty());
            }
            let first = pts_on(&w.release_due(t0 + ms(420)), 0x0100)[0];
            assert_eq!(first, if moved { LEAD + (23 + 700) * 90 } else { LEAD + 2_300 * 90 });
        }

        // Ten pictures of a mini-GOP, in one go: of fifteen B-frames, the
        // one shown first the fifth.
        let mut w = pull(true);
        assert_eq!(video_at(&mut w, t0, 0, &idr).len(), 1);
        let order = decode_order(15, true, 18);
        for (k, shown) in order[1..=10].iter().enumerate() {
            let out = later(&mut w, 20 + k as u64, 3_000 + shown * 40);
            assert_eq!(pts_on(&out, 0x0100).len(), if k < 9 { 0 } else { 10 }, "picture {k}");
            assert!(k < 9 || pts_on(&out, 0x0100)[4] == LEAD + 24 * 90, "the one shown first as it arrived");
        }

        // The pull ending, or `HOLD_MAX_BYTES` of it.
        let mut w = pull(true);
        assert_eq!(video_at(&mut w, t0, 0, &idr).len(), 1);
        assert!(held(later(&mut w, 20, 1_200)));
        assert!(opus_at(&mut w, t0, 25, 0xFC).is_empty());
        assert_eq!(w.end_session().len(), 2);
        let mut w = pull(true);
        assert_eq!(video_at(&mut w, t0, 0, &idr).len(), 1);
        assert!(held(later(&mut w, 20, 1_200)));
        let mut big = p.clone();
        big.resize(super::HOLD_MAX_BYTES + 1, 0x11);
        assert_eq!(w.frame(true, &big, 1_000 + 1_180 * 90, 90_000, t0 + ms(60)).len(), 2);
    }

    /// The live pictures coming a while behind the cached IDR — its source
    /// paused, or the PMT's next — the PCR runs on to them as it does over
    /// any step: with fillers up to a second, and past that, where nothing
    /// bridges it, marked with `discontinuity_indicator`, once, and not
    /// bridged. It used to step by the pause and the IDR's age, unmarked.
    #[test]
    fn a_pause_behind_a_cached_idr_is_bridged_or_marked() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        for (pause_us, marked) in [(700_000u64, false), (1_000_000, false), (1_000_100, true), (9_000_000, true)] {
            for sound in [false, true] {
                let mut frames = pictures(&[0], 0, PERIOD_90K, |_| 0);
                frames.extend(pictures(&(500..560).collect::<Vec<_>>(), pause_us, PERIOD_90K, |k| k * PERIOD_US));
                let mut frames = coded(frames, 0, SPS_IN_ORDER, PERIOD_90K);
                if sound {
                    frames.extend(opus(40_000, 600, 0xFC));
                }
                let muxed = one_session(true, &arriving(frames));
                let video = pts_on(&muxed, 0x0100);
                assert_eq!((video[0], video[1]), (LEAD, LEAD + pause_us * 9 / 100));
                let pcrs = pcrs_in(&muxed);
                let steps = pcr_steps(&pcrs);
                let at = pcrs.iter().position(|p| p.1);
                assert_eq!(at, marked.then_some(1), "{pause_us} µs: {pcrs:?}");
                assert_eq!(pcrs.iter().filter(|p| p.1).count(), usize::from(marked));
                let unmarked = steps.iter().enumerate().filter(|(k, _)| Some(k + 1) != at);
                assert!(unmarked.clone().all(|(_, s)| *s <= PERIOD_90K), "{pause_us} µs: {steps:?}");
            }
        }
    }

    /// A pull after another on the same input, primed with a cached IDR as
    /// well: the IDR goes past the last pull's end — lifted there, its
    /// sender's clock having run fast — with `discontinuity_indicator` on its
    /// PCR, the live pictures as far behind it as they arrived, and the sound
    /// with them; nothing is behind what was sent.
    #[test]
    fn a_reconnecting_pull_is_timed_from_its_live_pictures_too() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        let t0 = std::time::Instant::now();
        let primed = |from_us: u64, period_us: u64| {
            let mut frames = pictures(&[0], from_us, PERIOD_90K, |_| 0);
            let live: Vec<u64> = (45..120).collect();
            frames.extend(pictures(&live, from_us + 15_000, PERIOD_90K, |k| k * period_us));
            let mut frames = coded(frames, 0, SPS_IN_ORDER, PERIOD_90K);
            let packets = opus(from_us + 21_000, 150, 0xFC);
            frames.extend(packets);
            arriving(frames)
        };
        for lifted in [false, true] {
            let mut w = av_whip_mux();
            w.session(true, false);
            w.expect_primed_idr();
            // The sender's clock twice as fast as it should run, or not.
            let first = fed(&mut w, t0, &primed(0, if lifted { PERIOD_US / 2 } else { PERIOD_US }));
            let end = *pts_on(&first, 0x0100).iter().max().unwrap();
            w.session(true, false);
            w.expect_primed_idr();
            let from_us = if lifted { 2_000_000 } else { 6_000_000 };
            let second = fed(&mut w, t0, &primed(from_us, PERIOD_US));
            let video = pts_on(&second, 0x0100);
            let start = if lifted { video[0] } else { LEAD + from_us * 9 / 100 };
            if lifted {
                assert!(video[0] > end, "past the last pull's end: {} / {end}", video[0]);
            }
            assert_eq!(video[0], start);
            assert_eq!(video[1..], (0..75).map(|k| start + 15 * 90 + k * PERIOD_90K).collect::<Vec<_>>()[..]);
            assert_eq!(pts_on(&second, 0x0101)[0], start + 21 * 90);
            let pcrs = pcrs_in(&second);
            assert!(pcrs[0].1 && pcrs[1..].iter().all(|p| !p.1), "the new session's DI alone: {pcrs:?}");
            assert!(pcr_steps(&pcrs).iter().all(|s| *s <= PERIOD_90K));
        }
    }

    /// The same across the 33-bit wrap, between the IDR and its live
    /// pictures and just behind the first of them.
    #[test]
    fn a_cached_idr_by_the_wrap_is_timed_as_anywhere() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        const WRAP: u64 = 1 << 33;
        for before_us in [10_000u64, 30_000, 70_000, 180_000] {
            let mut w = av_whip_mux();
            w.session(false, false);
            w.expect_primed_idr();
            let t0 = std::time::Instant::now();
            // The input's origin 26.5 h back, this pull `before_us` short of
            // the wrap.
            let _ = fed(&mut w, t0, &pictures(&[0], 0, PERIOD_90K, |_| 0));
            w.session(false, false);
            w.expect_primed_idr();
            let from_us = (WRAP - LEAD) * 100 / 9 - before_us;
            let mut frames = pictures(&[0], from_us, PERIOD_90K, |_| 0);
            frames.extend(pictures(&(30..80).collect::<Vec<_>>(), from_us + 20_000, PERIOD_90K, |k| k * PERIOD_US));
            let muxed = fed(&mut w, t0, &coded(frames, 0, SPS_IN_ORDER, PERIOD_90K));
            let video = pts_on(&muxed, 0x0100);
            let idr = LEAD + from_us * 9 / 100;
            let live = LEAD + (from_us + 20_000) * 9 / 100;
            let expected: Vec<u64> =
                std::iter::once(idr).chain((0..50).map(|k| live + k * PERIOD_90K)).map(|pts| pts % WRAP).collect();
            assert_eq!(video, expected, "{before_us} µs short of the wrap");
            assert!(pcr_steps(&pcrs_in(&muxed)).iter().all(|s| *s <= PERIOD_90K));
        }
    }

    /// The sound runs on over the 33-bit wrap with the pictures, on its own
    /// and beside them, where it is the PCR's as well. It stopped there: a PTS
    /// a tick past the last for every packet after the wrap, for the rest of
    /// the input's life.
    #[test]
    fn the_sound_runs_on_over_the_33_bit_wrap() {
        use crate::engine::rtmp::ts_mux::PCR_LEAD_90K as LEAD;
        const WRAP: u64 = 1 << 33;
        for video in [true, false] {
            let mut w = av_whip_mux();
            w.muxer.set_has_video(video);
            let t0 = std::time::Instant::now();
            // The input's first frame, and a session a second short of the
            // wrap, 26.5 h on.
            w.session(true, true);
            let _ = fed(&mut w, t0, &opus(0, 1, 0xFC));
            w.session(true, true);
            let from_us = (WRAP - LEAD) * 100 / 9 - 1_000_000;
            let mut frames = opus(from_us, 200, 0xFC);
            if video {
                frames.extend(pictures(&(0..100).collect::<Vec<_>>(), from_us, PERIOD_90K, |k| k * PERIOD_US));
            }
            let muxed = fed(&mut w, t0, &arriving(frames));
            let from = LEAD + from_us * 9 / 100;
            assert_eq!(pts_on(&muxed, 0x0101), (0..200).map(|k| (from + k * 1_800) % WRAP).collect::<Vec<_>>());
            let pcrs = pcrs_in(&muxed);
            assert!(pcrs[1..].iter().all(|p| !p.1), "the new session's DI alone");
            assert!(pcr_steps(&pcrs).iter().all(|s| (1..=PERIOD_90K).contains(s)), "{:?}", pcr_steps(&pcrs));
        }
    }

    /// A WHIP publish is not looked at: one whose second picture is stamped
    /// 1.5 s past its first, sent 40 ms after it, is timed from its first as
    /// it always was — its PTS 1.5 s apart.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whip_publishs_first_picture_is_never_taken_for_cached() {
        use crate::api::webrtc::registry::NewSessionMsg;
        use crate::engine::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
        use std::time::{Duration, Instant};
        use str0m::media::{Frequency, MediaTime};

        let config: crate::config::models::WebrtcInputConfig =
            serde_json::from_value(serde_json::json!({ "public_ip": "127.0.0.1" })).unwrap();
        let (broadcast_tx, mut published) = tokio::sync::broadcast::channel(16_384);
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

        let peer = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
        let mut publisher = WebrtcSession::new(&peer).await.unwrap();
        let (offer, pending) = publisher.create_offer(true, false, true).unwrap();
        let (reply, answer) = tokio::sync::oneshot::channel();
        session_tx.send(NewSessionMsg { offer_sdp: offer, reply }).await.unwrap();
        let (answer, _session, _cancel) = answer.await.unwrap().unwrap();
        publisher.apply_answer(&answer, pending).unwrap();
        let connected = tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(publisher.poll_event(&cancel).await, SessionEvent::Connected) {}
        })
        .await;
        assert!(connected.is_ok(), "the publisher never connected");
        publisher.drain_pending_events();
        let mid = publisher.video_mid.unwrap();
        let pt = publisher.get_pt(mid).expect("H.264 accepted");
        let sps = hex(SPS_IN_ORDER);
        let idr = [&[0, 0, 0, 1], &sps[..], &[0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88, 0x84]].concat();
        let mut ts = Vec::new();
        for k in 0..20u64 {
            let at = if k == 0 { 5_000 } else { 5_000 + 135_000 + (k - 1) * 3_600 };
            let at = MediaTime::new(at, Frequency::NINETY_KHZ);
            let picture = if k == 0 { idr.clone() } else { p_slice(6 + k as u32) };
            publisher.write_media(mid, pt, Instant::now(), at, &picture).unwrap();
            publisher.drain_outputs().await;
            tokio::time::sleep(Duration::from_millis(40)).await;
            let _ = publisher.drive_udp_io().await;
            while let Ok(pkt) = published.try_recv() {
                ts.extend(pkt.data.chunks(188).map(bytes::Bytes::copy_from_slice));
            }
        }
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        let pes = video_pes_of(&ts);
        assert!(pes.len() > 10 && pes[0].1.contains(&5), "{pes:?}");
        assert_eq!(pes[1].0 - pes[0].0, 135_000);
    }

    /// The whole input: a pull of a server that primes it with an IDR 1.5 s
    /// old and then sends the live pictures and their sound carries the two
    /// together — each picture within 200 ms of the packet sent with it,
    /// where the pictures went out 1.5 s past theirs.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn a_whep_pull_primed_with_a_cached_idr_keeps_its_sound_with_its_pictures() {
        use crate::engine::ts_parse::{extract_pes_pts, ts_pid, ts_pusi};
        let script = WhepScript {
            video: true,
            audio: true,
            for_ms: 1_200,
            gop: true,
            cached_ms: 1_500,
            ..WhepScript::default()
        };
        let server = serve_whep_scripted(|a| a, script, None).await;
        let (ts, _) = pull_whep(server.addr, 1_600).await;
        server.task.abort();
        let pictures = video_pes_of(&ts);
        let packets: Vec<u64> =
            ts.iter().filter(|p| ts_pid(p) == 0x0101 && ts_pusi(p)).filter_map(|p| extract_pes_pts(p)).collect();
        assert!(pictures.len() > 10 && packets.len() > 20, "{} pictures, {} packets", pictures.len(), packets.len());
        assert!(pictures[0].1.contains(&5), "the IDR first");
        assert_eq!(pictures.iter().filter(|p| p.1.contains(&5)).count(), 1);
        assert!(pictures.windows(2).all(|w| w[0].0 < w[1].0), "in order: {pictures:?}");
        // Picture k was sent with packet 2k.
        for k in 1..pictures.len().min(packets.len() / 2) {
            let apart = pictures[k].0 as i64 - packets[2 * k] as i64;
            assert!(apart.abs() < 18_000, "picture {k} {} ms past its sound", apart / 90);
        }
    }
}
