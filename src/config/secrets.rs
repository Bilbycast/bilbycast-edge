// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Infrastructure secrets — stored in `secrets.json`, never sent to the manager.
//!
//! This module defines the `SecretsConfig` struct and related types that hold
//! infrastructure credentials (node auth, tunnel keys, TLS/auth config). At
//! runtime, secrets are merged into the unified `AppConfig` in memory. On disk,
//! they live in a separate file with restrictive permissions.
//!
//! Flow-level user parameters (SRT passphrases, RTSP credentials, RTMP stream
//! keys, bearer tokens, HLS auth tokens) stay in `config.json` — they are not
//! treated as secrets. This ensures they are visible in the manager UI and
//! survive round-trip config updates.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::api::auth::AuthConfig;
use crate::config::models::{
    AppConfig, InputConfig, OutputConfig, TlsConfig,
};

/// Root secrets configuration, persisted to `secrets.json`.
///
/// Only infrastructure secrets live here: node credentials, server TLS/auth,
/// and tunnel encryption keys. Flow-level user parameters (SRT passphrases,
/// RTSP credentials, RTMP keys, bearer tokens) stay in `config.json` so they
/// are visible in the manager UI and survive round-trip config updates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretsConfig {
    /// Schema version for forward compatibility.
    #[serde(default = "default_version")]
    pub version: u32,

    // -- Node-level secrets --
    /// Manager authentication secret (assigned after registration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_node_secret: Option<String>,
    /// One-time registration token for initial manager registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_registration_token: Option<String>,

    /// Server TLS configuration (cert/key paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tls: Option<TlsConfig>,

    /// Server OAuth 2.0 auth configuration (jwt_secret, client credentials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_auth: Option<AuthConfig>,

    // -- Per-tunnel secrets, keyed by tunnel ID --
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tunnels: HashMap<String, TunnelSecrets>,

    // -- Legacy per-flow secrets (v1 migration only) --
    // Flow secrets were moved back to config.json in v2. This field exists
    // solely to deserialize old secrets.json files during migration.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub flows: HashMap<String, FlowSecrets>,
}

fn default_version() -> u32 {
    1
}

/// Secret fields for a single tunnel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TunnelSecrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_encryption_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_bind_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_psk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_pem: Option<String>,
}

/// Secret fields for a single flow (input + outputs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlowSecrets {
    /// Secrets for the flow's input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<InputSecrets>,
    /// Secrets for each output, keyed by output ID.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub outputs: HashMap<String, OutputSecrets>,
}

/// Secret fields that can appear on various input types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputSecrets {
    /// SRT passphrase (primary leg).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// SRT passphrase for redundancy leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy_passphrase: Option<String>,
    /// RTMP stream key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_key: Option<String>,
    /// RTSP username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// RTSP password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// WebRTC/WHIP/WHEP bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
}

/// Secret fields that can appear on various output types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputSecrets {
    /// SRT passphrase (primary leg).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// SRT passphrase for redundancy leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy_passphrase: Option<String>,
    /// RTMP stream key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_key: Option<String>,
    /// HLS auth token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// WebRTC bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
}

impl TunnelSecrets {
    fn is_empty(&self) -> bool {
        self.tunnel_encryption_key.is_none()
            && self.tunnel_bind_secret.is_none()
            && self.tunnel_psk.is_none()
            && self.tls_cert_pem.is_none()
            && self.tls_key_pem.is_none()
    }
}

// InputSecrets, OutputSecrets, FlowSecrets structs are retained for
// deserializing legacy secrets.json files during migration. No methods needed.

impl SecretsConfig {
    /// Returns true if there are any secrets stored.
    pub fn is_empty(&self) -> bool {
        self.manager_node_secret.is_none()
            && self.manager_registration_token.is_none()
            && self.server_tls.is_none()
            && self.server_auth.is_none()
            && self.tunnels.is_empty()
            && self.flows.is_empty()
    }

    /// Extract infrastructure secret fields from an `AppConfig` into a `SecretsConfig`.
    ///
    /// Flow-level parameters (SRT passphrases, RTSP credentials, RTMP keys, bearer
    /// tokens) are intentionally kept in `config.json` so they are visible in the
    /// manager UI and survive round-trip config updates.
    pub fn extract_from(config: &AppConfig) -> Self {
        let mut secrets = SecretsConfig {
            version: 1,
            ..Default::default()
        };

        // Manager secrets
        if let Some(ref mgr) = config.manager {
            secrets.manager_node_secret = mgr.node_secret.clone();
            secrets.manager_registration_token = mgr.registration_token.clone();
        }

        // Server TLS
        secrets.server_tls = config.server.tls.clone();

        // Server auth
        secrets.server_auth = config.server.auth.clone();

        // Tunnel secrets
        for tunnel in &config.tunnels {
            let ts = TunnelSecrets {
                tunnel_encryption_key: tunnel.tunnel_encryption_key.clone(),
                tunnel_bind_secret: tunnel.tunnel_bind_secret.clone(),
                tunnel_psk: tunnel.tunnel_psk.clone(),
                tls_cert_pem: tunnel.tls_cert_pem.clone(),
                tls_key_pem: tunnel.tls_key_pem.clone(),
            };
            if !ts.is_empty() {
                secrets.tunnels.insert(tunnel.id.clone(), ts);
            }
        }

        // Flow secrets are NOT extracted — they stay in config.json

        secrets
    }

    /// Merge secrets back into an `AppConfig`.
    ///
    /// Merges infrastructure secrets (node credentials, TLS, tunnel keys).
    /// Also handles legacy migration: if `secrets.json` has flow secrets from
    /// the old format (v1), those are merged back into config so they are
    /// preserved during the transition.
    pub fn merge_into(&self, config: &mut AppConfig) {
        // Manager secrets
        if let Some(ref mut mgr) = config.manager {
            if self.manager_node_secret.is_some() && mgr.node_secret.is_none() {
                mgr.node_secret = self.manager_node_secret.clone();
            }
            if self.manager_registration_token.is_some() && mgr.registration_token.is_none() {
                mgr.registration_token = self.manager_registration_token.clone();
            }
        }

        // Server TLS
        if self.server_tls.is_some() && config.server.tls.is_none() {
            config.server.tls = self.server_tls.clone();
        }

        // Server auth
        if self.server_auth.is_some() && config.server.auth.is_none() {
            config.server.auth = self.server_auth.clone();
        }

        // Tunnel secrets
        for tunnel in &mut config.tunnels {
            if let Some(ts) = self.tunnels.get(&tunnel.id) {
                if ts.tunnel_encryption_key.is_some() && tunnel.tunnel_encryption_key.is_none() {
                    tunnel.tunnel_encryption_key = ts.tunnel_encryption_key.clone();
                }
                if ts.tunnel_bind_secret.is_some() && tunnel.tunnel_bind_secret.is_none() {
                    tunnel.tunnel_bind_secret = ts.tunnel_bind_secret.clone();
                }
                if ts.tunnel_psk.is_some() && tunnel.tunnel_psk.is_none() {
                    tunnel.tunnel_psk = ts.tunnel_psk.clone();
                }
                if ts.tls_cert_pem.is_some() && tunnel.tls_cert_pem.is_none() {
                    tunnel.tls_cert_pem = ts.tls_cert_pem.clone();
                }
                if ts.tls_key_pem.is_some() && tunnel.tls_key_pem.is_none() {
                    tunnel.tls_key_pem = ts.tls_key_pem.clone();
                }
            }
        }

        // Legacy flow secrets migration: if old secrets.json had flow secrets,
        // merge them back into top-level inputs/outputs so they are not lost
        // during upgrade. In the new model, inputs and outputs live at
        // config.inputs / config.outputs (not inside flows).
        if !self.flows.is_empty() {
            tracing::info!(
                "Migrating {} flow secret(s) from secrets.json back to config.json",
                self.flows.len()
            );
            for (_flow_id, fs) in &self.flows {
                // Merge input secrets into top-level inputs
                if let Some(is) = &fs.input {
                    for input_def in &mut config.inputs {
                        Self::merge_input_secrets(is, &mut input_def.config);
                    }
                }
                // Merge output secrets into top-level outputs
                for output in &mut config.outputs {
                    if let Some(os) = fs.outputs.get(output.id()) {
                        Self::merge_output_secrets(os, output);
                    }
                }
            }
        }
    }

    fn merge_input_secrets(is: &InputSecrets, input: &mut InputConfig) {
        match input {
            InputConfig::Srt(srt) => {
                if is.passphrase.is_some() && srt.passphrase.is_none() {
                    srt.passphrase = is.passphrase.clone();
                }
                if is.redundancy_passphrase.is_some() {
                    if let Some(ref mut red) = srt.redundancy {
                        if red.passphrase.is_none() {
                            red.passphrase = is.redundancy_passphrase.clone();
                        }
                    }
                }
            }
            InputConfig::Rtmp(rtmp) => {
                if is.stream_key.is_some() && rtmp.stream_key.is_none() {
                    rtmp.stream_key = is.stream_key.clone();
                }
            }
            InputConfig::Rtsp(rtsp) => {
                if is.username.is_some() && rtsp.username.is_none() {
                    rtsp.username = is.username.clone();
                }
                if is.password.is_some() && rtsp.password.is_none() {
                    rtsp.password = is.password.clone();
                }
            }
            InputConfig::Webrtc(webrtc) => {
                if is.bearer_token.is_some() && webrtc.bearer_token.is_none() {
                    webrtc.bearer_token = is.bearer_token.clone();
                }
            }
            InputConfig::Whep(whep) => {
                if is.bearer_token.is_some() && whep.bearer_token.is_none() {
                    whep.bearer_token = is.bearer_token.clone();
                }
            }
            InputConfig::Rtp(_) | InputConfig::Udp(_) | InputConfig::Rist(_) => {}
            // SMPTE ST 2110 inputs carry no per-input secrets in Phase 1.
            InputConfig::St2110_30(_)
            | InputConfig::St2110_31(_)
            | InputConfig::St2110_40(_)
            | InputConfig::St2110_20(_)
            | InputConfig::St2110_23(_)
            | InputConfig::RtpAudio(_) => {}
            // Bonded inputs reference TLS material by on-disk path in
            // their `BondQuicTls::Pem` variant; the path strings
            // themselves aren't secrets (they're fine in config.json),
            // and the cert / key files are read lazily at path
            // construction. Nothing to merge at this layer.
            InputConfig::Bonded(_) => {}
        }
    }

    fn merge_output_secrets(os: &OutputSecrets, output: &mut OutputConfig) {
        match output {
            OutputConfig::Srt(srt) => {
                if os.passphrase.is_some() && srt.passphrase.is_none() {
                    srt.passphrase = os.passphrase.clone();
                }
                if os.redundancy_passphrase.is_some() {
                    if let Some(ref mut red) = srt.redundancy {
                        if red.passphrase.is_none() {
                            red.passphrase = os.redundancy_passphrase.clone();
                        }
                    }
                }
            }
            OutputConfig::Rtmp(rtmp) => {
                if let Some(ref key) = os.stream_key {
                    if rtmp.stream_key.is_empty() {
                        rtmp.stream_key = key.clone();
                    }
                }
            }
            OutputConfig::Hls(hls) => {
                if os.auth_token.is_some() && hls.auth_token.is_none() {
                    hls.auth_token = os.auth_token.clone();
                }
            }
            OutputConfig::Cmaf(cmaf) => {
                if os.auth_token.is_some() && cmaf.auth_token.is_none() {
                    cmaf.auth_token = os.auth_token.clone();
                }
            }
            OutputConfig::Webrtc(webrtc) => {
                if os.bearer_token.is_some() && webrtc.bearer_token.is_none() {
                    webrtc.bearer_token = os.bearer_token.clone();
                }
            }
            OutputConfig::Rtp(_) | OutputConfig::Udp(_) | OutputConfig::Rist(_) => {}
            // SMPTE ST 2110 outputs carry no per-output secrets in Phase 1.
            OutputConfig::St2110_30(_)
            | OutputConfig::St2110_31(_)
            | OutputConfig::St2110_40(_)
            | OutputConfig::St2110_20(_)
            | OutputConfig::St2110_23(_)
            | OutputConfig::RtpAudio(_) => {}
            // Bonded outputs: see note on the input side — TLS
            // material references paths, not inline secrets.
            OutputConfig::Bonded(_) => {}
        }
    }
}

impl AppConfig {
    /// Remove infrastructure secret fields from this config, making it safe to send
    /// to the manager.
    ///
    /// After calling this, node credentials, server TLS/auth, and tunnel encryption
    /// keys are removed. Flow-level user parameters (SRT passphrases, RTSP credentials,
    /// RTMP keys, bearer tokens) are preserved so they remain visible in the manager UI.
    pub fn strip_secrets(&mut self) {
        // Manager secrets
        if let Some(ref mut mgr) = self.manager {
            mgr.node_secret = None;
            mgr.registration_token = None;
        }

        // Server TLS and auth
        self.server.tls = None;
        self.server.auth = None;

        // Tunnel secrets
        for tunnel in &mut self.tunnels {
            tunnel.tunnel_encryption_key = None;
            tunnel.tunnel_bind_secret = None;
            tunnel.tunnel_psk = None;
            tunnel.tls_cert_pem = None;
            tunnel.tls_key_pem = None;
        }

        // Flow parameters are NOT stripped — they are user-configured values
        // that need to be visible in the manager UI and survive round-trip updates.
    }
}

impl AppConfig {
    /// Strip infrastructure secrets for local API display.
    ///
    /// Internal secrets (manager credentials, server TLS/auth, tunnel keys)
    /// are set to `None` — they are never relevant to the user's API view.
    /// Flow-level parameters are preserved in full — users need to see their
    /// configured passphrases, credentials, and keys.
    pub fn mask_secrets(&mut self) {
        // Manager secrets — internal, always hide
        if let Some(ref mut mgr) = self.manager {
            mgr.node_secret = None;
            mgr.registration_token = None;
        }

        // Server TLS and auth — internal
        self.server.tls = None;
        self.server.auth = None;

        // Tunnel secrets — internal
        for tunnel in &mut self.tunnels {
            tunnel.tunnel_encryption_key = None;
            tunnel.tunnel_bind_secret = None;
            tunnel.tunnel_psk = None;
            tunnel.tls_cert_pem = None;
            tunnel.tls_key_pem = None;
        }

        // Flow parameters are NOT masked — users need to see their configured
        // passphrases, credentials, and keys in full.
    }
}

/// Returns true if the given `AppConfig` contains any infrastructure secrets
/// that should be split out into `secrets.json`. Used for migration detection.
///
/// Flow-level parameters (passphrases, credentials, keys) are NOT considered
/// secrets for this purpose — they stay in `config.json`.
pub fn has_secrets(config: &AppConfig) -> bool {
    // Manager secrets
    if let Some(ref mgr) = config.manager {
        if mgr.node_secret.is_some() || mgr.registration_token.is_some() {
            return true;
        }
    }

    // Server TLS/auth
    if config.server.tls.is_some() || config.server.auth.is_some() {
        return true;
    }

    // Tunnel secrets
    for tunnel in &config.tunnels {
        if tunnel.tunnel_encryption_key.is_some()
            || tunnel.tunnel_bind_secret.is_some()
            || tunnel.tunnel_psk.is_some()
            || tunnel.tls_cert_pem.is_some()
            || tunnel.tls_key_pem.is_some()
        {
            return true;
        }
    }

    // Flow parameters are NOT secrets — they stay in config.json
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::*;
    use crate::manager::ManagerConfig;
    use crate::tunnel::TunnelConfig;

    fn srt_input_config(passphrase: Option<&str>) -> SrtInputConfig {
        SrtInputConfig {
            mode: SrtMode::Listener,
            local_addr: Some("0.0.0.0:9000".to_string()),
            remote_addr: None,
            latency_ms: 120,
            recv_latency_ms: None,
            peer_latency_ms: None,
            peer_idle_timeout_secs: 30,
            passphrase: passphrase.map(|s| s.to_string()),
            aes_key_len: None,
            crypto_mode: None,
            max_rexmit_bw: None,
            stream_id: None,
            packet_filter: None,
            max_bw: None,
            input_bw: None,
            overhead_bw: None,
            enforced_encryption: None,
            connect_timeout_secs: None,
            flight_flag_size: None,
            send_buffer_size: None,
            recv_buffer_size: None,
            ip_tos: None,
            retransmit_algo: None,
            send_drop_delay: None,
            loss_max_ttl: None,
            km_refresh_rate: None,
            km_pre_announce: None,
            payload_size: None,
            mss: None,
            tlpkt_drop: None,
            ip_ttl: None,
            redundancy: None,
            transport_mode: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }
    }

    #[test]
    fn test_extract_and_merge_roundtrip() {
        let config = AppConfig {
            version: 1,
            node_id: Some("test-node".to_string()),
            device_name: Some("Test".to_string()),
            setup_enabled: true,
            server: ServerConfig::default(),
            monitor: None,
            manager: Some(ManagerConfig {
                enabled: true,
                url: "wss://manager:8443/ws/node".to_string(),
                accept_self_signed_cert: false,
                cert_fingerprint: None,
                registration_token: Some("reg-token".to_string()),
                node_id: Some("node-1".to_string()),
                node_secret: Some("secret-123".to_string()),
            }),
            inputs: vec![InputDefinition {
                active: true,
                group: None,
                id: "srt-in".to_string(),
                name: "SRT Input".to_string(),
                config: InputConfig::Srt(srt_input_config(Some("my-secret-pass"))),
            }],
            outputs: vec![],
            flows: vec![FlowConfig {
                id: "srt-flow".to_string(),
                name: "SRT Flow".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["srt-in".to_string()],
                output_ids: vec![],
            }],
            tunnels: vec![TunnelConfig {
                id: "tunnel-1".to_string(),
                name: "Test Tunnel".to_string(),
                enabled: true,
                protocol: crate::tunnel::config::TunnelProtocol::Udp,
                mode: crate::tunnel::config::TunnelMode::Relay,
                direction: crate::tunnel::config::TunnelDirection::Ingress,
                local_addr: "127.0.0.1:9000".to_string(),
                relay_addrs: vec!["relay:4433".to_string()],
                relay_addr: None,
                max_rtt_failback_increase_ms: None,
                tunnel_encryption_key: Some("a".repeat(64)),
                tunnel_bind_secret: Some("b".repeat(64)),
                tunnel_psk: None,
                peer_addr: None,
                direct_listen_addr: None,
                tls_cert_pem: None,
                tls_key_pem: None,
            }],
            resource_limits: None,
            flow_groups: vec![],
        };

        // Extract secrets — only infrastructure secrets, not flow params
        let secrets = SecretsConfig::extract_from(&config);
        assert_eq!(secrets.manager_node_secret, Some("secret-123".to_string()));
        assert_eq!(
            secrets.manager_registration_token,
            Some("reg-token".to_string())
        );
        assert!(secrets.tunnels.contains_key("tunnel-1"));
        assert_eq!(
            secrets.tunnels["tunnel-1"].tunnel_encryption_key,
            Some("a".repeat(64))
        );
        // Flow secrets should NOT be extracted
        assert!(secrets.flows.is_empty());

        // Strip secrets from config — flow params should be preserved
        let mut stripped = config.clone();
        stripped.strip_secrets();
        assert!(stripped.manager.as_ref().unwrap().node_secret.is_none());
        assert!(stripped
            .manager
            .as_ref()
            .unwrap()
            .registration_token
            .is_none());
        assert!(stripped.tunnels[0].tunnel_encryption_key.is_none());
        assert!(stripped.tunnels[0].tunnel_bind_secret.is_none());
        // SRT passphrase should still be present in top-level input after strip_secrets
        match &stripped.inputs[0].config {
            InputConfig::Srt(srt) => {
                assert_eq!(srt.passphrase, Some("my-secret-pass".to_string()));
            }
            _ => panic!("Expected SRT input"),
        }

        // Merge secrets back into stripped config
        secrets.merge_into(&mut stripped);
        assert_eq!(
            stripped.manager.as_ref().unwrap().node_secret,
            Some("secret-123".to_string())
        );
        assert_eq!(
            stripped.tunnels[0].tunnel_encryption_key,
            Some("a".repeat(64))
        );
    }

    #[test]
    fn test_has_secrets_empty_config() {
        let config = AppConfig::default();
        assert!(!has_secrets(&config));
    }

    #[test]
    fn test_has_secrets_with_manager_secret() {
        let mut config = AppConfig::default();
        config.manager = Some(ManagerConfig {
            enabled: true,
            url: "wss://manager:8443/ws/node".to_string(),
            accept_self_signed_cert: false,
            cert_fingerprint: None,
            registration_token: None,
            node_id: None,
            node_secret: Some("secret".to_string()),
        });
        assert!(has_secrets(&config));
    }

    #[test]
    fn test_has_secrets_ignores_flow_params() {
        // Flow parameters (SRT passphrase, RTMP key, etc.) are NOT secrets —
        // they live in top-level inputs/outputs, not in infrastructure secrets
        let config = AppConfig {
            inputs: vec![InputDefinition {
                active: true,
                group: None,
                id: "srt-in".to_string(),
                name: "SRT Input".to_string(),
                config: InputConfig::Srt(srt_input_config(Some("my-secret-pass"))),
            }],
            flows: vec![FlowConfig {
                id: "srt-flow".to_string(),
                name: "SRT Flow".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["srt-in".to_string()],
                output_ids: vec![],
            }],
            ..Default::default()
        };
        // Config with only flow params should NOT be considered as having secrets
        assert!(!has_secrets(&config));
    }

    #[test]
    fn test_strip_secrets_preserves_flow_params() {
        let mut config = AppConfig {
            inputs: vec![InputDefinition {
                active: true,
                group: None,
                id: "rtp-in".to_string(),
                name: "RTP Input".to_string(),
                config: InputConfig::Rtp(RtpInputConfig {
                    bind_addr: "0.0.0.0:5000".to_string(),
                    interface_addr: None,
                    fec_decode: None,
                    allowed_sources: None,
                    allowed_payload_types: None,
                    max_bitrate_mbps: None,
                    tr07_mode: None,
                    redundancy: None,
                    audio_encode: None,
                    transcode: None,
                    video_encode: None,
                }),
            }],
            outputs: vec![OutputConfig::Rtmp(RtmpOutputConfig {
                active: true,
                group: None,
                id: "rtmp-out".to_string(),
                name: "RTMP Out".to_string(),
                dest_url: "rtmp://live.twitch.tv/app".to_string(),
                stream_key: "my-stream-key".to_string(),
                reconnect_delay_secs: 5,
                max_reconnect_attempts: None,
                program_number: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            })],
            flows: vec![FlowConfig {
                id: "rtmp-flow".to_string(),
                name: "RTMP Flow".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["rtp-in".to_string()],
                output_ids: vec!["rtmp-out".to_string()],
            }],
            ..Default::default()
        };

        // strip_secrets should preserve flow params in top-level outputs
        config.strip_secrets();
        match &config.outputs[0] {
            OutputConfig::Rtmp(rtmp) => assert_eq!(rtmp.stream_key, "my-stream-key"),
            _ => panic!("Expected RTMP output"),
        }
    }

    #[test]
    fn test_legacy_flow_secrets_migration() {
        // Simulate old secrets.json with flow secrets
        let mut legacy_secrets = SecretsConfig::default();
        legacy_secrets.flows.insert(
            "srt-flow".to_string(),
            FlowSecrets {
                input: Some(InputSecrets {
                    passphrase: Some("legacy-pass".to_string()),
                    ..Default::default()
                }),
                outputs: HashMap::new(),
            },
        );

        // Config without the passphrase (as it was stripped in old format).
        // In the new model, inputs are top-level.
        let mut config = AppConfig {
            inputs: vec![InputDefinition {
                active: true,
                group: None,
                id: "srt-in".to_string(),
                name: "SRT Input".to_string(),
                config: InputConfig::Srt(srt_input_config(None)),
            }],
            flows: vec![FlowConfig {
                id: "srt-flow".to_string(),
                name: "SRT Flow".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["srt-in".to_string()],
                output_ids: vec![],
            }],
            ..Default::default()
        };

        // Legacy merge should restore flow secrets into top-level inputs
        legacy_secrets.merge_into(&mut config);
        match &config.inputs[0].config {
            InputConfig::Srt(srt) => {
                assert_eq!(srt.passphrase, Some("legacy-pass".to_string()));
            }
            _ => panic!("Expected SRT input"),
        }
    }
}
