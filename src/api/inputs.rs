// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use axum::Json;
use axum::extract::{Path, State};

use crate::config::models::InputDefinition;
use crate::config::persistence::save_config_split_async;
use crate::config::validation::validate_input_definition;

use super::auth::RequireAdmin;
use super::errors::ApiError;
use super::models::ApiResponse;
use super::server::AppState;

/// `GET /api/v1/inputs` — List all top-level input definitions.
pub async fn list_inputs(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<Vec<InputListEntry>>>, ApiError> {
    let config = state.config.read().await;
    let entries: Vec<InputListEntry> = config
        .inputs
        .iter()
        .map(|def| {
            let assigned_flow = config
                .flow_using_input(&def.id)
                .map(|f| f.id.clone());
            InputListEntry {
                id: def.id.clone(),
                name: def.name.clone(),
                input_type: def.config.type_name().to_string(),
                assigned_flow,
            }
        })
        .collect();
    Ok(Json(ApiResponse::ok(entries)))
}

/// `GET /api/v1/inputs/{input_id}` — Get a single input definition.
pub async fn get_input(
    State(state): State<AppState>,
    Path(input_id): Path<String>,
) -> Result<Json<ApiResponse<InputDefinition>>, ApiError> {
    let config = state.config.read().await;
    let def = config
        .inputs
        .iter()
        .find(|i| i.id == input_id)
        .ok_or_else(|| ApiError::NotFound(format!("Input '{input_id}' not found")))?;
    Ok(Json(ApiResponse::ok(def.clone())))
}

/// `POST /api/v1/inputs` — Create a new input definition.
pub async fn create_input(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Json(input): Json<InputDefinition>,
) -> Result<Json<ApiResponse<InputDefinition>>, ApiError> {
    validate_input_definition(&input)
        .map_err(|e| ApiError::BadRequest(format!("Validation failed: {e}")))?;

    let mut config = state.config.write().await;

    // Check for duplicate ID
    if config.inputs.iter().any(|i| i.id == input.id) {
        return Err(ApiError::Conflict(format!(
            "Input '{}' already exists",
            input.id
        )));
    }
    // Also check against output IDs to avoid cross-entity collisions
    if config.outputs.iter().any(|o| o.id() == input.id) {
        return Err(ApiError::Conflict(format!(
            "ID '{}' already used by an output",
            input.id
        )));
    }

    config.inputs.push(input.clone());
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Created input '{}' ({})", input.id, input.config.type_name());
    Ok(Json(ApiResponse::ok(input)))
}

/// `PUT /api/v1/inputs/{input_id}` — Update an existing input definition.
///
/// If the input is assigned to a running flow, that flow is restarted.
pub async fn update_input(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Path(input_id): Path<String>,
    Json(mut input): Json<InputDefinition>,
) -> Result<Json<ApiResponse<InputDefinition>>, ApiError> {
    // Ensure path ID matches body ID
    input.id = input_id.clone();

    validate_input_definition(&input)
        .map_err(|e| ApiError::BadRequest(format!("Validation failed: {e}")))?;

    let mut config = state.config.write().await;

    let idx = config
        .inputs
        .iter()
        .position(|i| i.id == input_id)
        .ok_or_else(|| ApiError::NotFound(format!("Input '{input_id}' not found")))?;

    // Diff the relevant fields so we can choose between a lightweight
    // active-flip (no restart) and a full flow restart.
    let old = config.inputs[idx].clone();
    let active_changed = old.active != input.active;
    let config_or_metadata_changed =
        old.config != input.config || old.name != input.name || old.group != input.group;
    let becoming_active = input.active && !old.active;

    config.inputs[idx] = input.clone();

    // Cascade: if this input is becoming active, passivate its siblings in
    // the same flow.
    if becoming_active {
        let flow_opt = config.flow_using_input(&input_id).map(|f| f.id.clone());
        if let Some(fid) = flow_opt {
            let member_ids: Vec<String> = config
                .flows
                .iter()
                .find(|f| f.id == fid)
                .map(|f| f.input_ids.clone())
                .unwrap_or_default();
            for def in config.inputs.iter_mut() {
                if def.id != input_id && member_ids.contains(&def.id) && def.active {
                    def.active = false;
                }
            }
        }
    }

    let flow_id = config.flow_using_input(&input_id).map(|f| f.id.clone());

    if let Some(fid) = &flow_id {
        if state.flow_manager.is_running(fid) {
            if config_or_metadata_changed {
                // Full restart: input address/protocol/metadata changed,
                // which requires respawning the protocol-specific receiver.
                if let Err(e) = state.flow_manager.destroy_flow(fid).await {
                    tracing::warn!("Failed to stop flow '{fid}' for input update: {e}");
                }
                if let Some(flow) = config.flows.iter().find(|f| f.id == *fid) {
                    if flow.enabled {
                        match config.resolve_flow(flow) {
                            Ok(resolved) => match state.flow_manager.create_flow(resolved).await {
                                Ok(_) => tracing::info!("Restarted flow '{fid}' after input update"),
                                Err(e) => tracing::error!("Failed to restart flow '{fid}': {e}"),
                            },
                            Err(e) => tracing::error!("Failed to resolve flow '{fid}': {e}"),
                        }
                    }
                }
            } else if active_changed && becoming_active {
                // Cheap path: just swap the active input watch channel.
                if let Err(e) = state
                    .flow_manager
                    .switch_active_input(fid, &input_id)
                    .await
                {
                    tracing::warn!("switch_active_input failed for '{fid}': {e}");
                }
            }
        }
    }

    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Updated input '{}'", input_id);
    Ok(Json(ApiResponse::ok(input)))
}

/// `DELETE /api/v1/inputs/{input_id}` — Delete an input definition.
///
/// Fails with 409 Conflict if the input is assigned to a flow.
pub async fn delete_input(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Path(input_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let mut config = state.config.write().await;

    // Check assignment
    if let Some(flow) = config.flow_using_input(&input_id) {
        return Err(ApiError::Conflict(format!(
            "Input '{input_id}' is assigned to flow '{}' — unassign it first",
            flow.id
        )));
    }

    let len_before = config.inputs.len();
    config.inputs.retain(|i| i.id != input_id);
    if config.inputs.len() == len_before {
        return Err(ApiError::NotFound(format!("Input '{input_id}' not found")));
    }

    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Deleted input '{input_id}'");
    Ok(Json(ApiResponse::ok(())))
}

/// Summary entry for input listing.
#[derive(serde::Serialize)]
pub struct InputListEntry {
    pub id: String,
    pub name: String,
    pub input_type: String,
    /// Flow ID this input is assigned to, or `null` if unassigned.
    pub assigned_flow: Option<String>,
}
