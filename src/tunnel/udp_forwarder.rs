// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! UDP tunnel forwarder.
//!
//! Forwards UDP datagrams between a local UDP socket and a QUIC connection
//! (relay or direct peer) using QUIC datagrams (unreliable, unordered).
//!
//! When a `TunnelCipher` is provided, all payloads are encrypted end-to-end
//! before being sent through the relay. The relay sees only ciphertext.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use quinn::Connection;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::crypto::TunnelCipher;
use super::protocol::{self, try_decode_udp_control, UdpRelayControl};

/// A bidirectional datagram carrier the UDP forwarder rides.
///
/// Each datagram is already framed (`[16-byte tunnel_id][AEAD payload]` via
/// [`protocol::encode_udp_datagram`]). This trait lets [`run_egress`] /
/// [`run_ingress`] work over either the legacy QUIC carrier ([`QuicLink`]) or
/// the native plain-UDP carrier ([`PlainUdpLink`]) with one implementation —
/// the encrypt / frame / `AtomicPeerAddr` / stats logic is transport-agnostic.
pub trait DatagramLink: Send + Sync {
    /// Send one framed datagram. Best-effort: a full buffer is dropped (not an
    /// error) just like QUIC datagrams — media transports own their own recovery.
    fn send_datagram(&self, data: Bytes) -> Result<()>;
    /// Receive the next inbound framed media datagram. Control datagrams (the
    /// nil-UUID-prefixed [`UdpRelayControl`]) are consumed internally and never
    /// surface here.
    fn recv_datagram(&self) -> impl Future<Output = Result<Bytes>> + Send;
}

/// QUIC carrier — the legacy tunnel data plane. Thin wrapper over a
/// [`quinn::Connection`] so it satisfies [`DatagramLink`].
pub struct QuicLink(pub Connection);

impl DatagramLink for QuicLink {
    fn send_datagram(&self, data: Bytes) -> Result<()> {
        self.0
            .send_datagram(data)
            .map_err(|e| anyhow::anyhow!("quic datagram send: {e}"))
    }

    fn recv_datagram(&self) -> impl Future<Output = Result<Bytes>> + Send {
        async move {
            self.0
                .read_datagram()
                .await
                .map_err(|e| anyhow::anyhow!("quic datagram recv: {e}"))
        }
    }
}

/// Plain-UDP carrier — native SRT/RIST over relay / direct without QUIC.
///
/// Wraps a `UdpSocket` *connected* to the relay (or direct peer), so the same
/// 5-tuple carries both the registration/keepalive control datagrams (sent by a
/// sibling task — see `udp_relay_client`) and the media. `recv_datagram` peels
/// off control acks (updating liveness state used for failover) and returns
/// only media.
pub struct PlainUdpLink {
    sock: Arc<UdpSocket>,
    /// Set true once the relay/peer acks with both sides latched.
    pub ready: Arc<AtomicBool>,
    /// Epoch-ms of the last control ack (0 = none). Drives relay-liveness /
    /// failover decisions in the register task.
    pub last_ack_ms: Arc<AtomicU64>,
}

impl PlainUdpLink {
    pub fn new(sock: Arc<UdpSocket>) -> Self {
        Self {
            sock,
            ready: Arc::new(AtomicBool::new(false)),
            last_ack_ms: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Wall-clock epoch in milliseconds (saturating to 0 before 1970).
pub(crate) fn now_epoch_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl DatagramLink for PlainUdpLink {
    fn send_datagram(&self, data: Bytes) -> Result<()> {
        match self.sock.try_send(&data) {
            Ok(_) => Ok(()),
            // Buffer full → drop, matching QUIC datagram best-effort semantics.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(anyhow::anyhow!("udp datagram send: {e}")),
        }
    }

    fn recv_datagram(&self) -> impl Future<Output = Result<Bytes>> + Send {
        async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let n = self.sock.recv(&mut buf).await?;
                let data = &buf[..n];
                // Control plane (acks) → update liveness, keep looping for media.
                if let Some(ctrl) = try_decode_udp_control(data) {
                    if let UdpRelayControl::Ack { ready, .. } = ctrl {
                        self.ready.store(ready, Ordering::Relaxed);
                        self.last_ack_ms.store(now_epoch_ms(), Ordering::Relaxed);
                    }
                    continue;
                }
                return Ok(Bytes::copy_from_slice(data));
            }
        }
    }
}

/// Lock-free peer address cache using atomics.
///
/// Stores a `SocketAddr` (IPv4 or IPv6) as atomic fields so the UDP forwarder
/// can update and read the peer address on every packet without locking.
pub struct AtomicPeerAddr {
    /// 0 = unset, 4 = IPv4, 6 = IPv6
    family: AtomicU8,
    /// IPv4: 4 bytes in network order stored in low 32 bits.
    /// IPv6: full 128 bits stored as two AtomicU64.
    ip_high: AtomicU64,
    ip_low: AtomicU64,
    port: AtomicU16,
}

impl AtomicPeerAddr {
    pub fn new() -> Self {
        Self {
            family: AtomicU8::new(0),
            ip_high: AtomicU64::new(0),
            ip_low: AtomicU64::new(0),
            port: AtomicU16::new(0),
        }
    }

    pub fn store(&self, addr: SocketAddr) {
        match addr.ip() {
            IpAddr::V4(v4) => {
                let bits = u32::from(v4) as u64;
                self.ip_low.store(bits, Ordering::Relaxed);
                self.ip_high.store(0, Ordering::Relaxed);
                self.port.store(addr.port(), Ordering::Relaxed);
                // Write family last to publish the complete address.
                self.family.store(4, Ordering::Release);
            }
            IpAddr::V6(v6) => {
                let octets = v6.octets();
                let high = u64::from_be_bytes(octets[..8].try_into().unwrap());
                let low = u64::from_be_bytes(octets[8..].try_into().unwrap());
                self.ip_high.store(high, Ordering::Relaxed);
                self.ip_low.store(low, Ordering::Relaxed);
                self.port.store(addr.port(), Ordering::Relaxed);
                self.family.store(6, Ordering::Release);
            }
        }
    }

    pub fn load(&self) -> Option<SocketAddr> {
        let fam = self.family.load(Ordering::Acquire);
        if fam == 0 {
            return None;
        }
        let port = self.port.load(Ordering::Relaxed);
        let ip = if fam == 4 {
            IpAddr::V4(Ipv4Addr::from(self.ip_low.load(Ordering::Relaxed) as u32))
        } else {
            let high = self.ip_high.load(Ordering::Relaxed).to_be_bytes();
            let low = self.ip_low.load(Ordering::Relaxed).to_be_bytes();
            let mut octets = [0u8; 16];
            octets[..8].copy_from_slice(&high);
            octets[8..].copy_from_slice(&low);
            IpAddr::V6(Ipv6Addr::from(octets))
        };
        Some(SocketAddr::new(ip, port))
    }
}

/// Statistics for a UDP tunnel forwarder.
#[derive(Debug, Default)]
pub struct UdpForwarderStats {
    pub packets_sent: AtomicU64,
    pub packets_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub send_errors: AtomicU64,
}

/// Run the UDP forwarder for an **egress** tunnel.
///
/// Egress: listen on `local_addr` for UDP packets from local applications,
/// wrap each in a QUIC datagram with tunnel_id prefix, and send to the relay/peer.
/// Also receive QUIC datagrams from relay/peer and forward to the last-seen local sender.
pub async fn run_egress<L: DatagramLink>(
    tunnel_id: Uuid,
    local_addr: SocketAddr,
    link: L,
    stats: Arc<UdpForwarderStats>,
    cancel: CancellationToken,
    cipher: Option<Arc<TunnelCipher>>,
) -> Result<()> {
    let socket = UdpSocket::bind(local_addr).await.map_err(|e| {
        crate::util::port_error::annotate_bind_error(e, local_addr, "UDP tunnel egress")
    })?;
    tracing::info!(
        tunnel_id = %tunnel_id,
        local_addr = %local_addr,
        encrypted = cipher.is_some(),
        "UDP egress forwarder started"
    );

    let socket = Arc::new(socket);
    // Track the last sender address for return traffic (lock-free)
    let peer_addr = Arc::new(AtomicPeerAddr::new());

    let mut buf = vec![0u8; 2048];

    loop {
        tokio::select! {
            // Local → QUIC: receive from local UDP socket, encrypt, send as QUIC datagram
            result = socket.recv_from(&mut buf) => {
                let (len, from_addr) = result?;
                // Remember sender for return traffic (lock-free)
                peer_addr.store(from_addr);
                let payload = if let Some(ref c) = cipher {
                    match c.encrypt(&buf[..len]) {
                        Ok(encrypted) => encrypted,
                        Err(e) => {
                            tracing::debug!(tunnel_id = %tunnel_id, "Encryption error: {e}");
                            stats.send_errors.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                } else {
                    buf[..len].to_vec()
                };
                let datagram = protocol::encode_udp_datagram(&tunnel_id, &payload);
                match link.send_datagram(Bytes::from(datagram)) {
                    Ok(()) => {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_sent.fetch_add(len as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        stats.send_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(tunnel_id = %tunnel_id, "tunnel datagram send error: {e}");
                    }
                }
            }

            // Carrier → Local: receive a framed datagram, decrypt, forward to last-seen local sender
            result = link.recv_datagram() => {
                let datagram = result?;
                if let Some((_tid, encrypted_payload)) = protocol::decode_udp_datagram(&datagram) {
                    let payload = if let Some(ref c) = cipher {
                        match c.decrypt(encrypted_payload) {
                            Ok(decrypted) => decrypted,
                            Err(e) => {
                                tracing::debug!(tunnel_id = %tunnel_id, "Decryption error: {e}");
                                continue;
                            }
                        }
                    } else {
                        encrypted_payload.to_vec()
                    };
                    if let Some(addr) = peer_addr.load() {
                        let _ = socket.send_to(&payload, addr).await;
                        stats.packets_received.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_received.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    }
                }
            }

            _ = cancel.cancelled() => {
                tracing::info!(tunnel_id = %tunnel_id, "UDP egress forwarder stopping");
                break;
            }
        }
    }

    Ok(())
}

/// Run the UDP forwarder for an **ingress** tunnel.
///
/// Ingress: receive QUIC datagrams from relay/peer, decrypt the payload,
/// and forward to `forward_addr`. Also listen for return UDP traffic and
/// send it back through the QUIC connection.
pub async fn run_ingress<L: DatagramLink>(
    tunnel_id: Uuid,
    forward_addr: SocketAddr,
    link: L,
    stats: Arc<UdpForwarderStats>,
    cancel: CancellationToken,
    cipher: Option<Arc<TunnelCipher>>,
) -> Result<()> {
    // Bind an ephemeral port for sending to the forward address
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    tracing::info!(
        tunnel_id = %tunnel_id,
        forward_addr = %forward_addr,
        local_port = %socket.local_addr()?,
        encrypted = cipher.is_some(),
        "UDP ingress forwarder started"
    );

    let socket = Arc::new(socket);
    let mut buf = vec![0u8; 2048];

    loop {
        tokio::select! {
            // Carrier → Local: receive a framed datagram, decrypt, forward to local app
            result = link.recv_datagram() => {
                let datagram = result?;
                if let Some((_tid, encrypted_payload)) = protocol::decode_udp_datagram(&datagram) {
                    let payload = if let Some(ref c) = cipher {
                        match c.decrypt(encrypted_payload) {
                            Ok(decrypted) => decrypted,
                            Err(e) => {
                                tracing::debug!(tunnel_id = %tunnel_id, "Decryption error: {e}");
                                continue;
                            }
                        }
                    } else {
                        encrypted_payload.to_vec()
                    };
                    let _ = socket.send_to(&payload, forward_addr).await;
                    stats.packets_received.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_received.fetch_add(payload.len() as u64, Ordering::Relaxed);
                }
            }

            // Local → Carrier: receive return traffic from local app, encrypt, send back
            result = socket.recv_from(&mut buf) => {
                let (len, _from_addr) = result?;
                let payload = if let Some(ref c) = cipher {
                    match c.encrypt(&buf[..len]) {
                        Ok(encrypted) => encrypted,
                        Err(e) => {
                            tracing::debug!(tunnel_id = %tunnel_id, "Encryption error: {e}");
                            stats.send_errors.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                } else {
                    buf[..len].to_vec()
                };
                let datagram = protocol::encode_udp_datagram(&tunnel_id, &payload);
                match link.send_datagram(Bytes::from(datagram)) {
                    Ok(()) => {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_sent.fetch_add(len as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        stats.send_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(tunnel_id = %tunnel_id, "tunnel datagram send error: {e}");
                    }
                }
            }

            _ = cancel.cancelled() => {
                tracing::info!(tunnel_id = %tunnel_id, "UDP ingress forwarder stopping");
                break;
            }
        }
    }

    Ok(())
}
