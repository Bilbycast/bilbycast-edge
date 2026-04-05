// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! NMOS IS-04 Node API (v1.3).
//!
//! Exposes the edge node as an NMOS Node with Device, Source, Flow, Sender,
//! and Receiver resources derived from the running flow configuration.
//!
//! All NMOS resource UUIDs are deterministic (UUID v5 from the persistent node_id)
//! so they remain stable across restarts as long as flow/output IDs don't change.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use uuid::Uuid;

use super::server::AppState;
use crate::config::models::{InputConfig, OutputConfig};

/// Build the NMOS IS-04 Node API router.
pub fn nmos_node_router() -> Router<AppState> {
    Router::new()
        .route("/", get(node_root))
        .route("/self", get(node_self))
        .route("/devices", get(list_devices))
        .route("/devices/", get(list_devices))
        .route("/devices/{id}", get(get_device))
        .route("/sources", get(list_sources))
        .route("/sources/", get(list_sources))
        .route("/sources/{id}", get(get_source))
        .route("/flows", get(list_flows))
        .route("/flows/", get(list_flows))
        .route("/flows/{id}", get(get_flow))
        .route("/senders", get(list_senders))
        .route("/senders/", get(list_senders))
        .route("/senders/{id}", get(get_sender))
        .route("/receivers", get(list_receivers))
        .route("/receivers/", get(list_receivers))
        .route("/receivers/{id}", get(get_receiver))
}

// ── Helpers ──

pub fn node_uuid_from_config(config: &crate::config::models::AppConfig) -> Uuid {
    node_uuid(config)
}

pub fn sender_uuid_for(node_id: &Uuid, flow_id: &str, out_id: &str) -> Uuid {
    sender_uuid(node_id, flow_id, out_id)
}

pub fn receiver_uuid_for(node_id: &Uuid, flow_id: &str) -> Uuid {
    receiver_uuid(node_id, flow_id)
}

pub fn output_id_of(output: &OutputConfig) -> &str {
    output_id(output)
}

pub fn output_transport_of(output: &OutputConfig) -> &'static str {
    output_transport(output)
}

pub fn input_transport_of(input: &InputConfig) -> &'static str {
    input_transport(input)
}

fn node_uuid(config: &crate::config::models::AppConfig) -> Uuid {
    config
        .node_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::nil)
}

fn device_uuid(node_id: &Uuid) -> Uuid {
    Uuid::new_v5(node_id, b"device:default")
}

fn source_uuid(node_id: &Uuid, flow_id: &str) -> Uuid {
    Uuid::new_v5(node_id, format!("source:{flow_id}").as_bytes())
}

fn flow_uuid(node_id: &Uuid, flow_id: &str) -> Uuid {
    Uuid::new_v5(node_id, format!("flow:{flow_id}").as_bytes())
}

fn sender_uuid(node_id: &Uuid, flow_id: &str, output_id: &str) -> Uuid {
    Uuid::new_v5(
        node_id,
        format!("sender:{flow_id}:{output_id}").as_bytes(),
    )
}

fn receiver_uuid(node_id: &Uuid, flow_id: &str) -> Uuid {
    Uuid::new_v5(node_id, format!("receiver:{flow_id}").as_bytes())
}

fn nmos_version() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}:{}", now.as_secs(), now.subsec_nanos())
}

fn input_transport(input: &InputConfig) -> &'static str {
    match input {
        InputConfig::Rtp(_) => "urn:x-nmos:transport:rtp",
        InputConfig::Udp(_) => "urn:x-nmos:transport:rtp",
        InputConfig::Srt(_) => "urn:x-nmos:transport:rtp",
        InputConfig::Rtmp(_) => "urn:x-nmos:transport:rtp",
        InputConfig::Rtsp(_) => "urn:x-nmos:transport:rtp",
        InputConfig::Webrtc(_) | InputConfig::Whep(_) => "urn:x-nmos:transport:websocket",
    }
}

fn output_transport(output: &OutputConfig) -> &'static str {
    match output {
        OutputConfig::Rtp(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Udp(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Srt(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Rtmp(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Hls(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Webrtc(_) => "urn:x-nmos:transport:websocket",
    }
}

fn input_type_str(input: &InputConfig) -> &'static str {
    match input {
        InputConfig::Rtp(_) => "rtp",
        InputConfig::Udp(_) => "udp",
        InputConfig::Srt(_) => "srt",
        InputConfig::Rtmp(_) => "rtmp",
        InputConfig::Rtsp(_) => "rtsp",
        InputConfig::Webrtc(_) => "webrtc",
        InputConfig::Whep(_) => "whep",
    }
}

fn output_id(output: &OutputConfig) -> &str {
    match output {
        OutputConfig::Rtp(c) => &c.id,
        OutputConfig::Udp(c) => &c.id,
        OutputConfig::Srt(c) => &c.id,
        OutputConfig::Rtmp(c) => &c.id,
        OutputConfig::Hls(c) => &c.id,
        OutputConfig::Webrtc(c) => &c.id,
    }
}

fn output_name(output: &OutputConfig) -> &str {
    match output {
        OutputConfig::Rtp(c) => &c.name,
        OutputConfig::Udp(c) => &c.name,
        OutputConfig::Srt(c) => &c.name,
        OutputConfig::Rtmp(c) => &c.name,
        OutputConfig::Hls(c) => &c.name,
        OutputConfig::Webrtc(c) => &c.name,
    }
}

// ── NMOS Resource Types ──

#[derive(Serialize)]
struct NmosNode {
    id: String,
    version: String,
    label: String,
    description: String,
    tags: serde_json::Value,
    href: String,
    hostname: String,
    caps: serde_json::Value,
    api: serde_json::Value,
    services: Vec<serde_json::Value>,
    clocks: Vec<serde_json::Value>,
    interfaces: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct NmosDevice {
    id: String,
    version: String,
    label: String,
    description: String,
    tags: serde_json::Value,
    #[serde(rename = "type")]
    device_type: String,
    node_id: String,
    senders: Vec<String>,
    receivers: Vec<String>,
    controls: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct NmosSource {
    id: String,
    version: String,
    label: String,
    description: String,
    tags: serde_json::Value,
    format: String,
    caps: serde_json::Value,
    device_id: String,
    parents: Vec<String>,
    clock_name: serde_json::Value,
}

#[derive(Serialize)]
struct NmosFlow {
    id: String,
    version: String,
    label: String,
    description: String,
    tags: serde_json::Value,
    format: String,
    source_id: String,
    device_id: String,
    parents: Vec<String>,
}

#[derive(Serialize)]
struct NmosSender {
    id: String,
    version: String,
    label: String,
    description: String,
    tags: serde_json::Value,
    flow_id: String,
    transport: String,
    device_id: String,
    manifest_href: String,
    interface_bindings: Vec<String>,
    subscription: serde_json::Value,
}

#[derive(Serialize)]
struct NmosReceiver {
    id: String,
    version: String,
    label: String,
    description: String,
    tags: serde_json::Value,
    format: String,
    caps: serde_json::Value,
    device_id: String,
    transport: String,
    interface_bindings: Vec<String>,
    subscription: serde_json::Value,
}

// ── Route Handlers ──

async fn node_root() -> Json<Vec<&'static str>> {
    Json(vec![
        "self/",
        "devices/",
        "sources/",
        "flows/",
        "senders/",
        "receivers/",
    ])
}

async fn node_self(State(state): State<AppState>) -> Json<NmosNode> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let version = nmos_version();
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "bilbycast-edge".into());
    let listen = format!(
        "{}:{}",
        config.server.listen_addr, config.server.listen_port
    );

    Json(NmosNode {
        id: nid.to_string(),
        version,
        label: hostname.clone(),
        description: format!("bilbycast-edge v{}", env!("CARGO_PKG_VERSION")),
        tags: serde_json::json!({}),
        href: format!("http://{listen}/x-nmos/node/v1.3/"),
        hostname,
        caps: serde_json::json!({}),
        api: serde_json::json!({
            "versions": ["v1.3"],
            "endpoints": [{
                "host": config.server.listen_addr,
                "port": config.server.listen_port,
                "protocol": "http"
            }]
        }),
        services: vec![],
        clocks: vec![],
        interfaces: vec![],
    })
}

async fn list_devices(State(state): State<AppState>) -> Json<Vec<NmosDevice>> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    Json(vec![build_device(&config, &nid)])
}

async fn get_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<NmosDevice>, StatusCode> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    if id == did.to_string() {
        Ok(Json(build_device(&config, &nid)))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

fn build_device(
    config: &crate::config::models::AppConfig,
    nid: &Uuid,
) -> NmosDevice {
    let did = device_uuid(nid);
    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    for flow in &config.flows {
        receivers.push(receiver_uuid(nid, &flow.id).to_string());
        for output in &flow.outputs {
            senders.push(sender_uuid(nid, &flow.id, output_id(output)).to_string());
        }
    }
    NmosDevice {
        id: did.to_string(),
        version: nmos_version(),
        label: "bilbycast-edge".into(),
        description: "bilbycast-edge media transport gateway".into(),
        tags: serde_json::json!({}),
        device_type: "urn:x-nmos:device:generic".into(),
        node_id: nid.to_string(),
        senders,
        receivers,
        controls: vec![],
    }
}

async fn list_sources(State(state): State<AppState>) -> Json<Vec<NmosSource>> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    let sources: Vec<NmosSource> = config
        .flows
        .iter()
        .map(|f| NmosSource {
            id: source_uuid(&nid, &f.id).to_string(),
            version: nmos_version(),
            label: f.name.clone(),
            description: format!("{} input ({})", f.name, input_type_str(&f.input)),
            tags: serde_json::json!({}),
            format: "urn:x-nmos:format:mux".into(),
            caps: serde_json::json!({}),
            device_id: did.to_string(),
            parents: vec![],
            clock_name: serde_json::Value::Null,
        })
        .collect();
    Json(sources)
}

async fn get_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<NmosSource>, StatusCode> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    for f in &config.flows {
        let sid = source_uuid(&nid, &f.id);
        if id == sid.to_string() {
            return Ok(Json(NmosSource {
                id: sid.to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!("{} input ({})", f.name, input_type_str(&f.input)),
                tags: serde_json::json!({}),
                format: "urn:x-nmos:format:mux".into(),
                caps: serde_json::json!({}),
                device_id: did.to_string(),
                parents: vec![],
                clock_name: serde_json::Value::Null,
            }));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn list_flows(State(state): State<AppState>) -> Json<Vec<NmosFlow>> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    let flows: Vec<NmosFlow> = config
        .flows
        .iter()
        .map(|f| NmosFlow {
            id: flow_uuid(&nid, &f.id).to_string(),
            version: nmos_version(),
            label: f.name.clone(),
            description: format!("Flow: {}", f.name),
            tags: serde_json::json!({}),
            format: "urn:x-nmos:format:mux".into(),
            source_id: source_uuid(&nid, &f.id).to_string(),
            device_id: did.to_string(),
            parents: vec![],
        })
        .collect();
    Json(flows)
}

async fn get_flow(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<NmosFlow>, StatusCode> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    for f in &config.flows {
        let fid = flow_uuid(&nid, &f.id);
        if id == fid.to_string() {
            return Ok(Json(NmosFlow {
                id: fid.to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!("Flow: {}", f.name),
                tags: serde_json::json!({}),
                format: "urn:x-nmos:format:mux".into(),
                source_id: source_uuid(&nid, &f.id).to_string(),
                device_id: did.to_string(),
                parents: vec![],
            }));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn list_senders(State(state): State<AppState>) -> Json<Vec<NmosSender>> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    let mut senders = Vec::new();
    for f in &config.flows {
        let fid = flow_uuid(&nid, &f.id);
        for output in &f.outputs {
            senders.push(NmosSender {
                id: sender_uuid(&nid, &f.id, output_id(output)).to_string(),
                version: nmos_version(),
                label: output_name(output).to_string(),
                description: format!("Output {} of flow {}", output_name(output), f.name),
                tags: serde_json::json!({}),
                flow_id: fid.to_string(),
                transport: output_transport(output).into(),
                device_id: did.to_string(),
                manifest_href: String::new(),
                interface_bindings: vec![],
                subscription: serde_json::json!({"receiver_id": null, "active": true}),
            });
        }
    }
    Json(senders)
}

async fn get_sender(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<NmosSender>, StatusCode> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    for f in &config.flows {
        let fid = flow_uuid(&nid, &f.id);
        for output in &f.outputs {
            let sid = sender_uuid(&nid, &f.id, output_id(output));
            if id == sid.to_string() {
                return Ok(Json(NmosSender {
                    id: sid.to_string(),
                    version: nmos_version(),
                    label: output_name(output).to_string(),
                    description: format!(
                        "Output {} of flow {}",
                        output_name(output),
                        f.name
                    ),
                    tags: serde_json::json!({}),
                    flow_id: fid.to_string(),
                    transport: output_transport(output).into(),
                    device_id: did.to_string(),
                    manifest_href: String::new(),
                    interface_bindings: vec![],
                    subscription: serde_json::json!({"receiver_id": null, "active": true}),
                }));
            }
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn list_receivers(State(state): State<AppState>) -> Json<Vec<NmosReceiver>> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    let receivers: Vec<NmosReceiver> = config
        .flows
        .iter()
        .map(|f| NmosReceiver {
            id: receiver_uuid(&nid, &f.id).to_string(),
            version: nmos_version(),
            label: f.name.clone(),
            description: format!("{} receiver ({})", f.name, input_type_str(&f.input)),
            tags: serde_json::json!({}),
            format: "urn:x-nmos:format:mux".into(),
            caps: serde_json::json!({"media_types": ["video/MP2T"]}),
            device_id: did.to_string(),
            transport: input_transport(&f.input).into(),
            interface_bindings: vec![],
            subscription: serde_json::json!({"sender_id": null, "active": true}),
        })
        .collect();
    Json(receivers)
}

async fn get_receiver(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<NmosReceiver>, StatusCode> {
    let config = state.config.read().await;
    let nid = node_uuid(&config);
    let did = device_uuid(&nid);
    for f in &config.flows {
        let rid = receiver_uuid(&nid, &f.id);
        if id == rid.to_string() {
            return Ok(Json(NmosReceiver {
                id: rid.to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!(
                    "{} receiver ({})",
                    f.name,
                    input_type_str(&f.input)
                ),
                tags: serde_json::json!({}),
                format: "urn:x-nmos:format:mux".into(),
                caps: serde_json::json!({"media_types": ["video/MP2T"]}),
                device_id: did.to_string(),
                transport: input_transport(&f.input).into(),
                interface_bindings: vec![],
                subscription: serde_json::json!({"sender_id": null, "active": true}),
            }));
        }
    }
    Err(StatusCode::NOT_FOUND)
}
