// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! NMOS IS-05 Connection Management API (v1.1).
//!
//! Provides staged/active transport parameter management for senders and receivers.
//! External NMOS controllers can modify transport parameters and activate connections.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::nmos;
use super::server::AppState;
use crate::config::models::{InputConfig, OutputConfig};
use crate::manager::events::{EventSeverity, category};

/// In-memory staged transport parameters for IS-05.
pub struct Is05State {
    pub staged_senders: DashMap<Uuid, TransportParams>,
    pub staged_receivers: DashMap<Uuid, TransportParams>,
}

impl Is05State {
    pub fn new() -> Self {
        Self {
            staged_senders: DashMap::new(),
            staged_receivers: DashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportParams {
    #[serde(default)]
    pub transport_params: Vec<TransportParamSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<Activation>,
    #[serde(default = "default_true")]
    pub master_enable: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransportParamSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtp_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_time: Option<String>,
}

#[derive(Serialize)]
struct ActiveResponse {
    transport_params: Vec<TransportParamSet>,
    activation: Option<Activation>,
    master_enable: bool,
    sender_id: Option<String>,
    transport_file: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct Constraint {
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// Build the NMOS IS-05 Connection Management router.
pub fn nmos_connection_router() -> Router<AppState> {
    Router::new()
        .route("/", get(connection_root))
        .route("/single", get(single_root))
        .route("/single/", get(single_root))
        .route("/single/senders", get(list_senders))
        .route("/single/senders/", get(list_senders))
        .route("/single/senders/{id}", get(sender_root))
        .route("/single/senders/{id}/", get(sender_root))
        .route("/single/senders/{id}/staged", get(get_sender_staged).patch(patch_sender_staged))
        .route("/single/senders/{id}/active", get(get_sender_active))
        .route("/single/senders/{id}/transporttype", get(get_sender_transport_type))
        .route("/single/senders/{id}/constraints", get(get_sender_constraints))
        .route("/single/receivers", get(list_receivers))
        .route("/single/receivers/", get(list_receivers))
        .route("/single/receivers/{id}", get(receiver_root))
        .route("/single/receivers/{id}/", get(receiver_root))
        .route("/single/receivers/{id}/staged", get(get_receiver_staged).patch(patch_receiver_staged))
        .route("/single/receivers/{id}/active", get(get_receiver_active))
        .route("/single/receivers/{id}/transporttype", get(get_receiver_transport_type))
        .route("/single/receivers/{id}/constraints", get(get_receiver_constraints))
}

// ── Navigation endpoints ──

async fn connection_root() -> Json<Vec<&'static str>> {
    Json(vec!["single/"])
}

async fn single_root() -> Json<Vec<&'static str>> {
    Json(vec!["senders/", "receivers/"])
}

async fn sender_root() -> Json<Vec<&'static str>> {
    Json(vec!["constraints/", "staged/", "active/", "transporttype"])
}

async fn receiver_root() -> Json<Vec<&'static str>> {
    Json(vec!["constraints/", "staged/", "active/", "transporttype"])
}

// ── Sender endpoints ──

async fn list_senders(State(state): State<AppState>) -> Json<Vec<String>> {
    let config = state.config.read().await;
    let nid = nmos::node_uuid_from_config(&config);
    let mut ids = Vec::new();
    for f in &config.flows {
        let resolved = match config.resolve_flow(f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for output in &resolved.outputs {
            let sid = nmos::sender_uuid_for(&nid, &f.id, nmos::output_id_of(output));
            ids.push(format!("{}/", sid));
        }
    }
    Json(ids)
}

async fn get_sender_staged(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TransportParams>, StatusCode> {
    let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;
    let is05 = &state.is05_state;
    if let Some(entry) = is05.staged_senders.get(&uuid) {
        return Ok(Json(entry.value().clone()));
    }
    // Return default from active config
    let config = state.config.read().await;
    let nid = nmos::node_uuid_from_config(&config);
    let params = active_sender_params(&config, &nid, &uuid)?;
    Ok(Json(params))
}

async fn patch_sender_staged(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<TransportParams>,
) -> Result<Json<TransportParams>, StatusCode> {
    let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;

    // Verify this sender exists; capture flow_id for event scoping.
    let sender_flow_id = {
        let config = state.config.read().await;
        let nid = nmos::node_uuid_from_config(&config);
        let (fid, _) = find_sender(&config, &nid, &uuid)?;
        fid
    };

    // Check for immediate activation
    let should_activate = patch
        .activation
        .as_ref()
        .and_then(|a| a.mode.as_deref())
        .map(|m| m == "activate_immediate")
        .unwrap_or(false);

    if should_activate {
        // For immediate activation, store staged and return with activation_time
        let mut result = patch.clone();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let activation_time = format!("{}:{}", now.as_secs(), now.subsec_nanos());
        if let Some(ref mut act) = result.activation {
            act.activation_time = Some(activation_time);
        }
        state.is05_state.staged_senders.insert(uuid, result.clone());
        // TODO: Apply transport parameter changes to the running flow
        // This would involve reverse-looking up the flow_id/output_id and calling
        // flow_manager.remove_output() + flow_manager.add_output() with new params
        if let Some(ref tx) = state.event_sender {
            tx.emit_flow_with_details(
                EventSeverity::Info,
                category::NMOS,
                format!("IS-05 sender {uuid} activated"),
                &sender_flow_id,
                serde_json::json!({
                    "subsystem": "is-05",
                    "action": "sender_activated",
                    "sender_id": uuid.to_string(),
                }),
            );
        }
        return Ok(Json(result));
    }

    // Scheduled modes not supported yet
    if let Some(ref act) = patch.activation {
        if let Some(ref mode) = act.mode {
            if mode != "activate_immediate" {
                return Err(StatusCode::NOT_IMPLEMENTED);
            }
        }
    }

    // Just stage without activation
    state.is05_state.staged_senders.insert(uuid, patch.clone());
    Ok(Json(patch))
}

async fn get_sender_active(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ActiveResponse>, StatusCode> {
    let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;
    let config = state.config.read().await;
    let nid = nmos::node_uuid_from_config(&config);
    let params = active_sender_params(&config, &nid, &uuid)?;
    Ok(Json(ActiveResponse {
        transport_params: params.transport_params,
        activation: params.activation,
        master_enable: params.master_enable,
        sender_id: None,
        transport_file: None,
    }))
}

async fn get_sender_transport_type(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;
    let config = state.config.read().await;
    let nid = nmos::node_uuid_from_config(&config);
    let (_, output) = find_sender(&config, &nid, &uuid)?;
    Ok(Json(serde_json::json!(nmos::output_transport_of(&output))))
}

async fn get_sender_constraints(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
) -> Json<Vec<HashMap<String, Constraint>>> {
    // Return minimal constraints — one set per transport leg
    let mut set = HashMap::new();
    set.insert(
        "destination_port".into(),
        Constraint {
            minimum: Some(serde_json::json!(1)),
            maximum: Some(serde_json::json!(65535)),
            description: Some("Destination UDP port".into()),
        },
    );
    Json(vec![set])
}

// ── Receiver endpoints ──

async fn list_receivers(State(state): State<AppState>) -> Json<Vec<String>> {
    let config = state.config.read().await;
    let nid = nmos::node_uuid_from_config(&config);
    let ids: Vec<String> = config
        .flows
        .iter()
        .map(|f| format!("{}/", nmos::receiver_uuid_for(&nid, &f.id)))
        .collect();
    Json(ids)
}

async fn get_receiver_staged(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TransportParams>, StatusCode> {
    let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;
    let is05 = &state.is05_state;
    if let Some(entry) = is05.staged_receivers.get(&uuid) {
        return Ok(Json(entry.value().clone()));
    }
    let config = state.config.read().await;
    let nid = nmos::node_uuid_from_config(&config);
    let params = active_receiver_params(&config, &nid, &uuid)?;
    Ok(Json(params))
}

async fn patch_receiver_staged(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<TransportParams>,
) -> Result<Json<TransportParams>, StatusCode> {
    let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;

    // Verify this receiver exists; capture flow_id for event scoping.
    let receiver_flow_id = {
        let config = state.config.read().await;
        let nid = nmos::node_uuid_from_config(&config);
        let (flow_id, _) = find_receiver(&config, &nid, &uuid)?;
        flow_id
    };

    let should_activate = patch
        .activation
        .as_ref()
        .and_then(|a| a.mode.as_deref())
        .map(|m| m == "activate_immediate")
        .unwrap_or(false);

    if should_activate {
        let mut result = patch.clone();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let activation_time = format!("{}:{}", now.as_secs(), now.subsec_nanos());
        if let Some(ref mut act) = result.activation {
            act.activation_time = Some(activation_time);
        }
        state.is05_state.staged_receivers.insert(uuid, result.clone());
        // TODO: Apply transport parameter changes to the running flow
        // This would involve reverse-looking up the flow_id and calling
        // flow_manager.destroy_flow() + flow_manager.create_flow() with updated InputConfig
        if let Some(ref tx) = state.event_sender {
            tx.emit_flow_with_details(
                EventSeverity::Info,
                category::NMOS,
                format!("IS-05 receiver {uuid} activated"),
                &receiver_flow_id,
                serde_json::json!({
                    "subsystem": "is-05",
                    "action": "receiver_activated",
                    "receiver_id": uuid.to_string(),
                }),
            );
        }
        return Ok(Json(result));
    }

    if let Some(ref act) = patch.activation {
        if let Some(ref mode) = act.mode {
            if mode != "activate_immediate" {
                return Err(StatusCode::NOT_IMPLEMENTED);
            }
        }
    }

    state.is05_state.staged_receivers.insert(uuid, patch.clone());
    Ok(Json(patch))
}

async fn get_receiver_active(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ActiveResponse>, StatusCode> {
    let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;
    let config = state.config.read().await;
    let nid = nmos::node_uuid_from_config(&config);
    let params = active_receiver_params(&config, &nid, &uuid)?;
    Ok(Json(ActiveResponse {
        transport_params: params.transport_params,
        activation: params.activation,
        master_enable: params.master_enable,
        sender_id: None,
        transport_file: None,
    }))
}

async fn get_receiver_transport_type(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;
    let config = state.config.read().await;
    let nid = nmos::node_uuid_from_config(&config);
    let (_, resolved_input) = find_receiver(&config, &nid, &uuid)?;
    let input = resolved_input.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!(nmos::input_transport_of(input))))
}

async fn get_receiver_constraints(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
) -> Json<Vec<HashMap<String, Constraint>>> {
    let mut set = HashMap::new();
    set.insert(
        "source_port".into(),
        Constraint {
            minimum: Some(serde_json::json!(1)),
            maximum: Some(serde_json::json!(65535)),
            description: Some("Source/listen port".into()),
        },
    );
    Json(vec![set])
}

// ── Helpers ──

fn find_sender(
    config: &crate::config::models::AppConfig,
    nid: &Uuid,
    target: &Uuid,
) -> Result<(String, OutputConfig), StatusCode> {
    for f in &config.flows {
        let resolved = match config.resolve_flow(f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for output in &resolved.outputs {
            let sid = nmos::sender_uuid_for(nid, &f.id, nmos::output_id_of(output));
            if &sid == target {
                return Ok((f.id.clone(), output.clone()));
            }
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// Returns (flow_id, resolved_input) for the receiver matching `target`.
fn find_receiver(
    config: &crate::config::models::AppConfig,
    nid: &Uuid,
    target: &Uuid,
) -> Result<(String, Option<InputConfig>), StatusCode> {
    for f in &config.flows {
        let rid = nmos::receiver_uuid_for(nid, &f.id);
        if &rid == target {
            let resolved_input = config.resolve_flow(f)
                .ok()
                .and_then(|r| r.active_input().map(|d| d.config.clone()));
            return Ok((f.id.clone(), resolved_input));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

fn active_sender_params(
    config: &crate::config::models::AppConfig,
    nid: &Uuid,
    target: &Uuid,
) -> Result<TransportParams, StatusCode> {
    let (_, output) = find_sender(config, nid, target)?;
    let param_set = match &output {
        OutputConfig::Rtp(c) => TransportParamSet {
            destination_ip: Some(c.dest_addr.split(':').next().unwrap_or("").to_string()),
            destination_port: c.dest_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            source_ip: c.bind_addr.as_deref().map(|a| a.split(':').next().unwrap_or("").to_string()),
            interface_ip: c.interface_addr.clone(),
            ..Default::default()
        },
        OutputConfig::Udp(c) => TransportParamSet {
            destination_ip: Some(c.dest_addr.split(':').next().unwrap_or("").to_string()),
            destination_port: c.dest_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            source_ip: c.bind_addr.as_deref().map(|a| a.split(':').next().unwrap_or("").to_string()),
            interface_ip: c.interface_addr.clone(),
            ..Default::default()
        },
        OutputConfig::Srt(c) => TransportParamSet {
            destination_ip: c.remote_addr.as_deref().map(|a| a.split(':').next().unwrap_or("").to_string()),
            destination_port: c.remote_addr.as_deref().and_then(|a| a.split(':').nth(1).and_then(|p| p.parse().ok())),
            source_ip: c.local_addr.as_deref().map(|a| a.split(':').next().unwrap_or("").to_string()),
            source_port: c.local_addr.as_deref().and_then(|a| a.split(':').nth(1).and_then(|p| p.parse().ok())),
            ..Default::default()
        },
        _ => TransportParamSet::default(),
    };
    Ok(TransportParams {
        transport_params: vec![param_set],
        activation: None,
        master_enable: true,
    })
}

fn active_receiver_params(
    config: &crate::config::models::AppConfig,
    nid: &Uuid,
    target: &Uuid,
) -> Result<TransportParams, StatusCode> {
    let (_, resolved_input) = find_receiver(config, nid, target)?;
    let input = resolved_input.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let param_set = match input {
        InputConfig::Rtp(c) => TransportParamSet {
            interface_ip: c.interface_addr.clone(),
            source_port: c.bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            ..Default::default()
        },
        InputConfig::Udp(c) => TransportParamSet {
            interface_ip: c.interface_addr.clone(),
            source_port: c.bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            ..Default::default()
        },
        InputConfig::Srt(c) => TransportParamSet {
            source_ip: c.local_addr.as_deref().map(|a| a.split(':').next().unwrap_or("").to_string()),
            source_port: c.local_addr.as_deref().and_then(|a| a.split(':').nth(1).and_then(|p| p.parse().ok())),
            destination_ip: c.remote_addr.as_deref().map(|a| a.split(':').next().unwrap_or("").to_string()),
            destination_port: c.remote_addr.as_deref().and_then(|a| a.split(':').nth(1).and_then(|p| p.parse().ok())),
            ..Default::default()
        },
        InputConfig::Rist(c) => TransportParamSet {
            source_port: c.bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            ..Default::default()
        },
        InputConfig::Rtmp(c) => TransportParamSet {
            source_port: c.listen_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            ..Default::default()
        },
        InputConfig::Rtsp(_) => TransportParamSet {
            ..Default::default()
        },
        InputConfig::Webrtc(_) | InputConfig::Whep(_) => TransportParamSet {
            ..Default::default()
        },
        // SMPTE ST 2110 — Phase 1 reports primary leg bind only.
        InputConfig::St2110_30(c) | InputConfig::St2110_31(c) => TransportParamSet {
            interface_ip: c.interface_addr.clone(),
            source_port: c.bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            ..Default::default()
        },
        InputConfig::St2110_40(c) => TransportParamSet {
            interface_ip: c.interface_addr.clone(),
            source_port: c.bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            ..Default::default()
        },
        InputConfig::RtpAudio(c) => TransportParamSet {
            interface_ip: c.interface_addr.clone(),
            source_port: c.bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            ..Default::default()
        },
        InputConfig::St2110_20(c) => TransportParamSet {
            interface_ip: c.interface_addr.clone(),
            source_port: c.bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()),
            ..Default::default()
        },
        InputConfig::St2110_23(c) => {
            // Report primary sub-stream only for IS-05 transport params.
            let first = c.sub_streams.first();
            TransportParamSet {
                interface_ip: first.and_then(|s| s.interface_addr.clone()),
                source_port: first.and_then(|s| s.bind_addr.split(':').nth(1).and_then(|p| p.parse().ok())),
                ..Default::default()
            }
        }
    };
    Ok(TransportParams {
        transport_params: vec![param_set],
        activation: None,
        master_enable: true,
    })
}
