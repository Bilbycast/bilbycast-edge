// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;

use super::server::AppState;

/// GET /api/v1/ws/stats - WebSocket upgrade handler
pub async fn ws_stats_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws_connection(socket, state))
}

/// Handles an individual WebSocket connection for real-time stats streaming.
///
/// Subscribes to the [`AppState::ws_stats_tx`] broadcast channel and forwards every
/// JSON stats message to the connected WebSocket client. The function runs in a loop
/// using `tokio::select!` to concurrently:
///
/// 1. **Receive from broadcast channel**: forwards each stats message as a WebSocket
///    text frame. If the client falls behind (broadcast channel lag), skipped messages
///    are logged as a warning and the connection continues with the next available message.
/// 2. **Receive from WebSocket**: listens for close frames or errors from the client
///    to detect disconnection. All other client messages are ignored since this is a
///    server-push-only channel.
///
/// The loop exits (and the connection is closed) when:
/// - The client sends a `Close` frame or disconnects.
/// - Sending a message to the client fails (broken pipe).
/// - The broadcast channel is closed (all senders dropped).
async fn handle_ws_connection(mut socket: WebSocket, state: AppState) {
    let mut rx = state.ws_stats_tx.subscribe();

    tracing::info!("WebSocket client connected for stats");

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(msg) => {
                        if socket.send(Message::Text(msg.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket client lagged, skipped {n} messages");
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // ignore other messages from client
                }
            }
        }
    }

    tracing::info!("WebSocket client disconnected");
}
