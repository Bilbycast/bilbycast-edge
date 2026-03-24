// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Wire protocol for QUIC tunnel control and data streams.
//!
//! This must match the protocol defined in `bilbycast-relay/src/protocol.rs`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── ALPN protocol identifiers ──

/// ALPN protocol for edge-to-relay connections.
pub const ALPN_RELAY: &[u8] = b"bilbycast-relay";

/// ALPN protocol for direct edge-to-edge connections.
pub const ALPN_DIRECT: &[u8] = b"bilbycast-direct";

// ── Relay mode messages ──

/// Messages sent from edge to relay on the control stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EdgeMessage {
    /// Authenticate with a relay token.
    #[serde(rename = "auth")]
    Auth { token: String },

    /// Bind a tunnel endpoint on this edge.
    #[serde(rename = "tunnel_bind")]
    TunnelBind {
        tunnel_id: Uuid,
        direction: RelayDirection,
        protocol: RelayProtocol,
    },

    /// Unbind a tunnel.
    #[serde(rename = "tunnel_unbind")]
    TunnelUnbind { tunnel_id: Uuid },

    /// Keepalive ping.
    #[serde(rename = "ping")]
    Ping,
}

/// Messages sent from relay to edge on the control stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RelayMessage {
    /// Authentication succeeded.
    #[serde(rename = "auth_ok")]
    AuthOk { edge_id: String },

    /// Authentication failed.
    #[serde(rename = "auth_error")]
    AuthError { reason: String },

    /// Tunnel is ready (both sides have bound).
    #[serde(rename = "tunnel_ready")]
    TunnelReady { tunnel_id: Uuid },

    /// Tunnel is waiting for the peer to bind.
    #[serde(rename = "tunnel_waiting")]
    TunnelWaiting { tunnel_id: Uuid },

    /// Tunnel went down (peer disconnected or unbound).
    #[serde(rename = "tunnel_down")]
    TunnelDown { tunnel_id: Uuid, reason: String },

    /// Keepalive pong.
    #[serde(rename = "pong")]
    Pong,
}

// ── Direct mode messages ──

/// Messages exchanged on the control stream of a direct edge-to-edge connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PeerMessage {
    /// Authenticate with a pre-shared key.
    #[serde(rename = "peer_auth")]
    PeerAuth { tunnel_id: Uuid, token: String },

    /// Authentication succeeded.
    #[serde(rename = "peer_auth_ok")]
    PeerAuthOk { tunnel_id: Uuid },

    /// Authentication failed.
    #[serde(rename = "peer_auth_error")]
    PeerAuthError { reason: String },

    /// Keepalive ping.
    #[serde(rename = "ping")]
    Ping,

    /// Keepalive pong.
    #[serde(rename = "pong")]
    Pong,
}

// ── Direction / Protocol enums (relay wire format) ──

/// Direction as sent over the relay wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayDirection {
    #[serde(rename = "ingress")]
    Ingress,
    #[serde(rename = "egress")]
    Egress,
}

/// Protocol as sent over the relay wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayProtocol {
    #[serde(rename = "tcp")]
    Tcp,
    #[serde(rename = "udp")]
    Udp,
}

// ── Data plane structures ──

/// Header sent at the start of each data QUIC stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHeader {
    pub tunnel_id: Uuid,
    pub stream_type: StreamType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamType {
    #[serde(rename = "tcp")]
    Tcp,
}

// ── Wire format helpers ──

/// Maximum message size (1 MB).
pub const MAX_MESSAGE_SIZE: usize = 1_048_576;

/// UDP datagram prefix length (16-byte UUID).
pub const UDP_DATAGRAM_PREFIX_LEN: usize = 16;

/// Read a length-prefixed JSON message from a QUIC stream.
pub async fn read_message<T: serde::de::DeserializeOwned>(
    recv: &mut quinn::RecvStream,
) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("message too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

/// Write a length-prefixed JSON message to a QUIC stream.
pub async fn write_message<T: Serialize>(
    send: &mut quinn::SendStream,
    msg: &T,
) -> anyhow::Result<()> {
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_be_bytes();
    send.write_all(&len).await?;
    send.write_all(&json).await?;
    Ok(())
}

/// Read a stream header from a newly opened data stream.
pub async fn read_stream_header(recv: &mut quinn::RecvStream) -> anyhow::Result<StreamHeader> {
    read_message(recv).await
}

/// Write a stream header to a newly opened data stream.
pub async fn write_stream_header(
    send: &mut quinn::SendStream,
    header: &StreamHeader,
) -> anyhow::Result<()> {
    write_message(send, header).await
}

/// Encode a UDP datagram with tunnel_id prefix (16-byte UUID binary + payload).
pub fn encode_udp_datagram(tunnel_id: &Uuid, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(UDP_DATAGRAM_PREFIX_LEN + payload.len());
    buf.extend_from_slice(tunnel_id.as_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decode a UDP datagram: extract tunnel_id and payload.
pub fn decode_udp_datagram(data: &[u8]) -> Option<(Uuid, &[u8])> {
    if data.len() < UDP_DATAGRAM_PREFIX_LEN {
        return None;
    }
    let id = Uuid::from_bytes(data[..16].try_into().ok()?);
    Some((id, &data[16..]))
}
