use axum::Json;
use axum::extract::State;
use axum::response::Html;
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
        .with_state(state)
}

async fn dashboard_page() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn monitor_stats(State(state): State<AppState>) -> Json<AllStatsResponse> {
    Json(gather_all_stats(&state).await)
}

pub async fn start_monitor_server(
    state: AppState,
    config: &MonitorConfig,
    shutdown_token: CancellationToken,
) -> anyhow::Result<JoinHandle<()>> {
    let addr = format!("{}:{}", config.listen_addr, config.listen_port);
    let listener = TcpListener::bind(&addr).await?;
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
