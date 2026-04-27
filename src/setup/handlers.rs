// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::api::server::AppState;
use crate::config::persistence::save_config_split_async;
use crate::config::validation::validate_config;
use crate::manager::ManagerConfig;

use super::wizard::{SETUP_DISABLED_HTML, SETUP_HTML};

/// Request body for POST /setup.
///
/// `manager_urls` carries the full ordered list the wizard collected
/// from the operator — one to sixteen entries. Single-instance
/// deployments still send a one-element array; the schema does not
/// accept a scalar `manager_url` any more.
#[derive(Debug, Deserialize)]
pub struct SetupPayload {
    pub listen_addr: Option<String>,
    pub listen_port: Option<u16>,
    pub manager_urls: Vec<String>,
    pub accept_self_signed_cert: Option<bool>,
    pub registration_token: Option<String>,
    pub device_name: Option<String>,
}

/// Response body for POST /setup and GET /setup/status.
#[derive(Serialize)]
pub struct SetupResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Current setup-relevant config values for pre-filling the form.
#[derive(Serialize)]
pub struct SetupStatus {
    pub listen_addr: String,
    pub listen_port: u16,
    pub manager_urls: Vec<String>,
    pub accept_self_signed_cert: bool,
    pub registration_token: Option<String>,
    pub device_name: Option<String>,
    pub setup_enabled: bool,
}

/// GET /setup — serves the setup wizard HTML page.
pub async fn setup_page(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    if !config.setup_enabled {
        return Html(SETUP_DISABLED_HTML);
    }
    Html(SETUP_HTML)
}

/// GET /setup/status — returns current setup-relevant config as JSON.
pub async fn setup_status(State(state): State<AppState>) -> Json<SetupStatus> {
    let config = state.config.read().await;
    let (manager_urls, accept_self_signed) = match &config.manager {
        Some(m) => (m.urls.clone(), m.accept_self_signed_cert),
        None => (Vec::new(), false),
    };
    Json(SetupStatus {
        listen_addr: config.server.listen_addr.clone(),
        listen_port: config.server.listen_port,
        manager_urls,
        accept_self_signed_cert: accept_self_signed,
        // Never expose the registration token — it's a secret
        registration_token: None,
        device_name: config.device_name.clone(),
        setup_enabled: config.setup_enabled,
    })
}

/// POST /setup — validates and saves setup configuration.
///
/// Auth model: requests originating from a loopback address (127.0.0.0/8 or ::1)
/// bypass the bearer-token check — an operator who is already on the box has
/// authenticated by other means (SSH, local console). Non-loopback callers must
/// supply `Authorization: Bearer <setup_token>`, where `setup_token` is the
/// one-shot token auto-generated on first boot and printed to stdout.
pub async fn apply_setup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SetupPayload>,
) -> impl IntoResponse {
    // Check if setup is enabled, and (for non-loopback callers) verify the
    // one-shot bearer token in constant time.
    {
        let config = state.config.read().await;
        if !config.setup_enabled {
            return (
                StatusCode::FORBIDDEN,
                Json(SetupResponse {
                    success: false,
                    message: None,
                    error: Some("Setup wizard is disabled on this node".to_string()),
                }),
            );
        }
        if !peer.ip().is_loopback() {
            let expected = config.setup_token.as_deref().unwrap_or("");
            let provided = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            if expected.is_empty()
                || !bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(SetupResponse {
                        success: false,
                        message: None,
                        error: Some(
                            "setup token required: supply `Authorization: Bearer <token>`. \
                             Run `bilbycast-edge --print-setup-token` on the node, or check the \
                             first-boot stdout banner. Loopback callers bypass this check."
                                .to_string(),
                        ),
                    }),
                );
            }
        }
    }

    // Validate manager URL list (1..16, wss://, ≤2048 chars, unique).
    let manager_urls: Vec<String> = payload
        .manager_urls
        .iter()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect();
    if manager_urls.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(SetupResponse {
                success: false,
                message: None,
                error: Some(
                    "At least one manager URL is required (single-instance deploys still use a 1-entry list)".to_string(),
                ),
            }),
        );
    }
    if manager_urls.len() > 16 {
        return (
            StatusCode::BAD_REQUEST,
            Json(SetupResponse {
                success: false,
                message: None,
                error: Some(
                    "At most 16 manager URLs are permitted; front a larger cluster with a load balancer.".to_string(),
                ),
            }),
        );
    }
    for url in &manager_urls {
        if !url.starts_with("wss://") {
            return (
                StatusCode::BAD_REQUEST,
                Json(SetupResponse {
                    success: false,
                    message: None,
                    error: Some(format!("Manager URL {url:?} must start with wss:// (TLS required)")),
                }),
            );
        }
        if url.len() > 2048 {
            return (
                StatusCode::BAD_REQUEST,
                Json(SetupResponse {
                    success: false,
                    message: None,
                    error: Some(format!("Manager URL {url:?} must be at most 2048 characters")),
                }),
            );
        }
    }

    // Validate registration token length
    if let Some(ref token) = payload.registration_token {
        if token.len() > 4096 {
            return (
                StatusCode::BAD_REQUEST,
                Json(SetupResponse {
                    success: false,
                    message: None,
                    error: Some(
                        "Registration token must be at most 4096 characters".to_string(),
                    ),
                }),
            );
        }
    }

    // Validate device name length
    if let Some(ref name) = payload.device_name {
        if name.len() > 256 {
            return (
                StatusCode::BAD_REQUEST,
                Json(SetupResponse {
                    success: false,
                    message: None,
                    error: Some("Device name must be at most 256 characters".to_string()),
                }),
            );
        }
    }

    // Validate listen port
    if let Some(port) = payload.listen_port {
        if port == 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(SetupResponse {
                    success: false,
                    message: None,
                    error: Some("Listen port must be between 1 and 65535".to_string()),
                }),
            );
        }
    }

    // Patch the config
    let mut config = state.config.write().await;

    if let Some(ref addr) = payload.listen_addr {
        config.server.listen_addr = addr.trim().to_string();
    }
    if let Some(port) = payload.listen_port {
        config.server.listen_port = port;
    }

    config.device_name = payload.device_name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());

    let registration_token = payload
        .registration_token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    config.manager = Some(ManagerConfig {
        enabled: true,
        urls: manager_urls,
        accept_self_signed_cert: payload.accept_self_signed_cert.unwrap_or(false),
        cert_fingerprint: config.manager.as_ref().and_then(|m| m.cert_fingerprint.clone()),
        registration_token,
        node_id: config.manager.as_ref().and_then(|m| m.node_id.clone()),
        node_secret: config.manager.as_ref().and_then(|m| m.node_secret.clone()),
    });

    // Run full validation
    if let Err(e) = validate_config(&config) {
        // Revert — drop the write guard and return error
        // Since we already mutated, we need to reload. For simplicity,
        // we return the error and the in-memory state is stale until restart.
        return (
            StatusCode::BAD_REQUEST,
            Json(SetupResponse {
                success: false,
                message: None,
                error: Some(format!("Validation failed: {e}")),
            }),
        );
    }

    // Save to disk
    if let Err(e) = save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SetupResponse {
                success: false,
                message: None,
                error: Some(format!("Failed to save config: {e}")),
            }),
        );
    }

    tracing::info!("Setup wizard: configuration saved successfully");

    (
        StatusCode::OK,
        Json(SetupResponse {
            success: true,
            message: Some(
                "Configuration saved. Restart the bilbycast-edge service to apply the new settings."
                    .to_string(),
            ),
            error: None,
        }),
    )
}
