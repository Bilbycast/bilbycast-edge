// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! WHIP/WHEP HTTP endpoint handlers for WebRTC signaling.
//!
//! Provides Axum route handlers for:
//! - `POST /api/v1/flows/{flow_id}/whip` — Accept WHIP publisher (SDP offer → answer)
//! - `DELETE /api/v1/flows/{flow_id}/whip/{session_id}` — Disconnect WHIP publisher
//! - `POST /api/v1/flows/{flow_id}/whep` — Accept WHEP viewer (SDP offer → answer)
//! - `DELETE /api/v1/flows/{flow_id}/whep/{session_id}` — Disconnect WHEP viewer
//!
//! All endpoints follow RFC 9725 (WHIP) and draft-ietf-wish-whep (WHEP)
//! signaling conventions: Content-Type: application/sdp, 201 Created with
//! Location header, Bearer token auth.

#[cfg(feature = "webrtc")]
pub mod handlers {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::{IntoResponse, Response};

    use crate::api::server::AppState;

    /// POST /api/v1/flows/{flow_id}/whip — WHIP ingest endpoint.
    ///
    /// Accepts an SDP offer from a WHIP publisher (OBS, browser, etc.),
    /// creates a WebRTC session, and returns the SDP answer.
    pub async fn whip_offer(
        State(state): State<AppState>,
        Path(flow_id): Path<String>,
        headers: HeaderMap,
        body: String,
    ) -> Result<Response, StatusCode> {
        // Validate flow exists and is running
        if !state.flow_manager.is_running(&flow_id) {
            return Err(StatusCode::NOT_FOUND);
        }

        // Validate Content-Type
        if let Some(ct) = headers.get(header::CONTENT_TYPE) {
            if ct.to_str().unwrap_or("") != "application/sdp" {
                return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
            }
        }

        // Validate Bearer token if configured
        if let Some(ref registry) = state.webrtc_sessions {
            if let Some(ref expected_token) = registry.whip_bearer_token(&flow_id) {
                let provided = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                match provided {
                    Some(token) if token == expected_token.as_str() => {}
                    _ => return Err(StatusCode::UNAUTHORIZED),
                }
            }
        }

        // Create WebRTC session and process SDP offer
        let registry = state.webrtc_sessions.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let (answer_sdp, session_id) = registry
            .handle_whip_offer(&flow_id, &body)
            .await
            .map_err(|e| {
                tracing::error!("WHIP offer error for flow '{}': {}", flow_id, e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        let location = format!("/api/v1/flows/{}/whip/{}", flow_id, session_id);

        Ok(Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_TYPE, "application/sdp")
            .header(header::LOCATION, location)
            .body(Body::from(answer_sdp))
            .unwrap())
    }

    /// DELETE /api/v1/flows/{flow_id}/whip/{session_id} — Teardown WHIP session.
    pub async fn whip_delete(
        State(state): State<AppState>,
        Path((flow_id, session_id)): Path<(String, String)>,
    ) -> StatusCode {
        if let Some(ref registry) = state.webrtc_sessions {
            registry.remove_session(&flow_id, &session_id);
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        }
    }

    /// POST /api/v1/flows/{flow_id}/whep — WHEP playback endpoint.
    ///
    /// Accepts an SDP offer from a WHEP viewer (browser), creates a
    /// WebRTC session subscribed to the flow, and returns the SDP answer.
    pub async fn whep_offer(
        State(state): State<AppState>,
        Path(flow_id): Path<String>,
        headers: HeaderMap,
        body: String,
    ) -> Result<Response, StatusCode> {
        if !state.flow_manager.is_running(&flow_id) {
            return Err(StatusCode::NOT_FOUND);
        }

        if let Some(ct) = headers.get(header::CONTENT_TYPE) {
            if ct.to_str().unwrap_or("") != "application/sdp" {
                return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
            }
        }

        // Validate Bearer token if configured
        if let Some(ref registry) = state.webrtc_sessions {
            if let Some(ref expected_token) = registry.whep_bearer_token(&flow_id) {
                let provided = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                match provided {
                    Some(token) if token == expected_token.as_str() => {}
                    _ => return Err(StatusCode::UNAUTHORIZED),
                }
            }
        }

        let registry = state.webrtc_sessions.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let (answer_sdp, session_id) = registry
            .handle_whep_offer(&flow_id, &body)
            .await
            .map_err(|e| {
                tracing::error!("WHEP offer error for flow '{}': {}", flow_id, e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        let location = format!("/api/v1/flows/{}/whep/{}", flow_id, session_id);

        Ok(Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_TYPE, "application/sdp")
            .header(header::LOCATION, location)
            .body(Body::from(answer_sdp))
            .unwrap())
    }

    /// DELETE /api/v1/flows/{flow_id}/whep/{session_id} — Teardown WHEP session.
    pub async fn whep_delete(
        State(state): State<AppState>,
        Path((flow_id, session_id)): Path<(String, String)>,
    ) -> StatusCode {
        if let Some(ref registry) = state.webrtc_sessions {
            registry.remove_session(&flow_id, &session_id);
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        }
    }
}

// ── Session Registry ─────────────────────────────────────────────────────

#[cfg(feature = "webrtc")]
pub mod registry {
    use std::sync::Arc;

    use anyhow::Result;
    use dashmap::DashMap;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Handle to a single WebRTC session (WHIP or WHEP).
    pub struct WebrtcSessionHandle {
        /// Cancellation token to stop this session's task.
        pub cancel: CancellationToken,
        /// Session type (whip or whep).
        pub session_type: &'static str,
    }

    /// Message sent from API handler to input/output task when a new session is created.
    pub struct NewSessionMsg {
        /// The SDP offer from the remote peer.
        pub offer_sdp: String,
        /// Channel to send back the SDP answer + session ID.
        pub reply: tokio::sync::oneshot::Sender<Result<(String, String)>>,
    }

    /// Registry of active WebRTC sessions, keyed by (flow_id, session_id).
    ///
    /// Stored in `AppState` and shared between API handlers and engine tasks.
    pub struct WebrtcSessionRegistry {
        /// Active sessions: key = "flow_id/session_id".
        sessions: DashMap<String, WebrtcSessionHandle>,
        /// Channels for WHIP input: API handler → input task.
        /// Key = flow_id. Input tasks register their sender here on startup.
        whip_input_channels: DashMap<String, mpsc::Sender<NewSessionMsg>>,
        /// Channels for WHEP output: API handler → output task.
        /// Key = flow_id. Output tasks register their sender here on startup.
        whep_output_channels: DashMap<String, mpsc::Sender<NewSessionMsg>>,
        /// Bearer tokens for WHIP inputs (flow_id → token).
        whip_tokens: DashMap<String, String>,
        /// Bearer tokens for WHEP outputs (flow_id → token).
        whep_tokens: DashMap<String, String>,
    }

    impl WebrtcSessionRegistry {
        pub fn new() -> Self {
            Self {
                sessions: DashMap::new(),
                whip_input_channels: DashMap::new(),
                whep_output_channels: DashMap::new(),
                whip_tokens: DashMap::new(),
                whep_tokens: DashMap::new(),
            }
        }

        /// Register a WHIP input channel for a flow.
        /// Called by `spawn_webrtc_input` when the flow starts.
        pub fn register_whip_input(&self, flow_id: &str, tx: mpsc::Sender<NewSessionMsg>, bearer_token: Option<String>) {
            self.whip_input_channels.insert(flow_id.to_string(), tx);
            if let Some(token) = bearer_token {
                self.whip_tokens.insert(flow_id.to_string(), token);
            }
        }

        /// Register a WHEP output channel for a flow.
        /// Called by `spawn_webrtc_output` (WHEP server mode) when the output starts.
        pub fn register_whep_output(&self, flow_id: &str, tx: mpsc::Sender<NewSessionMsg>, bearer_token: Option<String>) {
            self.whep_output_channels.insert(flow_id.to_string(), tx);
            if let Some(token) = bearer_token {
                self.whep_tokens.insert(flow_id.to_string(), token);
            }
        }

        /// Unregister channels for a flow (called on flow stop).
        pub fn unregister_flow(&self, flow_id: &str) {
            self.whip_input_channels.remove(flow_id);
            self.whep_output_channels.remove(flow_id);
            self.whip_tokens.remove(flow_id);
            self.whep_tokens.remove(flow_id);
            // Remove all sessions for this flow
            let prefix = format!("{}/", flow_id);
            self.sessions.retain(|k, _| !k.starts_with(&prefix));
        }

        /// Get the expected Bearer token for WHIP on a flow.
        pub fn whip_bearer_token(&self, flow_id: &str) -> Option<String> {
            self.whip_tokens.get(flow_id).map(|v| v.clone())
        }

        /// Get the expected Bearer token for WHEP on a flow.
        pub fn whep_bearer_token(&self, flow_id: &str) -> Option<String> {
            self.whep_tokens.get(flow_id).map(|v| v.clone())
        }

        /// Handle a WHIP offer: forward to the input task and wait for the answer.
        pub async fn handle_whip_offer(&self, flow_id: &str, offer_sdp: &str) -> Result<(String, String)> {
            let tx = self.whip_input_channels.get(flow_id)
                .ok_or_else(|| anyhow::anyhow!("No WHIP input registered for flow '{}'", flow_id))?
                .clone();

            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            tx.send(NewSessionMsg {
                offer_sdp: offer_sdp.to_string(),
                reply: reply_tx,
            }).await.map_err(|_| anyhow::anyhow!("WHIP input task not responding"))?;

            let (answer, session_id) = reply_rx.await
                .map_err(|_| anyhow::anyhow!("WHIP input task dropped reply"))??;

            // Track session
            let key = format!("{}/{}", flow_id, session_id);
            self.sessions.insert(key, WebrtcSessionHandle {
                cancel: CancellationToken::new(), // TODO: wire to actual session cancel
                session_type: "whip",
            });

            Ok((answer, session_id))
        }

        /// Handle a WHEP offer: forward to the output task and wait for the answer.
        pub async fn handle_whep_offer(&self, flow_id: &str, offer_sdp: &str) -> Result<(String, String)> {
            let tx = self.whep_output_channels.get(flow_id)
                .ok_or_else(|| anyhow::anyhow!("No WHEP output registered for flow '{}'", flow_id))?
                .clone();

            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            tx.send(NewSessionMsg {
                offer_sdp: offer_sdp.to_string(),
                reply: reply_tx,
            }).await.map_err(|_| anyhow::anyhow!("WHEP output task not responding"))?;

            let (answer, session_id) = reply_rx.await
                .map_err(|_| anyhow::anyhow!("WHEP output task dropped reply"))??;

            let key = format!("{}/{}", flow_id, session_id);
            self.sessions.insert(key, WebrtcSessionHandle {
                cancel: CancellationToken::new(),
                session_type: "whep",
            });

            Ok((answer, session_id))
        }

        /// Remove a session.
        pub fn remove_session(&self, flow_id: &str, session_id: &str) {
            let key = format!("{}/{}", flow_id, session_id);
            if let Some((_, handle)) = self.sessions.remove(&key) {
                handle.cancel.cancel();
                tracing::info!("Removed {} session {}/{}", handle.session_type, flow_id, session_id);
            }
        }

        /// Count active sessions for a flow.
        pub fn session_count(&self, flow_id: &str) -> usize {
            let prefix = format!("{}/", flow_id);
            self.sessions.iter().filter(|e| e.key().starts_with(&prefix)).count()
        }
    }
}
