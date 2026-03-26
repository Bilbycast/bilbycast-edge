// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

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
}

/// A WebRTC session wrapping str0m's `Rtc` state machine.
pub struct WebrtcSession {
    rtc: Rtc,
    socket: UdpSocket,
    local_addr: SocketAddr,
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

        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .build(Instant::now());

        // Add host candidate — resolve 0.0.0.0 to a real local IP
        let candidate_ip = config.public_ip.unwrap_or_else(|| {
            let ip = local_addr.ip();
            if ip.is_unspecified() {
                // Discover a real local IP by connecting a throwaway UDP socket
                std::net::UdpSocket::bind("0.0.0.0:0")
                    .and_then(|s| { s.connect("8.8.8.8:80")?; s.local_addr() })
                    .map(|a| a.ip())
                    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
            } else {
                ip
            }
        });
        let candidate_addr = SocketAddr::new(candidate_ip, local_addr.port());
        rtc.add_local_candidate(
            Candidate::host(candidate_addr, Protocol::Udp)
                .map_err(|e| anyhow::anyhow!("ICE candidate error: {}", e))?,
        );

        Ok(Self {
            rtc,
            socket,
            local_addr,
            video_mid: None,
            audio_mid: None,
            buf: vec![0u8; 2000],
        })
    }

    /// Accept an SDP offer (server mode) and return the SDP answer string.
    pub fn accept_offer(&mut self, offer_sdp: &str) -> Result<String> {
        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| anyhow::anyhow!("SDP parse error: {}", e))?;

        let answer = self.rtc.sdp_api().accept_offer(offer)
            .map_err(|e| anyhow::anyhow!("SDP accept error: {}", e))?;

        // MIDs will be discovered via MediaAdded events
        Ok(answer.to_sdp_string())
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

        Ok((offer.to_sdp_string(), pending))
    }

    /// Apply an SDP answer received from the remote peer (client mode).
    /// Requires the pending offer from `create_offer()`.
    pub fn apply_answer(&mut self, answer_sdp: &str, pending: str0m::change::SdpPendingOffer) -> Result<()> {
        let answer = str0m::change::SdpAnswer::from_sdp_string(answer_sdp)
            .map_err(|e| anyhow::anyhow!("SDP answer parse error: {}", e))?;

        self.rtc.sdp_api().accept_answer(pending, answer)
            .map_err(|e| anyhow::anyhow!("SDP answer accept error: {}", e))?;

        Ok(())
    }

    /// Get the local socket address.
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

    /// Get the first negotiated payload type for a given MID.
    pub fn get_pt(&mut self, mid: Mid) -> Option<Pt> {
        let writer = self.rtc.writer(mid)?;
        writer.payload_params().next().map(|p| p.pt())
    }

    /// Check if the session is still alive.
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// Poll str0m for output without blocking. Used by the WHIP client
    /// output to drain transmits after writing media data.
    pub fn rtc_poll_output(&mut self) -> Result<Output, str0m::RtcError> {
        self.rtc.poll_output()
    }

    /// Send UDP data to a destination. Thin wrapper for the output loop.
    pub async fn send_udp(&self, transmit: &str0m::net::Transmit) -> std::io::Result<usize> {
        self.socket.send_to(&transmit.contents, transmit.destination).await
    }

    /// Drive the session event loop. Blocks until a meaningful event occurs.
    pub async fn poll_event(&mut self, cancel: &CancellationToken) -> SessionEvent {
        loop {
            // Drain all pending str0m outputs
            match self.rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    let _ = self.socket.send_to(&transmit.contents, transmit.destination).await;
                    continue;
                }
                Ok(Output::Event(event)) => {
                    if let Some(se) = self.handle_event(event) {
                        return se;
                    }
                    continue;
                }
                Ok(Output::Timeout(deadline)) => {
                    // Wait for input
                    let sleep_dur = deadline.saturating_duration_since(Instant::now());
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
                                    let receive = str0m::net::Receive {
                                        proto: Protocol::Udp,
                                        source,
                                        destination: self.local_addr,
                                        contents: (&self.buf[..len]).try_into().unwrap(),
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
                tracing::info!("Media track added: {:?} mid={:?}", kind, added.mid);
                Some(SessionEvent::MediaAdded { mid: added.mid, kind })
            }
            Event::MediaData(data) => {
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
