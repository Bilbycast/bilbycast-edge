// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! REST API handlers for IP tunnel management.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::config::persistence::save_config_split_async;
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
///
/// On success the tunnel is also written into the in-memory `AppConfig` and
/// persisted to `config.json` (and any secrets to `secrets.json`) so it
/// survives an edge restart — mirroring the manager WS `create_tunnel` path
/// and the inputs/outputs REST handlers. Without this the local REST create
/// is non-durable: the tunnel would vanish on the next restart.
pub async fn create_tunnel(
    State(state): State<AppState>,
    Json(config): Json<TunnelConfig>,
) -> impl IntoResponse {
    if let Err(e) = crate::config::validation::validate_tunnel(&config) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string() }))).into_response();
    }
    let persisted = config.clone();
    match state.tunnel_manager.create_tunnel(config).await {
        Ok(()) => {
            let mut cfg = state.config.write().await;
            // Upsert by id (replace an existing entry, else append).
            cfg.tunnels.retain(|t| t.id != persisted.id);
            cfg.tunnels.push(persisted);
            if let Err(e) = save_config_split_async(
                state.config_path.clone(),
                state.secrets_path.clone(),
                cfg.clone(),
            )
            .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("tunnel created but config persist failed: {e}") })),
                )
                    .into_response();
            }
            (StatusCode::CREATED, Json(serde_json::json!({ "status": "created" }))).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// DELETE /api/v1/tunnels/:id — destroy a tunnel.
///
/// On success the tunnel is also removed from the in-memory `AppConfig` and the
/// change persisted to `config.json`, so the removal is not resurrected on the
/// next restart (mirrors the manager WS `delete_tunnel` path).
pub async fn delete_tunnel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Idempotent: a tunnel that already self-evicted from the runtime registry
    // (e.g. a connect failure) returns Ok(false) from destroy_tunnel rather than
    // erroring, so we still reconcile it out of config.json and persist. Without
    // this an orphaned config entry could never be deleted via REST and would
    // resurrect on the next restart.
    let was_live = match state.tunnel_manager.destroy_tunnel(&id).await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let mut cfg = state.config.write().await;
    let before = cfg.tunnels.len();
    cfg.tunnels.retain(|t| t.id != id);
    let removed_from_config = cfg.tunnels.len() != before;
    if was_live || removed_from_config {
        if let Err(e) = save_config_split_async(
            state.config_path.clone(),
            state.secrets_path.clone(),
            cfg.clone(),
        )
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("tunnel destroyed but config persist failed: {e}") })),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({
        "status": "deleted",
        "was_live": was_live,
        "removed_from_config": removed_from_config
    }))
    .into_response()
}
