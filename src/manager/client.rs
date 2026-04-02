// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

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

use super::events::{self, Event, EventSender, EventSeverity, build_event_envelope};

use crate::config::models::{AppConfig, FlowConfig, OutputConfig};
use crate::config::persistence::save_config_split;
use crate::config::secrets::SecretsConfig;
use crate::config::validation::{validate_config, validate_flow, validate_output, validate_tunnel};
use crate::engine::manager::FlowManager;
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
            registry.register_whip_input(&runtime.config.id, tx.clone(), bearer_token.clone());
            tracing::info!("Registered WHIP input for flow '{}'", runtime.config.id);
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
            registry.register_whep_output(&runtime.config.id, tx.clone(), bearer_token.clone());
            tracing::info!("Registered WHEP output for flow '{}'", runtime.config.id);
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
    event_rx: mpsc::UnboundedReceiver<Event>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        manager_client_loop(
            config, flow_manager, tunnel_manager, ws_stats_rx, app_config, config_path,
            secrets_path, api_addr, monitor_addr, webrtc_sessions, event_rx,
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
    mut event_rx: mpsc::UnboundedReceiver<Event>,
) {
    // If we already have a node_id from config, set it on the tunnel manager
    // so relay tunnels can identify this edge before the first manager connection.
    if let Some(ref node_id) = config.node_id {
        tunnel_manager.set_manager_node_id(node_id.clone());
    }

    let mut backoff_secs = 1u64;
    let max_backoff = 60u64;

    loop {
        tracing::info!("Connecting to manager at {}", config.url);

        match try_connect(
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
        )
        .await
        {
            Ok(ConnectResult::Closed) => {
                tracing::info!("Manager connection closed normally");
                backoff_secs = 1;
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
                backoff_secs = 1;

                // Persist to config + secrets files
                persist_credentials(&app_config, &config_path, &secrets_path, &node_id, &node_secret).await;
            }
            Err(e) => {
                tracing::warn!("Manager connection failed: {e}");
                tunnel_manager.event_sender().emit(
                    EventSeverity::Warning,
                    "manager",
                    "Manager connection lost, reconnecting",
                );
            }
        }

        tracing::info!("Reconnecting to manager in {backoff_secs}s...");
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(max_backoff);
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
    event_rx: &mut mpsc::UnboundedReceiver<Event>,
) -> Result<ConnectResult, String> {
    // Enforce TLS — only wss:// connections are allowed
    if !config.url.starts_with("wss://") {
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
            &config.url,
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
            &config.url,
            None,
            false,
            Some(connector),
        )
        .await
        .map_err(|e| format!("WebSocket connect failed: {e}"))?
    } else {
        tokio_tungstenite::connect_async(&config.url)
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
                        "manager",
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
                        "manager",
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
                        "manager",
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
    let health = build_health_message(flow_manager, api_addr, monitor_addr);
    if let Ok(json) = serde_json::to_string(&health) {
        let _ = ws_write.send(Message::Text(json.into())).await;
    }

    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));
    // Periodic stats timer ensures tunnel-only nodes (no flows) still send
    // stats to the manager. Without this, nodes with 0 flows never fire
    // stats_rx and the manager never receives tunnel stats.
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));
    stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Thumbnail polling: check for new thumbnails every 10 seconds
    let mut thumbnail_interval = tokio::time::interval(Duration::from_secs(10));
    thumbnail_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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

                        let envelope = serde_json::json!({
                            "type": "stats",
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "payload": {
                                "flows": flows_value,
                                "tunnels": tunnel_statuses,
                                "uptime_secs": 0,
                                "active_flows": flow_manager.active_flow_count(),
                                "total_flows": total_flows
                            }
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

                let envelope = serde_json::json!({
                    "type": "stats",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": {
                        "flows": [],
                        "tunnels": tunnel_statuses,
                        "uptime_secs": 0,
                        "active_flows": flow_manager.active_flow_count(),
                        "total_flows": flow_manager.active_flow_count()
                    }
                });
                if let Ok(json) = serde_json::to_string(&envelope) {
                    if ws_write.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            msg = ws_read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_manager_message(
                            &text, flow_manager, tunnel_manager, app_config, config_path,
                            secrets_path, &webrtc_sessions, &mut ws_write,
                        ).await;
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
                    "payload": build_health_payload(flow_manager, api_addr, monitor_addr)
                });
                if let Ok(json) = serde_json::to_string(&pong) {
                    if ws_write.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            // Send new thumbnails to manager
            _ = thumbnail_interval.tick() => {
                use base64::Engine;
                let stats = flow_manager.stats();
                for entry in stats.flow_stats.iter() {
                    let flow_id = entry.key().clone();
                    let acc = entry.value();
                    if let Some(thumb_acc) = acc.thumbnail.get() {
                        let thumb_gen = thumb_acc.generation.load(std::sync::atomic::Ordering::Relaxed);
                        let prev = thumbnail_generations.get(&flow_id).copied().unwrap_or(0);
                        if thumb_gen > prev {
                            // Clone JPEG data out of the mutex before any .await
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
                                        break;
                                    }
                                }
                                thumbnail_generations.insert(flow_id, thumb_gen);
                            }
                        }
                    }
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

fn build_health_message(flow_manager: &FlowManager, api_addr: &str, monitor_addr: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "type": "health",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": build_health_payload(flow_manager, api_addr, monitor_addr)
    })
}

fn build_health_payload(flow_manager: &FlowManager, api_addr: &str, monitor_addr: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": 0,
        "active_flows": flow_manager.active_flow_count(),
        "total_flows": flow_manager.active_flow_count(),
        "api_addr": api_addr,
        "monitor_addr": monitor_addr
    })
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
    if let Err(e) = save_config_split(config_path, secrets_path, &cfg) {
        tracing::warn!("Failed to persist manager credentials: {e}");
    } else {
        tracing::info!("Manager credentials saved (node_secret → {})", secrets_path.display());
    }
}

/// Handle a message from the manager.
async fn handle_manager_message<S>(
    text: &str,
    flow_manager: &Arc<FlowManager>,
    tunnel_manager: &Arc<TunnelManager>,
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
    secrets_path: &PathBuf,
    webrtc_sessions: &WebrtcRegistry,
    ws_write: &mut futures_util::stream::SplitSink<S, Message>,
) where
    S: futures_util::Sink<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
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
                let _ = ws_write.send(Message::Text(json.into())).await;
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
                    let _ = ws_write.send(Message::Text(json.into())).await;
                }
                return;
            }

            let result = execute_command(
                action_type, action, flow_manager, tunnel_manager,
                app_config, config_path, secrets_path, webrtc_sessions,
            )
            .await;

            let ack = serde_json::json!({
                "type": "command_ack",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "payload": {
                    "command_id": command_id,
                    "success": result.is_ok(),
                    "error": result.err()
                }
            });
            if let Ok(json) = serde_json::to_string(&ack) {
                let _ = ws_write.send(Message::Text(json.into())).await;
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

async fn execute_command(
    action_type: &str,
    action: &serde_json::Value,
    flow_manager: &Arc<FlowManager>,
    tunnel_manager: &Arc<TunnelManager>,
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
    secrets_path: &PathBuf,
    _webrtc_sessions: &WebrtcRegistry,
) -> Result<(), String> {
    match action_type {
        "create_flow" => {
            let flow: FlowConfig = serde_json::from_value(action["flow"].clone())
                .map_err(|e| format!("Invalid flow config: {e}"))?;
            validate_flow(&flow).map_err(|e| format!("Invalid flow config: {e}"))?;
            // Check for duplicate flow ID
            {
                let cfg = app_config.read().await;
                if cfg.flows.iter().any(|f| f.id == flow.id) {
                    return Err(format!("Duplicate flow ID '{}'", flow.id));
                }
            }
            tracing::info!("Manager command: create flow '{}'", flow.id);
            let _runtime = flow_manager
                .create_flow(flow.clone())
                .await
                .map_err(|e| e.to_string())?;
            #[cfg(feature = "webrtc")]
            register_whip_if_needed(_webrtc_sessions, &_runtime);
            #[cfg(feature = "webrtc")]
            register_whep_if_needed(_webrtc_sessions, &_runtime);
            // Persist to config
            let mut cfg = app_config.write().await;
            cfg.flows.push(flow);
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
        }
        "update_flow" => {
            let new_flow: FlowConfig = serde_json::from_value(action["flow"].clone())
                .map_err(|e| format!("Invalid flow config: {e}"))?;
            validate_flow(&new_flow).map_err(|e| format!("Invalid flow config: {e}"))?;
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
                    // Flow exists and should keep running — diff it
                    let input_changed = old_flow.input != new_flow.input;
                    let meta_changed = old_flow.name != new_flow.name
                        || old_flow.media_analysis != new_flow.media_analysis;

                    if input_changed || meta_changed {
                        // Input or metadata changed — must restart entire flow
                        tracing::info!("Update flow '{flow_id}': restarting (input changed={input_changed}, meta changed={meta_changed})");
                        let _ = flow_manager.destroy_flow(flow_id).await;
                        let _runtime = flow_manager
                            .create_flow(new_flow.clone())
                            .await
                            .map_err(|e| e.to_string())?;
                        #[cfg(feature = "webrtc")]
                        register_whip_if_needed(_webrtc_sessions, &_runtime);
                        #[cfg(feature = "webrtc")]
                        register_whep_if_needed(_webrtc_sessions, &_runtime);
                    } else {
                        // Only outputs changed — diff surgically
                        tracing::info!("Update flow '{flow_id}': input unchanged, diffing outputs ({} old → {} new)",
                            old_flow.outputs.len(), new_flow.outputs.len());
                        diff_outputs(flow_manager, flow_id, &old_flow.outputs, &new_flow.outputs).await;
                    }
                } else if was_running && !new_flow.enabled {
                    // Disable
                    tracing::info!("Update flow '{flow_id}': disabling");
                    let _ = flow_manager.destroy_flow(flow_id).await;
                } else if !was_running && new_flow.enabled {
                    // Enable / start
                    tracing::info!("Update flow '{flow_id}': starting");
                    let _runtime = flow_manager
                        .create_flow(new_flow.clone())
                        .await
                        .map_err(|e| e.to_string())?;
                    #[cfg(feature = "webrtc")]
                    register_whip_if_needed(_webrtc_sessions, &_runtime);
                    #[cfg(feature = "webrtc")]
                    register_whep_if_needed(_webrtc_sessions, &_runtime);
                }
            } else {
                // No old flow — create new
                tracing::info!("Update flow '{flow_id}': creating (no previous config)");
                let _ = flow_manager.destroy_flow(flow_id).await; // in case running without config
                let _runtime = flow_manager
                    .create_flow(new_flow.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                #[cfg(feature = "webrtc")]
                register_whip_if_needed(_webrtc_sessions, &_runtime);
                #[cfg(feature = "webrtc")]
                register_whep_if_needed(_webrtc_sessions, &_runtime);
            }

            // Update config
            let mut cfg = app_config.write().await;
            if let Some(pos) = cfg.flows.iter().position(|f| f.id == flow_id) {
                cfg.flows[pos] = new_flow;
            } else {
                cfg.flows.push(new_flow);
            }
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
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
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
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
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
        }
        "start_flow" | "restart_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: {action_type} flow '{flow_id}'");
            // If restarting, stop first (ignore error if not running)
            if action_type == "restart_flow" {
                let _ = flow_manager.destroy_flow(flow_id).await;
            }
            // Find flow config and start it
            let flow_config = {
                let cfg = app_config.read().await;
                cfg.flows
                    .iter()
                    .find(|f| f.id == flow_id)
                    .cloned()
                    .ok_or_else(|| format!("Flow '{flow_id}' not found in config"))?
            };
            let _runtime = flow_manager
                .create_flow(flow_config)
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
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
        }
        "add_output" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let output: OutputConfig =
                serde_json::from_value(action["output"].clone())
                    .map_err(|e| format!("Invalid output config: {e}"))?;
            validate_output(&output).map_err(|e| format!("Invalid output config: {e}"))?;
            tracing::info!("Manager command: add output to flow '{flow_id}'");
            flow_manager
                .add_output(flow_id, output.clone())
                .await
                .map_err(|e| e.to_string())?;
            // Add to config
            let mut cfg = app_config.write().await;
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.outputs.push(output);
            }
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
        }
        "remove_output" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let output_id = action["output_id"].as_str().ok_or("Missing output_id")?;
            tracing::info!("Manager command: remove output '{output_id}' from flow '{flow_id}'");
            flow_manager
                .remove_output(flow_id, output_id)
                .await
                .map_err(|e| e.to_string())?;
            // Remove from config
            let mut cfg = app_config.write().await;
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.outputs.retain(|o| o.id() != output_id);
            }
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
        }
        "create_tunnel" => {
            let tunnel: TunnelConfig = serde_json::from_value(action["tunnel"].clone())
                .map_err(|e| format!("Invalid tunnel config: {e}"))?;
            validate_tunnel(&tunnel).map_err(|e| format!("Invalid tunnel config: {e}"))?;
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
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
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
            persist_config(&cfg, config_path, secrets_path);
            Ok(())
        }
        "update_config" => {
            let mut new_config: AppConfig = serde_json::from_value(action["config"].clone())
                .map_err(|e| format!("Invalid config: {e}"))?;

            // The manager doesn't have infrastructure secrets (GetConfig strips
            // node credentials, TLS, and tunnel keys), so merge the node's
            // existing secrets into the incoming config before validation.
            let old_config = app_config.read().await.clone();
            let existing_secrets = SecretsConfig::extract_from(&old_config);
            existing_secrets.merge_into(&mut new_config);

            validate_config(&new_config).map_err(|e| format!("Invalid config: {e}"))?;
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
                            match flow_manager.create_flow(new_flow.clone()).await {
                                Ok(_runtime) => {
                                    #[cfg(feature = "webrtc")]
                                    register_whip_if_needed(_webrtc_sessions, &_runtime);
                                    #[cfg(feature = "webrtc")]
                                    register_whep_if_needed(_webrtc_sessions, &_runtime);
                                }
                                Err(e) => tracing::warn!("Failed to start new flow '{id}': {e}"),
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
                            match flow_manager.create_flow(new_flow.clone()).await {
                                Ok(_runtime) => {
                                    #[cfg(feature = "webrtc")]
                                    register_whip_if_needed(_webrtc_sessions, &_runtime);
                                    #[cfg(feature = "webrtc")]
                                    register_whep_if_needed(_webrtc_sessions, &_runtime);
                                }
                                Err(e) => tracing::warn!("Failed to start flow '{id}': {e}"),
                            }
                        } else if was_running && should_run {
                            // Both running — check what changed
                            let input_changed = old_flow.input != new_flow.input;
                            let meta_changed = old_flow.name != new_flow.name
                                || old_flow.media_analysis != new_flow.media_analysis;

                            if input_changed || meta_changed {
                                // Input or flow metadata changed → must restart entire flow
                                tracing::info!("Config diff: restarting flow '{id}' (input changed={input_changed}, meta changed={meta_changed})");
                                let _ = flow_manager.destroy_flow(id).await;
                                match flow_manager.create_flow(new_flow.clone()).await {
                                    Ok(_runtime) => {
                                        #[cfg(feature = "webrtc")]
                                        register_whip_if_needed(_webrtc_sessions, &_runtime);
                                        #[cfg(feature = "webrtc")]
                                        register_whep_if_needed(_webrtc_sessions, &_runtime);
                                    }
                                    Err(e) => tracing::warn!("Failed to restart flow '{id}': {e}"),
                                }
                            } else {
                                // Input unchanged — diff outputs surgically
                                tracing::info!("Config diff: flow '{id}' unchanged, diffing outputs ({} old → {} new)",
                                    old_flow.outputs.len(), new_flow.outputs.len());
                                diff_outputs(flow_manager, id, &old_flow.outputs, &new_flow.outputs).await;
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
                cfg.flows = new_config.flows.clone();
                cfg.tunnels = new_config.tunnels.clone();
                cfg.server = new_config.server.clone();
                cfg.monitor = new_config.monitor.clone();
                if let Err(e) = save_config_split(config_path.as_path(), secrets_path.as_path(), &cfg) {
                    tracing::warn!("Failed to persist config after manager command: {e}");
                    tunnel_manager.event_sender().emit(
                        EventSeverity::Warning,
                        "config",
                        format!("Failed to persist configuration: {e}"),
                    );
                }
            }

            tunnel_manager.event_sender().emit(
                EventSeverity::Info,
                "config",
                "Configuration updated",
            );

            Ok(())
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
                persist_config(&cfg, config_path, secrets_path);
            }

            tracing::info!("Node secret rotated and persisted to secrets.json");
            Ok(())
        }
        _ => Err(format!("Unknown command: {action_type}")),
    }
}

/// Diff outputs between old and new config, applying surgical hot-add/remove.
///
/// Only outputs that actually changed are touched; unchanged outputs keep running
/// without interruption.
async fn diff_outputs(
    flow_manager: &FlowManager,
    flow_id: &str,
    old_outputs: &[OutputConfig],
    new_outputs: &[OutputConfig],
) {
    let old_map: HashMap<&str, &OutputConfig> =
        old_outputs.iter().map(|o| (o.id(), o)).collect();
    let new_map: HashMap<&str, &OutputConfig> =
        new_outputs.iter().map(|o| (o.id(), o)).collect();

    // Remove outputs that are no longer present
    for &id in old_map.keys() {
        if !new_map.contains_key(id) {
            tracing::info!("Config diff: removing output '{id}' from flow '{flow_id}'");
            if let Err(e) = flow_manager.remove_output(flow_id, id).await {
                tracing::warn!("Failed to remove output '{id}' from flow '{flow_id}': {e}");
            }
        }
    }

    // Add new outputs or replace changed ones
    for (&id, &new_output) in &new_map {
        match old_map.get(id) {
            None => {
                // Brand-new output
                tracing::info!("Config diff: adding output '{id}' to flow '{flow_id}'");
                if let Err(e) = flow_manager.add_output(flow_id, new_output.clone()).await {
                    tracing::warn!("Failed to add output '{id}' to flow '{flow_id}': {e}");
                }
            }
            Some(&old_output) => {
                // Output exists in both — check if config changed
                if old_output != new_output {
                    tracing::warn!("Config diff: REPLACING output '{id}' in flow '{flow_id}' — configs differ!");
                    tracing::warn!("  old: {:?}", serde_json::to_value(old_output).ok());
                    tracing::warn!("  new: {:?}", serde_json::to_value(new_output).ok());
                    if let Err(e) = flow_manager.remove_output(flow_id, id).await {
                        tracing::warn!("Failed to remove output '{id}' for replacement: {e}");
                    }
                    if let Err(e) = flow_manager.add_output(flow_id, new_output.clone()).await {
                        tracing::warn!("Failed to re-add output '{id}' after replacement: {e}");
                    }
                } else {
                    tracing::info!("Config diff: output '{id}' unchanged, keeping alive");
                }
            }
        }
    }
}

/// Persist config to disk (fire-and-forget, logs on error).
fn persist_config(config: &AppConfig, config_path: &PathBuf, secrets_path: &PathBuf) {
    if let Err(e) = save_config_split(config_path.as_path(), secrets_path.as_path(), config) {
        tracing::warn!("Failed to persist config after manager command: {e}");
    }
}
