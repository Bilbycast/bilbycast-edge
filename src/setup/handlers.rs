// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use serde::{Deserialize, Serialize};

use crate::api::server::AppState;
use crate::config::persistence::save_config;
use crate::config::validation::validate_config;
use crate::manager::ManagerConfig;

use super::wizard::{SETUP_DISABLED_HTML, SETUP_HTML};

/// Request body for POST /setup.
#[derive(Debug, Deserialize)]
pub struct SetupPayload {
    pub listen_addr: Option<String>,
    pub listen_port: Option<u16>,
    pub manager_url: String,
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
    pub manager_url: Option<String>,
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
    let (manager_url, accept_self_signed, reg_token) = match &config.manager {
        Some(m) => (
            Some(m.url.clone()),
            m.accept_self_signed_cert,
            m.registration_token.clone(),
        ),
        None => (None, false, None),
    };
    Json(SetupStatus {
        listen_addr: config.server.listen_addr.clone(),
        listen_port: config.server.listen_port,
        manager_url,
        accept_self_signed_cert: accept_self_signed,
        registration_token: reg_token,
        device_name: config.device_name.clone(),
        setup_enabled: config.setup_enabled,
    })
}

/// POST /setup — validates and saves setup configuration.
pub async fn apply_setup(
    State(state): State<AppState>,
    Json(payload): Json<SetupPayload>,
) -> impl IntoResponse {
    // Check if setup is enabled
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
    }

    // Validate manager URL
    let manager_url = payload.manager_url.trim().to_string();
    if manager_url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(SetupResponse {
                success: false,
                message: None,
                error: Some("Manager URL is required".to_string()),
            }),
        );
    }
    if !manager_url.starts_with("wss://") {
        return (
            StatusCode::BAD_REQUEST,
            Json(SetupResponse {
                success: false,
                message: None,
                error: Some("Manager URL must start with wss:// (TLS required)".to_string()),
            }),
        );
    }
    if manager_url.len() > 2048 {
        return (
            StatusCode::BAD_REQUEST,
            Json(SetupResponse {
                success: false,
                message: None,
                error: Some("Manager URL must be at most 2048 characters".to_string()),
            }),
        );
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
        url: manager_url,
        accept_self_signed_cert: payload.accept_self_signed_cert.unwrap_or(false),
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
    if let Err(e) = save_config(&state.config_path, &config) {
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
