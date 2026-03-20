//! Tunnel configuration models.

use serde::{Deserialize, Serialize};

/// Configuration for a single IP tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelConfig {
    /// Unique tunnel identifier (UUID).
    pub id: String,
    /// Human-readable tunnel name.
    pub name: String,
    /// Whether this tunnel is active.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Transport protocol for tunneled data.
    pub protocol: TunnelProtocol,
    /// Tunnel connectivity mode.
    pub mode: TunnelMode,
    /// This edge's role in the tunnel.
    pub direction: TunnelDirection,

    /// Local address.
    /// - For **egress** (sending side): the address to listen on for incoming
    ///   local traffic that will be tunneled. E.g. `"0.0.0.0:9000"`.
    /// - For **ingress** (receiving side): the address to forward received
    ///   tunnel traffic to. E.g. `"127.0.0.1:9000"`.
    pub local_addr: String,

    // ── Relay mode fields ──
    /// Relay server QUIC address, e.g. `"relay.example.com:4433"`.
    /// Required when `mode` is `relay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_addr: Option<String>,

    /// This edge's identity for relay authentication.
    /// The relay verifies `HMAC-SHA256(edge_id, shared_secret)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_edge_id: Option<String>,

    /// Shared secret for relay HMAC-SHA256 authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_secret: Option<String>,

    // ── Direct mode fields ──
    /// Remote peer QUIC address for direct mode, e.g. `"203.0.113.50:4433"`.
    /// Required when `mode` is `direct` and `direction` is `egress` (connecting side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_addr: Option<String>,

    /// QUIC listen address for direct mode, e.g. `"0.0.0.0:4433"`.
    /// Required when `mode` is `direct` and `direction` is `ingress` (listening side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_listen_addr: Option<String>,

    /// Pre-shared key for direct mode HMAC-SHA256 authentication.
    /// Both edges must have the same PSK (distributed by the manager).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_psk: Option<String>,

    /// TLS certificate PEM for direct mode listener (auto-generated if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_pem: Option<String>,

    /// TLS key PEM for direct mode listener (auto-generated if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_pem: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Transport protocol for tunneled data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunnelProtocol {
    /// Reliable, ordered (QUIC streams). Good for camera control, signaling.
    #[serde(rename = "tcp")]
    Tcp,
    /// Unreliable, unordered (QUIC datagrams). Good for SRT, media.
    #[serde(rename = "udp")]
    Udp,
}

/// Tunnel connectivity mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunnelMode {
    /// Both edges behind NAT — traffic flows through a bilbycast-relay server.
    #[serde(rename = "relay")]
    Relay,
    /// One edge has a public IP — direct QUIC connection between edges.
    #[serde(rename = "direct")]
    Direct,
}

/// This edge's role in the tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunnelDirection {
    /// Ingress: receives traffic from relay/peer and forwards to a local address.
    #[serde(rename = "ingress")]
    Ingress,
    /// Egress: captures local traffic and sends through relay/peer.
    #[serde(rename = "egress")]
    Egress,
}

impl std::fmt::Display for TunnelProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(f, "tcp"),
            Self::Udp => write!(f, "udp"),
        }
    }
}

impl std::fmt::Display for TunnelMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Relay => write!(f, "relay"),
            Self::Direct => write!(f, "direct"),
        }
    }
}

impl std::fmt::Display for TunnelDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ingress => write!(f, "ingress"),
            Self::Egress => write!(f, "egress"),
        }
    }
}
