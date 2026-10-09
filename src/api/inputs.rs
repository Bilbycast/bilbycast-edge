// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use axum::Json;
use axum::extract::{Path, State};

use crate::config::models::InputDefinition;
use crate::config::persistence::save_config_split_async;
use crate::config::validation::{validate_input_definition, validate_port_conflicts_with_input};
use crate::manager::client::restart_flow_for_input_update;

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

    // Cross-entity port check runs *after* the identity checks so a duplicate
    // id still reports 409, not 400: the probe substitutes by id, so it would
    // otherwise silently drop the colliding entity and mask the real error.
    validate_port_conflicts_with_input(&config, &input)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    config.inputs.push(input.clone());
    save_config_split_async(state.config_path.clone(), state.secrets_path.clone(), config.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!("Created input '{}' ({})", input.id, input.config.type_name());
    Ok(Json(ApiResponse::ok(input)))
}

/// `PUT /api/v1/inputs/{input_id}` — Update an existing input definition.
///
/// If the input is assigned to a running flow, that flow is restarted, and
/// the edit is saved only once the flow is running on it. An edit the flow
/// will not start on is refused with 409 and the flow keeps the previous
/// definition; 500 means it could not be restarted on that either and is
/// down.
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

    // After the existence check, so editing a non-existent input still reports
    // 404 rather than a port conflict against a phantom entity.
    validate_port_conflicts_with_input(&config, &input)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let old = config.inputs[idx].clone();
    // Activation is owned by POST /api/v1/flows/{id}/activate-input (and the
    // WS `activate_input` command). Edits to an input's config must never
    // change its active state — otherwise any save flips the edited input to
    // active because serde defaults missing `active` fields to true.
    input.active = old.active;
    let config_or_metadata_changed =
        old.config != input.config || old.name != input.name || old.group != input.group;

    config.inputs[idx] = input.clone();

    if let Some(flow) = config.flow_using_input(&input_id).cloned()
        && flow.enabled
        && state.flow_manager.is_running(&flow.id)
        && config_or_metadata_changed
    {
        #[cfg(feature = "webrtc")]
        let webrtc_sessions = &state.webrtc_sessions;
        #[cfg(not(feature = "webrtc"))]
        let webrtc_sessions = &();
        // The rebuild the `update_input` manager command runs, shared so the
        // two cannot drift: it fails — and puts `config` back on `old` —
        // whenever the flow does not come up on the edit. Every refusal
        // leaves the flow running on `old` (409) except the one where it
        // could not be restarted on `old` either and is down (500).
        restart_flow_for_input_update(
            &state.flow_manager,
            &mut config,
            &flow,
            idx,
            old,
            webrtc_sessions,
        )
        .await
        .map_err(|e| match e.code.as_deref() {
            Some("input_update_rollback_failed") => ApiError::Internal(e.message),
            _ => ApiError::Conflict(e.message),
        })?;
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

#[cfg(test)]
mod update_input_tests {
    //! `PUT /api/v1/inputs/{id}` on an input whose flow is running. It used
    //! to stop the flow, log a restart that failed, save the edit and answer
    //! 200 — and, never re-registering the rebuilt flow's WHIP/WHEP channels
    //! (`destroy_flow` unregisters them), leave a WHIP input unreachable
    //! after even a successful edit. It now runs the `update_input` command's
    //! rebuild (`restart_flow_for_input_update`, whose outcomes are pinned by
    //! `update_input_restart_tests` in `manager::client`); these pin that the
    //! REST mirror does.
    use super::*;
    use std::path::Path as FsPath;
    use std::sync::Arc;
    use std::time::Instant;

    use tokio::sync::{RwLock, broadcast, mpsc};

    use crate::api::nmos_is05::Is05State;
    use crate::api::nmos_is08::Is08State;
    use crate::config::models::{AppConfig, FlowConfig};
    use crate::engine::manager::FlowManager;
    use crate::engine::resource_monitor::SystemResourceState;
    use crate::manager::events::Event;
    use crate::tunnel::manager::TunnelManager;

    fn input(value: serde_json::Value) -> InputDefinition {
        serde_json::from_value(value).expect("input fixture")
    }

    fn udp_input(id: &str, port: u16) -> InputDefinition {
        input(serde_json::json!({
            "id": id, "name": id, "type": "udp",
            "bind_addr": format!("127.0.0.1:{port}")
        }))
    }

    fn bind_of(def: &InputDefinition) -> String {
        serde_json::to_value(def).expect("serialise input")["bind_addr"]
            .as_str()
            .expect("udp input has a bind_addr")
            .to_string()
    }

    /// Flow `f1` running on `inputs`, behind the state the API serves it
    /// from. The event receiver is returned so emits are not send errors.
    async fn running_flow(
        dir: &FsPath,
        inputs: Vec<InputDefinition>,
    ) -> (AppState, mpsc::Receiver<Event>) {
        let (event_sender, events) = crate::manager::events::event_channel();
        let resource_state = Arc::new(SystemResourceState::new());
        #[cfg(feature = "webrtc")]
        let webrtc_sessions =
            Some(Arc::new(crate::api::webrtc::registry::WebrtcSessionRegistry::new()));
        let flow_manager = Arc::new(FlowManager::new(
            Arc::new(crate::stats::collector::StatsCollector::new()),
            false,
            event_sender.clone(),
            resource_state.clone(),
            None,
            None,
            #[cfg(all(feature = "display", target_os = "linux"))]
            crate::display::claim_registry::DisplayClaimRegistry::new(),
            #[cfg(feature = "webrtc")]
            webrtc_sessions.clone(),
        ));
        let flow: FlowConfig = serde_json::from_value(serde_json::json!({
            "id": "f1", "name": "f1",
            "input_ids": inputs.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            "output_ids": [],
        }))
        .expect("flow fixture");
        let config = AppConfig {
            inputs,
            flows: vec![flow],
            ..Default::default()
        };
        let _runtime = flow_manager
            .create_flow(config.resolve_flow(&config.flows[0]).expect("resolve"))
            .await
            .expect("create_flow");
        // As `POST /api/v1/flows/{id}/start` does after a start.
        #[cfg(feature = "webrtc")]
        if let (Some(registry), Some((tx, token))) = (&webrtc_sessions, &_runtime.whip_session_tx) {
            registry.register_whip_input("f1", tx.clone(), token.clone());
        }
        let (ws_stats_tx, _) = broadcast::channel(4);
        let state = AppState {
            config: Arc::new(RwLock::new(config)),
            config_path: dir.join("config.json"),
            secrets_path: dir.join("secrets.json"),
            flow_manager,
            tunnel_manager: Arc::new(TunnelManager::new(event_sender.clone())),
            start_time: Instant::now(),
            ws_stats_tx,
            auth_state: None,
            is05_state: Arc::new(Is05State::new()),
            is08_state: Is08State::load_or_default(dir.join("nmos_channel_map.json")),
            #[cfg(feature = "webrtc")]
            webrtc_sessions,
            event_sender: Some(event_sender),
            resource_state,
            standby_listeners: None,
            token_rate_limiter: None,
            manager_link: crate::manager::link_state::ManagerLinkState::new(false),
            ptp_node_state: crate::engine::st2110::ptp::PtpStateHandle::new(0),
        };
        (state, events)
    }

    async fn put(state: &AppState, edit: InputDefinition) -> Result<InputDefinition, ApiError> {
        update_input(
            State(state.clone()),
            RequireAdmin,
            Path(edit.id.clone()),
            Json(edit),
        )
        .await
        .map(|Json(response)| response.data.expect("an update answers with the input"))
    }

    /// The edit asks for a port something else holds, so the flow cannot run
    /// on it: 409, the flow back on the previous definition, nothing saved.
    #[tokio::test]
    async fn put_refuses_an_edit_its_flow_cannot_restart_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _events) = running_flow(dir.path(), vec![udp_input("a", 41872)]).await;
        let _held = std::net::UdpSocket::bind("127.0.0.1:41873").expect("hold the port");

        let refused = put(&state, udp_input("a", 41873)).await;

        assert!(
            matches!(refused, Err(ApiError::Conflict(_))),
            "an edit the flow cannot run on must be refused with 409"
        );
        assert_eq!(bind_of(&state.config.read().await.inputs[0]), "127.0.0.1:41872");
        let running = state.flow_manager.get_runtime("f1").expect("the flow is running");
        assert_eq!(bind_of(&running.config.inputs[0]), "127.0.0.1:41872");
        assert!(
            !std::fs::read_to_string(dir.path().join("config.json"))
                .is_ok_and(|saved| saved.contains("41873")),
            "config.json must not record an edit that was refused"
        );
        let _ = state.flow_manager.destroy_flow("f1").await;
    }

    /// An edit the flow runs on is saved, and the flow's WHIP input is still
    /// reachable afterwards: the rebuild registers it again.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn put_keeps_the_flows_whip_input_reachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let whip = input(serde_json::json!({
            "id": "w", "name": "w", "type": "webrtc", "bearer_token": "whip-token"
        }));
        let (state, _events) =
            running_flow(dir.path(), vec![udp_input("a", 41874), whip]).await;
        let registry = state.webrtc_sessions.clone().expect("registry");
        assert_eq!(registry.whip_bearer_token("f1").as_deref(), Some("whip-token"));

        let saved = put(&state, udp_input("a", 41875))
            .await
            .unwrap_or_else(|_| panic!("an edit the flow runs on must be applied"));

        assert_eq!(bind_of(&saved), "127.0.0.1:41875");
        let running = state.flow_manager.get_runtime("f1").expect("the flow is running");
        assert_eq!(bind_of(&running.config.inputs[0]), "127.0.0.1:41875");
        assert_eq!(
            registry.whip_bearer_token("f1").as_deref(),
            Some("whip-token"),
            "the rebuilt flow's WHIP input must be registered again"
        );
        let _ = state.flow_manager.destroy_flow("f1").await;
    }
}
