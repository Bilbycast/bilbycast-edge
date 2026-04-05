// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use axum::Json;
use axum::extract::{Path, State};

use crate::config::models::{AppConfig, FlowConfig, OutputConfig};
use crate::config::persistence::save_config_split;
use crate::config::validation::{validate_config, validate_flow, validate_output};

use super::auth::RequireAdmin;
use super::errors::ApiError;
use super::models::{ApiResponse, FlowListResponse, FlowSummary};
use super::server::AppState;

/// Register a WHIP input channel with the WebRTC session registry (if applicable).
#[cfg(feature = "webrtc")]
fn register_whip_if_needed(state: &AppState, runtime: &crate::engine::flow::FlowRuntime) {
    if let Some((tx, bearer_token)) = &runtime.whip_session_tx {
        if let Some(ref registry) = state.webrtc_sessions {
            registry.register_whip_input(&runtime.config.id, tx.clone(), bearer_token.clone());
            tracing::info!("Registered WHIP input for flow '{}'", runtime.config.id);
        }
    }
}

/// Register a WHEP output channel with the WebRTC session registry (if applicable).
#[cfg(feature = "webrtc")]
fn register_whep_if_needed(state: &AppState, runtime: &crate::engine::flow::FlowRuntime) {
    if let Some((tx, bearer_token)) = &runtime.whep_session_tx {
        if let Some(ref registry) = state.webrtc_sessions {
            registry.register_whep_output(&runtime.config.id, tx.clone(), bearer_token.clone());
            tracing::info!("Registered WHEP output for flow '{}'", runtime.config.id);
        }
    }
}

/// `GET /api/v1/flows` -- List all configured flows.
///
/// Returns a [`FlowListResponse`] containing a summary of every flow in the current
/// configuration. Each summary includes the flow ID, name, enabled flag, input type,
/// and the number of outputs. This endpoint only reads the in-memory config and does
/// not query the engine for runtime state.
///
/// # Errors
///
/// This handler is infallible under normal operation since it only reads the config.
pub async fn list_flows(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<FlowListResponse>>, ApiError> {
    let config = state.config.read().await;
    let flows: Vec<FlowSummary> = config.flows.iter().map(FlowSummary::from).collect();
    Ok(Json(ApiResponse::ok(FlowListResponse { flows })))
}

/// `GET /api/v1/flows/{flow_id}` -- Retrieve a single flow by its ID.
///
/// Returns the full [`FlowConfig`] for the requested flow, including input configuration
/// and all output definitions.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) if no flow with the given `flow_id` exists in the config.
pub async fn get_flow(
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<ApiResponse<FlowConfig>>, ApiError> {
    let config = state.config.read().await;
    let flow = config
        .flows
        .iter()
        .find(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?;
    Ok(Json(ApiResponse::ok(flow.clone())))
}

/// `POST /api/v1/flows` -- Create a new flow.
///
/// Accepts a JSON [`FlowConfig`] body, validates it, persists it to the config file,
/// and (if the flow is enabled) starts it in the engine immediately. The newly created
/// flow configuration is returned in the response.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] (400) if the flow config fails validation (e.g., invalid
///   addresses, empty ID/name, invalid FEC parameters).
/// - [`ApiError::Conflict`] (409) if a flow with the same ID already exists.
/// - [`ApiError::Internal`] (500) if persisting the config to disk fails.
///
/// Note: if the flow is enabled but the engine fails to start it, the flow is still
/// persisted to config and a warning is logged. The flow can be started later via the
/// start endpoint.
pub async fn create_flow(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(flow): Json<FlowConfig>,
) -> Result<Json<ApiResponse<FlowConfig>>, ApiError> {
    validate_flow(&flow).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let mut config = state.config.write().await;

    if config.flows.iter().any(|f| f.id == flow.id) {
        return Err(ApiError::Conflict(format!(
            "Flow '{}' already exists",
            flow.id
        )));
    }

    config.flows.push(flow.clone());
    save_config_split(&state.config_path, &state.secrets_path, &config).map_err(|e| ApiError::Internal(e.to_string()))?;

    // Start the flow in the engine if enabled
    if flow.enabled {
        match state.flow_manager.create_flow(flow.clone()).await {
            Ok(_runtime) => {
                #[cfg(feature = "webrtc")]
                register_whip_if_needed(&state, &_runtime);
                register_whep_if_needed(&state, &_runtime);
            }
            Err(e) => tracing::warn!("Flow '{}' persisted but failed to start: {e}", flow.id),
        }
    }

    tracing::info!("Created flow '{}' ({})", flow.id, flow.name);
    Ok(Json(ApiResponse::ok(flow)))
}

/// `PUT /api/v1/flows/{flow_id}` -- Update (replace) an existing flow.
///
/// Replaces the flow configuration at the given `flow_id` with the provided JSON body.
/// The `id` field in the body is overwritten with `flow_id` from the path to ensure
/// consistency. If the flow is currently running, it is stopped and destroyed before
/// the update. After persisting, the flow is restarted if its `enabled` flag is true.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] (400) if the new flow config fails validation.
/// - [`ApiError::NotFound`] (404) if no flow with the given `flow_id` exists.
/// - [`ApiError::Internal`] (500) if persisting the config to disk fails.
///
/// Note: if the engine fails to restart the flow after update, the config is still
/// persisted and a warning is logged.
pub async fn update_flow(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
    Json(mut flow): Json<FlowConfig>,
) -> Result<Json<ApiResponse<FlowConfig>>, ApiError> {
    flow.id = flow_id.clone();
    validate_flow(&flow).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let mut config = state.config.write().await;

    let idx = config
        .flows
        .iter()
        .position(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?;

    // Stop the running flow if it exists, then restart with new config
    if state.flow_manager.is_running(&flow_id) {
        let _ = state.flow_manager.destroy_flow(&flow_id).await;
    }

    config.flows[idx] = flow.clone();
    save_config_split(&state.config_path, &state.secrets_path, &config).map_err(|e| ApiError::Internal(e.to_string()))?;

    if flow.enabled {
        match state.flow_manager.create_flow(flow.clone()).await {
            Ok(_runtime) => {
                #[cfg(feature = "webrtc")]
                register_whip_if_needed(&state, &_runtime);
                register_whep_if_needed(&state, &_runtime);
            }
            Err(e) => tracing::warn!("Flow '{}' updated but failed to restart: {e}", flow_id),
        }
    }

    tracing::info!("Updated flow '{}'", flow_id);
    Ok(Json(ApiResponse::ok(flow)))
}

/// `DELETE /api/v1/flows/{flow_id}` -- Delete a flow.
///
/// Stops the flow in the engine (if running), removes it from the in-memory config,
/// and persists the change to disk. Returns an empty success response on completion.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) if no flow with the given `flow_id` exists in the config.
/// - [`ApiError::Internal`] (500) if persisting the updated config to disk fails.
pub async fn delete_flow(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    // Stop the flow in engine first
    if state.flow_manager.is_running(&flow_id) {
        let _ = state.flow_manager.destroy_flow(&flow_id).await;
    }

    let mut config = state.config.write().await;

    let idx = config
        .flows
        .iter()
        .position(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?;

    config.flows.remove(idx);
    save_config_split(&state.config_path, &state.secrets_path, &config).map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Deleted flow '{}'", flow_id);
    Ok(Json(ApiResponse::ok(())))
}

/// `POST /api/v1/flows/{flow_id}/start` -- Start a stopped flow.
///
/// Looks up the flow configuration by `flow_id` and instructs the engine to create
/// and start it. The read lock on config is released before the async engine call
/// to avoid holding it across an await point.
///
/// # Errors
///
/// - [`ApiError::Conflict`] (409) if the flow is already running.
/// - [`ApiError::NotFound`] (404) if no flow with the given `flow_id` exists in the config.
/// - [`ApiError::Internal`] (500) if the engine fails to start the flow.
pub async fn start_flow(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    if state.flow_manager.is_running(&flow_id) {
        return Err(ApiError::Conflict(format!("Flow '{flow_id}' is already running")));
    }

    let config = state.config.read().await;
    let flow = config
        .flows
        .iter()
        .find(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?
        .clone();

    drop(config); // release read lock before async engine call

    let runtime = state
        .flow_manager
        .create_flow(flow)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to start flow '{flow_id}': {e}")))?;

    #[cfg(feature = "webrtc")]
    register_whip_if_needed(&state, &runtime);
    register_whep_if_needed(&state, &runtime);
    let _ = runtime; // suppress unused warning when webrtc feature is off

    tracing::info!("Started flow '{}'", flow_id);
    Ok(Json(ApiResponse::ok(())))
}

/// `POST /api/v1/flows/{flow_id}/stop` -- Stop a running flow.
///
/// Verifies the flow exists in the config, then instructs the engine to stop it.
/// The flow configuration is preserved; only the runtime instance is torn down.
/// The flow can be restarted later via the start endpoint.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) if no flow with the given `flow_id` exists in the config.
/// - [`ApiError::Conflict`] (409) if the flow is not currently running.
/// - [`ApiError::Internal`] (500) if the engine fails to stop the flow.
pub async fn stop_flow(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    // Verify the flow exists in config
    let config = state.config.read().await;
    let _flow = config
        .flows
        .iter()
        .find(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?;
    drop(config);

    if !state.flow_manager.is_running(&flow_id) {
        return Err(ApiError::Conflict(format!("Flow '{flow_id}' is not running")));
    }

    state
        .flow_manager
        .stop_flow(&flow_id)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to stop flow '{flow_id}': {e}")))?;

    tracing::info!("Stopped flow '{}'", flow_id);
    Ok(Json(ApiResponse::ok(())))
}

/// `POST /api/v1/flows/{flow_id}/restart` -- Restart a flow (stop + start).
///
/// Looks up the flow by `flow_id`, destroys the running instance (if any), and
/// creates a fresh instance from the current configuration. This is useful for
/// picking up configuration changes or recovering from transient errors without
/// needing separate stop/start calls.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) if no flow with the given `flow_id` exists in the config.
/// - [`ApiError::Internal`] (500) if the engine fails to create the new flow instance.
pub async fn restart_flow(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let config = state.config.read().await;
    let flow = config
        .flows
        .iter()
        .find(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?
        .clone();
    drop(config);

    // Stop if running
    if state.flow_manager.is_running(&flow_id) {
        let _ = state.flow_manager.destroy_flow(&flow_id).await;
    }

    // Start
    let runtime = state
        .flow_manager
        .create_flow(flow)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to restart flow '{flow_id}': {e}")))?;

    #[cfg(feature = "webrtc")]
    register_whip_if_needed(&state, &runtime);
    register_whep_if_needed(&state, &runtime);
    let _ = runtime;

    tracing::info!("Restarted flow '{}'", flow_id);
    Ok(Json(ApiResponse::ok(())))
}

/// `POST /api/v1/flows/{flow_id}/outputs` -- Add a new output to an existing flow.
///
/// Accepts a JSON [`OutputConfig`] body, validates it, appends it to the flow's output
/// list, and persists the change to disk. If the flow is currently running, the output
/// is hot-added to the engine without stopping the flow. Returns the created output
/// configuration on success.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] (400) if the output config fails validation.
/// - [`ApiError::NotFound`] (404) if no flow with the given `flow_id` exists.
/// - [`ApiError::Conflict`] (409) if an output with the same ID already exists in the flow.
/// - [`ApiError::Internal`] (500) if persisting the config to disk fails.
///
/// Note: if hot-adding the output to a running flow fails, the output is still persisted
/// to config and a warning is logged.
pub async fn add_output(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
    Json(output): Json<OutputConfig>,
) -> Result<Json<ApiResponse<OutputConfig>>, ApiError> {
    validate_output(&output).map_err(|e: anyhow::Error| ApiError::BadRequest(e.to_string()))?;

    let mut config = state.config.write().await;

    let flow = config
        .flows
        .iter_mut()
        .find(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?;

    let output_id = output.id().to_string();
    if flow.outputs.iter().any(|o| o.id() == output_id) {
        return Err(ApiError::Conflict(format!(
            "Output '{output_id}' already exists in flow '{flow_id}'"
        )));
    }

    flow.outputs.push(output.clone());
    save_config_split(&state.config_path, &state.secrets_path, &config).map_err(|e| ApiError::Internal(e.to_string()))?;
    drop(config);

    // Hot-add output to running flow
    if state.flow_manager.is_running(&flow_id) {
        if let Err(e) = state.flow_manager.add_output(&flow_id, output.clone()).await {
            tracing::warn!("Output '{output_id}' persisted but failed to hot-add: {e}");
        }
    }

    tracing::info!("Added output '{}' to flow '{}'", output_id, flow_id);
    Ok(Json(ApiResponse::ok(output)))
}

/// `DELETE /api/v1/flows/{flow_id}/outputs/{output_id}` -- Remove an output from a flow.
///
/// If the flow is currently running, the output is hot-removed from the engine first.
/// Then the output is removed from the in-memory config and persisted to disk.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) if the flow or the output within it does not exist.
/// - [`ApiError::Internal`] (500) if persisting the config to disk fails.
pub async fn remove_output(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((flow_id, output_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    // Remove from running engine first
    if state.flow_manager.is_running(&flow_id) {
        let _ = state.flow_manager.remove_output(&flow_id, &output_id).await;
    }

    let mut config = state.config.write().await;

    let flow = config
        .flows
        .iter_mut()
        .find(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?;

    let idx = flow
        .outputs
        .iter()
        .position(|o| o.id() == output_id)
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "Output '{output_id}' not found in flow '{flow_id}'"
            ))
        })?;

    flow.outputs.remove(idx);
    save_config_split(&state.config_path, &state.secrets_path, &config).map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Removed output '{}' from flow '{}'", output_id, flow_id);
    Ok(Json(ApiResponse::ok(())))
}

/// `GET /api/v1/config` -- Retrieve the full running configuration.
///
/// Returns the complete [`AppConfig`] including server settings and all flow
/// definitions. This reflects the current in-memory state, which may differ from
/// the on-disk file if external edits were made without reloading.
///
/// # Errors
///
/// This handler is infallible under normal operation since it only reads the config.
pub async fn get_config(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<AppConfig>>, ApiError> {
    let config = state.config.read().await;
    // Mask secrets for API display — shows that secrets are configured without exposing them
    let mut safe_config = config.clone();
    safe_config.mask_secrets();
    Ok(Json(ApiResponse::ok(safe_config)))
}

/// `PUT /api/v1/config` -- Replace the entire application configuration.
///
/// Accepts a full [`AppConfig`] JSON body, validates it, stops all currently running
/// flows, replaces the in-memory config, persists it to disk, and starts all flows
/// that have `enabled: true` in the new config. This is an atomic swap of the entire
/// configuration.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] (400) if the new config fails validation (e.g., duplicate
///   flow IDs, invalid addresses).
/// - [`ApiError::Internal`] (500) if persisting the config to disk fails, or if
///   individual flows fail to start (logged as errors but does not fail the request).
pub async fn replace_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(new_config): Json<AppConfig>,
) -> Result<Json<ApiResponse<AppConfig>>, ApiError> {
    validate_config(&new_config).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Stop all running flows
    state.flow_manager.stop_all().await;

    let mut config = state.config.write().await;
    *config = new_config.clone();
    save_config_split(&state.config_path, &state.secrets_path, &config).map_err(|e| ApiError::Internal(e.to_string()))?;

    // Start all enabled flows from new config
    for flow in &config.flows {
        if flow.enabled {
            match state.flow_manager.create_flow(flow.clone()).await {
                Ok(_runtime) => {
                    #[cfg(feature = "webrtc")]
                    register_whip_if_needed(&state, &_runtime);
                    #[cfg(feature = "webrtc")]
                    register_whep_if_needed(&state, &_runtime);
                }
                Err(e) => tracing::error!("Failed to start flow '{}' after config replace: {e}", flow.id),
            }
        }
    }

    tracing::info!("Replaced entire config with {} flow(s)", config.flows.len());
    // Mask secrets for API display
    let mut safe_config = new_config;
    safe_config.mask_secrets();
    Ok(Json(ApiResponse::ok(safe_config)))
}

/// `POST /api/v1/config/reload` -- Reload configuration from disk.
///
/// Reads the config file from [`AppState::config_path`], validates it, stops all
/// currently running flows, replaces the in-memory config with the disk version,
/// and starts all flows that have `enabled: true`. This is useful after manual
/// edits to the config file or after deploying a new config via external tooling.
///
/// # Errors
///
/// - [`ApiError::Internal`] (500) if the config file cannot be read or parsed.
/// - [`ApiError::BadRequest`] (400) if the loaded config fails validation.
///
/// Note: individual flow start failures are logged as errors but do not cause the
/// reload request itself to fail.
pub async fn reload_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<AppConfig>>, ApiError> {
    let new_config = crate::config::persistence::load_config_split(&state.config_path, &state.secrets_path)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    validate_config(&new_config).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Stop all running flows
    state.flow_manager.stop_all().await;

    let mut config = state.config.write().await;
    *config = new_config.clone();

    // Start all enabled flows from reloaded config
    for flow in &config.flows {
        if flow.enabled {
            match state.flow_manager.create_flow(flow.clone()).await {
                Ok(_runtime) => {
                    #[cfg(feature = "webrtc")]
                    register_whip_if_needed(&state, &_runtime);
                    #[cfg(feature = "webrtc")]
                    register_whep_if_needed(&state, &_runtime);
                }
                Err(e) => tracing::error!("Failed to start flow '{}' after config reload: {e}", flow.id),
            }
        }
    }

    tracing::info!(
        "Reloaded config from disk with {} flow(s)",
        config.flows.len()
    );
    // Mask secrets for API display
    let mut safe_config = new_config;
    safe_config.mask_secrets();
    Ok(Json(ApiResponse::ok(safe_config)))
}
