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

    /// On the control stream: the bind this answers was refused. On a stream
    /// of its own: the other side is gone ([`PEER_DISCONNECTED`],
    /// [`PEER_UNBOUND`]), or with any other reason the side of the edge this is
    /// sent to was removed, and takes a bind to hold again.
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

/// The reasons of a [`RelayMessage::TunnelDown`] that is about the other side of
/// the tunnel. The edge it is sent to still holds its own. Edges tell the two
/// apart by these, so they stay as every release has them.
pub const PEER_DISCONNECTED: &str = "peer disconnected";
/// See [`PEER_DISCONNECTED`].
pub const PEER_UNBOUND: &str = "peer unbound";

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
///
/// Every field added after the first release is optional and left off the wire
/// when unset, so each side still reads the other's older shape: a relay up to
/// v0.20.0 sends `{"type":"ack","tunnel_id":…,"ready":…}` and nothing else, and
/// an edge up to v0.117.0 sends a `Register` without `seq` or `standby`. Neither
/// enum denies unknown keys, so a newer shape decodes on an older build too.
/// [`TUNNEL_PROTOCOL_VERSION`] stays 2: no message type and no meaning changed.
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
        /// A per-socket counter: random at the socket's first Register, one up
        /// on each after. The relay echoes it in its [`UdpRelayControl::Ack`],
        /// which lets the edge time the round trip and refuse an Ack that
        /// answers nothing it sent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u32>,
        /// `true` when the socket this came on is not the one the edge sends
        /// its media on: it stands by on this relay, to meet its peer there or
        /// to return to it. The relay counts a pair of standing-by sides as
        /// standing by rather than active.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        standby: Option<bool>,
    },

    /// relay → edge (or peer → edge): acknowledge a [`UdpRelayControl::Register`] and
    /// report whether both sides of the tunnel are now registered (media may flow).
    ///
    /// `ready` keeps its first meaning: both slots have been latched. The
    /// liveness fields below are what a relay after v0.20.0 adds, and their
    /// presence is how an edge tells such a relay from an older one.
    #[serde(rename = "ack")]
    Ack {
        tunnel_id: Uuid,
        ready: bool,
        /// The other side of the tunnel was heard within 16 s: a Register
        /// accepted from its slot, or media forwarded from its address.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_live: Option<bool>,
        /// How long, in milliseconds, both sides have been heard without a
        /// break, as the relay saw it at Registers. 0 while `peer_live` is false.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        live_ms: Option<u64>,
        /// 32 lowercase hex characters, one value per relay process: two
        /// relay addresses that answer with the same id are one relay, and a
        /// new id is a restarted relay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_id: Option<String>,
        /// The `seq` of the Register this answers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u32>,
    },
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
            seq: Some(u32::MAX),
            standby: Some(true),
        };
        let bytes = encode_udp_control(&msg).unwrap();
        // Control datagrams carry the nil-UUID sentinel prefix.
        assert_eq!(&bytes[..16], Uuid::nil().as_bytes());
        match try_decode_udp_control(&bytes) {
            Some(UdpRelayControl::Register { tunnel_id, direction, bind_token, seq, standby, .. }) => {
                assert_eq!(tunnel_id, tid);
                assert_eq!(direction, RelayDirection::Egress);
                assert_eq!(bind_token.as_deref(), Some("deadbeef"));
                assert_eq!(seq, Some(u32::MAX));
                assert_eq!(standby, Some(true));
            }
            other => panic!("expected Register, got {other:?}"),
        }
    }

    #[test]
    fn ack_control_roundtrips() {
        let tid = Uuid::new_v4();
        let bytes = encode_udp_control(&UdpRelayControl::Ack {
            tunnel_id: tid,
            ready: true,
            peer_live: Some(false),
            live_ms: Some(604_800_000),
            relay_id: Some("0123456789abcdef0123456789abcdef".into()),
            seq: Some(7),
        })
        .unwrap();
        match try_decode_udp_control(&bytes) {
            Some(UdpRelayControl::Ack { tunnel_id, ready, peer_live, live_ms, relay_id, seq }) => {
                assert_eq!(tunnel_id, tid);
                assert!(ready);
                assert_eq!(peer_live, Some(false));
                assert_eq!(live_ms, Some(604_800_000));
                assert_eq!(relay_id.as_deref(), Some("0123456789abcdef0123456789abcdef"));
                assert_eq!(seq, Some(7));
            }
            other => panic!("expected Ack, got {other:?}"),
        }
    }

    /// A field nobody set stays off the wire, so an older peer reads exactly
    /// what it always did.
    #[test]
    fn unset_new_fields_are_not_sent() {
        let tid = Uuid::new_v4();
        let json = serde_json::to_value(UdpRelayControl::Ack {
            tunnel_id: tid,
            ready: true,
            peer_live: None,
            live_ms: None,
            relay_id: None,
            seq: None,
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({ "type": "ack", "tunnel_id": tid, "ready": true }));
        let json = serde_json::to_value(UdpRelayControl::Register {
            tunnel_id: tid,
            direction: RelayDirection::Ingress,
            bind_token: None,
            protocol_version: 2,
            seq: None,
            standby: None,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "register", "tunnel_id": tid, "direction": "ingress", "protocol_version": 2
            })
        );
    }

    /// What a relay up to v0.20.0 sends and what an edge up to v0.117.0 sends,
    /// spelled out literally.
    #[test]
    fn the_old_shapes_decode_with_every_new_field_unset() {
        let tid = Uuid::new_v4();
        let control = |json: String| {
            let mut bytes = Uuid::nil().as_bytes().to_vec();
            bytes.extend_from_slice(json.as_bytes());
            try_decode_udp_control(&bytes)
        };
        match control(format!(r#"{{"type":"ack","tunnel_id":"{tid}","ready":true}}"#)) {
            Some(UdpRelayControl::Ack { tunnel_id, ready, peer_live, live_ms, relay_id, seq }) => {
                assert_eq!(tunnel_id, tid);
                assert!(ready);
                assert_eq!((peer_live, live_ms, relay_id, seq), (None, None, None, None));
            }
            other => panic!("expected Ack, got {other:?}"),
        }
        let old = format!(
            r#"{{"type":"register","tunnel_id":"{tid}","direction":"egress","bind_token":"ab","protocol_version":2}}"#
        );
        match control(old) {
            Some(UdpRelayControl::Register { tunnel_id, seq, standby, .. }) => {
                assert_eq!(tunnel_id, tid);
                assert_eq!((seq, standby), (None, None));
            }
            other => panic!("expected Register, got {other:?}"),
        }
    }

    /// Neither enum denies unknown keys, so a key a later release adds does
    /// not make this one drop the message.
    #[test]
    fn a_new_shape_with_a_key_unknown_here_still_decodes() {
        let tid = Uuid::new_v4();
        let mut bytes = Uuid::nil().as_bytes().to_vec();
        bytes.extend_from_slice(
            format!(
                r#"{{"type":"ack","tunnel_id":"{tid}","ready":false,"peer_live":true,"live_ms":5,"relay_id":"ab","seq":9,"later":1}}"#
            )
            .as_bytes(),
        );
        match try_decode_udp_control(&bytes) {
            Some(UdpRelayControl::Ack { peer_live, live_ms, seq, .. }) => {
                assert_eq!((peer_live, live_ms, seq), (Some(true), Some(5), Some(9)));
            }
            other => panic!("expected Ack, got {other:?}"),
        }
        let mut bytes = Uuid::nil().as_bytes().to_vec();
        bytes.extend_from_slice(
            format!(
                r#"{{"type":"register","tunnel_id":"{tid}","direction":"egress","protocol_version":2,"seq":1,"standby":false,"later":"x"}}"#
            )
            .as_bytes(),
        );
        assert!(matches!(
            try_decode_udp_control(&bytes),
            Some(UdpRelayControl::Register { seq: Some(1), standby: Some(false), .. })
        ));
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
