// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

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
    #[serde(default)]
    pub accept_self_signed_cert: bool,
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
