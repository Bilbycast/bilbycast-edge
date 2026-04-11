// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use axum::Json;
use axum::extract::{Path, State};

use crate::config::models::OutputConfig;
use crate::config::persistence::save_config_split_async;
use crate::config::validation::validate_output;

use super::auth::RequireAdmin;
use super::errors::ApiError;
use super::models::ApiResponse;
use super::server::AppState;

/// `GET /api/v1/outputs` — List all top-level output definitions.
pub async fn list_outputs(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<Vec<OutputListEntry>>>, ApiError> {
    let config = state.config.read().await;
    let entries: Vec<OutputListEntry> = config
        .outputs
        .iter()
        .map(|out| {
            let assigned_flow = config
                .flow_using_output(out.id())
                .map(|f| f.id.clone());
            OutputListEntry {
                id: out.id().to_string(),
                name: out.name().to_string(),
                output_type: out.type_name().to_string(),
                assigned_flow,
            }
        })
        .collect();
    Ok(Json(ApiResponse::ok(entries)))
}

/// `GET /api/v1/outputs/{output_id}` — Get a single output definition.
pub async fn get_output(
    State(state): State<AppState>,
    Path(output_id): Path<String>,
) -> Result<Json<ApiResponse<OutputConfig>>, ApiError> {
    let config = state.config.read().await;
    let out = config
        .outputs
        .iter()
        .find(|o| o.id() == output_id)
        .ok_or_else(|| ApiError::NotFound(format!("Output '{output_id}' not found")))?;
    Ok(Json(ApiResponse::ok(out.clone())))
}

/// `POST /api/v1/outputs` — Create a new output definition.
pub async fn create_output(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Json(output): Json<OutputConfig>,
) -> Result<Json<ApiResponse<OutputConfig>>, ApiError> {
    validate_output(&output)
        .map_err(|e| ApiError::BadRequest(format!("Validation failed: {e}")))?;

    let mut config = state.config.write().await;

    // Check for duplicate ID
    if config.outputs.iter().any(|o| o.id() == output.id()) {
        return Err(ApiError::Conflict(format!(
            "Output '{}' already exists",
            output.id()
        )));
    }
    // Also check against input IDs
    if config.inputs.iter().any(|i| i.id == output.id()) {
        return Err(ApiError::Conflict(format!(
            "ID '{}' already used by an input",
            output.id()
        )));
    }

    config.outputs.push(output.clone());
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Created output '{}' ({})", output.id(), output.type_name());
    Ok(Json(ApiResponse::ok(output)))
}

/// `PUT /api/v1/outputs/{output_id}` — Update an existing output definition.
///
/// If the output is assigned to a running flow, it is hot-swapped (removed and re-added).
pub async fn update_output(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Path(output_id): Path<String>,
    Json(output): Json<OutputConfig>,
) -> Result<Json<ApiResponse<OutputConfig>>, ApiError> {
    // Verify ID consistency
    if output.id() != output_id {
        return Err(ApiError::BadRequest(format!(
            "Path output_id '{output_id}' does not match body id '{}'",
            output.id()
        )));
    }

    validate_output(&output)
        .map_err(|e| ApiError::BadRequest(format!("Validation failed: {e}")))?;

    let mut config = state.config.write().await;

    let idx = config
        .outputs
        .iter()
        .position(|o| o.id() == output_id)
        .ok_or_else(|| ApiError::NotFound(format!("Output '{output_id}' not found")))?;

    let old_output = config.outputs[idx].clone();
    config.outputs[idx] = output.clone();

    // If a flow uses this output and is running, hot-swap it
    let flow_id = config.flow_using_output(&output_id).map(|f| f.id.clone());
    if let Some(fid) = &flow_id {
        if state.flow_manager.is_running(fid) && old_output != output {
            // Hot-remove old, hot-add new
            if let Err(e) = state.flow_manager.remove_output(fid, &output_id).await {
                tracing::warn!("Failed to remove output '{output_id}' from flow '{fid}': {e}");
            }
            if let Err(e) = state.flow_manager.add_output(fid, output.clone()).await {
                tracing::error!("Failed to re-add output '{output_id}' to flow '{fid}': {e}");
            }
        }
    }

    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Updated output '{output_id}'");
    Ok(Json(ApiResponse::ok(output)))
}

/// `DELETE /api/v1/outputs/{output_id}` — Delete an output definition.
///
/// Fails with 409 Conflict if the output is assigned to a flow.
pub async fn delete_output(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Path(output_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let mut config = state.config.write().await;

    // Check assignment
    if let Some(flow) = config.flow_using_output(&output_id) {
        return Err(ApiError::Conflict(format!(
            "Output '{output_id}' is assigned to flow '{}' — unassign it first",
            flow.id
        )));
    }

    let len_before = config.outputs.len();
    config.outputs.retain(|o| o.id() != output_id);
    if config.outputs.len() == len_before {
        return Err(ApiError::NotFound(format!("Output '{output_id}' not found")));
    }

    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Deleted output '{output_id}'");
    Ok(Json(ApiResponse::ok(())))
}

/// Summary entry for output listing.
#[derive(serde::Serialize)]
pub struct OutputListEntry {
    pub id: String,
    pub name: String,
    pub output_type: String,
    /// Flow ID this output is assigned to, or `null` if unassigned.
    pub assigned_flow: Option<String>,
}
