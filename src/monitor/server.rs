// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api::models::AllStatsResponse;
use crate::api::server::AppState;
use crate::api::stats::gather_all_stats;
use crate::config::models::MonitorConfig;
use crate::engine::hardware_probe::{LiveUtilizationState, StaticCapabilities};

use super::dashboard::DASHBOARD_HTML;

/// Combined router state for the monitor dashboard. Wraps the main
/// [`AppState`] plus the hardware-probe handles the `/api/health`
/// handler needs to mirror the JSON the manager receives.
#[derive(Clone)]
pub struct MonitorState {
    pub app: AppState,
    pub static_caps: Arc<StaticCapabilities>,
    pub live_gpu: Arc<LiveUtilizationState>,
}

fn build_monitor_router(state: MonitorState) -> Router {
    Router::new()
        .route("/", get(dashboard_page))
        .route("/api/stats", get(monitor_stats))
        .route("/api/health", get(monitor_health))
        .route("/api/thumbnail/{flow_id}", get(monitor_thumbnail))
        .route("/api/thumbnail/{flow_id}/input/{input_id}", get(monitor_input_thumbnail))
        .route("/api/tunnels", get(monitor_tunnels))
        .route("/api/ws", get(monitor_ws))
        .with_state(state)
}

async fn dashboard_page() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn monitor_stats(State(state): State<MonitorState>) -> Json<AllStatsResponse> {
    Json(gather_all_stats(&state.app).await)
}

/// Rich health payload that mirrors what the edge sends to the manager
/// over WS — capabilities, resource budget, live NVML utilisation, and
/// (when the `display` Cargo feature is on) the local-display
/// enumeration. The edge's own dashboard SPA renders the Resources card
/// and capability list off this endpoint.
async fn monitor_health(State(state): State<MonitorState>) -> Json<serde_json::Value> {
    let (api_addr, monitor_addr) = {
        let cfg = state.app.config.read().await;
        (
            format!("{}:{}", cfg.server.listen_addr, cfg.server.listen_port),
            cfg.monitor
                .as_ref()
                .map(|m| format!("{}:{}", m.listen_addr, m.listen_port)),
        )
    };
    let payload = crate::manager::client::build_health_payload(
        &state.app.flow_manager,
        &api_addr,
        monitor_addr.as_deref(),
        &state.app.resource_state,
        &state.static_caps,
        &state.live_gpu,
        None, // network sampler — stateless enumeration on the monitor path
        None, // cellular cache — capability advertised via the manager path
        state.app.start_time,
    );
    // Inject the device-local manager-link indicator. This is the edge's OWN
    // view of its link to the manager (NOT part of the payload sent UP to the
    // manager — `build_health_payload` above is untouched). The dashboard SPA
    // renders a green/red "Manager" badge off this field. Keyed `"manager"`
    // with the same typed `ManagerLinkStatus` shape the REST `/health` endpoint
    // exposes, so both local surfaces present an identical JSON object.
    let mut payload = payload;
    if let serde_json::Value::Object(ref mut map) = payload {
        map.insert(
            "manager".to_string(),
            state.app.manager_link.snapshot_json(),
        );
    }
    Json(payload)
}

async fn monitor_thumbnail(
    State(state): State<MonitorState>,
    Path(flow_id): Path<String>,
) -> impl IntoResponse {
    let stats = state.app.flow_manager.stats();
    let Some(acc) = stats.flow_stats.get(&flow_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let Some(thumb_acc) = acc.thumbnail.get() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let guard = thumb_acc.latest_jpeg.lock().unwrap();
    let Some((jpeg_data, _ts)) = guard.as_ref() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let data = jpeg_data.clone();
    drop(guard);

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "image/jpeg".parse().unwrap());
    headers.insert(
        "cache-control",
        "no-cache, no-store, must-revalidate".parse().unwrap(),
    );
    Ok((headers, data))
}

async fn monitor_input_thumbnail(
    State(state): State<MonitorState>,
    Path((flow_id, input_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let stats = state.app.flow_manager.stats();
    let Some(acc) = stats.flow_stats.get(&flow_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let Some(thumb_acc) = acc.per_input_thumbnails.get(&input_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let guard = thumb_acc.latest_jpeg.lock().unwrap();
    let Some((jpeg_data, _ts)) = guard.as_ref() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let data = jpeg_data.clone();
    drop(guard);

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "image/jpeg".parse().unwrap());
    headers.insert(
        "cache-control",
        "no-cache, no-store, must-revalidate".parse().unwrap(),
    );
    Ok((headers, data))
}

async fn monitor_tunnels(State(state): State<MonitorState>) -> impl IntoResponse {
    let tunnels = state.app.tunnel_manager.list_tunnels();
    Json(tunnels)
}

async fn monitor_ws(
    ws: WebSocketUpgrade,
    State(state): State<MonitorState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| monitor_ws_connection(socket, state))
}

async fn monitor_ws_connection(mut socket: WebSocket, state: MonitorState) {
    let mut rx = state.app.ws_stats_tx.subscribe();

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(msg) => {
                        if socket.send(Message::Text(msg.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}

pub async fn start_monitor_server(
    state: AppState,
    config: &MonitorConfig,
    static_caps: Arc<StaticCapabilities>,
    live_gpu: Arc<LiveUtilizationState>,
    shutdown_token: CancellationToken,
) -> anyhow::Result<JoinHandle<()>> {
    // Resolve effective dual-stack bind list. `listen_addrs` takes
    // precedence over `listen_addr`; v6 entries get IPV6_V6ONLY=1 so
    // they coexist with v4 listeners on the same port.
    let bind_entries = config.effective_listen_addrs();
    let bind_ips =
        crate::config::validation::parse_listen_addrs(&bind_entries, "monitor")
            .map_err(|e| anyhow::anyhow!("Monitor bind addresses: {e}"))?;
    let listen_port = config.listen_port;
    let bind_addrs: Vec<std::net::SocketAddr> = bind_ips
        .iter()
        .map(|ip| std::net::SocketAddr::new(*ip, listen_port))
        .collect();

    let monitor_state = MonitorState {
        app: state,
        static_caps,
        live_gpu,
    };
    let router = build_monitor_router(monitor_state);

    let mut listeners: Vec<(std::net::SocketAddr, TcpListener)> = Vec::new();
    for bind_addr in &bind_addrs {
        let listener = build_monitor_listener(*bind_addr).map_err(|e| {
            crate::util::port_error::annotate_bind_error(
                e,
                &bind_addr.to_string(),
                "Monitor dashboard server",
            )
        })?;
        tracing::info!("Monitor dashboard listening on {bind_addr}");
        listeners.push((*bind_addr, listener));
    }

    let handle = tokio::spawn(async move {
        let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        for (bind_addr, listener) in listeners {
            let router_clone = router.clone();
            let shutdown_clone = shutdown_token.clone();
            set.spawn(async move {
                if let Err(e) = axum::serve(listener, router_clone)
                    .with_graceful_shutdown(async move {
                        shutdown_clone.cancelled().await;
                    })
                    .await
                {
                    tracing::error!("Monitor server on {bind_addr} error: {e}");
                }
            });
        }
        // Wait for the first listener to exit; dropping the set aborts the rest.
        let _ = set.join_next().await;
    });

    Ok(handle)
}

/// Build a tokio `TcpListener` for the monitor dashboard with
/// `IPV6_V6ONLY=1` on v6 sockets and `SO_REUSEADDR` on both families.
/// Matches the contract used by the edge API server's bind path.
fn build_monitor_listener(addr: std::net::SocketAddr) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = match addr.ip() {
        std::net::IpAddr::V4(_) => Domain::IPV4,
        std::net::IpAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    if matches!(addr.ip(), std::net::IpAddr::V6(_)) {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
}
