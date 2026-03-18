//! Manager WebSocket client.
//!
//! Maintains a persistent outbound WebSocket connection to the manager,
//! forwarding stats and events, and executing commands received from the manager.

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
/// `app_config` and `config_path` are used to persist node credentials to disk after registration.
pub fn start_manager_client(
    config: ManagerConfig,
    flow_manager: Arc<FlowManager>,
    ws_stats_rx: broadcast::Sender<String>,
    app_config: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        manager_client_loop(config, flow_manager, ws_stats_rx, app_config, config_path).await;
    })
}

async fn manager_client_loop(
    mut config: ManagerConfig,
    flow_manager: Arc<FlowManager>,
    ws_stats_tx: broadcast::Sender<String>,
    app_config: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
) {
    let mut backoff_secs = 1u64;
    let max_backoff = 60u64;

    loop {
        tracing::info!("Connecting to manager at {}", config.url);

        match try_connect(&config, &flow_manager, &ws_stats_tx, &app_config, &config_path).await {
            Ok(ConnectResult::Closed) => {
                tracing::info!("Manager connection closed normally");
                backoff_secs = 1;
            }
            Ok(ConnectResult::Registered { node_id, node_secret }) => {
                // Save credentials for reconnection (in memory)
                tracing::info!("Registered with manager as node_id={node_id}, persisting credentials to config");
                config.registration_token = None;
                config.node_id = Some(node_id.clone());
                config.node_secret = Some(node_secret.clone());
                backoff_secs = 1;

                // Persist to config file so reconnection works after restart
                {
                    let mut cfg = app_config.write().await;
                    if let Some(ref mut mgr) = cfg.manager {
                        mgr.registration_token = None;
                        mgr.node_id = Some(node_id);
                        mgr.node_secret = Some(node_secret);
                    }
                    if let Ok(json) = serde_json::to_string_pretty(&*cfg) {
                        if let Err(e) = std::fs::write(&config_path, &json) {
                            tracing::warn!("Failed to persist manager credentials to config: {e}");
                        } else {
                            tracing::info!("Manager credentials saved to {}", config_path.display());
                        }
                    }
                }
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
    Registered { node_id: String, node_secret: String },
}

async fn try_connect(
    config: &ManagerConfig,
    flow_manager: &Arc<FlowManager>,
    ws_stats_tx: &broadcast::Sender<String>,
    app_config: &Arc<RwLock<AppConfig>>,
    config_path: &PathBuf,
) -> Result<ConnectResult, String> {
    let connect_url = build_connect_url(config)?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(&connect_url)
        .await
        .map_err(|e| format!("WebSocket connect failed: {e}"))?;

    tracing::info!("Connected to manager");

    let (mut ws_write, mut ws_read) = ws_stream.split();

    let mut stats_rx = ws_stats_tx.subscribe();

    // Send initial health
    let health = build_health_message(flow_manager);
    if let Ok(json) = serde_json::to_string(&health) {
        let _ = ws_write.send(Message::Text(json.into())).await;
    }

    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));
    let registered = Arc::new(RwLock::new(None::<(String, String)>));

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
                        if let Some((node_id, node_secret)) = handle_manager_message(&text, flow_manager, &mut ws_write).await {
                            *registered.write().await = Some((node_id.clone(), node_secret.clone()));
                            // Persist immediately so reconnection works even if process restarts
                            let mut cfg = app_config.write().await;
                            if let Some(ref mut mgr) = cfg.manager {
                                mgr.registration_token = None;
                                mgr.node_id = Some(node_id);
                                mgr.node_secret = Some(node_secret);
                            }
                            if let Ok(json) = serde_json::to_string_pretty(&*cfg) {
                                if let Err(e) = std::fs::write(config_path, &json) {
                                    tracing::warn!("Failed to persist credentials: {e}");
                                } else {
                                    tracing::info!("Manager credentials persisted to {}", config_path.display());
                                }
                            }
                        }
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
                    "payload": build_health_payload(flow_manager)
                });
                if let Ok(json) = serde_json::to_string(&pong) {
                    if ws_write.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    // Check if we received registration credentials during this session
    let creds = registered.read().await.clone();
    if let Some((node_id, node_secret)) = creds {
        return Ok(ConnectResult::Registered { node_id, node_secret });
    }

    Ok(ConnectResult::Closed)
}

fn build_connect_url(config: &ManagerConfig) -> Result<String, String> {
    // Prefer node_id/node_secret (reconnection) over registration_token (first time)
    if let (Some(node_id), Some(node_secret)) = (&config.node_id, &config.node_secret) {
        Ok(format!(
            "{}?node_id={}&node_secret={}",
            config.url, node_id, node_secret
        ))
    } else if let Some(token) = &config.registration_token {
        Ok(format!("{}?token={}", config.url, token))
    } else {
        Err("No registration_token or node_id/node_secret configured".into())
    }
}

fn build_health_message(flow_manager: &FlowManager) -> serde_json::Value {
    serde_json::json!({
        "type": "health",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": build_health_payload(flow_manager)
    })
}

fn build_health_payload(flow_manager: &FlowManager) -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": 0,
        "active_flows": flow_manager.active_flow_count(),
        "total_flows": flow_manager.active_flow_count()
    })
}

/// Handle a message from the manager. Returns Some((node_id, node_secret)) if registration ack received.
async fn handle_manager_message<S>(
    text: &str,
    flow_manager: &Arc<FlowManager>,
    ws_write: &mut futures_util::stream::SplitSink<S, Message>,
) -> Option<(String, String)>
where
    S: futures_util::Sink<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let envelope: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Invalid message from manager: {e}");
            return None;
        }
    };

    let msg_type = envelope["type"].as_str().unwrap_or("");
    let payload = &envelope["payload"];

    match msg_type {
        "ping" => {
            let pong = serde_json::json!({"type": "pong", "timestamp": chrono::Utc::now().to_rfc3339(), "payload": null});
            if let Ok(json) = serde_json::to_string(&pong) {
                let _ = ws_write.send(Message::Text(json.into())).await;
            }
            None
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
            None
        }
        "register_ack" => {
            let node_id = payload["node_id"].as_str().unwrap_or("").to_string();
            let node_secret = payload["node_secret"].as_str().unwrap_or("").to_string();
            tracing::info!("Registered with manager: node_id={node_id}");
            if !node_id.is_empty() && !node_secret.is_empty() {
                Some((node_id, node_secret))
            } else {
                None
            }
        }
        _ => {
            tracing::debug!("Unknown message type from manager: {msg_type}");
            None
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
            flow_manager.create_flow(flow).await.map_err(|e| e.to_string())
        }
        "delete_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: delete flow '{flow_id}'");
            flow_manager.destroy_flow(flow_id).await.map_err(|e| e.to_string())
        }
        "stop_flow" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            tracing::info!("Manager command: stop flow '{flow_id}'");
            flow_manager.stop_flow(flow_id).await.map_err(|e| e.to_string())
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
            flow_manager.add_output(flow_id, output).await.map_err(|e| e.to_string())
        }
        "remove_output" => {
            let flow_id = action["flow_id"].as_str().ok_or("Missing flow_id")?;
            let output_id = action["output_id"].as_str().ok_or("Missing output_id")?;
            tracing::info!("Manager command: remove output '{output_id}' from flow '{flow_id}'");
            flow_manager.remove_output(flow_id, output_id).await.map_err(|e| e.to_string())
        }
        _ => Err(format!("Unknown command: {action_type}")),
    }
}
