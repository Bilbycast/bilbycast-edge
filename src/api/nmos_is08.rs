// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! NMOS IS-08 Audio Channel Mapping API (v1.0).
//!
//! IS-08 lets external NMOS controllers query and modify how the audio
//! channels of a node's inputs (sources) and outputs (senders) are mapped to
//! one another. For bilbycast-edge, IS-08 surfaces the channel layout of
//! ST 2110-30/-31 audio inputs and outputs, exposing each one as an "input"
//! or "output" entry under `/x-nmos/channelmapping/v1.0/io`. The map itself
//! is a passthrough — bilbycast does not currently re-route channels —
//! but the endpoints exist so NMOS controllers can stage and activate maps,
//! and so the manager UI can render the channel layout.
//!
//! ## Endpoints
//!
//! - `GET /io` — list all IS-08 inputs and outputs
//! - `GET /map/active` — currently-active mapping (passthrough by default)
//! - `GET /map/staged` — staged mapping pending activation
//! - `POST /map/activate` — activate a previously-staged mapping
//!
//! ## Persistence
//!
//! The active map is persisted to `<config_dir>/nmos_channel_map.json` so it
//! survives node restarts. The staged map is in-memory only.
//!
//! ## Backward compatibility
//!
//! The router is mounted under a fresh URL prefix (`/x-nmos/channelmapping/`)
//! and adds no fields to existing IS-04/IS-05 endpoints. Old NMOS controllers
//! that don't speak IS-08 see no behavioural change.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::server::AppState;
use crate::config::models::{InputConfig, OutputConfig, St2110AudioInputConfig, St2110AudioOutputConfig};

/// In-memory IS-08 state.
///
/// `active` is the currently-active map. `staged` holds a pending change.
/// Both are stored as a `ChannelMap` keyed by IS-08 output id (`st2110_30:<flow>:<output>`).
///
/// `channel_map_tx` is a `tokio::sync::watch` sender that broadcasts the
/// active map to subscribers (per-output PCM transcoders) on every
/// activation. Subscribers snapshot the map via `borrow()` per packet to read
/// their own routing — see `engine::audio_transcode::MatrixSource::Is08Tracked`.
/// The watch is initialized with an empty map at construction time so
/// subscribers always see a valid value before the first activation.
pub struct Is08State {
    pub active: Mutex<ChannelMap>,
    pub staged: Mutex<ChannelMap>,
    pub persist_path: Mutex<Option<PathBuf>>,
    pub channel_map_tx: watch::Sender<Arc<ChannelMap>>,
}

impl Is08State {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(Arc::new(ChannelMap::default()));
        Self {
            active: Mutex::new(ChannelMap::default()),
            staged: Mutex::new(ChannelMap::default()),
            persist_path: Mutex::new(None),
            channel_map_tx: tx,
        }
    }

    /// Subscribe to the IS-08 active channel map watch. Each subscriber sees
    /// every activation. Cheap; can be called from any task. Currently unused
    /// directly — `engine::audio_transcode::subscribe_global_is08` is the
    /// preferred entry point — but kept on the type for direct callers.
    #[allow(dead_code)]
    pub fn subscribe_active(&self) -> watch::Receiver<Arc<ChannelMap>> {
        self.channel_map_tx.subscribe()
    }

    /// Construct, attempting to read the active map from `path`. If the file
    /// is missing or unreadable the active map starts empty (the I/O loop
    /// rebuilds it as a passthrough on first use). Errors are logged once
    /// and otherwise swallowed — IS-08 is best-effort and must never block
    /// flow startup.
    ///
    /// Side-effect: registers the channel-map watch sender in the global
    /// `engine::audio_transcode` slot so audio output spawn modules can
    /// subscribe to live IS-08 routing without plumbing AppState through
    /// FlowManager. Idempotent — only the first call wins.
    pub fn load_or_default(path: PathBuf) -> Arc<Self> {
        let s = Arc::new(Self::new());
        *s.persist_path.lock().unwrap() = Some(path.clone());
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(json) => match serde_json::from_str::<ChannelMap>(&json) {
                    Ok(map) => {
                        *s.active.lock().unwrap() = map.clone();
                        // Seed the watch with the persisted map.
                        let _ = s.channel_map_tx.send(Arc::new(map));
                        tracing::info!("IS-08: loaded active channel map from {}", path.display());
                    }
                    Err(e) => {
                        tracing::warn!("IS-08: failed to parse {}: {}", path.display(), e);
                    }
                },
                Err(e) => {
                    tracing::warn!("IS-08: failed to read {}: {}", path.display(), e);
                }
            }
        }
        // Register the channel-map watch sender globally so audio output
        // spawn modules can subscribe to it.
        crate::engine::audio_transcode::set_global_is08_sender(s.channel_map_tx.clone());
        s
    }

    /// Persist the active map to disk. Best-effort: errors are logged but
    /// not surfaced to the caller (IS-08 controllers receive 200 OK on a
    /// successful in-memory swap regardless of the disk write).
    fn save_active(&self) {
        let path = match self.persist_path.lock().unwrap().clone() {
            Some(p) => p,
            None => return,
        };
        let map = self.active.lock().unwrap().clone();
        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!("IS-08: failed to write {}: {}", path.display(), e);
                }
            }
            Err(e) => tracing::warn!("IS-08: failed to serialize channel map: {}", e),
        }
    }
}

/// IS-08 channel map. Outer key is the IS-08 output id; inner is a list of
/// `(input_id, channel_index)` pairs that feed each output channel.
///
/// Empty map = passthrough (every output channel mirrors the same-numbered
/// input channel from the matching IS-08 input).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ChannelMap {
    pub outputs: BTreeMap<String, OutputMapping>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OutputMapping {
    /// Per-channel routing entries.
    pub channels: Vec<ChannelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEntry {
    /// IS-08 input id, or `null` for muted.
    pub input: Option<String>,
    /// 0-based channel index inside the source input. `null` when `input` is null.
    pub channel_index: Option<u32>,
}

/// IS-08 IO listing payload.
#[derive(Serialize)]
struct IoResponse {
    inputs: BTreeMap<String, IoBlock>,
    outputs: BTreeMap<String, IoBlock>,
}

#[derive(Serialize)]
struct IoBlock {
    name: String,
    description: String,
    channels: Vec<ChannelDescriptor>,
    properties: serde_json::Value,
}

#[derive(Serialize)]
struct ChannelDescriptor {
    label: String,
}

/// Build the IS-08 router.
pub fn nmos_is08_router() -> Router<AppState> {
    Router::new()
        .route("/", get(root))
        .route("/io", get(get_io))
        .route("/io/", get(get_io))
        .route("/map", get(map_root))
        .route("/map/", get(map_root))
        .route("/map/active", get(get_active))
        .route("/map/active/", get(get_active))
        .route("/map/staged", get(get_staged).post(post_staged))
        .route("/map/staged/", get(get_staged).post(post_staged))
        .route("/map/activate", post(activate))
}

async fn root() -> Json<Vec<&'static str>> {
    Json(vec!["io/", "map/"])
}

async fn map_root() -> Json<Vec<&'static str>> {
    Json(vec!["active/", "staged/", "activate"])
}

async fn get_io(State(state): State<AppState>) -> Json<IoResponse> {
    let cfg = state.config.read().await;
    let mut inputs: BTreeMap<String, IoBlock> = BTreeMap::new();
    let mut outputs: BTreeMap<String, IoBlock> = BTreeMap::new();

    for f in &cfg.flows {
        let resolved = match cfg.resolve_flow(f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Inputs: an audio input contributes one IS-08 input.
        if let Some(InputConfig::St2110_30(c) | InputConfig::St2110_31(c)) = &resolved.input {
            let id = format!("st2110_30:{}", f.id);
            inputs.insert(
                id.clone(),
                io_block_for_audio_input(&f.name, c),
            );
        }
        // Outputs: each ST 2110-30/-31 output contributes one IS-08 output.
        for output in &resolved.outputs {
            if let OutputConfig::St2110_30(c) | OutputConfig::St2110_31(c) = output {
                let id = format!("st2110_30:{}:{}", f.id, c.id);
                outputs.insert(id, io_block_for_audio_output(&c.name, c));
            }
        }
    }

    Json(IoResponse { inputs, outputs })
}

fn io_block_for_audio_input(name: &str, c: &St2110AudioInputConfig) -> IoBlock {
    IoBlock {
        name: name.to_string(),
        description: format!("ST 2110-30 audio input ({} ch @ {} Hz)", c.channels, c.sample_rate),
        channels: (0..c.channels).map(|i| ChannelDescriptor { label: format!("ch{}", i + 1) }).collect(),
        properties: serde_json::json!({
            "sample_rate": c.sample_rate,
            "bit_depth": c.bit_depth,
        }),
    }
}

fn io_block_for_audio_output(name: &str, c: &St2110AudioOutputConfig) -> IoBlock {
    IoBlock {
        name: name.to_string(),
        description: format!("ST 2110-30 audio output ({} ch @ {} Hz)", c.channels, c.sample_rate),
        channels: (0..c.channels).map(|i| ChannelDescriptor { label: format!("ch{}", i + 1) }).collect(),
        properties: serde_json::json!({
            "sample_rate": c.sample_rate,
            "bit_depth": c.bit_depth,
        }),
    }
}

async fn get_active(State(state): State<AppState>) -> Json<ChannelMap> {
    Json(state.is08_state.active.lock().unwrap().clone())
}

async fn get_staged(State(state): State<AppState>) -> Json<ChannelMap> {
    Json(state.is08_state.staged.lock().unwrap().clone())
}

async fn post_staged(
    State(state): State<AppState>,
    Json(map): Json<ChannelMap>,
) -> Result<Json<ChannelMap>, StatusCode> {
    // Bound the staged map size to keep an unauth IS-08 controller from
    // exhausting memory. 1024 outputs × 64 channels each is far above any
    // realistic ST 2110 facility deployment.
    if map.outputs.len() > 1024 {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    for entry in map.outputs.values() {
        if entry.channels.len() > 64 {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
    }
    *state.is08_state.staged.lock().unwrap() = map.clone();
    if let Some(ref tx) = state.event_sender {
        tx.emit(
            crate::manager::events::EventSeverity::Info,
            crate::manager::events::category::NMOS,
            format!("IS-08 channel map staged ({} outputs)", map.outputs.len()),
        );
    }
    Ok(Json(map))
}

async fn activate(State(state): State<AppState>) -> Json<ChannelMap> {
    let staged = state.is08_state.staged.lock().unwrap().clone();
    *state.is08_state.active.lock().unwrap() = staged.clone();
    state.is08_state.save_active();
    // Broadcast the new active map to subscribers (per-output transcoders).
    // `send` only fails when there are no receivers, which we treat as a
    // no-op since transcoders may not exist yet.
    let _ = state
        .is08_state
        .channel_map_tx
        .send(Arc::new(staged.clone()));
    if let Some(ref tx) = state.event_sender {
        tx.emit(
            crate::manager::events::EventSeverity::Info,
            crate::manager::events::category::NMOS,
            format!(
                "IS-08 channel map activated ({} outputs)",
                staged.outputs.len()
            ),
        );
    }
    Json(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_map_roundtrip() {
        let mut m = ChannelMap::default();
        let mut out = OutputMapping::default();
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:flow1".into()),
            channel_index: Some(0),
        });
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:flow1".into()),
            channel_index: Some(1),
        });
        m.outputs.insert("st2110_30:flow2:out1".into(), out);
        let s = serde_json::to_string(&m).unwrap();
        let back: ChannelMap = serde_json::from_str(&s).unwrap();
        assert_eq!(back.outputs.len(), 1);
        assert_eq!(back.outputs.get("st2110_30:flow2:out1").unwrap().channels.len(), 2);
    }

    #[test]
    fn test_load_or_default_missing_path_is_empty() {
        let p = std::env::temp_dir().join(format!(
            "bilbycast-is08-test-missing-{}.json",
            std::process::id()
        ));
        // Ensure absent.
        let _ = std::fs::remove_file(&p);
        let s = Is08State::load_or_default(p);
        assert!(s.active.lock().unwrap().outputs.is_empty());
    }

    #[test]
    fn test_save_active_roundtrip_via_disk() {
        let p = std::env::temp_dir().join(format!(
            "bilbycast-is08-test-save-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let s = Is08State::load_or_default(p.clone());
        // Set an active mapping and save.
        let mut m = ChannelMap::default();
        m.outputs.insert(
            "st2110_30:f:o".into(),
            OutputMapping {
                channels: vec![ChannelEntry {
                    input: Some("st2110_30:f".into()),
                    channel_index: Some(0),
                }],
            },
        );
        *s.active.lock().unwrap() = m;
        s.save_active();
        // Reload from disk via a fresh state.
        let s2 = Is08State::load_or_default(p.clone());
        assert_eq!(s2.active.lock().unwrap().outputs.len(), 1);
        let _ = std::fs::remove_file(&p);
    }
}
