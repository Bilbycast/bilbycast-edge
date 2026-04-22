// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use axum::Json;
use axum::extract::{Path, State};

use serde::Deserialize;

use crate::config::models::{AppConfig, FlowConfig};
use crate::config::persistence::save_config_split_async;
use crate::config::validation::{validate_config, validate_flow};

use super::auth::RequireAdmin;
use super::errors::ApiError;
use super::models::{ApiResponse, FlowListResponse, FlowSummary};
use super::server::AppState;

/// Request body for `POST /api/v1/flows/{flow_id}/outputs` — assigns an existing
/// output (by ID) to a flow.
#[derive(Deserialize)]
pub struct AddOutputRequest {
    /// The ID of the output to assign (must already exist in `AppConfig.outputs`).
    pub output_id: String,
}

/// Request body for `POST /api/v1/flows/{flow_id}/activate-input` — selects
/// which input of a multi-input flow is currently active.
#[derive(Deserialize)]
pub struct ActivateInputRequest {
    /// ID of the input to activate. Must be one of the flow's `input_ids`.
    pub input_id: String,
}

/// Request body for `POST /api/v1/outputs/{output_id}/active` — toggles an
/// output between active (running task) and passive (persisted but not running).
#[derive(Deserialize)]
pub struct SetOutputActiveRequest {
    /// New active state. `true` starts the output task, `false` stops it.
    pub active: bool,
}

/// Register a WHIP input channel with the WebRTC session registry (if applicable).
#[cfg(feature = "webrtc")]
fn register_whip_if_needed(state: &AppState, runtime: &crate::engine::flow::FlowRuntime) {
    if let Some((tx, bearer_token)) = &runtime.whip_session_tx {
        if let Some(ref registry) = state.webrtc_sessions {
            registry.register_whip_input(&runtime.config.config.id, tx.clone(), bearer_token.clone());
            tracing::info!("Registered WHIP input for flow '{}'", runtime.config.config.id);
        }
    }
}

/// Register a WHEP output channel with the WebRTC session registry (if applicable).
#[cfg(feature = "webrtc")]
fn register_whep_if_needed(state: &AppState, runtime: &crate::engine::flow::FlowRuntime) {
    if let Some((tx, bearer_token)) = &runtime.whep_session_tx {
        if let Some(ref registry) = state.webrtc_sessions {
            registry.register_whep_output(&runtime.config.config.id, tx.clone(), bearer_token.clone());
            tracing::info!("Registered WHEP output for flow '{}'", runtime.config.config.id);
        }
    }
}

/// Resolve a flow's references against the config and return a `ResolvedFlow`.
/// Maps resolution errors to `ApiError::BadRequest`.
fn resolve_flow(config: &AppConfig, flow: &FlowConfig) -> Result<crate::config::models::ResolvedFlow, ApiError> {
    config.resolve_flow(flow).map_err(|e| ApiError::BadRequest(e.to_string()))
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
    let flows: Vec<FlowSummary> = config.flows.iter().map(|f| FlowSummary::from_flow(f, &config)).collect();
    Ok(Json(ApiResponse::ok(FlowListResponse { flows })))
}

/// `GET /api/v1/flows/{flow_id}` -- Retrieve a single flow by its ID.
///
/// Returns the full [`FlowConfig`] for the requested flow, including its `input_id`
/// and `output_ids` references.
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

    // Resolve references before persisting to catch dangling input_id/output_ids early
    let resolved = resolve_flow(&config, &flow)?;

    config.flows.push(flow.clone());
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone()).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    // Start the flow in the engine if enabled
    if flow.enabled {
        match state.flow_manager.create_flow(resolved).await {
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

    // Resolve references before persisting to catch dangling input_id/output_ids early
    let resolved = resolve_flow(&config, &flow)?;

    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone()).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    if flow.enabled {
        match state.flow_manager.create_flow(resolved).await {
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
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone()).await.map_err(|e| ApiError::Internal(e.to_string()))?;

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
    let resolved = resolve_flow(&config, &flow)?;

    drop(config); // release read lock before async engine call

    let runtime = state
        .flow_manager
        .create_flow(resolved)
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

/// `PUT /api/v1/flows/{flow_id}/assembly` -- Hot-swap a flow's
/// PID-bus assembly plan without restarting the flow (Phase 7).
///
/// Body: a `FlowAssembly` JSON object — same schema as the `assembly`
/// field on `FlowConfig`. The handler:
///
/// 1. Looks up the running flow.
/// 2. No-op if the new assembly is structurally identical to the
///    current one.
/// 3. Calls [`crate::engine::flow::FlowRuntime::replace_assembly`]
///    which validates, resolves any `Essence` slots, spawns Hitless
///    mergers, and dispatches `PlanCommand::ReplacePlan` to the
///    running assembler. PMT version bumps automatically per program.
/// 4. Persists the new `assembly` block to `config.json`.
///
/// Mirrors the WS `update_flow_assembly` command — same helper, same
/// validation, same error codes. Used by the testbed probe so it can
/// drive the runtime switch without standing up a manager.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) when the flow isn't running.
/// - [`ApiError::BadRequest`] (400) when the assembly body fails
///   validation, when essence resolution can't find a matching ES, or
///   when the input shape forbids the requested switch.
pub async fn update_flow_assembly(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
    Json(new_assembly): Json<crate::config::models::FlowAssembly>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let runtime = state
        .flow_manager
        .get_runtime(&flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' is not running")))?;

    if let Some(cur) = runtime.current_assembly().await {
        if cur == new_assembly {
            tracing::info!("update_flow_assembly '{flow_id}': no-op (unchanged)");
            return Ok(Json(ApiResponse::ok(())));
        }
    }

    runtime
        .replace_assembly(new_assembly.clone())
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cfg_clone = {
        let mut cfg = state.config.write().await;
        if let Some(flow) = cfg.flows.iter_mut().find(|f| f.id == flow_id) {
            flow.assembly = Some(new_assembly);
        }
        cfg.clone()
    };
    if let Err(e) = save_config_split_async(
        state.config_path.clone(),
        state.secrets_path.clone(),
        cfg_clone,
    )
    .await
    {
        tracing::warn!("update_flow_assembly '{flow_id}': persist failed: {e}");
    }

    tracing::info!("update_flow_assembly '{flow_id}': hot-swap complete");
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
    let resolved = resolve_flow(&config, &flow)?;
    drop(config);

    // Stop if running
    if state.flow_manager.is_running(&flow_id) {
        let _ = state.flow_manager.destroy_flow(&flow_id).await;
    }

    // Start
    let runtime = state
        .flow_manager
        .create_flow(resolved)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to restart flow '{flow_id}': {e}")))?;

    #[cfg(feature = "webrtc")]
    register_whip_if_needed(&state, &runtime);
    register_whep_if_needed(&state, &runtime);
    let _ = runtime;

    tracing::info!("Restarted flow '{}'", flow_id);
    Ok(Json(ApiResponse::ok(())))
}

/// `POST /api/v1/flows/{flow_id}/outputs` -- Assign an existing output to a flow.
///
/// Accepts a JSON body with an `output_id` field. The output must already exist in
/// `AppConfig.outputs`. The output_id is appended to the flow's `output_ids` list
/// and persisted to disk. If the flow is currently running, the output is hot-added
/// to the engine without stopping the flow.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) if the flow or the referenced output does not exist.
/// - [`ApiError::Conflict`] (409) if the output is already assigned to this flow or
///   is assigned to another flow.
/// - [`ApiError::Internal`] (500) if persisting the config to disk fails.
///
/// Note: if hot-adding the output to a running flow fails, the assignment is still
/// persisted to config and a warning is logged.
pub async fn add_output(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
    Json(req): Json<AddOutputRequest>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let output_id = req.output_id;

    let mut config = state.config.write().await;

    // Verify the flow exists
    let flow_idx = config
        .flows
        .iter()
        .position(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?;

    // Verify the output exists in AppConfig.outputs
    let output = config
        .outputs
        .iter()
        .find(|o| o.id() == output_id)
        .ok_or_else(|| {
            ApiError::NotFound(format!("Output '{output_id}' does not exist in the configuration"))
        })?
        .clone();

    // Check the output isn't already assigned to this flow
    if config.flows[flow_idx].output_ids.contains(&output_id) {
        return Err(ApiError::Conflict(format!(
            "Output '{output_id}' is already assigned to flow '{flow_id}'"
        )));
    }

    // Check the output isn't assigned to another flow
    for f in &config.flows {
        if f.id != flow_id && f.output_ids.contains(&output_id) {
            return Err(ApiError::Conflict(format!(
                "Output '{output_id}' is already assigned to flow '{}'",
                f.id
            )));
        }
    }

    config.flows[flow_idx].output_ids.push(output_id.clone());
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone()).await.map_err(|e| ApiError::Internal(e.to_string()))?;
    drop(config);

    // Hot-add output to running flow
    if state.flow_manager.is_running(&flow_id) {
        if let Err(e) = state.flow_manager.add_output(&flow_id, output).await {
            tracing::warn!("Output '{output_id}' assigned but failed to hot-add: {e}");
        }
    }

    tracing::info!("Assigned output '{}' to flow '{}'", output_id, flow_id);
    Ok(Json(ApiResponse::ok(())))
}

/// `DELETE /api/v1/flows/{flow_id}/outputs/{output_id}` -- Unassign an output from a flow.
///
/// Removes the `output_id` from the flow's `output_ids` list. The output definition
/// remains in `AppConfig.outputs` (it is simply unassigned from the flow). If the flow
/// is currently running, the output is hot-removed from the engine first.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) if the flow does not exist or the output_id is not
///   assigned to the flow.
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
        .output_ids
        .iter()
        .position(|oid| oid == &output_id)
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "Output '{output_id}' is not assigned to flow '{flow_id}'"
            ))
        })?;

    flow.output_ids.remove(idx);
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone()).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Unassigned output '{}' from flow '{}'", output_id, flow_id);
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
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone()).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    // Start all enabled flows from new config
    for flow in &config.flows {
        if flow.enabled {
            match config.resolve_flow(flow) {
                Ok(resolved) => {
                    match state.flow_manager.create_flow(resolved).await {
                        Ok(_runtime) => {
                            #[cfg(feature = "webrtc")]
                            register_whip_if_needed(&state, &_runtime);
                            #[cfg(feature = "webrtc")]
                            register_whep_if_needed(&state, &_runtime);
                        }
                        Err(e) => tracing::error!("Failed to start flow '{}' after config replace: {e}", flow.id),
                    }
                }
                Err(e) => tracing::error!("Failed to resolve flow '{}' after config replace: {e}", flow.id),
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
            match config.resolve_flow(flow) {
                Ok(resolved) => {
                    match state.flow_manager.create_flow(resolved).await {
                        Ok(_runtime) => {
                            #[cfg(feature = "webrtc")]
                            register_whip_if_needed(&state, &_runtime);
                            #[cfg(feature = "webrtc")]
                            register_whep_if_needed(&state, &_runtime);
                        }
                        Err(e) => tracing::error!("Failed to start flow '{}' after config reload: {e}", flow.id),
                    }
                }
                Err(e) => tracing::error!("Failed to resolve flow '{}' after config reload: {e}", flow.id),
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

/// `POST /api/v1/flows/{flow_id}/activate-input` — Select which input is active.
///
/// Atomically marks the target input `active = true` in the config, sets every
/// other input on the same flow to `active = false`, and tells the running
/// `FlowRuntime` to switch its active input. The actual switch is a single
/// watch channel update — no task restart, no reconnect gap. Persists the
/// updated config to disk.
///
/// # Errors
///
/// - `NotFound` if the flow does not exist.
/// - `BadRequest` if `input_id` is not a member of the flow.
pub async fn activate_input(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
    Json(req): Json<ActivateInputRequest>,
) -> Result<Json<ApiResponse<FlowConfig>>, ApiError> {
    let mut config = state.config.write().await;
    // Locate the target flow and verify the input is a member.
    let flow_idx = config
        .flows
        .iter()
        .position(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{}' not found", flow_id)))?;
    if !config.flows[flow_idx].input_ids.iter().any(|id| id == &req.input_id) {
        return Err(ApiError::BadRequest(format!(
            "Input '{}' is not a member of flow '{}'",
            req.input_id, flow_id
        )));
    }
    // Flip active flags in config.inputs for the members of this flow.
    let member_ids: Vec<String> = config.flows[flow_idx].input_ids.clone();
    for def in config.inputs.iter_mut() {
        if member_ids.contains(&def.id) {
            def.active = def.id == req.input_id;
        }
    }
    let flow_snapshot = config.flows[flow_idx].clone();
    // Validate and persist before touching the runtime so a validation
    // failure leaves the state unchanged.
    validate_config(&config).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    drop(config);
    // Apply the switch to the running flow. If the flow is not running we
    // silently skip (the new active flag will take effect on next start).
    if state.flow_manager.is_running(&flow_id) {
        state
            .flow_manager
            .switch_active_input(&flow_id, &req.input_id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    Ok(Json(ApiResponse::ok(flow_snapshot)))
}

/// `POST /api/v1/outputs/{output_id}/active` — Toggle an output active/passive.
///
/// Persists the new active flag on the output config and, if the output is
/// assigned to a running flow, starts or stops the output task accordingly.
/// An output can be toggled without restarting the flow's input or any
/// sibling outputs.
///
/// # Errors
///
/// - `NotFound` if the output does not exist.
/// - `Internal` if the runtime toggle fails.
pub async fn set_output_active(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(output_id): Path<String>,
    Json(req): Json<SetOutputActiveRequest>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let mut config = state.config.write().await;
    // Update the output's active flag in the top-level outputs list.
    let output = config
        .outputs
        .iter_mut()
        .find(|o| o.id() == output_id)
        .ok_or_else(|| ApiError::NotFound(format!("Output '{}' not found", output_id)))?;
    output.set_active(req.active);
    // Find the flow that owns this output (if any). Outputs are still
    // exclusive to at most one flow.
    let owning_flow_id = config
        .flows
        .iter()
        .find(|f| f.output_ids.iter().any(|id| id == &output_id))
        .map(|f| f.id.clone());
    validate_config(&config).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    drop(config);
    // Apply the toggle at runtime if the flow is running.
    if let Some(flow_id) = owning_flow_id {
        if state.flow_manager.is_running(&flow_id) {
            state
                .flow_manager
                .set_output_active(&flow_id, &output_id, req.active)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
        }
    }
    Ok(Json(ApiResponse::ok(())))
}
