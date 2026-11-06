// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use axum::Json;
use axum::extract::{Path, State};

use crate::config::models::OutputConfig;
use crate::config::persistence::save_config_split_async;
use crate::config::validation::{validate_output, validate_port_conflicts_with_output};
use crate::manager::client::running_flow_listing_output;

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

    // Cross-entity port check runs *after* the identity checks so a duplicate
    // id still reports 409, not 400 — the probe substitutes by id and would
    // otherwise drop the colliding entity and mask the real error.
    validate_port_conflicts_with_output(&config, &output)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    config.outputs.push(output.clone());
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Created output '{}' ({})", output.id(), output.type_name());
    Ok(Json(ApiResponse::ok(output)))
}

/// `PUT /api/v1/outputs/{output_id}` — Update an existing output definition.
///
/// If the output is listed by a running flow and the definition changed, it is
/// hot-swapped. The stored `active` flag stands whatever the body says, and the
/// answer carries it: only `POST /api/v1/outputs/{output_id}/active` changes it.
/// So a passive output's edit only persists, and activation starts the edited
/// definition.
pub async fn update_output(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Path(output_id): Path<String>,
    Json(mut output): Json<OutputConfig>,
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

    // After the existence check, so editing a non-existent output still reports
    // 404 rather than a port conflict against a phantom entity.
    validate_port_conflicts_with_output(&config, &output)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let old_output = config.outputs[idx].clone();
    output.set_active(old_output.active());
    config.outputs[idx] = output.clone();

    // If a running flow lists this output, hot-swap it
    if old_output != output
        && let Some(fid) = running_flow_listing_output(&state.flow_manager, &config, &output_id)
        && let Err(e) = state.flow_manager.replace_output(&fid, output.clone()).await
    {
        tracing::error!("Failed to hot-swap output '{output_id}' on flow '{fid}': {e}");
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

#[cfg(test)]
mod update_output_tests {
    //! `PUT /api/v1/outputs/{id}` holds the stored `active` flag and swaps only
    //! a task that exists.
    use super::*;
    use crate::api::flows::passive_output_tests::{running_flow, udp_output};

    async fn put(state: &AppState, body: OutputConfig) -> OutputConfig {
        update_output(
            State(state.clone()),
            RequireAdmin,
            Path(body.id().to_string()),
            Json(body),
        )
        .await
        .map(|Json(response)| response.data.expect("an update answers with the output"))
        .unwrap_or_else(|_| panic!("the update is accepted"))
    }

    /// A body with no `active` key parses as true.
    #[tokio::test]
    async fn put_keeps_a_passive_output_passive_with_no_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _events) =
            running_flow(dir.path(), 27876, vec![udp_output("o", 27877, false)], vec![]).await;
        let body: OutputConfig = serde_json::from_value(serde_json::json!({
            "id": "o", "name": "o", "type": "udp", "dest_addr": "127.0.0.1:27878"
        }))
        .expect("output fixture");

        let answered = put(&state, body).await;

        assert!(!answered.active(), "the answer carries the held flag");
        assert!(!state.flow_manager.output_is_running("f1", "o").await);
        let config = state.config.read().await;
        let stored = serde_json::to_value(&config.outputs[0]).expect("serialise");
        assert_eq!(stored["active"], false);
        assert_eq!(stored["dest_addr"], "127.0.0.1:27878", "the edit is stored");
        drop(config);
        let _ = state.flow_manager.destroy_flow("f1").await;
    }

    /// Nor can a body switch a running output off: it is swapped and stays on.
    #[tokio::test]
    async fn put_swaps_a_running_output_and_keeps_it_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _events) =
            running_flow(dir.path(), 27879, vec![udp_output("o", 27880, true)], vec![]).await;
        let old_task = {
            let runtime = state.flow_manager.get_runtime("f1").expect("f1 runs");
            let handles = runtime.output_handles.read().await;
            handles.get("o").expect("running").cancel_token.clone()
        };

        let answered = put(&state, udp_output("o", 27881, false)).await;

        assert!(answered.active());
        assert!(old_task.is_cancelled(), "the old task was stopped");
        assert!(state.flow_manager.output_is_running("f1", "o").await);
        let config = state.config.read().await;
        let stored = serde_json::to_value(&config.outputs[0]).expect("serialise");
        assert_eq!(stored["dest_addr"], "127.0.0.1:27881", "the edit is stored");
        drop(config);
        let _ = state.flow_manager.destroy_flow("f1").await;
    }
}
