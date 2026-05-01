// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
#[cfg(all(feature = "display", target_os = "linux"))]
mod display;
mod engine;
mod fec;
mod manager;
mod media;
mod monitor;
mod observability;
#[cfg(feature = "replay")]
mod replay;
mod redundancy;
mod setup;
mod srt;
mod stats;
mod tunnel;
mod util;

use config::persistence::{load_config_split, save_config_split};
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

    /// Print the one-shot setup-wizard bearer token from the loaded secrets
    /// file and exit. Useful when the first-boot stdout banner was missed.
    #[arg(long)]
    print_setup_token: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // --print-setup-token: load the secrets file, print the token (or a clear
    // "already registered" message), and exit. Runs before tracing init so the
    // output stream stays clean for shell pipelines.
    if cli.print_setup_token {
        let secrets_path = cli.config.with_file_name("secrets.json");
        let app_config = load_config_split(&cli.config, &secrets_path)?;
        match app_config.setup_token.as_deref() {
            Some(t) => {
                println!("{t}");
                return Ok(());
            }
            None => {
                eprintln!(
                    "no setup token configured — node already registered \
                     (setup_enabled={}, setup_token=None)",
                    app_config.setup_enabled,
                );
                return Ok(());
            }
        }
    }

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

    // Derive secrets file path (same directory as config, named "secrets.json")
    let secrets_path = cli.config.with_file_name("secrets.json");

    // Load configuration (split: config.json + secrets.json, with auto-migration)
    let mut app_config = load_config_split(&cli.config, &secrets_path)?;

    // Ensure persistent node UUID exists (used for NMOS IS-04)
    if app_config.node_id.is_none() {
        let node_id = uuid::Uuid::new_v4().to_string();
        tracing::info!("Generated new node_id: {node_id}");
        app_config.node_id = Some(node_id);
        save_config_split(&cli.config, &secrets_path, &app_config)?;
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

    // First-boot setup-token bootstrap. Auto-generates a 256-bit one-shot
    // bearer token if the wizard is still open and no token has been
    // generated yet. The token is persisted (encrypted) to secrets.json
    // and printed to stdout exactly once. Required by /setup for any
    // non-loopback caller; cleared by the manager-client on first
    // successful registration. Runs after CLI overrides so the banner
    // reflects the actual listen port.
    if app_config.setup_enabled && app_config.setup_token.is_none() {
        use rand::RngExt;
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        app_config.setup_token = Some(token.clone());
        save_config_split(&cli.config, &secrets_path, &app_config).map_err(|e| {
            anyhow::anyhow!("Failed to persist auto-generated setup token: {e}")
        })?;
        println!(
            "\n=== bilbycast-edge first-boot setup token ===\n\
             {0}\n\
             Use it to authenticate the wizard from a non-loopback caller:\n  \
             curl -H 'Authorization: Bearer {0}' https://<node>:{1}/setup …\n\
             Loopback (localhost / 127.0.0.1 / ::1) bypasses the check.\n\
             The token is one-shot and is cleared automatically on the first\n\
             successful manager registration. Re-print it any time with:\n  \
             bilbycast-edge --config {2} --print-setup-token\n\
             =================================================",
            token,
            app_config.server.listen_port,
            cli.config.display(),
        );
    }

    let listen_addr = format!(
        "{}:{}",
        app_config.server.listen_addr, app_config.server.listen_port
    );

    // Detect thumbnail generation capability
    let ffmpeg_available = engine::thumbnail::check_thumbnail_available();
    if ffmpeg_available {
        #[cfg(feature = "video-thumbnail")]
        tracing::info!("video-thumbnail: in-process (libavcodec) — flow thumbnail generation available");
        #[cfg(not(feature = "video-thumbnail"))]
        tracing::info!("video-thumbnail: ffmpeg subprocess — flow thumbnail generation available");
    } else {
        #[cfg(not(feature = "video-thumbnail"))]
        tracing::info!("ffmpeg not found — flow thumbnail generation disabled (install ffmpeg or enable the video-thumbnail feature)");
        #[cfg(feature = "video-thumbnail")]
        tracing::info!("thumbnail generation unavailable");
    }

    // Create shared state
    let (ws_stats_tx, _) = broadcast::channel::<String>(64);
    let (mut event_sender, event_rx) = manager::event_channel();

    // Optional structured-JSON log shipper. Installed once before any
    // clone of the original sender is handed out — later clones inherit
    // the same `Option<JsonLogShipper>` value.
    if let Some(ref logging_cfg) = app_config.logging {
        let edge_id = app_config
            .node_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        match observability::JsonLogShipper::from_config(
            logging_cfg,
            edge_id,
            env!("CARGO_PKG_VERSION"),
        ) {
            Ok(Some(shipper)) => {
                tracing::info!("structured-JSON log shipper enabled");
                event_sender.set_log_shipper(shipper);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("failed to start structured-JSON log shipper: {e:#}");
            }
        }
    }

    let global_stats = Arc::new(StatsCollector::new());

    // System resource monitoring (CPU, RAM)
    let resource_state = Arc::new(engine::resource_monitor::SystemResourceState::new());
    // Static hardware capabilities — probed once at startup.
    let static_capabilities =
        Arc::new(engine::hardware_probe::probe_static_capabilities());
    // Local-display enumeration. Linux-only; the cache is consulted by
    // both the capability advertisement and the HealthPayload
    // `display_devices` field. Empty when the box has no DRI nodes —
    // the `"display"` capability simply isn't advertised in that case.
    #[cfg(all(feature = "display", target_os = "linux"))]
    {
        let displays = display::init_displays();
        tracing::info!("display: enumerated {} connector(s)", displays.len());
    }
    tracing::info!(
        "hardware probe: cpu={} ({}/{} cores, avx={:?}); hw_encoders any={}; sw x264/x265={}/{}",
        static_capabilities.cpu.brand,
        static_capabilities.cpu.physical_cores,
        static_capabilities.cpu.logical_cores,
        static_capabilities.cpu.avx_class,
        static_capabilities.hw_encoders.any(),
        static_capabilities.sw_capacity.x264_720p30_streams,
        static_capabilities.sw_capacity.x265_720p30_streams,
    );
    // Live NVIDIA NVENC / NVDEC utilisation. Atomics stay zeroed and
    // `available` stays false on non-NVIDIA hosts and on builds without
    // the `hardware-monitor-nvml` feature.
    let live_gpu_state = Arc::new(engine::hardware_probe::LiveUtilizationState::new());
    let resource_action = app_config.resource_limits.as_ref().map(|rl| rl.critical_action.clone());
    let flow_manager = Arc::new(FlowManager::new(
        global_stats.clone(),
        ffmpeg_available,
        event_sender.clone(),
        resource_state.clone(),
        resource_action,
    ));
    let tunnel_manager = Arc::new(TunnelManager::new(event_sender.clone()));
    let standby_listeners = Arc::new(
        engine::standby_listeners::StandbyListenerManager::new(event_sender.clone()),
    );

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

    // Build rate limiter for /oauth/token endpoint
    let token_rate_limiter = auth_state.as_ref().and_then(|a| {
        let limit = a.config.token_rate_limit_per_minute;
        if limit > 0 {
            tracing::info!("OAuth token endpoint rate limit: {limit} requests/minute per IP");
            Some(Arc::new(api::auth::TokenEndpointRateLimiter::new(limit)))
        } else {
            None
        }
    });

    let state = AppState {
        config: Arc::new(RwLock::new(app_config.clone())),
        config_path: cli.config.clone(),
        secrets_path: secrets_path.clone(),
        flow_manager: flow_manager.clone(),
        tunnel_manager: tunnel_manager.clone(),
        start_time: Instant::now(),
        ws_stats_tx: ws_stats_tx.clone(),
        auth_state,
        is05_state: Arc::new(api::nmos_is05::Is05State::new()),
        is08_state: api::nmos_is08::Is08State::load_or_default(
            cli.config
                .parent()
                .map(|p| p.join("nmos_channel_map.json"))
                .unwrap_or_else(|| std::path::PathBuf::from("nmos_channel_map.json")),
        ),
        #[cfg(feature = "webrtc")]
        webrtc_sessions: Some(Arc::new(api::webrtc::registry::WebrtcSessionRegistry::new())),
        event_sender: Some(event_sender.clone()),
        resource_state: resource_state.clone(),
        standby_listeners: Some(standby_listeners.clone()),
        token_rate_limiter,
    };

    // Start all enabled flows from config
    for flow in &app_config.flows {
        if flow.enabled {
            let resolved = match app_config.resolve_flow(flow) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to resolve flow '{}': {}", flow.id, e);
                    continue;
                }
            };
            match flow_manager.create_flow(resolved).await {
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
                    // Register WHEP output channel with session registry
                    #[cfg(feature = "webrtc")]
                    if let Some((tx, bearer_token)) = &_runtime.whep_session_tx {
                        if let Some(ref registry) = state.webrtc_sessions {
                            registry.register_whep_output(&flow.id, tx.clone(), bearer_token.clone());
                            tracing::info!("Registered WHEP output for flow '{}'", flow.id);
                        }
                    }
                }
                Err(e) => tracing::error!("Failed to auto-start flow '{}': {e}", flow.id),
            }
        }
    }

    // Initialize standby listeners for unassigned passive inputs
    {
        let assigned_input_ids: Vec<&str> = app_config
            .flows
            .iter()
            .filter(|f| f.enabled)
            .flat_map(|f| f.input_ids.iter().map(|s| s.as_str()))
            .collect();
        standby_listeners.sync(&app_config.inputs, &assigned_input_ids);
        let standby_count = standby_listeners.snapshot().len();
        if standby_count > 0 {
            tracing::info!("Started {standby_count} standby listener(s) for unassigned inputs");
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

    // Best-effort NMOS mDNS-SD registration. Kept alive for the lifetime of
    // the process; dropped on shutdown to unregister cleanly. Failures are
    // logged inside the helper and never block flow startup.
    let _nmos_mdns = {
        let node_id = app_config
            .node_id
            .clone()
            .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".into());
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "bilbycast-edge".into());
        let https = app_config.server.tls.is_some();
        api::nmos_mdns::spawn_nmos_node_advertisement(
            &node_id,
            &hostname,
            app_config.server.listen_port,
            https,
        )
    };

    // Shared shutdown token for coordinated graceful shutdown
    let shutdown_token = CancellationToken::new();

    // Spawn system resource monitor (CPU, RAM, optional NVIDIA GPU util)
    let _resource_monitor_handle = engine::resource_monitor::spawn_resource_monitor(
        app_config.resource_limits.clone(),
        resource_state.clone(),
        Some(live_gpu_state.clone()),
        event_sender.clone(),
        shutdown_token.clone(),
    );

    // Optionally start the monitor dashboard server
    let _monitor_handle = if let Some(ref monitor_config) = app_config.monitor {
        Some(
            monitor::server::start_monitor_server(
                state.clone(),
                monitor_config,
                static_capabilities.clone(),
                live_gpu_state.clone(),
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
        let stats_config = state.config.clone();
        tokio::spawn(async move {
            stats_publisher_loop(ws_tx, stats_fm, stats_config).await;
        });
    }

    // Optionally start manager client
    if let Some(ref mgr_config) = app_config.manager {
        if mgr_config.enabled {
            tracing::info!(
                "Manager client enabled, connecting to {} URL(s): {}",
                mgr_config.urls.len(),
                mgr_config.urls.join(", "),
            );
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
                secrets_path.clone(),
                mgr_api_addr,
                mgr_monitor_addr,
                #[cfg(feature = "webrtc")]
                { state.webrtc_sessions.clone() },
                #[cfg(not(feature = "webrtc"))]
                (),
                event_rx,
                resource_state.clone(),
                static_capabilities.clone(),
                live_gpu_state.clone(),
                Some(standby_listeners.clone()),
            );
        }
    }

    // Optionally start the AMWA IS-04 registration client. When enabled, it
    // POSTs the node + IS-04 resources to the configured registry and
    // heartbeats the node so registry-driven NMOS controllers (Celebrum,
    // Riedel, Lawo, EVS, etc.) discover the edge automatically.
    if let Some(reg_cfg) = app_config
        .nmos_registration
        .as_ref()
        .filter(|c| c.enabled)
        .cloned()
    {
        let cfg_handle = state.config.clone();
        let events = event_sender.clone();
        let cancel = shutdown_token.clone();
        let listen_host = resolve_local_ip();
        let listen_port = app_config.server.listen_port;
        let https = app_config.server.tls.is_some();
        tokio::spawn(async move {
            api::nmos_registration::run(
                cfg_handle,
                reg_cfg,
                listen_host,
                listen_port,
                https,
                events,
                cancel,
            )
            .await;
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
                .serve(router.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await
                .map_err(|e| anyhow::anyhow!(
                    "Edge API server failed to start on {listen_addr}: {e}"
                ))?;
        } else {
            let listener = TcpListener::bind(&listen_addr).await.map_err(|e| {
                crate::util::port_error::annotate_bind_error(e, &listen_addr, "Edge API server")
            })?;
            axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
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
        let listener = TcpListener::bind(&listen_addr).await.map_err(|e| {
            crate::util::port_error::annotate_bind_error(e, &listen_addr, "Edge API server")
        })?;
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
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
    app_config: Arc<RwLock<config::models::AppConfig>>,
) {
    use stats::models::FlowStats;
    use stats::throughput::ThroughputEstimator;

    let mut interval = tokio::time::interval(Duration::from_secs(1));

    // Per-flow throughput estimators: flow_id -> (input_estimator, output_id -> output_estimator)
    let mut estimators: HashMap<String, (ThroughputEstimator, HashMap<String, ThroughputEstimator>)> =
        HashMap::new();

    loop {
        interval.tick().await;

        // Skip all work when nobody is listening to stats
        if ws_tx.receiver_count() == 0 {
            continue;
        }

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

        // Include configured-but-not-running flows as idle so they remain
        // visible in the manager UI when stopped (instead of disappearing)
        {
            let config = app_config.read().await;
            for flow_cfg in &config.flows {
                if !snapshots.iter().any(|s| s.flow_id == flow_cfg.id) {
                    snapshots.push(FlowStats {
                        flow_id: flow_cfg.id.clone(),
                        flow_name: flow_cfg.name.clone(),
                        ..Default::default()
                    });
                }
            }
        }

        // Broadcast to all WebSocket subscribers (ignore if no subscribers)
        if let Ok(json) = serde_json::to_string(&snapshots) {
            let _ = ws_tx.send(json);
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

