// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! AMWA IS-04 v1.3 registration client.
//!
//! When `nmos_registration.enabled = true`, a background task POSTs the
//! node's IS-04 resource set (node, device, sources, flows, senders,
//! receivers) to the configured registry's
//! `<base>/x-nmos/registration/v1.3/resource` endpoint, and heartbeats the
//! node every `heartbeat_interval_secs` against
//! `<base>/x-nmos/registration/v1.3/health/nodes/{id}`. On shutdown the
//! node resource is DELETEd best-effort so the registry stops listing it.
//!
//! Resource UUIDs are deterministic UUID v5 values rooted at the persisted
//! `node_id` (see [`crate::api::nmos`]) — re-POSTing the same set updates the
//! registry record in place, with `version` advanced only when the live
//! `AppConfig` actually changes.
//!
//! The task is the single registration-driven contact with the registry; the
//! IS-04 GET endpoints on this node continue to serve directly via Axum and
//! are unaffected.
//!
//! See `docs/nmos.md` ("Registration Client") for the operator-facing config
//! reference and `docs/events-and-alarms.md` ("nmos_registry") for the
//! lifecycle events this task emits.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::api::nmos;
use crate::config::models::{AppConfig, NmosRegistrationConfig};
use crate::manager::events::EventSender;

/// Process-wide HTTP client shared across registration ticks.
fn client(timeout: Duration) -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    // The first caller wins the timeout — this is fine because validation
    // bounds it to 1..=30 s and the field is rarely retuned at runtime.
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(timeout)
                .connect_timeout(Duration::from_secs(5))
                .user_agent(format!("bilbycast-edge/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("build reqwest client")
        })
        .clone()
}

/// Resource snapshot used for change detection. Hashed once per heartbeat tick;
/// when the hash differs from the last successfully-registered hash, the client
/// re-POSTs every resource with a fresh `version` stamp.
struct Snapshot {
    node_id: String,
    node: serde_json::Value,
    device: serde_json::Value,
    sources: Vec<serde_json::Value>,
    flows: Vec<serde_json::Value>,
    senders: Vec<serde_json::Value>,
    receivers: Vec<serde_json::Value>,
    /// Hash of the (versionless) JSON content. Stable across calls when the
    /// underlying config has not changed.
    content_hash: u64,
}

impl Snapshot {
    /// Build a snapshot from the live config. `listen_host` / `listen_port` /
    /// `https` are the values to advertise on the node `href` — typically the
    /// edge's actual reachable address (resolved by `main.rs`), not the
    /// `0.0.0.0` bind address.
    fn build(
        config: &AppConfig,
        listen_host: &str,
        listen_port: u16,
        https: bool,
    ) -> Self {
        // Build content first with a stable placeholder version so the hash is
        // independent of timestamps. Then re-stamp with a fresh version once we
        // know the snapshot needs to be sent.
        const PLACEHOLDER: &str = "0:0";
        let node_id = nmos::node_uuid_from_config(config).to_string();
        let node = nmos::build_node_value(config, PLACEHOLDER, listen_host, listen_port, https);
        let device = nmos::build_device_value(config, PLACEHOLDER);
        let sources = nmos::build_source_values(config, PLACEHOLDER);
        let flows = nmos::build_flow_values(config, PLACEHOLDER);
        let senders = nmos::build_sender_values(config, PLACEHOLDER);
        let receivers = nmos::build_receiver_values(config, PLACEHOLDER);

        let mut hasher = DefaultHasher::new();
        node.to_string().hash(&mut hasher);
        device.to_string().hash(&mut hasher);
        for v in &sources {
            v.to_string().hash(&mut hasher);
        }
        for v in &flows {
            v.to_string().hash(&mut hasher);
        }
        for v in &senders {
            v.to_string().hash(&mut hasher);
        }
        for v in &receivers {
            v.to_string().hash(&mut hasher);
        }
        let content_hash = hasher.finish();

        Snapshot {
            node_id,
            node,
            device,
            sources,
            flows,
            senders,
            receivers,
            content_hash,
        }
    }

    /// Replace the placeholder version on every resource with a fresh one.
    fn stamp_version(&mut self, version: &str) {
        Self::set_version(&mut self.node, version);
        Self::set_version(&mut self.device, version);
        for v in &mut self.sources {
            Self::set_version(v, version);
        }
        for v in &mut self.flows {
            Self::set_version(v, version);
        }
        for v in &mut self.senders {
            Self::set_version(v, version);
        }
        for v in &mut self.receivers {
            Self::set_version(v, version);
        }
    }

    fn set_version(value: &mut serde_json::Value, version: &str) {
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "version".into(),
                serde_json::Value::String(version.to_string()),
            );
        }
    }
}

/// Public entry point — spawned once from `main.rs` after the manager-client
/// setup. Returns when `cancel` fires.
pub async fn run(
    config: Arc<RwLock<AppConfig>>,
    reg_cfg: NmosRegistrationConfig,
    listen_host: String,
    listen_port: u16,
    https: bool,
    events: EventSender,
    cancel: CancellationToken,
) {
    let registry_base = reg_cfg.registry_url.trim_end_matches('/').to_string();
    let api_version = reg_cfg.api_version.clone();
    let timeout = Duration::from_secs(reg_cfg.request_timeout_secs.max(1) as u64);
    let heartbeat = Duration::from_secs(reg_cfg.heartbeat_interval_secs.max(1) as u64);
    let bearer = reg_cfg.bearer_token.clone();
    let http = client(timeout);

    tracing::info!(
        "NMOS registration client starting — registry={registry_base} \
         api_version={api_version} heartbeat={}s",
        heartbeat.as_secs()
    );

    // State: last successfully-registered content hash, and the node id of
    // that registration (so we know which id to DELETE on shutdown).
    let mut last_registered_hash: Option<u64> = None;
    let mut last_node_id: Option<String> = None;
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);

    loop {
        // Build a snapshot of the current config.
        let mut snapshot = {
            let cfg = config.read().await;
            Snapshot::build(&cfg, &listen_host, listen_port, https)
        };

        let needs_registration = match last_registered_hash {
            Some(prev) => prev != snapshot.content_hash,
            None => true,
        };

        if needs_registration {
            let version = nmos::fresh_version();
            snapshot.stamp_version(&version);
            match post_all(&http, &registry_base, &api_version, bearer.as_deref(), &snapshot).await
            {
                Ok(()) => {
                    let first_time = last_registered_hash.is_none();
                    last_registered_hash = Some(snapshot.content_hash);
                    last_node_id = Some(snapshot.node_id.clone());
                    backoff = Duration::from_secs(1);
                    if first_time {
                        events.emit_nmos_registered(&registry_base, &snapshot.node_id);
                        tracing::info!(
                            "NMOS registration: posted node {} to {registry_base}",
                            snapshot.node_id
                        );
                    } else {
                        tracing::debug!(
                            "NMOS registration: refreshed resources for node {}",
                            snapshot.node_id
                        );
                    }
                }
                Err(e) => {
                    match e {
                        PostError::Http { resource, status, body } => {
                            events.emit_nmos_registration_failed(
                                &registry_base,
                                &resource,
                                status,
                                &body,
                            );
                            tracing::warn!(
                                "NMOS registration: POST {resource} returned HTTP {status}: {body}"
                            );
                        }
                        PostError::Network(err) => {
                            events.emit_nmos_registry_unreachable(&registry_base, &err);
                            tracing::warn!("NMOS registration: registry unreachable: {err}");
                        }
                    }
                    if sleep_or_cancel(&cancel, backoff).await {
                        break;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
        }

        // Heartbeat phase — only fires when we have a successful registration
        // to refresh.
        if last_registered_hash.is_some() {
            match heartbeat_once(
                &http,
                &registry_base,
                &api_version,
                bearer.as_deref(),
                &snapshot.node_id,
            )
            .await
            {
                Ok(()) => {
                    backoff = Duration::from_secs(1);
                }
                Err(HeartbeatError::Expired { status }) => {
                    events.emit_nmos_heartbeat_lost(&registry_base, status);
                    tracing::info!(
                        "NMOS heartbeat: registry returned HTTP {status} — re-registering on next tick"
                    );
                    last_registered_hash = None;
                }
                Err(HeartbeatError::Network(err)) => {
                    events.emit_nmos_registry_unreachable(&registry_base, &err);
                    tracing::warn!("NMOS heartbeat: registry unreachable: {err}");
                    if sleep_or_cancel(&cancel, backoff).await {
                        break;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
        }

        if sleep_or_cancel(&cancel, heartbeat).await {
            break;
        }
    }

    // Best-effort DELETE on shutdown so the registry stops listing the node
    // immediately. 1 s timeout — we're already on the shutdown path.
    if let Some(nid) = last_node_id {
        let url = format!(
            "{registry_base}/x-nmos/registration/{api_version}/resource/nodes/{nid}"
        );
        let mut req = http.delete(&url).timeout(Duration::from_secs(1));
        if let Some(ref tok) = bearer {
            req = req.bearer_auth(tok);
        }
        match req.send().await {
            Ok(r) => tracing::info!(
                "NMOS registration: DELETE {} on shutdown returned {}",
                url,
                r.status()
            ),
            Err(e) => {
                tracing::debug!("NMOS registration: DELETE {url} on shutdown failed: {e}")
            }
        }
    }

    tracing::info!("NMOS registration client stopped");
}

/// Returns true when `cancel` fires before the deadline.
async fn sleep_or_cancel(cancel: &CancellationToken, dur: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}

#[derive(Debug)]
enum PostError {
    Http {
        resource: String,
        status: u16,
        body: String,
    },
    Network(String),
}

#[derive(Debug)]
enum HeartbeatError {
    Expired { status: u16 },
    Network(String),
}

/// Post the entire resource set in dependency order. Returns the first error;
/// later resources in the set are not posted.
async fn post_all(
    http: &reqwest::Client,
    registry_base: &str,
    api_version: &str,
    bearer: Option<&str>,
    snap: &Snapshot,
) -> Result<(), PostError> {
    // Order matters: parents (node, device, source, flow) must exist before
    // their children (sender, receiver).
    post_resource(http, registry_base, api_version, bearer, "node", &snap.node).await?;
    post_resource(http, registry_base, api_version, bearer, "device", &snap.device).await?;
    for src in &snap.sources {
        post_resource(http, registry_base, api_version, bearer, "source", src).await?;
    }
    for f in &snap.flows {
        post_resource(http, registry_base, api_version, bearer, "flow", f).await?;
    }
    for s in &snap.senders {
        post_resource(http, registry_base, api_version, bearer, "sender", s).await?;
    }
    for r in &snap.receivers {
        post_resource(http, registry_base, api_version, bearer, "receiver", r).await?;
    }
    Ok(())
}

async fn post_resource(
    http: &reqwest::Client,
    registry_base: &str,
    api_version: &str,
    bearer: Option<&str>,
    resource_type: &str,
    data: &serde_json::Value,
) -> Result<(), PostError> {
    let url = format!("{registry_base}/x-nmos/registration/{api_version}/resource");
    let body = serde_json::to_string(&serde_json::json!({
        "type": resource_type,
        "data": data,
    }))
    .map_err(|e| PostError::Network(format!("serialize body: {e}")))?;
    let mut req = http
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body);
    if let Some(tok) = bearer {
        req = req.bearer_auth(tok);
    }
    let resp: reqwest::Response = req
        .send()
        .await
        .map_err(|e| PostError::Network(e.to_string()))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let status_u16 = status.as_u16();
        let body: String = resp.text().await.unwrap_or_default();
        Err(PostError::Http {
            resource: resource_type.to_string(),
            status: status_u16,
            body: body.chars().take(500).collect(),
        })
    }
}

async fn heartbeat_once(
    http: &reqwest::Client,
    registry_base: &str,
    api_version: &str,
    bearer: Option<&str>,
    node_id: &str,
) -> Result<(), HeartbeatError> {
    let url =
        format!("{registry_base}/x-nmos/registration/{api_version}/health/nodes/{node_id}");
    let mut req = http.post(&url);
    if let Some(tok) = bearer {
        req = req.bearer_auth(tok);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| HeartbeatError::Network(e.to_string()))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        // 404 = registry expired the node; 409 = node id collision; both mean
        // re-register.
        Err(HeartbeatError::Expired {
            status: status.as_u16(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::AppConfig;

    fn min_config_with_one_flow() -> AppConfig {
        // Build via JSON so we don't have to enumerate every field on the
        // protocol-specific config structs (which lack Default impls and have
        // dozens of optional fields).
        let json = serde_json::json!({
            "version": 2,
            "node_id": "11111111-1111-1111-1111-111111111111",
            "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
            "inputs": [{
                "id": "in1",
                "name": "in1",
                "type": "rtp",
                "bind_addr": "0.0.0.0:5000"
            }],
            "outputs": [{
                "type": "rtp",
                "id": "out1",
                "name": "out1",
                "dest_addr": "127.0.0.1:5004"
            }],
            "flows": [{
                "id": "f1",
                "name": "f1",
                "input_ids": ["in1"],
                "output_ids": ["out1"]
            }]
        });
        serde_json::from_value(json).expect("min config must deserialize")
    }

    #[test]
    fn snapshot_hash_is_stable_across_rebuilds() {
        let cfg = min_config_with_one_flow();
        let s1 = Snapshot::build(&cfg, "10.0.0.1", 8080, false);
        let s2 = Snapshot::build(&cfg, "10.0.0.1", 8080, false);
        assert_eq!(s1.content_hash, s2.content_hash);
    }

    #[test]
    fn snapshot_hash_changes_when_a_flow_is_added() {
        let cfg = min_config_with_one_flow();
        let s1 = Snapshot::build(&cfg, "10.0.0.1", 8080, false);

        // Add a second flow + output via JSON re-build so we don't depend on
        // any per-protocol struct shape.
        let mut bigger: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        bigger["outputs"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "type": "rtp",
                "id": "out2",
                "name": "out2",
                "dest_addr": "127.0.0.1:5006"
            }));
        bigger["flows"].as_array_mut().unwrap().push(serde_json::json!({
            "id": "f2",
            "name": "f2",
            "input_ids": ["in1"],
            "output_ids": ["out2"]
        }));
        let cfg2: AppConfig = serde_json::from_value(bigger).unwrap();
        let s2 = Snapshot::build(&cfg2, "10.0.0.1", 8080, false);
        assert_ne!(s1.content_hash, s2.content_hash);
    }

    #[test]
    fn snapshot_hash_excludes_version_so_it_is_stable_under_stamping() {
        let cfg = min_config_with_one_flow();
        let mut s1 = Snapshot::build(&cfg, "10.0.0.1", 8080, false);
        let mut s2 = Snapshot::build(&cfg, "10.0.0.1", 8080, false);
        s1.stamp_version("100:0");
        s2.stamp_version("999:42");
        // The recorded content_hash was taken before stamping and must not
        // be perturbed by version differences.
        assert_eq!(s1.content_hash, s2.content_hash);
        assert_eq!(s1.node["version"], "100:0");
        assert_eq!(s2.node["version"], "999:42");
    }

    #[test]
    fn snapshot_orders_resources_for_dependency_safety() {
        let cfg = min_config_with_one_flow();
        let s = Snapshot::build(&cfg, "10.0.0.1", 8080, false);
        // Node and device are singletons per node.
        assert!(s.node["id"].is_string());
        assert_eq!(s.device["node_id"], s.node["id"]);
        // The flow points back to the source we publish.
        assert_eq!(s.sources.len(), 1);
        assert_eq!(s.flows.len(), 1);
        assert_eq!(s.flows[0]["source_id"], s.sources[0]["id"]);
        // Sender's flow_id matches the flow we built.
        assert_eq!(s.senders.len(), 1);
        assert_eq!(s.senders[0]["flow_id"], s.flows[0]["id"]);
        // Receiver belongs to the flow's device.
        assert_eq!(s.receivers.len(), 1);
        assert_eq!(s.receivers[0]["device_id"], s.device["id"]);
    }

    #[test]
    fn snapshot_node_advertises_listen_host_and_https_flag() {
        let cfg = min_config_with_one_flow();
        let s_http = Snapshot::build(&cfg, "10.1.2.3", 8080, false);
        assert!(s_http.node["href"]
            .as_str()
            .unwrap()
            .starts_with("http://10.1.2.3:8080/"));

        let s_https = Snapshot::build(&cfg, "10.1.2.3", 8443, true);
        assert!(s_https.node["href"]
            .as_str()
            .unwrap()
            .starts_with("https://10.1.2.3:8443/"));
        assert_eq!(s_https.node["api"]["endpoints"][0]["protocol"], "https");
    }
}
