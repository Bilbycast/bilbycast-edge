//! srtedge binary entry point.
//!
//! Parses CLI arguments, loads and validates the JSON config,
//! starts all enabled flows, launches the axum API server, and
//! runs a background task that publishes stats to WebSocket subscribers every
//! second.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, broadcast};
use tokio_util::sync::CancellationToken;

mod api;
mod config;
mod engine;
mod fec;
mod monitor;
mod redundancy;
mod srt;
mod stats;
mod util;

use config::persistence::load_config;
use config::validation::validate_config;
use engine::manager::FlowManager;
use stats::collector::StatsCollector;
use api::server::{AppState, build_router};

#[derive(Parser, Debug)]
#[command(name = "srtedge")]
#[command(about = "RTP/SMPTE 2022-2 over SRT transport bridge with 2022-7 hitless redundancy")]
#[command(version)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "./config.json")]
    config: PathBuf,

    /// Override API listen port (overrides config file)
    #[arg(short, long)]
    port: Option<u16>,

    /// Override API listen address (overrides config file)
    #[arg(short = 'b', long)]
    bind: Option<String>,

    /// Override monitor dashboard port (overrides config file)
    #[arg(long)]
    monitor_port: Option<u16>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cli.log_level));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .init();

    // Initialize monotonic clock
    util::time::init_epoch();

    tracing::info!(
        "srtedge v{} starting",
        env!("CARGO_PKG_VERSION")
    );

    // Load configuration
    let mut app_config = load_config(&cli.config)?;

    // Validate config
    if let Err(e) = validate_config(&app_config) {
        tracing::error!("Invalid configuration: {e}");
        return Err(e);
    }

    // Apply CLI overrides
    if let Some(port) = cli.port {
        app_config.server.listen_port = port;
    }
    if let Some(ref bind) = cli.bind {
        app_config.server.listen_addr = bind.clone();
    }
    if let Some(mp) = cli.monitor_port {
        if let Some(ref mut mon) = app_config.monitor {
            mon.listen_port = mp;
        }
    }

    let listen_addr = format!(
        "{}:{}",
        app_config.server.listen_addr, app_config.server.listen_port
    );

    // Create shared state
    let (ws_stats_tx, _) = broadcast::channel::<String>(64);
    let global_stats = Arc::new(StatsCollector::new());
    let flow_manager = Arc::new(FlowManager::new(global_stats.clone()));

    let state = AppState {
        config: Arc::new(RwLock::new(app_config.clone())),
        config_path: cli.config.clone(),
        flow_manager: flow_manager.clone(),
        start_time: Instant::now(),
        ws_stats_tx: ws_stats_tx.clone(),
    };

    // Start all enabled flows from config
    for flow in &app_config.flows {
        if flow.enabled {
            match flow_manager.create_flow(flow.clone()).await {
                Ok(()) => tracing::info!("Auto-started flow '{}'", flow.id),
                Err(e) => tracing::error!("Failed to auto-start flow '{}': {e}", flow.id),
            }
        }
    }

    // Build router and start server
    let router = build_router(state.clone());

    let listener = TcpListener::bind(&listen_addr).await?;
    tracing::info!("API server listening on {listen_addr}");

    // Shared shutdown token for coordinated graceful shutdown
    let shutdown_token = CancellationToken::new();

    // Optionally start the monitor dashboard server
    let _monitor_handle = if let Some(ref monitor_config) = app_config.monitor {
        Some(
            monitor::server::start_monitor_server(
                state.clone(),
                monitor_config,
                shutdown_token.clone(),
            )
            .await?,
        )
    } else {
        None
    };

    // Spawn background stats WebSocket publisher (1/sec)
    {
        let ws_tx = ws_stats_tx.clone();
        let stats_fm = flow_manager.clone();
        tokio::spawn(async move {
            stats_publisher_loop(ws_tx, stats_fm).await;
        });
    }

    // Spawn shutdown signal handler
    {
        let token = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to install CTRL+C signal handler");
            tracing::info!("Received shutdown signal");
            token.cancel();
        });
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
        })
        .await?;

    // Graceful shutdown: stop all flows
    flow_manager.stop_all().await;

    tracing::info!("srtedge shutting down");
    Ok(())
}

/// Background task that runs on a 1-second interval to publish live statistics.
///
/// Each tick:
/// 1. Snapshots all flow stats from the atomic accumulators
/// 2. Computes bitrates using per-flow `ThroughputEstimator`s (delta bytes / elapsed * 8)
/// 3. Serializes the enriched snapshots to JSON
/// 4. Broadcasts to all connected WebSocket clients via the broadcast channel
///
/// Estimators are lazily created per flow and per output, and persist across
/// ticks for accurate delta-based throughput computation.
async fn stats_publisher_loop(
    ws_tx: broadcast::Sender<String>,
    flow_manager: Arc<FlowManager>,
) {
    use stats::throughput::ThroughputEstimator;

    let mut interval = tokio::time::interval(Duration::from_secs(1));

    // Per-flow throughput estimators: flow_id -> (input_estimator, output_id -> output_estimator)
    let mut estimators: HashMap<String, (ThroughputEstimator, HashMap<String, ThroughputEstimator>)> =
        HashMap::new();

    loop {
        interval.tick().await;

        let mut snapshots = flow_manager.stats().all_snapshots();

        // Enrich snapshots with computed bitrates
        for snapshot in &mut snapshots {
            let (input_est, output_ests) = estimators
                .entry(snapshot.flow_id.clone())
                .or_insert_with(|| (ThroughputEstimator::new(), HashMap::new()));

            snapshot.input.bitrate_bps = input_est.sample(snapshot.input.bytes_received);

            for output in &mut snapshot.outputs {
                let out_est = output_ests
                    .entry(output.output_id.clone())
                    .or_insert_with(ThroughputEstimator::new);
                output.bitrate_bps = out_est.sample(output.bytes_sent);
            }
        }

        // Broadcast to all WebSocket subscribers (ignore if no subscribers)
        if !snapshots.is_empty() {
            if let Ok(json) = serde_json::to_string(&snapshots) {
                let _ = ws_tx.send(json);
            }
        }
    }
}

