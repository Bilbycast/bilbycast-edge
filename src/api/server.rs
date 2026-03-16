use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::routing::{delete, get, post};
use tokio::sync::{RwLock, broadcast};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::config::models::AppConfig;
use crate::engine::manager::FlowManager;

use super::{flows, stats, ws};

/// Shared application state accessible from all Axum handlers via [`axum::extract::State`].
///
/// This struct is cheaply cloneable (all fields are either `Arc`-wrapped or `Copy`)
/// and is injected into every request handler by the Axum router.
#[derive(Clone)]
pub struct AppState {
    /// The current in-memory application configuration, protected by an async read-write lock.
    /// Handlers acquire a read lock for queries and a write lock for mutations (create, update,
    /// delete, config replace, config reload).
    pub config: Arc<RwLock<AppConfig>>,
    /// Filesystem path to the persisted `config.json` file. Used by mutation handlers to
    /// write configuration changes back to disk after modifying the in-memory config.
    pub config_path: PathBuf,
    /// Handle to the flow engine manager that controls the lifecycle of all SRT/RTP flows.
    /// Provides methods to create, start, stop, and destroy flows, as well as to query
    /// runtime statistics.
    pub flow_manager: Arc<FlowManager>,
    /// Monotonic timestamp recorded at application startup. Used to compute uptime for
    /// the health endpoint and statistics responses.
    pub start_time: Instant,
    /// Broadcast channel sender for pushing real-time statistics to WebSocket clients.
    /// The stats publisher task sends JSON-serialized [`super::models::WsStatsMessage`]
    /// payloads through this channel, and each connected WebSocket client subscribes
    /// via [`broadcast::Sender::subscribe`].
    pub ws_stats_tx: broadcast::Sender<String>,
}

/// Constructs the main Axum [`Router`] with all API routes and middleware.
///
/// # Routes
///
/// **Flow CRUD**
/// - `GET    /api/v1/flows`                        -- list all configured flows
/// - `POST   /api/v1/flows`                        -- create a new flow
/// - `GET    /api/v1/flows/{flow_id}`              -- get a single flow by ID
/// - `PUT    /api/v1/flows/{flow_id}`              -- update (replace) a flow
/// - `DELETE /api/v1/flows/{flow_id}`              -- delete a flow
///
/// **Flow actions**
/// - `POST   /api/v1/flows/{flow_id}/start`        -- start a stopped flow
/// - `POST   /api/v1/flows/{flow_id}/stop`         -- stop a running flow
/// - `POST   /api/v1/flows/{flow_id}/restart`      -- restart (stop + start) a flow
///
/// **Output management**
/// - `POST   /api/v1/flows/{flow_id}/outputs`              -- add an output to a flow
/// - `DELETE /api/v1/flows/{flow_id}/outputs/{output_id}`   -- remove an output from a flow
///
/// **Statistics**
/// - `GET    /api/v1/stats`              -- aggregated system + per-flow statistics
/// - `GET    /api/v1/stats/{flow_id}`    -- statistics for a single flow
///
/// **Configuration**
/// - `GET    /api/v1/config`         -- retrieve the full running configuration
/// - `PUT    /api/v1/config`         -- replace the entire configuration
/// - `POST   /api/v1/config/reload`  -- reload configuration from disk
///
/// **Health & Metrics**
/// - `GET    /health`    -- lightweight health check
/// - `GET    /metrics`   -- Prometheus-compatible metrics endpoint
///
/// **WebSocket**
/// - `GET    /api/v1/ws/stats`  -- WebSocket upgrade for real-time stats streaming
///
/// # Middleware
///
/// The router includes [`TraceLayer`] for HTTP request/response logging and
/// [`CorsLayer::permissive`] for unrestricted cross-origin requests during development.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Flow CRUD
        .route("/api/v1/flows", get(flows::list_flows).post(flows::create_flow))
        .route(
            "/api/v1/flows/{flow_id}",
            get(flows::get_flow)
                .put(flows::update_flow)
                .delete(flows::delete_flow),
        )
        // Flow actions
        .route("/api/v1/flows/{flow_id}/start", post(flows::start_flow))
        .route("/api/v1/flows/{flow_id}/stop", post(flows::stop_flow))
        .route("/api/v1/flows/{flow_id}/restart", post(flows::restart_flow))
        // Output management
        .route("/api/v1/flows/{flow_id}/outputs", post(flows::add_output))
        .route(
            "/api/v1/flows/{flow_id}/outputs/{output_id}",
            delete(flows::remove_output),
        )
        // Stats
        .route("/api/v1/stats", get(stats::all_stats))
        .route("/api/v1/stats/{flow_id}", get(stats::flow_stats))
        // Config
        .route(
            "/api/v1/config",
            get(flows::get_config).put(flows::replace_config),
        )
        .route("/api/v1/config/reload", post(flows::reload_config))
        // Health & Metrics
        .route("/health", get(stats::health))
        .route("/metrics", get(stats::prometheus_metrics))
        // WebSocket
        .route("/api/v1/ws/stats", get(ws::ws_stats_handler))
        // Middleware
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
