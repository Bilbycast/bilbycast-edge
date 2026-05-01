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
    let mut payload = crate::manager::client::build_health_payload(
        &state.app.flow_manager,
        &api_addr,
        monitor_addr.as_deref(),
        &state.app.resource_state,
        &state.static_caps,
        &state.live_gpu,
    );
    // Stamp uptime from the application's start_time so the SPA can
    // render the system bar without a separate /api/v1/stats call.
    if let Some(obj) = payload.as_object_mut() {
        let uptime = state.app.start_time.elapsed().as_secs();
        obj.insert("uptime_secs".into(), serde_json::json!(uptime));
        obj.insert(
            "active_flows".into(),
            serde_json::json!(state.app.flow_manager.active_flow_count()),
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
    let addr = format!("{}:{}", config.listen_addr, config.listen_port);
    let listener = TcpListener::bind(&addr).await.map_err(|e| {
        crate::util::port_error::annotate_bind_error(e, &addr, "Monitor dashboard server")
    })?;
    tracing::info!("Monitor dashboard listening on {addr}");

    let monitor_state = MonitorState {
        app: state,
        static_caps,
        live_gpu,
    };
    let router = build_monitor_router(monitor_state);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                shutdown_token.cancelled().await;
            })
            .await
        {
            tracing::error!("Monitor server error: {e}");
        }
    });

    Ok(handle)
}
