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
use std::time::{Duration, Instant};

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
    static_caps: Arc<crate::engine::hardware_probe::StaticCapabilities>,
    live_gpu: Arc<crate::engine::hardware_probe::LiveUtilizationState>,
    standby_listeners: Option<Arc<crate::engine::standby_listeners::StandbyListenerManager>>,
    cellular_cache: Arc<crate::util::cellular::CellularCache>,
    starlink_cache: Arc<crate::util::starlink::StarlinkCache>,
    start_time: Instant,
    manager_link: Arc<crate::manager::link_state::ManagerLinkState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        manager_client_loop(
            config, flow_manager, tunnel_manager, ws_stats_rx, app_config, config_path,
            secrets_path, api_addr, monitor_addr, webrtc_sessions, event_rx, resource_state,
            static_caps, live_gpu, standby_listeners, cellular_cache, starlink_cache, start_time,
            manager_link,
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
    static_caps: Arc<crate::engine::hardware_probe::StaticCapabilities>,
    live_gpu: Arc<crate::engine::hardware_probe::LiveUtilizationState>,
    standby_listeners: Option<Arc<crate::engine::standby_listeners::StandbyListenerManager>>,
    cellular_cache: Arc<crate::util::cellular::CellularCache>,
    starlink_cache: Arc<crate::util::starlink::StarlinkCache>,
    start_time: Instant,
    manager_link: Arc<crate::manager::link_state::ManagerLinkState>,
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
            &static_caps,
            &live_gpu,
            &standby_listeners,
            &cellular_cache,
            &starlink_cache,
            start_time,
            &manager_link,
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

        // The session ended (closed, registered-then-closed, or failed to
        // connect). Reflect the lost link on the device's local surfaces
        // before we back off and retry. Idempotent if already disconnected.
        manager_link.set_disconnected();

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

/// Establish one manager session: connect, authenticate, then pump until the
/// session ends.
///
/// **Link-state invariant:** on a successful `auth_ok` / `register_ack` this
/// calls `manager_link.set_connected()` mid-function (it does NOT mark the link
/// disconnected on any of its return paths). The caller (`manager_client_loop`)
/// is responsible for calling `manager_link.set_disconnected()` after every
/// `try_connect` return — both `Ok` and `Err`. If a future early-return is
/// added AFTER the `set_connected()` calls below, it must preserve this
/// contract: leave the disconnect to the caller's unconditional
/// `set_disconnected()`, or the local `/health` + monitor surfaces will
/// latch a stale "Connected" state after the session has actually ended.
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
    static_caps: &Arc<crate::engine::hardware_probe::StaticCapabilities>,
    live_gpu: &Arc<crate::engine::hardware_probe::LiveUtilizationState>,
    standby_listeners: &Option<Arc<crate::engine::standby_listeners::StandbyListenerManager>>,
    cellular_cache: &Arc<crate::util::cellular::CellularCache>,
    starlink_cache: &Arc<crate::util::starlink::StarlinkCache>,
    start_time: Instant,
    manager_link: &Arc<crate::manager::link_state::ManagerLinkState>,
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
                    // Device-local link indicator: an authenticated session is
                    // now live. Surfaced on the edge's own /health + monitor UI.
                    manager_link.set_connected();
                    tunnel_manager.event_sender().emit(
                        EventSeverity::Info,
                        category::MANAGER,
                        "Connected to manager",
                    );
                    // First successful auth on the new binary post-upgrade —
                    // tells the boot watchdog that the new version reached
                    // the network, so it can promote `pending_health → stable`
                    // once the configured boot_health_window elapses.
                    if let Some(ref up_cfg) = app_config.read().await.upgrades {
                        crate::upgrade::watchdog::record_healthy_beat(&up_cfg.install_root);
                    }
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
                    // Device-local link indicator: registration counts as an
                    // authenticated session for the operator-facing surfaces.
                    manager_link.set_connected();
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

    // Per-interface bandwidth sampler. State (last byte counters per
    // iface) lives here so deltas survive across health ticks. The
    // first sample after auth has no prior counters, so rx_bps / tx_bps
    // come back as None on tick #1; from tick #2 onwards they're real.
    let mut network_sampler = crate::util::network_interfaces::NetworkSampler::with_caches(
        cellular_cache.clone(),
        starlink_cache.clone(),
    );

    // Send initial health
    let health = build_health_message(
        flow_manager,
        api_addr,
        monitor_addr,
        resource_state,
        static_caps,
        live_gpu,
        Some(&mut network_sampler),
        Some(cellular_cache),
        Some(starlink_cache),
        start_time,
    );
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
                            "uptime_secs": start_time.elapsed().as_secs(),
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
                    "uptime_secs": start_time.elapsed().as_secs(),
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
                    Some(Ok(Message::Binary(bytes))) => {
                        // Binary frames carry media-upload chunks with no
                        // base64 inflation. Spawn per-frame like the Text arm
                        // so a slow disk write never parks the receive loop;
                        // the `command_ack` reply rides `out_tx` (single-owner
                        // writer rule).
                        let tunnel_manager = tunnel_manager.clone();
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            handle_manager_binary(&bytes, &tunnel_manager, &out_tx).await;
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
                    "payload": build_health_payload(
                        flow_manager,
                        api_addr,
                        monitor_addr,
                        resource_state,
                        static_caps,
                        live_gpu,
                        Some(&mut network_sampler),
                        Some(cellular_cache),
                        Some(starlink_cache),
                        start_time,
                    )
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
/// WebSocket protocol version. Sent in auth payload so the manager can detect
/// mismatches. MUST track `bilbycast-manager/crates/manager-core/src/models/
/// ws_protocol.rs::WS_PROTOCOL_VERSION` — this edge ships the v4 surface:
/// v3 (capability bits, direction-tagged VAAPI caps, assembly hot-swap, hot
/// add/remove input) PLUS the binary media-upload frame (`Message::Binary`
/// chunk transport, no base64). The manager gates binary-frame sends on
/// `protocol_version >= 4`, so a stale advertisement would make media uploads
/// fail loudly with a compatibility alarm instead of using the binary path.
const WS_PROTOCOL_VERSION: u32 = 4;

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

fn build_health_message(
    flow_manager: &FlowManager,
    api_addr: &str,
    monitor_addr: Option<&str>,
    resource_state: &SystemResourceState,
    static_caps: &crate::engine::hardware_probe::StaticCapabilities,
    live_gpu: &crate::engine::hardware_probe::LiveUtilizationState,
    network_sampler: Option<&mut crate::util::network_interfaces::NetworkSampler>,
    cellular_cache: Option<&Arc<crate::util::cellular::CellularCache>>,
    starlink_cache: Option<&Arc<crate::util::starlink::StarlinkCache>>,
    start_time: Instant,
) -> serde_json::Value {
    serde_json::json!({
        "type": "health",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": build_health_payload(flow_manager, api_addr, monitor_addr, resource_state, static_caps, live_gpu, network_sampler, cellular_cache, starlink_cache, start_time)
    })
}

pub(crate) fn build_health_payload(
    flow_manager: &FlowManager,
    api_addr: &str,
    monitor_addr: Option<&str>,
    resource_state: &SystemResourceState,
    static_caps: &crate::engine::hardware_probe::StaticCapabilities,
    live_gpu: &crate::engine::hardware_probe::LiveUtilizationState,
    network_sampler: Option<&mut crate::util::network_interfaces::NetworkSampler>,
    cellular_cache: Option<&Arc<crate::util::cellular::CellularCache>>,
    starlink_cache: Option<&Arc<crate::util::starlink::StarlinkCache>>,
    start_time: Instant,
) -> serde_json::Value {
    // Capabilities are mostly compile-time/probe-derived; the `"cellular"` bit
    // is runtime — advertised whenever the poller has at least one source
    // (configured RutOS uplink or auto-detected modem), independent of whether
    // a snapshot is currently fresh, so it stays stable while data ages out.
    let mut caps = edge_capabilities();
    if cellular_cache.map(|c| c.has_sources()).unwrap_or(false) {
        caps.push("cellular");
        // `cellular-control` advertises that an operator can wake a dormant
        // USB-modem bond leg from the manager — true only when the host
        // keep-alive daemon is running to service the request (status-file
        // heartbeat fresh). Gated so the UI never shows a dead Wake button.
        if crate::util::cellular::wake_control_available() {
            caps.push("cellular-control");
        }
    }
    // `"starlink"` is the dish-telemetry sibling of `"cellular"` — advertised
    // whenever the Starlink poller has at least one configured dish, so the UI
    // gates the dish signal strip + bond-leg strip on it.
    if starlink_cache.map(|c| c.has_sources()).unwrap_or(false) {
        caps.push("starlink");
    }
    // `"bond-broker"` advertises the shared-leg broker is active (on by
    // default) so the manager UI can render the priority-aware Shared Uplink
    // Contention card. The card itself only appears once a leg is actually
    // shared by ≥2 flows (non-empty `bond_leg_contention`).
    if crate::engine::bond_leg_broker::broker().enabled() {
        caps.push("bond-broker");
    }
    #[allow(unused_mut)]
    let mut payload = serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": start_time.elapsed().as_secs(),
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
        "capabilities": caps,
        "system_resources": build_system_resources_payload(resource_state),
        // Static + live HW / SW resource budget. Manager UI keys off the
        // `"resources"` capability string to render the Resources card
        // and the per-flow Resource-impact widget.
        "resource_budget": build_resource_budget_payload(flow_manager, static_caps, live_gpu),
        // Hot-path scheduling status — surfaces whether the operator's
        // production tuning (mlockall, SCHED_FIFO, RLIMIT_RTPRIO) has
        // actually landed. The manager UI can flag a degraded state
        // ("running at SCHED_OTHER — expect 1–5 ms PCR_AC jitter")
        // without the operator chasing the value through logs.
        // Backward-compatible additive field — old managers ignore it.
        "scheduling_status": build_scheduling_status_payload(),
    });
    // Node-level PTP state — one-shot poll so the manager's Time page
    // shows lock status without requiring a running ST 2110 flow.
    {
        let ptp = crate::engine::st2110::ptp::poll_once_from_config();
        let ptp_settings = crate::util::ptp_config::load();
        if let Ok(v) = serde_json::to_value(&ptp) {
            let obj = payload.as_object_mut().unwrap();
            obj.insert("ptp_state".into(), v);
            obj.insert(
                "ptp_mode".into(),
                serde_json::Value::String(ptp_settings.mode.as_str().to_string()),
            );
        }
    }
    // Per-interface enumeration. Empty Vec → omit (defensive: if_addrs
    // can fail in some containerised environments). Manager UI hides
    // the card unless the `"network-info"` capability is also present.
    {
        // When a long-lived sampler is supplied (manager-client task)
        // we get rate-derivation across ticks; the monitor /health
        // endpoint passes None and falls back to static enumeration.
        let ifaces = match network_sampler {
            Some(sampler) => sampler.sample(),
            None => crate::util::network_interfaces::enumerate(),
        };
        if !ifaces.is_empty() {
            if let Ok(v) = serde_json::to_value(&ifaces) {
                payload
                    .as_object_mut()
                    .map(|o| o.insert("network_interfaces".into(), v));
            }
        }
    }
    // Shared-leg capacity-broker contention — one entry per physical uplink
    // shared by ≥1 bonded flow, each flow's demand vs. its fair-share
    // allocation. Empty → omit. Manager UI gates on the "bond-broker"
    // capability (additive field; older managers ignore it).
    {
        let contention = crate::engine::bond_leg_broker::broker().contention_snapshot();
        if !contention.is_empty() {
            if let Ok(v) = serde_json::to_value(&contention) {
                payload
                    .as_object_mut()
                    .map(|o| o.insert("bond_leg_contention".into(), v));
            }
        }
    }
    // Local-display enumeration. Linux-only and gated on the `display`
    // Cargo feature; absent on every other build so older managers /
    // non-display edges remain wire-compatible.
    #[cfg(all(feature = "display", target_os = "linux"))]
    {
        let devs = crate::display::cached_displays();
        if !devs.is_empty() {
            if let Ok(json) = serde_json::to_value(devs.as_slice()) {
                payload
                    .as_object_mut()
                    .map(|o| o.insert("display_devices".into(), json));
            }
        }
    }
    // Per-port DeckLink hardware status (signal lock, genlock, detected raster,
    // PCIe link). Gated on the `sdi-decklink` Cargo feature; absent on every
    // other build so older managers / non-SDI edges stay wire-compatible. A
    // lock-free atomic load — the ~25 ms SDK sweep runs on the background
    // status poller, never on this tick.
    #[cfg(feature = "sdi-decklink")]
    {
        let ports = crate::engine::decklink::status::cached();
        if !ports.is_empty() {
            if let Ok(json) = serde_json::to_value(ports.as_slice()) {
                payload
                    .as_object_mut()
                    .map(|o| o.insert("sdi_devices".into(), json));
            }
        }
    }
    // System-clock discipline (adjtimex read). Inserted only when probe()
    // returns Some — non-Linux hosts and seccomp-blocked adjtimex omit the
    // field, so older managers and clock-less hosts stay wire-compatible.
    // Manager UI hides the card unless the `"clock-sync"` capability is also
    // present. Reflects the wallclock master's underlying CLOCK_MONOTONIC
    // discipline (chrony / ntpd / phc2sys vs free-running).
    if let Some(status) = crate::util::clock_sync::probe() {
        if let Ok(v) = serde_json::to_value(&status) {
            payload
                .as_object_mut()
                .map(|o| o.insert("clock_sync".into(), v));
        }
    }
    payload
}

fn build_system_resources_payload(state: &SystemResourceState) -> serde_json::Value {
    serde_json::json!({
        "cpu_percent": state.cpu_percent_f64(),
        "ram_percent": state.ram_percent_f64(),
        "ram_used_mb": state.ram_used_mb.load(std::sync::atomic::Ordering::Relaxed),
        "ram_total_mb": state.ram_total_mb.load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// Aggregate hot-path scheduling state captured at startup + accrued
/// per wire-emit thread.
///
/// The manager UI uses `sched_fifo_granted` + `sched_fifo_failed` to
/// flag a "running degraded" warning on the per-node Resources card —
/// silent fallback to SCHED_OTHER is the single most common cause of
/// elevated PCR_AC jitter in field deployments. `mlockall_status`
/// surfaces whether the operator enabled the page-fault stall guard.
fn build_scheduling_status_payload() -> serde_json::Value {
    use crate::util::runtime_diag::{
        mlockall_status, rlimit_rtprio_max, sched_fifo_failed_any, sched_fifo_granted_any,
    };
    serde_json::json!({
        "mlockall": mlockall_status(),
        "sched_fifo_granted": sched_fifo_granted_any(),
        "sched_fifo_failed": sched_fifo_failed_any(),
        "rlimit_rtprio_max": rlimit_rtprio_max(),
    })
}

fn build_resource_budget_payload(
    flow_manager: &FlowManager,
    static_caps: &crate::engine::hardware_probe::StaticCapabilities,
    live_gpu: &crate::engine::hardware_probe::LiveUtilizationState,
) -> serde_json::Value {
    let units_total = crate::engine::hardware_probe::compute_units_total(&static_caps.cpu);
    let units_used = flow_manager.total_cost_units();
    let hw_session_usage = flow_manager.total_hw_sessions();
    let live = live_gpu.snapshot();
    let mut payload = serde_json::json!({
        "hw_encoders": static_caps.hw_encoders,
        "hw_decoders": static_caps.hw_decoders,
        "cpu": static_caps.cpu,
        "sw_capacity": static_caps.sw_capacity,
        "units_total": units_total,
        "units_used": units_used,
        "hw_session_usage": hw_session_usage,
        "live": live,
    });
    // Per-family session limits — additive on the wire; older managers
    // ignore unknown fields. Only emit when at least one family was
    // probed so we don't waste 18 bytes of `{}` per health tick on
    // hosts that have no HW accelerators or where probing was disabled.
    if !static_caps.hw_encoder_session_limits.is_empty() {
        if let Ok(v) = serde_json::to_value(&static_caps.hw_encoder_session_limits) {
            payload
                .as_object_mut()
                .map(|o| o.insert("hw_encoder_session_limits".into(), v));
        }
    }
    if !static_caps.hw_decoder_session_limits.is_empty() {
        if let Ok(v) = serde_json::to_value(&static_caps.hw_decoder_session_limits) {
            payload
                .as_object_mut()
                .map(|o| o.insert("hw_decoder_session_limits".into(), v));
        }
    }
    if !static_caps.hw_encoder_chroma.is_empty() {
        if let Ok(v) = serde_json::to_value(&static_caps.hw_encoder_chroma) {
            payload
                .as_object_mut()
                .map(|o| o.insert("hw_encoder_chroma".into(), v));
        }
    }
    // VAAPI direction-tagged capability (WS_PROTOCOL_VERSION ≥ 2). Older
    // managers ignore the field; newer ones prefer it over the legacy
    // `hw_encoders.{h264,hevc}_vaapi` mirror, which we keep populating
    // for backwards compatibility.
    if !static_caps.vaapi.is_empty() {
        if let Ok(v) = serde_json::to_value(&static_caps.vaapi) {
            payload
                .as_object_mut()
                .map(|o| o.insert("vaapi".into(), v));
        }
    }
    // Live thread inventory — counts of hot-path OS threads currently
    // running. Surfaces the Stage 1 codec-thread fleet, Stage 2 PID-bus
    // threads (when wired), and Stage 3 PCR PLL sampler threads (when
    // wired). The wire-emit count comes from active outputs (one
    // wire-emit thread per UDP-socket-owning output). Additive on the
    // wire — older managers ignore the field.
    let threads_payload = serde_json::json!({
        "codec_pool_count": crate::util::runtime_diag::codec_thread_count(),
    });
    payload
        .as_object_mut()
        .map(|o| o.insert("threads".into(), threads_payload));
    payload
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
        // Resource budget: HW encoder/decoder probe + SW capacity
        // estimate + live unit-utilization. Manager UI keys the
        // Resources card + per-flow Resource-impact widget off this.
        "resources",
        // ETSI TR 101 290 priorities 1, 2, and 3 (P2-extended PTS / CAT
        // / PCR-repetition split + P3 NIT / SDT / EIT / TDT / RST /
        // Unreferenced_PID counters). The manager UI gates the new
        // sub-panel on this — older edges still expose the P1-only
        // surface they always have.
        "tr101290_full",
        // SMPTE 2022-7 seq-aware hitless on the PID-bus pre-bus path —
        // EsPacket now carries upstream_seq + upstream_leg_id, and the
        // assembly Hitless variant accepts a `mode = seq_aware` selector.
        // Manager UI gates the SeqAware option on this capability.
        "hitless_2022_7",
        // Operator-driven N-input switch slot in Flow Assembly. The
        // assembly `SlotSource::Switch` variant carries 1..=64 legs and
        // an `initial_input_id`; `ActivateInput` flips every Switch
        // slot whose leg list contains the named input, with PMT
        // version bump + DI=1 + monotonic CC so receivers stay locked
        // without re-tuning. Manager UI gates the Switch option in
        // Pick Tracks + Advanced editor on this capability so older
        // edges hide the source type.
        "pid_bus_switch_slot",
        // PES Switch redesign Phase 3: the elementary-stream bus is
        // node-wide (Phase 1) and accepts cross-flow assembly slot
        // references (Phase 2.1+); foreign-input Essence slots resolve
        // against the owning flow's PSI catalogue (Phase 2.2b). Manager
        // UI gates the Node Bus matrix view + the bus_route switcher
        // preset action + the master-clock badge in the input picker
        // on this capability — older edges keep the per-flow assembly
        // editor only, with no matrix surface.
        "node_bus",
        // PES Switch redesign Phase 4 (audio-aligned splice MVP): the
        // `SwitchActiveInput` handler honours `splice_mode = pes_aligned`
        // on audio switch slots — buffers the slot's outbound bytes at
        // the from-leg's last fully-emitted PES boundary and concatenates
        // the to-leg's next aligned PES; on budget exhaust falls back to
        // PmtBump and emits `pes_splice_timeout`. Non-audio slots fall
        // through to PmtBump silently (video splice is a follow-up).
        // Manager UI gates the splice_mode dropdown in the assembly
        // editor on this capability so older edges hide it cleanly.
        "pes_splice",
        // Remote binary upgrade: edge accepts `upgrade_binary` WS
        // commands and stages a Sigstore-verified release tarball,
        // atomically swaps the `current` symlink, then drains and exits
        // for systemd respawn. Manager UI gates the upgrade button on
        // this capability so older edges (no upgrade module) hide it.
        "upgrade",
        // Per-flow A/V sync mux + master clock (source PCR PLL / PTP /
        // wallclock fallback). Manager UI gates the master-clock
        // telemetry card + lipsync trim knob on this. FlowStats carries
        // master_clock and the WS dispatcher accepts
        // `set_master_clock_lipsync` commands — older edges return
        // `unknown_action`.
        "master_clock",
        // Edge-added A/V skew (exact lip-sync error from PTS-touching
        // stages, stats::av_skew) + the renamed A/V mux-interleave
        // metric (FlowStats.av_interleave_flow / OutputStats.
        // av_interleave, formerly av_sync*). ADVISORY today: the
        // manager UI gates the split strips on field presence (the
        // master_clock precedent), not on this bit — advertised so
        // future tooling / fleet dashboards can detect the metric
        // generation without parsing stats shapes.
        "av-skew",
        // Per-NIC enumeration on every health tick: name, IPv4/IPv6,
        // MAC, MTU, link speed (Linux), up/down (Linux). Manager UI
        // gates the "Network Interfaces" card on this so older edges
        // hide the surface cleanly. Multi-homed edges expose every
        // wired/virtual interface — operators no longer have to SSH
        // in to find which IP is on which port.
        "network-info",
        // Egress de-jitter: compressed UDP/RTP outputs run a closed-loop
        // release-rate servo + bounded residence-cap shed in `wire_emit`,
        // so a source-rate-vs-wallclock offset or network burst is absorbed
        // (rate-matched) or shed instead of integrating into output latency
        // (the diagnosed latency runaway). Per-output `egress_buffer_ms`
        // tunes the servo setpoint; `OutputStats.egress_shed` +
        // `wire_emit_depth` report it. Manager UI gates the egress-buffer
        // knob + the de-jitter telemetry card on this so older edges hide
        // the surface. ST 2110 keeps strict raster/SO_TXTIME pacing.
        "egress_dejitter",
        // Per-output egress pacing model: compressed UDP/RTP outputs accept
        // `egress_pacing: forward | pcr | servo` (+ `egress_buffer_ms`
        // cushion, servo-only) as plain config — fully manager-configurable,
        // no env vars. Unset = auto, resolved at flow start (`pcr` when the
        // flow has a bonded input, else `forward`). Edges WITHOUT this bit
        // silently ignore the field on config push (serde unknown-field
        // tolerance), so the manager UI MUST gate the pacing dropdown on
        // this capability to avoid the silent-no-op trap.
        "egress_pacing",
        // Ingress de-jitter: raw UDP / RTP inputs run the ingress
        // counterpart to the egress servo — recover the source rate from
        // inter-PCR observations and release packets paced at that rate
        // (leaky bucket + fill servo + residence-cap shed), so every
        // downstream consumer (analysers, PCR PLL, PID bus, SRT/RIST
        // outputs) sees a smooth cadence regardless of network PDV.
        // Per-input `ingress_dejitter_ms` tunes the setpoint;
        // `InputStats.ingress_dejitter_shed` + `ingress_buffer_depth`
        // report it. Cooperates with SMPTE 2022-7 (runs after the merger).
        // Manager UI gates the per-input de-jitter knob on this.
        "ingress_dejitter",
        // Per-input/per-output `interface_binding` field is honoured
        // (loose source-IP binding via NIC name lookup, plus per-leg
        // binding inside 2022-7 redundancy and per-endpoint binding
        // inside SRT bonding). Manager UI keys the picker dropdown off
        // this. Always present from this release on; the strict
        // companion below is conditional on CAP_NET_RAW.
        "interface-binding",
        // Media-aware multi-path bonding (bilbycast-bonding): `bonded`
        // input/output types, adaptive capacity scheduler + congestion
        // knobs, gateway-mode policy routing, session-epoch reset,
        // interface-watcher rebuilds, hold telemetry (`hold_ms`), and
        // the bond stats block on Input/OutputStats. NOTE: the manager
        // UI must NOT hide the bonded surface when this bit is absent
        // — the bonded engine shipped releases before the bit did, so
        // absence is the normal state for already-deployed bonding-
        // capable edges. The bit exists so a future manager can gate
        // NEW bonding sub-features (introduced alongside it) cleanly.
        "bonding",
        // Native plain-UDP tunnel transport (no QUIC): `TunnelConfig.transport
        // = "udp"` carries SRT/RIST over the relay's UDP rendezvous plane (or
        // direct) without QUIC's overhead / second congestion controller.
        // Manager UI gates the "Native (no QUIC)" relay option on this so it's
        // never pushed to an older edge that would reject the transport.
        "native-udp-tunnel",
        // Per-tunnel uplink (NIC) pinning on the native plain-UDP carrier:
        // `TunnelConfig.{interface, source, gateway}` send each tunnel out a
        // specific interface / source IP / gateway — same SO_BINDTODEVICE +
        // policy-route mechanism as a bonded UDP leg. Lets a path-aggregation
        // bond carried over per-leg tunnels use a distinct uplink (5G vs
        // Starlink vs ISP) per leg instead of collapsing onto the default
        // route. Manager UI gates the per-tunnel uplink picker on this so the
        // fields are never pushed to an older edge that would ignore them.
        "tunnel-nic-pin",
    ];
    // Strict mode (`SO_BINDTODEVICE`) requires `CAP_NET_RAW`. Probed
    // once at startup; advertised only when the setsockopt actually
    // succeeds. Operators grant the cap via the
    // `packaging/strict-binding.conf` systemd drop-in. Manager UI
    // gates the strict checkbox on this — no-cap edges still see the
    // (loose-only) picker, just without the strict toggle.
    if crate::util::socket::probe_strict_binding_supported() {
        caps.push("interface-binding-strict");
    }
    if cfg!(feature = "ptp-internal") {
        caps.push("ptp-internal");
    }
    // Bond leg pre-flight test (Phase 1): the manager UI gates the "Test
    // leg" button on this. Emits zero packets, never touches live traffic.
    caps.push("bond-leg-test");
    // Bond leg active capacity probe + peer responder (Phase 2): gates the
    // "Measure capacity" flow. Idle-gated + bounded + isolated — see
    // engine::bond_leg_probe + docs/leg-bandwidth-test.md.
    caps.push("bond-leg-capacity");
    caps.push("bond-probe-responder");
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
        // Stage 2 of the replay UX plan — filmstrip thumbnails on disk
        // for the manager `/replay` scrubber strip. UI checks for this
        // before showing the filmstrip-cadence field on the flow form
        // and before fetching `…/replay/filmstrip` on the timeline.
        caps.push("replay-filmstrip");
        // MP4 export — TS→fMP4 remuxer for the Recordings library
        // download surface. UI uses this to light up the ⬇ MP4 button
        // alongside ⬇ TS. H.264+AAC only; HEVC / non-AAC streams
        // surface `replay_export_format_unsupported`.
        caps.push("replay_export_mp4");
    }
    if cfg!(any(
        feature = "video-encoder-x264",
        feature = "video-encoder-x265",
        feature = "video-encoder-nvenc",
        feature = "video-encoder-qsv",
        feature = "video-encoder-rkmpp"
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
    if cfg!(feature = "video-encoder-vaapi") {
        caps.push("video-encoder-vaapi");
    }
    if cfg!(feature = "video-encoder-rkmpp") {
        caps.push("video-encoder-rkmpp");
    }
    // HW decoder capability strings — used by both the display output
    // and the transcode input-decode path. Advertised when the matching
    // `video-decoder-*` Cargo feature is on AND the runtime probe found
    // at least one decoder in that family usable. The manager UI reads
    // these to populate the "HW Decoder" dropdown on transcode flow
    // outputs (separate from the display-output decoder dropdown which
    // keys off the legacy `display-*` strings emitted below).
    if cfg!(feature = "video-decoder-nvdec") {
        if let Some(c) = crate::engine::hardware_probe::static_capabilities() {
            if c.hw_decoders.h264_nvenc || c.hw_decoders.hevc_nvenc {
                caps.push("video-decoder-nvdec");
            }
        }
    }
    if cfg!(feature = "video-decoder-qsv") {
        if let Some(c) = crate::engine::hardware_probe::static_capabilities() {
            if c.hw_decoders.h264_qsv || c.hw_decoders.hevc_qsv {
                caps.push("video-decoder-qsv");
            }
        }
    }
    if cfg!(feature = "video-decoder-vaapi") {
        if let Some(c) = crate::engine::hardware_probe::static_capabilities() {
            if c.hw_decoders.h264_vaapi || c.hw_decoders.hevc_vaapi {
                caps.push("video-decoder-vaapi");
            }
        }
    }
    if cfg!(feature = "fdk-aac") {
        caps.push("fdk-aac");
    }
    if cfg!(feature = "media-codecs") {
        caps.push("media-codecs");
    }
    if cfg!(feature = "webrtc") {
        caps.push("webrtc");
    }
    if cfg!(feature = "tls") {
        caps.push("tls");
    }
    // Local-display output. Advertised only when (a) the `display`
    // Cargo feature is on, AND (b) the host actually has at least one
    // KMS connector enumerated (so headless build servers don't see
    // the option in the manager UI).
    #[cfg(all(feature = "display", target_os = "linux"))]
    {
        if !crate::display::cached_displays().is_empty() {
            caps.push("display");

            // Per-backend HW-decode capabilities for the display output
            // are advertised via the `video-decoder-{nvdec,qsv,vaapi}`
            // strings pushed earlier — those decoders are shared
            // between transcode and the display output, so the manager
            // UI keys the display "Video Decoder" dropdown off the
            // same capability strings as the transcode resolver.
        }
    }
    // Native SDI capture + playout via Blackmagic DeckLink. Advertised only
    // when the feature is compiled in, the boot probe reached the SDK (Desktop
    // Video installed), and a card is present *now* — the device list comes
    // from the status poller, so it tracks hot-plug in both directions rather
    // than freezing whatever was true at boot. A host with no card therefore
    // never offers SDI in the manager UI, and one that gains a card starts
    // offering it within a poll interval without a restart.
    #[cfg(feature = "sdi-decklink")]
    {
        if crate::engine::decklink::domain::probe_succeeded()
            && !crate::engine::decklink::status::cached().is_empty()
        {
            caps.push("sdi-decklink");
        }
    }
    // Kernel-paced wire emission via SO_TXTIME — gates the per-output
    // `wire_pacing` config knob on ST 2110-20 / -23 outputs in the
    // manager UI. Linux ≥ 4.19 typically advertises; non-Linux + older
    // kernels don't. Note: capability says the kernel ACCEPTS SO_TXTIME
    // — operator-side ETF qdisc setup is still required for actual
    // pacing (see packaging/setup-etf-qdisc.sh).
    if let Some(c) = crate::engine::hardware_probe::static_capabilities() {
        if c.wire_pacing_txtime {
            caps.push("wire_pacing_txtime");
        }
    }
    // System-clock discipline indicator. Advertised only when the adjtimex
    // read succeeds (Linux + syscall permitted). Manager UI gates the "Clock
    // Sync" card on this so non-Linux / seccomp-blocked / older edges hide
    // the surface cleanly. Reflects whether the wallclock master's
    // underlying CLOCK_MONOTONIC is disciplined or free-running.
    if crate::util::clock_sync::probe().is_some() {
        caps.push("clock-sync");
    }
    // MXL (Media eXchange Layer) — advertised per-essence only when the
    // `mxl` Cargo feature is on AND the boot probe successfully dlopen'd
    // libmxl.so. Manager UI keys MXL option dropdowns off these bits.
    // Three per-essence bits parallel the ST 2110-20/-30/-40 pattern so
    // future per-essence build slicing stays clean.
    #[cfg(feature = "mxl")]
    {
        if crate::engine::mxl::domain::probe_succeeded() {
            caps.push("mxl-video");
            caps.push("mxl-audio");
            caps.push("mxl-anc");
        }
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
            // `!=` not `>` — on flow stop+start the accumulator is rebuilt
            // with `generation = 0`, so a freshly-captured `gen = 1` is
            // *less than* the stale `prev` from the previous flow lifetime.
            // Using `>` silently gated every push until the new generation
            // overtook the old (~7 min for a flow that ran ~7 min). `!=`
            // catches both forward progress and the reset-to-zero case.
            if thumb_gen != prev && thumb_gen != 0 {
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
            // See the flow-level note above — same reset-on-restart trap.
            if thumb_gen != prev && thumb_gen != 0 {
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

/// Magic byte prefixing every manager→edge binary WS frame (sanity guard so a
/// stray binary frame from a buggy/old peer is rejected loudly, not misparsed).
const BINARY_FRAME_MAGIC: u8 = 0xBC;
/// Binary frame kind: a single media-library upload chunk.
const BINARY_FRAME_KIND_MEDIA_CHUNK: u8 = 0x01;

/// Handle a binary WS frame from the manager. The only binary frame defined
/// today is a media-upload chunk, framed as:
///
/// ```text
/// [0]        magic   = 0xBC
/// [1]        kind    = 0x01 (media upload chunk)
/// [2..6]     header_len: u32 big-endian
/// [6..6+L]   header JSON: { command_id, name, chunk_index, total_chunks, total_bytes }
/// [6+L..]    raw chunk bytes (no base64)
/// ```
///
/// A structurally unparseable / unknown-kind frame raises a Warning event (the
/// compatibility alarm) and is dropped — there is no `command_id` to ack
/// against, so the manager's per-chunk timeout surfaces the failure. A frame
/// that parses but whose write fails replies with a `command_ack`
/// `success: false` so the manager fails fast instead of waiting out the
/// timeout. Reply rides `out_tx` (single-owner writer rule).
async fn handle_manager_binary(
    frame: &[u8],
    tunnel_manager: &Arc<TunnelManager>,
    out_tx: &mpsc::Sender<String>,
) {
    let alarm = |msg: String| {
        tracing::warn!("{msg}");
        tunnel_manager
            .event_sender()
            .emit(EventSeverity::Warning, category::MANAGER, msg);
    };

    if frame.len() < 6 {
        alarm(format!(
            "manager binary frame too short ({} bytes, need ≥ 6)",
            frame.len()
        ));
        return;
    }
    if frame[0] != BINARY_FRAME_MAGIC {
        alarm(format!(
            "manager binary frame bad magic 0x{:02x} (expected 0x{BINARY_FRAME_MAGIC:02x})",
            frame[0]
        ));
        return;
    }
    if frame[1] != BINARY_FRAME_KIND_MEDIA_CHUNK {
        alarm(format!(
            "manager binary frame unknown kind 0x{:02x} — edge cannot handle it (upgrade mismatch?)",
            frame[1]
        ));
        return;
    }
    let header_len = u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]]) as usize;
    let header_end = 6usize.saturating_add(header_len);
    if header_end > frame.len() {
        alarm(format!(
            "manager binary frame header_len {header_len} exceeds frame size {}",
            frame.len()
        ));
        return;
    }
    let header: serde_json::Value = match serde_json::from_slice(&frame[6..header_end]) {
        Ok(h) => h,
        Err(e) => {
            alarm(format!("manager binary frame header is not valid JSON: {e}"));
            return;
        }
    };
    let command_id = header["command_id"].as_str().unwrap_or("").to_string();
    if command_id.is_empty() {
        alarm("manager binary frame header missing 'command_id'".to_string());
        return;
    }
    let payload = &frame[header_end..];

    // Past this point we have a command_id, so any failure acks success:false
    // (fail fast) rather than letting the manager time out.
    let result = apply_binary_media_chunk(&header, payload).await;

    let mut ack_payload = serde_json::json!({
        "command_id": command_id,
        "success": result.is_ok(),
    });
    match result {
        Ok(data) => {
            ack_payload["data"] = data;
        }
        Err(e) => {
            ack_payload["error"] = serde_json::Value::String(e);
        }
    }
    let ack = serde_json::json!({
        "type": "command_ack",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": ack_payload,
    });
    if let Ok(json) = serde_json::to_string(&ack) {
        let _ = out_tx.send(json).await;
    }
}

/// Parse the media-upload header fields and apply the raw chunk to the library.
/// Returns the serialised `UploadProgress` for the ack's `data` field.
async fn apply_binary_media_chunk(
    header: &serde_json::Value,
    payload: &[u8],
) -> Result<serde_json::Value, String> {
    let name = header["name"]
        .as_str()
        .ok_or("binary upload: missing 'name'")?;
    let chunk_index = header["chunk_index"]
        .as_u64()
        .ok_or("binary upload: missing or invalid 'chunk_index'")? as u32;
    let total_chunks = header["total_chunks"]
        .as_u64()
        .ok_or("binary upload: missing or invalid 'total_chunks'")? as u32;
    let total_bytes = header["total_bytes"]
        .as_u64()
        .ok_or("binary upload: missing or invalid 'total_bytes'")?;
    let progress = crate::media::global()
        .apply_chunk_raw(name, chunk_index, total_chunks, total_bytes, payload)
        .await
        .map_err(|e| format!("binary upload failed: {e}"))?;
    Ok(serde_json::to_value(&progress).unwrap_or_default())
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
                    let input_changed = input_set_changed(&old_flow.input_ids, &new_flow.input_ids);
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
                    let master_clock_changed = old_flow.master_clock != new_flow.master_clock;
                    // Crossing the passthrough↔assembled boundary — or any
                    // assembly-plan change that arrives via `update_flow`
                    // rather than the `update_flow_assembly` hot-swap — must
                    // rebuild the task graph. `FlowRuntime::start` keys
                    // passthrough vs assembled off `assembly`, and the live
                    // SPTS/MPTS assembler task is owned for the flow's
                    // lifetime; the surgical output-only diff below never
                    // tears it down. Without this the assembly-less config
                    // would persist while the assembler keeps emitting, so
                    // config and runtime silently diverge until the next
                    // manual restart. (Same-boundary assembly edits still
                    // hot-swap via `update_flow_assembly`, so this only forces
                    // a restart on genuine plan changes routed through here —
                    // notably the manager's "Convert to passthrough" button.)
                    let assembly_changed = old_flow.assembly != new_flow.assembly;
                    let restart_required = old_flow.bandwidth_limit != new_flow.bandwidth_limit
                        || content_analysis_changed
                        || recording_changed
                        || master_clock_changed
                        || assembly_changed;
                    let persist_only_meta_changed = old_flow.name != new_flow.name
                        || old_flow.media_analysis != new_flow.media_analysis
                        || old_flow.thumbnail != new_flow.thumbnail
                        || old_flow.thumbnail_interval_secs != new_flow.thumbnail_interval_secs;
                    if persist_only_meta_changed && !(input_changed || restart_required) {
                        tracing::info!(
                            "Update flow '{flow_id}': metadata-only change (name/thumbnail/thumbnail_interval/media_analysis) — persisting without restart; thumbnail_interval applies live (below), thumbnail on/off + media_analysis take effect at next flow restart"
                        );
                    }

                    // Phase 2 hot-swap upgrade: an input-set change that
                    // touches a Hitless slot's leg list must fall back to
                    // a full restart because the hitless ES merger is not
                    // designed to grow / shrink legs in flight. For every
                    // other input-set delta the surgical add_input /
                    // remove_input paths apply without dropping outputs.
                    let input_touches_hitless = if input_changed {
                        input_delta_touches_hitless(
                            flow_manager,
                            flow_id,
                            &old_flow.input_ids,
                            &new_flow.input_ids,
                        )
                        .await
                    } else {
                        false
                    };
                    let force_restart = input_touches_hitless || restart_required;

                    if force_restart {
                        // Input change that touches a hitless leg, rate-limit
                        // change, or content-analysis / recording tier toggle
                        // — must restart entire flow.
                        tracing::info!(
                            "Update flow '{flow_id}': restarting (input_touches_hitless={input_touches_hitless}, bandwidth_limit/content_analysis/recording/master_clock/assembly changed={restart_required}, content_analysis_changed={content_analysis_changed}, recording_changed={recording_changed}, master_clock_changed={master_clock_changed}, assembly_changed={assembly_changed})"
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
                    } else if input_changed {
                        // Surgical input + output reconcile — outputs keep
                        // running across the edit. Reconcile inputs first
                        // so an added input is publishing before outputs
                        // are added, and removed inputs are gone before
                        // outputs that depended on them get dropped.
                        tracing::info!(
                            "Update flow '{flow_id}': surgical input + output reconcile (inputs old→new: {} → {}, outputs old→new: {} → {})",
                            old_flow.input_ids.len(),
                            new_flow.input_ids.len(),
                            old_flow.output_ids.len(),
                            new_flow.output_ids.len(),
                        );
                        let cfg = app_config.read().await;
                        diff_inputs(
                            flow_manager,
                            flow_id,
                            &new_flow.input_ids,
                            &cfg.inputs,
                            _webrtc_sessions,
                        )
                        .await;
                        diff_outputs(flow_manager, flow_id, &new_flow.output_ids, &cfg).await;
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

                    // Thumbnail cadence applies LIVE — push it to the running
                    // generators so it takes effect immediately instead of at
                    // the next flow restart. Skip when the flow was restarted
                    // above (a restart respawns the generators with the new
                    // config already).
                    if !force_restart
                        && old_flow.thumbnail_interval_secs != new_flow.thumbnail_interval_secs
                    {
                        match flow_manager
                            .apply_thumbnail_interval(flow_id, new_flow.thumbnail_interval_secs)
                        {
                            Ok(()) => tracing::info!(
                                "Update flow '{flow_id}': applied thumbnail cadence live ({:?})",
                                new_flow.thumbnail_interval_secs
                            ),
                            Err(e) => {
                                tracing::debug!("Live thumbnail cadence apply skipped: {e}")
                            }
                        }
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
            persist_config(&cfg, config_path, secrets_path).await?;
            Ok(None)
        }
        "update_flow_assembly" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let new_assembly: FlowAssembly = serde_json::from_value(action["assembly"].clone())
                .map_err(|e| format!("Invalid assembly: {e}"))?;
            // Optional per-swap splice request (additive field; absent on
            // older managers). Slot retargeting via plan replacement is
            // inherently a PMT-bump cut — `pes_aligned` can't be honoured
            // there and the assembler emits `splice_override_ignored` so
            // the operator's choice is never silently dropped.
            let splice_mode_override: Option<crate::config::models::SpliceMode> =
                action.get("splice_mode_override").and_then(|v| match v {
                    serde_json::Value::String(s) => match s.as_str() {
                        "pmt_bump" => Some(crate::config::models::SpliceMode::PmtBump),
                        "pes_aligned" => Some(crate::config::models::SpliceMode::PesAligned),
                        _ => None,
                    },
                    _ => None,
                });
            // Schema validation — the hot-swap path bypasses validate_flow
            // (config-load / create_flow / update_flow only), so without
            // this an assembly with out-of-range PIDs or out_pid/pmt_pid
            // collisions reached the running assembler unchecked.
            crate::config::validation::validate_assembly_for_hotswap(
                &new_assembly,
                &format!("Flow '{flow_id}'"),
            )
            .map_err(|e| CommandError::from_validation("Invalid assembly", e))?;
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
            if let Err(e) = runtime
                .replace_assembly(new_assembly.clone(), splice_mode_override)
                .await
            {
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
            persist_config(&cfg, config_path, secrets_path).await?;
            Ok(None)
        }
        "delete_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: delete flow '{flow_id}'");
            if flow_manager.is_running(flow_id) {
                flow_manager
                    .destroy_flow(flow_id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            // Remove from config
            let mut cfg = app_config.write().await;
            cfg.flows.retain(|f| f.id != flow_id);
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
            Ok(None)
        }
        "add_input" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?.to_string();
            let input: InputDefinition =
                serde_json::from_value(action["input"].clone())
                    .map_err(|e| format!("Invalid input config: {e}"))?;
            validate_input_definition(&input)
                .map_err(|e| CommandError::from_validation("Invalid input config", e))?;
            let input_id = input.id.clone();
            tracing::info!(
                "Manager command: add input '{input_id}' to flow '{flow_id}' (hot-add)"
            );

            let spawn_started_at = std::time::Instant::now();
            let hot_added = match flow_manager.add_input(&flow_id, input.clone()).await {
                Ok(info) => info,
                Err(e) => {
                    // Recover structured InputMembershipError code if present.
                    if let Some(membership_err) =
                        e.downcast_ref::<crate::engine::flow::InputMembershipError>()
                    {
                        return Err(CommandError::with_code(
                            membership_err.message.clone(),
                            membership_err.code,
                        ));
                    }
                    return Err(CommandError::new(e.to_string()));
                }
            };

            // Listener inputs (SRT-listener, RIST, RTSP, WHIP server) bind
            // asynchronously. Watch the event channel briefly for a bind
            // failure scoped to this flow; if one fires, roll back via
            // `remove_input` and surface a structured `port_conflict` /
            // `bind_failed` on the ack so the manager UI can highlight the
            // offending field.
            if let Some(err) = wait_for_first_bind_failure(
                flow_manager.event_sender(),
                &flow_id,
                spawn_started_at,
            )
            .await
            {
                let _ = flow_manager.remove_input(&flow_id, &input_id).await;
                return Err(err);
            }

            // Register the WHIP-server session channel with the WebrtcSessionRegistry
            // so browsers can pair against the hot-added input without restarting
            // the flow. Pre-Phase-1 behaviour was to spawn the input task but skip
            // registry registration (operators had to restart). Surfacing
            // `whip_info` from `add_input` closes that gap.
            #[cfg(feature = "webrtc")]
            if let Some((tx, bearer_token)) = hot_added.whip_info {
                if let Some(registry) = _webrtc_sessions {
                    registry.register_whip_input(&flow_id, tx, bearer_token);
                    tracing::info!(
                        "Registered hot-added WHIP input '{input_id}' for flow '{flow_id}'"
                    );
                }
            }
            // Suppress unused-variable warnings when webrtc feature is off.
            #[cfg(not(feature = "webrtc"))]
            let _ = hot_added;

            // Persist: ensure the input definition exists in cfg.inputs and
            // the flow's input_ids carries the id.
            let mut cfg = app_config.write().await;
            if !cfg.inputs.iter().any(|i| i.id == input_id) {
                cfg.inputs.push(input);
            }
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                if !flow.input_ids.contains(&input_id) {
                    flow.input_ids.push(input_id.clone());
                }
            }
            persist_config(&cfg, config_path, secrets_path).await?;
            Ok(None)
        }
        "remove_input" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?.to_string();
            let input_id = action["input_id"].as_str().ok_or("Missing input_id")?.to_string();
            tracing::info!(
                "Manager command: remove input '{input_id}' from flow '{flow_id}' (hot-remove)"
            );

            if let Err(e) = flow_manager.remove_input(&flow_id, &input_id).await {
                if let Some(membership_err) =
                    e.downcast_ref::<crate::engine::flow::InputMembershipError>()
                {
                    return Err(CommandError::with_code(
                        membership_err.message.clone(),
                        membership_err.code,
                    ));
                }
                return Err(CommandError::new(e.to_string()));
            }

            // Persist: drop the id from the flow's input_ids, then garbage-
            // collect the top-level inputs entry if no other flow still
            // references it. Mirrors the symmetric `add_input` persist.
            let mut cfg = app_config.write().await;
            if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.input_ids.retain(|id| id != &input_id);
            }
            let still_referenced = cfg
                .flows
                .iter()
                .any(|f| f.input_ids.iter().any(|id| id == &input_id));
            if !still_referenced {
                cfg.inputs.retain(|i| i.id != input_id);
            }
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
            flow_manager.event_sender().emit_input(
                EventSeverity::Info, category::FLOW,
                format!("Input '{}' updated", input_id), input_id,
            );
            Ok(None)
        }
        "activate_input" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?.to_string();
            let input_id = action["input_id"].as_str().ok_or("Missing input_id")?.to_string();
            // PES Switch Phase 4 — optional per-switch override.
            // Beats the slot's config-time `splice_mode` for this one
            // switch only; the persisted config is untouched. Manager
            // UI surfaces (Switcher preset, Assembly editor Take) emit
            // this when the operator wants to force a specific splice
            // path. Unknown/invalid values fall through to None so old
            // edges still accept the command.
            let splice_mode_override: Option<crate::config::models::SpliceMode> =
                action.get("splice_mode_override").and_then(|v| match v {
                    serde_json::Value::String(s) => match s.as_str() {
                        "pmt_bump" => Some(crate::config::models::SpliceMode::PmtBump),
                        "pes_aligned" => Some(crate::config::models::SpliceMode::PesAligned),
                        _ => None,
                    },
                    _ => None,
                });
            tracing::info!(
                "Manager command: activate_input '{input_id}' on flow '{flow_id}' splice_mode_override={:?}",
                splice_mode_override,
            );
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
            // Assembled flows: activate_input only drives Switch-slot legs.
            // A flow whose slots are all pinned (Pid / Essence / Hitless)
            // used to ACK success and emit "active input switched" while
            // the media kept playing the pinned source — a silent lie that
            // made the switcher look broken. Refuse with a structured code
            // so the manager can route the Take through a bus_route swap
            // (its activate-input proxy does this for SPTS) or tell the
            // operator to re-point via the Node Bus matrix.
            if let Some(asm) = cfg.flows[flow_idx].assembly.as_ref().filter(|a| {
                !matches!(a.kind, crate::config::models::AssemblyKind::Passthrough)
            }) {
                let has_matching_leg = asm.programs.iter().any(|p| {
                    p.streams.iter().any(|s| match &s.source {
                        crate::config::models::SlotSource::Switch { legs, .. } => {
                            legs.iter().any(|l| l.input_id() == input_id)
                        }
                        _ => false,
                    })
                });
                if !has_matching_leg {
                    return Err(CommandError::with_code(
                        format!(
                            "Flow '{flow_id}' is assembled and no switch slot carries a \
                             leg for input '{input_id}' — activate_input would have no \
                             effect on the media. Re-point the program via the Node Bus \
                             Swap (bus_route), or author switch legs on the slots."
                        ),
                        "pid_bus_activate_input_no_switch_slots",
                    ));
                }
            }
            // Drive the runtime switch BEFORE flipping the config's active
            // flags — a failed switch must not persist a config that claims
            // the new input is active (and the error must reach the ack
            // instead of being swallowed into a tracing::warn).
            if flow_manager.is_running(&flow_id) {
                flow_manager
                    .switch_active_input(&flow_id, &input_id, splice_mode_override)
                    .await
                    .map_err(|e| {
                        CommandError::new(format!("activate_input failed: {e}"))
                    })?;
            }
            let member_ids: Vec<String> = cfg.flows[flow_idx].input_ids.clone();
            for def in cfg.inputs.iter_mut() {
                if member_ids.contains(&def.id) {
                    def.active = def.id == input_id;
                }
            }
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
            // Hot-swap on the running flow if the config actually changed.
            // Surface a failed swap to the operator instead of swallowing it
            // with `let _ =`: a silent failure leaves the runtime on the OLD
            // definition (or with no output at all, if remove succeeded but
            // add failed) while the manager believes the edit applied — the
            // exact trap behind "I changed the display connector and nothing
            // happened, had to restart the node".
            let mut swap_error: Option<String> = None;
            if let Some(flow) = cfg.flow_using_output(output_id).cloned() {
                if flow_manager.is_running(&flow.id) && old_output != output {
                    if let Err(e) = flow_manager.remove_output(&flow.id, output_id).await {
                        tracing::warn!(
                            "update_output '{output_id}': remove_output on flow '{}' failed: {e}",
                            flow.id
                        );
                    }
                    if let Err(e) = flow_manager.add_output(&flow.id, output.clone()).await {
                        tracing::warn!(
                            "update_output '{output_id}': add_output on flow '{}' failed: {e}",
                            flow.id
                        );
                        swap_error = Some(e.to_string());
                    }
                }
            }
            // Persist regardless: config.json is the operator's stated intent
            // and the change still takes effect at the next (re)start even if
            // the live hot-swap failed.
            persist_config(&cfg, config_path, secrets_path).await?;
            if let Some(e) = &swap_error {
                flow_manager.event_sender().emit_output(
                    EventSeverity::Warning, category::FLOW,
                    format!("Output '{output_id}' saved but live hot-swap failed: {e}"),
                    output_id,
                );
                return Err(CommandError::with_code(
                    format!(
                        "output '{output_id}' configuration saved but the running output could \
                         not be restarted with the new settings: {e}"
                    ),
                    "output_hotswap_failed",
                ));
            }
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
            persist_config(&cfg, config_path, secrets_path).await?;
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
            persist_config(&cfg, config_path, secrets_path).await?;
            Ok(None)
        }
        "delete_tunnel" => {
            let tunnel_id = action["tunnel_id"].as_str().ok_or("Missing tunnel_id")?;
            tracing::info!("Manager command: delete tunnel '{tunnel_id}'");
            // Tolerate a tunnel that already self-evicted from the runtime
            // registry (failed to establish): destroy_tunnel returns Ok(false)
            // rather than erroring, so we still reconcile it out of config.json
            // and persist. Otherwise an orphaned entry could never be deleted
            // ("Tunnel not found") and would resurrect on the next restart.
            let was_live = tunnel_manager
                .destroy_tunnel(tunnel_id)
                .await
                .map_err(|e| e.to_string())?;
            let mut cfg = app_config.write().await;
            let before = cfg.tunnels.len();
            cfg.tunnels.retain(|t| t.id != tunnel_id);
            let removed_from_config = cfg.tunnels.len() != before;
            if was_live || removed_from_config {
                persist_config(&cfg, config_path, secrets_path).await?;
            }
            Ok(None)
        }
        "set_bond_uplinks" => {
            // Merge/upsert (or replace) the host-level `bond_uplinks` the
            // shared-leg capacity broker uses, WITHOUT a whole-config round-trip
            // — the edge mutates its own running config so real secrets stay
            // intact (no redaction/clobber hazard). Drives the bonded-link
            // wizard's uplink-capacity capture + the AI configurator's
            // `set_bond_uplinks` action. Applies live on the broker's next tick.
            let replace = action["replace"].as_bool().unwrap_or(false);
            let incoming: Vec<crate::config::models::BondUplinkConfig> = serde_json::from_value(
                action.get("bond_uplinks").cloned().unwrap_or(serde_json::json!([])),
            )
            .map_err(|e| format!("Invalid bond_uplinks: {e}"))?;
            let mut cfg = app_config.write().await;
            if replace {
                cfg.bond_uplinks = incoming;
            } else {
                for u in incoming {
                    if let Some(ex) =
                        cfg.bond_uplinks.iter_mut().find(|x| x.interface == u.interface)
                    {
                        *ex = u;
                    } else {
                        cfg.bond_uplinks.push(u);
                    }
                }
            }
            if let Some(v) = action.get("shared_leg_broker") {
                if v.is_null() {
                    cfg.shared_leg_broker = None;
                } else if let Some(b) = v.as_bool() {
                    cfg.shared_leg_broker = Some(b);
                }
            }
            crate::config::validation::validate_bond_uplinks(&cfg.bond_uplinks)
                .map_err(|e| e.to_string())?;
            persist_config(&cfg, config_path, secrets_path).await?;
            crate::engine::bond_leg_broker::broker()
                .configure(&cfg.bond_uplinks, cfg.shared_leg_broker);
            let n = cfg.bond_uplinks.len();
            drop(cfg);
            tracing::info!("Manager command: set_bond_uplinks → {n} uplink(s), broker enabled={}",
                crate::engine::bond_leg_broker::broker().enabled());
            Ok(Some(serde_json::json!({ "bond_uplinks_count": n })))
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

            // The manager doesn't manage these local-only fields. Preserve the
            // node's existing values so an update_config push doesn't wipe
            // operator-configured local settings.
            if new_config.upgrades.is_none() {
                new_config.upgrades = old_config.upgrades.clone();
            }
            if new_config.monitor.is_none() {
                new_config.monitor = old_config.monitor.clone();
            }
            if new_config.resource_limits.is_none() {
                new_config.resource_limits = old_config.resource_limits.clone();
            }
            if new_config.logging.is_none() {
                new_config.logging = old_config.logging.clone();
            }
            if new_config.nmos_registration.is_none() {
                new_config.nmos_registration = old_config.nmos_registration.clone();
            }
            if new_config.device_name.is_none() {
                new_config.device_name = old_config.device_name.clone();
            }

            validate_config(&new_config).map_err(|e| CommandError::from_validation("Invalid config", e))?;
            tracing::info!("Manager command: update_config (diff-based)");

            tracing::info!("Config diff: old={} flows/{} tunnels, new={} flows/{} tunnels",
                old_config.flows.len(), old_config.tunnels.len(),
                new_config.flows.len(), new_config.tunnels.len());

            // Re-apply shared-leg capacity-broker uplinks so bond_uplinks /
            // shared_leg_broker edits take effect on reload (not just at boot).
            // Runs before the flow diff so restarted bonded flows register
            // against the new config; a disable reverts live legs to
            // unconstrained.
            crate::engine::bond_leg_broker::broker()
                .configure(&new_config.bond_uplinks, new_config.shared_leg_broker);

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
                            let input_changed = input_set_changed(&old_flow.input_ids, &new_flow.input_ids);
                            let restart_required = old_flow.bandwidth_limit != new_flow.bandwidth_limit;
                            let persist_only_meta_changed = old_flow.name != new_flow.name
                                || old_flow.media_analysis != new_flow.media_analysis
                                || old_flow.thumbnail != new_flow.thumbnail
                                || old_flow.thumbnail_interval_secs != new_flow.thumbnail_interval_secs;
                            if persist_only_meta_changed && !(input_changed || restart_required) {
                                tracing::info!(
                                    "Config diff: flow '{id}' metadata-only change — persisting without restart; thumbnail/media_analysis settings apply at next flow restart"
                                );
                            }

                            // Phase 2 hot-swap upgrade: an input-set change
                            // that touches a Hitless slot's leg list must
                            // fall back to a full restart; every other
                            // input-set change uses the surgical add/remove
                            // path so outputs keep running.
                            let input_touches_hitless = if input_changed {
                                input_delta_touches_hitless(
                                    flow_manager,
                                    id,
                                    &old_flow.input_ids,
                                    &new_flow.input_ids,
                                )
                                .await
                            } else {
                                false
                            };
                            let force_restart = input_touches_hitless || restart_required;

                            if force_restart {
                                tracing::info!(
                                    "Config diff: restarting flow '{id}' (input_touches_hitless={input_touches_hitless}, bandwidth_limit changed={restart_required})"
                                );
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
                            } else if input_changed {
                                tracing::info!(
                                    "Config diff: flow '{id}' surgical input + output reconcile (inputs: {} → {}, outputs: {} → {})",
                                    old_flow.input_ids.len(),
                                    new_flow.input_ids.len(),
                                    old_flow.output_ids.len(),
                                    new_flow.output_ids.len(),
                                );
                                diff_inputs(
                                    flow_manager,
                                    id,
                                    &new_flow.input_ids,
                                    &new_config.inputs,
                                    _webrtc_sessions,
                                )
                                .await;
                                diff_outputs_with_configs(
                                    flow_manager,
                                    id,
                                    &new_flow.output_ids,
                                    &old_config,
                                    &new_config,
                                )
                                .await;
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
                // Cellular monitoring uplinks (RutOS routers). The slow poller
                // reads these from the live AppConfig each cycle; without this
                // copy a manager-added uplink is parsed + ACKed but silently
                // dropped on apply (never reaches the poller or config.json).
                cfg.cellular_uplinks = new_config.cellular_uplinks.clone();
                // Starlink dish telemetry sources — same live-poller + silent-drop
                // class as cellular_uplinks above; the poller re-reads these from
                // the live AppConfig each cycle.
                cfg.starlink_uplinks = new_config.starlink_uplinks.clone();
                // Shared-leg broker config — same silent-drop class. `configure()`
                // was already re-applied live above, but without copying these the
                // in-memory config + config.json keep the OLD value, so a UI
                // add/edit/remove of a policy cap (or the broker toggle) is ACKed
                // yet reverts on the next config fetch / restart.
                cfg.bond_uplinks = new_config.bond_uplinks.clone();
                cfg.shared_leg_broker = new_config.shared_leg_broker;
                // Same silent-drop class as cellular_uplinks above: these were
                // restored-if-None from old_config earlier, but a manager-pushed
                // NEW value must also be applied here or it's ACKed yet lost.
                cfg.logging = new_config.logging.clone();
                cfg.nmos_registration = new_config.nmos_registration.clone();
                cfg.upgrades = new_config.upgrades.clone();
                if let Err(e) = save_config_split_async(config_path.clone(), secrets_path.clone(), cfg.clone()).await {
                    tracing::error!("Failed to persist config after manager command: {e}");
                    tunnel_manager.event_sender().emit(
                        EventSeverity::Warning,
                        category::CONFIG,
                        format!("Failed to persist configuration: {e}"),
                    );
                    // Surface the failure to the manager rather than acking
                    // success: the diff was applied to the live engine + the
                    // in-memory config, but config.json on disk is now stale, so
                    // a restart would revert it (e.g. resurrect a removed
                    // tunnel) — the same Issue-16 divergence the dedicated
                    // create/delete_tunnel handlers now guard against.
                    return Err(CommandError::with_code(
                        format!("config change applied in memory but failed to persist to disk: {e}"),
                        "config_persist_failed",
                    ));
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
        // ── Cellular monitoring: one-shot reachability probe ──
        // Powers the manager UI "Test reachability" button: builds a transient
        // RutOS source from the supplied (or stored) credentials, performs
        // login + modem-status fetch, and returns pass/fail with the exact
        // cause — so creds / API access are validated at config time instead of
        // save-and-pray.
        "test_cellular_uplink" => {
            use crate::config::models::CellularUplinkConfig;
            let interface = action["interface"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("test")
                .to_string();
            let address = action["address"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("test_cellular_uplink: missing 'address'")?
                .to_string();
            let username = action["username"].as_str().map(|s| s.to_string());
            // Password: use the supplied one; if blank, fall back to the stored
            // secret for this interface (edit-mode "leave blank to keep").
            let mut password = action["password"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            if password.is_none() {
                let cfg = app_config.read().await;
                password = cfg
                    .cellular_uplinks
                    .iter()
                    .find(|u| u.interface == interface)
                    .and_then(|u| u.password.clone());
            }
            let probe = CellularUplinkConfig {
                interface: interface.clone(),
                kind: "rutos".to_string(),
                scheme: action["scheme"].as_str().unwrap_or("https").to_string(),
                address,
                api: action["api"].as_str().unwrap_or("ubus").to_string(),
                username,
                password,
                verify_tls: action["verify_tls"].as_bool().unwrap_or(false),
                cert_fingerprint: action["cert_fingerprint"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string()),
            };
            let sources =
                crate::util::cellular::rutos::build_sources(std::slice::from_ref(&probe));
            let src = sources
                .values()
                .next()
                .ok_or("test_cellular_uplink: could not build probe (check interface/address)")?;
            let result = match src.sample().await {
                Ok(m) => serde_json::json!({
                    "ok": true,
                    "operator": m.operator,
                    "access_tech": m.access_tech,
                    "band": m.band,
                    "signal": m.signal,
                    "state": m.state,
                }),
                Err(reason) => serde_json::json!({ "ok": false, "error": reason }),
            };
            Ok(Some(result))
        }
        // Probe a Starlink dish's reachability without persisting — mirrors
        // `test_cellular_uplink`. Runs one `get_status` gRPC round-trip against
        // the supplied (or stored) address and returns the decoded link state or
        // the failure cause, so the dish endpoint / route are validated at config
        // time instead of save-and-pray.
        "test_starlink_uplink" => {
            use crate::config::models::StarlinkUplinkConfig;
            let interface = action["interface"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("test")
                .to_string();
            let address = action["address"]
                .as_str()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(crate::config::models::DEFAULT_DISH_ADDR)
                .to_string();
            let source_address = action["source_address"]
                .as_str()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let probe = StarlinkUplinkConfig {
                interface,
                address,
                source_address,
                gateway: None,
            };
            let sources = crate::util::starlink::grpc::build_sources(std::slice::from_ref(&probe));
            let src = sources
                .values()
                .next()
                .ok_or("test_starlink_uplink: could not build probe (check interface/address)")?;
            let result = match src.sample().await {
                Ok(m) => serde_json::json!({
                    "ok": true,
                    "state": m.state,
                    "software_version": m.software_version,
                    "hardware_version": m.hardware_version,
                    "device_id": m.device_id,
                    "obstruction_fraction": m.obstruction_fraction,
                    "pop_ping_latency_ms": m.pop_ping_latency_ms,
                    "pop_ping_drop_rate": m.pop_ping_drop_rate,
                    "downlink_bps": m.downlink_bps,
                    "uplink_bps": m.uplink_bps,
                    "bars": m.bars,
                    "alerts": m.alerts,
                }),
                Err(reason) => serde_json::json!({ "ok": false, "error": reason }),
            };
            Ok(Some(result))
        }
        // ── Cellular control: wake / connect a host-owned USB-modem bond leg ──
        // The cellular telemetry path is read-only and the edge can't drive
        // ModemManager directly (polkit Device.Control denies a headless
        // service). Instead the edge drops a request file that the opt-in root
        // keep-alive daemon (bilbycast-cellular-modem.service) executes, then
        // reports back the result. See docs/cellular.md ("request/execute
        // split"). The edge gains no modem privilege — it only touches a
        // bilbycast-owned file. Gated by the `cellular-control` capability.
        "wake_uplink" => {
            let interface = action["interface"]
                .as_str()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .ok_or("wake_uplink: missing 'interface'")?;
            crate::config::validation::validate_interface_name(interface, "wake_uplink")
                .map_err(|e| e.to_string())?;
            // Optional APN override — the operator can fix a wrong APN from the
            // UI; the keeper's env-file APN is the fallback when this is unset.
            // Validated (incl. as an injection guard — it reaches the keeper's
            // request file + mmcli --simple-connect arg).
            let apn = action["apn"]
                .as_str()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            if let Some(a) = apn {
                crate::config::validation::validate_apn(a, "wake_uplink")
                    .map_err(|e| e.to_string())?;
            }
            let outcome = crate::util::cellular::request_wake(interface, apn).await?;
            Ok(Some(serde_json::to_value(&outcome).unwrap_or_default()))
        }
        // ── Bond leg pre-flight test (zero-impact usability check) ──
        // Powers the manager UI "Test leg" button. Validates that a bond
        // leg is correctly wired (NIC up, source IP present, egresses the
        // intended interface and doesn't collapse onto the default route,
        // first-hop reachable) WITHOUT putting any media on the wire and
        // WITHOUT touching a live BondSocket/scheduler/route — so it is
        // safe to run even while the bond is carrying live traffic. The
        // active throughput probe (Phase 2) is idle-gated and lives
        // elsewhere. See engine::bond_leg_test + the bonding project's
        // docs/leg-bandwidth-test.md.
        "test_bond_leg" => {
            use crate::config::models::BondPathTransportConfig;
            let transport: BondPathTransportConfig =
                serde_json::from_value(action["transport"].clone())
                    .map_err(|e| format!("test_bond_leg: invalid 'transport': {e}"))?;
            let mode = action["mode"].as_str().unwrap_or("usability");
            match mode {
                // ── Phase 1: usability (zero packets) ──
                "usability" => {
                    let leg_live = action["leg_live"].as_bool().unwrap_or(false);
                    let report = tokio::task::spawn_blocking(move || {
                        crate::engine::bond_leg_test::test_leg(&transport, leg_live)
                    })
                    .await
                    .map_err(|e| format!("test_bond_leg: probe task failed: {e}"))?;
                    Ok(Some(serde_json::to_value(&report).unwrap_or_default()))
                }
                // ── Phase 2: active probe against a peer responder ──
                "reachability" | "capacity" => {
                    use crate::engine::bond_leg_probe::{self, CapacityOpts, ProbeMode};
                    let responder_str = action["responder"].as_str().ok_or(
                        "test_bond_leg: 'responder' (ip:port) is required for reachability/capacity mode",
                    )?;
                    let responder: std::net::SocketAddr = responder_str.parse().map_err(|_| {
                        format!("test_bond_leg: invalid responder address '{responder_str}'")
                    })?;
                    let net = crate::engine::bond_leg_test::normalize(&transport);
                    let pm = if mode == "capacity" {
                        ProbeMode::Capacity
                    } else {
                        ProbeMode::Reachability
                    };
                    // Zero-impact gate: refuse a capacity ramp that would
                    // share a physical link with a running bonded flow.
                    if pm == ProbeMode::Capacity {
                        let busy = bond_leg_probe::busy_keys();
                        if let Some(reason) = bond_leg_probe::capacity_conflict(&net, &busy) {
                            return Err(CommandError::with_code(
                                format!("capacity probe refused (would affect live traffic): {reason}"),
                                "bond_leg_test_unsafe_link_busy",
                            ));
                        }
                    }
                    let dur = action["duration_secs"].as_u64().unwrap_or(8).clamp(1, 60);
                    let max_bps = action["max_bitrate_bps"].as_u64().filter(|v| *v > 0);
                    let opts = CapacityOpts {
                        duration: std::time::Duration::from_secs(dur),
                        max_bps,
                        ..CapacityOpts::default()
                    };
                    let report = bond_leg_probe::run_leg_probe(net, responder, pm, opts).await;
                    Ok(Some(serde_json::to_value(&report).unwrap_or_default()))
                }
                other => Err(CommandError::new(format!(
                    "test_bond_leg: unknown mode '{other}' (use usability | reachability | capacity)"
                ))),
            }
        }
        // ── Bond probe responder (the cooperating far end for a capacity
        //    test). Stateless echo on its own socket/port; auto-expires;
        //    isolated from every live media socket. ──
        "start_bond_probe_responder" => {
            let bind_str = action["bind"].as_str().unwrap_or("0.0.0.0:0");
            let bind: std::net::SocketAddr = bind_str
                .parse()
                .map_err(|_| format!("start_bond_probe_responder: invalid bind '{bind_str}'"))?;
            let dur = action["duration_secs"].as_u64().unwrap_or(30).clamp(1, 120);
            let port = crate::engine::bond_leg_probe::start_responder(
                bind,
                std::time::Duration::from_secs(dur),
            )
            .await?;
            Ok(Some(serde_json::json!({
                "ok": true,
                "port": port,
                "bind": bind_str,
                "expires_in_secs": dur,
            })))
        }
        "stop_bond_probe_responder" => {
            let port = action["port"]
                .as_u64()
                .filter(|p| *p <= u16::MAX as u64)
                .ok_or("stop_bond_probe_responder: valid 'port' required")?
                as u16;
            let stopped = crate::engine::bond_leg_probe::stop_responder(port);
            Ok(Some(serde_json::json!({ "ok": true, "stopped": stopped })))
        }
        // ── Media library commands (file-backed media-player input) ──
        "list_media" => {
            let files = crate::media::MediaLibrary::list()
                .await
                .map_err(|e| format!("list_media failed: {e}"))?;
            Ok(Some(serde_json::json!({ "files": files })))
        }
        "scan_media" => {
            // Probe a media-library file's PAT to enumerate its programs.
            // Powers the media-player input UI's program-picker dropdown
            // so an operator never has to guess program numbers when
            // down-selecting an MPTS to a single program.
            let name = action["name"]
                .as_str()
                .ok_or("scan_media: missing 'name'")?;
            let result = crate::media::MediaLibrary::scan_programs(name)
                .await
                .map_err(|e| format!("scan_media failed: {e}"))?;
            Ok(Some(serde_json::to_value(&result).unwrap_or_default()))
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
                persist_config(&cfg, config_path, secrets_path).await?;
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
        "list_filmstrip" => {
            // Returns a windowed batch of filmstrip frame metadata for
            // the manager UI's scrubber strip. Inline base64 JPEG is
            // intentionally NOT returned here — even at 200 frames ×
            // 5 KB the response would be ~1 MB, more than the manager
            // wants to forward in a single WS round-trip. The UI fetches
            // each JPEG via the companion `get_filmstrip_frame` command
            // (proxied through the manager REST layer with strong cache
            // headers, since `<pts>.jpg` is content-addressed and never
            // changes once written).
            //
            // Resolution: same flow_id-or-recording_id fallback as
            // `list_clips`, plus optional `from_pts_90khz` /
            // `to_pts_90khz` window and a `max_count` evenly-spaced
            // downsample (default 60, max 200) so a 1-hour recording
            // doesn't return 720 entries on every scrub render.
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
                return Err(CommandError::new("list_filmstrip: provide flow_id or recording_id".to_string()));
            };
            let from_pts = action["from_pts_90khz"].as_u64();
            let to_pts = action["to_pts_90khz"].as_u64();
            let max_count = action["max_count"].as_u64().unwrap_or(60).clamp(1, 200) as usize;
            let dir = crate::replay::recording_dir(&recording_id);
            let frames = crate::replay::filmstrip::list_frames(&dir, from_pts, to_pts).await
                .map_err(|e| CommandError::new(format!("list_filmstrip failed: {e}")))?;
            // Evenly-spaced downsample. We always include the first and
            // last entries when `frames.len() > max_count` so the scrub
            // strip's edges line up with the requested window.
            let downsampled: Vec<&(u64, u64)> = if frames.len() <= max_count {
                frames.iter().collect()
            } else {
                let n = frames.len();
                (0..max_count)
                    .map(|i| &frames[i * (n - 1) / (max_count - 1)])
                    .collect()
            };
            let frames_json: Vec<serde_json::Value> = downsampled
                .iter()
                .map(|(pts, size)| serde_json::json!({
                    "pts_90khz": pts,
                    "size_bytes": size,
                }))
                .collect();
            Ok(Some(serde_json::json!({
                "recording_id": recording_id,
                "frames": frames_json,
                "total_count": frames.len(),
            })))
        }
        #[cfg(feature = "replay")]
        "get_filmstrip_frame" => {
            // Returns one filmstrip JPEG by exact PTS as base64 — the
            // companion to `list_filmstrip`. PTS values are content-
            // addressed (never re-written once flushed), so the manager
            // forwards with `Cache-Control: public, max-age=86400` and
            // strong ETag.
            let recording_id = action["recording_id"].as_str()
                .ok_or("get_filmstrip_frame: missing 'recording_id'")?
                .to_string();
            let pts_90khz = action["pts_90khz"].as_u64()
                .ok_or("get_filmstrip_frame: missing or invalid 'pts_90khz'")?;
            let dir = crate::replay::recording_dir(&recording_id);
            let bytes = crate::replay::filmstrip::read_frame(&dir, pts_90khz).await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => CommandError::with_code(
                        format!("filmstrip frame {pts_90khz} not found"),
                        "replay_filmstrip_frame_not_found",
                    ),
                    _ => CommandError::new(format!("get_filmstrip_frame failed: {e}")),
                })?;
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(Some(serde_json::json!({
                "recording_id": recording_id,
                "pts_90khz": pts_90khz,
                "data": b64,
                "width": 160,
                "height": 90,
            })))
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
        "list_recordings" => {
            // Library-style enumeration of every on-disk recording under
            // the replay root. Decorates each row with `flow_id` (the
            // owning flow if one is currently armed against it) and
            // `armed` (the writer is rolling). Recordings whose flow
            // either has recording disabled now or whose flow is gone
            // entirely surface as `flow_id: null, armed: false` so the
            // manager UI can render them with an `(orphan)` chip.
            let root = crate::replay::replay_root();
            let summaries = crate::replay::recordings::enumerate(&root).await;
            let armed_map: std::collections::HashMap<String, String> = flow_manager
                .flows_with_recording()
                .into_iter()
                .map(|(flow, rec)| (rec, flow))
                .collect();
            let (free, total) = crate::replay::replay_disk_usage().unwrap_or((0, 0));
            let recordings_json: Vec<serde_json::Value> = summaries
                .into_iter()
                .map(|s| {
                    let owning_flow = armed_map.get(&s.recording_id).cloned();
                    let armed = owning_flow.is_some();
                    serde_json::json!({
                        "recording_id": s.recording_id,
                        "flow_id": owning_flow,
                        "armed": armed,
                        "segment_count": s.segment_count,
                        "total_bytes": s.total_bytes,
                        "first_pts_90khz": s.first_pts_90khz,
                        "last_pts_90khz": s.last_pts_90khz,
                        "created_at_unix": s.created_at_unix,
                        "last_modified_unix": s.last_modified_unix,
                        "clip_count": s.clip_count,
                    })
                })
                .collect();
            Ok(Some(serde_json::json!({
                "recordings": recordings_json,
                "replay_root_free_bytes": free,
                "replay_root_total_bytes": total,
            })))
        }
        #[cfg(feature = "replay")]
        "delete_recording" => {
            // Refuse to nuke a recording that the writer is currently
            // appending to — operator must `stop_recording` first. The
            // disk-side delete is otherwise irreversible, so we lift
            // `replay_recording_active` onto `command_ack.error_code`
            // and let the manager UI surface a precise error toast.
            let recording_id = action["recording_id"].as_str()
                .ok_or("delete_recording: missing 'recording_id'")?
                .to_string();
            let armed_map: std::collections::HashMap<String, String> = flow_manager
                .flows_with_recording()
                .into_iter()
                .map(|(flow, rec)| (rec, flow))
                .collect();
            if let Some(flow_id) = armed_map.get(&recording_id) {
                return Err(CommandError::with_code(
                    format!(
                        "recording '{recording_id}' is currently armed by flow '{flow_id}' — \
                         stop the recording first"
                    ),
                    "replay_recording_active",
                ));
            }
            let root = crate::replay::replay_root();
            let bytes_freed = crate::replay::recordings::delete_recording(&root, &recording_id)
                .await
                .map_err(|e| CommandError::with_code(
                    e.to_string(),
                    "replay_storage_id_invalid",
                ))?;
            flow_manager.event_sender().emit_with_details(
                crate::manager::events::EventSeverity::Info,
                crate::manager::events::category::REPLAY,
                format!(
                    "Recording '{recording_id}' deleted ({} bytes freed)",
                    bytes_freed
                ),
                None,
                serde_json::json!({
                    "replay_event": "recording_deleted",
                    "recording_id": recording_id,
                    "bytes_freed": bytes_freed,
                }),
            );
            Ok(Some(serde_json::json!({
                "recording_id": recording_id,
                "bytes_freed": bytes_freed,
            })))
        }
        #[cfg(feature = "replay")]
        "export_clip" => {
            // Pull-based chunked TS export. The manager calls repeatedly
            // with increasing `byte_offset` until the response carries
            // `eof: true`. `format` is reserved — Phase 1 ships TS only;
            // requests for other formats short-circuit so the manager
            // gets a clean error instead of a silent TS download.
            let clip_id = action["clip_id"].as_str()
                .ok_or("export_clip: missing 'clip_id'")?
                .to_string();
            let format = action["format"].as_str().unwrap_or("ts");
            if format != "ts" && format != "mp4" {
                return Err(CommandError::with_code(
                    format!("export_clip: format '{format}' not supported (ts | mp4)"),
                    "replay_export_format_unsupported",
                ));
            }
            let byte_offset = action["byte_offset"].as_u64().unwrap_or(0);
            let max_bytes = action["chunk_bytes"].as_u64()
                .unwrap_or(crate::replay::export::DEFAULT_CHUNK_BYTES);
            // Locate the recording that owns the clip — same walk as
            // the get_clip / rename_clip / delete_clip arms.
            let root = crate::replay::replay_root();
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
                    if store.get(&clip_id).await.is_some() {
                        owning_recording = Some(rid);
                        break;
                    }
                }
            }
            let recording_id = owning_recording.ok_or_else(|| CommandError::with_code(
                format!("Clip '{clip_id}' not found"),
                "replay_clip_not_found",
            ))?;
            let chunk = if format == "mp4" {
                crate::replay::export_mp4::export_clip_mp4_chunk(
                    &recording_id, &clip_id, byte_offset, max_bytes,
                ).await
            } else {
                crate::replay::export::export_clip_chunk(
                    &recording_id, &clip_id, byte_offset, max_bytes,
                ).await
            }.map_err(|e| {
                let code = match e.to_string().as_str() {
                    s if s.contains("replay_invalid_range") => "replay_invalid_range",
                    s if s.contains("replay_clip_not_found") => "replay_clip_not_found",
                    s if s.contains("replay_no_index") => "replay_no_index",
                    s if s.contains("replay_export_format_unsupported") => "replay_export_format_unsupported",
                    s if s.contains("replay_export_too_large") => "replay_export_too_large",
                    _ => "replay_export_failed",
                };
                CommandError::with_code(e.to_string(), code)
            })?;
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&chunk.data);
            Ok(Some(serde_json::json!({
                "clip_id": clip_id,
                "recording_id": recording_id,
                "format": format,
                "byte_offset": chunk.byte_offset,
                "total_bytes": chunk.total_bytes,
                "chunk_bytes": chunk.data.len(),
                "data": b64,
                "eof": chunk.eof,
            })))
        }
        #[cfg(feature = "replay")]
        "export_recording" => {
            // Whole-recording export, optionally bounded by [from_pts, to_pts].
            // Same pull-based chunked shape as export_clip. The
            // edge-side cap (replay::export::MAX_EXPORT_TOTAL_BYTES)
            // lifts `replay_export_too_large` for >4 GiB exports —
            // operator marks a clip first.
            let recording_id = action["recording_id"].as_str()
                .ok_or("export_recording: missing 'recording_id'")?
                .to_string();
            let format = action["format"].as_str().unwrap_or("ts");
            if format != "ts" && format != "mp4" {
                return Err(CommandError::with_code(
                    format!("export_recording: format '{format}' not supported (ts | mp4)"),
                    "replay_export_format_unsupported",
                ));
            }
            let from_pts = action["from_pts_90khz"].as_u64();
            let to_pts = action["to_pts_90khz"].as_u64();
            let byte_offset = action["byte_offset"].as_u64().unwrap_or(0);
            let max_bytes = action["chunk_bytes"].as_u64()
                .unwrap_or(crate::replay::export::DEFAULT_CHUNK_BYTES);
            let chunk = if format == "mp4" {
                crate::replay::export_mp4::export_recording_mp4_chunk(
                    &recording_id, from_pts, to_pts, byte_offset, max_bytes,
                ).await
            } else {
                crate::replay::export::export_recording_chunk(
                    &recording_id, from_pts, to_pts, byte_offset, max_bytes,
                ).await
            }.map_err(|e| {
                let code = match e.to_string().as_str() {
                    s if s.contains("replay_invalid_range") => "replay_invalid_range",
                    s if s.contains("replay_clip_not_found") => "replay_clip_not_found",
                    s if s.contains("replay_no_index") => "replay_no_index",
                    s if s.contains("replay_no_segments") => "replay_no_segments",
                    s if s.contains("replay_export_too_large") => "replay_export_too_large",
                    s if s.contains("replay_export_format_unsupported") => "replay_export_format_unsupported",
                    _ => "replay_export_failed",
                };
                CommandError::with_code(e.to_string(), code)
            })?;
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&chunk.data);
            Ok(Some(serde_json::json!({
                "recording_id": recording_id,
                "format": format,
                "byte_offset": chunk.byte_offset,
                "total_bytes": chunk.total_bytes,
                "chunk_bytes": chunk.data.len(),
                "data": b64,
                "eof": chunk.eof,
                "from_pts_90khz": from_pts,
                "to_pts_90khz": to_pts,
            })))
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
        "upgrade_binary" => {
            let coord = crate::upgrade::global_coordinator().ok_or_else(|| {
                CommandError::with_code(
                    "upgrades not configured on this node — set upgrades.enabled = true in config.json",
                    crate::upgrade::error_codes::UPGRADE_DISABLED,
                )
            })?;
            let version = action["version"].as_str().ok_or_else(|| {
                CommandError::with_code(
                    "upgrade_binary: missing 'version'",
                    crate::upgrade::error_codes::UPGRADE_VERSION_INVALID,
                )
            })?;
            let channel = action["channel"].as_str().ok_or_else(|| {
                CommandError::with_code(
                    "upgrade_binary: missing 'channel'",
                    crate::upgrade::error_codes::UPGRADE_CHANNEL_NOT_ALLOWED,
                )
            })?;
            let target_arch = action["target_arch"].as_str();
            let variant = action["variant"].as_str();

            let staged = coord
                .stage(version, channel, target_arch, variant)
                .await
                .map_err(|e| CommandError::with_code(e.message.clone(), e.code))?;

            // Stage succeeded — drain flows and exit so systemd respawns
            // into the new binary. The ack we return below races the
            // exit, but tokio is fast enough that the manager sees the
            // ack first ~99% of the time. The manager already treats a
            // missed ack the same way (the new edge re-authenticates
            // with `software_version = <new>` on its first beat).
            let from_v_log = staged.from_version.clone();
            let to_v_log = staged.to_version.clone();
            let flow_manager_clone = flow_manager.clone();
            tokio::spawn(async move {
                tracing::info!(
                    "upgrade staged ({from_v_log} → {to_v_log}); draining flows for respawn"
                );
                // 5 s drain so any in-flight TS packet completes its
                // hop and outputs flush their socket buffers. systemd
                // sees a clean exit(0) and respawns into `current/`.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let _ = flow_manager_clone;
                std::process::exit(0);
            });
            Ok(Some(serde_json::json!({
                "status": "staged",
                "from_version": staged.from_version,
                "to_version": staged.to_version,
                "channel": staged.channel,
                "variant": staged.variant,
                "arch": staged.arch,
            })))
        }
        "get_ptp_mode" => {
            // Reads /var/lib/bilbycast/ptp.conf and returns the current
            // operator-facing settings + a list of NICs the helper would
            // pick from. Manager UI uses this to populate the Time page.
            let settings = crate::util::ptp_config::load();
            let ifaces: Vec<String> = crate::util::network_interfaces::enumerate()
                .into_iter()
                .filter(|n| !n.is_loopback && n.mac.is_some() && n.is_up.unwrap_or(true))
                .map(|n| n.name)
                .collect();
            Ok(Some(serde_json::json!({
                "mode": settings.mode.as_str(),
                "iface": settings.iface,
                "domain": settings.domain,
                "priority1": settings.priority1,
                "scan_timeout": settings.scan_timeout,
                "offset_warn_ns": settings.offset_warn_ns,
                "path_delay_warn_ns": settings.path_delay_warn_ns,
                "config_path": crate::util::ptp_config::config_path().display().to_string(),
                "available_ifaces": ifaces,
            })))
        }
        "set_ptp_mode" => {
            // Writes /var/lib/bilbycast/ptp.conf with the operator's
            // selection; `bilbycast-ptp-helper` picks it up via its
            // 1 Hz mtime watch and restarts ptp4l + phc2sys with the
            // new role. No sudo path needed at runtime — the helper's
            // systemd unit owns the ambient caps. Same dispatch is
            // mirrored at PUT /api/v1/ptp for direct REST clients.
            use crate::util::ptp_config::{PtpMode, PtpSettings};
            let mode_str = action["mode"].as_str().ok_or_else(|| {
                CommandError::with_code("set_ptp_mode: missing 'mode'", "missing_field")
            })?;
            let mode = match mode_str.to_ascii_lowercase().as_str() {
                "auto" => PtpMode::Auto,
                "grandmaster" | "gm" | "master" => PtpMode::Grandmaster,
                "slave-only" | "slave" => PtpMode::SlaveOnly,
                "off" | "disabled" | "none" => PtpMode::Off,
                _ => {
                    return Err(CommandError::with_code(
                        format!(
                            "set_ptp_mode: unknown mode '{mode_str}' \
                             (expected auto, grandmaster, slave-only, off)"
                        ),
                        "invalid_value",
                    ));
                }
            };
            let iface = action["iface"].as_str().unwrap_or("").to_string();
            let domain = action["domain"].as_u64().and_then(|d| u8::try_from(d).ok());
            let priority1 = action["priority1"].as_u64().and_then(|p| u8::try_from(p).ok());
            let scan_timeout = action["scan_timeout"]
                .as_u64()
                .and_then(|s| u8::try_from(s).ok());
            let offset_warn_ns = action["offset_warn_ns"].as_i64();
            let path_delay_warn_ns = action["path_delay_warn_ns"].as_i64();
            let settings = PtpSettings {
                mode,
                iface,
                domain,
                priority1,
                scan_timeout,
                offset_warn_ns,
                path_delay_warn_ns,
            }
            .normalised();
            // Defense-in-depth — iface goes via a file the privileged
            // helper reads; reject newline/length/shell-metachar attacks
            // before the bytes ever hit disk.
            if let Err(e) = settings.validate() {
                return Err(CommandError::with_code(
                    format!("set_ptp_mode: invalid settings: {e}"),
                    "invalid_value",
                ));
            }
            let conf_path = crate::util::ptp_config::config_path();
            if let Err(e) = crate::util::ptp_config::save(&settings) {
                return Err(CommandError::with_code(
                    format!(
                        "Cannot write PTP config to {}: {e}. \
                         Set BILBYCAST_PTP_CONF_PATH to override, \
                         or create the directory with write access.",
                        conf_path.display()
                    ),
                    "ptp_config_write_failed",
                ));
            }
            // bilbycast-ptp-helper picks it up via 1 Hz mtime poll;
            // we don't have to do anything else here. Return the
            // applied settings echo so the operator UI confirms the
            // value the edge actually wrote (after normalisation).
            Ok(Some(serde_json::json!({
                "mode": settings.mode.as_str(),
                "iface": settings.iface,
                "domain": settings.domain,
                "priority1": settings.priority1,
                "scan_timeout": settings.scan_timeout,
                "offset_warn_ns": settings.offset_warn_ns,
                "path_delay_warn_ns": settings.path_delay_warn_ns,
                "config_path": crate::util::ptp_config::config_path().display().to_string(),
            })))
        }
        "set_master_clock_lipsync" => {
            let flow_id = action["flow_id"].as_str().ok_or_else(|| {
                CommandError::with_code(
                    "set_master_clock_lipsync: missing 'flow_id'",
                    "missing_field",
                )
            })?;
            let lipsync_offset_90k = action["lipsync_offset_90k"]
                .as_i64()
                .ok_or_else(|| {
                    CommandError::with_code(
                        "set_master_clock_lipsync: missing 'lipsync_offset_90k'",
                        "missing_field",
                    )
                })?;
            let runtime = flow_manager.get_runtime(flow_id).ok_or_else(|| {
                CommandError::with_code(format!("Unknown flow '{flow_id}'"), "unknown_flow")
            })?;
            // Bounded ±200 ms. The handle clamps internally; we mirror
            // the clamp here so the operator's wire value reflects the
            // accepted value when echoed back via FlowStats.
            let clamped = lipsync_offset_90k.clamp(-18_000, 18_000);
            runtime.master_clock().set_lipsync_offset_90k(clamped);
            // Mirror onto FlowStats so the next snapshot shows the new
            // trim without waiting for the 1 Hz telemetry tick.
            runtime.stats.set_master_clock_lipsync(clamped);
            Ok(Some(serde_json::json!({
                "lipsync_offset_90k": clamped,
            })))
        }
        "reset_counters" => {
            let flow_id = action["flow_id"].as_str().ok_or_else(|| {
                CommandError::with_code("reset_counters: missing 'flow_id'", "missing_field")
            })?;
            let scope = action["scope"].as_str().unwrap_or("all");
            let runtime = flow_manager.get_runtime(flow_id).ok_or_else(|| {
                CommandError::with_code(
                    format!("reset_counters: unknown flow '{flow_id}'"),
                    "unknown_flow",
                )
            })?;
            runtime.stats.reset_counters(scope);
            tracing::info!(flow_id, scope, "operator reset counters");
            Ok(Some(serde_json::json!({
                "flow_id": flow_id,
                "scope": scope,
            })))
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

/// Surgically reconcile a running flow's input set against `new_input_ids`,
/// dispatching `add_input` / `remove_input` per delta. The output side is
/// not touched — call [`diff_outputs`] afterwards if outputs also changed.
///
/// `input_defs` provides the canonical [`InputDefinition`] lookup for the
/// added side. For `update_flow` this is the manager's current `cfg.inputs`;
/// for `update_config` it's `new_config.inputs`.
///
/// **Hitless-leg constraint**: the caller must have already checked that no
/// added or removed id appears in `FlowManager::flow_hitless_inputs(flow_id)`
/// and routed to a full restart in that case. This helper assumes the
/// hitless-safe path.
///
/// Bind failures on hot-added listener inputs (SRT-listener / RIST / RTSP /
/// WHIP server) are detected via [`wait_for_first_bind_failure`]; on
/// failure the just-added input is rolled back via `remove_input`. The
/// helper continues processing the remaining deltas rather than aborting
/// the whole reconcile — a `port_conflict` on one input shouldn't block
/// applying changes to another.
/// Whether two `input_ids` lists differ as **sets** (order-insensitive).
///
/// The manager's flow modal rebuilds `input_ids` from the checkbox DOM, which
/// is rendered in the node's *global inputs* order — not the flow's stored
/// `input_ids` order. So a metadata-only edit (e.g. changing the thumbnail
/// cadence) can re-serialise `input_ids` in a different order with no real
/// membership change. An ordered `!=` comparison would treat that as an input
/// change and route the edit through the hitless-check + surgical reconcile
/// (or even a full restart) path, momentarily disturbing the active input —
/// the flow "freezes" until the operator re-activates a source in the
/// switcher. Input order carries no runtime meaning (`diff_inputs`, the
/// active-input selection, and hitless legs are all keyed by id, not
/// position), so a pure reorder must be a no-op.
fn input_set_changed(old: &[String], new: &[String]) -> bool {
    if old.len() != new.len() {
        return true;
    }
    let mut a: Vec<&str> = old.iter().map(|s| s.as_str()).collect();
    let mut b: Vec<&str> = new.iter().map(|s| s.as_str()).collect();
    a.sort_unstable();
    b.sort_unstable();
    a != b
}

#[cfg(test)]
mod input_diff_tests {
    use super::input_set_changed;

    fn v(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reorder_is_not_a_change() {
        // The freeze regression: the flow modal re-serialises input_ids in a
        // different order on a metadata-only edit. A pure reorder must NOT
        // register as an input-set change.
        assert!(!input_set_changed(&v(&["a", "b"]), &v(&["b", "a"])));
        assert!(!input_set_changed(&v(&["a", "b", "c"]), &v(&["c", "a", "b"])));
        assert!(!input_set_changed(&v(&["a"]), &v(&["a"])));
        assert!(!input_set_changed(&v(&[]), &v(&[])));
    }

    #[test]
    fn add_or_remove_is_a_change() {
        assert!(input_set_changed(&v(&["a"]), &v(&["a", "b"]))); // added
        assert!(input_set_changed(&v(&["a", "b"]), &v(&["a"]))); // removed
        assert!(input_set_changed(&v(&["a"]), &v(&["b"]))); // swapped
        assert!(input_set_changed(&v(&[]), &v(&["a"]))); // empty → one
        assert!(input_set_changed(&v(&["a"]), &v(&[]))); // one → empty (drops all)
    }
}

async fn diff_inputs(
    flow_manager: &Arc<FlowManager>,
    flow_id: &str,
    new_input_ids: &[String],
    input_defs: &[InputDefinition],
    webrtc_sessions: &WebrtcRegistry,
) {
    use std::collections::HashSet;

    let running: Vec<String> = flow_manager
        .running_input_ids(flow_id)
        .await
        .unwrap_or_default();
    let running_set: HashSet<&str> = running.iter().map(|s| s.as_str()).collect();
    let new_set: HashSet<&str> = new_input_ids.iter().map(|s| s.as_str()).collect();

    tracing::info!(
        "Flow '{flow_id}' input reconcile: running={:?} -> new={:?}",
        running,
        new_input_ids,
    );

    // Remove inputs no longer referenced.
    for id in running_set.iter().copied() {
        if !new_set.contains(id) {
            tracing::info!(
                "Config diff: removing input '{id}' from flow '{flow_id}' (running, not in new config)"
            );
            if let Err(e) = flow_manager.remove_input(flow_id, id).await {
                tracing::warn!("Failed to remove input '{id}' from flow '{flow_id}': {e}");
            }
        }
    }

    // Add newly referenced inputs.
    for &id in &new_set {
        if running_set.contains(id) {
            continue;
        }
        let Some(input_def) = input_defs.iter().find(|d| d.id == id) else {
            tracing::warn!(
                "Config diff: input '{id}' referenced by flow '{flow_id}' but not found in top-level inputs"
            );
            continue;
        };
        tracing::info!("Config diff: adding input '{id}' to flow '{flow_id}'");
        let spawn_started_at = std::time::Instant::now();
        let hot_added = match flow_manager.add_input(flow_id, input_def.clone()).await {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!("Failed to add input '{id}' to flow '{flow_id}': {e}");
                continue;
            }
        };
        if let Some(err) = wait_for_first_bind_failure(
            flow_manager.event_sender(),
            flow_id,
            spawn_started_at,
        )
        .await
        {
            tracing::warn!(
                "Input '{id}' bind failure on flow '{flow_id}': {} — rolling back",
                err.message,
            );
            let _ = flow_manager.remove_input(flow_id, id).await;
            continue;
        }
        // Register WHIP session channel if this hot-added input is a
        // WHIP server, so browsers can pair without a flow restart.
        #[cfg(feature = "webrtc")]
        if let Some((tx, bearer_token)) = hot_added.whip_info {
            if let Some(registry) = webrtc_sessions {
                registry.register_whip_input(flow_id, tx, bearer_token);
                tracing::info!(
                    "Registered hot-added WHIP input '{id}' for flow '{flow_id}' (diff path)"
                );
            }
        }
        #[cfg(not(feature = "webrtc"))]
        let _ = hot_added;
    }
    #[cfg(not(feature = "webrtc"))]
    let _ = webrtc_sessions;
}

/// Detect whether the delta between `old_ids` and `new_ids` would touch the
/// hitless leg list of a running flow. Used to gate the surgical input diff
/// — when any added/removed id appears in the current hitless inputs the
/// caller must fall back to a full flow restart instead.
async fn input_delta_touches_hitless(
    flow_manager: &FlowManager,
    flow_id: &str,
    old_ids: &[String],
    new_ids: &[String],
) -> bool {
    use std::collections::HashSet;
    let old_set: HashSet<&str> = old_ids.iter().map(|s| s.as_str()).collect();
    let new_set: HashSet<&str> = new_ids.iter().map(|s| s.as_str()).collect();
    if old_set == new_set {
        return false;
    }
    let Some(hitless) = flow_manager.flow_hitless_inputs(flow_id).await else {
        return false;
    };
    if hitless.is_empty() {
        return false;
    }
    // Any id in the symmetric difference that's also in the hitless set
    // would change the merger's leg list.
    for id in old_set.symmetric_difference(&new_set) {
        if hitless.contains(*id) {
            return true;
        }
    }
    false
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
                flow_manager.event_sender().emit_output(
                    EventSeverity::Warning, category::FLOW,
                    format!("Output '{id}' could not start during config sync: {e}"), id,
                );
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
                        flow_manager.event_sender().emit_output(
                            EventSeverity::Warning, category::FLOW,
                            format!("Output '{id}' could not restart with new settings during config sync: {e}"), id,
                        );
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
                        flow_manager.event_sender().emit_output(
                            EventSeverity::Warning, category::FLOW,
                            format!("Output '{id}' could not restart during config sync: {e}"), id,
                        );
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
/// Persist the in-memory config to disk after a manager command mutated it.
///
/// Returns the failure as a `CommandError` (code `config_persist_failed`) so the
/// caller propagates it into the `command_ack` rather than silently acking
/// success. A swallowed persist failure is exactly the Issue-16 trap: the live
/// engine + in-memory `AppConfig` reflect the change (e.g. a deleted tunnel) but
/// `config.json` on disk still holds the old entry, so the next restart
/// resurrects it while the manager believes the mutation stuck. Surfacing the
/// error lets the manager/operator see the on-disk/runtime divergence
/// immediately — the local REST tunnel handlers (`api::tunnels`) already behave
/// this way; this brings the manager-WS path to parity.
async fn persist_config(
    config: &AppConfig,
    config_path: &PathBuf,
    secrets_path: &PathBuf,
) -> Result<(), CommandError> {
    if let Err(e) =
        save_config_split_async(config_path.clone(), secrets_path.clone(), config.clone()).await
    {
        tracing::error!("Failed to persist config after manager command: {e}");
        return Err(CommandError::with_code(
            format!("config change applied in memory but failed to persist to disk: {e}"),
            "config_persist_failed",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod persist_config_tests {
    use super::persist_config;
    use crate::config::models::AppConfig;
    use std::path::PathBuf;

    /// Issue-16 guard: a manager command that mutates config but then fails to
    /// write config.json must surface a `config_persist_failed` CommandError, not
    /// silently ack success. Previously the failure was swallowed (warn-only), so
    /// the manager believed e.g. a tunnel deletion stuck while config.json on disk
    /// still held it — resurrecting it on the next restart. Here an unwritable
    /// path (parent dir absent → atomic temp-write fails) stands in for the
    /// real-world permission/disk-full cause the customer hit.
    #[tokio::test]
    async fn persist_failure_surfaces_command_error() {
        let bad = PathBuf::from("/nonexistent-bilbycast-persist-test-dir/config.json");
        let bad_secrets = PathBuf::from("/nonexistent-bilbycast-persist-test-dir/secrets.json");
        let err = persist_config(&AppConfig::default(), &bad, &bad_secrets)
            .await
            .expect_err("persist to a nonexistent directory must fail, not be swallowed");
        assert_eq!(err.code.as_deref(), Some("config_persist_failed"));
    }

    /// The happy path returns Ok and actually writes config.json to disk.
    #[tokio::test]
    async fn persist_success_writes_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.json");
        let secrets_path = dir.path().join("secrets.json");
        persist_config(&AppConfig::default(), &cfg_path, &secrets_path)
            .await
            .expect("persist to a writable directory must succeed");
        assert!(cfg_path.exists(), "config.json should be written on the success path");
    }
}
