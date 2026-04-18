// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Serialize};

/// Configuration for connecting to a bilbycast-manager instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerConfig {
    /// Enable manager connection.
    #[serde(default)]
    pub enabled: bool,
    /// Manager WebSocket URL, e.g. "wss://manager-host:8443/ws/node"
    pub url: String,
    /// Accept self-signed TLS certificates from the manager.
    /// Only enable this for development/testing. Default: false.
    /// Requires `BILBYCAST_ALLOW_INSECURE=1` env var as a safety guard.
    #[serde(default)]
    pub accept_self_signed_cert: bool,
    /// SHA-256 fingerprint of the manager's TLS certificate for certificate pinning.
    /// Format: hex-encoded, e.g. "ab:cd:ef:01:23:..." (64 hex chars with colons).
    /// When set, the edge will reject connections to any server presenting a different
    /// certificate, even if it has a valid CA signature. This protects against
    /// compromised CAs and targeted MITM attacks.
    /// On first connection, the server's fingerprint is logged so you can pin it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_fingerprint: Option<String>,
    /// One-time registration token (used on first connect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_token: Option<String>,
    /// Assigned node ID (set after registration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Assigned node secret (set after registration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_secret: Option<String>,
}
