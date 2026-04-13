// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

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

use super::dashboard::DASHBOARD_HTML;

fn build_monitor_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard_page))
        .route("/api/stats", get(monitor_stats))
        .route("/api/thumbnail/{flow_id}", get(monitor_thumbnail))
        .route("/api/thumbnail/{flow_id}/input/{input_id}", get(monitor_input_thumbnail))
        .route("/api/tunnels", get(monitor_tunnels))
        .route("/api/ws", get(monitor_ws))
        .with_state(state)
}

async fn dashboard_page() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn monitor_stats(State(state): State<AppState>) -> Json<AllStatsResponse> {
    Json(gather_all_stats(&state).await)
}

async fn monitor_thumbnail(
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
) -> impl IntoResponse {
    let stats = state.flow_manager.stats();
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
    State(state): State<AppState>,
    Path((flow_id, input_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let stats = state.flow_manager.stats();
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

async fn monitor_tunnels(State(state): State<AppState>) -> impl IntoResponse {
    let tunnels = state.tunnel_manager.list_tunnels();
    Json(tunnels)
}

async fn monitor_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| monitor_ws_connection(socket, state))
}

async fn monitor_ws_connection(mut socket: WebSocket, state: AppState) {
    let mut rx = state.ws_stats_tx.subscribe();

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
    shutdown_token: CancellationToken,
) -> anyhow::Result<JoinHandle<()>> {
    let addr = format!("{}:{}", config.listen_addr, config.listen_port);
    let listener = TcpListener::bind(&addr).await.map_err(|e| {
        crate::util::port_error::annotate_bind_error(e, &addr, "Monitor dashboard server")
    })?;
    tracing::info!("Monitor dashboard listening on {addr}");

    let router = build_monitor_router(state);

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
