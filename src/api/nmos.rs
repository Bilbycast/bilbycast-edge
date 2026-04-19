// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
        InputConfig::Rist(_) => "urn:x-nmos:transport:rtp",
        InputConfig::Rtmp(_) => "urn:x-nmos:transport:rtp",
        InputConfig::Rtsp(_) => "urn:x-nmos:transport:rtp",
        InputConfig::Webrtc(_) | InputConfig::Whep(_) => "urn:x-nmos:transport:websocket",
        InputConfig::St2110_30(_)
        | InputConfig::St2110_31(_)
        | InputConfig::St2110_40(_)
        | InputConfig::St2110_20(_)
        | InputConfig::St2110_23(_) => "urn:x-nmos:transport:rtp",
        InputConfig::RtpAudio(_) => "urn:x-nmos:transport:rtp",
        // Bonded inputs don't have a standard NMOS transport URN —
        // advertise as rtp for now so downstream NMOS clients see the
        // flow at all. A future NMOS extension could register a
        // bonding-specific URN.
        InputConfig::Bonded(_) => "urn:x-nmos:transport:rtp",
    }
}

fn output_transport(output: &OutputConfig) -> &'static str {
    match output {
        OutputConfig::Rtp(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Udp(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Srt(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Rist(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Rtmp(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Hls(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Cmaf(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Webrtc(_) => "urn:x-nmos:transport:websocket",
        OutputConfig::St2110_30(_)
        | OutputConfig::St2110_31(_)
        | OutputConfig::St2110_40(_)
        | OutputConfig::St2110_20(_)
        | OutputConfig::St2110_23(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::RtpAudio(_) => "urn:x-nmos:transport:rtp",
        OutputConfig::Bonded(_) => "urn:x-nmos:transport:rtp",
    }
}

/// NMOS format URN for an input. ST 2110-30/-31 → `urn:x-nmos:format:audio`,
/// ST 2110-40 → `urn:x-nmos:format:data`, everything else → `mux` (unchanged
/// from the historical single-format behaviour). Used by `list_sources`,
/// `list_flows`, `list_receivers`, and the IS-04 single-resource handlers
/// so each ST 2110 essence flow exposes the correct NMOS format.
fn input_format(input: &InputConfig) -> &'static str {
    match input {
        InputConfig::St2110_30(_) | InputConfig::St2110_31(_) => "urn:x-nmos:format:audio",
        InputConfig::St2110_40(_) => "urn:x-nmos:format:data",
        InputConfig::St2110_20(_) | InputConfig::St2110_23(_) => "urn:x-nmos:format:video",
        _ => "urn:x-nmos:format:mux",
    }
}

/// NMOS format URN for an output. Mirrors [`input_format`] for ST 2110.
/// Currently unused at the IS-04 sender level (NMOS senders inherit format
/// from the linked flow), but exposed for the SDP-viewer modal that picks
/// the per-output essence.
#[allow(dead_code)]
fn output_format(output: &OutputConfig) -> &'static str {
    match output {
        OutputConfig::St2110_30(_) | OutputConfig::St2110_31(_) => "urn:x-nmos:format:audio",
        OutputConfig::St2110_40(_) => "urn:x-nmos:format:data",
        OutputConfig::St2110_20(_) | OutputConfig::St2110_23(_) => "urn:x-nmos:format:video",
        _ => "urn:x-nmos:format:mux",
    }
}

/// BCP-004 receiver capabilities for ST 2110-30/-31 audio inputs.
///
/// Returns a `caps` JSON object with a `media_types` list and a
/// `constraint_sets` list keyed by `urn:x-nmos:cap:format:*` URNs. Older NMOS
/// controllers that only understand `media_types` ignore the constraint set;
/// BCP-004-aware controllers (e.g. Sony NMOS Commissioning Tool) use it to
/// reject incompatible senders before activation.
fn audio_receiver_caps(c: &crate::config::models::St2110AudioInputConfig) -> serde_json::Value {
    serde_json::json!({
        "media_types": ["audio/L16", "audio/L24"],
        "constraint_sets": [{
            "urn:x-nmos:cap:format:media_type": {
                "enum": ["audio/L16", "audio/L24"]
            },
            "urn:x-nmos:cap:format:sample_rate": {
                "enum": [{"numerator": c.sample_rate}]
            },
            "urn:x-nmos:cap:format:channel_count": {
                "enum": [c.channels as u64]
            },
            "urn:x-nmos:cap:format:sample_depth": {
                "enum": [c.bit_depth as u64]
            }
        }]
    })
}

/// BCP-004 receiver capabilities for ST 2110-40 ancillary data inputs.
fn anc_receiver_caps() -> serde_json::Value {
    serde_json::json!({
        "media_types": ["video/smpte291"],
        "constraint_sets": [{
            "urn:x-nmos:cap:format:media_type": {
                "enum": ["video/smpte291"]
            }
        }]
    })
}

/// Receiver caps for an arbitrary input. ST 2110 inputs return BCP-004
/// constraint sets; non-ST-2110 inputs continue to advertise the historical
/// `video/MP2T` shape so existing NMOS controllers don't break.
fn receiver_caps(input: &InputConfig) -> serde_json::Value {
    match input {
        InputConfig::St2110_30(c) | InputConfig::St2110_31(c) => audio_receiver_caps(c),
        InputConfig::St2110_40(_) => anc_receiver_caps(),
        InputConfig::St2110_20(c) => serde_json::json!({
            "media_types": ["video/raw"],
            "constraint_sets": [{
                "urn:x-nmos:cap:format:media_type": {"enum": ["video/raw"]},
                "urn:x-nmos:cap:format:color_sampling": {"enum": ["YCbCr-4:2:2"]},
                "urn:x-nmos:cap:format:component_depth": {"enum": [c.pixel_format.depth() as u64]},
                "urn:x-nmos:cap:format:frame_width": {"enum": [c.width as u64]},
                "urn:x-nmos:cap:format:frame_height": {"enum": [c.height as u64]},
                "urn:x-nmos:cap:format:grain_rate": {"enum": [{"numerator": c.frame_rate_num, "denominator": c.frame_rate_den}]}
            }]
        }),
        InputConfig::St2110_23(c) => serde_json::json!({
            "media_types": ["video/raw"],
            "constraint_sets": [{
                "urn:x-nmos:cap:format:media_type": {"enum": ["video/raw"]},
                "urn:x-nmos:cap:format:color_sampling": {"enum": ["YCbCr-4:2:2"]},
                "urn:x-nmos:cap:format:component_depth": {"enum": [c.pixel_format.depth() as u64]},
                "urn:x-nmos:cap:format:frame_width": {"enum": [c.width as u64]},
                "urn:x-nmos:cap:format:frame_height": {"enum": [c.height as u64]},
                "urn:x-nmos:cap:format:grain_rate": {"enum": [{"numerator": c.frame_rate_num, "denominator": c.frame_rate_den}]}
            }]
        }),
        _ => serde_json::json!({"media_types": ["video/MP2T"]}),
    }
}

/// Source-side `caps` for ST 2110 audio. Empty for non-ST-2110.
fn source_caps(input: &InputConfig) -> serde_json::Value {
    match input {
        InputConfig::St2110_30(c) | InputConfig::St2110_31(c) => serde_json::json!({
            "media_types": ["audio/L16", "audio/L24"],
            "constraint_sets": [{
                "urn:x-nmos:cap:format:sample_rate": {
                    "enum": [{"numerator": c.sample_rate}]
                },
                "urn:x-nmos:cap:format:channel_count": {
                    "enum": [c.channels as u64]
                }
            }]
        }),
        InputConfig::St2110_40(_) => serde_json::json!({"media_types": ["video/smpte291"]}),
        InputConfig::St2110_20(_) | InputConfig::St2110_23(_) => {
            serde_json::json!({"media_types": ["video/raw"]})
        }
        _ => serde_json::json!({}),
    }
}

fn input_type_str(input: &InputConfig) -> &'static str {
    match input {
        InputConfig::Rtp(_) => "rtp",
        InputConfig::Udp(_) => "udp",
        InputConfig::Srt(_) => "srt",
        InputConfig::Rist(_) => "rist",
        InputConfig::Rtmp(_) => "rtmp",
        InputConfig::Rtsp(_) => "rtsp",
        InputConfig::Webrtc(_) => "webrtc",
        InputConfig::Whep(_) => "whep",
        InputConfig::St2110_30(_) => "st2110_30",
        InputConfig::St2110_31(_) => "st2110_31",
        InputConfig::St2110_40(_) => "st2110_40",
        InputConfig::St2110_20(_) => "st2110_20",
        InputConfig::St2110_23(_) => "st2110_23",
        InputConfig::RtpAudio(_) => "rtp_audio",
        InputConfig::Bonded(_) => "bonded",
    }
}

fn output_id(output: &OutputConfig) -> &str {
    match output {
        OutputConfig::Rtp(c) => &c.id,
        OutputConfig::Udp(c) => &c.id,
        OutputConfig::Srt(c) => &c.id,
        OutputConfig::Rist(c) => &c.id,
        OutputConfig::Rtmp(c) => &c.id,
        OutputConfig::Hls(c) => &c.id,
        OutputConfig::Cmaf(c) => &c.id,
        OutputConfig::Webrtc(c) => &c.id,
        OutputConfig::St2110_30(c) => &c.id,
        OutputConfig::St2110_31(c) => &c.id,
        OutputConfig::St2110_40(c) => &c.id,
        OutputConfig::St2110_20(c) => &c.id,
        OutputConfig::St2110_23(c) => &c.id,
        OutputConfig::RtpAudio(c) => &c.id,
        OutputConfig::Bonded(c) => &c.id,
    }
}

fn output_name(output: &OutputConfig) -> &str {
    match output {
        OutputConfig::Rtp(c) => &c.name,
        OutputConfig::Udp(c) => &c.name,
        OutputConfig::Srt(c) => &c.name,
        OutputConfig::Rist(c) => &c.name,
        OutputConfig::Rtmp(c) => &c.name,
        OutputConfig::Hls(c) => &c.name,
        OutputConfig::Cmaf(c) => &c.name,
        OutputConfig::Webrtc(c) => &c.name,
        OutputConfig::St2110_30(c) => &c.name,
        OutputConfig::St2110_31(c) => &c.name,
        OutputConfig::St2110_40(c) => &c.name,
        OutputConfig::St2110_20(c) => &c.name,
        OutputConfig::St2110_23(c) => &c.name,
        OutputConfig::RtpAudio(c) => &c.name,
        OutputConfig::Bonded(c) => &c.name,
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

    // Advertise PTP clock(s) when any flow declares a `clock_domain`. The
    // manager UI gates the ST 2110 PTP card on the presence of this list,
    // and external NMOS controllers use it to validate IS-04 sources whose
    // `clock_name` references one of the entries below.
    let mut clocks: Vec<serde_json::Value> = Vec::new();
    let mut seen_domains: std::collections::HashSet<u8> = std::collections::HashSet::new();
    for flow in &config.flows {
        if let Some(d) = flow.clock_domain {
            if seen_domains.insert(d) {
                clocks.push(serde_json::json!({
                    "name": "clk0",
                    "ref_type": "ptp",
                    "traceable": true,
                    "version": "IEEE1588-2008",
                    "gmid": "00-00-00-00-00-00-00-00",
                    "locked": false
                }));
            }
        }
    }

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
        clocks,
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
        if !flow.input_ids.is_empty() {
            receivers.push(receiver_uuid(nid, &flow.id).to_string());
        }
        if let Ok(resolved) = config.resolve_flow(flow) {
            for output in &resolved.outputs {
                senders.push(sender_uuid(nid, &flow.id, output_id(output)).to_string());
            }
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
        .filter_map(|f| {
            let resolved = config.resolve_flow(f).ok()?;
            let input = &resolved.active_input()?.config;
            Some(NmosSource {
                id: source_uuid(&nid, &f.id).to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!("{} input ({})", f.name, input_type_str(input)),
                tags: serde_json::json!({}),
                format: input_format(input).into(),
                caps: source_caps(input),
                device_id: did.to_string(),
                parents: vec![],
                clock_name: f
                    .clock_domain
                    .map(|_| serde_json::Value::String("clk0".into()))
                    .unwrap_or(serde_json::Value::Null),
            })
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
        let resolved = match config.resolve_flow(f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let input = match resolved.active_input() {
            Some(def) => &def.config,
            None => continue,
        };
        let sid = source_uuid(&nid, &f.id);
        if id == sid.to_string() {
            return Ok(Json(NmosSource {
                id: sid.to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!("{} input ({})", f.name, input_type_str(input)),
                tags: serde_json::json!({}),
                format: input_format(input).into(),
                caps: source_caps(input),
                device_id: did.to_string(),
                parents: vec![],
                clock_name: f
                    .clock_domain
                    .map(|_| serde_json::Value::String("clk0".into()))
                    .unwrap_or(serde_json::Value::Null),
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
        .map(|f| {
            let resolved_input = config.resolve_flow(f).ok().and_then(|r| r.active_input().map(|d| d.config.clone()));
            let format = resolved_input.as_ref()
                .map(|i| input_format(i))
                .unwrap_or("urn:x-nmos:format:mux");
            NmosFlow {
                id: flow_uuid(&nid, &f.id).to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!("Flow: {}", f.name),
                tags: serde_json::json!({}),
                format: format.into(),
                source_id: source_uuid(&nid, &f.id).to_string(),
                device_id: did.to_string(),
                parents: vec![],
            }
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
            let resolved_input = config.resolve_flow(f).ok().and_then(|r| r.active_input().map(|d| d.config.clone()));
            let format = resolved_input.as_ref()
                .map(|i| input_format(i))
                .unwrap_or("urn:x-nmos:format:mux");
            return Ok(Json(NmosFlow {
                id: fid.to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!("Flow: {}", f.name),
                tags: serde_json::json!({}),
                format: format.into(),
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
        let resolved = match config.resolve_flow(f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for output in &resolved.outputs {
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
        let resolved = match config.resolve_flow(f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for output in &resolved.outputs {
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
        .filter_map(|f| {
            let resolved = config.resolve_flow(f).ok()?;
            let input = resolved.active_input()?.config.clone();
            Some(NmosReceiver {
                id: receiver_uuid(&nid, &f.id).to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!("{} receiver ({})", f.name, input_type_str(&input)),
                tags: serde_json::json!({}),
                format: input_format(&input).into(),
                caps: receiver_caps(&input),
                device_id: did.to_string(),
                transport: input_transport(&input).into(),
                interface_bindings: vec![],
                subscription: serde_json::json!({"sender_id": null, "active": true}),
            })
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
        let resolved = match config.resolve_flow(f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let input = match resolved.active_input() {
            Some(def) => def.config.clone(),
            None => continue,
        };
        let rid = receiver_uuid(&nid, &f.id);
        if id == rid.to_string() {
            return Ok(Json(NmosReceiver {
                id: rid.to_string(),
                version: nmos_version(),
                label: f.name.clone(),
                description: format!(
                    "{} receiver ({})",
                    f.name,
                    input_type_str(&input)
                ),
                tags: serde_json::json!({}),
                format: input_format(&input).into(),
                caps: receiver_caps(&input),
                device_id: did.to_string(),
                transport: input_transport(&input).into(),
                interface_bindings: vec![],
                subscription: serde_json::json!({"sender_id": null, "active": true}),
            }));
        }
    }
    Err(StatusCode::NOT_FOUND)
}
