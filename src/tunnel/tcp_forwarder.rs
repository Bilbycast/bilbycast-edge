// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! TCP tunnel forwarder.
//!
//! Forwards TCP connections between local TCP sockets and QUIC streams.
//! Each TCP connection maps to a single QUIC bidirectional stream.
//!
//! Suitable for camera control, signaling, and other reliable-delivery use cases.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use quinn::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::protocol::{self, StreamHeader, StreamType};

/// Statistics for a TCP tunnel forwarder.
#[derive(Debug, Default)]
pub struct TcpForwarderStats {
    pub connections_total: AtomicU64,
    pub connections_active: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
}

/// Run the TCP forwarder for an **egress** tunnel.
///
/// Egress: listen on `local_addr` for incoming TCP connections. For each
/// connection, open a QUIC bidi stream to the relay/peer, send a StreamHeader,
/// and pipe data bidirectionally.
pub async fn run_egress(
    tunnel_id: Uuid,
    local_addr: SocketAddr,
    conn: Connection,
    stats: Arc<TcpForwarderStats>,
    cancel: CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(local_addr).await?;
    tracing::info!(
        tunnel_id = %tunnel_id,
        local_addr = %local_addr,
        "TCP egress forwarder started"
    );

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (tcp_stream, peer_addr) = result?;
                stats.connections_total.fetch_add(1, Ordering::Relaxed);
                stats.connections_active.fetch_add(1, Ordering::Relaxed);

                tracing::debug!(
                    tunnel_id = %tunnel_id,
                    peer = %peer_addr,
                    "New TCP connection, opening QUIC stream"
                );

                let conn = conn.clone();
                let stats = stats.clone();
                let cancel = cancel.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_egress_tcp(tunnel_id, tcp_stream, conn, &stats, cancel).await {
                        tracing::debug!(tunnel_id = %tunnel_id, "TCP egress stream ended: {e}");
                    }
                    stats.connections_active.fetch_sub(1, Ordering::Relaxed);
                });
            }

            _ = cancel.cancelled() => {
                tracing::info!(tunnel_id = %tunnel_id, "TCP egress forwarder stopping");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_egress_tcp(
    tunnel_id: Uuid,
    mut tcp_stream: TcpStream,
    conn: Connection,
    stats: &TcpForwarderStats,
    cancel: CancellationToken,
) -> Result<()> {
    // Open a QUIC bidi stream
    let (mut quic_send, mut quic_recv) = conn.open_bi().await?;

    // Send stream header so the peer knows which tunnel this belongs to
    protocol::write_stream_header(
        &mut quic_send,
        &StreamHeader {
            tunnel_id,
            stream_type: StreamType::Tcp,
        },
    )
    .await?;

    // Pipe data bidirectionally
    let (mut tcp_read, mut tcp_write) = tcp_stream.split();

    let stats_tx = &stats.bytes_sent;
    let stats_rx = &stats.bytes_received;

    tokio::select! {
        result = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = tcp_read.read(&mut buf).await?;
                if n == 0 { break; }
                quic_send.write_all(&buf[..n]).await?;
                stats_tx.fetch_add(n as u64, Ordering::Relaxed);
            }
            quic_send.finish()?;
            Ok::<_, anyhow::Error>(())
        } => { result?; }

        result = async {
            let mut buf = vec![0u8; 8192];
            loop {
                match quic_recv.read(&mut buf).await? {
                    Some(n) => {
                        tcp_write.write_all(&buf[..n]).await?;
                        stats_rx.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    None => break,
                }
            }
            Ok::<_, anyhow::Error>(())
        } => { result?; }

        _ = cancel.cancelled() => {}
    }

    Ok(())
}

/// Run the TCP forwarder for an **ingress** tunnel.
///
/// Ingress: accept QUIC bidi streams from the relay/peer. For each stream,
/// read the StreamHeader, connect to `forward_addr`, and pipe data bidirectionally.
pub async fn run_ingress(
    tunnel_id: Uuid,
    forward_addr: SocketAddr,
    conn: Connection,
    stats: Arc<TcpForwarderStats>,
    cancel: CancellationToken,
) -> Result<()> {
    tracing::info!(
        tunnel_id = %tunnel_id,
        forward_addr = %forward_addr,
        "TCP ingress forwarder started"
    );

    loop {
        tokio::select! {
            result = conn.accept_bi() => {
                let (quic_send, mut quic_recv) = result?;
                stats.connections_total.fetch_add(1, Ordering::Relaxed);
                stats.connections_active.fetch_add(1, Ordering::Relaxed);

                let forward_addr = forward_addr;
                let stats = stats.clone();
                let cancel = cancel.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_ingress_tcp(
                        tunnel_id, quic_send, &mut quic_recv, forward_addr, &stats, cancel,
                    ).await {
                        tracing::debug!(tunnel_id = %tunnel_id, "TCP ingress stream ended: {e}");
                    }
                    stats.connections_active.fetch_sub(1, Ordering::Relaxed);
                });
            }

            _ = cancel.cancelled() => {
                tracing::info!(tunnel_id = %tunnel_id, "TCP ingress forwarder stopping");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_ingress_tcp(
    tunnel_id: Uuid,
    mut quic_send: quinn::SendStream,
    quic_recv: &mut quinn::RecvStream,
    forward_addr: SocketAddr,
    stats: &TcpForwarderStats,
    cancel: CancellationToken,
) -> Result<()> {
    // Read stream header
    let header = protocol::read_stream_header(quic_recv).await?;
    if header.tunnel_id != tunnel_id {
        anyhow::bail!(
            "Stream header tunnel_id mismatch: expected {tunnel_id}, got {}",
            header.tunnel_id
        );
    }

    // Connect to local forward address
    let mut tcp_stream = TcpStream::connect(forward_addr).await?;
    tracing::debug!(
        tunnel_id = %tunnel_id,
        forward_addr = %forward_addr,
        "TCP ingress: connected to local service"
    );

    let (mut tcp_read, mut tcp_write) = tcp_stream.split();

    let stats_tx = &stats.bytes_sent;
    let stats_rx = &stats.bytes_received;

    tokio::select! {
        result = async {
            let mut buf = vec![0u8; 8192];
            loop {
                match quic_recv.read(&mut buf).await? {
                    Some(n) => {
                        tcp_write.write_all(&buf[..n]).await?;
                        stats_rx.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    None => break,
                }
            }
            Ok::<_, anyhow::Error>(())
        } => { result?; }

        result = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = tcp_read.read(&mut buf).await?;
                if n == 0 { break; }
                quic_send.write_all(&buf[..n]).await?;
                stats_tx.fetch_add(n as u64, Ordering::Relaxed);
            }
            quic_send.finish()?;
            Ok::<_, anyhow::Error>(())
        } => { result?; }

        _ = cancel.cancelled() => {}
    }

    Ok(())
}
