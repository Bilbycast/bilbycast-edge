// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WebRTC session wrapper around str0m.
//!
//! Manages the lifecycle of a single WebRTC PeerConnection: ICE, DTLS,
//! SRTP, and media I/O. Integrates str0m's sans-I/O model with tokio
//! by driving the UDP socket and str0m poll loop in a select! loop.
//!
//! **KNOWN DIVERGENCE FROM THE RELAY'S VENDORED COPY — do not "resync" it
//! away.** `bilbycast-relay::distribution::webrtc::session` is vendored from
//! this file and is meant to track it, but it carries one thing this copy does
//! not: `PeerPin`, an ingress source filter that closes an ICE-Lite
//! peer-reflexive UDP reflector. In lite mode `is` mints a peer-reflexive
//! candidate from any STUN Binding Request carrying a valid MESSAGE-INTEGRITY
//! and nominates it on the sender's own PRIORITY, so a client that completed
//! one legitimate session (it holds our ice-pwd from the answer) can repoint
//! the whole SRTP stream at a spoofed address with one ~100-byte datagram.
//!
//! This file runs the same state machine in the same ICE-Lite server role from
//! `engine::input_webrtc` (WHIP ingest) and `engine::output_webrtc` (WHEP
//! output), so **the reflector is still present here**. It is recorded as
//! accepted residual risk rather than fixed: the edge's `/whip` and `/whep`
//! routes sit behind `api::server`'s `auth_middleware`, so exploiting it costs
//! one valid edge API credential — which any legitimate WHEP viewer of this
//! edge already holds. Porting `PeerPin` over is the fix; until then a sync in
//! either direction must carry the relay's control forward, never delete it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use str0m::change::SdpOffer;
use str0m::format::{Codec, PayloadParams};
use str0m::media::{Direction, MediaKind, MediaTime, Mid, Pt};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use str0m::net::Protocol;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::manager::events::{EventSender, EventSeverity, category};

/// str0m's built-in H.264 payload types (`CodecConfig::enable_h264`, str0m
/// 0.24.1), re-registered one for one with the level raised to 5.1 (`0x33`):
/// `(PT, RTX PT, packetization-mode=1, profile-level-id)`. Only the level
/// byte differs from str0m's table — see [`rtc_config`].
const H264_LEVEL_5_1: [(u8, u8, bool, u32); 7] = [
    (127, 121, true, 0x42_00_33),  // Baseline
    (125, 107, false, 0x42_00_33),
    (108, 109, true, 0x42_e0_33),  // Constrained Baseline
    (124, 120, false, 0x42_e0_33),
    (123, 119, true, 0x4d_00_33),  // Main
    (35, 36, false, 0x4d_00_33),
    (114, 115, true, 0x64_00_33),  // High
];

/// The configuration every session's `Rtc` is built from: ICE role, receive
/// tuning, and the codec set — Opus, plus H.264 in str0m's seven built-in
/// profile / packetization-mode variants, at level 5.1. Nothing else.
///
/// **The codec set.** The edge writes and muxes H.264 and Opus only, so that
/// is all an SDP of ours offers or answers:
///
/// * str0m's built-in H.264 entries are level 3.1 (`0x1f`). They are
///   *replaced* by the level-5.1 table above, not supplemented. Through edge
///   0.114.0 four level-5.1 entries were added beside them, which gave one
///   profile two local entries; since str0m 0.22 a level mismatch only lowers
///   the match score, so wherever the peer's PTs are binding — a `recvonly` or
///   `sendrecv` offer (every browser WHEP viewer), or an answer to an offer of
///   ours that keeps one H.264 PT — both entries locked the same remote PT and
///   str0m panicked ("Pt locked multiple times", `assert_claim_once`). The
///   first extra also put its RTX on PT 111, Opus's own PT, so Opus was moved
///   off it (onto Chrome's telephone-event PT 110, in a Chrome publish).
/// * The rest of str0m's default set — VP8, VP9, AV1, H.265 — is left out.
///   With it, VP8 headed the answer to a browser's publish (so the browser
///   sent VP8, which the WHIP input muxes as H.264), and `Writer::
///   payload_params` listed VP8 first on any m-line that carried it.
/// * Level 5.1 is what the answer advertises, whatever level the peer offered:
///   str0m does not narrow H.264's level in an answer (its own documented
///   limitation in `update_param`), and every entry carries
///   `level-asymmetry-allowed=1`. A level-3.1 entry would advertise less than
///   a 1080p or 4K contribution needs.
///
/// **Keep this identical to `bilbycast-relay`'s
/// `distribution::webrtc::session`** — the edge's WHIP output publishes into
/// the relay's WHIP ingest, and both answer browsers.
///
/// **Receive reordering.** str0m 0.24 added a receive-reorder deadline (2 s
/// video, 1 s audio) that holds every later frame behind an incomplete one
/// until it expires. Without retransmission, one lost packet then stalls video
/// about twice as long as 0.23 did. `None` restores 0.23's release by frame
/// count, which is what this pipeline was tuned against.
fn rtc_config(ice_lite: bool) -> RtcConfig {
    let mut config = Rtc::builder()
        .set_ice_lite(ice_lite)
        .set_reordering_timeout_video(None)
        .set_reordering_timeout_audio(None)
        .clear_codecs()
        .enable_opus(true, false);
    let codecs = config.codec_config();
    for (pt, rtx, packetization_mode_1, profile_level_id) in H264_LEVEL_5_1 {
        codecs.add_h264(pt.into(), Some(rtx.into()), packetization_mode_1, profile_level_id);
    }
    config
}

/// A panic str0m raised while negotiating with one peer, caught so that it
/// fails that peer alone (see [`isolate_negotiation`]).
#[derive(Debug)]
pub struct NegotiationPanic {
    /// The step that panicked: `"session setup"`, `"SDP offer"`, `"SDP offer
    /// creation"` or `"SDP answer"`.
    pub step: &'static str,
    /// The panic message.
    pub message: String,
}

impl std::fmt::Display for NegotiationPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "str0m panicked during {}: {}", self.step, self.message)
    }
}

impl std::error::Error for NegotiationPanic {}

/// Run one synchronous str0m negotiation step, turning a panic inside it into
/// a [`NegotiationPanic`] error.
///
/// Every server-side peer is negotiated inline in its input's or output's
/// single task (`whip_input_loop`, `whep_server_loop`), so an unwind there
/// ends the task: every later publisher or viewer finds its reply dropped
/// until the flow restarts. The client loops would stop retrying the same way.
/// str0m keeps asserts on paths a peer's SDP reaches, so contain them here.
///
/// `AssertUnwindSafe` is sound because the session is never used again after
/// an error: every caller drops it (a server loop answers the request with the
/// error and waits for the next peer; a client loop backs off and builds a
/// fresh session).
fn isolate_negotiation<T>(step: &'static str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".to_string());
            let panic = NegotiationPanic { step, message };
            tracing::error!("WebRTC: {panic}; failing this peer only");
            Err(panic.into())
        }
    }
}

/// Raise the Warning event for a negotiation that str0m panicked in, naming
/// the `peer` (e.g. `"WHEP viewer"`). Any other negotiation error is the
/// peer's own — a malformed SDP — and stays a log line, as before.
pub fn report_negotiation_panic(err: &anyhow::Error, events: &EventSender, flow_id: &str, peer: &str) {
    if let Some(panic) = err.downcast_ref::<NegotiationPanic>() {
        events.emit_flow_with_details(
            EventSeverity::Warning,
            category::WEBRTC,
            format!("WebRTC negotiation with {peer} failed: {panic}"),
            flow_id,
            serde_json::json!({
                "error_code": "webrtc_negotiation_panic",
                "peer": peer,
                "step": panic.step,
                "panic": panic.message,
            }),
        );
    }
}

/// The payload type a track is written on, from the payload params its
/// m-line negotiated: H.264 on video, Opus on audio, `None` when the peer
/// accepted neither.
///
/// `Writer::payload_params` lists every negotiated param in codec-config
/// order, and its first entry used to be taken — whatever the codec. On video,
/// packetization-mode 1 is preferred: str0m packetizes into STAP-A / FU-A,
/// which a mode-0 PT forbids, so a mode-0 PT is taken only when the peer
/// accepted no other.
fn send_pt<'a>(kind: MediaKind, params: impl Iterator<Item = &'a PayloadParams>) -> Option<Pt> {
    let mut mode_0 = None;
    for p in params {
        let spec = p.spec();
        match kind {
            MediaKind::Audio if spec.codec == Codec::Opus => return Some(p.pt()),
            MediaKind::Video if spec.codec == Codec::H264 => {
                if spec.format.packetization_mode == Some(1) {
                    return Some(p.pt());
                }
                mode_0.get_or_insert(p.pt());
            }
            _ => {}
        }
    }
    mode_0
}

/// Events produced by the WebRTC session for the caller to handle.
///
/// Some fields are retained for future use (audio support, timing, diagnostics)
/// even though they are not yet consumed by callers.
#[allow(dead_code)]
pub enum SessionEvent {
    /// Received depayloaded media data on a track.
    MediaData {
        mid: Mid,
        pt: Pt,
        /// str0m 0.20 changed `MediaData.data` from `Vec<u8>` to `Arc<[u8]>`.
        /// Carried through as-is rather than `.to_vec()`'d: every consumer only
        /// borrows it (`&data`, indexing, `is_empty`), so widening keeps the
        /// WebRTC ingest path free of a per-frame allocation and copy.
        data: Arc<[u8]>,
        rtp_time: MediaTime,
        network_time: Instant,
        contiguous: bool,
    },
    /// ICE connection state changed.
    IceStateChange(IceConnectionState),
    /// The peer is connected (ICE + DTLS complete).
    Connected,
    /// A new media track was added.
    MediaAdded { mid: Mid, kind: MediaKind },
    /// Incoming keyframe request from the remote peer.
    KeyframeRequest { mid: Mid },
    /// Session has been disconnected or failed.
    Disconnected,
}

/// Configuration for creating a WebRTC session.
pub struct SessionConfig {
    /// Local UDP socket address to bind. Use "0.0.0.0:0" for auto-assign.
    pub bind_addr: SocketAddr,
    /// Public IP to advertise in ICE candidates (optional).
    pub public_ip: Option<std::net::IpAddr>,
    /// Whether this session should behave as an ICE-Lite agent. Set this to
    /// `true` for server-side roles (WHIP input, WHEP output) and `false` for
    /// client-side roles (WHIP output, WHEP input) — str0m rejects the
    /// handshake when both peers advertise `a=ice-lite`.
    pub ice_lite: bool,
}

/// A WebRTC session wrapping str0m's `Rtc` state machine.
pub struct WebrtcSession {
    rtc: Rtc,
    socket: UdpSocket,
    local_addr: SocketAddr,
    /// ICE host candidate IPs we advertised. Used to map incoming packets
    /// to the correct `destination` field for str0m when the socket is
    /// bound to an unspecified address (`0.0.0.0`).
    candidate_ips: Vec<std::net::IpAddr>,
    /// Video track MID (if any).
    pub video_mid: Option<Mid>,
    /// Audio track MID (if any).
    pub audio_mid: Option<Mid>,
    buf: Vec<u8>,
}

impl WebrtcSession {
    /// Create a new session with ICE-lite and bind a UDP socket.
    pub async fn new(config: &SessionConfig) -> Result<Self> {
        let socket = UdpSocket::bind(config.bind_addr).await?;
        let local_addr = socket.local_addr()?;

        // Build the host-candidate set the answer SDP will advertise.
        //
        // When the operator pinned a `public_ip` we honour it verbatim —
        // they know the deployment topology better than we do (NAT 1:1
        // mappings, behind-LB deployments). The caller has *also* bound
        // the UDP socket to that exact IP so the destination address on
        // every incoming packet matches the local candidate (see
        // `engine::input_webrtc::whip_input_loop` for the matching bind
        // logic — without it, the `is` ICE state machine discards every
        // STUN binding request as `unknown interface`).
        //
        // When the bind is unspecified (`0.0.0.0`) we advertise both
        // loopback **and** the route-discovered LAN IP, so same-host
        // peers (loopback / dev / WHIP smoke tests) and real LAN peers
        // both have a candidate they can reach. The previous
        // implementation only advertised the LAN IP and silently broke
        // loopback testing on macOS.
        let port = local_addr.port();
        let route_discovered_lan_ip = || -> Option<std::net::IpAddr> {
            std::net::UdpSocket::bind("0.0.0.0:0")
                .and_then(|s| { s.connect("8.8.8.8:80")?; s.local_addr() })
                .ok()
                .map(|a| a.ip())
                .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        };
        let candidate_ips = select_local_candidate_ips(
            local_addr.ip(),
            config.public_ip,
            route_discovered_lan_ip,
        );

        let rtc = isolate_negotiation("session setup", || {
            let mut rtc = rtc_config(config.ice_lite).build(Instant::now());
            for ip in &candidate_ips {
                let cand_addr = SocketAddr::new(*ip, port);
                let cand = Candidate::host(cand_addr, Protocol::Udp)
                    .map_err(|e| anyhow::anyhow!("ICE candidate error: {}", e))?;
                rtc.add_local_candidate(cand);
                tracing::debug!("WebRTC: added local ICE host candidate {cand_addr}");
            }
            Ok(rtc)
        })?;

        Ok(Self {
            rtc,
            socket,
            local_addr,
            candidate_ips: candidate_ips.clone(),
            video_mid: None,
            audio_mid: None,
            buf: vec![0u8; 2048],
        })
    }

    /// Accept an SDP offer (server mode) and return the SDP answer string.
    ///
    /// A panic inside str0m comes back as a [`NegotiationPanic`] error; the
    /// session must then be dropped, as on any other error.
    pub fn accept_offer(&mut self, offer_sdp: &str) -> Result<String> {
        // str0m 0.18's SDP parser hard-codes the session name field to a
        // single dash (`s=-`) and rejects every other session name. ffmpeg
        // and a number of other production WHIP publishers send a real
        // session name (e.g. `s=FFmpegPublishSession`), which is RFC 4566
        // legal but trips str0m. We normalise the offer here before parsing
        // so the rest of the pipeline doesn't have to know about the quirk.
        let normalised = normalise_sdp_offer_for_str0m(offer_sdp);

        let answer_sdp = isolate_negotiation("SDP offer", || {
            let offer = SdpOffer::from_sdp_string(&normalised)
                .map_err(|e| anyhow::anyhow!("SDP parse error: {}", e))?;

            tracing::info!("SDP offer (normalised):\n{}", normalised);

            let answer = self.rtc.sdp_api().accept_offer(offer)
                .map_err(|e| anyhow::anyhow!("SDP accept error: {}", e))?;
            Ok(answer.to_sdp_string())
        })?;
        tracing::info!("SDP answer:\n{}", answer_sdp);

        // MIDs will be discovered via MediaAdded events
        Ok(answer_sdp)
    }

    /// Create an SDP offer (client mode). Returns the SDP offer string.
    /// The pending offer must be kept and passed to `apply_answer()`.
    ///
    /// A panic inside str0m comes back as a [`NegotiationPanic`] error.
    pub fn create_offer(&mut self, video: bool, audio: bool, send_only: bool) -> Result<(String, str0m::change::SdpPendingOffer)> {
        let direction = if send_only { Direction::SendOnly } else { Direction::RecvOnly };

        let (offer, pending) = isolate_negotiation("SDP offer creation", || {
            let mut api = self.rtc.sdp_api();

            if video {
                let mid = api.add_media(MediaKind::Video, direction, None, None, None);
                self.video_mid = Some(mid);
            }
            if audio {
                let mid = api.add_media(MediaKind::Audio, direction, None, None, None);
                self.audio_mid = Some(mid);
            }

            api.apply().ok_or_else(|| anyhow::anyhow!("No SDP changes to apply"))
        })?;

        let offer_sdp = offer.to_sdp_string();
        tracing::info!("SDP offer (created):\n{}", offer_sdp);
        Ok((offer_sdp, pending))
    }

    /// Apply an SDP answer received from the remote peer (client mode).
    /// Requires the pending offer from `create_offer()`.
    ///
    /// A panic inside str0m comes back as a [`NegotiationPanic`] error; the
    /// session must then be dropped, as on any other error.
    pub fn apply_answer(&mut self, answer_sdp: &str, pending: str0m::change::SdpPendingOffer) -> Result<()> {
        isolate_negotiation("SDP answer", || {
            let answer = str0m::change::SdpAnswer::from_sdp_string(answer_sdp)
                .map_err(|e| anyhow::anyhow!("SDP answer parse error: {}", e))?;

            self.rtc.sdp_api().accept_answer(pending, answer)
                .map_err(|e| anyhow::anyhow!("SDP answer accept error: {}", e))?;

            // Kickstart the ICE agent. After accept_answer the agent has
            // remote candidates and credentials, but str0m's first
            // `poll_output()` may return a `Timeout` with a deadline ~100
            // years in the future ("nothing to do") because the sans-IO
            // state machine hasn't been told to advance time. Without this
            // call, our `poll_event` loop on the sender side sleeps until
            // doomsday and ICE never starts. One zero-cost time injection
            // wakes the agent and the next `poll_output` produces the first
            // STUN binding request immediately.
            let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));

            Ok(())
        })
    }

    /// Get the local socket address.
    /// Retained for diagnostics and future ICE candidate reporting.
    #[allow(dead_code)]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Write media data to a track.
    pub fn write_media(
        &mut self,
        mid: Mid,
        pt: Pt,
        wallclock: Instant,
        rtp_time: MediaTime,
        data: &[u8],
    ) -> Result<()> {
        if let Some(writer) = self.rtc.writer(mid) {
            writer.write(pt, wallclock, rtp_time, data.to_vec())
                .map_err(|e| anyhow::anyhow!("Write error: {}", e))?;
        }
        Ok(())
    }

    /// Drain str0m's pending output queue, sending any queued UDP transmits
    /// to the wire. This MUST be called between consecutive `write_media`
    /// calls — str0m queues writes in `to_payload` (cap 512 since str0m
    /// 0.24, 100 before) and only drains them via `handle_timeout`, which
    /// is reached from a `Output::Timeout` poll cycle. Without this drain,
    /// the inner H.264 fragmentation loop overflows the queue after 512
    /// writes and every subsequent `write_media` returns
    /// `Err("Consecutive calls to write() without poll_output() in
    /// between")`. We feed an `Input::Timeout`
    /// so the per-write payload queue is processed eagerly. Cheap when
    /// there's nothing pending (one no-op timeout + one no-op poll).
    pub async fn drain_outputs(&mut self) {
        // Feed a current-time timeout so str0m runs `do_payload` and turns
        // the just-written sample into RTP packets ready for `poll_output`.
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    let _ = self.socket.send_to(&transmit.contents, transmit.destination).await;
                }
                Ok(Output::Event(event)) => {
                    let _ = self.handle_event(event);
                }
                Ok(Output::Timeout(_)) | Err(_) => break,
            }
        }
    }

    /// The negotiated payload type to write `mid`'s media on: H.264 on a
    /// video m-line, Opus on an audio one (see [`send_pt`]). `None` when the
    /// peer accepted neither, rather than another codec's PT.
    pub fn get_pt(&mut self, mid: Mid) -> Option<Pt> {
        let kind = self.rtc.media(mid)?.kind();
        let writer = self.rtc.writer(mid)?;
        send_pt(kind, writer.payload_params())
    }

    /// Drain all pending str0m events without blocking, populating
    /// `self.video_mid` / `self.audio_mid` from any queued
    /// `MediaAdded` events.
    ///
    /// str0m may emit `Event::Connected` *before* the queued
    /// `MediaAdded` events. Callers that wait only for Connected
    /// can race past the track discovery and end up with
    /// `video_mid == None` (the WHEP viewer "no video MID
    /// negotiated" bug). Call this after Connected to flush any
    /// pending events.
    pub fn drain_pending_events(&mut self) {
        while let Ok(Output::Event(event)) = self.rtc.poll_output() {
            let _ = self.handle_event(event);
        }
    }

    /// Check if the session is still alive.
    /// Retained for future session health monitoring.
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// Drive the session event loop. Blocks until a meaningful event occurs.
    pub async fn poll_event(&mut self, cancel: &CancellationToken) -> SessionEvent {
        loop {
            // Drain all pending str0m outputs
            match self.rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    tracing::trace!("poll_event: Transmit {} bytes -> {}", transmit.contents.len(), transmit.destination);
                    let _ = self.socket.send_to(&transmit.contents, transmit.destination).await;
                    continue;
                }
                Ok(Output::Event(event)) => {
                    tracing::trace!("poll_event: Event {:?}", std::any::type_name_of_val(&event));
                    if let Some(se) = self.handle_event(event) {
                        return se;
                    }
                    continue;
                }
                Ok(Output::Timeout(deadline)) => {
                    // Wait for input
                    let sleep_dur = deadline.saturating_duration_since(Instant::now());
                    tracing::trace!("poll_event: Timeout, sleeping {:?}", sleep_dur);
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            return SessionEvent::Disconnected;
                        }
                        _ = tokio::time::sleep(sleep_dur) => {
                            let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
                            continue;
                        }
                        result = self.socket.recv_from(&mut self.buf) => {
                            match result {
                                Ok((len, source)) => {
                                    let now = Instant::now();
                                    // str0m's DatagramRecv try_into rejects
                                    // datagrams that aren't STUN/DTLS/RTP/RTCP.
                                    // Hostile or stray packets must NOT crash
                                    // the WebRTC session task — drop them and
                                    // keep going.
                                    let contents = match (&self.buf[..len]).try_into() {
                                        Ok(c) => c,
                                        Err(e) => {
                                            tracing::debug!(
                                                "WebRTC: dropped {len}-byte datagram from {source}: {e}"
                                            );
                                            continue;
                                        }
                                    };
                                    let destination = self.destination_for_source(source);
                                    let receive = str0m::net::Receive {
                                        proto: Protocol::Udp,
                                        source,
                                        destination,
                                        contents,
                                    };
                                    let _ = self.rtc.handle_input(Input::Receive(now, receive));
                                    continue;
                                }
                                Err(e) => {
                                    tracing::error!("UDP recv error: {}", e);
                                    return SessionEvent::Disconnected;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("str0m error: {}", e);
                    return SessionEvent::Disconnected;
                }
            }
        }
    }

    /// Map an incoming packet's source address to the correct local
    /// destination address that str0m expects.
    ///
    /// When the socket is bound to an unspecified address (`0.0.0.0`),
    /// `self.local_addr` is `0.0.0.0:<port>` — which doesn't match any
    /// ICE host candidate. str0m's ICE agent routes packets by matching
    /// `(source, destination)` to a candidate pair; if the destination
    /// doesn't match a local candidate, the packet is silently discarded.
    ///
    /// This method picks the correct candidate IP based on the source:
    /// - Source is loopback → prefer loopback candidate
    /// - Source is non-loopback → prefer non-loopback candidate
    /// - Fallback → first candidate
    ///
    /// When `local_addr` is already a specific IP (operator set
    /// `public_ip`, or bound to a specific interface), it matches the
    /// candidate directly, so we return it as-is.
    fn destination_for_source(&self, source: SocketAddr) -> SocketAddr {
        resolve_destination(self.local_addr, &self.candidate_ips, source)
    }

    /// Non-blocking: receive any pending UDP packets and feed them to
    /// str0m, then drain all pending transmits. Returns the first
    /// meaningful session event (if any) discovered while processing.
    ///
    /// Designed for the WHIP client output and WHEP viewer send loops,
    /// which need to keep str0m alive (RTCP, STUN keepalives) while
    /// they are primarily driven by the broadcast channel. Call this
    /// after writing media and after each broadcast packet batch.
    pub async fn drive_udp_io(&mut self) -> Option<SessionEvent> {
        let mut event_out: Option<SessionEvent> = None;

        // Non-blocking receive loop: drain all pending UDP packets.
        loop {
            match self.socket.try_recv_from(&mut self.buf) {
                Ok((len, source)) => {
                    let now = Instant::now();
                    let contents = match (&self.buf[..len]).try_into() {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let destination = self.destination_for_source(source);
                    let receive = str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination,
                        contents,
                    };
                    let _ = self.rtc.handle_input(Input::Receive(now, receive));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // Process any pending timeouts.
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));

        // Drain all pending str0m outputs (transmits + events).
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    let _ = self.socket.send_to(&transmit.contents, transmit.destination).await;
                }
                Ok(Output::Event(ev)) => {
                    if event_out.is_none() {
                        event_out = self.handle_event(ev);
                    }
                    // Continue draining even if we got an event.
                }
                Ok(Output::Timeout(_)) | Err(_) => break,
            }
        }

        event_out
    }

    fn handle_event(&mut self, event: Event) -> Option<SessionEvent> {
        match event {
            Event::Connected => {
                tracing::info!("WebRTC connected (ICE + DTLS complete)");
                Some(SessionEvent::Connected)
            }
            Event::IceConnectionStateChange(state) => {
                tracing::debug!("ICE state: {:?}", state);
                match state {
                    IceConnectionState::Disconnected => Some(SessionEvent::Disconnected),
                    _ => Some(SessionEvent::IceStateChange(state)),
                }
            }
            Event::MediaAdded(added) => {
                let kind = self.rtc.media(added.mid)?.kind();
                match kind {
                    MediaKind::Video => self.video_mid = Some(added.mid),
                    MediaKind::Audio => self.audio_mid = Some(added.mid),
                }
                tracing::info!(
                    "Media track added: {:?} mid={:?} (direction={:?})",
                    kind,
                    added.mid,
                    self.rtc.media(added.mid).map(|m| m.direction()),
                );
                Some(SessionEvent::MediaAdded { mid: added.mid, kind })
            }
            Event::MediaData(data) => {
                tracing::trace!(
                    "MediaData: mid={:?} pt={} len={} contiguous={}",
                    data.mid,
                    data.pt,
                    data.data.len(),
                    data.contiguous,
                );
                Some(SessionEvent::MediaData {
                    mid: data.mid,
                    pt: data.pt,
                    data: data.data,
                    rtp_time: data.time,
                    network_time: data.network_time,
                    contiguous: data.contiguous,
                })
            }
            Event::KeyframeRequest(kf) => {
                Some(SessionEvent::KeyframeRequest { mid: kf.mid })
            }
            _ => None,
        }
    }
}

/// Pick the set of local IPs to advertise as ICE host candidates.
///
/// This is the pure-data half of `WebrtcSession::new`'s candidate-selection
/// logic, broken out so it can be unit-tested without binding real
/// sockets. The interesting cases are:
///
/// - **Operator pinned `public_ip`** — return exactly that IP. The operator
///   knows the deployment topology better than we do (e.g. NAT 1:1
///   mappings, behind-LB deployments), so honour it verbatim. The caller
///   is expected to bind the UDP socket to that same IP so per-packet
///   destination matches the local candidate.
/// - **Bound to an unspecified address** (`0.0.0.0` / `::`) — advertise
///   loopback **and** the route-discovered LAN IP, so both same-host
///   peers (loopback / unit tests / WHIP smoke tests on a developer
///   laptop) and real LAN peers can reach us. The previous implementation
///   only advertised the LAN IP and silently broke loopback testing on
///   macOS — see the 2026-04-09 Bug A fix in QUALITY_REPORT.md.
/// - **Bound to a specific interface** — advertise that interface's IP.
fn select_local_candidate_ips(
    bound_ip: std::net::IpAddr,
    pinned: Option<std::net::IpAddr>,
    route_discovered_lan_ip: impl FnOnce() -> Option<std::net::IpAddr>,
) -> Vec<std::net::IpAddr> {
    if let Some(p) = pinned {
        return vec![p];
    }
    if !bound_ip.is_unspecified() {
        return vec![bound_ip];
    }
    let mut out: Vec<std::net::IpAddr> =
        vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)];
    if let Some(lan) = route_discovered_lan_ip()
        && !out.contains(&lan) {
            out.push(lan);
        }
    out
}

/// Map an incoming packet's source address to the correct local destination
/// address for str0m's `Receive` struct.
///
/// When the socket is bound to `0.0.0.0`, `local_addr` is `0.0.0.0:<port>`
/// which doesn't match any ICE host candidate. str0m routes packets by
/// matching `(source, destination)` to a candidate pair; mismatched
/// destination causes silent packet drops — the root cause of Scenario K
/// (no `Event::MediaData` after ICE+DTLS complete) and Scenario L (ICE
/// stuck in Checking on the server side).
///
/// Logic:
/// - If `local_addr` is a specific IP → use it (matches the candidate).
/// - If single candidate → use that candidate.
/// - If multiple candidates → match source locality (loopback ↔ loopback,
///   LAN ↔ LAN).
/// - Fallback → first candidate.
fn resolve_destination(
    local_addr: SocketAddr,
    candidate_ips: &[std::net::IpAddr],
    source: SocketAddr,
) -> SocketAddr {
    if !local_addr.ip().is_unspecified() {
        return local_addr;
    }

    let port = local_addr.port();

    if candidate_ips.len() == 1 {
        let dest = SocketAddr::new(candidate_ips[0], port);
        tracing::trace!(
            "WebRTC destination mapped: source={source} → dest={dest} (single candidate)"
        );
        return dest;
    }

    let src_is_loopback = source.ip().is_loopback();

    // Try to match: loopback source → loopback candidate, LAN → LAN.
    for &ip in candidate_ips {
        if src_is_loopback == ip.is_loopback() {
            let dest = SocketAddr::new(ip, port);
            tracing::trace!(
                "WebRTC destination mapped: source={source} → dest={dest}"
            );
            return dest;
        }
    }

    // Fallback to first candidate.
    if let Some(&ip) = candidate_ips.first() {
        let dest = SocketAddr::new(ip, port);
        tracing::trace!(
            "WebRTC destination fallback: source={source} → dest={dest}"
        );
        dest
    } else {
        local_addr
    }
}

/// Normalise an incoming SDP offer so str0m's overly strict parser will
/// accept it.
///
/// Workarounds applied (all safe — affect only descriptive/grouping
/// metadata, never ICE, DTLS, crypto, or codec semantics):
///
/// 1. **Session name** (`s=`): str0m 0.18 hard-codes `s=-` and rejects
///    any other value. ffmpeg sends `s=FFmpegPublishSession`. We rewrite
///    to `s=-`.
///
/// 2. **BUNDLE group** (`a=group:BUNDLE`): ffmpeg 8.x WHIP muxer emits
///    `a=group:BUNDLE 0 1` but only includes one m-section with
///    `a=mid:1` — mid 0 doesn't exist. str0m tries to reconcile the
///    group with the actual m-sections and silently drops the codec
///    payload parameters, producing an answer with an empty
///    `m=video 0 UDP/TLS/RTP/SAVPF ` line. We rewrite the BUNDLE group
///    to only list MIDs that have a corresponding `a=mid:X` attribute.
fn normalise_sdp_offer_for_str0m(offer: &str) -> String {
    // First pass: collect all MIDs declared in the SDP via `a=mid:X`.
    let mut declared_mids: Vec<String> = Vec::new();
    for line in offer.lines() {
        let trimmed = line.trim();
        if let Some(mid) = trimmed.strip_prefix("a=mid:") {
            declared_mids.push(mid.to_string());
        }
    }

    // Second pass: rewrite.
    let mut out = String::with_capacity(offer.len());
    let mut session_name_rewritten = false;
    let mut bundle_rewritten = false;

    for raw_line in offer.split_inclusive('\n') {
        let line_no_eol = raw_line.trim_end_matches(['\r', '\n']);
        let eol = &raw_line[line_no_eol.len()..];

        // Workaround 1: session name
        if !session_name_rewritten && line_no_eol.starts_with("s=") && line_no_eol != "s=-" {
            out.push_str("s=-");
            out.push_str(eol);
            session_name_rewritten = true;
            continue;
        }

        // Workaround 2: BUNDLE group with phantom MIDs.
        if !bundle_rewritten && line_no_eol.starts_with("a=group:BUNDLE ") {
            let bundle_mids: Vec<&str> = line_no_eol
                .strip_prefix("a=group:BUNDLE ")
                .unwrap_or("")
                .split_whitespace()
                .filter(|mid| declared_mids.iter().any(|d| d == mid))
                .collect();
            if !bundle_mids.is_empty() {
                out.push_str("a=group:BUNDLE ");
                out.push_str(&bundle_mids.join(" "));
                out.push_str(eol);
            }
            // If no valid MIDs remain, drop the BUNDLE line entirely.
            bundle_rewritten = true;
            continue;
        }

        out.push_str(raw_line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_replaces_real_session_name_with_dash() {
        let offer = "v=0\r\n\
                     o=- 123 2 IN IP4 127.0.0.1\r\n\
                     s=FFmpegPublishSession\r\n\
                     t=0 0\r\n";
        let out = normalise_sdp_offer_for_str0m(offer);
        assert!(out.contains("\r\ns=-\r\n"));
        assert!(!out.contains("FFmpegPublishSession"));
    }

    #[test]
    fn normalise_leaves_dash_session_name_alone() {
        let offer = "v=0\r\ns=-\r\nt=0 0\r\n";
        assert_eq!(normalise_sdp_offer_for_str0m(offer), offer);
    }

    #[test]
    fn normalise_only_rewrites_first_session_name() {
        // Per RFC 4566 there is exactly one s= line per SDP, but a media
        // description in some pathological inputs might contain a literal
        // `s=` substring. Make sure we don't accidentally touch m=/a= lines
        // that happen to start with `s` later in the document.
        let offer = "v=0\r\n\
                     s=Foo\r\n\
                     t=0 0\r\n\
                     m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                     a=sendonly\r\n";
        let out = normalise_sdp_offer_for_str0m(offer);
        assert!(out.contains("\r\ns=-\r\n"));
        assert!(out.contains("a=sendonly"));
    }

    #[test]
    fn normalise_preserves_lf_only_line_endings() {
        let offer = "v=0\ns=Whatever\nt=0 0\n";
        let out = normalise_sdp_offer_for_str0m(offer);
        assert_eq!(out, "v=0\ns=-\nt=0 0\n");
    }

    /// ffmpeg 8.x WHIP muxer emits `a=group:BUNDLE 0 1` but only has one
    /// m-section with `a=mid:1`. The phantom mid=0 reference confuses
    /// str0m into generating an answer with empty payload types. Our
    /// normaliser must strip the phantom MID from the BUNDLE group.
    #[test]
    fn normalise_strips_phantom_mids_from_bundle() {
        let offer = "v=0\r\n\
                     o=FFmpeg 123 2 IN IP4 127.0.0.1\r\n\
                     s=-\r\n\
                     t=0 0\r\n\
                     a=group:BUNDLE 0 1\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 106\r\n\
                     a=mid:1\r\n\
                     a=rtpmap:106 H264/90000\r\n";
        let out = normalise_sdp_offer_for_str0m(offer);
        assert!(out.contains("a=group:BUNDLE 1\r\n"), "BUNDLE should only list mid=1, got: {}", out);
        assert!(out.contains("a=mid:1"));
        assert!(out.contains("a=rtpmap:106 H264/90000"));
    }

    /// When the BUNDLE group is valid (all MIDs exist), leave it alone.
    #[test]
    fn normalise_preserves_valid_bundle() {
        let offer = "v=0\r\ns=-\r\nt=0 0\r\n\
                     a=group:BUNDLE 0 1\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                     a=mid:0\r\n\
                     m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                     a=mid:1\r\n";
        let out = normalise_sdp_offer_for_str0m(offer);
        assert!(out.contains("a=group:BUNDLE 0 1\r\n"));
    }

    /// Bug A regression (2026-04-09): when the WebRTC socket is bound to
    /// `0.0.0.0` and the operator did not pin a `public_ip`, we MUST
    /// advertise loopback in addition to the LAN IP so same-host peers
    /// (notably ffmpeg WHIP on a developer laptop) can reach us. The
    /// previous implementation only advertised the LAN IP, which silently
    /// broke loopback ICE/DTLS on macOS.
    #[test]
    fn select_candidate_ips_unspecified_bind_advertises_loopback_and_lan() {
        let lan: std::net::IpAddr = "192.168.7.42".parse().unwrap();
        let ips = select_local_candidate_ips(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            None,
            || Some(lan),
        );
        assert_eq!(ips.len(), 2);
        assert_eq!(ips[0], std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        assert_eq!(ips[1], lan);
    }

    #[test]
    fn select_candidate_ips_unspecified_bind_falls_back_to_loopback_only() {
        // No discoverable LAN IP (e.g. host has no default route).
        let ips = select_local_candidate_ips(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            None,
            || None,
        );
        assert_eq!(ips, vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)]);
    }

    #[test]
    fn select_candidate_ips_pinned_public_ip_wins() {
        let pinned: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        let ips = select_local_candidate_ips(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            Some(pinned),
            || panic!("must not call route discovery when public_ip is pinned"),
        );
        assert_eq!(ips, vec![pinned]);
    }

    #[test]
    fn select_candidate_ips_specific_bind_uses_bound_ip() {
        let bound: std::net::IpAddr = "10.0.0.5".parse().unwrap();
        let ips = select_local_candidate_ips(
            bound,
            None,
            || panic!("must not call route discovery when bind is specific"),
        );
        assert_eq!(ips, vec![bound]);
    }

    #[test]
    fn select_candidate_ips_dedupes_loopback_lan() {
        // Pathological: route discovery returns loopback. Don't list twice.
        let lo: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        let ips = select_local_candidate_ips(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            None,
            || Some(lo),
        );
        // Route-discovery filter inside `WebrtcSession::new` rejects
        // loopback before passing it in, so this would normally be `None`,
        // but the dedupe logic should still hold defensively.
        assert_eq!(ips, vec![lo]);
    }

    // ── resolve_destination tests ──────────────────────────────────────

    #[test]
    fn resolve_dest_specific_bind_returns_local_addr() {
        let local: SocketAddr = "10.0.0.5:5000".parse().unwrap();
        let candidates = vec!["10.0.0.5".parse().unwrap()];
        let source: SocketAddr = "192.168.1.100:9999".parse().unwrap();
        assert_eq!(resolve_destination(local, &candidates, source), local);
    }

    #[test]
    fn resolve_dest_single_candidate() {
        let local: SocketAddr = "0.0.0.0:5000".parse().unwrap();
        let candidates = vec!["127.0.0.1".parse().unwrap()];
        let source: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let expected: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        assert_eq!(resolve_destination(local, &candidates, source), expected);
    }

    #[test]
    fn resolve_dest_loopback_source_picks_loopback_candidate() {
        let local: SocketAddr = "0.0.0.0:5000".parse().unwrap();
        let lo: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let lan: std::net::IpAddr = "192.168.7.42".parse().unwrap();
        let candidates = vec![lo, lan];
        let source: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        assert_eq!(
            resolve_destination(local, &candidates, source),
            SocketAddr::new(lo, 5000),
        );
    }

    #[test]
    fn resolve_dest_lan_source_picks_lan_candidate() {
        let local: SocketAddr = "0.0.0.0:5000".parse().unwrap();
        let lo: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let lan: std::net::IpAddr = "192.168.7.42".parse().unwrap();
        let candidates = vec![lo, lan];
        let source: SocketAddr = "192.168.7.100:9999".parse().unwrap();
        assert_eq!(
            resolve_destination(local, &candidates, source),
            SocketAddr::new(lan, 5000),
        );
    }

    #[test]
    fn resolve_dest_empty_candidates_falls_back_to_local() {
        let local: SocketAddr = "0.0.0.0:5000".parse().unwrap();
        let source: SocketAddr = "10.0.0.1:9999".parse().unwrap();
        assert_eq!(resolve_destination(local, &[], source), local);
    }
}

/// Negotiation against real peers' SDP: what the answer carries, which payload
/// type each track is then written on, and what one peer's str0m panic costs.
#[cfg(test)]
mod negotiation_tests {
    use super::*;
    use std::collections::HashMap;

    /// A real Chrome 124 WHEP offer (two `recvonly` m-lines), as it reached
    /// `accept_offer` in the 2026-10-07 interop run — where it panicked str0m
    /// ("Pt locked multiple times: 102") and took the WHEP output down.
    const CHROME_WHEP_OFFER: &str = include_str!("testdata/chrome124-whep-recvonly.sdp");
    /// A real Chrome 124 WHIP publish: `addTransceiver(track, { direction:
    /// 'sendonly' })` for a canvas video and a WebAudio track, no codec
    /// preferences. VP8 first, then H.264, AV1, VP9, red, rtx and ulpfec.
    const CHROME_WHIP_OFFER: &str = include_str!("testdata/chrome124-whip-sendonly.sdp");
    /// A real Chrome 124 WHEP offer narrowed by `setCodecPreferences` to its
    /// packetization-mode=0 H.264 entries, from the same interop run.
    const CHROME_WHEP_MODE_0_OFFER: &str = include_str!("testdata/chrome124-whep-recvonly-pm0.sdp");
    /// A hand-made `recvonly` offer whose one RTX PT repairs two H.264 PTs
    /// (Baseline in packetization mode 1 and 0). str0m 0.24.1 locks that RTX
    /// PT once per primary and panics ("Pt locked multiple times: 103") with
    /// any H.264 set it ships, this one included — so it stands for whatever
    /// negotiation panic str0m still has.
    const RTX_REPAIRING_TWO_PTS: &str = include_str!("testdata/rtx-repairing-two-pts.sdp");

    const FINGERPRINT: &str = "5B:3E:8C:A1:0F:77:2D:94:C6:E2:19:B8:4A:D0:63:F5:\
                               8E:21:CA:7B:90:3F:E6:58:12:AD:C4:6E:B9:07:F3:2C";

    /// An offer from FFmpeg's WHIP muxer: the `generate_sdp_offer` template in
    /// FFmpeg n9.0.2's `libavformat/whip.c` (the vendored copy), rendered for
    /// an Opus + H.264 publish. FFmpeg always sends on PT 106 / 105 / 111,
    /// whatever the answer says, so the answer must keep those.
    fn ffmpeg_whip_offer(profile_level_id: &str) -> String {
        let ice = "a=ice-ufrag:4f3a9c1e\r\n\
                   a=ice-pwd:9b1e6c0d2f7a4e8b5c3d1a0f6e2b7c94\r\n";
        format!(
            "v=0\r\n\
             o=FFmpeg 4489045141692799359 2 IN IP4 127.0.0.1\r\n\
             s=FFmpegPublishSession\r\n\
             t=0 0\r\n\
             a=group:BUNDLE 0 1\r\n\
             a=extmap-allow-mixed\r\n\
             a=msid-semantic: WMS\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             c=IN IP4 0.0.0.0\r\n\
             {ice}\
             a=fingerprint:sha-256 {FINGERPRINT}\r\n\
             a=setup:passive\r\n\
             a=mid:0\r\n\
             a=sendonly\r\n\
             a=msid:FFmpeg audio\r\n\
             a=rtcp-mux\r\n\
             a=rtpmap:111 opus/48000/2\r\n\
             a=ssrc:2780934121 cname:FFmpeg\r\n\
             a=ssrc:2780934121 msid:FFmpeg audio\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF 106 105\r\n\
             c=IN IP4 0.0.0.0\r\n\
             {ice}\
             a=fingerprint:sha-256 {FINGERPRINT}\r\n\
             a=setup:passive\r\n\
             a=mid:1\r\n\
             a=sendonly\r\n\
             a=msid:FFmpeg video\r\n\
             a=rtcp-mux\r\n\
             a=rtcp-rsize\r\n\
             a=rtpmap:106 H264/90000\r\n\
             a=fmtp:106 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}\r\n\
             a=rtcp-fb:106 nack\r\n\
             a=rtpmap:105 rtx/90000\r\n\
             a=fmtp:105 apt=106\r\n\
             a=ssrc-group:FID 2780934122 2780934123\r\n\
             a=ssrc:2780934122 cname:FFmpeg\r\n\
             a=ssrc:2780934122 msid:FFmpeg video\r\n"
        )
    }

    /// A server's answer to one of our offers that keeps a single H.264 PT —
    /// at the server's own profile-level-id — plus Opus.
    fn single_pt_answer(
        offer: &str,
        video_mid: Mid,
        audio_mid: Mid,
        (pt, rtx, profile_level_id): (u8, u8, &str),
    ) -> String {
        let dir = if offer.contains("a=sendonly") { "recvonly" } else { "sendonly" };
        let transport = format!(
            "c=IN IP4 0.0.0.0\r\n\
             a=ice-ufrag:srvu\r\n\
             a=ice-pwd:serverpasswordserverpw\r\n\
             a=fingerprint:sha-256 {FINGERPRINT}\r\n\
             a=setup:passive\r\n"
        );
        format!(
            "v=0\r\n\
             o=- 1 2 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE {video_mid} {audio_mid}\r\n\
             a=ice-lite\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF {pt} {rtx}\r\n\
             {transport}\
             a=mid:{video_mid}\r\n\
             a={dir}\r\n\
             a=rtcp-mux\r\n\
             a=rtpmap:{pt} H264/90000\r\n\
             a=fmtp:{pt} level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}\r\n\
             a=rtpmap:{rtx} rtx/90000\r\n\
             a=fmtp:{rtx} apt={pt}\r\n\
             a=candidate:1 1 udp 2130706431 127.0.0.1 5000 typ host\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             {transport}\
             a=mid:{audio_mid}\r\n\
             a={dir}\r\n\
             a=rtcp-mux\r\n\
             a=rtpmap:111 opus/48000/2\r\n\
             a=fmtp:111 minptime=10;useinbandfec=1\r\n"
        )
    }

    /// One m-section: its kind, mid, the PTs on its m-line, and each PT's
    /// `a=rtpmap` encoding name (upper-cased) and `a=fmtp` parameters.
    #[derive(Debug, Default)]
    struct Section {
        kind: String,
        mid: String,
        pts: Vec<u8>,
        codec: HashMap<u8, String>,
        fmtp: HashMap<u8, String>,
    }

    fn sections(sdp: &str) -> Vec<Section> {
        let mut out: Vec<Section> = Vec::new();
        for line in sdp.lines().map(str::trim_end) {
            if let Some(m) = line.strip_prefix("m=") {
                let mut f = m.split_whitespace();
                let kind = f.next().unwrap_or_default().to_string();
                let pts = f.skip(2).filter_map(|p| p.parse().ok()).collect();
                out.push(Section { kind, pts, ..Default::default() });
                continue;
            }
            let Some(s) = out.last_mut() else { continue };
            if let Some(mid) = line.strip_prefix("a=mid:") {
                s.mid = mid.to_string();
            } else if let Some((pt, rest)) = line.strip_prefix("a=rtpmap:").and_then(|r| r.split_once(' ')) {
                let name = rest.split('/').next().unwrap_or_default().to_ascii_uppercase();
                s.codec.insert(pt.parse().unwrap(), name);
            } else if let Some((pt, rest)) = line.strip_prefix("a=fmtp:").and_then(|r| r.split_once(' ')) {
                s.fmtp.insert(pt.parse().unwrap(), rest.to_string());
            }
        }
        out
    }

    fn section<'a>(all: &'a [Section], kind: &str) -> &'a Section {
        all.iter().find(|s| s.kind == kind).unwrap_or_else(|| panic!("no m={kind} section"))
    }

    fn fmtp_param<'a>(fmtp: &'a str, key: &str) -> Option<&'a str> {
        fmtp.split(';').find_map(|kv| kv.trim().strip_prefix(key)?.strip_prefix('='))
    }

    /// What we put in an SDP of our own, offer or answer: a video m-line of
    /// H.264 (with its RTX) and nothing else, every H.264 PT advertising level
    /// 5.1, and an audio m-line of Opus alone.
    fn assert_h264_and_opus_only(ours: &str) {
        let ours = sections(ours);
        let v = section(&ours, "video");
        assert!(v.pts.iter().any(|pt| v.codec[pt] == "H264"), "no H.264 on the video m-line: {v:?}");
        for pt in &v.pts {
            match v.codec[pt].as_str() {
                "H264" => {
                    let plid = fmtp_param(&v.fmtp[pt], "profile-level-id").unwrap();
                    assert!(plid.to_ascii_lowercase().ends_with("33"),
                        "PT {pt} carries profile-level-id {plid}, not level 5.1");
                }
                "RTX" => {
                    let apt: u8 = fmtp_param(&v.fmtp[pt], "apt").unwrap().parse().unwrap();
                    assert_eq!(v.codec[&apt], "H264", "RTX PT {pt} repairs PT {apt}");
                }
                other => panic!("{other} on video PT {pt}: {v:?}"),
            }
        }
        let a = section(&ours, "audio");
        assert!(!a.pts.is_empty() && a.pts.iter().all(|pt| a.codec[pt] == "OPUS"),
            "audio m-line: {a:?}");
    }

    /// Our answer to `offer` carries H.264 and Opus only (above), and only on
    /// PTs the offer gave the same codec.
    fn assert_answer_keeps_offered_pts(offer: &str, answer: &str) {
        assert_h264_and_opus_only(answer);
        let (offer, answer) = (sections(offer), sections(answer));
        for a in &answer {
            let o = section(&offer, &a.kind);
            for pt in a.pts.iter().filter(|pt| a.codec[pt] != "RTX") {
                assert_eq!(o.codec.get(pt), Some(&a.codec[pt]),
                    "answered {} on {} PT {pt}, which the offer did not give it", a.codec[pt], a.kind);
            }
        }
    }

    async fn server() -> WebrtcSession {
        let bind_addr = "127.0.0.1:0".parse().unwrap();
        WebrtcSession::new(&SessionConfig { bind_addr, public_ip: None, ice_lite: true })
            .await
            .unwrap()
    }

    async fn client() -> WebrtcSession {
        let bind_addr = "127.0.0.1:0".parse().unwrap();
        WebrtcSession::new(&SessionConfig { bind_addr, public_ip: None, ice_lite: false })
            .await
            .unwrap()
    }

    /// Accept `offer` as the server and check the answer and the PT each of
    /// its tracks is written on.
    async fn accept(offer: &str, video_pt: u8) {
        let mut s = server().await;
        let answer = s.accept_offer(offer).unwrap();
        assert_answer_keeps_offered_pts(offer, &answer);
        let answered = sections(&answer);
        let (v, a) = (section(&answered, "video"), section(&answered, "audio"));
        assert_eq!(s.get_pt(Mid::from(v.mid.as_str())), Some(Pt::new_with_value(video_pt)));
        assert_eq!(s.get_pt(Mid::from(a.mid.as_str())), Some(Pt::new_with_value(111)));
    }

    /// The WHEP output's case. Chrome's offer is `recvonly`, so its PTs are
    /// binding; the video goes out on its packetization-mode=1 Baseline PT.
    #[tokio::test]
    async fn a_chrome_whep_offer_is_answered_with_h264_and_opus() {
        accept(CHROME_WHEP_OFFER, 102).await;
    }

    /// The WHIP input's case. VP8 heads Chrome's list; the answer must not
    /// carry it, or Chrome publishes VP8 that the input muxes as H.264.
    #[tokio::test]
    async fn a_chrome_whip_offer_is_answered_with_h264_and_opus() {
        accept(CHROME_WHIP_OFFER, 102).await;
    }

    /// FFmpeg sends on PT 106 whatever the answer says. A High 5.1 publish
    /// was answered on PT 118, so all of its video was discarded.
    #[tokio::test]
    async fn an_ffmpeg_whip_offer_keeps_its_h264_payload_type() {
        accept(&ffmpeg_whip_offer("640033"), 106).await;
        accept(&ffmpeg_whip_offer("42e01f"), 106).await;
    }

    /// The WHIP-output and WHEP-input case: a server that answers our offer
    /// with one H.264 PT. That answer panicked `accept_answer`.
    #[tokio::test]
    async fn a_single_pt_h264_answer_is_accepted_and_written_on() {
        for send_only in [true, false] {
            for chosen in [(127, 121, "42001f"), (108, 109, "42e01f"), (114, 115, "64001f")] {
                let mut c = client().await;
                let (offer, pending) = c.create_offer(true, true, send_only).unwrap();
                let (video_mid, audio_mid) = (c.video_mid.unwrap(), c.audio_mid.unwrap());
                assert_h264_and_opus_only(&offer);
                let answer = single_pt_answer(&offer, video_mid, audio_mid, chosen);
                c.apply_answer(&answer, pending).unwrap();
                assert_eq!(c.get_pt(video_mid), Some(Pt::new_with_value(chosen.0)));
                assert_eq!(c.get_pt(audio_mid), Some(Pt::new_with_value(111)));
            }
        }
    }

    /// Edge to edge, both directions (WHIP output -> WHIP input, WHEP input ->
    /// WHEP output): whichever side sends writes H.264 and Opus. With the old
    /// codec set a WHIP output wrote H.264 on VP8's PT 96 and Opus on 104, and
    /// the WHEP pull panicked `accept_offer` as a browser's offer did.
    #[tokio::test]
    async fn edge_to_edge_negotiation_writes_h264_and_opus() {
        for send_only in [true, false] {
            let (mut c, mut s) = (client().await, server().await);
            let (offer, pending) = c.create_offer(true, true, send_only).unwrap();
            assert_h264_and_opus_only(&offer);
            let answer = s.accept_offer(&offer).unwrap();
            assert_answer_keeps_offered_pts(&offer, &answer);
            c.apply_answer(&answer, pending).unwrap();
            let answered = sections(&answer);
            let (v, a) = (section(&answered, "video"), section(&answered, "audio"));
            let sender = if send_only { &mut c } else { &mut s };
            let video_pt = *sender.get_pt(Mid::from(v.mid.as_str())).unwrap();
            assert_eq!(v.codec[&video_pt], "H264", "video written on PT {video_pt}");
            assert_eq!(fmtp_param(&v.fmtp[&video_pt], "packetization-mode"), Some("1"));
            let audio_pt = *sender.get_pt(Mid::from(a.mid.as_str())).unwrap();
            assert_eq!(a.codec[&audio_pt], "OPUS", "audio written on PT {audio_pt}");
        }
    }

    /// A viewer that accepts only packetization-mode=0 H.264 still gets
    /// video, on its mode-0 PT, rather than none.
    #[tokio::test]
    async fn a_mode_0_only_viewer_is_written_on_its_mode_0_pt() {
        accept(CHROME_WHEP_MODE_0_OFFER, 104).await;
    }

    /// The codec set through edge 0.114.0: str0m's defaults plus four level-5.1
    /// H.264 entries, the first with its RTX on Opus's PT 111.
    fn legacy_rtc(ice_lite: bool) -> Rtc {
        let mut config = Rtc::builder().set_ice_lite(ice_lite);
        let codecs = config.codec_config();
        codecs.add_h264(110.into(), Some(111.into()), true, 0x42_00_33);
        codecs.add_h264(112.into(), Some(113.into()), true, 0x42_e0_33);
        codecs.add_h264(116.into(), Some(117.into()), true, 0x4d_00_33);
        codecs.add_h264(118.into(), Some(122.into()), true, 0x64_00_33);
        config.build(Instant::now())
    }

    fn negotiation_panic(err: &anyhow::Error) -> &NegotiationPanic {
        err.downcast_ref::<NegotiationPanic>().unwrap_or_else(|| panic!("not a str0m panic: {err}"))
    }

    /// A str0m panic left with this codec set — an offer, or an answer to our
    /// offer, whose one RTX PT repairs two H.264 PTs — is that peer's error.
    #[tokio::test]
    async fn a_str0m_panic_in_negotiation_is_an_error() {
        let err = server().await.accept_offer(RTX_REPAIRING_TWO_PTS).unwrap_err();
        let panic = negotiation_panic(&err);
        assert_eq!(panic.step, "SDP offer");
        assert!(panic.message.contains("Pt locked multiple times"), "{panic}");

        // The same shape as a server's answer to a WHIP client's offer, on the
        // two Baseline PTs that offer carries.
        let mut c = client().await;
        let (offer, pending) = c.create_offer(true, false, true).unwrap();
        let mid = c.video_mid.unwrap();
        let answer = RTX_REPAIRING_TWO_PTS
            .replace("a=group:BUNDLE 0", &format!("a=group:BUNDLE {mid}\r\na=ice-lite"))
            .replace("a=mid:0", &format!("a=mid:{mid}"))
            .replace("a=setup:actpass", "a=setup:passive")
            .replace(" 102 104 103\r\n", " 127 125 121\r\n")
            .replace(":102 ", ":127 ")
            .replace(":104 ", ":125 ")
            .replace(":103 ", ":121 ")
            .replace("apt=102", "apt=127")
            .replace("apt=104", "apt=125");
        assert!(offer.contains("a=rtpmap:127 H264/90000") && offer.contains("a=rtpmap:125 H264/90000"));
        let err = c.apply_answer(&answer, pending).unwrap_err();
        assert_eq!(negotiation_panic(&err).step, "SDP answer");
    }

    /// The panic that shipped — the old codec set against the Chrome WHEP
    /// offer, and against a single-PT answer — is caught the same way, and
    /// raises one Warning event. A plain bad SDP raises none.
    #[tokio::test]
    async fn a_str0m_panic_fails_only_the_peer_that_hit_it() {
        let (events, mut raised) = crate::manager::events::event_channel();

        let mut s = server().await;
        s.rtc = legacy_rtc(true);
        let err = s.accept_offer(CHROME_WHEP_OFFER).unwrap_err();
        let panic = negotiation_panic(&err);
        assert_eq!(panic.step, "SDP offer");
        assert!(panic.message.contains("Pt locked multiple times"), "{panic}");

        report_negotiation_panic(&err, &events, "flow-n", "WHEP viewer");
        let ev = raised.try_recv().expect("a Warning event");
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(ev.category, category::WEBRTC);
        assert_eq!(ev.flow_id.as_deref(), Some("flow-n"));
        assert!(ev.message.starts_with("WebRTC negotiation with WHEP viewer failed: str0m panicked"), "{}", ev.message);
        let details = ev.details.unwrap();
        assert_eq!(details["error_code"], "webrtc_negotiation_panic");
        assert_eq!(details["peer"], "WHEP viewer");
        assert_eq!(details["step"], "SDP offer");
        assert!(details["panic"].as_str().unwrap().contains("Pt locked multiple times"));

        let mut c = client().await;
        c.rtc = legacy_rtc(false);
        let (offer, pending) = c.create_offer(true, true, true).unwrap();
        let (video_mid, audio_mid) = (c.video_mid.unwrap(), c.audio_mid.unwrap());
        let answer = single_pt_answer(&offer, video_mid, audio_mid, (127, 121, "42001f"));
        let err = c.apply_answer(&answer, pending).unwrap_err();
        assert_eq!(negotiation_panic(&err).step, "SDP answer");

        // The next peer is negotiated on a fresh session, as every loop does.
        accept(CHROME_WHEP_OFFER, 102).await;

        let err = server().await.accept_offer("v=0\r\nnot an offer\r\n").unwrap_err();
        assert!(err.downcast_ref::<NegotiationPanic>().is_none(), "{err}");
        report_negotiation_panic(&err, &events, "flow-n", "WHEP viewer");
        assert!(raised.try_recv().is_err(), "a malformed offer raised an event");
    }

    #[test]
    fn isolate_negotiation_reads_either_panic_payload() {
        let err = isolate_negotiation::<()>("SDP offer", || panic!("formatted {}", 7)).unwrap_err();
        assert_eq!(negotiation_panic(&err).message, "formatted 7");
        let err = isolate_negotiation::<()>("SDP answer", || panic!("static")).unwrap_err();
        assert_eq!(err.to_string(), "str0m panicked during SDP answer: static");
        assert_eq!(isolate_negotiation("SDP offer", || Ok(5)).unwrap(), 5);
    }

    /// The H.264 table is str0m's own built-in one with only the level raised:
    /// same PTs, RTX PTs, packetization modes and profiles, in the same order.
    /// A str0m bump that changes its table fails here, so the edge and the
    /// relay are re-checked together.
    #[test]
    fn the_h264_set_is_str0ms_built_in_one_at_level_5_1() {
        let key = |p: &PayloadParams| {
            let spec = p.spec();
            (*p.pt(), p.resend().map(|r| *r), spec.format.packetization_mode, spec.format.profile_level_id)
        };
        let builtin: Vec<_> = str0m::format::CodecConfig::new_with_defaults()
            .params()
            .iter()
            .filter(|p| p.spec().codec == Codec::H264)
            .map(key)
            .collect();
        let ours: Vec<_> = H264_LEVEL_5_1
            .iter()
            .map(|&(pt, rtx, mode_1, plid)| (pt, Some(rtx), Some(mode_1 as u8), Some(plid)))
            .collect();
        assert_eq!(ours.len(), builtin.len());
        for (o, b) in ours.iter().zip(&builtin) {
            assert_eq!(o.3.unwrap() & 0xff, 0x33, "{o:?} is not level 5.1");
            assert_eq!((o.0, o.1, o.2, o.3.map(|l| l >> 8)), (b.0, b.1, b.2, b.3.map(|l| l >> 8)));
        }

        // ... and it is the whole set, beside Opus on its own PT.
        let mut config = rtc_config(true);
        let all: Vec<_> = config.codec_config().params().iter().map(|p| (p.spec().codec, key(p))).collect();
        assert_eq!(all.len(), 8);
        assert_eq!(all[0], (Codec::Opus, (111, None, None, None)));
        for (codec, k) in &all[1..] {
            assert_eq!(*codec, Codec::H264);
            assert!(ours.contains(k), "{k:?}");
        }
    }
}
