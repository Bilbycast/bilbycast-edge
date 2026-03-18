//! Manager WebSocket client.
//!
//! Maintains a persistent outbound WebSocket connection to the manager,
//! forwarding stats and events, and executing commands received from the manager.
//!
//! Authentication is done via an "auth" message sent as the first WebSocket frame
//! after connecting (not via query parameters, to avoid leaking secrets in URLs/logs).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, RwLock};
use tokio_tungstenite::tungstenite::Message;

use crate::config::models::{AppConfig, FlowConfig};
use crate::engine::manager::FlowManager;

use super::ManagerConfig;

/// Start the manager client background task.
pub fn start_manager_client(
    config: ManagerConfig,
    flow_manager: Arc<FlowManager>,
    ws_stats_rx: broadcast::Sender<String>,
    app_config: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
    api_addr: String,
    monitor_addr: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        manager_client_loop(config, flow_manager, ws_stats_rx, app_config, config_path, api_addr, monitor_addr).await;
    })
}

async fn manager_client_loop(
    mut config: ManagerConfig,
    flow_manager: Arc<FlowManager>,
    ws_stats_tx: broadcast::Sender<String>,
    app_config: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
    api_addr: String,
    monitor_addr: Option<String>,
) {
    let mut backoff_secs = 1u64;
    let max_backoff = 60u64;

    loop {
        tracing::info!("Connecting to manager at {}", config.url);

        match try_connect(
            &config,
            &flow_manager,
            &ws_stats_tx,
            &app_config,
            &config_path,
            &api_addr,
            monitor_addr.as_deref(),
        )
        .await
        {
            Ok(ConnectResult::Closed) => {
                tracing::info!("Manager connection closed normally");
                backoff_secs = 1;
            }
            Ok(ConnectResult::Registered {
                node_id,
                node_secret,
            }) => {
                tracing::info!(
                    "Registered with manager as node_id={node_id}, persisting credentials"
                );
                config.registration_token = None;
                config.node_id = Some(node_id.clone());
                config.node_secret = Some(node_secret.clone());
                backoff_secs = 1;

                // Persist to config file
                persist_credentials(&app_config, &config_path, &node_id, &node_secret).await;
            }
            Err(e) => {
                tracing::warn!("Manager connection failed: {e}");
            }
        }

        tracing::info!("Reconnecting to manager in {backoff_secs}s...");
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(max_backoff);
    }
}

enum ConnectResult {
    Closed,
    Registered {
        node_id: String,
        node_secret: String,
    },
}

async fn try_connect(
    config: &ManagerConfig,
    flow_manager: &Arc<FlowManager>,
    ws_stats_tx: &broadcast::Sender<String>,
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
    api_addr: &str,
    monitor_addr: Option<&str>,
) -> Result<ConnectResult, String> {
    // Connect to the bare WebSocket URL (no credentials in query params)
    let connect_url = &config.url;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(connect_url)
        .await
        .map_err(|e| format!("WebSocket connect failed: {e}"))?;

    tracing::info!("WebSocket connected, sending auth...");

    let (mut ws_write, mut ws_read) = ws_stream.split();

    // Step 1: Send auth message as first frame
    let auth_msg = build_auth_message(config);
    if let Ok(json) = serde_json::to_string(&auth_msg) {
        ws_write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| format!("Failed to send auth: {e}"))?;
    }

    // Step 2: Wait for auth response (auth_ok, register_ack, or auth_error)
    let auth_timeout =
        tokio::time::timeout(Duration::from_secs(10), ws_read.next()).await;

    let mut registered_creds: Option<(String, String)> = None;

    match auth_timeout {
        Ok(Some(Ok(Message::Text(text)))) => {
            let response: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("Invalid auth response: {e}"))?;

            match response["type"].as_str().unwrap_or("") {
                "auth_ok" => {
                    tracing::info!("Authenticated with manager");
                }
                "register_ack" => {
                    let payload = &response["payload"];
                    let node_id = payload["node_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let node_secret = payload["node_secret"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    tracing::info!("Registered with manager: node_id={node_id}");

                    if !node_id.is_empty() && !node_secret.is_empty() {
                        // Persist immediately
                        persist_credentials(app_config, config_path, &node_id, &node_secret)
                            .await;
                        registered_creds = Some((node_id, node_secret));
                    }
                }
                "auth_error" => {
                    let msg = response["message"]
                        .as_str()
                        .unwrap_or("Unknown auth error");
                    return Err(format!("Auth rejected: {msg}"));
                }
                other => {
                    return Err(format!("Unexpected auth response type: {other}"));
                }
            }
        }
        Ok(Some(Ok(_))) => return Err("Unexpected non-text auth response".into()),
        Ok(Some(Err(e))) => return Err(format!("WebSocket error during auth: {e}")),
        Ok(None) => return Err("Connection closed during auth".into()),
        Err(_) => return Err("Auth response timeout (10s)".into()),
    }

    // Now in the authenticated session — run the main loop
    let mut stats_rx = ws_stats_tx.subscribe();

    // Send initial health
    let health = build_health_message(flow_manager, api_addr, monitor_addr);
    if let Ok(json) = serde_json::to_string(&health) {
        let _ = ws_write.send(Message::Text(json.into())).await;
    }

    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));

    loop {
        tokio::select! {
            stats_result = stats_rx.recv() => {
                match stats_result {
                    Ok(stats_json) => {
                        let envelope = serde_json::json!({
                            "type": "stats",
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "payload": {
                                "flows": serde_json::from_str::<serde_json::Value>(&stats_json).unwrap_or_default(),
                                "uptime_secs": 0,
                                "active_flows": flow_manager.active_flow_count(),
                                "total_flows": flow_manager.active_flow_count()
                            }
                        });
                        if let Ok(json) = serde_json::to_string(&envelope) {
                            if ws_write.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("Manager client lagged {n} stats messages");
                    }
                    Err(_) => break,
                }
            }

            msg = ws_read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_manager_message(&text, flow_manager, &mut ws_write).await;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        return Err(format!("WebSocket error: {e}"));
                    }
                    _ => {}
                }
            }

            _ = ping_interval.tick() => {
                let pong = serde_json::json!({
                    "type": "health",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": build_health_payload(flow_manager, api_addr, monitor_addr)
                });
                if let Ok(json) = serde_json::to_string(&pong) {
                    if ws_write.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    if let Some((node_id, node_secret)) = registered_creds {
        return Ok(ConnectResult::Registered {
            node_id,
            node_secret,
        });
    }

    Ok(ConnectResult::Closed)
}

/// Build the auth message sent as the first WebSocket frame.
/// Contains either registration_token OR node_id + node_secret.
fn build_auth_message(config: &ManagerConfig) -> serde_json::Value {
    if let (Some(node_id), Some(node_secret)) = (&config.node_id, &config.node_secret) {
        serde_json::json!({
            "type": "auth",
            "payload": {
                "node_id": node_id,
                "node_secret": node_secret
            }
        })
    } else if let Some(token) = &config.registration_token {
        serde_json::json!({
            "type": "auth",
            "payload": {
                "registration_token": token
            }
        })
    } else {
        serde_json::json!({
            "type": "auth",
            "payload": {}
        })
    }
}

fn build_health_message(flow_manager: &FlowManager, api_addr: &str, monitor_addr: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "type": "health",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": build_health_payload(flow_manager, api_addr, monitor_addr)
    })
}

fn build_health_payload(flow_manager: &FlowManager, api_addr: &str, monitor_addr: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": 0,
        "active_flows": flow_manager.active_flow_count(),
        "total_flows": flow_manager.active_flow_count(),
        "api_addr": api_addr,
        "monitor_addr": monitor_addr
    })
}

/// Persist manager credentials to the config file.
async fn persist_credentials(
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
    node_id: &str,
    node_secret: &str,
) {
    let mut cfg = app_config.write().await;
    if let Some(ref mut mgr) = cfg.manager {
        mgr.registration_token = None;
        mgr.node_id = Some(node_id.to_string());
        mgr.node_secret = Some(node_secret.to_string());
    }
    if let Ok(json) = serde_json::to_string_pretty(&*cfg) {
        if let Err(e) = std::fs::write(config_path, &json) {
            tracing::warn!("Failed to persist manager credentials: {e}");
        } else {
            tracing::info!(
                "Manager credentials saved to {}",
                config_path.display()
            );
        }
    }
}

/// Handle a message from the manager.
async fn handle_manager_message<S>(
    text: &str,
    flow_manager: &Arc<FlowManager>,
    ws_write: &mut futures_util::stream::SplitSink<S, Message>,
) where
    S: futures_util::Sink<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let envelope: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Invalid message from manager: {e}");
            return;
        }
    };

    let msg_type = envelope["type"].as_str().unwrap_or("");
    let payload = &envelope["payload"];

    match msg_type {
        "ping" => {
            let pong = serde_json::json!({
                "type": "pong",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "payload": null
            });
            if let Ok(json) = serde_json::to_string(&pong) {
                let _ = ws_write.send(Message::Text(json.into())).await;
            }
        }
        "command" => {
            let command_id = payload["command_id"].as_str().unwrap_or("unknown");
            let action = &payload["action"];
            let action_type = action["type"].as_str().unwrap_or("");

            let result = execute_command(action_type, action, flow_manager).await;

            let ack = serde_json::json!({
                "type": "command_ack",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "payload": {
                    "command_id": command_id,
                    "success": result.is_ok(),
                    "error": result.err()
                }
            });
            if let Ok(json) = serde_json::to_string(&ack) {
                let _ = ws_write.send(Message::Text(json.into())).await;
            }
        }
        "register_ack" => {
            // Handled during initial auth handshake; ignore if received later
            tracing::debug!("Late register_ack received, ignoring");
        }
        _ => {
            tracing::debug!("Unknown message type from manager: {msg_type}");
        }
    }
}

async fn execute_command(
    action_type: &str,
    action: &serde_json::Value,
    flow_manager: &Arc<FlowManager>,
) -> Result<(), String> {
    match action_type {
        "create_flow" => {
            let flow: FlowConfig = serde_json::from_value(action["flow"].clone())
                .map_err(|e| format!("Invalid flow config: {e}"))?;
            tracing::info!("Manager command: create flow '{}'", flow.id);
            flow_manager
                .create_flow(flow)
                .await
                .map_err(|e| e.to_string())
        }
        "delete_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: delete flow '{flow_id}'");
            flow_manager
                .destroy_flow(flow_id)
                .await
                .map_err(|e| e.to_string())
        }
        "stop_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: stop flow '{flow_id}'");
            flow_manager
                .stop_flow(flow_id)
                .await
                .map_err(|e| e.to_string())
        }
        "start_flow" | "restart_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: {action_type} flow '{flow_id}'");
            Ok(())
        }
        "add_output" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let output: crate::config::models::OutputConfig =
                serde_json::from_value(action["output"].clone())
                    .map_err(|e| format!("Invalid output config: {e}"))?;
            tracing::info!("Manager command: add output to flow '{flow_id}'");
            flow_manager
                .add_output(flow_id, output)
                .await
                .map_err(|e| e.to_string())
        }
        "remove_output" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let output_id = action["output_id"].as_str().ok_or("Missing output_id")?;
            tracing::info!("Manager command: remove output '{output_id}' from flow '{flow_id}'");
            flow_manager
                .remove_output(flow_id, output_id)
                .await
                .map_err(|e| e.to_string())
        }
        _ => Err(format!("Unknown command: {action_type}")),
    }
}
