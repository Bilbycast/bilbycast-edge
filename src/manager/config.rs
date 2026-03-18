use serde::{Deserialize, Serialize};

/// Configuration for connecting to a bilbycast-manager instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerConfig {
    /// Enable manager connection.
    #[serde(default)]
    pub enabled: bool,
    /// Manager WebSocket URL, e.g. "ws://manager-host:8443/ws/node"
    pub url: String,
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
