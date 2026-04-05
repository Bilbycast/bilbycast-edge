// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::middleware;
use axum::Router;
use axum::routing::{delete, get, post, put};
use tokio::sync::{RwLock, broadcast};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::config::models::AppConfig;
use crate::engine::manager::FlowManager;
use crate::tunnel::manager::TunnelManager;

use super::auth::{self, AuthState};
use super::nmos_is05::Is05State;
use super::{flows, nmos, nmos_is05, stats, tunnels, ws};

/// Shared application state accessible from all Axum handlers via [`axum::extract::State`].
#[derive(Clone)]
pub struct AppState {
    /// The current in-memory application configuration.
    pub config: Arc<RwLock<AppConfig>>,
    /// Filesystem path to the persisted `config.json` file.
    pub config_path: PathBuf,
    /// Filesystem path to the persisted `secrets.json` file.
    pub secrets_path: PathBuf,
    /// Handle to the flow engine manager.
    pub flow_manager: Arc<FlowManager>,
    /// Handle to the IP tunnel manager.
    pub tunnel_manager: Arc<TunnelManager>,
    /// Monotonic timestamp recorded at application startup.
    pub start_time: Instant,
    /// Broadcast channel sender for WebSocket stats.
    pub ws_stats_tx: broadcast::Sender<String>,
    /// Optional auth state (None = auth disabled).
    pub auth_state: Option<Arc<AuthState>>,
    /// NMOS IS-05 staged transport parameters.
    pub is05_state: Arc<Is05State>,
    /// WebRTC session registry for WHIP/WHEP endpoints (None when webrtc feature disabled).
    #[cfg(feature = "webrtc")]
    pub webrtc_sessions: Option<Arc<crate::api::webrtc::registry::WebrtcSessionRegistry>>,
}

/// Constructs the main Axum [`Router`] with all API routes, auth middleware, and layers.
///
/// When auth is enabled, the router is split into:
/// - **Public routes**: `/health`, `/oauth/token`, and optionally `/metrics` — no auth required
/// - **Read-only routes**: GET endpoints — require valid JWT (any role)
/// - **Admin routes**: POST/PUT/DELETE mutation endpoints — require `admin` role
///
/// When auth is disabled (no `auth` config or `enabled: false`), all routes are open.
pub fn build_router(state: AppState) -> Router {
    let auth_state = state.auth_state.clone();

    // --- Public routes (never require auth) ---
    let public_routes = Router::new()
        .route("/health", get(stats::health))
        .route("/oauth/token", post(auth::oauth_token_handler));

    // --- Optionally public metrics ---
    let metrics_public = auth_state
        .as_ref()
        .map(|a| a.config.public_metrics)
        .unwrap_or(true);

    // Setup wizard routes (public, no auth — for initial provisioning)
    let public_routes = public_routes
        .route("/setup", get(crate::setup::handlers::setup_page).post(crate::setup::handlers::apply_setup))
        .route("/setup/status", get(crate::setup::handlers::setup_status));

    let public_routes = if metrics_public {
        public_routes.route("/metrics", get(stats::prometheus_metrics))
    } else {
        public_routes
    };

    // --- Protected routes (require valid JWT when auth enabled) ---
    // Read-only routes: any authenticated role (admin or monitor)
    let read_routes = Router::new()
        .route("/api/v1/flows", get(flows::list_flows))
        .route("/api/v1/flows/{flow_id}", get(flows::get_flow))
        .route("/api/v1/stats", get(stats::all_stats))
        .route("/api/v1/stats/{flow_id}", get(stats::flow_stats))
        .route("/api/v1/config", get(flows::get_config))
        .route("/api/v1/ws/stats", get(ws::ws_stats_handler))
        .route("/api/v1/tunnels", get(tunnels::list_tunnels))
        .route("/api/v1/tunnels/{id}", get(tunnels::get_tunnel));

    // Add metrics under auth if not public
    let read_routes = if !metrics_public {
        read_routes.route("/metrics", get(stats::prometheus_metrics))
    } else {
        read_routes
    };

    // Write routes: require admin role (enforced by RequireAdmin extractor in handlers,
    // middleware just validates the JWT is present and not expired)
    let write_routes = Router::new()
        .route("/api/v1/flows", post(flows::create_flow))
        .route(
            "/api/v1/flows/{flow_id}",
            put(flows::update_flow).delete(flows::delete_flow),
        )
        .route("/api/v1/flows/{flow_id}/start", post(flows::start_flow))
        .route("/api/v1/flows/{flow_id}/stop", post(flows::stop_flow))
        .route("/api/v1/flows/{flow_id}/restart", post(flows::restart_flow))
        .route("/api/v1/flows/{flow_id}/outputs", post(flows::add_output))
        .route(
            "/api/v1/flows/{flow_id}/outputs/{output_id}",
            delete(flows::remove_output),
        )
        .route("/api/v1/config", put(flows::replace_config))
        .route("/api/v1/config/reload", post(flows::reload_config))
        .route("/api/v1/tunnels", post(tunnels::create_tunnel))
        .route("/api/v1/tunnels/{id}", delete(tunnels::delete_tunnel));

    // WHIP/WHEP routes (feature-gated)
    #[cfg(feature = "webrtc")]
    let write_routes = {
        use crate::api::webrtc::handlers;
        write_routes
            .route("/api/v1/flows/{flow_id}/whip", post(handlers::whip_offer))
            .route("/api/v1/flows/{flow_id}/whip/{session_id}", delete(handlers::whip_delete))
            .route("/api/v1/flows/{flow_id}/whep", post(handlers::whep_offer))
            .route("/api/v1/flows/{flow_id}/whep/{session_id}", delete(handlers::whep_delete))
    };

    // Combine protected routes with auth middleware
    let protected_routes = Router::new()
        .merge(read_routes)
        .merge(write_routes)
        .route_layer(middleware::from_fn_with_state(
            auth_state.clone(),
            auth::auth_middleware,
        ));

    // NMOS IS-04 and IS-05 routes (public, no auth — for NMOS controller compatibility)
    let nmos_routes = Router::new()
        .nest("/x-nmos/node/v1.3", nmos::nmos_node_router())
        .nest("/x-nmos/connection/v1.1", nmos_is05::nmos_connection_router());

    // Merge everything
    Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .merge(nmos_routes)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
