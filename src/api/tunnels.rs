// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! REST API handlers for IP tunnel management.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::tunnel::TunnelConfig;

use super::server::AppState;

/// GET /api/v1/tunnels — list all active tunnels.
pub async fn list_tunnels(State(state): State<AppState>) -> impl IntoResponse {
    let tunnels = state.tunnel_manager.list_tunnels();
    Json(serde_json::json!({ "tunnels": tunnels }))
}

/// GET /api/v1/tunnels/:id — get tunnel status.
pub async fn get_tunnel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.tunnel_manager.tunnel_status(&id) {
        Some(status) => Json(serde_json::json!(status)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Tunnel not found" }))).into_response(),
    }
}

/// POST /api/v1/tunnels — create a new tunnel.
pub async fn create_tunnel(
    State(state): State<AppState>,
    Json(config): Json<TunnelConfig>,
) -> impl IntoResponse {
    if let Err(e) = crate::config::validation::validate_tunnel(&config) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string() }))).into_response();
    }
    match state.tunnel_manager.create_tunnel(config).await {
        Ok(()) => (StatusCode::CREATED, Json(serde_json::json!({ "status": "created" }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// DELETE /api/v1/tunnels/:id — destroy a tunnel.
pub async fn delete_tunnel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.tunnel_manager.destroy_tunnel(&id).await {
        Ok(()) => Json(serde_json::json!({ "status": "deleted" })).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}
