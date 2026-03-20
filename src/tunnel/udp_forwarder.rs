//! UDP tunnel forwarder.
//!
//! Forwards UDP datagrams between a local UDP socket and a QUIC connection
//! (relay or direct peer) using QUIC datagrams (unreliable, unordered).
//!
//! This is ideal for SRT traffic — SRT has its own retransmission and
//! congestion control, so the unreliable QUIC datagram transport is perfect.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use quinn::Connection;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::protocol;

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
) -> Result<()> {
    let socket = UdpSocket::bind(local_addr).await?;
    tracing::info!(
        tunnel_id = %tunnel_id,
        local_addr = %local_addr,
        "UDP egress forwarder started"
    );

    let socket = Arc::new(socket);
    // Track the last sender address for return traffic
    let peer_addr: Arc<tokio::sync::Mutex<Option<SocketAddr>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    let mut buf = vec![0u8; 2048];

    loop {
        tokio::select! {
            // Local → QUIC: receive from local UDP socket, send as QUIC datagram
            result = socket.recv_from(&mut buf) => {
                let (len, from_addr) = result?;
                // Remember sender for return traffic
                {
                    let mut pa = peer_addr.lock().await;
                    *pa = Some(from_addr);
                }
                let datagram = protocol::encode_udp_datagram(&tunnel_id, &buf[..len]);
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

            // QUIC → Local: receive QUIC datagram, forward to last-seen local sender
            result = conn.read_datagram() => {
                let datagram = result?;
                if let Some((_tid, payload)) = protocol::decode_udp_datagram(&datagram) {
                    let pa = peer_addr.lock().await;
                    if let Some(addr) = *pa {
                        let _ = socket.send_to(payload, addr).await;
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
/// Ingress: receive QUIC datagrams from relay/peer, extract the payload,
/// and forward to `forward_addr`. Also listen for return UDP traffic and
/// send it back through the QUIC connection.
pub async fn run_ingress(
    tunnel_id: Uuid,
    forward_addr: SocketAddr,
    conn: Connection,
    stats: Arc<UdpForwarderStats>,
    cancel: CancellationToken,
) -> Result<()> {
    // Bind an ephemeral port for sending to the forward address
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    tracing::info!(
        tunnel_id = %tunnel_id,
        forward_addr = %forward_addr,
        local_port = %socket.local_addr()?,
        "UDP ingress forwarder started"
    );

    let socket = Arc::new(socket);
    let mut buf = vec![0u8; 2048];

    loop {
        tokio::select! {
            // QUIC → Local: receive datagram from relay, forward to local app
            result = conn.read_datagram() => {
                let datagram = result?;
                if let Some((_tid, payload)) = protocol::decode_udp_datagram(&datagram) {
                    let _ = socket.send_to(payload, forward_addr).await;
                    stats.packets_received.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_received.fetch_add(payload.len() as u64, Ordering::Relaxed);
                }
            }

            // Local → QUIC: receive return traffic from local app, send back through tunnel
            result = socket.recv_from(&mut buf) => {
                let (len, _from_addr) = result?;
                let datagram = protocol::encode_udp_datagram(&tunnel_id, &buf[..len]);
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
