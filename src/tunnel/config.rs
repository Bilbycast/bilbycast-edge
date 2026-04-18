// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tunnel configuration models.

use serde::{Deserialize, Serialize};

/// Configuration for a single IP tunnel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Ordered list of relay server QUIC addresses. Index 0 is the primary;
    /// a second entry (if present) is a backup used for automatic failover.
    /// Required when `mode` is `relay`. At most 2 entries are accepted.
    ///
    /// The legacy single-field `relay_addr` is still accepted on input and is
    /// migrated into `relay_addrs[0]` at config load. Always read via
    /// [`TunnelConfig::effective_relays`] / [`TunnelConfig::primary_relay_addr`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relay_addrs: Vec<String>,

    /// Legacy single-relay address. Accepted on read for backward compatibility;
    /// migrated into `relay_addrs[0]` at load time. New configs should use
    /// `relay_addrs` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_addr: Option<String>,

    /// Maximum acceptable RTT increase (in milliseconds) when failing back from
    /// the backup relay to the primary. If the primary's measured RTT exceeds
    /// the active backup's RTT by more than this value, failback is refused and
    /// retried later. Only meaningful when a backup relay is configured.
    /// Default: 50ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rtt_failback_increase_ms: Option<u32>,

    // ── End-to-end encryption ──
    /// Symmetric encryption key for end-to-end tunnel encryption (hex-encoded, 64 chars = 32 bytes).
    /// Both edges must have the same key. Distributed by the manager.
    /// Required for relay mode (relay sees only ciphertext).
    /// Optional for direct mode (QUIC already encrypts, this adds defense-in-depth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_encryption_key: Option<String>,

    /// Shared secret for relay tunnel bind authentication (hex-encoded, 64 chars = 32 bytes).
    /// Used to compute HMAC-SHA256 bind tokens that prove this edge is authorized to bind
    /// a specific tunnel on the relay. Distributed by the manager alongside the encryption key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_bind_secret: Option<String>,

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

/// Default RTT gate (ms) used when failing back to the primary relay.
/// If the primary's RTT exceeds the active backup's RTT by more than this,
/// failback is refused.
pub const DEFAULT_FAILBACK_RTT_GATE_MS: u32 = 50;

/// Maximum number of relay addresses supported per tunnel (primary + 1 backup).
pub const MAX_RELAY_ADDRS: usize = 2;

#[allow(dead_code)] // public helpers consumed by status/API code added incrementally
impl TunnelConfig {
    /// Returns the canonical ordered relay address list.
    ///
    /// If `relay_addrs` is non-empty it wins; otherwise the legacy single
    /// `relay_addr` field is returned as a one-element slice. The result is
    /// empty only when neither is set (a validation error for relay mode).
    pub fn effective_relays(&self) -> Vec<&str> {
        if !self.relay_addrs.is_empty() {
            self.relay_addrs.iter().map(String::as_str).collect()
        } else {
            self.relay_addr.iter().map(String::as_str).collect()
        }
    }

    /// Convenience accessor for the primary relay address (index 0).
    pub fn primary_relay_addr(&self) -> Option<&str> {
        self.effective_relays().into_iter().next()
    }

    /// Number of relays configured (0, 1, or 2).
    pub fn relay_count(&self) -> usize {
        self.effective_relays().len()
    }

    /// Migrate the legacy `relay_addr` field into `relay_addrs[0]` when
    /// `relay_addrs` is empty. Idempotent. Call once after deserialization.
    pub fn normalize_relay_addrs(&mut self) {
        if self.relay_addrs.is_empty()
            && let Some(addr) = self.relay_addr.take()
        {
            self.relay_addrs.push(addr);
        }
    }

    /// Returns the failback RTT gate in milliseconds, defaulting to
    /// [`DEFAULT_FAILBACK_RTT_GATE_MS`].
    pub fn failback_rtt_gate_ms(&self) -> u32 {
        self.max_rtt_failback_increase_ms
            .unwrap_or(DEFAULT_FAILBACK_RTT_GATE_MS)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_relay_tunnel() -> TunnelConfig {
        TunnelConfig {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
            name: "t".to_string(),
            enabled: true,
            protocol: TunnelProtocol::Udp,
            mode: TunnelMode::Relay,
            direction: TunnelDirection::Ingress,
            local_addr: "127.0.0.1:9000".to_string(),
            relay_addrs: vec![],
            relay_addr: None,
            max_rtt_failback_increase_ms: None,
            tunnel_encryption_key: None,
            tunnel_bind_secret: None,
            peer_addr: None,
            direct_listen_addr: None,
            tunnel_psk: None,
            tls_cert_pem: None,
            tls_key_pem: None,
        }
    }

    #[test]
    fn effective_relays_prefers_relay_addrs_over_legacy_field() {
        let mut t = make_relay_tunnel();
        t.relay_addrs = vec!["a:1".to_string(), "b:2".to_string()];
        t.relay_addr = Some("c:3".to_string());
        assert_eq!(t.effective_relays(), vec!["a:1", "b:2"]);
        assert_eq!(t.primary_relay_addr(), Some("a:1"));
        assert_eq!(t.relay_count(), 2);
    }

    #[test]
    fn effective_relays_falls_back_to_legacy_field() {
        let mut t = make_relay_tunnel();
        t.relay_addr = Some("legacy:1".to_string());
        assert_eq!(t.effective_relays(), vec!["legacy:1"]);
        assert_eq!(t.primary_relay_addr(), Some("legacy:1"));
    }

    #[test]
    fn effective_relays_empty_when_nothing_set() {
        let t = make_relay_tunnel();
        assert!(t.effective_relays().is_empty());
        assert_eq!(t.primary_relay_addr(), None);
        assert_eq!(t.relay_count(), 0);
    }

    #[test]
    fn normalize_moves_legacy_field_into_list() {
        let mut t = make_relay_tunnel();
        t.relay_addr = Some("legacy:1".to_string());
        t.normalize_relay_addrs();
        assert_eq!(t.relay_addrs, vec!["legacy:1".to_string()]);
        assert!(t.relay_addr.is_none());
    }

    #[test]
    fn normalize_is_idempotent_when_list_already_populated() {
        let mut t = make_relay_tunnel();
        t.relay_addrs = vec!["a:1".to_string()];
        t.relay_addr = Some("legacy:1".to_string());
        t.normalize_relay_addrs();
        assert_eq!(t.relay_addrs, vec!["a:1".to_string()]);
        // legacy field is left alone; effective_relays prefers the list anyway
        assert_eq!(t.relay_addr.as_deref(), Some("legacy:1"));
    }

    #[test]
    fn failback_rtt_gate_defaults_when_unset() {
        let t = make_relay_tunnel();
        assert_eq!(t.failback_rtt_gate_ms(), DEFAULT_FAILBACK_RTT_GATE_MS);
    }

    #[test]
    fn failback_rtt_gate_respects_config() {
        let mut t = make_relay_tunnel();
        t.max_rtt_failback_increase_ms = Some(123);
        assert_eq!(t.failback_rtt_gate_ms(), 123);
    }
}
