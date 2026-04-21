// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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

/// Apply a staged `TransportParamSet` to an `OutputConfig` in-place.
///
/// Operates through `serde_json::Value` rather than per-variant match: for
/// each address field produced by `active_sender_params`, we rewrite the
/// corresponding `"ip:port"` string shape (or split `remote_addr` for SRT)
/// by re-serialising the output, mutating, and deserialising back.
///
/// Returns `Ok(Some(new_output))` if the mutation changed anything, `Ok(None)`
/// if the patch was a no-op. Only the primary leg is mutated — 2022-7
/// leg 2 activation is deferred.
fn apply_sender_params_to_output(
    output: &OutputConfig,
    params: &TransportParamSet,
) -> Result<Option<OutputConfig>, String> {
    let mut v = serde_json::to_value(output).map_err(|e| format!("serialise output: {e}"))?;
    let obj = v.as_object_mut().ok_or("output is not a JSON object")?;

    let new_dest_ip = params.destination_ip.as_deref();
    let new_dest_port = params.destination_port;
    let new_src_ip = params.source_ip.as_deref();
    let new_src_port = params.source_port;

    let mut changed = false;

    // dest_addr = "ip:port" — used by RTP / UDP / ST 2110-* outputs.
    if let Some(dest) = obj.get("dest_addr").and_then(|d| d.as_str()) {
        let (host, port) = split_addr(dest);
        let host = new_dest_ip.unwrap_or(host);
        let port = new_dest_port.unwrap_or(port);
        let combined = format!("{host}:{port}");
        if combined != dest {
            obj.insert("dest_addr".into(), serde_json::Value::String(combined));
            changed = true;
        }
    }
    // remote_addr = "ip:port" — used by SRT outputs (caller / rendezvous).
    if let Some(rem) = obj.get("remote_addr").and_then(|d| d.as_str()).map(|s| s.to_owned()) {
        let (host, port) = split_addr(&rem);
        let host = new_dest_ip.unwrap_or(host);
        let port = new_dest_port.unwrap_or(port);
        let combined = format!("{host}:{port}");
        if combined != rem {
            obj.insert("remote_addr".into(), serde_json::Value::String(combined));
            changed = true;
        }
    }
    // bind_addr / local_addr = "ip:port" — source side (used by SRT listener, RTP bind).
    for key in ["bind_addr", "local_addr"] {
        if let Some(existing) = obj.get(key).and_then(|d| d.as_str()).map(|s| s.to_owned()) {
            let (host, port) = split_addr(&existing);
            let host = new_src_ip.unwrap_or(host);
            let port = new_src_port.unwrap_or(port);
            let combined = format!("{host}:{port}");
            if combined != existing {
                obj.insert(key.into(), serde_json::Value::String(combined));
                changed = true;
            }
        }
    }

    if !changed { return Ok(None); }

    let new_output: OutputConfig = serde_json::from_value(v)
        .map_err(|e| format!("deserialise patched output: {e}"))?;
    Ok(Some(new_output))
}

fn apply_receiver_params_to_input(
    input: &InputConfig,
    params: &TransportParamSet,
) -> Result<Option<InputConfig>, String> {
    let mut v = serde_json::to_value(input).map_err(|e| format!("serialise input: {e}"))?;
    let obj = v.as_object_mut().ok_or("input is not a JSON object")?;

    let new_iface = params.interface_ip.as_deref();
    let new_port = params.source_port;

    let mut changed = false;
    // For receivers, IS-05 surfaces `interface_ip` + `source_port` (the
    // multicast/unicast bind host + port). bind_addr / local_addr follow the
    // same "ip:port" shape as senders.
    for key in ["bind_addr", "local_addr"] {
        if let Some(existing) = obj.get(key).and_then(|d| d.as_str()).map(|s| s.to_owned()) {
            let (host, port) = split_addr(&existing);
            let host = new_iface.unwrap_or(host);
            let port = new_port.unwrap_or(port);
            let combined = format!("{host}:{port}");
            if combined != existing {
                obj.insert(key.into(), serde_json::Value::String(combined));
                changed = true;
            }
        }
    }
    if let Some(iface) = new_iface {
        if obj.get("interface_addr").and_then(|d| d.as_str()) != Some(iface) {
            obj.insert("interface_addr".into(), serde_json::Value::String(iface.to_owned()));
            changed = true;
        }
    }

    if !changed { return Ok(None); }
    let new_input: InputConfig = serde_json::from_value(v)
        .map_err(|e| format!("deserialise patched input: {e}"))?;
    Ok(Some(new_input))
}

fn split_addr(s: &str) -> (&str, u16) {
    if let Some((h, p)) = s.rsplit_once(':') {
        let port = p.parse::<u16>().unwrap_or(0);
        (h, port)
    } else {
        (s, 0)
    }
}

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

    // Verify this sender exists; capture flow_id + current output for apply.
    let (sender_flow_id, existing_output) = {
        let config = state.config.read().await;
        let nid = nmos::node_uuid_from_config(&config);
        let (fid, output) = find_sender(&config, &nid, &uuid)?;
        (fid, output)
    };

    // Check for immediate activation
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
        state.is05_state.staged_senders.insert(uuid, result.clone());

        // Apply transport params onto the running flow. Only the primary
        // leg is mutated — 2022-7 leg 2 activation is deferred. If the
        // patch didn't change any address fields (master_enable-only, etc.)
        // we skip the restart and just emit an info event.
        let primary = patch
            .transport_params
            .get(0)
            .cloned()
            .unwrap_or_default();
        let output_id = existing_output.id().to_string();
        let apply_result = apply_sender_params_to_output(&existing_output, &primary)
            .map_err(|e| {
                tracing::warn!("IS-05 apply: failed to patch output {output_id}: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        if let Some(new_output) = apply_result {
            // Hot-swap: remove then add the patched output. If the remove
            // succeeds but add fails the flow ends up with one fewer
            // output; we surface that to the NMOS client as a 500 so it
            // can retry.
            let _ = state.flow_manager.remove_output(&sender_flow_id, &output_id).await;
            if let Err(e) = state.flow_manager.add_output(&sender_flow_id, new_output.clone()).await {
                if let Some(ref tx) = state.event_sender {
                    tx.emit_flow_with_details(
                        EventSeverity::Critical,
                        category::NMOS,
                        format!("IS-05 activation failed to re-add output '{output_id}': {e}"),
                        &sender_flow_id,
                        serde_json::json!({
                            "subsystem": "is-05",
                            "action": "sender_activation_failed",
                            "sender_id": uuid.to_string(),
                            "output_id": output_id,
                            "error": e.to_string(),
                        }),
                    );
                }
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            // Persist the mutated output back to config so the change
            // survives a restart. Write the atomically-updated AppConfig
            // through the persistence helper which owns the encrypt +
            // atomic-rename dance for `secrets.json`.
            {
                let mut cfg = state.config.write().await;
                if let Some(existing) = cfg.outputs.iter_mut().find(|o| o.id() == output_id) {
                    *existing = new_output;
                }
                let cfg_snapshot = cfg.clone();
                let cfg_path = state.config_path.clone();
                let secrets_path = state.secrets_path.clone();
                drop(cfg);
                tokio::spawn(async move {
                    if let Err(e) = crate::config::persistence::save_config_split_async(
                        cfg_path,
                        secrets_path,
                        cfg_snapshot,
                    ).await {
                        tracing::warn!("IS-05 apply: failed to persist config: {e}");
                    }
                });
            }

            if let Some(ref tx) = state.event_sender {
                tx.emit_flow_with_details(
                    EventSeverity::Info,
                    category::NMOS,
                    format!("IS-05 sender {uuid} activated and applied to live flow"),
                    &sender_flow_id,
                    serde_json::json!({
                        "subsystem": "is-05",
                        "action": "sender_activated",
                        "sender_id": uuid.to_string(),
                        "output_id": output_id,
                    }),
                );
            }
        } else if let Some(ref tx) = state.event_sender {
            tx.emit_flow_with_details(
                EventSeverity::Info,
                category::NMOS,
                format!("IS-05 sender {uuid} activated (no address change — no-op on live flow)"),
                &sender_flow_id,
                serde_json::json!({
                    "subsystem": "is-05",
                    "action": "sender_activated_noop",
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

    // Verify this receiver exists; capture flow_id + current input for apply.
    let (receiver_flow_id, existing_input) = {
        let config = state.config.read().await;
        let nid = nmos::node_uuid_from_config(&config);
        let (flow_id, input) = find_receiver(&config, &nid, &uuid)?;
        (flow_id, input)
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

        // Apply transport params onto the running receiver by mutating the
        // matching input in the config, destroying and recreating the flow
        // with the new input config. Only the primary leg is handled.
        let primary = patch.transport_params.get(0).cloned().unwrap_or_default();

        // If the receiver has no resolved active input (multi-input flow
        // with no active set) or the patch is a no-op, fall back to the
        // previous "staged only" behaviour with an info event.
        let patched_input = match existing_input {
            Some(ref input) => apply_receiver_params_to_input(input, &primary)
                .map_err(|e| {
                    tracing::warn!("IS-05 apply: failed to patch receiver input: {e}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?,
            None => None,
        };

        if let Some(new_input) = patched_input {
            // Find which input definition this receiver's active input
            // corresponds to. Receivers in NMOS map 1:1 to flows; the
            // flow's `active_input_id` (or its only input if single) is
            // the one whose transport params we just mutated.
            let active_input_id = {
                let cfg = state.config.read().await;
                let flow = cfg.flows.iter().find(|f| f.id == receiver_flow_id);
                flow.and_then(|f| {
                    // Find the input marked active; fall back to the first
                    // input if none flagged.
                    for iid in &f.input_ids {
                        if let Some(def) = cfg.inputs.iter().find(|i| &i.id == iid) {
                            if def.active { return Some(def.id.clone()); }
                        }
                    }
                    f.input_ids.first().cloned()
                })
            };

            // Update config.inputs[...], destroy+recreate the flow with the
            // resolved new input set, persist.
            {
                let mut cfg = state.config.write().await;
                if let Some(ref id) = active_input_id {
                    if let Some(existing) = cfg.inputs.iter_mut().find(|i| &i.id == id) {
                        existing.config = new_input.clone();
                    }
                }
                // Resolve+restart the flow under the write lock's snapshot.
                let flow_cfg = cfg.flows.iter().find(|f| f.id == receiver_flow_id).cloned();
                let resolved = flow_cfg
                    .and_then(|f| cfg.resolve_flow(&f).ok());
                drop(cfg);

                let _ = state.flow_manager.destroy_flow(&receiver_flow_id).await;
                if let Some(r) = resolved {
                    if let Err(e) = state.flow_manager.create_flow(r).await {
                        if let Some(ref tx) = state.event_sender {
                            tx.emit_flow_with_details(
                                EventSeverity::Critical,
                                category::NMOS,
                                format!("IS-05 receiver activation failed to restart flow: {e}"),
                                &receiver_flow_id,
                                serde_json::json!({
                                    "subsystem": "is-05",
                                    "action": "receiver_activation_failed",
                                    "receiver_id": uuid.to_string(),
                                    "error": e.to_string(),
                                }),
                            );
                        }
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
                let cfg_snapshot = state.config.read().await.clone();
                let cfg_path = state.config_path.clone();
                let secrets_path = state.secrets_path.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::config::persistence::save_config_split_async(
                        cfg_path,
                        secrets_path,
                        cfg_snapshot,
                    ).await {
                        tracing::warn!("IS-05 apply: failed to persist config: {e}");
                    }
                });
            }

            if let Some(ref tx) = state.event_sender {
                tx.emit_flow_with_details(
                    EventSeverity::Info,
                    category::NMOS,
                    format!("IS-05 receiver {uuid} activated and applied to live flow"),
                    &receiver_flow_id,
                    serde_json::json!({
                        "subsystem": "is-05",
                        "action": "receiver_activated",
                        "receiver_id": uuid.to_string(),
                    }),
                );
            }
        } else if let Some(ref tx) = state.event_sender {
            tx.emit_flow_with_details(
                EventSeverity::Info,
                category::NMOS,
                format!("IS-05 receiver {uuid} activated (no address change — no-op on live flow)"),
                &receiver_flow_id,
                serde_json::json!({
                    "subsystem": "is-05",
                    "action": "receiver_activated_noop",
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
        // Bonded inputs aggregate N paths — IS-05's single-
        // transport-param model can't represent that. Report the
        // bond flow id as a placeholder in `source_port` so tools
        // see the input exists. A future NMOS extension could
        // surface per-path endpoints.
        InputConfig::Bonded(_) => TransportParamSet::default(),
        // Synthetic input — no transport parameters to report.
        InputConfig::TestPattern(_) => TransportParamSet::default(),
    };
    Ok(TransportParams {
        transport_params: vec![param_set],
        activation: None,
        master_enable: true,
    })
}
