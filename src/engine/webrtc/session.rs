// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WebRTC session wrapper around str0m.
//!
//! Manages the lifecycle of a single WebRTC PeerConnection: ICE, DTLS,
//! SRTP, and media I/O. Integrates str0m's sans-I/O model with tokio
//! by driving the UDP socket and str0m poll loop in a select! loop.

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::Result;
use str0m::change::SdpOffer;
use str0m::media::{Direction, MediaKind, MediaTime, Mid, Pt};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use str0m::net::Protocol;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

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
        data: Vec<u8>,
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

        // str0m 0.18 ships H.264 profiles all clamped to level 3.1 (0x1f).
        // ffmpeg's WHIP muxer offers H.264 at higher levels (typically
        // 4.0 / 0x28 for 1080p sources), and `match_h264_score` rejects
        // any offered level higher than the local config's level. Result:
        // ICE+DTLS complete, but the SDP answer drops all video PTs and
        // the depayloader silently discards every RTP packet.
        //
        // Workaround: register additional H.264 entries with level 5.1
        // (0x33) so the level check passes for any 1080p/4K source. The
        // PTs we choose must NOT collide with str0m's built-in defaults
        // (which already occupy 35, 36, 45, 46, 96–103, 107–109, 114–115,
        // 119–125, 127). Available dynamic PTs: 110–113, 116–118, 122,
        // 126. We pick 110/111, 112/113, 116/117, 118/122 — duplicate
        // PTs in the m-line produce SDP that even str0m's own parser
        // rejects (Scenario L: edge → edge WHIP failed at SDP parse).
        //
        // Whenever str0m bumps its built-in H.264 levels (or adds an
        // ergonomic API to set them), retire this block.
        let mut rtc_builder = Rtc::builder().set_ice_lite(config.ice_lite);
        let codec_config = rtc_builder.codec_config();
        codec_config.add_h264(
            Pt::new_with_value(110),
            Some(Pt::new_with_value(111)),
            true,        // packetization-mode=1
            0x42_00_33,  // Baseline profile, level 5.1
        );
        codec_config.add_h264(
            Pt::new_with_value(112),
            Some(Pt::new_with_value(113)),
            true,
            0x42_e0_33,  // Constrained Baseline, level 5.1
        );
        codec_config.add_h264(
            Pt::new_with_value(116),
            Some(Pt::new_with_value(117)),
            true,
            0x4d_00_33,  // Main profile, level 5.1
        );
        codec_config.add_h264(
            Pt::new_with_value(118),
            Some(Pt::new_with_value(122)),
            true,
            0x64_00_33,  // High profile, level 5.1
        );
        let mut rtc = rtc_builder.build(Instant::now());

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

        for ip in &candidate_ips {
            let cand_addr = SocketAddr::new(*ip, port);
            let cand = Candidate::host(cand_addr, Protocol::Udp)
                .map_err(|e| anyhow::anyhow!("ICE candidate error: {}", e))?;
            rtc.add_local_candidate(cand);
            tracing::debug!("WebRTC: added local ICE host candidate {cand_addr}");
        }

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
    pub fn accept_offer(&mut self, offer_sdp: &str) -> Result<String> {
        // str0m 0.18's SDP parser hard-codes the session name field to a
        // single dash (`s=-`) and rejects every other session name. ffmpeg
        // and a number of other production WHIP publishers send a real
        // session name (e.g. `s=FFmpegPublishSession`), which is RFC 4566
        // legal but trips str0m. We normalise the offer here before parsing
        // so the rest of the pipeline doesn't have to know about the quirk.
        let normalised = normalise_sdp_offer_for_str0m(offer_sdp);

        let offer = SdpOffer::from_sdp_string(&normalised)
            .map_err(|e| anyhow::anyhow!("SDP parse error: {}", e))?;

        tracing::info!("SDP offer (normalised):\n{}", normalised);

        let answer = self.rtc.sdp_api().accept_offer(offer)
            .map_err(|e| anyhow::anyhow!("SDP accept error: {}", e))?;

        let answer_sdp = answer.to_sdp_string();
        tracing::info!("SDP answer:\n{}", answer_sdp);

        // MIDs will be discovered via MediaAdded events
        Ok(answer_sdp)
    }

    /// Create an SDP offer (client mode). Returns the SDP offer string.
    /// The pending offer must be kept and passed to `apply_answer()`.
    pub fn create_offer(&mut self, video: bool, audio: bool, send_only: bool) -> Result<(String, str0m::change::SdpPendingOffer)> {
        let mut api = self.rtc.sdp_api();
        let direction = if send_only { Direction::SendOnly } else { Direction::RecvOnly };

        if video {
            let mid = api.add_media(MediaKind::Video, direction, None, None, None);
            self.video_mid = Some(mid);
        }
        if audio {
            let mid = api.add_media(MediaKind::Audio, direction, None, None, None);
            self.audio_mid = Some(mid);
        }

        let (offer, pending) = api.apply()
            .ok_or_else(|| anyhow::anyhow!("No SDP changes to apply"))?;

        let offer_sdp = offer.to_sdp_string();
        tracing::info!("SDP offer (created):\n{}", offer_sdp);
        Ok((offer_sdp, pending))
    }

    /// Apply an SDP answer received from the remote peer (client mode).
    /// Requires the pending offer from `create_offer()`.
    pub fn apply_answer(&mut self, answer_sdp: &str, pending: str0m::change::SdpPendingOffer) -> Result<()> {
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
    /// calls — str0m queues writes in `to_payload` (cap 100) and only
    /// drains them via `handle_timeout`, which is reached from a
    /// `Output::Timeout` poll cycle. Without this drain, the inner H.264
    /// fragmentation loop overflows the queue after 100 writes and every
    /// subsequent `write_media` returns `Err("Consecutive calls to write()
    /// without poll_output() in between")`. We feed an `Input::Timeout`
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

    /// Get the first negotiated payload type for a given MID.
    pub fn get_pt(&mut self, mid: Mid) -> Option<Pt> {
        let writer = self.rtc.writer(mid)?;
        writer.payload_params().next().map(|p| p.pt())
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
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Event(event)) => {
                    let _ = self.handle_event(event);
                }
                Ok(Output::Transmit(_)) | Ok(Output::Timeout(_)) | Err(_) => break,
            }
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
                let kind = if let Some(media) = self.rtc.media(added.mid) {
                    media.kind()
                } else {
                    return None;
                };
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
    if let Some(lan) = route_discovered_lan_ip() {
        if !out.contains(&lan) {
            out.push(lan);
        }
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
