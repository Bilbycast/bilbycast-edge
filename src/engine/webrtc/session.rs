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
use std::time::{Duration, Instant};

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

/// How long ICE may stay `Disconnected` on an ICE-Lite session before
/// [`WebrtcSession::is_disconnected`] counts it over.
///
/// str0m's ICE-Lite agent (the WHEP output's role) reports `Disconnected`
/// 15 s after the peer's last STUN check (`is`'s `RECENT_BINDING_REQUEST`),
/// and recovers on the peer's next nominating check: it re-creates the pair
/// and goes back to `Completed`. So a departed peer is let go 15 s + this
/// grace after its last check (about 30 s), and one whose checks resume
/// within it is kept: a viewer whose browser was suspended or backgrounded
/// (measured up to 20 s), or whose network came back before the browser gave
/// up. Chrome fails a connection about 15 s into unanswered checks and never
/// recovers it (the WHEP flows do no ICE restart), so a longer network loss
/// is not survived either way.
///
/// The grace is not free on its own: str0m keeps the last nominated address
/// and goes on sending to it whatever the ICE state. So nothing is written
/// while ICE is down ([`WebrtcSession::write_media`] drops it), and a
/// departed viewer costs no bandwidth past ICE's 15 s.
///
/// A full-ICE session (the WHIP output) gets no grace: its agent reports
/// `Disconnected` only once its consent retransmits run out, about 24 s after
/// the last answered check, by when an ICE-Lite endpoint (the relay, an
/// edge's WHIP input) has pruned the pair and will never send the checks that
/// would make a new one. There is nothing left to wait for.
pub const ICE_DISCONNECT_GRACE: Duration = Duration::from_secs(15);

/// How long a client mode's session (the WHIP output's, the WHEP input's)
/// must stay connected before its reconnect backoff starts over from 1 s.
/// One that ends sooner — an endpoint that accepts each session and then
/// closes it — keeps doubling to 30 s like any other failure, rather than
/// being reconnected to about once a second for good.
pub const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(10);

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

/// A peer's offer this end cannot answer: it does not parse, str0m refused
/// it, or it carries nothing this end can send. The fault is the offer's,
/// so the edge's own WHIP / WHEP endpoint answers 400 (`api::webrtc`), not
/// 500.
#[derive(Debug)]
pub struct OfferRefused(pub String);

impl std::fmt::Display for OfferRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OfferRefused {}

/// Whether a failed server-side negotiation was the peer's offer's fault: an
/// [`OfferRefused`], or a str0m panic while accepting the offer (str0m keeps
/// asserts on paths only the peer's SDP reaches). Anything else — a socket
/// that would not bind, an input or output task gone — is this end's.
pub fn offer_was_at_fault(err: &anyhow::Error) -> bool {
    err.downcast_ref::<OfferRefused>().is_some()
        || err.downcast_ref::<NegotiationPanic>().is_some_and(|p| p.step == "SDP offer")
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
    /// Session has been disconnected or failed: ICE reported `Disconnected`
    /// (which an ICE-Lite agent can recover from — see
    /// [`WebrtcSession::is_disconnected`]), the peer closed DTLS, or the
    /// socket or str0m failed.
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
    /// Latched on what ends a session for good, on whichever path drained
    /// it: the peer closed DTLS (`Event::Closed`), the socket failed, or
    /// str0m returned an error. See [`Self::is_disconnected`].
    disconnected: bool,
    /// When ICE last went `Disconnected` and has not recovered since — a
    /// state an ICE-Lite agent leaves on the peer's next nomination, so it is
    /// given [`ICE_DISCONNECT_GRACE`] rather than latched.
    ice_disconnected_since: Option<Instant>,
    /// This end is an ICE-Lite agent (`SessionConfig::ice_lite`): only such
    /// a session is given [`ICE_DISCONNECT_GRACE`].
    ice_lite: bool,
    /// The peer asked for a keyframe (PLI / FIR) since the last
    /// [`Self::take_keyframe_request`]. Kept, not only returned, because
    /// `drain_outputs` returns no events and `drive_udp_io` only a batch's
    /// first.
    keyframe_requested: bool,
    buf: Vec<u8>,
}

impl WebrtcSession {
    /// Create a new session with ICE-lite and bind a UDP socket.
    pub async fn new(config: &SessionConfig) -> Result<Self> {
        Self::with_rtc_config(config, rtc_config(config.ice_lite)).await
    }

    /// [`Self::new`] on another `Rtc` configuration — a test's peer with
    /// another codec set (a browser without H.264, say).
    pub(crate) async fn with_rtc_config(config: &SessionConfig, rtc_cfg: RtcConfig) -> Result<Self> {
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
            let mut rtc = rtc_cfg.build(Instant::now());
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
            disconnected: false,
            ice_disconnected_since: None,
            keyframe_requested: false,
            ice_lite: config.ice_lite,
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
            // The parser's own error stays in the log: it is several lines of
            // parser internals and names a heap address of this process
            // (`PointerOffset(0x…)`), and the refusal goes back to whoever
            // POSTed the offer — on a WHEP output, often anyone at all.
            let offer = SdpOffer::from_sdp_string(&normalised).map_err(|e| {
                tracing::warn!("WebRTC: the offer is not valid SDP: {e}");
                OfferRefused("SDP parse error: the offer is not valid SDP".into())
            })?;

            tracing::info!("SDP offer (normalised):\n{}", normalised);

            let answer = self.rtc.sdp_api().accept_offer(offer)
                .map_err(|e| OfferRefused(format!("SDP accept error: {e}")))?;
            Ok(fill_rejected_formats(&answer.to_sdp_string()))
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
        let kinds: Vec<MediaKind> = [(video, MediaKind::Video), (audio, MediaKind::Audio)]
            .into_iter()
            .filter_map(|(wanted, kind)| wanted.then_some(kind))
            .collect();
        self.offer_media(&kinds, send_only)
    }

    /// [`Self::create_offer`] for the m-lines `kinds`, in that order — the
    /// first one is the offer's BUNDLE tag. The edge's own offers put video
    /// first; a test stands in for a peer that puts audio first.
    pub(crate) fn offer_media(&mut self, kinds: &[MediaKind], send_only: bool) -> Result<(String, str0m::change::SdpPendingOffer)> {
        let direction = if send_only { Direction::SendOnly } else { Direction::RecvOnly };

        let (offer, pending) = isolate_negotiation("SDP offer creation", || {
            let mut api = self.rtc.sdp_api();

            for &kind in kinds {
                let mid = api.add_media(kind, direction, None, None, None);
                match kind {
                    MediaKind::Video => self.video_mid = Some(mid),
                    MediaKind::Audio => self.audio_mid = Some(mid),
                }
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

    /// Write media data to a track — or, while ICE is down
    /// ([`Self::ice_down`]), drop it: str0m keeps the last nominated address
    /// and sends to it whatever the ICE state, so a departed peer would be
    /// sent its full bitrate for the whole grace.
    ///
    /// It is dropped here, not by the send loops, so that they go on
    /// demuxing, decoding and encoding while ICE is down. An encoder's
    /// timeline must stay continuous: one stopped for the outage resumed
    /// where it left off (an Opus encoder runs on from its anchor, so its
    /// audio was late by the whole outage for the rest of the session — 3.8 s
    /// measured), and a video decoder resumed mid-GOP on references it never
    /// decoded. A peer that comes back is sent the frame being encoded now,
    /// on the RTP clock it would have had all along.
    pub fn write_media(
        &mut self,
        mid: Mid,
        pt: Pt,
        wallclock: Instant,
        rtp_time: MediaTime,
        data: &[u8],
    ) -> Result<()> {
        if self.ice_down() {
            return Ok(());
        }
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
    ///
    /// Events drained here are not returned, but what ends the session is
    /// kept (see [`Self::is_disconnected`]): str0m reports ICE giving up from
    /// inside the very timeout this runs, so for a viewer that left without
    /// a DELETE this is where it usually surfaces — and it used to be thrown
    /// away here.
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
                Ok(Output::Timeout(_)) => break,
                Err(e) => {
                    self.failed(&e);
                    break;
                }
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

    /// The payload types `answer` — the answer [`Self::accept_offer`] just
    /// gave — settled for this end's video and audio: `(H.264, Opus)`, each
    /// `None` when the offer carried no such m-line or the peer accepted
    /// neither codec on it (str0m answers such an m-line on port 0).
    ///
    /// Read straight off the answer's mids, before any I/O: the tracks only
    /// reach `video_mid` / `audio_mid` through `MediaAdded` events, which
    /// would mean polling str0m before ICE.
    pub fn answered_pts(&mut self, answer: &str) -> (Option<Pt>, Option<Pt>) {
        let (mut video, mut audio) = (None, None);
        for mid in answer.lines().filter_map(|l| l.trim_end().strip_prefix("a=mid:")) {
            let mid = Mid::from(mid);
            let Some(kind) = self.rtc.media(mid).map(|m| m.kind()) else {
                continue;
            };
            let pt = self.get_pt(mid);
            match kind {
                MediaKind::Video => video = video.or(pt),
                MediaKind::Audio => audio = audio.or(pt),
            }
        }
        (video, audio)
    }

    /// The kind of the m-line `mid` names — one of an offer this session
    /// answered, say — read before any I/O, as [`Self::answered_pts`] reads.
    pub fn media_kind(&self, mid: &str) -> Option<MediaKind> {
        self.rtc.media(Mid::from(mid)).map(|m| m.kind())
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
                Err(e) => {
                    self.failed(&e);
                    break;
                }
                Ok(_) => break,
            }
        }
    }

    /// Check if the session is still alive.
    /// Retained for future session health monitoring.
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// The session is over, and a send loop driving it with
    /// [`Self::drain_outputs`] / [`Self::drive_udp_io`] — the WHEP viewer's,
    /// the WHIP output's — must let it go: the peer closed DTLS, the socket
    /// or str0m failed, the `Rtc` is closed, or ICE has been `Disconnected`
    /// for [`ICE_DISCONNECT_GRACE`] without recovering. Check it after each
    /// of those calls, and on an idle tick when nothing is being sent.
    ///
    /// Those calls drop most events — `drain_outputs` returns none and
    /// `drive_udp_io` only the first of a batch — so a send loop that waited
    /// for a `Disconnected` from them never saw one: a WHEP viewer that left
    /// without a DELETE was sent the stream until the flow stopped. Every
    /// event goes through [`Self::handle_event`], which keeps what this
    /// needs, so nothing is lost however it was drained.
    ///
    /// ICE `Disconnected` alone is not the end of an ICE-Lite session. Its
    /// agent recovers on the peer's next nominating check, so a viewer whose
    /// checks paused for 15 s (`is` prunes the pair then) is kept if it comes
    /// back within the grace. A full-ICE session (the WHIP output) ends at
    /// its first `Disconnected` (see [`ICE_DISCONNECT_GRACE`]).
    /// [`Self::poll_event`] returns `Disconnected` at once either way, as it
    /// always has: its callers (the WHIP and WHEP inputs, the setup waits)
    /// end there.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected
            || !self.rtc.is_alive()
            || self
                .ice_disconnected_since
                .is_some_and(|since| since.elapsed() >= ICE_DISCONNECT_GRACE)
    }

    /// ICE is `Disconnected` and has not recovered — the peer has gone
    /// quiet, and an ICE-Lite session's grace is running. Nothing is written
    /// meanwhile ([`Self::write_media`] drops it; str0m would send it to the
    /// last nominated address all the same), and the send loops count
    /// nothing as sent; they keep encoding and driving the session, so a
    /// peer that comes back is heard and sent the stream again, on its
    /// timeline.
    pub fn ice_down(&self) -> bool {
        self.ice_disconnected_since.is_some()
    }

    /// Whether the peer has asked for a keyframe (PLI / FIR) since the last
    /// call, from whichever drain read it: a viewer that lost packets, or
    /// one back from an outage, whose decoder waits for an IDR.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.keyframe_requested)
    }

    /// Latch a str0m error: the session is over (see [`Self::is_disconnected`]).
    fn failed(&mut self, e: &str0m::RtcError) {
        if !self.disconnected {
            tracing::warn!("WebRTC: str0m error, ending the session: {e}");
        }
        self.disconnected = true;
    }

    /// Start an orderly close — RTCP BYE, DTLS `close_notify` — and send what
    /// it queued. A test's peer closing the way a browser's `pc.close()`
    /// does.
    #[cfg(test)]
    pub(crate) async fn close(&mut self) {
        let _ = self.rtc.close();
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let _ = self.socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(_)) => {}
                Ok(Output::Timeout(_)) | Err(_) => break,
            }
        }
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
                    // A closed `Rtc` is inert: it answers every poll with a
                    // timeout that never comes, and nothing it is handed
                    // changes that. `Event::Closed` already ended the
                    // session; this is the backstop for any other way in.
                    if !self.rtc.is_alive() {
                        self.disconnected = true;
                        return SessionEvent::Disconnected;
                    }
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
                                    self.disconnected = true;
                                    return SessionEvent::Disconnected;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("str0m error: {}", e);
                    self.disconnected = true;
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
    /// meaningful session event (if any) discovered while processing; what
    /// ends the session is kept whether or not it was first, so a send loop
    /// checks [`Self::is_disconnected`] rather than the return value.
    ///
    /// Designed for the WHIP client output and WHEP viewer send loops,
    /// which need to keep str0m alive (RTCP, STUN keepalives) while
    /// they are primarily driven by the broadcast channel. Call this
    /// after writing media and after each broadcast packet batch, and on an
    /// idle tick, so ICE's timeouts run while the source is stalled.
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
                Err(e) => {
                    // As in `poll_event`: the socket failed.
                    if !self.disconnected {
                        tracing::error!("UDP recv error: {e}");
                    }
                    self.disconnected = true;
                    break;
                }
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
                    // Every event goes through `handle_event`, which keeps
                    // what `is_disconnected` needs; only the first is
                    // returned.
                    let event = self.handle_event(ev);
                    if event_out.is_none() {
                        event_out = event;
                    }
                }
                Ok(Output::Timeout(_)) => break,
                Err(e) => {
                    self.failed(&e);
                    break;
                }
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
                    IceConnectionState::Disconnected => {
                        // Kept here, the one place every drain goes through,
                        // and timed rather than latched on an ICE-Lite
                        // session: its agent recovers on the peer's next
                        // nomination (see `is_disconnected`). A full agent's
                        // Disconnected is the end (`ICE_DISCONNECT_GRACE`).
                        self.ice_disconnected_since.get_or_insert_with(Instant::now);
                        if !self.ice_lite {
                            self.disconnected = true;
                        }
                        Some(SessionEvent::Disconnected)
                    }
                    _ => {
                        self.ice_disconnected_since = None;
                        Some(SessionEvent::IceStateChange(state))
                    }
                }
            }
            Event::Closed => {
                // The peer closed DTLS (a browser's `pc.close()`), or SCTP
                // lost its association: str0m sends its own close and then
                // goes inert — no ICE timeout or Disconnected ever follows.
                // It used to be dropped, and a closed viewer was waited on
                // for good.
                tracing::info!("WebRTC: the peer closed the session (DTLS close_notify)");
                self.disconnected = true;
                Some(SessionEvent::Disconnected)
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
                self.keyframe_requested = true;
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

/// Give every m-line str0m's `answer` rejects a format to carry.
///
/// str0m answers an m-line it shares no codec on — VP8-only video, say — on
/// port 0 with an **empty** format list (`m=video 0 UDP/TLS/RTP/SAVPF `). RFC
/// 3264 §6 lets a rejected stream list any formats, which are ignored, but
/// requires at least one, and str0m's own parser refuses the line ("Expected
/// at least one PT"): a peer could not apply the answer at all — not even the
/// m-lines it did accept (a viewer's Opus, when its video was refused). Such
/// a line takes format `0`, as Pion writes a rejected m-line: a static payload
/// type (RFC 3551), so no parser looks for an `a=rtpmap` to go with it (str0m
/// does, for a dynamic one). Every other line passes unchanged.
fn fill_rejected_formats(answer: &str) -> String {
    let mut out = String::with_capacity(answer.len() + 8);
    for raw_line in answer.split_inclusive('\n') {
        let line_no_eol = raw_line.trim_end_matches(['\r', '\n']);
        let eol = &raw_line[line_no_eol.len()..];
        if let Some(m) = line_no_eol.strip_prefix("m=")
            && let [media, "0", proto] = m.split_whitespace().collect::<Vec<_>>().as_slice()
        {
            out.push_str(&format!("m={media} 0 {proto} 0"));
            out.push_str(eol);
            continue;
        }
        out.push_str(raw_line);
    }
    out
}

/// The mid of the m-line that `answer` — our answer to `offer` — rejects
/// (port 0) although the offer made it the tag of its BUNDLE group (the first
/// mid of its `a=group:BUNDLE`), or `None`.
///
/// RFC 8843 §7.3.3: the answerer cannot reject the offerer-tagged m= section.
/// The tag carries the group's transport, so a browser cannot apply such an
/// answer at all (Chrome 124: "Failed to setup RTCP mux"; with
/// `bundlePolicy: 'balanced'` it applies, but the only candidate sits in the
/// rejected section and ICE never starts) — not even for the m-lines it
/// accepted. str0m writes it anyway, and re-tags its own BUNDLE line to the
/// first accepted mid; a str0m peer applies it. Browsers put video first, so
/// a viewer or publisher offering no H.264 hits this unless its audio leads.
///
/// The tag is read from the offer as `accept_offer` gave it to str0m
/// (phantom mids dropped, see `normalise_sdp_offer_for_str0m`). An offer with
/// no BUNDLE group has no tag, and nothing to break.
pub fn rejected_bundle_tag(offer: &str, answer: &str) -> Option<String> {
    let offer = normalise_sdp_offer_for_str0m(offer);
    let tag = offer
        .lines()
        .find_map(|l| l.trim_end().strip_prefix("a=group:BUNDLE "))?
        .split_whitespace()
        .next()?
        .to_string();
    let mut rejected = false;
    for line in answer.lines().map(str::trim_end) {
        if let Some(m) = line.strip_prefix("m=") {
            rejected = m.split_whitespace().nth(1) == Some("0");
        } else if line.strip_prefix("a=mid:") == Some(tag.as_str()) {
            return rejected.then_some(tag);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offer's BUNDLE tag rejected by the answer is found, by its mid —
    /// and only that: a rejected m-line further down the group, an offer
    /// without a group, a phantom tag the normaliser drops.
    #[test]
    fn a_rejected_bundle_tag_is_found() {
        let offer = |group: &str| {
            format!(
                "v=0\r\ns=-\r\nt=0 0\r\n{group}\
                 m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:v\r\n\
                 m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:a\r\n"
            )
        };
        let answer = |video_port: u16, audio_port: u16| {
            format!(
                "v=0\r\ns=-\r\nt=0 0\r\n\
                 m=video {video_port} UDP/TLS/RTP/SAVPF 0\r\na=mid:v\r\n\
                 m=audio {audio_port} UDP/TLS/RTP/SAVPF 111\r\na=mid:a\r\n"
            )
        };
        let bundled = offer("a=group:BUNDLE v a\r\n");
        assert_eq!(
            rejected_bundle_tag(&bundled, &answer(0, 9)).as_deref(),
            Some("v")
        );
        assert_eq!(
            rejected_bundle_tag(&bundled, &answer(9, 0)),
            None,
            "not the tag"
        );
        assert_eq!(rejected_bundle_tag(&bundled, &answer(9, 9)), None);
        let audio_tagged = offer("a=group:BUNDLE a v\r\n");
        assert_eq!(rejected_bundle_tag(&audio_tagged, &answer(0, 9)), None);
        assert_eq!(
            rejected_bundle_tag(&audio_tagged, &answer(9, 0)).as_deref(),
            Some("a")
        );
        assert_eq!(
            rejected_bundle_tag(&offer(""), &answer(0, 9)),
            None,
            "no BUNDLE group"
        );
        // ffmpeg's phantom first mid is not the tag str0m was given.
        let phantom = offer("a=group:BUNDLE x a v\r\n");
        assert_eq!(rejected_bundle_tag(&phantom, &answer(0, 9)), None);
        assert_eq!(
            rejected_bundle_tag(&phantom, &answer(9, 0)).as_deref(),
            Some("a")
        );
    }

    /// A rejected m-line str0m left without a format takes format 0; an
    /// accepted one, or a rejected one that lists a format, is left as it is.
    #[test]
    fn a_rejected_m_line_gets_a_format() {
        let answer = "v=0\r\nm=video 0 UDP/TLS/RTP/SAVPF \r\na=mid:0\r\n\
                      m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:1\r\n\
                      m=video 0 UDP/TLS/RTP/SAVPF 96\r\na=mid:2\r\n";
        assert_eq!(
            fill_rejected_formats(answer),
            "v=0\r\nm=video 0 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:1\r\n\
             m=video 0 UDP/TLS/RTP/SAVPF 96\r\na=mid:2\r\n"
        );
    }

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

    // ── ffmpeg 8.0.x ICE checks without PRIORITY (vendored `is`) ───────
    //
    // ffmpeg's WHIP muxer sends its connectivity checks without the PRIORITY
    // attribute in every 8.0.x release (8.1 added it, ffmpeg 7fd967c2c1). The
    // vendored `is` carries two hunks for that: one in `src/stun.rs` so the
    // parser accepts the request, one in `src/agent.rs` so the ICE agent does
    // not then `expect` the attribute and panic. These tests feed a request
    // built the way ffmpeg builds it through both.

    /// ffmpeg's ICE credentials in the offer below: its `ice_ufrag_local`
    /// (`%08x`) and `ice_pwd_local` (`%08x` four times), fixed here.
    const FFMPEG_UFRAG: &str = "5f3c2a91";
    const FFMPEG_PWD: &str = "0d9e8b7a6c5d4e3f2a1b0c9d8e7f6a5b";

    /// The offer ffmpeg n8.0.3's WHIP muxer sends (`generate_sdp_offer` in
    /// libavformat/whip.c) for Opus + H.264, with its random ufrag, pwd,
    /// SSRCs and DTLS fingerprint fixed. `profile-level-id=640028` (High@4.0)
    /// is what libx264 reports for a 1080p source.
    const FFMPEG_8_0_WHIP_OFFER: &str = "v=0\r\n\
        o=FFmpeg 4489045141692799359 2 IN IP4 127.0.0.1\r\n\
        s=FFmpegPublishSession\r\n\
        t=0 0\r\n\
        a=group:BUNDLE 0 1\r\n\
        a=extmap-allow-mixed\r\n\
        a=msid-semantic: WMS\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=ice-ufrag:5f3c2a91\r\n\
        a=ice-pwd:0d9e8b7a6c5d4e3f2a1b0c9d8e7f6a5b\r\n\
        a=fingerprint:sha-256 3A:6E:1F:90:C2:4B:7D:08:E5:A1:36:F9:2C:84:D0:5B:71:E3:0A:9F:46:B8:2D:C7:15:6A:F0:83:9E:24:BB:5D\r\n\
        a=setup:passive\r\n\
        a=mid:0\r\n\
        a=sendonly\r\n\
        a=msid:FFmpeg audio\r\n\
        a=rtcp-mux\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        a=ssrc:2846271538 cname:FFmpeg\r\n\
        a=ssrc:2846271538 msid:FFmpeg audio\r\n\
        m=video 9 UDP/TLS/RTP/SAVPF 106\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=ice-ufrag:5f3c2a91\r\n\
        a=ice-pwd:0d9e8b7a6c5d4e3f2a1b0c9d8e7f6a5b\r\n\
        a=fingerprint:sha-256 3A:6E:1F:90:C2:4B:7D:08:E5:A1:36:F9:2C:84:D0:5B:71:E3:0A:9F:46:B8:2D:C7:15:6A:F0:83:9E:24:BB:5D\r\n\
        a=setup:passive\r\n\
        a=mid:1\r\n\
        a=sendonly\r\n\
        a=msid:FFmpeg video\r\n\
        a=rtcp-mux\r\n\
        a=rtcp-rsize\r\n\
        a=rtpmap:106 H264/90000\r\n\
        a=fmtp:106 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640028\r\n\
        a=ssrc:2846271539 cname:FFmpeg\r\n\
        a=ssrc:2846271539 msid:FFmpeg video\r\n";

    /// The PRIORITY a PRIORITY-less request is given by the vendored `is`
    /// (src/agent.rs): RFC 8445 §5.1.2's formula with the peer-reflexive type
    /// preference 110, local preference 65535 and component ID 1.
    const PRFLX_PRIORITY_WITHOUT_ATTRIBUTE: u32 = (110 << 24) | (65_535 << 8) | (256 - 1);

    fn stun_sha1_hmac(key: &[u8], payloads: &[&[u8]]) -> [u8; 20] {
        str0m::crypto::from_feature_flags()
            .sha1_hmac_provider
            .sha1_hmac(key, payloads)
    }

    /// CRC-32/ISO-HDLC, what ffmpeg's `AV_CRC_32_IEEE_LE` computes for the
    /// STUN FINGERPRINT. Bitwise: a test helper, not a hot path.
    fn crc32_ieee(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// A STUN Binding Request laid out exactly as ffmpeg n8.0 to n8.0.3 lay it
    /// out (`ice_create_request` in libavformat/whip.c): USERNAME
    /// (`<server ufrag>:<ffmpeg ufrag>`), USE-CANDIDATE, MESSAGE-INTEGRITY
    /// keyed with the server's ice-pwd, FINGERPRINT. No PRIORITY and no
    /// ICE-CONTROLLING.
    fn ffmpeg_8_0_binding_request(
        trans_id: &[u8; 12],
        server_ufrag: &str,
        server_pwd: &str,
    ) -> Vec<u8> {
        fn set_length(buf: &mut [u8]) {
            let len = u16::try_from(buf.len() - 20).unwrap();
            buf[2..4].copy_from_slice(&len.to_be_bytes());
        }

        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&0x0001_u16.to_be_bytes()); // Binding request
        buf.extend_from_slice(&0_u16.to_be_bytes()); // length, set below
        buf.extend_from_slice(&0x2112_A442_u32.to_be_bytes()); // magic cookie
        buf.extend_from_slice(trans_id);

        let username = format!("{server_ufrag}:{FFMPEG_UFRAG}");
        buf.extend_from_slice(&0x0006_u16.to_be_bytes()); // USERNAME
        buf.extend_from_slice(&u16::try_from(username.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(username.as_bytes());
        buf.resize(buf.len() + (4 - username.len() % 4) % 4, 0);

        buf.extend_from_slice(&0x0025_u16.to_be_bytes()); // USE-CANDIDATE
        buf.extend_from_slice(&0_u16.to_be_bytes());

        // MESSAGE-INTEGRITY over everything before it, with the header length
        // already counting the attribute itself (RFC 5389 §15.4).
        buf.extend_from_slice(&0x0008_u16.to_be_bytes());
        buf.extend_from_slice(&20_u16.to_be_bytes());
        buf.extend_from_slice(&[0; 20]);
        set_length(&mut buf);
        let at = buf.len() - 20;
        let mac = stun_sha1_hmac(server_pwd.as_bytes(), &[&buf[..at - 4]]);
        buf[at..].copy_from_slice(&mac);

        // FINGERPRINT over everything before it, XOR "STUN".
        buf.extend_from_slice(&0x8028_u16.to_be_bytes());
        buf.extend_from_slice(&4_u16.to_be_bytes());
        buf.extend_from_slice(&[0; 4]);
        set_length(&mut buf);
        let at = buf.len() - 4;
        let crc = crc32_ieee(&buf[..at - 4]) ^ 0x5354_554E;
        buf[at..].copy_from_slice(&crc.to_be_bytes());
        buf
    }

    /// Pull one `a=<name>:` value out of an SDP.
    fn sdp_attribute(sdp: &str, name: &str) -> String {
        let prefix = format!("a={name}:");
        sdp.lines()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .unwrap_or_else(|| panic!("no a={name} in SDP:\n{sdp}"))
            .trim()
            .to_owned()
    }

    #[test]
    fn ffmpeg_8_0_binding_request_is_what_it_claims() {
        // The FINGERPRINT is never checked on receive, so pin the CRC against
        // the standard CRC-32 check value instead.
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);

        let request = ffmpeg_8_0_binding_request(b"ffmpeg-8.0.3", "srvu", "serverpasswordserverpw");
        let message = str0m::ice::StunMessage::parse(&request)
            .expect("the vendored `is` parser accepts a PRIORITY-less Binding Request");
        assert!(message.is_binding_request());
        assert_eq!(message.split_username(), Some(("srvu", FFMPEG_UFRAG)));
        assert!(message.use_candidate());
        assert_eq!(message.prio(), None);
        assert_eq!(message.ice_controlling(), None);
        assert_eq!(message.ice_controlled(), None);
        assert!(message.verify(b"serverpasswordserverpw", stun_sha1_hmac));
    }

    /// Regression: an authenticated ffmpeg 8.0.x check used to panic the
    /// ICE-lite agent at `message.prio().expect("STUN request prio")` — the
    /// second hunk of the original PRIORITY patch, lost in the `is` 0.9.0
    /// re-vendor. On the WHIP input that panic took down the input task.
    ///
    /// Drives the same `Rtc` the WHIP input builds (`WebrtcSession::new`,
    /// ICE-lite, controlled after `accept_offer`) and asserts the check is
    /// answered and ICE completes on it.
    #[tokio::test]
    async fn whip_ice_lite_answers_ffmpeg_8_0_check_without_priority() {
        let mut session = WebrtcSession::new(&SessionConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            public_ip: None,
            ice_lite: true,
        })
        .await
        .unwrap();
        // The request's USERNAME names ffmpeg by the ufrag its offer carries.
        assert!(FFMPEG_8_0_WHIP_OFFER.contains(&format!("a=ice-ufrag:{FFMPEG_UFRAG}\r\n")));
        let answer = session.accept_offer(FFMPEG_8_0_WHIP_OFFER).unwrap();
        let server_ufrag = sdp_attribute(&answer, "ice-ufrag");
        let server_pwd = sdp_attribute(&answer, "ice-pwd");
        // Drain whatever accepting the offer queued, so everything polled
        // below is the agent's response to the check.
        session.drain_pending_events();

        let request = ffmpeg_8_0_binding_request(b"ffmpeg-8.0.3", &server_ufrag, &server_pwd);
        let request_trans_id = str0m::ice::StunMessage::parse(&request).unwrap().trans_id();
        let ffmpeg_addr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let local_addr = session.local_addr;
        let receive = str0m::net::Receive {
            proto: Protocol::Udp,
            source: ffmpeg_addr,
            destination: local_addr,
            contents: request.as_slice().try_into().unwrap(),
        };
        let rtc = &mut session.rtc;
        let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rtc.handle_input(Input::Receive(Instant::now(), receive))
        }))
        .unwrap_or_else(|_| {
            panic!("str0m panicked on an ffmpeg 8.0.x Binding Request without PRIORITY")
        });
        handled.expect("str0m accepts the Binding Request");

        // Drain, then advance time once so the agent re-evaluates its state,
        // then drain again.
        let mut replies = Vec::new();
        let mut ice_states = Vec::new();
        let mut advanced = false;
        loop {
            match session.rtc.poll_output().expect("poll_output") {
                Output::Transmit(t) => {
                    // ffmpeg offered `setup:passive`, so once ICE is up we
                    // also start DTLS; only the STUN replies matter here.
                    if let Ok(m) = str0m::ice::StunMessage::parse(&t.contents)
                        && m.is_successful_binding_response()
                    {
                        assert_eq!(m.trans_id(), request_trans_id);
                        assert_eq!(t.source, local_addr);
                        assert_eq!(m.mapped_address(), Some(ffmpeg_addr));
                        assert!(m.verify(server_pwd.as_bytes(), stun_sha1_hmac));
                        replies.push(t.destination);
                    }
                }
                Output::Event(Event::IceConnectionStateChange(state)) => ice_states.push(state),
                Output::Event(_) => {}
                Output::Timeout(_) if !advanced => {
                    advanced = true;
                    session
                        .rtc
                        .handle_input(Input::Timeout(Instant::now()))
                        .unwrap();
                }
                Output::Timeout(_) => break,
            }
        }

        assert_eq!(
            replies,
            vec![ffmpeg_addr],
            "exactly one Binding Success response, to ffmpeg"
        );
        // An ICE-lite agent goes straight to Completed on the first
        // USE-CANDIDATE it answers.
        assert!(
            ice_states.contains(&IceConnectionState::Completed),
            "ICE should complete on ffmpeg's USE-CANDIDATE, saw {ice_states:?}"
        );
    }

    /// The PRIORITY the agent substitutes, pinned on the peer-reflexive
    /// candidate it learns from the request (RFC 8445 §7.3.1.3), with
    /// `str0m::ice::IceAgent` set up as `Rtc` sets it up for an ICE-lite
    /// answerer.
    #[test]
    fn ice_lite_gives_priority_less_check_a_prflx_priority() {
        let server = str0m::ice::IceCreds {
            ufrag: "srvu".into(),
            pass: "serverpasswordserverpw".into(),
        };
        let mut agent = str0m::ice::IceAgent::with_hmac(
            server.clone(),
            str0m::crypto::from_feature_flags().sha1_hmac_provider,
        );
        agent.set_ice_lite(true);
        agent.set_controlling(false);
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        agent.add_local_candidate(Candidate::host(local, Protocol::Udp).unwrap());
        agent.set_remote_credentials(str0m::ice::IceCreds {
            ufrag: FFMPEG_UFRAG.into(),
            pass: FFMPEG_PWD.into(),
        });

        let request = ffmpeg_8_0_binding_request(b"ffmpeg-8.0.3", &server.ufrag, &server.pass);
        let ffmpeg_addr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let packet = str0m::ice::StunPacket {
            proto: Protocol::Udp,
            source: ffmpeg_addr,
            destination: local,
            message: str0m::ice::StunMessage::parse(&request).unwrap(),
        };
        let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            agent.handle_packet(Instant::now(), packet)
        }))
        .unwrap_or_else(|_| panic!("IceAgent panicked on a Binding Request without PRIORITY"));
        assert!(handled);

        let prflx: Vec<_> = agent
            .remote_candidates()
            .filter(|c| c.addr() == ffmpeg_addr)
            .collect();
        assert_eq!(
            prflx.len(),
            1,
            "one peer-reflexive remote learnt from the request"
        );
        assert_eq!(prflx[0].kind(), str0m::CandidateKind::PeerReflexive);
        assert_eq!(prflx[0].prio(), PRFLX_PRIORITY_WITHOUT_ATTRIBUTE);
        assert_eq!(PRFLX_PRIORITY_WITHOUT_ATTRIBUTE, 1_862_270_975);
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

    /// A viewer offering VP8 for its video (a browser without H.264), with
    /// Opus or without audio at all: the offer it would POST.
    async fn vp8_viewer_offer(audio: bool) -> String {
        let bind_addr = "127.0.0.1:0".parse().unwrap();
        let vp8 = Rtc::builder().clear_codecs().enable_vp8(true).enable_opus(true, false);
        let mut c = WebrtcSession::with_rtc_config(&SessionConfig { bind_addr, public_ip: None, ice_lite: false }, vp8)
            .await
            .unwrap();
        c.create_offer(true, audio, false).unwrap().0
    }

    /// The PTs an answer settles are read straight off it, before any I/O
    /// (the tracks reach `video_mid` / `audio_mid` only through events): a
    /// Chrome viewer's H.264 and Opus; for a viewer offering VP8, no video PT
    /// — its video m-line answered on port 0 — and its Opus if it offered
    /// one.
    #[tokio::test]
    async fn the_answered_pts_are_read_off_the_answer() {
        let mut s = server().await;
        let answer = s.accept_offer(CHROME_WHEP_OFFER).unwrap();
        assert_eq!(s.answered_pts(&answer), (Some(Pt::new_with_value(102)), Some(Pt::new_with_value(111))));
        assert_eq!((s.video_mid, s.audio_mid), (None, None), "no event has been polled");

        let offer = vp8_viewer_offer(true).await;
        let mut s = server().await;
        let answer = s.accept_offer(&offer).unwrap();
        assert!(answer.contains("m=video 0 "), "{answer}");
        assert_eq!(s.answered_pts(&answer), (None, Some(Pt::new_with_value(111))), "{answer}");

        let offer = vp8_viewer_offer(false).await;
        let mut s = server().await;
        let answer = s.accept_offer(&offer).unwrap();
        assert_eq!(s.answered_pts(&answer), (None, None), "{answer}");
    }

    /// An offer that does not parse, or that str0m panics on, is the offer's
    /// fault — the endpoint answers it 400. A panic setting a session up, or
    /// an error of any other kind, is this end's: 500.
    #[tokio::test]
    async fn only_the_offers_own_faults_are_laid_at_it() {
        let garbage = server().await.accept_offer("v=0\r\nnot an offer\r\n").unwrap_err();
        assert!(garbage.to_string().starts_with("SDP parse error: "), "{garbage}");
        assert!(offer_was_at_fault(&garbage), "{garbage}");
        let panicked = server().await.accept_offer(RTX_REPAIRING_TWO_PTS).unwrap_err();
        assert!(offer_was_at_fault(&panicked), "{panicked}");
        assert!(offer_was_at_fault(&OfferRefused("nothing to send".into()).into()));

        let setup = isolate_negotiation::<()>("session setup", || panic!("no socket")).unwrap_err();
        assert!(!offer_was_at_fault(&setup));
        assert!(!offer_was_at_fault(&anyhow::anyhow!("WHEP output task dropped reply")));
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

/// What ends a session, and what does not, as the send loops judge it
/// (`is_disconnected`).
#[cfg(test)]
mod liveness_tests {
    use super::*;
    use std::time::Duration;

    async fn session(ice_lite: bool) -> WebrtcSession {
        let bind_addr = "127.0.0.1:0".parse().unwrap();
        WebrtcSession::new(&SessionConfig {
            bind_addr,
            public_ip: None,
            ice_lite,
        })
        .await
        .unwrap()
    }

    /// Every way the session ends is kept by `handle_event`, through which
    /// every drain passes its events — the drains themselves drop them.
    /// ICE `Disconnected` is the end only once it has lasted the grace, and
    /// a recovery clears it; the peer closing DTLS, or a closed `Rtc`, is the
    /// end at once.
    #[tokio::test]
    async fn what_ends_a_session_is_kept_however_it_was_drained() {
        let ice = |state| Event::IceConnectionStateChange(state);
        let mut s = session(true).await;
        assert!(!s.is_disconnected());
        let _ = s.handle_event(ice(IceConnectionState::Checking));
        assert!(!s.is_disconnected(), "not every state change is the end");

        let _ = s.handle_event(ice(IceConnectionState::Disconnected));
        assert!(
            !s.is_disconnected(),
            "an ICE-Lite agent recovers from Disconnected"
        );
        // A second report does not restart the clock; the grace running out
        // is the end.
        let since = Instant::now() - ICE_DISCONNECT_GRACE + Duration::from_millis(200);
        s.ice_disconnected_since = Some(since);
        let _ = s.handle_event(ice(IceConnectionState::Disconnected));
        assert_eq!(s.ice_disconnected_since, Some(since));
        assert!(!s.is_disconnected());
        std::thread::sleep(Duration::from_millis(250));
        assert!(s.is_disconnected(), "Disconnected for the whole grace");
        // ICE back up: the clock is cleared.
        let _ = s.handle_event(ice(IceConnectionState::Completed));
        assert!(!s.is_disconnected(), "recovered");

        // The peer's DTLS close_notify.
        let _ = s.handle_event(Event::Closed);
        assert!(s.is_disconnected(), "the peer closed the session");

        let mut s = session(true).await;
        s.rtc.disconnect();
        assert!(s.is_disconnected(), "a closed Rtc is over");
    }

    /// A full-ICE session (the WHIP output's) gets no grace: its agent
    /// reports `Disconnected` only once its consent retransmits have run out,
    /// by when an ICE-Lite endpoint has pruned the pair for good, so its
    /// first `Disconnected` is the end — even if ICE later reports another
    /// state. It used to be given the ICE-Lite grace too: 15 s more of
    /// outage, the stream sent to a dead address throughout. Either kind is
    /// `ice_down` from the report until it recovers.
    #[tokio::test]
    async fn a_full_ice_session_ends_at_its_first_disconnected() {
        let ice = |state| Event::IceConnectionStateChange(state);
        let mut full = session(false).await;
        let mut lite = session(true).await;
        for s in [&mut full, &mut lite] {
            assert!(!s.ice_down());
            let _ = s.handle_event(ice(IceConnectionState::Disconnected));
            assert!(s.ice_down());
        }
        assert!(full.is_disconnected(), "a full agent's Disconnected is the end");
        assert!(!lite.is_disconnected(), "an ICE-Lite one has its grace");

        let _ = full.handle_event(ice(IceConnectionState::Checking));
        assert!(full.is_disconnected(), "latched");
        let _ = lite.handle_event(ice(IceConnectionState::Completed));
        assert!(!lite.ice_down() && !lite.is_disconnected(), "recovered");
    }

    /// A viewer — `recvonly` H.264 + Opus, full ICE, as a browser is —
    /// connected to an ICE-Lite server (the WHEP output's role) on loopback.
    async fn connected() -> (WebrtcSession, WebrtcSession) {
        let (mut server, mut viewer) = (session(true).await, session(false).await);
        let (offer, pending) = viewer.create_offer(true, true, false).unwrap();
        let answer = server.accept_offer(&offer).unwrap();
        viewer.apply_answer(&answer, pending).unwrap();
        let cancel = CancellationToken::new();
        let (mut server_up, mut viewer_up) = (false, false);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !(server_up && viewer_up) {
                tokio::select! {
                    e = server.poll_event(&cancel) => server_up |= matches!(e, SessionEvent::Connected),
                    e = viewer.poll_event(&cancel) => viewer_up |= matches!(e, SessionEvent::Connected),
                }
            }
        })
        .await
        .expect("the viewer never connected");
        server.drain_pending_events();
        viewer.drain_pending_events();
        (server, viewer)
    }

    /// One 20 ms Opus frame of `byte`s from the server, as a send loop
    /// writes one: written, drained, then the socket driven.
    async fn send_frame(server: &mut WebrtcSession, n: &mut u64, byte: u8) {
        let mid = server.audio_mid.unwrap();
        let pt = server.get_pt(mid).unwrap();
        let at = MediaTime::new(*n * 960, str0m::media::Frequency::FORTY_EIGHT_KHZ);
        *n += 1;
        server
            .write_media(mid, pt, Instant::now(), at, &[byte; 40])
            .unwrap();
        server.drain_outputs().await;
        server.drive_udp_io().await;
    }

    /// ICE `Disconnected` on an ICE-Lite session is timed, not latched. A
    /// viewer whose checks stop for 18 s — `is` prunes its pair 15 s after
    /// the last one, and ICE goes `Disconnected` — and then resume is
    /// re-nominated: ICE is back up, the session was never counted over,
    /// and the viewer hears what is written after. A latch on the first
    /// `Disconnected` (bilbycast-relay 9836299) ended it at about 15 s.
    #[tokio::test]
    async fn an_ice_lite_session_rides_out_a_pause_its_viewer_comes_back_from() {
        let (mut server, mut viewer) = connected().await;
        let viewer_audio = viewer.audio_mid.unwrap();
        let mut n = 0;

        // The viewer pauses — not driven at all — while the server sends.
        let mut ice_went_down = false;
        let resume_at = Instant::now() + Duration::from_secs(18);
        while Instant::now() < resume_at {
            send_frame(&mut server, &mut n, 0xAA).await;
            ice_went_down |= server.ice_disconnected_since.is_some();
            assert!(!server.is_disconnected(), "counted over during the grace");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            ice_went_down,
            "ICE never went Disconnected: the pause proved nothing"
        );

        // The viewer is back: it is heard from, and hears the new frames.
        let cancel = CancellationToken::new();
        let mut heard = 0;
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let back = tokio::time::timeout(Duration::from_secs(10), async {
            while heard < 25 || server.ice_disconnected_since.is_some() {
                tokio::select! {
                    e = viewer.poll_event(&cancel) => {
                        if let SessionEvent::MediaData { mid, data, .. } = e
                            && mid == viewer_audio
                            && data.first() == Some(&0xBB)
                        {
                            heard += 1;
                        }
                    }
                    _ = tick.tick() => send_frame(&mut server, &mut n, 0xBB).await,
                }
            }
        })
        .await;
        assert!(
            back.is_ok(),
            "heard {heard} new frames; ICE back up: {}",
            server.ice_disconnected_since.is_none()
        );
        assert!(!server.is_disconnected());
    }

    /// What is written while ICE is down never reaches the peer — str0m
    /// would send it to the last nominated address whatever the ICE state —
    /// and what is written once ICE is back up does. (The send loops go on
    /// encoding meanwhile, so it is `write_media` that must drop it.)
    #[tokio::test]
    async fn nothing_written_while_ice_is_down_reaches_the_peer() {
        let (mut server, mut viewer) = connected().await;
        let viewer_audio = viewer.audio_mid.unwrap();
        let mut n = 0;
        server.ice_disconnected_since = Some(Instant::now());
        for _ in 0..10 {
            send_frame(&mut server, &mut n, 0xCC).await;
        }
        server.ice_disconnected_since = None;
        for _ in 0..10 {
            send_frame(&mut server, &mut n, 0xBB).await;
        }
        let cancel = CancellationToken::new();
        let (mut while_down, mut once_up) = (0, 0);
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let SessionEvent::MediaData { mid, data, .. } = viewer.poll_event(&cancel).await
                    && mid == viewer_audio
                {
                    match data.first() {
                        Some(0xCC) => while_down += 1,
                        Some(0xBB) => once_up += 1,
                        _ => {}
                    }
                }
            }
        })
        .await;
        assert_eq!(while_down, 0, "frames written while ICE was down were sent");
        assert!(once_up > 0, "nothing written once ICE was back up was heard");
    }

    /// A viewer's keyframe request (PLI) is kept until the send loop takes
    /// it, whichever drain read it: `drive_udp_io` returns only a batch's
    /// first event, and the WHEP viewer loop dropped even that.
    #[tokio::test]
    async fn a_viewers_keyframe_request_is_kept_until_taken() {
        let (mut server, mut viewer) = connected().await;
        let (server_video, viewer_video) = (server.video_mid.unwrap(), viewer.video_mid.unwrap());
        let pt = server.get_pt(server_video).unwrap();
        let mut au = vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x01, 0x41, 0xfb, 0x01, 0x10];
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 0, 1, 0x65, 0x88, 0x84]);
        au.extend(std::iter::repeat_n(0x5a, 600));
        let cancel = CancellationToken::new();
        let mut tick = tokio::time::interval(Duration::from_millis(40));
        let mut k = 0u64;

        // Video flows, so the viewer has a stream to ask on.
        let seen = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    e = viewer.poll_event(&cancel) => {
                        if matches!(e, SessionEvent::MediaData { mid, .. } if mid == viewer_video) {
                            break;
                        }
                    }
                    _ = tick.tick() => {
                        let at = MediaTime::new(k * 3600, str0m::media::Frequency::NINETY_KHZ);
                        k += 1;
                        server.write_media(server_video, pt, Instant::now(), at, &au).unwrap();
                        server.drain_outputs().await;
                        server.drive_udp_io().await;
                    }
                }
            }
        })
        .await;
        assert!(seen.is_ok(), "the viewer was sent no video");
        assert!(!server.take_keyframe_request(), "a request before any was made");

        viewer
            .rtc
            .writer(viewer_video)
            .unwrap()
            .request_keyframe(None, str0m::media::KeyframeRequestKind::Pli)
            .unwrap();
        let taken = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                viewer.drain_outputs().await;
                server.drive_udp_io().await;
                if server.take_keyframe_request() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(taken.is_ok(), "the viewer's PLI never reached the server's send loop");
        assert!(!server.take_keyframe_request(), "taken, but still pending");
    }
}
