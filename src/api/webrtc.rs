// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
    use axum::body::Body;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::{IntoResponse, Response};

    use crate::api::server::AppState;

    /// The longest reason a refused offer's 400 carries in its body.
    const REFUSAL_BODY_MAX: usize = 256;

    /// The response to a WHIP / WHEP offer that could not be answered. An
    /// offer at fault (`session::offer_was_at_fault`: it did not parse, str0m
    /// refused it or panicked on it, or it carries nothing to send) gets 400
    /// and a short text reason, as the relay answers; anything else is this
    /// end's fault and stays a bare 500. Every failure used to be the bare
    /// 500, so a client could not tell its own bad offer from an edge fault.
    /// The `webrtc_negotiation_panic` Warning is raised where the offer was
    /// negotiated, either way.
    pub(crate) fn offer_failed(kind: &str, flow_id: &str, err: &anyhow::Error) -> Response {
        tracing::error!("{kind} offer error for flow '{flow_id}': {err}");
        if !crate::engine::webrtc::session::offer_was_at_fault(err) {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let mut reason = format!("{kind} offer refused: {err}");
        if reason.len() > REFUSAL_BODY_MAX {
            let mut end = REFUSAL_BODY_MAX;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
        reason.push('\n');
        (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            reason,
        )
            .into_response()
    }

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
        if let Some(ct) = headers.get(header::CONTENT_TYPE)
            && ct.to_str().unwrap_or("") != "application/sdp" {
                return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
            }

        // Validate Bearer token if configured
        if let Some(ref registry) = state.webrtc_sessions
            && let Some(ref expected_token) = registry.whip_bearer_token(&flow_id) {
                let provided = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                match provided {
                    Some(token) if token == expected_token.as_str() => {}
                    _ => return Err(StatusCode::UNAUTHORIZED),
                }
            }

        // Create WebRTC session and process SDP offer
        let registry = state.webrtc_sessions.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let (answer_sdp, session_id) = match registry.handle_whip_offer(&flow_id, &body).await {
            Ok(answered) => answered,
            // 400 with the reason for an offer at fault, else 500.
            Err(e) => return Ok(offer_failed("WHIP", &flow_id, &e)),
        };

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

        if let Some(ct) = headers.get(header::CONTENT_TYPE)
            && ct.to_str().unwrap_or("") != "application/sdp" {
                return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
            }

        // Validate Bearer token if configured
        if let Some(ref registry) = state.webrtc_sessions
            && let Some(ref expected_token) = registry.whep_bearer_token(&flow_id) {
                let provided = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                match provided {
                    Some(token) if token == expected_token.as_str() => {}
                    _ => return Err(StatusCode::UNAUTHORIZED),
                }
            }

        let registry = state.webrtc_sessions.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let (answer_sdp, session_id) = match registry.handle_whep_offer(&flow_id, &body).await {
            Ok(answered) => answered,
            // 400 with the reason for an offer at fault, else 500.
            Err(e) => return Ok(offer_failed("WHEP", &flow_id, &e)),
        };

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
        /// Channel to send back the SDP answer, session ID, and a
        /// per-session `CancellationToken` the session task listens to.
        /// The API layer holds that token and cancels it from
        /// [`WebrtcSessionRegistry::remove_session`] so DELETE /whip/... or
        /// DELETE /whep/... tears down the exact session rather than
        /// orphaning it.
        pub reply: tokio::sync::oneshot::Sender<Result<(String, String, CancellationToken)>>,
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

        /// Unregister all channels + sessions for a flow.
        ///
        /// Called on flow stop / destroy (via `FlowManager::destroy_flow`)
        /// so the registry doesn't hold dead `mpsc::Sender`s for flows
        /// that no longer exist. Without this, a browser pairing against
        /// the stale flow id would race on a closed channel — clean
        /// removal returns a 404-style "no WHIP input registered for
        /// flow" instead.
        pub fn unregister_flow(&self, flow_id: &str) {
            self.whip_input_channels.remove(flow_id);
            self.whep_output_channels.remove(flow_id);
            self.whip_tokens.remove(flow_id);
            self.whep_tokens.remove(flow_id);
            // Remove all sessions for this flow
            let prefix = format!("{}/", flow_id);
            self.sessions.retain(|k, _| !k.starts_with(&prefix));
        }

        /// Unregister only the WHIP-input entry for a flow. Used by
        /// `FlowRuntime::remove_input` when the input being hot-removed
        /// is the flow's WHIP-server input — leaves the flow's WHEP
        /// outputs (and their tokens) untouched.
        pub fn unregister_whip_input(&self, flow_id: &str) {
            self.whip_input_channels.remove(flow_id);
            self.whip_tokens.remove(flow_id);
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

            let (answer, session_id, session_cancel) = reply_rx.await
                .map_err(|_| anyhow::anyhow!("WHIP input task dropped reply"))??;

            // Track session. `session_cancel` is the token the engine session
            // task is listening on, so cancelling it from remove_session()
            // actually terminates the RTCPeerConnection instead of leaking it.
            let key = format!("{}/{}", flow_id, session_id);
            self.sessions.insert(key, WebrtcSessionHandle {
                cancel: session_cancel,
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

            let (answer, session_id, session_cancel) = reply_rx.await
                .map_err(|_| anyhow::anyhow!("WHEP output task dropped reply"))??;

            let key = format!("{}/{}", flow_id, session_id);
            self.sessions.insert(key, WebrtcSessionHandle {
                cancel: session_cancel,
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
        /// Retained for future monitoring/diagnostics use.
        #[allow(dead_code)]
        pub fn session_count(&self, flow_id: &str) -> usize {
            let prefix = format!("{}/", flow_id);
            self.sessions.iter().filter(|e| e.key().starts_with(&prefix)).count()
        }
    }
}

#[cfg(all(test, feature = "webrtc"))]
mod tests {
    use super::handlers::offer_failed;
    use super::registry::WebrtcSessionRegistry;
    use axum::http::StatusCode;
    use std::sync::Arc;

    const CHROME_WHEP_OFFER: &str = include_str!("../engine/webrtc/testdata/chrome124-whep-recvonly.sdp");
    const RTX_REPAIRING_TWO_PTS: &str = include_str!("../engine/webrtc/testdata/rtx-repairing-two-pts.sdp");

    async fn body(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// An offer the WHEP output could not answer for its own fault — it does
    /// not parse, or str0m panics on it — comes back 400 with a short text
    /// reason, as the relay answers it; one this end failed is still a bare
    /// 500. Every failure used to be the bare 500, empty-bodied.
    #[tokio::test]
    async fn an_offer_at_fault_is_answered_400_with_its_reason() {
        let config: crate::config::models::WebrtcOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "whep-out",
            "name": "WHEP",
            "mode": "whep_server",
            "public_ip": "127.0.0.1",
        }))
        .unwrap();
        let (broadcast_tx, _keep) = tokio::sync::broadcast::channel(16);
        let stats = Arc::new(crate::stats::collector::OutputStatsAccumulator::new(
            "whep-out".into(),
            "WHEP".into(),
            "webrtc".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
        let (events, _raised) = crate::manager::events::event_channel();
        let task = crate::engine::output_webrtc::spawn_webrtc_output(
            config, &broadcast_tx, stats, cancel.clone(), Some(session_rx), events, "flow-a".into(), false,
        );
        let registry = WebrtcSessionRegistry::new();
        registry.register_whep_output("flow-a", session_tx, None);

        for (offer, reason) in [
            ("v=0\r\nnot an offer\r\n", "SDP parse error"),
            (RTX_REPAIRING_TWO_PTS, "str0m panicked during SDP offer: Pt locked multiple times"),
        ] {
            let err = registry.handle_whep_offer("flow-a", offer).await.unwrap_err();
            let resp = offer_failed("WHEP", "flow-a", &err);
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{err}");
            assert_eq!(resp.headers()["content-type"], "text/plain; charset=utf-8");
            let text = body(resp).await;
            assert!(text.starts_with("WHEP offer refused: ") && text.contains(reason), "{text}");
            assert!(text.len() <= 257, "{} bytes", text.len());
            // str0m's parse error names a heap address of this process
            // (`PointerOffset(0x…)`) and spans several lines of parser
            // internals: it stays in the log, and the body says what it is.
            assert!(
                !text.contains("0x") && !text.contains("PointerOffset"),
                "{text}"
            );
            assert_eq!(text.lines().count(), 1, "{text}");
        }
        let err = registry
            .handle_whep_offer("flow-a", "v=0\r\nbogus\r\n")
            .await
            .unwrap_err();
        assert_eq!(
            body(offer_failed("WHEP", "flow-a", &err)).await,
            "WHEP offer refused: SDP parse error: the offer is not valid SDP\n"
        );
        assert!(registry.handle_whep_offer("flow-a", CHROME_WHEP_OFFER).await.is_ok());

        // Not the offer's fault: no WHEP output on this flow.
        let err = registry.handle_whep_offer("flow-b", CHROME_WHEP_OFFER).await.unwrap_err();
        let resp = offer_failed("WHEP", "flow-b", &err);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body(resp).await.is_empty());

        // A long reason is cut short, on a character boundary.
        let long = anyhow::Error::from(crate::engine::webrtc::session::OfferRefused("é".repeat(400)));
        let text = body(offer_failed("WHIP", "flow-a", &long)).await;
        assert!(text.len() <= 257 && text.ends_with('\n'), "{} bytes", text.len());
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }
}
