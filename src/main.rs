// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! bilbycast-edge binary entry point.
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
mod manager;
mod monitor;
mod redundancy;
mod setup;
mod srt;
mod stats;
mod tunnel;
mod util;

use config::persistence::load_config;
use config::validation::validate_config;
use engine::manager::FlowManager;
use stats::collector::StatsCollector;
use tunnel::manager::TunnelManager;
use api::server::{AppState, build_router};

#[derive(Parser, Debug)]
#[command(name = "bilbycast-edge")]
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

    // Install rustls crypto provider (required by rustls 0.23+ for QUIC tunnels)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    // Initialize monotonic clock
    util::time::init_epoch();

    tracing::info!(
        "bilbycast-edge v{} starting (tunnel support enabled)",
        env!("CARGO_PKG_VERSION")
    );

    // Load configuration
    let mut app_config = load_config(&cli.config)?;

    // Ensure persistent node UUID exists (used for NMOS IS-04)
    if app_config.node_id.is_none() {
        let node_id = uuid::Uuid::new_v4().to_string();
        tracing::info!("Generated new node_id: {node_id}");
        app_config.node_id = Some(node_id);
        config::persistence::save_config(&cli.config, &app_config)?;
    }

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
    let tunnel_manager = Arc::new(TunnelManager::new());

    // Set the manager node_id on the tunnel manager so relay tunnels can identify this edge
    if let Some(ref mgr) = app_config.manager {
        if let Some(ref node_id) = mgr.node_id {
            tunnel_manager.set_manager_node_id(node_id.clone());
        }
    }

    // Build auth state from config (None if auth not configured or disabled)
    let auth_state = app_config
        .server
        .auth
        .as_ref()
        .filter(|a| a.enabled)
        .map(|auth_config| {
            tracing::info!(
                "API authentication enabled: {} client(s) configured",
                auth_config.clients.len()
            );
            Arc::new(api::auth::AuthState::new(auth_config.clone()))
        });

    if auth_state.is_none() {
        tracing::warn!("API authentication is DISABLED — all endpoints are open");
    }

    let state = AppState {
        config: Arc::new(RwLock::new(app_config.clone())),
        config_path: cli.config.clone(),
        flow_manager: flow_manager.clone(),
        tunnel_manager: tunnel_manager.clone(),
        start_time: Instant::now(),
        ws_stats_tx: ws_stats_tx.clone(),
        auth_state,
        is05_state: Arc::new(api::nmos_is05::Is05State::new()),
        #[cfg(feature = "webrtc")]
        webrtc_sessions: Some(Arc::new(api::webrtc::registry::WebrtcSessionRegistry::new())),
    };

    // Start all enabled flows from config
    for flow in &app_config.flows {
        if flow.enabled {
            match flow_manager.create_flow(flow.clone()).await {
                Ok(_runtime) => {
                    tracing::info!("Auto-started flow '{}'", flow.id);
                    // Register WHIP input channel with session registry if this is a WebRTC flow
                    #[cfg(feature = "webrtc")]
                    if let Some((tx, bearer_token)) = &_runtime.whip_session_tx {
                        if let Some(ref registry) = state.webrtc_sessions {
                            registry.register_whip_input(&flow.id, tx.clone(), bearer_token.clone());
                            tracing::info!("Registered WHIP input for flow '{}'", flow.id);
                        }
                    }
                }
                Err(e) => tracing::error!("Failed to auto-start flow '{}': {e}", flow.id),
            }
        }
    }

    // Start all enabled tunnels from config
    for tunnel_cfg in &app_config.tunnels {
        if tunnel_cfg.enabled {
            match tunnel_manager.create_tunnel(tunnel_cfg.clone()).await {
                Ok(()) => tracing::info!("Auto-started tunnel '{}'", tunnel_cfg.id),
                Err(e) => tracing::error!("Failed to auto-start tunnel '{}': {e}", tunnel_cfg.id),
            }
        }
    }

    // Build router and start server
    let router = build_router(state.clone());
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

    // Optionally start manager client
    if let Some(ref mgr_config) = app_config.manager {
        if mgr_config.enabled {
            tracing::info!("Manager client enabled, connecting to {}", mgr_config.url);
            let local_ip = resolve_local_ip();
            let api_port = app_config.server.listen_port;
            let mgr_api_addr = format!("{}:{}", local_ip, api_port);
            let mgr_monitor_addr = app_config.monitor.as_ref().map(|m| {
                format!("{}:{}", local_ip, m.listen_port)
            });
            manager::client::start_manager_client(
                mgr_config.clone(),
                flow_manager.clone(),
                tunnel_manager.clone(),
                ws_stats_tx.clone(),
                state.config.clone(),
                cli.config.clone(),
                mgr_api_addr,
                mgr_monitor_addr,
                #[cfg(feature = "webrtc")]
                { state.webrtc_sessions.clone() },
                #[cfg(not(feature = "webrtc"))]
                (),
            );
        }
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

    // Start API server (with or without TLS)
    #[cfg(feature = "tls")]
    {
        if let Some(ref tls_config) = app_config.server.tls {
            use axum_server::tls_rustls::RustlsConfig;
            let rustls_config = RustlsConfig::from_pem_file(&tls_config.cert_path, &tls_config.key_path)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to load TLS cert/key: {e}"))?;
            tracing::info!("API server TLS enabled (cert={}, key={})", tls_config.cert_path, tls_config.key_path);

            let addr: std::net::SocketAddr = listen_addr.parse()?;
            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                shutdown_token.cancelled().await;
                shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
            });

            axum_server::bind_rustls(addr, rustls_config)
                .handle(handle)
                .serve(router.into_make_service())
                .await?;
        } else {
            let listener = TcpListener::bind(&listen_addr).await?;
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    shutdown_token.cancelled().await;
                })
                .await?;
        }
    }

    #[cfg(not(feature = "tls"))]
    {
        if app_config.server.tls.is_some() {
            tracing::warn!("TLS is configured but the 'tls' feature is not enabled. Build with --features tls to enable HTTPS.");
        }
        let listener = TcpListener::bind(&listen_addr).await?;
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                shutdown_token.cancelled().await;
            })
            .await?;
    }

    // Graceful shutdown: stop all flows and tunnels
    flow_manager.stop_all().await;
    tunnel_manager.stop_all().await;

    tracing::info!("bilbycast-edge shutting down");
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

/// Resolve the machine's actual LAN IP address by connecting a UDP socket
/// to an external address (no actual traffic is sent). Falls back to 127.0.0.1.
fn resolve_local_ip() -> String {
    // UDP connect trick — works on Mac and Linux without extra crates.
    // Connecting a UDP socket to a public IP sets the OS routing table
    // source address without sending any packets.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "0.0.0.0" {
                    return ip;
                }
            }
        }
    }
    "127.0.0.1".to_string()
}

