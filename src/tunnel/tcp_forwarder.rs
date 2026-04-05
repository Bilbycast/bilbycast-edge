// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! TCP tunnel forwarder.
//!
//! Forwards TCP connections between local TCP sockets and QUIC streams.
//! Each TCP connection maps to a single QUIC bidirectional stream.
//!
//! When a `TunnelCipher` is provided, data is encrypted in chunks with framing:
//! `[4-byte BE length][nonce + ciphertext + tag]`. The relay copies these bytes
//! as-is — it cannot decrypt the content.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use quinn::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::crypto::TunnelCipher;
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
/// and pipe data bidirectionally (with optional encryption).
pub async fn run_egress(
    tunnel_id: Uuid,
    local_addr: SocketAddr,
    conn: Connection,
    stats: Arc<TcpForwarderStats>,
    cancel: CancellationToken,
    cipher: Option<Arc<TunnelCipher>>,
) -> Result<()> {
    let listener = TcpListener::bind(local_addr).await?;
    tracing::info!(
        tunnel_id = %tunnel_id,
        local_addr = %local_addr,
        encrypted = cipher.is_some(),
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
                let cipher = cipher.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_egress_tcp(tunnel_id, tcp_stream, conn, &stats, cancel, cipher).await {
                        tracing::debug!(tunnel_id = %tunnel_id, "TCP egress stream ended: {e}");
                    }
                    stats.connections_active.fetch_sub(1, Ordering::Relaxed);
                });
            }

            reason = conn.closed() => {
                tracing::warn!(
                    tunnel_id = %tunnel_id,
                    reason = %reason,
                    "QUIC connection closed, stopping TCP egress forwarder"
                );
                anyhow::bail!("QUIC connection closed: {reason}");
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
    cipher: Option<Arc<TunnelCipher>>,
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
        // TCP → QUIC (encrypt if cipher present)
        result = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = tcp_read.read(&mut buf).await?;
                if n == 0 { break; }
                if let Some(ref c) = cipher {
                    let encrypted = c.encrypt(&buf[..n])?;
                    let len = (encrypted.len() as u32).to_be_bytes();
                    quic_send.write_all(&len).await?;
                    quic_send.write_all(&encrypted).await?;
                } else {
                    quic_send.write_all(&buf[..n]).await?;
                }
                stats_tx.fetch_add(n as u64, Ordering::Relaxed);
            }
            quic_send.finish()?;
            Ok::<_, anyhow::Error>(())
        } => { result?; }

        // QUIC → TCP (decrypt if cipher present)
        result = async {
            if cipher.is_some() {
                read_encrypted_to_tcp(&cipher, &mut quic_recv, &mut tcp_write, stats_rx).await
            } else {
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
                Ok(())
            }
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
    cipher: Option<Arc<TunnelCipher>>,
) -> Result<()> {
    tracing::info!(
        tunnel_id = %tunnel_id,
        forward_addr = %forward_addr,
        encrypted = cipher.is_some(),
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
                let cipher = cipher.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_ingress_tcp(
                        tunnel_id, quic_send, &mut quic_recv, forward_addr, &stats, cancel, cipher,
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
    cipher: Option<Arc<TunnelCipher>>,
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
        // QUIC → TCP (decrypt if cipher present)
        result = async {
            if cipher.is_some() {
                read_encrypted_to_tcp(&cipher, quic_recv, &mut tcp_write, stats_rx).await
            } else {
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
                Ok(())
            }
        } => { result?; }

        // TCP → QUIC (encrypt if cipher present)
        result = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = tcp_read.read(&mut buf).await?;
                if n == 0 { break; }
                if let Some(ref c) = cipher {
                    let encrypted = c.encrypt(&buf[..n])?;
                    let len = (encrypted.len() as u32).to_be_bytes();
                    quic_send.write_all(&len).await?;
                    quic_send.write_all(&encrypted).await?;
                } else {
                    quic_send.write_all(&buf[..n]).await?;
                }
                stats_tx.fetch_add(n as u64, Ordering::Relaxed);
            }
            quic_send.finish()?;
            Ok::<_, anyhow::Error>(())
        } => { result?; }

        _ = cancel.cancelled() => {}
    }

    Ok(())
}

/// Read length-framed encrypted chunks from QUIC, decrypt, write to TCP.
async fn read_encrypted_to_tcp(
    cipher: &Option<Arc<TunnelCipher>>,
    quic_recv: &mut quinn::RecvStream,
    tcp_write: &mut (impl AsyncWriteExt + Unpin),
    stats_rx: &AtomicU64,
) -> Result<()> {
    let cipher = cipher.as_ref().expect("cipher required for encrypted read");
    let mut len_buf = [0u8; 4];
    loop {
        // Read 4-byte length prefix
        match quic_recv.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(_)) => {
                // Stream closed cleanly
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
        }
        let chunk_len = u32::from_be_bytes(len_buf) as usize;
        if chunk_len > 1_048_576 {
            anyhow::bail!("encrypted chunk too large: {chunk_len} bytes");
        }

        // Read the encrypted chunk
        let mut encrypted = vec![0u8; chunk_len];
        quic_recv.read_exact(&mut encrypted).await?;

        // Decrypt
        let plaintext = cipher.decrypt(&encrypted)?;
        tcp_write.write_all(&plaintext).await?;
        stats_rx.fetch_add(plaintext.len() as u64, Ordering::Relaxed);
    }
    Ok(())
}
