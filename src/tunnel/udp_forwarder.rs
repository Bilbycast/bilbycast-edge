// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! UDP tunnel forwarder.
//!
//! Forwards UDP datagrams between a local UDP socket and a QUIC connection
//! (relay or direct peer) using QUIC datagrams (unreliable, unordered).
//!
//! When a `TunnelCipher` is provided, all payloads are encrypted end-to-end
//! before being sent through the relay. The relay sees only ciphertext.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use quinn::Connection;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::crypto::TunnelCipher;
use super::protocol;

/// Lock-free peer address cache using atomics.
///
/// Stores a `SocketAddr` (IPv4 or IPv6) as atomic fields so the UDP forwarder
/// can update and read the peer address on every packet without locking.
struct AtomicPeerAddr {
    /// 0 = unset, 4 = IPv4, 6 = IPv6
    family: AtomicU8,
    /// IPv4: 4 bytes in network order stored in low 32 bits.
    /// IPv6: full 128 bits stored as two AtomicU64.
    ip_high: AtomicU64,
    ip_low: AtomicU64,
    port: AtomicU16,
}

impl AtomicPeerAddr {
    fn new() -> Self {
        Self {
            family: AtomicU8::new(0),
            ip_high: AtomicU64::new(0),
            ip_low: AtomicU64::new(0),
            port: AtomicU16::new(0),
        }
    }

    fn store(&self, addr: SocketAddr) {
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

    fn load(&self) -> Option<SocketAddr> {
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
pub async fn run_egress(
    tunnel_id: Uuid,
    local_addr: SocketAddr,
    conn: Connection,
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
                match conn.send_datagram(Bytes::from(datagram)) {
                    Ok(()) => {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_sent.fetch_add(len as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        stats.send_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(tunnel_id = %tunnel_id, "QUIC datagram send error: {e}");
                    }
                }
            }

            // QUIC → Local: receive QUIC datagram, decrypt, forward to last-seen local sender
            result = conn.read_datagram() => {
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
pub async fn run_ingress(
    tunnel_id: Uuid,
    forward_addr: SocketAddr,
    conn: Connection,
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
            // QUIC → Local: receive datagram from relay, decrypt, forward to local app
            result = conn.read_datagram() => {
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

            // Local → QUIC: receive return traffic from local app, encrypt, send back
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
                match conn.send_datagram(Bytes::from(datagram)) {
                    Ok(()) => {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_sent.fetch_add(len as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        stats.send_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(tunnel_id = %tunnel_id, "QUIC datagram send error: {e}");
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
