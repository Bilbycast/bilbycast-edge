// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Manager WebSocket client.
//!
//! Maintains a persistent outbound WebSocket connection to the manager,
//! forwarding stats and events, and executing commands received from the manager.
//!
//! Authentication is done via an "auth" message sent as the first WebSocket frame
//! after connecting (not via query parameters, to avoid leaking secrets in URLs/logs).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;

use super::events::{Event, EventSeverity, build_event_envelope, category};

use crate::config::models::{AppConfig, FlowAssembly, FlowConfig, FlowGroupConfig, InputDefinition, OutputConfig, ResolvedFlow};
use crate::config::persistence::save_config_split_async;
use crate::config::secrets::SecretsConfig;
use crate::config::validation::{validate_config, validate_flow, validate_input_definition, validate_output, validate_tunnel};
use crate::engine::manager::FlowManager;
use crate::engine::resource_monitor::SystemResourceState;
use crate::tunnel::TunnelConfig;
use crate::tunnel::manager::TunnelManager;

use super::ManagerConfig;
#[cfg(feature = "webrtc")]
use crate::engine::flow::FlowRuntime;

#[cfg(feature = "webrtc")]
type WebrtcRegistry = Option<Arc<crate::api::webrtc::registry::WebrtcSessionRegistry>>;
#[cfg(not(feature = "webrtc"))]
type WebrtcRegistry = ();

/// Register a WHIP input channel with the WebRTC session registry (if applicable).
#[cfg(feature = "webrtc")]
fn register_whip_if_needed(
    webrtc_sessions: &WebrtcRegistry,
    runtime: &FlowRuntime,
) {
    if let Some((tx, bearer_token)) = &runtime.whip_session_tx {
        if let Some(registry) = webrtc_sessions {
            registry.register_whip_input(&runtime.config.config.id, tx.clone(), bearer_token.clone());
            tracing::info!("Registered WHIP input for flow '{}'", runtime.config.config.id);
        }
    }
}

/// Register a WHEP output channel with the WebRTC session registry (if applicable).
#[cfg(feature = "webrtc")]
fn register_whep_if_needed(
    webrtc_sessions: &WebrtcRegistry,
    runtime: &FlowRuntime,
) {
    if let Some((tx, bearer_token)) = &runtime.whep_session_tx {
        if let Some(registry) = webrtc_sessions {
            registry.register_whep_output(&runtime.config.config.id, tx.clone(), bearer_token.clone());
            tracing::info!("Registered WHEP output for flow '{}'", runtime.config.config.id);
        }
    }
}

/// Compute SHA-256 fingerprint of a DER-encoded certificate.
/// Returns colon-separated hex string, e.g. "ab:cd:ef:01:23:...".
fn compute_cert_fingerprint(cert_der: &[u8]) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(cert_der);
    hash.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Certificate verifier that accepts any certificate (for self-signed cert support).
#[derive(Debug)]
struct InsecureCertVerifier;

impl rustls::client::danger::ServerCertVerifier for InsecureCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Certificate verifier with fingerprint pinning.
///
/// Performs standard TLS certificate validation (CA chain, hostname, etc.),
/// then additionally checks that the server certificate's SHA-256 fingerprint
/// matches the configured pin. Protects against compromised CAs.
#[derive(Debug)]
struct PinnedCertVerifier {
    /// The expected SHA-256 fingerprint (colon-separated hex).
    expected_fingerprint: String,
    /// The standard webpki-based verifier for CA chain validation.
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // First, perform standard CA chain validation
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;

        // Then check the fingerprint pin
        let actual = compute_cert_fingerprint(end_entity.as_ref());
        if actual != self.expected_fingerprint {
            tracing::error!(
                "Certificate fingerprint mismatch! Expected: {}, got: {}. \
                 This could indicate a man-in-the-middle attack or a legitimate \
                 certificate rotation. Update cert_fingerprint in the manager config \
                 if the certificate was intentionally changed.",
                self.expected_fingerprint,
                actual
            );
            return Err(rustls::Error::General(format!(
                "Certificate fingerprint mismatch: expected {}, got {}",
                self.expected_fingerprint, actual
            )));
        }

        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Start the manager client background task.
pub fn start_manager_client(
    config: ManagerConfig,
    flow_manager: Arc<FlowManager>,
    tunnel_manager: Arc<TunnelManager>,
    ws_stats_rx: broadcast::Sender<String>,
    app_config: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
    secrets_path: PathBuf,
    api_addr: String,
    monitor_addr: Option<String>,
    webrtc_sessions: WebrtcRegistry,
    event_rx: mpsc::Receiver<Event>,
    resource_state: Arc<SystemResourceState>,
    standby_listeners: Option<Arc<crate::engine::standby_listeners::StandbyListenerManager>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        manager_client_loop(
            config, flow_manager, tunnel_manager, ws_stats_rx, app_config, config_path,
            secrets_path, api_addr, monitor_addr, webrtc_sessions, event_rx, resource_state,
            standby_listeners,
        ).await;
    })
}

async fn manager_client_loop(
    mut config: ManagerConfig,
    flow_manager: Arc<FlowManager>,
    tunnel_manager: Arc<TunnelManager>,
    ws_stats_tx: broadcast::Sender<String>,
    app_config: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
    secrets_path: PathBuf,
    api_addr: String,
    monitor_addr: Option<String>,
    webrtc_sessions: WebrtcRegistry,
    mut event_rx: mpsc::Receiver<Event>,
    resource_state: Arc<SystemResourceState>,
    standby_listeners: Option<Arc<crate::engine::standby_listeners::StandbyListenerManager>>,
) {
    // If we already have a node_id from config, set it on the tunnel manager
    // so relay tunnels can identify this edge before the first manager connection.
    if let Some(ref node_id) = config.node_id {
        tunnel_manager.set_manager_node_id(node_id.clone());
    }

    // Multi-URL failover (scale-out): 1..16 URLs in config.urls. We
    // rotate on every close/failure with a fixed 5 s backoff that
    // resets on successful auth. The cursor advances regardless of
    // outcome so a flapping primary does not starve the standbys.
    let fixed_backoff = Duration::from_secs(5);
    let mut cursor: usize = 0;

    loop {
        if config.urls.is_empty() {
            tracing::error!("Manager client started with no URLs — config.urls is empty");
            tokio::time::sleep(fixed_backoff).await;
            continue;
        }
        let current_url = config.urls[cursor % config.urls.len()].clone();
        tracing::info!(
            "Connecting to manager at {current_url} (url {} of {})",
            (cursor % config.urls.len()) + 1,
            config.urls.len(),
        );

        match try_connect(
            &current_url,
            &config,
            &flow_manager,
            &tunnel_manager,
            &ws_stats_tx,
            &app_config,
            &config_path,
            &secrets_path,
            &api_addr,
            monitor_addr.as_deref(),
            &webrtc_sessions,
            &mut event_rx,
            &resource_state,
            &standby_listeners,
        )
        .await
        {
            Ok(ConnectResult::Closed) => {
                tracing::info!("Manager connection to {current_url} closed normally");
            }
            Ok(ConnectResult::Registered {
                node_id,
                node_secret,
            }) => {
                tracing::info!(
                    "Registered with manager as node_id={node_id}, persisting credentials"
                );
                config.registration_token = None;
                config.node_id = Some(node_id.clone());
                config.node_secret = Some(node_secret.clone());
                tunnel_manager.set_manager_node_id(node_id.clone());

                // Persist to config + secrets files
                persist_credentials(&app_config, &config_path, &secrets_path, &node_id, &node_secret).await;
            }
            Err(e) => {
                tracing::warn!("Manager connection to {current_url} failed: {e}");
                tunnel_manager.event_sender().emit(
                    EventSeverity::Warning,
                    category::MANAGER,
                    "Manager connection lost, rotating to next URL",
                );
            }
        }

        cursor = cursor.wrapping_add(1);
        tracing::info!(
            "Reconnecting to next manager URL in {}s...",
            fixed_backoff.as_secs()
        );
        tokio::time::sleep(fixed_backoff).await;
    }
}

enum ConnectResult {
    Closed,
    Registered {
        node_id: String,
        node_secret: String,
    },
}

async fn try_connect(
    current_url: &str,
    config: &ManagerConfig,
    flow_manager: &Arc<FlowManager>,
    tunnel_manager: &Arc<TunnelManager>,
    ws_stats_tx: &broadcast::Sender<String>,
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
    secrets_path: &PathBuf,
    api_addr: &str,
    monitor_addr: Option<&str>,
    webrtc_sessions: &WebrtcRegistry,
    event_rx: &mut mpsc::Receiver<Event>,
    resource_state: &Arc<SystemResourceState>,
    standby_listeners: &Option<Arc<crate::engine::standby_listeners::StandbyListenerManager>>,
) -> Result<ConnectResult, String> {
    // Enforce TLS — only wss:// connections are allowed
    if !current_url.starts_with("wss://") {
        return Err(
            "Manager URL must use wss:// (TLS). Plaintext ws:// connections are not allowed."
                .into(),
        );
    }

    let (ws_stream, _response) = if config.accept_self_signed_cert {
        // SECURITY: Require explicit env var to allow insecure connections.
        // This prevents accidental use in production when left in config files.
        if std::env::var("BILBYCAST_ALLOW_INSECURE").as_deref() != Ok("1") {
            return Err(
                "accept_self_signed_cert is enabled but BILBYCAST_ALLOW_INSECURE=1 is not set. \
                 This is a security safeguard — self-signed cert mode disables ALL certificate \
                 validation, making the connection vulnerable to MITM attacks. Set \
                 BILBYCAST_ALLOW_INSECURE=1 to confirm this is intentional (dev/testing only)."
                    .into(),
            );
        }
        tracing::warn!(
            "SECURITY WARNING: accept_self_signed_cert is enabled — ALL TLS certificate \
             validation is disabled. This makes the connection vulnerable to man-in-the-middle \
             attacks. Do NOT use this in production."
        );
        // Build a rustls ClientConfig that accepts any certificate
        let tls_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(InsecureCertVerifier))
            .with_no_client_auth();
        let connector = tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(tls_config));
        tokio_tungstenite::connect_async_tls_with_config(
            current_url,
            None,
            false,
            Some(connector),
        )
        .await
        .map_err(|e| format!("WebSocket connect failed: {e}"))?
    } else if let Some(ref fingerprint) = config.cert_fingerprint {
        // Certificate pinning: validate CA chain AND check fingerprint
        tracing::info!("Certificate pinning enabled (fingerprint: {}...)", &fingerprint[..fingerprint.len().min(11)]);
        let root_store = rustls::RootCertStore::from_iter(
            webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
        );
        let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(root_store))
            .build()
            .map_err(|e| format!("Failed to build certificate verifier: {e}"))?;
        let verifier = PinnedCertVerifier {
            expected_fingerprint: fingerprint.clone(),
            inner,
        };
        let tls_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        let connector = tokio_tungstenite::Connector::Rustls(Arc::new(tls_config));
        tokio_tungstenite::connect_async_tls_with_config(
            current_url,
            None,
            false,
            Some(connector),
        )
        .await
        .map_err(|e| format!("WebSocket connect failed: {e}"))?
    } else {
        tokio_tungstenite::connect_async(current_url)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?
    };

    tracing::info!("WebSocket connected, sending auth...");

    // TOFU hint: if no cert pinning is configured, suggest it
    if config.cert_fingerprint.is_none() && !config.accept_self_signed_cert {
        tracing::info!(
            "Tip: Set \"cert_fingerprint\" in the manager config to enable certificate \
             pinning (protects against compromised CAs). Use the manager's TLS certificate \
             SHA-256 fingerprint (colon-separated hex)."
        );
    }

    let (mut ws_write, mut ws_read) = ws_stream.split();

    // Step 1: Send auth message as first frame
    let auth_msg = build_auth_message(config);
    if let Ok(json) = serde_json::to_string(&auth_msg) {
        ws_write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| format!("Failed to send auth: {e}"))?;
    }

    // Step 2: Wait for auth response (auth_ok, register_ack, or auth_error)
    let auth_timeout =
        tokio::time::timeout(Duration::from_secs(10), ws_read.next()).await;

    let mut registered_creds: Option<(String, String)> = None;

    match auth_timeout {
        Ok(Some(Ok(Message::Text(text)))) => {
            let response: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("Invalid auth response: {e}"))?;

            match response["type"].as_str().unwrap_or("") {
                "auth_ok" => {
                    tracing::info!("Authenticated with manager");
                    tunnel_manager.event_sender().emit(
                        EventSeverity::Info,
                        category::MANAGER,
                        "Connected to manager",
                    );
                }
                "register_ack" => {
                    let payload = &response["payload"];
                    let node_id = payload["node_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let node_secret = payload["node_secret"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    tracing::info!("Registered with manager: node_id={node_id}");
                    tunnel_manager.event_sender().emit(
                        EventSeverity::Info,
                        category::MANAGER,
                        "Connected to manager",
                    );

                    if !node_id.is_empty() && !node_secret.is_empty() {
                        // Persist immediately
                        persist_credentials(app_config, config_path, secrets_path, &node_id, &node_secret)
                            .await;
                        registered_creds = Some((node_id, node_secret));
                    }
                }
                "auth_error" => {
                    let msg = response["message"]
                        .as_str()
                        .unwrap_or("Unknown auth error");
                    tunnel_manager.event_sender().emit(
                        EventSeverity::Critical,
                        category::MANAGER,
                        format!("Manager authentication failed: {msg}"),
                    );
                    return Err(format!("Auth rejected: {msg}"));
                }
                other => {
                    return Err(format!("Unexpected auth response type: {other}"));
                }
            }
        }
        Ok(Some(Ok(_))) => return Err("Unexpected non-text auth response".into()),
        Ok(Some(Err(e))) => return Err(format!("WebSocket error during auth: {e}")),
        Ok(None) => return Err("Connection closed during auth".into()),
        Err(_) => return Err("Auth response timeout (10s)".into()),
    }

    // Now in the authenticated session — run the main loop
    let mut stats_rx = ws_stats_tx.subscribe();

    // Send initial health
    let health = build_health_message(flow_manager, api_addr, monitor_addr, resource_state);
    if let Ok(json) = serde_json::to_string(&health) {
        let _ = ws_write.send(Message::Text(json.into())).await;
    }

    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));
    // Periodic stats timer ensures tunnel-only nodes (no flows) still send
    // stats to the manager. Without this, nodes with 0 flows never fire
    // stats_rx and the manager never receives tunnel stats.
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));
    stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Outbound queue: spawned per-command handler tasks send their
    // command_ack / config_response / pong frames via this channel rather
    // than touching `ws_write` directly. The main loop is the sole writer
    // to the socket, draining `out_rx` alongside its own stats / health
    // / thumbnail / event paths. Buffer is generous (256) because every
    // command produces exactly one outbound frame and command_acks are
    // small, but a sustained burst from the manager (cluster reconcile,
    // boot-time fan-out) can pile up briefly.
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);

    // Event-driven thumbnail delivery: the thumbnail generators notify this
    // shared handle on every successful capture (input switch, freeze,
    // signal change, steady-state tick). The fallback tick is a long-period
    // safety net that also covers the cold-start case after a manager
    // reconnect, where the generation counters may already be at their
    // latest value with no pending notification.
    let thumbnail_notify = flow_manager.stats().thumbnail_update_notify.clone();
    let mut thumbnail_fallback = tokio::time::interval(Duration::from_secs(15));
    thumbnail_fallback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Track last-sent generation per flow to avoid re-sending unchanged thumbnails
    let mut thumbnail_generations: HashMap<String, u64> = HashMap::new();

    loop {
        tokio::select! {
            stats_result = stats_rx.recv() => {
                match stats_result {
                    Ok(stats_json) => {
                        // Collect tunnel stats alongside flow stats
                        let tunnel_statuses: Vec<serde_json::Value> = tunnel_manager
                            .list_tunnels()
                            .into_iter()
                            .map(|ts| serde_json::to_value(ts).unwrap_or_default())
                            .collect();

                        let flows_value = serde_json::from_str::<serde_json::Value>(&stats_json).unwrap_or_default();
                        let total_flows = flows_value.as_array().map_or(0, |a| a.len());

                        let standby = standby_listeners.as_ref()
                            .map(|sl| sl.snapshot().into_iter()
                                .map(|s| serde_json::to_value(s).unwrap_or_default())
                                .collect::<Vec<_>>());

                        let mut payload = serde_json::json!({
                            "flows": flows_value,
                            "tunnels": tunnel_statuses,
                            "uptime_secs": 0,
                            "active_flows": flow_manager.active_flow_count(),
                            "total_flows": total_flows,
                            "system_resources": build_system_resources_payload(resource_state)
                        });
                        if let Some(sb) = standby {
                            payload["standby_inputs"] = serde_json::Value::Array(sb);
                        }
                        let envelope = serde_json::json!({
                            "type": "stats",
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "payload": payload
                        });
                        if let Ok(json) = serde_json::to_string(&envelope) {
                            if ws_write.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                        // Reset the periodic timer since we just sent stats
                        stats_interval.reset();
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("Manager client lagged {n} stats messages");
                    }
                    Err(_) => break,
                }
            }

            // Periodic stats for tunnel-only nodes (no flows to trigger stats_rx)
            _ = stats_interval.tick() => {
                let tunnel_statuses: Vec<serde_json::Value> = tunnel_manager
                    .list_tunnels()
                    .into_iter()
                    .map(|ts| serde_json::to_value(ts).unwrap_or_default())
                    .collect();

                let standby = standby_listeners.as_ref()
                    .map(|sl| sl.snapshot().into_iter()
                        .map(|s| serde_json::to_value(s).unwrap_or_default())
                        .collect::<Vec<_>>());

                let mut payload = serde_json::json!({
                    "flows": [],
                    "tunnels": tunnel_statuses,
                    "uptime_secs": 0,
                    "active_flows": flow_manager.active_flow_count(),
                    "total_flows": flow_manager.active_flow_count(),
                    "system_resources": build_system_resources_payload(resource_state)
                });
                if let Some(sb) = standby {
                    payload["standby_inputs"] = serde_json::Value::Array(sb);
                }
                let envelope = serde_json::json!({
                    "type": "stats",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": payload
                });
                if let Ok(json) = serde_json::to_string(&envelope) {
                    if ws_write.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            // Drain queued outbound frames produced by spawned per-command
            // handler tasks. Keeping the socket writer single-owner here
            // avoids interleaving partial Text frames from concurrent
            // tasks.
            Some(json) = out_rx.recv() => {
                if ws_write.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }

            msg = ws_read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Spawn per-message so a long-running command
                        // (media upload chunk, update_config, etc.) can
                        // never park the WS receive loop. Each handler
                        // emits its reply via `out_tx` which the loop
                        // drains alongside stats / health / thumbnails.
                        let flow_manager = flow_manager.clone();
                        let tunnel_manager = tunnel_manager.clone();
                        let app_config = app_config.clone();
                        let config_path = config_path.clone();
                        let secrets_path = secrets_path.clone();
                        let webrtc_sessions = webrtc_sessions.clone();
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            handle_manager_message(
                                &text, &flow_manager, &tunnel_manager, &app_config,
                                &config_path, &secrets_path, &webrtc_sessions, &out_tx,
                            ).await;
                        });
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        return Err(format!("WebSocket error: {e}"));
                    }
                    _ => {}
                }
            }

            _ = ping_interval.tick() => {
                let pong = serde_json::json!({
                    "type": "health",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": build_health_payload(flow_manager, api_addr, monitor_addr, resource_state)
                });
                if let Ok(json) = serde_json::to_string(&pong) {
                    if ws_write.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            // Thumbnail push — fires when any ThumbnailAccumulator stores a
            // new JPEG (input switch / freeze detection / steady-state tick).
            _ = thumbnail_notify.notified() => {
                if send_pending_thumbnails(flow_manager, &mut thumbnail_generations, &mut ws_write).await.is_err() {
                    break;
                }
            }

            // Safety-net fallback. Covers the cold-start case after reconnect
            // (the notifier may have no permit waiting) and shields against
            // any missed notification.
            _ = thumbnail_fallback.tick() => {
                if send_pending_thumbnails(flow_manager, &mut thumbnail_generations, &mut ws_write).await.is_err() {
                    break;
                }
            }

            // Forward queued events to the manager
            Some(event) = event_rx.recv() => {
                let envelope = build_event_envelope(&event);
                if let Ok(json) = serde_json::to_string(&envelope) {
                    if ws_write.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    if let Some((node_id, node_secret)) = registered_creds {
        return Ok(ConnectResult::Registered {
            node_id,
            node_secret,
        });
    }

    Ok(ConnectResult::Closed)
}

/// Build the auth message sent as the first WebSocket frame.
/// Contains either registration_token OR node_id + node_secret.
/// WebSocket protocol version. Sent in auth payload so the manager can detect mismatches.
const WS_PROTOCOL_VERSION: u32 = 1;

fn build_auth_message(config: &ManagerConfig) -> serde_json::Value {
    if let (Some(node_id), Some(node_secret)) = (&config.node_id, &config.node_secret) {
        serde_json::json!({
            "type": "auth",
            "payload": {
                "node_id": node_id,
                "node_secret": node_secret,
                "software_version": env!("CARGO_PKG_VERSION"),
                "protocol_version": WS_PROTOCOL_VERSION
            }
        })
    } else if let Some(token) = &config.registration_token {
        serde_json::json!({
            "type": "auth",
            "payload": {
                "registration_token": token,
                "software_version": env!("CARGO_PKG_VERSION"),
                "protocol_version": WS_PROTOCOL_VERSION
            }
        })
    } else {
        serde_json::json!({
            "type": "auth",
            "payload": {}
        })
    }
}

fn build_health_message(flow_manager: &FlowManager, api_addr: &str, monitor_addr: Option<&str>, resource_state: &SystemResourceState) -> serde_json::Value {
    serde_json::json!({
        "type": "health",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": build_health_payload(flow_manager, api_addr, monitor_addr, resource_state)
    })
}

fn build_health_payload(flow_manager: &FlowManager, api_addr: &str, monitor_addr: Option<&str>, resource_state: &SystemResourceState) -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": 0,
        "active_flows": flow_manager.active_flow_count(),
        "total_flows": flow_manager.active_flow_count(),
        "api_addr": api_addr,
        "monitor_addr": monitor_addr,
        // Capability advertisement. The manager UI gates feature controls
        // per-node on the presence of these strings (e.g. ST 2110 form
        // sections, Phase B audio_encode block). Older edges that don't
        // include this field are treated as "no advertised features" by
        // the manager — the field is `Option` with `serde(default)` on
        // the manager side.
        "capabilities": edge_capabilities(),
        "system_resources": build_system_resources_payload(resource_state)
    })
}

fn build_system_resources_payload(state: &SystemResourceState) -> serde_json::Value {
    serde_json::json!({
        "cpu_percent": state.cpu_percent_f64(),
        "ram_percent": state.ram_percent_f64(),
        "ram_used_mb": state.ram_used_mb.load(std::sync::atomic::Ordering::Relaxed),
        "ram_total_mb": state.ram_total_mb.load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// Return the list of capability strings advertised by this build.
///
/// Always includes the ST 2110 Phase 1 capabilities (the default build
/// ships them all) and the Phase B `audio-encode` capability (the
/// ffmpeg-sidecar audio encoder is unconditionally compiled into every
/// build — ffmpeg itself is a runtime dependency, gated lazily on the
/// per-output `audio_encode` block). The `ptp-internal` feature is
/// reflected as an additional `"ptp-internal"` string for completeness.
fn edge_capabilities() -> Vec<&'static str> {
    let mut caps = vec![
        // SMPTE ST 2110 Phase 1 (audio + ancillary)
        "st2110-30",
        "st2110-31",
        "st2110-40",
        // SMPTE ST 2110 Phase 2 (uncompressed video, RFC 4175)
        "st2110-20",
        "st2110-23",
        "ptp",
        "is-04",
        "is-05",
        "is-08",
        "bcp-004",
        "redundancy-2022-7",
        // Phase B compressed-audio bridge: ffmpeg-sidecar audio encoder
        // on RTMP / HLS / WebRTC outputs.
        "audio-encode",
        // This edge supports flows with no input — standalone output
        // definitions whose input is connected at runtime by the manager.
        "optional-input",
    ];
    if cfg!(feature = "ptp-internal") {
        caps.push("ptp-internal");
    }
    if cfg!(feature = "replay") {
        // Replay-server: continuous flow recording + clip-based playback.
        // The manager UI gates the Replay tab on the presence of this string.
        caps.push("replay");
        // Phase 2 — Replay v2 surface: pre-buffer / ring-buffer recording,
        // variable-speed playback (0.1×–1.0×) with PCR/PTS rewrite, frame
        // step, multi-cam sync (start_at_unix_ms anchor), per-flow stall
        // alarms. Manager UI checks for `replay-v2` before exposing the
        // speed slider / step buttons / pre-buffer field / sync-group tab,
        // so a Phase 1 edge keeps the Phase 1 surface unchanged.
        caps.push("replay-v2");
    }
    if cfg!(any(
        feature = "video-encoder-x264",
        feature = "video-encoder-x265",
        feature = "video-encoder-nvenc",
        feature = "video-encoder-qsv"
    )) {
        caps.push("video-encode");
    }
    if cfg!(feature = "video-encoder-x264") {
        caps.push("video-encoder-x264");
    }
    if cfg!(feature = "video-encoder-x265") {
        caps.push("video-encoder-x265");
    }
    if cfg!(feature = "video-encoder-nvenc") {
        caps.push("video-encoder-nvenc");
    }
    if cfg!(feature = "video-encoder-qsv") {
        caps.push("video-encoder-qsv");
    }
    if cfg!(feature = "fdk-aac") {
        caps.push("fdk-aac");
    }
    if cfg!(feature = "video-thumbnail") {
        caps.push("video-thumbnail");
    }
    if cfg!(feature = "webrtc") {
        caps.push("webrtc");
    }
    if cfg!(feature = "tls") {
        caps.push("tls");
    }
    caps
}

/// Persist manager credentials to config + secrets files.
async fn persist_credentials(
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
    secrets_path: &PathBuf,
    node_id: &str,
    node_secret: &str,
) {
    let mut cfg = app_config.write().await;
    if let Some(ref mut mgr) = cfg.manager {
        mgr.registration_token = None;
        mgr.node_id = Some(node_id.to_string());
        mgr.node_secret = Some(node_secret.to_string());
    }
    // Node is now provisioned — close the setup wizard so /setup stops
    // accepting reconfiguration. Idempotent on reconnect.
    if cfg.setup_enabled {
        cfg.setup_enabled = false;
        // Clear the one-shot setup-wizard bearer token: the secret should not
        // outlive its purpose. /setup is already gated on setup_enabled, so
        // this is forensic hygiene rather than additional access control.
        cfg.setup_token = None;
        tracing::info!(
            "Setup wizard disabled and one-shot token cleared after successful manager registration"
        );
    }
    if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), cfg.clone()).await {
        tracing::warn!("Failed to persist manager credentials: {e}");
    } else {
        tracing::info!("Manager credentials saved (node_secret → {})", secrets_path.display());
    }
}

/// Scan every flow's thumbnail accumulators (flow-level + per-input) and
/// push any JPEG whose generation counter has advanced since the last send.
/// Returns `Err(())` if a WebSocket send fails so the caller can terminate
/// the session loop — matches the behavior of the stats / health / event
/// send sites.
async fn send_pending_thumbnails<S>(
    flow_manager: &Arc<FlowManager>,
    thumbnail_generations: &mut HashMap<String, u64>,
    ws_write: &mut futures_util::stream::SplitSink<S, Message>,
) -> Result<(), ()>
where
    S: futures_util::Sink<Message> + Unpin,
{
    use base64::Engine;
    let stats = flow_manager.stats();
    for entry in stats.flow_stats.iter() {
        let flow_id = entry.key().clone();
        let acc = entry.value();

        // Flow-level thumbnail (what outputs are sending).
        if let Some(thumb_acc) = acc.thumbnail.get() {
            let thumb_gen = thumb_acc.generation.load(std::sync::atomic::Ordering::Relaxed);
            let gen_key = format!("flow:{flow_id}");
            let prev = thumbnail_generations.get(&gen_key).copied().unwrap_or(0);
            if thumb_gen > prev {
                let jpeg_clone = thumb_acc.latest_jpeg.lock().unwrap()
                    .as_ref()
                    .map(|(jpeg, _)| jpeg.clone());
                if let Some(jpeg) = jpeg_clone {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
                    let envelope = serde_json::json!({
                        "type": "thumbnail",
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                        "payload": {
                            "flow_id": flow_id,
                            "data": b64,
                            "width": 320,
                            "height": 180
                        }
                    });
                    if let Ok(json) = serde_json::to_string(&envelope) {
                        if ws_write.send(Message::Text(json.into())).await.is_err() {
                            return Err(());
                        }
                    }
                    thumbnail_generations.insert(gen_key, thumb_gen);
                }
            }
        }

        // Per-input thumbnails (what each source is receiving).
        for per_input in acc.per_input_thumbnails.iter() {
            let input_id = per_input.key().clone();
            let thumb_acc = per_input.value();
            let thumb_gen = thumb_acc.generation.load(std::sync::atomic::Ordering::Relaxed);
            let gen_key = format!("input:{flow_id}:{input_id}");
            let prev = thumbnail_generations.get(&gen_key).copied().unwrap_or(0);
            if thumb_gen > prev {
                let jpeg_clone = thumb_acc.latest_jpeg.lock().unwrap()
                    .as_ref()
                    .map(|(jpeg, _)| jpeg.clone());
                if let Some(jpeg) = jpeg_clone {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
                    let envelope = serde_json::json!({
                        "type": "thumbnail",
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                        "payload": {
                            "flow_id": flow_id,
                            "input_id": input_id,
                            "data": b64,
                            "width": 320,
                            "height": 180
                        }
                    });
                    if let Ok(json) = serde_json::to_string(&envelope) {
                        if ws_write.send(Message::Text(json.into())).await.is_err() {
                            return Err(());
                        }
                    }
                    thumbnail_generations.insert(gen_key, thumb_gen);
                }
            }
        }
    }
    Ok(())
}

/// Handle a message from the manager.
///
/// Replies are queued onto `out_tx` rather than written to the socket
/// directly so this function can run on a `tokio::spawn`ed task — keeping
/// long-running commands (media-chunk upload, update_config, transcoder
/// bring-up) off the WS receive loop. The main loop drains `out_tx` and
/// is the single owner of the underlying socket writer.
async fn handle_manager_message(
    text: &str,
    flow_manager: &Arc<FlowManager>,
    tunnel_manager: &Arc<TunnelManager>,
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
    secrets_path: &PathBuf,
    webrtc_sessions: &WebrtcRegistry,
    out_tx: &mpsc::Sender<String>,
) {
    let envelope: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Invalid message from manager: {e}");
            return;
        }
    };

    let msg_type = envelope["type"].as_str().unwrap_or("");
    let payload = &envelope["payload"];

    match msg_type {
        "ping" => {
            let pong = serde_json::json!({
                "type": "pong",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "payload": null
            });
            if let Ok(json) = serde_json::to_string(&pong) {
                let _ = out_tx.send(json).await;
            }
        }
        "command" => {
            let command_id = payload["command_id"].as_str().unwrap_or("unknown");
            let action = &payload["action"];
            let action_type = action["type"].as_str().unwrap_or("");

            // Handle get_config specially — it sends a config_response, not a command_ack
            if action_type == "get_config" {
                tracing::info!("Manager command: get_config");
                let cfg = app_config.read().await;
                // Strip infrastructure secrets (node credentials, TLS, tunnel keys)
                // before sending — flow parameters are preserved for UI visibility
                let mut safe_cfg = (*cfg).clone();
                safe_cfg.strip_secrets();
                let config_json = serde_json::to_value(&safe_cfg).unwrap_or_default();
                let response = serde_json::json!({
                    "type": "config_response",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": config_json
                });
                if let Ok(json) = serde_json::to_string(&response) {
                    let _ = out_tx.send(json).await;
                }
                return;
            }

            let result = execute_command(
                action_type, action, flow_manager, tunnel_manager,
                app_config, config_path, secrets_path, webrtc_sessions,
            )
            .await;

            let mut payload = serde_json::json!({
                "command_id": command_id,
                "success": result.is_ok(),
            });
            match &result {
                Ok(Some(data)) => { payload["data"] = data.clone(); }
                Err(e) => {
                    payload["error"] = serde_json::Value::String(e.message.clone());
                    if let Some(code) = &e.code {
                        payload["error_code"] = serde_json::Value::String(code.clone());
                    }
                }
                _ => {}
            }
            let ack = serde_json::json!({
                "type": "command_ack",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "payload": payload
            });
            if let Ok(json) = serde_json::to_string(&ack) {
                let _ = out_tx.send(json).await;
            }
        }
        "register_ack" => {
            // Handled during initial auth handshake; ignore if received later
            tracing::debug!("Late register_ack received, ignoring");
        }
        _ => {
            tracing::debug!("Unknown message type from manager: {msg_type}");
        }
    }
}

/// Maximum window we wait after a flow spawn for the runtime input/output
/// bind to either succeed (no event) or surface a Critical event with a
/// known `error_code`. 600ms is enough for a UDP/TCP bind to complete on a
/// healthy host but short enough that the manager doesn't perceive the
/// command as hanging when no failure occurs.
const FIRST_BIND_WAIT: Duration = Duration::from_millis(600);
/// Polling interval inside the wait window. 60ms gives ~10 polls per window.
const FIRST_BIND_POLL: Duration = Duration::from_millis(60);

/// Poll the in-process recent-event tracker for a Critical port_conflict /
/// bind_failed event scoped to `flow_id` that occurred at or after
/// `spawn_started_at`. Returns the corresponding [`CommandError`] if one is
/// found within [`FIRST_BIND_WAIT`].
async fn wait_for_first_bind_failure(
    events: &super::events::EventSender,
    flow_id: &str,
    spawn_started_at: std::time::Instant,
) -> Option<CommandError> {
    let deadline = spawn_started_at + FIRST_BIND_WAIT;
    loop {
        if let Some(critical) = events.take_recent_critical_for_flow(flow_id, spawn_started_at) {
            let code = critical.error_code.unwrap_or_else(|| "bind_failed".to_string());
            // Only surface bind-related critical events on the ack — other
            // critical events (codec init failure, etc.) ride through the
            // generic flow error path.
            if code == "port_conflict" || code == "bind_failed" {
                return Some(CommandError::with_code(critical.event.message.clone(), code));
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(FIRST_BIND_POLL).await;
    }
}

/// Structured failure returned to the manager via `command_ack`. Old managers
/// ignore the optional `code`; new managers (>=0.25.x) read it as a stable
/// machine identifier (`"port_conflict"`, `"validation_error"`, etc.) so the
/// UI can render targeted error messages.
#[derive(Debug, Clone)]
pub(crate) struct CommandError {
    pub message: String,
    pub code: Option<String>,
}

impl CommandError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: None }
    }
    pub fn with_code(message: impl Into<String>, code: impl Into<String>) -> Self {
        Self { message: message.into(), code: Some(code.into()) }
    }

    /// Inspect a configuration validation error and pick the right code.
    /// Validation messages with the prefix "Port conflict:" come from
    /// `validate_port_conflicts` and map to the `port_conflict` code so the
    /// manager UI can highlight the offending field. Everything else falls
    /// back to `validation_error`.
    pub fn from_validation(prefix: &str, e: impl std::fmt::Display) -> Self {
        let inner = format!("{e}");
        let code = if inner.starts_with("Port conflict:") {
            "port_conflict"
        } else {
            "validation_error"
        };
        Self::with_code(format!("{prefix}: {inner}"), code)
    }
}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for CommandError {
    fn from(message: &str) -> Self {
        Self::new(message.to_string())
    }
}

async fn execute_command(
    action_type: &str,
    action: &serde_json::Value,
    flow_manager: &Arc<FlowManager>,
    tunnel_manager: &Arc<TunnelManager>,
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
    secrets_path: &PathBuf,
    _webrtc_sessions: &WebrtcRegistry,
) -> Result<Option<serde_json::Value>, CommandError> {
    match action_type {
        "create_flow" => {
            let flow: FlowConfig = serde_json::from_value(action["flow"].clone())
                .map_err(|e| format!("Invalid flow config: {e}"))?;
            validate_flow(&flow).map_err(|e| CommandError::from_validation("Invalid flow config", e))?;
            // Check for duplicate flow ID
            {
                let cfg = app_config.read().await;
                if cfg.flows.iter().any(|f| f.id == flow.id) {
                    return Err(CommandError::new(format!("Duplicate flow ID '{}'", flow.id)));
                }
            }
            tracing::info!("Manager command: create flow '{}'", flow.id);
            let resolved = {
                let cfg = app_config.read().await;
                cfg.resolve_flow(&flow).map_err(|e| e.to_string())?
            };
            let spawn_started_at = std::time::Instant::now();
            let _runtime = flow_manager
                .create_flow(resolved)
                .await
                .map_err(|e| e.to_string())?;
            #[cfg(feature = "webrtc")]
            register_whip_if_needed(_webrtc_sessions, &_runtime);
            #[cfg(feature = "webrtc")]
            register_whep_if_needed(_webrtc_sessions, &_runtime);
            // Wait briefly for the runtime spawn to bind. If a port conflict
            // or bind failure is reported within the window, surface it on
            // the command_ack with `error_code` so the manager UI can
            // attribute the failure to the just-submitted change instead of
            // racing the events page.
            if let Some(err) = wait_for_first_bind_failure(flow_manager.event_sender(), &flow.id, spawn_started_at).await {
                let _ = flow_manager.destroy_flow(&flow.id).await;
                return Err(err);
            }
            // Persist to config
            let mut cfg = app_config.write().await;
            cfg.flows.push(flow);
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "update_flow" => {
            let new_flow: FlowConfig = serde_json::from_value(action["flow"].clone())
                .map_err(|e| format!("Invalid flow config: {e}"))?;
            validate_flow(&new_flow).map_err(|e| CommandError::from_validation("Invalid flow config", e))?;
            let flow_id = action["flow_id"]
                .as_str()
                .unwrap_or(&new_flow.id);
            tracing::info!("Manager command: update flow '{flow_id}' (diff-based)");

            // Get old flow config for comparison
            let old_flow = {
                let cfg = app_config.read().await;
                cfg.flows.iter().find(|f| f.id == flow_id).cloned()
            };

            let was_running = flow_manager.is_running(flow_id);

            if let Some(old_flow) = old_flow {
                if was_running && new_flow.enabled {
                    // Flow exists and should keep running — diff it.
                    //
                    // Only fields that actively affect the running task graph
                    // force a restart. `name` is cosmetic; `thumbnail` and
                    // `media_analysis` are subscriber-task toggles whose
                    // current runtime binding is baked in at flow spawn —
                    // the new flag is persisted for the *next* start-up, but
                    // toggling mid-flight is not yet wired through the
                    // FlowManager, so we avoid the disruptive restart here.
                    let input_changed = old_flow.input_ids != new_flow.input_ids;
                    // `content_analysis` tier toggles are baked in at
                    // `FlowRuntime::start` (the analyser tasks are spawned
                    // there). Toggling them mid-flight requires a flow
                    // restart so the new tier tasks actually start running
                    // — without this, the operator flips the switch in the
                    // manager UI and nothing visible happens.
                    let content_analysis_changed = old_flow.content_analysis != new_flow.content_analysis;
                    // Recording-config changes (enabled flip, segment_seconds,
                    // retention, max_bytes, pre_buffer_seconds) are baked in
                    // at `FlowRuntime::start` — `spawn_writer` runs there and
                    // owns the recording_handle for the flow's lifetime.
                    // Mid-flight diff doesn't tear that handle down, so
                    // unchecking `recording.enabled` in the flow form would
                    // silently leave the writer rolling pre-roll TS to disk.
                    // Restart the flow on any recording-config change so the
                    // edit takes effect immediately.
                    let recording_changed = old_flow.recording != new_flow.recording;
                    let restart_required = old_flow.bandwidth_limit != new_flow.bandwidth_limit
                        || content_analysis_changed
                        || recording_changed;
                    let persist_only_meta_changed = old_flow.name != new_flow.name
                        || old_flow.media_analysis != new_flow.media_analysis
                        || old_flow.thumbnail != new_flow.thumbnail;
                    if persist_only_meta_changed && !(input_changed || restart_required) {
                        tracing::info!(
                            "Update flow '{flow_id}': metadata-only change (name/thumbnail/media_analysis) — persisting without restart; new thumbnail/media_analysis flag takes effect at next flow restart"
                        );
                    }

                    if input_changed || restart_required {
                        // Input, rate-limit, or content-analysis tier
                        // toggles changed — must restart entire flow.
                        tracing::info!(
                            "Update flow '{flow_id}': restarting (input changed={input_changed}, bandwidth_limit/content_analysis/recording changed={restart_required}, content_analysis_changed={content_analysis_changed}, recording_changed={recording_changed})"
                        );
                        let _ = flow_manager.destroy_flow(flow_id).await;
                        let resolved = {
                            let cfg = app_config.read().await;
                            cfg.resolve_flow(&new_flow).map_err(|e| e.to_string())?
                        };
                        let spawn_started_at = std::time::Instant::now();
                        let _runtime = flow_manager
                            .create_flow(resolved)
                            .await
                            .map_err(|e| e.to_string())?;
                        #[cfg(feature = "webrtc")]
                        register_whip_if_needed(_webrtc_sessions, &_runtime);
                        #[cfg(feature = "webrtc")]
                        register_whep_if_needed(_webrtc_sessions, &_runtime);
                        if let Some(err) = wait_for_first_bind_failure(flow_manager.event_sender(), flow_id, spawn_started_at).await {
                            let _ = flow_manager.destroy_flow(flow_id).await;
                            return Err(err);
                        }
                    } else {
                        // Only outputs changed — diff surgically against the runtime.
                        // `old_flow.output_ids` is intentionally unused here; see
                        // `diff_outputs_inner` for why runtime state is authoritative.
                        tracing::info!(
                            "Update flow '{flow_id}': input unchanged, reconciling outputs (config old→new: {} → {})",
                            old_flow.output_ids.len(),
                            new_flow.output_ids.len(),
                        );
                        let cfg = app_config.read().await;
                        diff_outputs(flow_manager, flow_id, &new_flow.output_ids, &cfg).await;
                    }
                } else if was_running && !new_flow.enabled {
                    // Disable
                    tracing::info!("Update flow '{flow_id}': disabling");
                    let _ = flow_manager.destroy_flow(flow_id).await;
                } else if !was_running && new_flow.enabled {
                    // Enable / start
                    tracing::info!("Update flow '{flow_id}': starting");
                    let resolved = {
                        let cfg = app_config.read().await;
                        cfg.resolve_flow(&new_flow).map_err(|e| e.to_string())?
                    };
                    let spawn_started_at = std::time::Instant::now();
                    let _runtime = flow_manager
                        .create_flow(resolved)
                        .await
                        .map_err(|e| e.to_string())?;
                    #[cfg(feature = "webrtc")]
                    register_whip_if_needed(_webrtc_sessions, &_runtime);
                    #[cfg(feature = "webrtc")]
                    register_whep_if_needed(_webrtc_sessions, &_runtime);
                    if let Some(err) = wait_for_first_bind_failure(flow_manager.event_sender(), flow_id, spawn_started_at).await {
                        let _ = flow_manager.destroy_flow(flow_id).await;
                        return Err(err);
                    }
                }
            } else {
                // No old flow — create new
                tracing::info!("Update flow '{flow_id}': creating (no previous config)");
                let _ = flow_manager.destroy_flow(flow_id).await; // in case running without config
                let resolved = {
                    let cfg = app_config.read().await;
                    cfg.resolve_flow(&new_flow).map_err(|e| e.to_string())?
                };
                let spawn_started_at = std::time::Instant::now();
                let _runtime = flow_manager
                    .create_flow(resolved)
                    .await
                    .map_err(|e| e.to_string())?;
                #[cfg(feature = "webrtc")]
                register_whip_if_needed(_webrtc_sessions, &_runtime);
                #[cfg(feature = "webrtc")]
                register_whep_if_needed(_webrtc_sessions, &_runtime);
                if let Some(err) = wait_for_first_bind_failure(flow_manager.event_sender(), flow_id, spawn_started_at).await {
                    let _ = flow_manager.destroy_flow(flow_id).await;
                    return Err(err);
                }
            }

            // Update config
            let mut cfg = app_config.write().await;
            if let Some(pos) = cfg.flows.iter().position(|f| f.id == flow_id) {
                cfg.flows[pos] = new_flow;
            } else {
                cfg.flows.push(new_flow);
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "update_flow_assembly" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let new_assembly: FlowAssembly = serde_json::from_value(action["assembly"].clone())
                .map_err(|e| format!("Invalid assembly: {e}"))?;
            tracing::info!(
                "Manager command: update_flow_assembly '{flow_id}' (kind={:?}, programs={})",
                new_assembly.kind,
                new_assembly.programs.len(),
            );

            // Look up the running flow.
            let runtime = match flow_manager.get_runtime(flow_id) {
                Some(r) => r,
                None => {
                    return Err(CommandError::with_code(
                        format!("Flow '{flow_id}' is not running"),
                        "flow_not_running",
                    ));
                }
            };

            // No-op short-circuit — preserves CC/PSI continuity by not
            // taking the merger/assembler through a transient state.
            if let Some(cur) = runtime.current_assembly().await {
                if cur == new_assembly {
                    tracing::info!(
                        "update_flow_assembly '{flow_id}': no-op (assembly unchanged)"
                    );
                    return Ok(None);
                }
            }

            // Hot-swap the running plan. `replace_assembly` validates,
            // resolves essence, spawns Hitless mergers, and dispatches
            // PlanCommand::ReplacePlan to the running assembler. On
            // failure it emits a Critical event carrying the structured
            // `pid_bus_*` `error_code` before bubbling the anyhow error
            // (see `build_assembly_plan` + `essence_error_to_event` in
            // `engine/flow.rs`). Recover that code from the in-process
            // recent-critical tracker so the manager UI can map it onto
            // a specific assembly-editor field. Without this, the UI
            // banner says the right thing but no field gets highlighted.
            let swap_start = std::time::Instant::now();
            if let Err(e) = runtime.replace_assembly(new_assembly.clone()).await {
                let code = runtime
                    .event_sender
                    .take_recent_critical_for_flow(flow_id, swap_start)
                    .and_then(|c| c.error_code);
                return Err(match code {
                    Some(c) => CommandError::with_code(e.to_string(), c),
                    None => CommandError::new(e.to_string()),
                });
            }

            // Persist to config.json so the change survives restart.
            let mut cfg = app_config.write().await;
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.assembly = Some(new_assembly);
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "delete_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: delete flow '{flow_id}'");
            flow_manager
                .destroy_flow(flow_id)
                .await
                .map_err(|e| e.to_string())?;
            // Remove from config
            let mut cfg = app_config.write().await;
            cfg.flows.retain(|f| f.id != flow_id);
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "stop_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: stop flow '{flow_id}'");
            flow_manager
                .stop_flow(flow_id)
                .await
                .map_err(|e| e.to_string())?;
            // Mark disabled in config
            let mut cfg = app_config.write().await;
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.enabled = false;
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "start_flow" | "restart_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: {action_type} flow '{flow_id}'");
            // If restarting, stop first (ignore error if not running)
            if action_type == "restart_flow" {
                let _ = flow_manager.destroy_flow(flow_id).await;
            }
            // Find flow config and start it
            let resolved = {
                let cfg = app_config.read().await;
                let flow_config = cfg.flows
                    .iter()
                    .find(|f| f.id == flow_id)
                    .ok_or_else(|| format!("Flow '{flow_id}' not found in config"))?;
                cfg.resolve_flow(flow_config).map_err(|e| e.to_string())?
            };
            let _runtime = flow_manager
                .create_flow(resolved)
                .await
                .map_err(|e| e.to_string())?;
            #[cfg(feature = "webrtc")]
            register_whip_if_needed(_webrtc_sessions, &_runtime);
            #[cfg(feature = "webrtc")]
            register_whep_if_needed(_webrtc_sessions, &_runtime);
            // Mark enabled in config
            let mut cfg = app_config.write().await;
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.enabled = true;
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "add_output" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let output: OutputConfig =
                serde_json::from_value(action["output"].clone())
                    .map_err(|e| format!("Invalid output config: {e}"))?;
            validate_output(&output).map_err(|e| CommandError::from_validation("Invalid output config", e))?;
            tracing::info!("Manager command: add output to flow '{flow_id}'");
            flow_manager
                .add_output(flow_id, output.clone())
                .await
                .map_err(|e| e.to_string())?;
            // Add to config: store output in top-level outputs and reference from flow
            let mut cfg = app_config.write().await;
            let output_id = output.id().to_string();
            // Add to top-level outputs if not already present
            if !cfg.outputs.iter().any(|o| o.id() == output_id) {
                cfg.outputs.push(output);
            }
            // Add output_id reference to the flow
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                if !flow.output_ids.contains(&output_id) {
                    flow.output_ids.push(output_id);
                }
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "remove_output" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let output_id = action["output_id"].as_str().ok_or("Missing output_id")?;
            tracing::info!("Manager command: remove output '{output_id}' from flow '{flow_id}'");
            flow_manager
                .remove_output(flow_id, output_id)
                .await
                .map_err(|e| e.to_string())?;
            // Remove output_id reference from the flow (and optionally from top-level outputs)
            let mut cfg = app_config.write().await;
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.output_ids.retain(|id| id != output_id);
            }
            // Remove from top-level outputs if no other flow references it
            let still_referenced = cfg.flows.iter().any(|f| f.output_ids.iter().any(|id| id == output_id));
            if !still_referenced {
                cfg.outputs.retain(|o| o.id() != output_id);
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        // ── Independent input CRUD ──
        "create_input" => {
            let input: InputDefinition = serde_json::from_value(action["input"].clone())
                .map_err(|e| format!("Invalid input config: {e}"))?;
            validate_input_definition(&input).map_err(|e| CommandError::from_validation("Invalid input", e))?;
            tracing::info!("Manager command: create input '{}' ({})", input.id, input.config.type_name());
            let mut cfg = app_config.write().await;
            if cfg.inputs.iter().any(|i| i.id == input.id) {
                return Err(CommandError::new(format!("Input '{}' already exists", input.id)));
            }
            let id = input.id.clone();
            cfg.inputs.push(input);
            persist_config(&cfg, config_path, secrets_path).await;
            flow_manager.event_sender().emit_input(
                EventSeverity::Info, category::FLOW,
                format!("Input '{}' created", id), &id,
            );
            Ok(None)
        }
        "update_input" => {
            let input_id = action["input_id"].as_str().ok_or("Missing input_id")?;
            let mut input: InputDefinition = serde_json::from_value(action["input"].clone())
                .map_err(|e| format!("Invalid input config: {e}"))?;
            validate_input_definition(&input).map_err(|e| CommandError::from_validation("Invalid input", e))?;
            tracing::info!("Manager command: update input '{input_id}'");
            let mut cfg = app_config.write().await;
            let idx = cfg.inputs.iter().position(|i| i.id == input_id)
                .ok_or_else(|| format!("Input '{input_id}' not found"))?;
            let old = cfg.inputs[idx].clone();
            // Activation is owned by the `activate_input` command. Edits to
            // an input's config must never change its active state.
            input.active = old.active;
            let config_or_meta_changed =
                old.config != input.config || old.name != input.name || old.group != input.group;
            cfg.inputs[idx] = input.clone();
            if let Some(flow) = cfg.flow_using_input(input_id).cloned() {
                if flow.enabled && flow_manager.is_running(&flow.id) && config_or_meta_changed {
                    let _ = flow_manager.destroy_flow(&flow.id).await;
                    if let Ok(resolved) = cfg.resolve_flow(&flow) {
                        match flow_manager.create_flow(resolved).await {
                            Ok(_) => tracing::info!("Restarted flow '{}' after input update", flow.id),
                            Err(e) => tracing::error!("Failed to restart flow '{}': {e}", flow.id),
                        }
                    }
                }
            }
            persist_config(&cfg, config_path, secrets_path).await;
            flow_manager.event_sender().emit_input(
                EventSeverity::Info, category::FLOW,
                format!("Input '{}' updated", input_id), input_id,
            );
            Ok(None)
        }
        "activate_input" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?.to_string();
            let input_id = action["input_id"].as_str().ok_or("Missing input_id")?.to_string();
            tracing::info!("Manager command: activate_input '{input_id}' on flow '{flow_id}'");
            let mut cfg = app_config.write().await;
            let flow_idx = cfg
                .flows
                .iter()
                .position(|f| f.id == flow_id)
                .ok_or_else(|| format!("Flow '{flow_id}' not found"))?;
            if !cfg.flows[flow_idx].input_ids.iter().any(|id| id == &input_id) {
                return Err(CommandError::new(format!(
                    "Input '{input_id}' is not a member of flow '{flow_id}'"
                )));
            }
            let member_ids: Vec<String> = cfg.flows[flow_idx].input_ids.clone();
            for def in cfg.inputs.iter_mut() {
                if member_ids.contains(&def.id) {
                    def.active = def.id == input_id;
                }
            }
            if flow_manager.is_running(&flow_id) {
                if let Err(e) = flow_manager.switch_active_input(&flow_id, &input_id).await {
                    tracing::warn!("switch_active_input failed for '{flow_id}': {e}");
                }
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "activate_output" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?.to_string();
            let output_id = action["output_id"].as_str().ok_or("Missing output_id")?.to_string();
            let active = action["active"].as_bool().ok_or("Missing active flag")?;
            tracing::info!(
                "Manager command: activate_output '{output_id}' on flow '{flow_id}' → active={active}"
            );
            let mut cfg = app_config.write().await;
            let output = cfg
                .outputs
                .iter_mut()
                .find(|o| o.id() == output_id)
                .ok_or_else(|| format!("Output '{output_id}' not found"))?;
            output.set_active(active);
            if flow_manager.is_running(&flow_id) {
                if let Err(e) = flow_manager
                    .set_output_active(&flow_id, &output_id, active)
                    .await
                {
                    tracing::warn!("set_output_active failed for '{flow_id}': {e}");
                }
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "delete_input" => {
            let input_id = action["input_id"].as_str().ok_or("Missing input_id")?;
            tracing::info!("Manager command: delete input '{input_id}'");
            let mut cfg = app_config.write().await;
            if cfg.flow_using_input(input_id).is_some() {
                return Err(CommandError::new(format!("Input '{input_id}' is assigned to a flow — unassign first")));
            }
            let before = cfg.inputs.len();
            cfg.inputs.retain(|i| i.id != input_id);
            if cfg.inputs.len() == before {
                return Err(CommandError::new(format!("Input '{input_id}' not found")));
            }
            persist_config(&cfg, config_path, secrets_path).await;
            flow_manager.event_sender().emit_input(
                EventSeverity::Info, category::FLOW,
                format!("Input '{}' deleted", input_id), input_id,
            );
            Ok(None)
        }
        // ── Independent output CRUD ──
        "create_output" => {
            let output: OutputConfig = serde_json::from_value(action["output"].clone())
                .map_err(|e| format!("Invalid output config: {e}"))?;
            validate_output(&output).map_err(|e| CommandError::from_validation("Invalid output", e))?;
            tracing::info!("Manager command: create output '{}' ({})", output.id(), output.type_name());
            let mut cfg = app_config.write().await;
            if cfg.outputs.iter().any(|o| o.id() == output.id()) {
                return Err(CommandError::new(format!("Output '{}' already exists", output.id())));
            }
            let id = output.id().to_string();
            cfg.outputs.push(output);
            persist_config(&cfg, config_path, secrets_path).await;
            flow_manager.event_sender().emit_output(
                EventSeverity::Info, category::FLOW,
                format!("Output '{}' created", id), &id,
            );
            Ok(None)
        }
        "update_output" => {
            let output_id = action["output_id"].as_str().ok_or("Missing output_id")?;
            let output: OutputConfig = serde_json::from_value(action["output"].clone())
                .map_err(|e| format!("Invalid output config: {e}"))?;
            validate_output(&output).map_err(|e| CommandError::from_validation("Invalid output", e))?;
            tracing::info!("Manager command: update output '{output_id}'");
            let mut cfg = app_config.write().await;
            let idx = cfg.outputs.iter().position(|o| o.id() == output_id)
                .ok_or_else(|| format!("Output '{output_id}' not found"))?;
            let old_output = cfg.outputs[idx].clone();
            cfg.outputs[idx] = output.clone();
            // Hot-swap on running flow if the config actually changed
            if let Some(flow) = cfg.flow_using_output(output_id).cloned() {
                if flow_manager.is_running(&flow.id) && old_output != output {
                    let _ = flow_manager.remove_output(&flow.id, output_id).await;
                    let _ = flow_manager.add_output(&flow.id, output).await;
                }
            }
            persist_config(&cfg, config_path, secrets_path).await;
            flow_manager.event_sender().emit_output(
                EventSeverity::Info, category::FLOW,
                format!("Output '{}' updated", output_id), output_id,
            );
            Ok(None)
        }
        "delete_output" => {
            let output_id = action["output_id"].as_str().ok_or("Missing output_id")?;
            tracing::info!("Manager command: delete output '{output_id}'");
            let mut cfg = app_config.write().await;
            if cfg.flow_using_output(output_id).is_some() {
                return Err(CommandError::new(format!("Output '{output_id}' is assigned to a flow — unassign first")));
            }
            let before = cfg.outputs.len();
            cfg.outputs.retain(|o| o.id() != output_id);
            if cfg.outputs.len() == before {
                return Err(CommandError::new(format!("Output '{output_id}' not found")));
            }
            persist_config(&cfg, config_path, secrets_path).await;
            flow_manager.event_sender().emit_output(
                EventSeverity::Info, category::FLOW,
                format!("Output '{}' deleted", output_id), output_id,
            );
            Ok(None)
        }
        "create_tunnel" => {
            let mut tunnel: TunnelConfig = serde_json::from_value(action["tunnel"].clone())
                .map_err(|e| format!("Invalid tunnel config: {e}"))?;
            tunnel.normalize_relay_addrs();
            validate_tunnel(&tunnel).map_err(|e| CommandError::from_validation("Invalid tunnel config", e))?;
            tracing::info!("Manager command: create tunnel '{}'", tunnel.id);
            tunnel_manager
                .create_tunnel(tunnel.clone())
                .await
                .map_err(|e| e.to_string())?;
            // Upsert in config: replace existing entry or append new
            let mut cfg = app_config.write().await;
            if let Some(existing) = cfg.tunnels.iter_mut().find(|t| t.id == tunnel.id) {
                *existing = tunnel;
            } else {
                cfg.tunnels.push(tunnel);
            }
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "delete_tunnel" => {
            let tunnel_id = action["tunnel_id"].as_str().ok_or("Missing tunnel_id")?;
            tracing::info!("Manager command: delete tunnel '{tunnel_id}'");
            tunnel_manager
                .destroy_tunnel(tunnel_id)
                .await
                .map_err(|e| e.to_string())?;
            // Remove from config
            let mut cfg = app_config.write().await;
            cfg.tunnels.retain(|t| t.id != tunnel_id);
            persist_config(&cfg, config_path, secrets_path).await;
            Ok(None)
        }
        "update_config" => {
            let mut new_config: AppConfig = serde_json::from_value(action["config"].clone())
                .map_err(|e| format!("Invalid config: {e}"))?;

            // Migrate any legacy `tunnel.relay_addr` fields into `relay_addrs`
            // before validation and secret merging.
            for tunnel in &mut new_config.tunnels {
                tunnel.normalize_relay_addrs();
            }

            // The manager doesn't have infrastructure secrets (GetConfig strips
            // node credentials, TLS, and tunnel keys), so merge the node's
            // existing secrets into the incoming config before validation.
            let old_config = app_config.read().await.clone();
            let existing_secrets = SecretsConfig::extract_from(&old_config);
            existing_secrets.merge_into(&mut new_config);

            validate_config(&new_config).map_err(|e| CommandError::from_validation("Invalid config", e))?;
            tracing::info!("Manager command: update_config (diff-based)");

            tracing::info!("Config diff: old={} flows/{} tunnels, new={} flows/{} tunnels",
                old_config.flows.len(), old_config.tunnels.len(),
                new_config.flows.len(), new_config.tunnels.len());

            // --- Diff flows ---
            let old_flow_map: HashMap<&str, &FlowConfig> =
                old_config.flows.iter().map(|f| (f.id.as_str(), f)).collect();
            let new_flow_map: HashMap<&str, &FlowConfig> =
                new_config.flows.iter().map(|f| (f.id.as_str(), f)).collect();

            // Removed flows: in old but not in new
            for &id in old_flow_map.keys() {
                if !new_flow_map.contains_key(id) {
                    tracing::info!("Config diff: removing flow '{id}'");
                    let _ = flow_manager.destroy_flow(id).await;
                }
            }

            // Added or changed flows
            for (&id, &new_flow) in &new_flow_map {
                let was_running = flow_manager.is_running(id);
                match old_flow_map.get(id) {
                    None => {
                        // Brand-new flow
                        if new_flow.enabled {
                            tracing::info!("Config diff: creating new flow '{id}'");
                            match new_config.resolve_flow(new_flow) {
                                Ok(resolved) => match flow_manager.create_flow(resolved).await {
                                    Ok(_runtime) => {
                                        #[cfg(feature = "webrtc")]
                                        register_whip_if_needed(_webrtc_sessions, &_runtime);
                                        #[cfg(feature = "webrtc")]
                                        register_whep_if_needed(_webrtc_sessions, &_runtime);
                                    }
                                    Err(e) => tracing::warn!("Failed to start new flow '{id}': {e}"),
                                }
                                Err(e) => tracing::warn!("Failed to resolve new flow '{id}': {e}"),
                            }
                        }
                    }
                    Some(&old_flow) => {
                        // Flow exists in both old and new config
                        let should_run = new_flow.enabled;

                        if was_running && !should_run {
                            // Was running, now disabled → stop
                            tracing::info!("Config diff: disabling flow '{id}'");
                            let _ = flow_manager.destroy_flow(id).await;
                        } else if !was_running && should_run {
                            // Was stopped, now enabled → start
                            tracing::info!("Config diff: enabling flow '{id}'");
                            match new_config.resolve_flow(new_flow) {
                                Ok(resolved) => match flow_manager.create_flow(resolved).await {
                                    Ok(_runtime) => {
                                        #[cfg(feature = "webrtc")]
                                        register_whip_if_needed(_webrtc_sessions, &_runtime);
                                        #[cfg(feature = "webrtc")]
                                        register_whep_if_needed(_webrtc_sessions, &_runtime);
                                    }
                                    Err(e) => tracing::warn!("Failed to start flow '{id}': {e}"),
                                }
                                Err(e) => tracing::warn!("Failed to resolve flow '{id}': {e}"),
                            }
                        } else if was_running && should_run {
                            // Both running — check what changed. Only fields
                            // bound into the running task graph force a
                            // restart; rename and thumbnail/media_analysis
                            // toggles just persist (they take effect at next
                            // start-up, since mid-flight subscriber toggle
                            // is not yet wired through FlowManager).
                            let input_changed = old_flow.input_ids != new_flow.input_ids;
                            let restart_required = old_flow.bandwidth_limit != new_flow.bandwidth_limit;
                            let persist_only_meta_changed = old_flow.name != new_flow.name
                                || old_flow.media_analysis != new_flow.media_analysis
                                || old_flow.thumbnail != new_flow.thumbnail;
                            if persist_only_meta_changed && !(input_changed || restart_required) {
                                tracing::info!(
                                    "Config diff: flow '{id}' metadata-only change — persisting without restart; thumbnail/media_analysis toggles apply at next flow restart"
                                );
                            }

                            if input_changed || restart_required {
                                // Input or rate-limit changed → must restart entire flow
                                tracing::info!("Config diff: restarting flow '{id}' (input changed={input_changed}, bandwidth_limit changed={restart_required})");
                                let _ = flow_manager.destroy_flow(id).await;
                                match new_config.resolve_flow(new_flow) {
                                    Ok(resolved) => match flow_manager.create_flow(resolved).await {
                                        Ok(_runtime) => {
                                            #[cfg(feature = "webrtc")]
                                            register_whip_if_needed(_webrtc_sessions, &_runtime);
                                            #[cfg(feature = "webrtc")]
                                            register_whep_if_needed(_webrtc_sessions, &_runtime);
                                        }
                                        Err(e) => tracing::warn!("Failed to restart flow '{id}': {e}"),
                                    }
                                    Err(e) => tracing::warn!("Failed to resolve flow '{id}': {e}"),
                                }
                            } else {
                                // Input unchanged — reconcile outputs against the runtime
                                tracing::info!(
                                    "Config diff: flow '{id}' unchanged, reconciling outputs (config old→new: {} → {})",
                                    old_flow.output_ids.len(),
                                    new_flow.output_ids.len(),
                                );
                                diff_outputs_with_configs(flow_manager, id, &new_flow.output_ids, &old_config, &new_config).await;
                            }
                        }
                        // else: !was_running && !should_run → no-op
                    }
                }
            }

            // --- Diff tunnels ---
            let old_tunnel_map: HashMap<&str, &TunnelConfig> =
                old_config.tunnels.iter().map(|t| (t.id.as_str(), t)).collect();
            let new_tunnel_map: HashMap<&str, &TunnelConfig> =
                new_config.tunnels.iter().map(|t| (t.id.as_str(), t)).collect();

            // Removed tunnels
            for &id in old_tunnel_map.keys() {
                if !new_tunnel_map.contains_key(id) {
                    tracing::info!("Config diff: removing tunnel '{id}'");
                    let _ = tunnel_manager.destroy_tunnel(id).await;
                }
            }

            // Added or changed tunnels
            for (&id, &new_tunnel) in &new_tunnel_map {
                match old_tunnel_map.get(id) {
                    None => {
                        tracing::info!("Config diff: creating new tunnel '{id}'");
                        if let Err(e) = tunnel_manager.create_tunnel(new_tunnel.clone()).await {
                            tracing::warn!("Failed to start new tunnel '{id}': {e}");
                        }
                    }
                    Some(&old_tunnel) => {
                        if old_tunnel != new_tunnel {
                            tracing::warn!("Config diff: RESTARTING tunnel '{id}' — old: {:?}, new: {:?}",
                                serde_json::to_value(old_tunnel).ok(), serde_json::to_value(new_tunnel).ok());
                            let _ = tunnel_manager.destroy_tunnel(id).await;
                            if let Err(e) = tunnel_manager.create_tunnel(new_tunnel.clone()).await {
                                tracing::warn!("Failed to restart tunnel '{id}': {e}");
                            }
                        } else {
                            tracing::info!("Config diff: tunnel '{id}' unchanged");
                        }
                    }
                }
            }

            // Apply new config and persist
            {
                let mut cfg = app_config.write().await;
                cfg.inputs = new_config.inputs.clone();
                cfg.outputs = new_config.outputs.clone();
                cfg.flows = new_config.flows.clone();
                cfg.tunnels = new_config.tunnels.clone();
                cfg.server = new_config.server.clone();
                cfg.monitor = new_config.monitor.clone();
                cfg.flow_groups = new_config.flow_groups.clone();
                cfg.device_name = new_config.device_name.clone();
                cfg.resource_limits = new_config.resource_limits.clone();
                if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), cfg.clone()).await {
                    tracing::warn!("Failed to persist config after manager command: {e}");
                    tunnel_manager.event_sender().emit(
                        EventSeverity::Warning,
                        category::CONFIG,
                        format!("Failed to persist configuration: {e}"),
                    );
                }
            }

            tunnel_manager.event_sender().emit(
                EventSeverity::Info,
                category::CONFIG,
                "Configuration updated",
            );

            Ok(None)
        }
        // ── SMPTE ST 2110 read commands ──
        //
        // Edges connect outbound to the manager over WS, typically behind
        // NAT, so the manager can't call the edge's REST `/x-nmos/`
        // endpoints directly. These WS commands are the data path the
        // manager uses instead. For each, we read the live state from the
        // globally-registered handle (set at startup) and return it as
        // `data` on the command ack. Unknown/unregistered state returns an
        // empty object rather than failing — the UI handles "no data".
        "get_audio_channel_map" => {
            let data = match crate::engine::audio_transcode::subscribe_global_is08() {
                Some(rx) => {
                    let snapshot = rx.borrow().clone();
                    serde_json::to_value(&*snapshot).unwrap_or_else(|_| serde_json::json!({}))
                }
                None => serde_json::json!({}),
            };
            Ok(Some(data))
        }
        // ── Media library commands (file-backed media-player input) ──
        "list_media" => {
            let files = crate::media::MediaLibrary::list()
                .await
                .map_err(|e| format!("list_media failed: {e}"))?;
            Ok(Some(serde_json::json!({ "files": files })))
        }
        "upload_media_chunk" => {
            let name = action["name"]
                .as_str()
                .ok_or("upload_media_chunk: missing 'name'")?
                .to_string();
            let chunk_index = action["chunk_index"]
                .as_u64()
                .ok_or("upload_media_chunk: missing or invalid 'chunk_index'")?
                as u32;
            let total_chunks = action["total_chunks"]
                .as_u64()
                .ok_or("upload_media_chunk: missing or invalid 'total_chunks'")?
                as u32;
            let total_bytes = action["total_bytes"]
                .as_u64()
                .ok_or("upload_media_chunk: missing or invalid 'total_bytes'")?;
            let data_b64 = action["data_b64"]
                .as_str()
                .ok_or("upload_media_chunk: missing 'data_b64'")?;
            let progress = crate::media::global()
                .apply_chunk(&name, chunk_index, total_chunks, total_bytes, data_b64)
                .await
                .map_err(|e| format!("upload_media_chunk failed: {e}"))?;
            Ok(Some(serde_json::to_value(&progress).unwrap_or_default()))
        }
        "delete_media" => {
            let name = action["name"]
                .as_str()
                .ok_or("delete_media: missing 'name'")?;
            let removed = crate::media::MediaLibrary::delete(name)
                .await
                .map_err(|e| format!("delete_media failed: {e}"))?;
            Ok(Some(serde_json::json!({ "deleted": removed })))
        }
        "get_nmos_state"
        | "get_ptp_state"
        | "get_sdp_document"
        | "set_audio_channel_map" => {
            // Remaining ST 2110 read commands still use their canonical
            // endpoints: `/ptp` is served from the manager's own
            // `ptp_state_cache` (populated from stats ingestion), the
            // others are typically consumed by external NMOS controllers
            // talking to the edge's `/x-nmos/` REST API directly. Return
            // an explicit "use REST endpoint" marker so any WS caller
            // sees a diagnostic rather than silent empty success.
            Ok(Some(serde_json::json!({
                "status": "use_rest_endpoint",
                "hint": "This command is intentionally unpopulated on the WS path. Use the edge's /x-nmos/ REST endpoints or, for PTP, the manager's /api/v1/nodes/{id}/ptp cache."
            })))
        }
        // ── SMPTE ST 2110 flow group mutation commands ──
        //
        // Mutate `cfg.flow_groups`, validate the resulting config, and persist
        // via save_config_split_async. Pattern mirrors the `update_config` arm above.
        "start_flow_group" => {
            // Start every member flow of a flow group atomically. The group
            // must already be persisted in `cfg.flow_groups`. The members are
            // looked up by id from `cfg.flows`. If any member is missing or
            // any FlowRuntime::start fails, every started member is rolled
            // back and an Err is returned to the manager.
            let group_id = action["flow_group_id"]
                .as_str()
                .ok_or("Missing flow_group_id in start_flow_group command")?
                .to_string();
            let cfg = app_config.read().await;
            let group = cfg
                .flow_groups
                .iter()
                .find(|g| g.id == group_id)
                .ok_or_else(|| format!("Flow group '{group_id}' not found"))?;
            let flow_configs: Vec<&FlowConfig> = group
                .flows
                .iter()
                .filter_map(|fid| cfg.flows.iter().find(|f| f.id == *fid))
                .collect();
            if flow_configs.len() != group.flows.len() {
                let missing: Vec<String> = group
                    .flows
                    .iter()
                    .filter(|fid| !cfg.flows.iter().any(|f| f.id == **fid))
                    .cloned()
                    .collect();
                return Err(format!(
                    "Flow group '{group_id}' references missing flow(s): {}",
                    missing.join(", ")
                )
                .into());
            }
            let members: Vec<ResolvedFlow> = flow_configs
                .iter()
                .map(|fc| cfg.resolve_flow(fc))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Failed to resolve flow group members: {e}"))?;
            drop(cfg);
            flow_manager
                .start_flow_group(&group_id, members)
                .await
                .map_err(|e| format!("start_flow_group failed: {e}"))?;
            Ok(None)
        }
        "stop_flow_group" => {
            let group_id = action["flow_group_id"]
                .as_str()
                .ok_or("Missing flow_group_id in stop_flow_group command")?
                .to_string();
            let cfg = app_config.read().await;
            let member_ids: Vec<String> = cfg
                .flow_groups
                .iter()
                .find(|g| g.id == group_id)
                .map(|g| g.flows.clone())
                .ok_or_else(|| format!("Flow group '{group_id}' not found"))?;
            drop(cfg);
            flow_manager
                .stop_flow_group(&group_id, &member_ids)
                .await
                .map_err(|e| format!("stop_flow_group failed: {e}"))?;
            Ok(None)
        }
        "add_flow_group" => {
            let group: FlowGroupConfig = serde_json::from_value(action["flow_group"].clone())
                .map_err(|e| format!("Invalid flow_group payload: {e}"))?;

            let mut cfg = app_config.write().await;
            if cfg.flow_groups.iter().any(|g| g.id == group.id) {
                return Err(format!("Flow group '{}' already exists", group.id).into());
            }
            cfg.flow_groups.push(group.clone());
            if let Err(e) = validate_config(&cfg) {
                // Roll back the push so an invalid group never lingers in memory.
                cfg.flow_groups.retain(|g| g.id != group.id);
                return Err(format!("Invalid flow group: {e}").into());
            }
            if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), cfg.clone()).await {
                tracing::warn!("Failed to persist config after add_flow_group: {e}");
                tunnel_manager.event_sender().emit(
                    EventSeverity::Warning,
                    category::CONFIG,
                    format!("Failed to persist flow group '{}': {e}", group.id),
                );
            } else {
                tunnel_manager.event_sender().emit(
                    EventSeverity::Info,
                    category::CONFIG,
                    format!("Flow group '{}' added", group.id),
                );
            }
            Ok(None)
        }
        "update_flow_group" => {
            let new_group: FlowGroupConfig =
                serde_json::from_value(action["flow_group"].clone())
                    .map_err(|e| format!("Invalid flow_group payload: {e}"))?;
            let target_id = action["flow_group_id"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| new_group.id.clone());

            let mut cfg = app_config.write().await;
            let idx = cfg
                .flow_groups
                .iter()
                .position(|g| g.id == target_id)
                .ok_or_else(|| format!("Flow group '{target_id}' not found"))?;
            let old = std::mem::replace(&mut cfg.flow_groups[idx], new_group.clone());
            if let Err(e) = validate_config(&cfg) {
                cfg.flow_groups[idx] = old;
                return Err(format!("Invalid flow group update: {e}").into());
            }
            if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), cfg.clone()).await {
                tracing::warn!("Failed to persist config after update_flow_group: {e}");
                tunnel_manager.event_sender().emit(
                    EventSeverity::Warning,
                    category::CONFIG,
                    format!("Failed to persist flow group '{target_id}': {e}"),
                );
            } else {
                tunnel_manager.event_sender().emit(
                    EventSeverity::Info,
                    category::CONFIG,
                    format!("Flow group '{target_id}' updated"),
                );
            }
            Ok(None)
        }
        "remove_flow_group" => {
            let target_id = action["flow_group_id"]
                .as_str()
                .ok_or("Missing flow_group_id in remove_flow_group command")?
                .to_string();

            let mut cfg = app_config.write().await;
            // Reject removal if any flow still references this group, otherwise
            // the persisted config would fail validate_config on next load.
            let referencing: Vec<String> = cfg
                .flows
                .iter()
                .filter(|f| f.flow_group_id.as_deref() == Some(target_id.as_str()))
                .map(|f| f.id.clone())
                .collect();
            if !referencing.is_empty() {
                return Err(format!(
                    "Flow group '{target_id}' is still referenced by flow(s): {}",
                    referencing.join(", ")
                )
                .into());
            }
            let before = cfg.flow_groups.len();
            cfg.flow_groups.retain(|g| g.id != target_id);
            if cfg.flow_groups.len() == before {
                return Err(format!("Flow group '{target_id}' not found").into());
            }
            if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), cfg.clone()).await {
                tracing::warn!("Failed to persist config after remove_flow_group: {e}");
                tunnel_manager.event_sender().emit(
                    EventSeverity::Warning,
                    category::CONFIG,
                    format!("Failed to persist removal of flow group '{target_id}': {e}"),
                );
            } else {
                tunnel_manager.event_sender().emit(
                    EventSeverity::Info,
                    category::CONFIG,
                    format!("Flow group '{target_id}' removed"),
                );
            }
            Ok(None)
        }
        "add_essence_flow" => {
            let group_id = action["flow_group_id"]
                .as_str()
                .ok_or("Missing flow_group_id in add_essence_flow command")?
                .to_string();
            let flow_id = action["flow_id"]
                .as_str()
                .ok_or("Missing flow_id in add_essence_flow command")?
                .to_string();

            let mut cfg = app_config.write().await;
            if !cfg.flows.iter().any(|f| f.id == flow_id) {
                return Err(format!("Flow '{flow_id}' does not exist on this node").into());
            }
            let group = cfg
                .flow_groups
                .iter_mut()
                .find(|g| g.id == group_id)
                .ok_or_else(|| format!("Flow group '{group_id}' not found"))?;
            if !group.flows.iter().any(|f| f == &flow_id) {
                group.flows.push(flow_id.clone());
            }
            if let Err(e) = validate_config(&cfg) {
                // Roll back: remove the flow_id we just added.
                if let Some(g) = cfg.flow_groups.iter_mut().find(|g| g.id == group_id) {
                    g.flows.retain(|f| f != &flow_id);
                }
                return Err(format!("Invalid essence flow add: {e}").into());
            }
            if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), cfg.clone()).await {
                tracing::warn!("Failed to persist config after add_essence_flow: {e}");
                tunnel_manager.event_sender().emit(
                    EventSeverity::Warning,
                    category::CONFIG,
                    format!("Failed to persist add_essence_flow '{flow_id}' → '{group_id}': {e}"),
                );
            } else {
                tunnel_manager.event_sender().emit(
                    EventSeverity::Info,
                    category::CONFIG,
                    format!("Flow '{flow_id}' added to group '{group_id}'"),
                );
            }
            Ok(None)
        }
        "remove_essence_flow" => {
            // Note: removing an essence flow from a group does NOT clear the
            // flow's `flow_group_id` field — that's a separate `update_flow`
            // operation. The bidirectional cross-check in validate_config will
            // reject this command if any flow still has flow_group_id pointing
            // at this group as its membership; the manager must clear the
            // flow's flow_group_id first (or the caller must use remove_flow_group).
            let group_id = action["flow_group_id"]
                .as_str()
                .ok_or("Missing flow_group_id in remove_essence_flow command")?
                .to_string();
            let flow_id = action["flow_id"]
                .as_str()
                .ok_or("Missing flow_id in remove_essence_flow command")?
                .to_string();

            let mut cfg = app_config.write().await;
            let group = cfg
                .flow_groups
                .iter_mut()
                .find(|g| g.id == group_id)
                .ok_or_else(|| format!("Flow group '{group_id}' not found"))?;
            let before = group.flows.len();
            group.flows.retain(|f| f != &flow_id);
            if group.flows.len() == before {
                return Err(format!(
                    "Flow '{flow_id}' is not a member of group '{group_id}'"
                )
                .into());
            }
            if let Err(e) = validate_config(&cfg) {
                // Roll back: re-add the flow_id we just removed.
                if let Some(g) = cfg.flow_groups.iter_mut().find(|g| g.id == group_id) {
                    g.flows.push(flow_id.clone());
                }
                return Err(format!("Invalid essence flow remove: {e}").into());
            }
            if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), cfg.clone()).await {
                tracing::warn!("Failed to persist config after remove_essence_flow: {e}");
                tunnel_manager.event_sender().emit(
                    EventSeverity::Warning,
                    category::CONFIG,
                    format!(
                        "Failed to persist remove_essence_flow '{flow_id}' from '{group_id}': {e}"
                    ),
                );
            } else {
                tunnel_manager.event_sender().emit(
                    EventSeverity::Info,
                    category::CONFIG,
                    format!("Flow '{flow_id}' removed from group '{group_id}'"),
                );
            }
            Ok(None)
        }
        "rotate_secret" => {
            let new_secret = action["new_secret"]
                .as_str()
                .ok_or("Missing new_secret in rotate_secret command")?;
            if new_secret.is_empty() {
                return Err("Empty new_secret in rotate_secret command".into());
            }

            tracing::info!("Manager command: rotate_secret — updating node authentication secret");

            // Update in-memory config and persist to disk
            {
                let mut cfg = app_config.write().await;
                if let Some(ref mut mgr) = cfg.manager {
                    mgr.node_secret = Some(new_secret.to_string());
                } else {
                    return Err("No manager config present to update secret".into());
                }
                persist_config(&cfg, config_path, secrets_path).await;
            }

            tracing::info!("Node secret rotated and persisted to secrets.json");
            // Surface a manager-visible confirmation so the UI can show the
            // rotation actually landed on disk. Without this, the manager
            // has only its own DB update to go on; if the edge reboots
            // before the new secret is persisted the next reconnect auth
            // fails silently.
            flow_manager.event_sender().emit(
                crate::manager::events::EventSeverity::Info,
                crate::manager::events::category::MANAGER,
                "Node authentication secret rotated and persisted",
            );
            Ok(None)
        }
        "test_input" => {
            let input_id = action["input_id"]
                .as_str()
                .ok_or("Missing input_id")?;
            let cfg = app_config.read().await;
            let input_def = cfg
                .inputs
                .iter()
                .find(|i| i.id == input_id)
                .ok_or_else(|| format!("Input '{input_id}' not found"))?;
            // Reject if assigned to a running flow (port conflict)
            if let Some(flow) = cfg.flows.iter().find(|f| f.input_ids.iter().any(|id| id == input_id)) {
                if flow_manager.is_running(&flow.id) {
                    return Err(CommandError::new(format!(
                        "Input '{input_id}' is in use by running flow '{}'",
                        flow.id
                    )));
                }
            }
            let input_config = input_def.config.clone();
            drop(cfg);

            tracing::info!("Testing input '{input_id}'");
            let result = crate::engine::test_connection::test_input(&input_config).await;
            Ok(Some(serde_json::to_value(result).unwrap()))
        }
        "test_output" => {
            let output_id = action["output_id"]
                .as_str()
                .ok_or("Missing output_id")?;
            let cfg = app_config.read().await;
            let output = cfg
                .outputs
                .iter()
                .find(|o| o.id() == output_id)
                .ok_or_else(|| format!("Output '{output_id}' not found"))?;
            // Reject if assigned to a running flow (port conflict)
            if let Some(flow) = cfg.flows.iter().find(|f| f.output_ids.iter().any(|oid| oid == output_id)) {
                if flow_manager.is_running(&flow.id) {
                    return Err(CommandError::new(format!(
                        "Output '{output_id}' is in use by running flow '{}'",
                        flow.id
                    )));
                }
            }
            let output_config = output.clone();
            drop(cfg);

            tracing::info!("Testing output '{output_id}'");
            let result = crate::engine::test_connection::test_output(&output_config).await;
            Ok(Some(serde_json::to_value(result).unwrap()))
        }
        // ── Replay-server commands (recording + playback) ──
        #[cfg(feature = "replay")]
        "start_recording" => {
            let flow_id = action["flow_id"].as_str()
                .ok_or("start_recording: missing 'flow_id'")?;
            let runtime = flow_manager.get_runtime(flow_id)
                .ok_or_else(|| CommandError::with_code(
                    format!("Unknown flow '{flow_id}'"),
                    "replay_recording_not_active",
                ))?;
            let handle = runtime.recording_handle.as_ref()
                .ok_or_else(|| CommandError::with_code(
                    format!("Flow '{flow_id}' has no recording configured"),
                    "replay_recording_not_active",
                ))?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            handle.command_tx.send(crate::replay::RecordingCommand::Start { reply: tx })
                .await
                .map_err(|_| CommandError::new("recording writer task is gone".to_string()))?;
            let recording_id = rx.await
                .map_err(|_| CommandError::new("recording writer dropped reply".to_string()))?
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_recording_not_active"))?;
            Ok(Some(serde_json::json!({ "recording_id": recording_id })))
        }
        #[cfg(feature = "replay")]
        "stop_recording" => {
            let flow_id = action["flow_id"].as_str()
                .ok_or("stop_recording: missing 'flow_id'")?;
            let runtime = flow_manager.get_runtime(flow_id)
                .ok_or_else(|| CommandError::new(format!("Unknown flow '{flow_id}'")))?;
            let handle = runtime.recording_handle.as_ref()
                .ok_or_else(|| CommandError::with_code(
                    format!("Flow '{flow_id}' has no recording configured"),
                    "replay_recording_not_active",
                ))?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            handle.command_tx.send(crate::replay::RecordingCommand::Stop { reply: tx })
                .await
                .map_err(|_| CommandError::new("recording writer task is gone".to_string()))?;
            rx.await
                .map_err(|_| CommandError::new("recording writer dropped reply".to_string()))?
                .map_err(|e| CommandError::new(e.to_string()))?;
            Ok(Some(serde_json::json!({})))
        }
        #[cfg(feature = "replay")]
        "mark_in" => {
            let flow_id = action["flow_id"].as_str()
                .ok_or("mark_in: missing 'flow_id'")?;
            let explicit_pts = action["pts_90khz"].as_u64();
            let runtime = flow_manager.get_runtime(flow_id)
                .ok_or_else(|| CommandError::new(format!("Unknown flow '{flow_id}'")))?;
            let handle = runtime.recording_handle.as_ref()
                .ok_or_else(|| CommandError::with_code(
                    format!("Flow '{flow_id}' has no recording configured"),
                    "replay_recording_not_active",
                ))?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            handle.command_tx.send(crate::replay::RecordingCommand::MarkIn { explicit_pts, reply: tx })
                .await
                .map_err(|_| CommandError::new("recording writer task is gone".to_string()))?;
            let ack = rx.await
                .map_err(|_| CommandError::new("recording writer dropped reply".to_string()))?
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_recording_not_active"))?;
            Ok(Some(serde_json::to_value(ack).unwrap_or_default()))
        }
        #[cfg(feature = "replay")]
        "mark_out" => {
            let flow_id = action["flow_id"].as_str()
                .ok_or("mark_out: missing 'flow_id'")?;
            let name = action["name"].as_str().map(|s| s.to_string());
            let description = action["description"].as_str().map(|s| s.to_string());
            crate::replay::clips::validate_clip_strings(name.as_deref(), description.as_deref())
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_invalid_field"))?;
            let explicit_out_pts = action["pts_90khz"].as_u64();
            let created_by = action["created_by"].as_str().map(|s| s.to_string());
            let runtime = flow_manager.get_runtime(flow_id)
                .ok_or_else(|| CommandError::new(format!("Unknown flow '{flow_id}'")))?;
            let handle = runtime.recording_handle.as_ref()
                .ok_or_else(|| CommandError::with_code(
                    format!("Flow '{flow_id}' has no recording configured"),
                    "replay_recording_not_active",
                ))?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            handle.command_tx.send(crate::replay::RecordingCommand::MarkOut {
                name, description, explicit_out_pts, created_by, reply: tx,
            }).await
                .map_err(|_| CommandError::new("recording writer task is gone".to_string()))?;
            let clip = rx.await
                .map_err(|_| CommandError::new("recording writer dropped reply".to_string()))?
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_recording_not_active"))?;
            Ok(Some(serde_json::to_value(clip).unwrap_or_default()))
        }
        #[cfg(feature = "replay")]
        "list_clips" => {
            // Two query shapes: by flow_id (resolves to the flow's
            // recording_id via FlowRuntime) or by recording_id directly
            // (lets the manager re-sync against an orphan recording).
            let recording_id = if let Some(rid) = action["recording_id"].as_str() {
                rid.to_string()
            } else if let Some(flow_id) = action["flow_id"].as_str() {
                let runtime = flow_manager.get_runtime(flow_id)
                    .ok_or_else(|| CommandError::new(format!("Unknown flow '{flow_id}'")))?;
                runtime.recording_handle.as_ref()
                    .map(|h| h.recording_id.clone())
                    .ok_or_else(|| CommandError::with_code(
                        format!("Flow '{flow_id}' has no recording configured"),
                        "replay_recording_not_active",
                    ))?
            } else {
                return Err(CommandError::new("list_clips: provide flow_id or recording_id".to_string()));
            };
            let dir = crate::replay::recording_dir(&recording_id);
            let store = crate::replay::clips::ClipStore::open(&recording_id, &dir).await
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_clip_not_found"))?;
            let clips = store.list().await;
            Ok(Some(serde_json::json!({ "clips": clips })))
        }
        #[cfg(feature = "replay")]
        "get_clip" => {
            // Single-clip metadata lookup. Walks `replay_root` to find
            // the recording that owns the clip — same pattern as
            // `rename_clip` / `delete_clip`. Lighter than `list_clips`
            // when the manager already knows the clip id (deep-links,
            // routine fire-time validation, post-rename refresh).
            let clip_id = action["clip_id"].as_str()
                .ok_or("get_clip: missing 'clip_id'")?;
            let root = crate::replay::replay_root();
            let mut found: Option<crate::replay::ClipInfo> = None;
            if let Ok(mut dir) = tokio::fs::read_dir(&root).await {
                while let Ok(Some(entry)) = dir.next_entry().await {
                    if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let rid = match entry.file_name().to_str() {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    let store = match crate::replay::clips::ClipStore::open(&rid, &entry.path()).await {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if let Some(info) = store.get(clip_id).await {
                        found = Some(info);
                        break;
                    }
                }
            }
            match found {
                Some(info) => Ok(Some(serde_json::to_value(info).unwrap_or_default())),
                None => Err(CommandError::with_code(
                    format!("Clip '{clip_id}' not found"),
                    "replay_clip_not_found",
                )),
            }
        }
        #[cfg(feature = "replay")]
        "rename_clip" => {
            let clip_id = action["clip_id"].as_str()
                .ok_or("rename_clip: missing 'clip_id'")?;
            let new_name = action["name"].as_str().map(|s| s.to_string());
            let new_description = action["description"].as_str().map(|s| s.to_string());
            if new_name.is_none() && new_description.is_none() {
                return Err(CommandError::new("rename_clip: provide name and/or description".to_string()));
            }
            crate::replay::clips::validate_clip_strings(
                new_name.as_deref(),
                new_description.as_deref(),
            ).map_err(|e| CommandError::with_code(e.to_string(), "replay_invalid_field"))?;
            // Find which recording owns this clip, then rename in place.
            let root = crate::replay::replay_root();
            let mut renamed: Option<crate::replay::ClipInfo> = None;
            if let Ok(mut dir) = tokio::fs::read_dir(&root).await {
                while let Ok(Some(entry)) = dir.next_entry().await {
                    if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let rid = match entry.file_name().to_str() {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    let store = match crate::replay::clips::ClipStore::open(&rid, &entry.path()).await {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if store.get(clip_id).await.is_some() {
                        match store.rename(clip_id, new_name.clone(), new_description.clone()).await {
                            Ok(Some(info)) => { renamed = Some(info); }
                            Ok(None) => {}
                            Err(e) => return Err(CommandError::new(format!("rename_clip failed: {e}"))),
                        }
                        break;
                    }
                }
            }
            match renamed {
                Some(info) => Ok(Some(serde_json::to_value(info).unwrap_or_default())),
                None => Err(CommandError::with_code(
                    format!("Clip '{clip_id}' not found"),
                    "replay_clip_not_found",
                )),
            }
        }
        // Phase 2 — `update_clip` is the general-purpose clip-mutation
        // command: name / description / tags (Sub-PR B quick-tag bar) /
        // in-out PTS (Sub-PR B `[` / `]` trim hotkeys). `rename_clip`
        // stays working unchanged for legacy manager builds; new
        // surfaces should target `update_clip` instead.
        #[cfg(feature = "replay")]
        "update_clip" => {
            let clip_id = action["clip_id"].as_str()
                .ok_or("update_clip: missing 'clip_id'")?;
            let new_name = action["name"].as_str().map(|s| s.to_string());
            let new_description = action["description"].as_str().map(|s| s.to_string());
            let new_tags: Option<Vec<String>> = action["tags"].as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });
            let new_in_pts = action["in_pts_90khz"].as_u64();
            let new_out_pts = action["out_pts_90khz"].as_u64();
            if new_name.is_none()
                && new_description.is_none()
                && new_tags.is_none()
                && new_in_pts.is_none()
                && new_out_pts.is_none()
            {
                return Err(CommandError::new(
                    "update_clip: at least one of name / description / tags / in_pts_90khz \
                     / out_pts_90khz must be set"
                        .to_string(),
                ));
            }
            crate::replay::clips::validate_clip_strings(
                new_name.as_deref(),
                new_description.as_deref(),
            )
            .map_err(|e| CommandError::with_code(e.to_string(), "replay_invalid_field"))?;
            if let Some(tags) = new_tags.as_ref() {
                crate::replay::clips::validate_and_normalise_tags(tags)
                    .map_err(|e| CommandError::with_code(e.to_string(), "replay_invalid_tag"))?;
            }
            // Find which recording owns this clip and update in place.
            let root = crate::replay::replay_root();
            let mut updated: Option<crate::replay::ClipInfo> = None;
            let mut found_clip = false;
            if let Ok(mut dir) = tokio::fs::read_dir(&root).await {
                while let Ok(Some(entry)) = dir.next_entry().await {
                    if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let rid = match entry.file_name().to_str() {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    let store =
                        match crate::replay::clips::ClipStore::open(&rid, &entry.path()).await {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                    if store.get(clip_id).await.is_some() {
                        found_clip = true;
                        match store
                            .update(
                                clip_id,
                                new_name.clone(),
                                new_description.clone(),
                                new_tags.clone(),
                                new_in_pts,
                                new_out_pts,
                            )
                            .await
                        {
                            Ok(Some(info)) => {
                                updated = Some(info);
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let msg = e.to_string();
                                let code = if msg.contains("replay_invalid_range") {
                                    "replay_invalid_range"
                                } else if msg.contains("replay_invalid_tag") {
                                    "replay_invalid_tag"
                                } else if msg.contains("replay_invalid_field") {
                                    "replay_invalid_field"
                                } else {
                                    "replay_clip_update_failed"
                                };
                                return Err(CommandError::with_code(
                                    format!("update_clip failed: {e}"),
                                    code,
                                ));
                            }
                        }
                        break;
                    }
                }
            }
            match updated {
                Some(info) => Ok(Some(serde_json::to_value(info).unwrap_or_default())),
                None if found_clip => Err(CommandError::with_code(
                    format!("Clip '{clip_id}' update produced no result"),
                    "replay_clip_update_failed",
                )),
                None => Err(CommandError::with_code(
                    format!("Clip '{clip_id}' not found"),
                    "replay_clip_not_found",
                )),
            }
        }
        #[cfg(feature = "replay")]
        "recording_status" => {
            let flow_id = action["flow_id"].as_str()
                .ok_or("recording_status: missing 'flow_id'")?;
            let runtime = flow_manager.get_runtime(flow_id)
                .ok_or_else(|| CommandError::new(format!("Unknown flow '{flow_id}'")))?;
            // Filesystem-level free / total bytes for the replay root.
            // Lets the manager UI render a disk meter even when no
            // per-recording `max_bytes` cap is set.
            let (free, total) = crate::replay::replay_disk_usage().unwrap_or((0, 0));
            // Per-recording cap from the flow's `RecordingConfig`. The
            // UI prefers this over the filesystem signal when present.
            let max_bytes = runtime.config.config.recording.as_ref()
                .map(|r| r.max_bytes).unwrap_or(0);
            match runtime.recording_handle.as_ref() {
                Some(h) => {
                    use std::sync::atomic::Ordering;
                    let s = &h.stats;
                    Ok(Some(serde_json::json!({
                        "armed": s.armed.load(Ordering::Relaxed),
                        "mode": crate::replay::writer::mode_to_wire_str(
                            s.mode.load(Ordering::Relaxed),
                        ),
                        "recording_id": h.recording_id,
                        "current_pts_90khz": s.current_pts_90khz.load(Ordering::Relaxed),
                        "segments_written": s.segments_written.load(Ordering::Relaxed),
                        "bytes_written": s.bytes_written.load(Ordering::Relaxed),
                        "segments_pruned": s.segments_pruned.load(Ordering::Relaxed),
                        "packets_dropped": s.packets_dropped.load(Ordering::Relaxed),
                        "index_entries": s.index_entries.load(Ordering::Relaxed),
                        "max_bytes": max_bytes,
                        "replay_root_free_bytes": free,
                        "replay_root_total_bytes": total,
                    })))
                }
                None => Ok(Some(serde_json::json!({
                    "armed": false,
                    "mode": "idle",
                    "recording_id": null,
                    "current_pts_90khz": 0,
                    "segments_written": 0,
                    "bytes_written": 0,
                    "segments_pruned": 0,
                    "packets_dropped": 0,
                    "index_entries": 0,
                    "max_bytes": max_bytes,
                    "replay_root_free_bytes": free,
                    "replay_root_total_bytes": total,
                }))),
            }
        }
        #[cfg(feature = "replay")]
        "delete_clip" => {
            let clip_id = action["clip_id"].as_str()
                .ok_or("delete_clip: missing 'clip_id'")?;
            // Search across recording dirs for the clip — clips.json
            // lookup is cheap (a few hundred bytes per recording).
            let root = crate::replay::replay_root();
            let mut deleted = false;
            let mut owning_recording: Option<String> = None;
            if let Ok(mut dir) = tokio::fs::read_dir(&root).await {
                while let Ok(Some(entry)) = dir.next_entry().await {
                    if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let rid = match entry.file_name().to_str() {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    let store = match crate::replay::clips::ClipStore::open(&rid, &entry.path()).await {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if store.get(clip_id).await.is_some() {
                        if store.delete(clip_id).await.unwrap_or(false) {
                            deleted = true;
                            owning_recording = Some(rid);
                            break;
                        }
                    }
                }
            }
            if deleted {
                if let Some(rid) = owning_recording.as_ref() {
                    flow_manager.event_sender().emit_with_details(
                        crate::manager::events::EventSeverity::Info,
                        crate::manager::events::category::REPLAY,
                        format!("Clip '{clip_id}' deleted"),
                        None,
                        serde_json::json!({
                            "replay_event": "clip_deleted",
                            "clip_id": clip_id,
                            "recording_id": rid,
                        }),
                    );
                }
                Ok(Some(serde_json::json!({ "deleted": true })))
            } else {
                Err(CommandError::with_code(
                    format!("Clip '{clip_id}' not found"),
                    "replay_clip_not_found",
                ))
            }
        }
        #[cfg(feature = "replay")]
        "cue_clip" | "play_clip" | "stop_playback" | "scrub_playback" => {
            let flow_id = action["flow_id"].as_str()
                .ok_or_else(|| CommandError::new(format!(
                    "{action_type}: missing 'flow_id'"
                )))?;
            let runtime = flow_manager.get_runtime(flow_id)
                .ok_or_else(|| CommandError::new(format!("Unknown flow '{flow_id}'")))?;
            let active_input_id = runtime.active_input_tx.borrow().clone();
            let cmd_tx = runtime.replay_command_txs.get(&active_input_id)
                .map(|r| r.clone())
                .ok_or_else(|| CommandError::with_code(
                    format!("Flow '{flow_id}' has no active replay input"),
                    "replay_no_playback_input",
                ))?;
            dispatch_replay_input_command(action_type, action, cmd_tx).await
        }
        _ => Err(CommandError::with_code(format!("Unknown command: {action_type}"), "unknown_action")),
    }
}

#[cfg(feature = "replay")]
async fn dispatch_replay_input_command(
    action_type: &str,
    action: &serde_json::Value,
    cmd_tx: tokio::sync::mpsc::Sender<crate::replay::ReplayCommand>,
) -> Result<Option<serde_json::Value>, CommandError> {
    use crate::replay::ReplayCommand;
    match action_type {
        "cue_clip" => {
            let clip_id = action["clip_id"].as_str()
                .ok_or("cue_clip: missing 'clip_id'")?
                .to_string();
            let (tx, rx) = tokio::sync::oneshot::channel();
            cmd_tx.send(ReplayCommand::Cue { clip_id, reply: tx }).await
                .map_err(|_| CommandError::new("replay input is gone".to_string()))?;
            rx.await
                .map_err(|_| CommandError::new("replay input dropped reply".to_string()))?
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_clip_not_found"))?;
            Ok(Some(serde_json::json!({})))
        }
        "play_clip" => {
            let clip_id = action["clip_id"].as_str().map(|s| s.to_string());
            let from_pts_90khz = action["from_pts_90khz"].as_u64();
            let to_pts_90khz = action["to_pts_90khz"].as_u64();
            let speed = action["speed"].as_f64().map(|v| v as f32);
            let start_at_unix_ms = action["start_at_unix_ms"].as_u64();
            let (tx, rx) = tokio::sync::oneshot::channel();
            cmd_tx.send(ReplayCommand::Play {
                clip_id, from_pts_90khz, to_pts_90khz,
                speed, start_at_unix_ms,
                reply: tx,
            }).await
                .map_err(|_| CommandError::new("replay input is gone".to_string()))?;
            rx.await
                .map_err(|_| CommandError::new("replay input dropped reply".to_string()))?
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_clip_not_found"))?;
            Ok(Some(serde_json::json!({})))
        }
        "stop_playback" => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            cmd_tx.send(ReplayCommand::Stop { reply: tx }).await
                .map_err(|_| CommandError::new("replay input is gone".to_string()))?;
            rx.await
                .map_err(|_| CommandError::new("replay input dropped reply".to_string()))?
                .map_err(|e| CommandError::new(e.to_string()))?;
            Ok(Some(serde_json::json!({})))
        }
        "set_speed" => {
            let speed = action["speed"].as_f64()
                .ok_or("set_speed: missing 'speed'")?
                as f32;
            let (tx, rx) = tokio::sync::oneshot::channel();
            cmd_tx.send(ReplayCommand::SetSpeed { speed, reply: tx }).await
                .map_err(|_| CommandError::new("replay input is gone".to_string()))?;
            rx.await
                .map_err(|_| CommandError::new("replay input dropped reply".to_string()))?
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_invalid_speed"))?;
            Ok(Some(serde_json::json!({})))
        }
        "step_frame" => {
            let dir_str = action["direction"].as_str().unwrap_or("forward");
            let direction = match dir_str {
                "backward" | "back" | "rev" | "reverse" =>
                    crate::replay::StepDirection::Backward,
                _ => crate::replay::StepDirection::Forward,
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            cmd_tx.send(ReplayCommand::StepFrame { direction, reply: tx }).await
                .map_err(|_| CommandError::new("replay input is gone".to_string()))?;
            let ack = rx.await
                .map_err(|_| CommandError::new("replay input dropped reply".to_string()))?
                .map_err(|e| CommandError::new(e.to_string()))?;
            Ok(Some(serde_json::json!({
                "pts_90khz": ack.pts_90khz,
                "segment_id": ack.segment_id,
                "byte_offset": ack.byte_offset,
            })))
        }
        "scrub_playback" => {
            let pts_90khz = action["pts_90khz"].as_u64()
                .ok_or("scrub_playback: missing 'pts_90khz'")?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            cmd_tx.send(ReplayCommand::Scrub { pts_90khz, reply: tx }).await
                .map_err(|_| CommandError::new("replay input is gone".to_string()))?;
            let ack = rx.await
                .map_err(|_| CommandError::new("replay input dropped reply".to_string()))?
                .map_err(|e| CommandError::with_code(e.to_string(), "replay_clip_not_found"))?;
            Ok(Some(serde_json::to_value(ack).unwrap_or_default()))
        }
        _ => Err(CommandError::with_code(
            format!("Unknown replay input command: {action_type}"),
            "unknown_action",
        )),
    }
}

/// Diff outputs between old and new config, applying surgical hot-add/remove.
///
/// Reconcile a running flow's outputs against the new config.
///
/// The "currently attached" set is read from **runtime state** (the keys of
/// `FlowRuntime.output_handles`), not from the old config. This is the whole
/// point — diffing config-vs-config misses orphans, i.e. outputs that stayed
/// running after a previous update where teardown didn't complete (e.g. an
/// output task that didn't respond to cancel in time and got detached). Once
/// the config's `output_ids` drifts away from what's actually running, a pure
/// config-vs-config diff sees "no change" and can never recover. Diffing
/// runtime-vs-new-config always tears down every orphan on the next update.
///
/// For `update_flow`, `old_config` and `new_config` are the same reference
/// (outputs live at the top level and the command only mutates the flow).
/// For `update_config` they are the old and new configs respectively — the
/// old config is only consulted to detect when an output definition changed
/// under an ID that is present in both (so we can replace it).
async fn diff_outputs(
    flow_manager: &FlowManager,
    flow_id: &str,
    new_output_ids: &[String],
    new_config: &AppConfig,
) {
    diff_outputs_inner(flow_manager, flow_id, new_output_ids, new_config, new_config).await;
}

async fn diff_outputs_with_configs(
    flow_manager: &FlowManager,
    flow_id: &str,
    new_output_ids: &[String],
    old_config: &AppConfig,
    new_config: &AppConfig,
) {
    diff_outputs_inner(flow_manager, flow_id, new_output_ids, old_config, new_config).await;
}

async fn diff_outputs_inner(
    flow_manager: &FlowManager,
    flow_id: &str,
    new_output_ids: &[String],
    old_config: &AppConfig,
    new_config: &AppConfig,
) {
    use std::collections::HashSet;

    let running_ids: Vec<String> = flow_manager
        .running_output_ids(flow_id)
        .await
        .unwrap_or_default();
    let running_set: HashSet<&str> = running_ids.iter().map(|s| s.as_str()).collect();
    let new_set: HashSet<&str> = new_output_ids.iter().map(|s| s.as_str()).collect();

    tracing::info!(
        "Flow '{flow_id}' output reconcile: running={:?} -> new={:?}",
        running_ids,
        new_output_ids,
    );

    // Remove outputs that are currently running but no longer referenced
    for id in running_set.iter().copied() {
        if !new_set.contains(id) {
            tracing::info!("Config diff: removing output '{id}' from flow '{flow_id}' (running, not in new config)");
            if let Err(e) = flow_manager.remove_output(flow_id, id).await {
                tracing::warn!("Failed to remove output '{id}' from flow '{flow_id}': {e}");
            }
        }
    }

    // Add new outputs or replace changed ones
    for &id in &new_set {
        let new_output = match new_config.outputs.iter().find(|o| o.id() == id) {
            Some(o) => o,
            None => {
                tracing::warn!("Config diff: output '{id}' referenced by flow '{flow_id}' but not found in top-level outputs");
                continue;
            }
        };

        if !running_set.contains(id) {
            // Not currently running — spawn it
            tracing::info!("Config diff: adding output '{id}' to flow '{flow_id}'");
            if let Err(e) = flow_manager.add_output(flow_id, new_output.clone()).await {
                tracing::warn!("Failed to add output '{id}' to flow '{flow_id}': {e}");
            }
        } else {
            // Running — check whether the config actually changed and replace if so
            let old_output = old_config.outputs.iter().find(|o| o.id() == id);

            match old_output {
                Some(old) if old != new_output => {
                    tracing::warn!("Config diff: REPLACING output '{id}' in flow '{flow_id}' — configs differ!");
                    tracing::warn!("  old: {:?}", serde_json::to_value(old).ok());
                    tracing::warn!("  new: {:?}", serde_json::to_value(new_output).ok());
                    if let Err(e) = flow_manager.remove_output(flow_id, id).await {
                        tracing::warn!("Failed to remove output '{id}' for replacement: {e}");
                    }
                    if let Err(e) = flow_manager.add_output(flow_id, new_output.clone()).await {
                        tracing::warn!("Failed to re-add output '{id}' after replacement: {e}");
                    }
                }
                Some(_) => {
                    tracing::info!("Config diff: output '{id}' unchanged, keeping alive");
                }
                None => {
                    // Running but absent from old config — treat as replacement so the
                    // runtime task is swapped for one matching the new definition.
                    tracing::warn!("Config diff: output '{id}' is running but missing from old config — replacing");
                    if let Err(e) = flow_manager.remove_output(flow_id, id).await {
                        tracing::warn!("Failed to remove output '{id}' for replacement: {e}");
                    }
                    if let Err(e) = flow_manager.add_output(flow_id, new_output.clone()).await {
                        tracing::warn!("Failed to add output '{id}' to flow '{flow_id}': {e}");
                    }
                }
            }
        }
    }
}

/// Persist config to disk (fire-and-forget, logs on error).
///
/// Offloads blocking file I/O to the Tokio blocking thread pool to avoid
/// stalling the async manager client loop.
async fn persist_config(config: &AppConfig, config_path: &PathBuf, secrets_path: &PathBuf) {
    if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), config.clone()).await {
        tracing::warn!("Failed to persist config after manager command: {e}");
    }
}
