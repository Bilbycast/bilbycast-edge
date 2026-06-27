// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wire protocol for QUIC tunnel control and data streams.
//!
//! This must match the protocol defined in `bilbycast-relay/src/protocol.rs`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Protocol version ──

/// Current tunnel protocol version. Bump when adding new message types or changing semantics.
///
/// v2 (2026-06): adds the plain-UDP relay/direct data plane (native SRT/RIST over
/// relay without QUIC) — see [`UdpRelayControl`] and [`encode_udp_control`].
pub const TUNNEL_PROTOCOL_VERSION: u32 = 2;

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
    /// Bind a tunnel endpoint on this edge.
    #[serde(rename = "tunnel_bind")]
    TunnelBind {
        tunnel_id: Uuid,
        direction: RelayDirection,
        protocol: RelayProtocol,
        /// HMAC-SHA256 bind token for relay authentication (optional, for backwards compat).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind_token: Option<String>,
    },

    /// Unbind a tunnel.
    #[serde(rename = "tunnel_unbind")]
    TunnelUnbind { tunnel_id: Uuid },

    /// Identify this edge with a stable ID (e.g., manager node_id).
    /// Should be sent before any TunnelBind. Optional — relay falls back to connection_id.
    #[serde(rename = "identify")]
    Identify { edge_id: String },

    /// Keepalive ping.
    #[serde(rename = "ping")]
    Ping,

    /// Protocol version handshake (sent as the first message on the control stream).
    /// Old relays (with resilient deserialization) will ignore this; new relays respond with HelloAck.
    #[serde(rename = "hello")]
    Hello {
        protocol_version: u32,
        software_version: String,
    },
}

/// Messages sent from relay to edge on the control stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RelayMessage {
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

    /// Protocol version handshake response.
    /// Sent in reply to an edge's Hello message. Contains the relay's protocol version
    /// so the edge can detect mismatches and log warnings.
    #[serde(rename = "hello_ack")]
    HelloAck {
        protocol_version: u32,
        software_version: String,
    },
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
///
/// Edge/relay-internal perspective, which is the OPPOSITE of the bilbycast-manager
/// UI naming: the manager's "ingress node" (the media *source*) binds as
/// [`RelayDirection::Egress`] here. See `crate::tunnel::udp_forwarder`
/// (`run_egress` = sender / source, `run_ingress` = receiver / destination).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayDirection {
    /// Destination side: receives tunnel traffic and forwards it to a local consumer.
    #[serde(rename = "ingress")]
    Ingress,
    /// Source side: captures local traffic and sends it INTO the tunnel.
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

/// Result of resilient message parsing: either a known typed message or an
/// unknown type that was gracefully skipped (instead of tearing down the connection).
#[derive(Debug, Clone)]
pub enum ParsedMessage<T> {
    /// Successfully deserialized into the expected type.
    Known(T),
    /// The message had an unrecognized "type" tag. The connection stays alive.
    Unknown { msg_type: String },
}

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

/// Read a length-prefixed JSON message, gracefully handling unknown "type" variants.
///
/// If the message has a "type" tag that doesn't match any known variant of `T`,
/// returns `ParsedMessage::Unknown` instead of an error. This prevents unknown
/// message types (e.g., from a newer protocol version) from tearing down the
/// entire QUIC connection.
pub async fn read_message_resilient<T: serde::de::DeserializeOwned>(
    recv: &mut quinn::RecvStream,
) -> anyhow::Result<ParsedMessage<T>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("message too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;

    match serde_json::from_slice::<T>(&buf) {
        Ok(msg) => Ok(ParsedMessage::Known(msg)),
        Err(_) => {
            let msg_type = serde_json::from_slice::<serde_json::Value>(&buf)
                .ok()
                .and_then(|v| v.get("type")?.as_str().map(String::from))
                .unwrap_or_else(|| "unknown".into());
            Ok(ParsedMessage::Unknown { msg_type })
        }
    }
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

// ── Plain-UDP relay/direct data plane (native SRT/RIST over relay, no QUIC) ──
//
// The native path reuses the exact `[16-byte tunnel_id][AEAD payload]` data framing
// above (so an edge forwarder is transport-agnostic). It rides plain UDP datagrams to
// a relay's UDP listener (or directly to a peer) instead of QUIC datagrams.
//
// A small control plane is multiplexed onto the SAME UDP socket using the **all-zero
// (nil) UUID** as a reserved sentinel prefix: a datagram whose 16-byte prefix is nil
// carries a JSON [`UdpRelayControl`] message rather than media. Real tunnel IDs are
// random v4 UUIDs (validation rejects nil), so there is no collision.

/// Control messages for the plain-UDP relay/direct data plane.
///
/// Carried in a datagram prefixed with the nil UUID (see [`encode_udp_control`] /
/// [`try_decode_udp_control`]). The relay distinguishes these from media datagrams
/// purely by the nil prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UdpRelayControl {
    /// edge → relay (or → direct peer): register/keepalive this endpoint's
    /// `(tunnel_id, direction)` and prove authorization. Re-sent periodically so the
    /// relay learns and maintains this edge's post-NAT source address (rendezvous),
    /// and so a dead relay can be detected (no [`UdpRelayControl::Ack`] → failover).
    #[serde(rename = "register")]
    Register {
        tunnel_id: Uuid,
        direction: RelayDirection,
        /// HMAC-SHA256 bind token (relay mode) or PSK-derived token (direct mode).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind_token: Option<String>,
        #[serde(default)]
        protocol_version: u32,
    },

    /// relay → edge (or peer → edge): acknowledge a [`UdpRelayControl::Register`] and
    /// report whether both sides of the tunnel are now registered (media may flow).
    #[serde(rename = "ack")]
    Ack { tunnel_id: Uuid, ready: bool },
}

/// Encode a [`UdpRelayControl`] message as a control datagram: nil-UUID prefix + JSON.
pub fn encode_udp_control(msg: &UdpRelayControl) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)?;
    let mut buf = Vec::with_capacity(UDP_DATAGRAM_PREFIX_LEN + json.len());
    buf.extend_from_slice(Uuid::nil().as_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// If `data` is a control datagram (nil-UUID prefix), decode the [`UdpRelayControl`].
///
/// Returns `None` for media datagrams (real tunnel UUID prefix) or malformed input —
/// callers treat `None` as "not control, try the data path".
pub fn try_decode_udp_control(data: &[u8]) -> Option<UdpRelayControl> {
    let (id, payload) = decode_udp_datagram(data)?;
    if !id.is_nil() {
        return None;
    }
    serde_json::from_slice::<UdpRelayControl>(payload).ok()
}

#[cfg(test)]
mod udp_relay_tests {
    use super::*;

    #[test]
    fn register_control_roundtrips() {
        let tid = Uuid::new_v4();
        let msg = UdpRelayControl::Register {
            tunnel_id: tid,
            direction: RelayDirection::Egress,
            bind_token: Some("deadbeef".to_string()),
            protocol_version: TUNNEL_PROTOCOL_VERSION,
        };
        let bytes = encode_udp_control(&msg).unwrap();
        // Control datagrams carry the nil-UUID sentinel prefix.
        assert_eq!(&bytes[..16], Uuid::nil().as_bytes());
        match try_decode_udp_control(&bytes) {
            Some(UdpRelayControl::Register { tunnel_id, direction, bind_token, .. }) => {
                assert_eq!(tunnel_id, tid);
                assert_eq!(direction, RelayDirection::Egress);
                assert_eq!(bind_token.as_deref(), Some("deadbeef"));
            }
            other => panic!("expected Register, got {other:?}"),
        }
    }

    #[test]
    fn ack_control_roundtrips() {
        let tid = Uuid::new_v4();
        let bytes = encode_udp_control(&UdpRelayControl::Ack { tunnel_id: tid, ready: true }).unwrap();
        match try_decode_udp_control(&bytes) {
            Some(UdpRelayControl::Ack { tunnel_id, ready }) => {
                assert_eq!(tunnel_id, tid);
                assert!(ready);
            }
            other => panic!("expected Ack, got {other:?}"),
        }
    }

    #[test]
    fn media_datagram_is_not_control() {
        // A real tunnel UUID prefix + payload must NOT decode as control —
        // this is what keeps the control/data planes unambiguous on one socket.
        let tid = Uuid::new_v4();
        let dg = encode_udp_datagram(&tid, b"\x47\x40\x00\x10 some TS bytes");
        assert!(try_decode_udp_control(&dg).is_none());
        let (decoded_id, payload) = decode_udp_datagram(&dg).unwrap();
        assert_eq!(decoded_id, tid);
        assert_eq!(payload, b"\x47\x40\x00\x10 some TS bytes");
    }

    #[test]
    fn short_datagram_decodes_to_none() {
        assert!(try_decode_udp_control(b"short").is_none());
        assert!(decode_udp_datagram(b"short").is_none());
    }
}
