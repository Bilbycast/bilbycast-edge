// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Standby listener manager for passive-type inputs.
//!
//! When a listener-type input (SRT listener, RTP/UDP receiver, RTMP server, etc.)
//! is not assigned to a running flow, this module keeps its socket bound so the
//! user can see basic connection status (e.g., "listening", "bound", "connected")
//! without needing a full flow.
//!
//! The standby listener does NOT process media — it only monitors the socket for
//! connection state. When the input is assigned to a flow, the standby listener
//! is stopped and the flow takes over the socket.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{InputConfig, InputDefinition, SrtMode};
use crate::manager::events::EventSender;

/// Status of a standby (unassigned) listener-type input.
#[derive(Debug, Clone, Serialize)]
pub struct StandbyInputStatus {
    pub input_id: String,
    pub input_name: String,
    pub input_type: String,
    /// Connection state: "listening", "bound", "connected", "bind_failed", "error".
    pub state: String,
    /// Number of active connections (SRT listener count, TCP accept count).
    pub connections: u32,
    /// Error message if bind/listen failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Local address being listened on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
}

/// Manages standby listeners for passive-type inputs that are not assigned
/// to running flows.
pub struct StandbyListenerManager {
    listeners: DashMap<String, StandbyEntry>,
    event_sender: EventSender,
}

struct StandbyEntry {
    status: Arc<StandbyStatus>,
    cancel: CancellationToken,
    _handle: JoinHandle<()>,
}

/// Thread-safe status storage for a single standby listener.
struct StandbyStatus {
    input_id: String,
    input_name: String,
    input_type: String,
    state: RwLock<String>,
    connections: AtomicU32,
    error: RwLock<Option<String>>,
    local_addr: RwLock<Option<String>>,
}

impl StandbyStatus {
    fn snapshot(&self) -> StandbyInputStatus {
        StandbyInputStatus {
            input_id: self.input_id.clone(),
            input_name: self.input_name.clone(),
            input_type: self.input_type.clone(),
            state: self.state.read().unwrap().clone(),
            connections: self.connections.load(Ordering::Relaxed),
            error: self.error.read().unwrap().clone(),
            local_addr: self.local_addr.read().unwrap().clone(),
        }
    }
}

impl StandbyListenerManager {
    pub fn new(event_sender: EventSender) -> Self {
        Self {
            listeners: DashMap::new(),
            event_sender,
        }
    }

    /// Start a standby listener for an unassigned passive-type input.
    /// No-op if the input is not a passive listener type.
    pub fn start_standby(&self, input_def: &InputDefinition) {
        if !is_passive_listener(&input_def.config) {
            return;
        }
        // Don't restart if already running
        if self.listeners.contains_key(&input_def.id) {
            return;
        }

        let status = Arc::new(StandbyStatus {
            input_id: input_def.id.clone(),
            input_name: input_def.name.clone(),
            input_type: input_def.config.type_name().to_string(),
            state: RwLock::new("starting".to_string()),
            connections: AtomicU32::new(0),
            error: RwLock::new(None),
            local_addr: RwLock::new(None),
        });

        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_standby_listener(
            input_def.config.clone(),
            status.clone(),
            cancel.clone(),
            self.event_sender.clone(),
            input_def.id.clone(),
        ));

        self.listeners.insert(
            input_def.id.clone(),
            StandbyEntry {
                status,
                cancel,
                _handle: handle,
            },
        );
    }

    /// Stop and remove a standby listener (e.g., when the input is assigned to a flow).
    pub fn stop_standby(&self, input_id: &str) {
        if let Some((_, entry)) = self.listeners.remove(input_id) {
            entry.cancel.cancel();
        }
    }

    /// Get status of a specific standby listener.
    pub fn get_status(&self, input_id: &str) -> Option<StandbyInputStatus> {
        self.listeners.get(input_id).map(|e| e.status.snapshot())
    }

    /// Snapshot all active standby listener statuses.
    pub fn snapshot(&self) -> Vec<StandbyInputStatus> {
        self.listeners
            .iter()
            .map(|e| e.value().status.snapshot())
            .collect()
    }

    /// Synchronize standby listeners with the current config.
    /// Starts standby for unassigned passive listeners, stops for assigned ones.
    pub fn sync(&self, inputs: &[InputDefinition], assigned_input_ids: &[&str]) {
        // Stop standby for inputs that are now assigned to a flow
        for id in assigned_input_ids {
            self.stop_standby(id);
        }

        // Start standby for unassigned passive listener inputs
        for input_def in inputs {
            if !assigned_input_ids.contains(&input_def.id.as_str()) {
                self.start_standby(input_def);
            }
        }

        // Remove standby for inputs that no longer exist in config
        let valid_ids: std::collections::HashSet<&str> =
            inputs.iter().map(|i| i.id.as_str()).collect();
        let stale: Vec<String> = self
            .listeners
            .iter()
            .filter(|e| !valid_ids.contains(e.key().as_str()))
            .map(|e| e.key().clone())
            .collect();
        for id in stale {
            self.stop_standby(&id);
        }
    }
}

/// Returns true if the input is a listener/receiver type that can meaningfully
/// maintain a standby socket (as opposed to callers/pullers that initiate connections).
fn is_passive_listener(config: &InputConfig) -> bool {
    match config {
        InputConfig::Srt(c) => c.mode == SrtMode::Listener,
        InputConfig::Rtp(_) => true,
        InputConfig::Udp(_) => true,
        InputConfig::Rtmp(_) => true,
        InputConfig::St2110_30(_) => true,
        InputConfig::St2110_31(_) => true,
        InputConfig::St2110_40(_) => true,
        InputConfig::RtpAudio(_) => true,
        // Callers/pullers and WebRTC (needs full HTTP/ICE stack) are NOT passive
        InputConfig::Webrtc(_) | InputConfig::Rtsp(_) | InputConfig::Whep(_) => false,
    }
}

/// Background task that binds a socket for a passive listener input
/// and monitors its connection state.
async fn run_standby_listener(
    config: InputConfig,
    status: Arc<StandbyStatus>,
    cancel: CancellationToken,
    event_sender: EventSender,
    input_id: String,
) {
    match config {
        InputConfig::Rtp(ref rtp) => {
            run_udp_standby(&rtp.bind_addr, &status, &cancel).await;
        }
        InputConfig::Udp(ref udp) => {
            run_udp_standby(&udp.bind_addr, &status, &cancel).await;
        }
        InputConfig::St2110_30(ref st) | InputConfig::St2110_31(ref st) => {
            run_udp_standby(&st.bind_addr, &status, &cancel).await;
        }
        InputConfig::St2110_40(ref anc) => {
            run_udp_standby(&anc.bind_addr, &status, &cancel).await;
        }
        InputConfig::RtpAudio(ref rtp_audio) => {
            run_udp_standby(&rtp_audio.bind_addr, &status, &cancel).await;
        }
        InputConfig::Rtmp(ref rtmp) => {
            run_tcp_standby(&rtmp.listen_addr, &status, &cancel).await;
        }
        InputConfig::Srt(ref srt) if srt.mode == SrtMode::Listener => {
            let addr = srt.local_addr.as_deref().unwrap_or("0.0.0.0:0");
            run_udp_standby(addr, &status, &cancel).await;
        }
        _ => {
            *status.state.write().unwrap() = "unsupported".to_string();
        }
    }

    // Emit event if the standby listener ended with an error
    let final_state = status.state.read().unwrap().clone();
    if final_state == "bind_failed" || final_state == "error" {
        let err_msg = status.error.read().unwrap().clone().unwrap_or_default();
        event_sender.emit_input(
            crate::manager::events::EventSeverity::Warning,
            "standby",
            format!("Standby listener for input '{input_id}' failed: {err_msg}"),
            &input_id,
        );
    }
}

/// UDP standby: bind the socket and wait, reporting if any data arrives.
async fn run_udp_standby(
    bind_addr: &str,
    status: &Arc<StandbyStatus>,
    cancel: &CancellationToken,
) {
    let addr: SocketAddr = match bind_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            *status.state.write().unwrap() = "bind_failed".to_string();
            *status.error.write().unwrap() = Some(format!("Invalid address: {e}"));
            return;
        }
    };

    let socket = match tokio::net::UdpSocket::bind(addr).await {
        Ok(s) => {
            let local = s.local_addr().map(|a| a.to_string()).ok();
            *status.state.write().unwrap() = "bound".to_string();
            *status.local_addr.write().unwrap() = local;
            s
        }
        Err(e) => {
            let msg = if crate::util::port_error::is_addr_in_use(&e) {
                format!(
                    "Port conflict: could not bind to {bind_addr} \
                     — address already in use"
                )
            } else {
                format!("{e}")
            };
            *status.state.write().unwrap() = "bind_failed".to_string();
            *status.error.write().unwrap() = Some(msg);
            return;
        }
    };

    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = socket.recv(&mut buf) => {
                match result {
                    Ok(n) if n > 0 => {
                        *status.state.write().unwrap() = "receiving".to_string();
                        // Keep receiving to detect when data stops
                        // (but don't actually process it)
                    }
                    Ok(_) => {}
                    Err(_) => {
                        *status.state.write().unwrap() = "error".to_string();
                        break;
                    }
                }
            }
        }
    }
}

/// TCP standby: listen for connections and track count.
async fn run_tcp_standby(
    listen_addr: &str,
    status: &Arc<StandbyStatus>,
    cancel: &CancellationToken,
) {
    let addr: SocketAddr = match listen_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            *status.state.write().unwrap() = "bind_failed".to_string();
            *status.error.write().unwrap() = Some(format!("Invalid address: {e}"));
            return;
        }
    };

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => {
            let local = l.local_addr().map(|a| a.to_string()).ok();
            *status.state.write().unwrap() = "listening".to_string();
            *status.local_addr.write().unwrap() = local;
            l
        }
        Err(e) => {
            let msg = if crate::util::port_error::is_addr_in_use(&e) {
                format!(
                    "Port conflict: could not bind to {listen_addr} \
                     — address already in use"
                )
            } else {
                format!("{e}")
            };
            *status.state.write().unwrap() = "bind_failed".to_string();
            *status.error.write().unwrap() = Some(msg);
            return;
        }
    };

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = listener.accept() => {
                match result {
                    Ok((_stream, _peer)) => {
                        status.connections.fetch_add(1, Ordering::Relaxed);
                        *status.state.write().unwrap() = "connected".to_string();
                        // We just accept and immediately drop the connection
                        // since we're not processing media in standby mode
                    }
                    Err(_) => {
                        *status.state.write().unwrap() = "error".to_string();
                        break;
                    }
                }
            }
        }
    }
}
