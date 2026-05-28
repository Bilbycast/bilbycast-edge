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
mod upgrade;
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

    /// Override API listen address (legacy single-address override).
    /// Use `--bind-addrs` for dual-stack / multi-listener overrides.
    #[arg(short = 'b', long)]
    bind: Option<String>,

    /// Override API dual-stack listener addresses (comma-separated, e.g.
    /// `0.0.0.0,[::]`). When set, takes precedence over `--bind` and the
    /// config file's `server.listen_addr` / `server.listen_addrs`. v6
    /// entries get `IPV6_V6ONLY=1` so they coexist with v4 listeners on
    /// the same port.
    #[arg(long)]
    bind_addrs: Option<String>,

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

/// SIGSEGV / SIGBUS / SIGABRT handler — diagnostic only. Captures a
/// Rust backtrace and writes it to stderr (which the testbed tees to
/// /tmp/bilbycast-edge-*.log). Keeps the default action afterwards by
/// re-raising the signal with the default disposition so the kernel
/// still core-dumps + the process exits with the expected code.
extern "C" fn diag_fatal_handler(sig: libc::c_int) {
    // Async-signal-safety caveat: std::backtrace + eprintln! are NOT
    // strictly signal-safe (heap allocs, locks). For a one-shot
    // diagnostic on an already-fatal signal this is acceptable —
    // worst case we deadlock and apport collects the core anyway.
    let bt = std::backtrace::Backtrace::force_capture();
    let sig_name = match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGABRT => "SIGABRT",
        libc::SIGILL => "SIGILL",
        libc::SIGFPE => "SIGFPE",
        _ => "FATAL",
    };
    eprintln!("\n=== bilbycast-edge fatal signal: {sig_name} ({sig}) ===");
    eprintln!("{bt}");
    eprintln!("=== end backtrace ===\n");
    // Restore default and re-raise so apport / shells see the right
    // exit code (139 for SEGV, etc.).
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install_diag_signal_handlers() {
    unsafe {
        for sig in [libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT, libc::SIGILL, libc::SIGFPE] {
            libc::signal(sig, diag_fatal_handler as *const () as libc::sighandler_t);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    install_diag_signal_handlers();
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

    // Capture RLIMIT_RTPRIO so HealthPayload.scheduling_status can show
    // the effective ceiling. Determines whether SCHED_FIFO at priority
    // 50 (wire-emit) will be granted by the kernel without CAP_SYS_NICE.
    let rtprio_max = util::runtime_diag::capture_rlimit_rtprio();
    tracing::debug!("RLIMIT_RTPRIO max = {rtprio_max} (SCHED_FIFO priority ceiling)");

    // ── Broadcast-grade scheduling preflight ────────────────────────────
    //
    // Real-time priority (`SCHED_FIFO`) on the wire-emit thread is THE
    // single biggest determinant of PCR_AC accuracy on a Linux host —
    // without it, the kernel can delay our `clock_nanosleep` wake by
    // 0.5–5 ms under load, the wire pacer accumulates lag into its
    // queue, and on long runs the queue overflows → packet drops.
    //
    // Production systemd unit (`packaging/bilbycast-edge.service`)
    // already ships `LimitRTPRIO=99` + `AmbientCapabilities=CAP_SYS_NICE`
    // so this preflight passes silently for installs done via
    // `packaging/install-edge.sh`. The CRITICAL log here surfaces the
    // misconfiguration immediately for manual installs / containers /
    // unprivileged testbed runs.
    if rtprio_max == 0 && !crate::util::runtime_diag::sched_fifo_self_test_can_acquire() {
        tracing::error!(
            "SCHEDULING-CRITICAL: RLIMIT_RTPRIO=0 and CAP_SYS_NICE not granted — \
             wire-emit threads will run at SCHED_OTHER and degrade PCR_AC by \
             0.5–5 ms (p99). Long-run TS outputs may accumulate latency in the \
             wire_tx queue and drop packets under sustained bitrate excursions. \
             Broadcast-grade fix: install via packaging/install-edge.sh (the \
             shipped systemd unit sets LimitRTPRIO=99 + AmbientCapabilities=\
             CAP_SYS_NICE), or grant cap_sys_nice manually: \
             `sudo setcap cap_sys_nice=eip ./bilbycast-edge`."
        );
    }

    // Optional: lock all current + future pages into RAM via mlockall(2)
    // to eliminate major-page-fault stalls on the data-plane hot path.
    // Opt-in via BILBYCAST_MLOCKALL=1 — off by default because hosts
    // without LimitMEMLOCK=infinity (development / testbed / manual
    // installs) would see mlockall fail noisily. The systemd unit at
    // packaging/bilbycast-edge.service ships with LimitMEMLOCK=infinity,
    // so production deployments via the installer get the safe default.
    // Outcome is recorded for HealthPayload.scheduling_status.
    if std::env::var("BILBYCAST_MLOCKALL").as_deref() == Ok("1") {
        let status = util::runtime_diag::try_mlockall_at_startup();
        match status {
            util::runtime_diag::MlockallStatus::Locked => {
                tracing::info!(
                    "mlockall(MCL_CURRENT | MCL_FUTURE): success — all current + future \
                     pages locked; hot path is now immune to major page faults"
                );
            }
            util::runtime_diag::MlockallStatus::Failed { errno } => {
                let hint = match errno {
                    libc::EPERM => " (process lacks CAP_IPC_LOCK and RLIMIT_MEMLOCK is too low; \
                                    set LimitMEMLOCK=infinity in the systemd unit or grant \
                                    CAP_IPC_LOCK)",
                    libc::ENOMEM => " (the calling process's RLIMIT_MEMLOCK is exceeded; raise \
                                     LimitMEMLOCK)",
                    _ => "",
                };
                tracing::warn!(
                    "mlockall(MCL_CURRENT | MCL_FUTURE) failed: errno={errno}{hint} — \
                     hot path remains vulnerable to major page faults; broadcast quality \
                     may degrade under memory pressure"
                );
            }
            util::runtime_diag::MlockallStatus::Disabled => {}
        }
    } else {
        util::runtime_diag::note_mlockall_disabled();
        tracing::debug!(
            "mlockall not requested (set BILBYCAST_MLOCKALL=1 to enable; recommended for \
             production deployments)"
        );
    }

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
    if let Some(ref bind_addrs) = cli.bind_addrs {
        // Comma-separated override; takes precedence over --bind and the
        // config file. Validated by the same parser the config layer uses.
        let entries: Vec<String> = bind_addrs
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        app_config.server.listen_addrs = Some(entries);
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
        #[cfg(feature = "media-codecs")]
        tracing::info!("media-codecs: in-process (libavcodec) — flow thumbnail generation available");
        #[cfg(not(feature = "media-codecs"))]
        tracing::info!("media-codecs: ffmpeg subprocess — flow thumbnail generation available");
    } else {
        #[cfg(not(feature = "media-codecs"))]
        tracing::info!("ffmpeg not found — flow thumbnail generation disabled (install ffmpeg or enable the media-codecs feature)");
        #[cfg(feature = "media-codecs")]
        tracing::info!("thumbnail generation unavailable");
    }

    // Create shared state
    let (ws_stats_tx, _) = broadcast::channel::<String>(64);
    let (mut event_sender, event_rx) = manager::event_channel();

    // Boot watchdog for staged upgrades. Runs *before* any flow init so a
    // crash-loop on the new binary triggers the symlink-revert on the
    // (max_boot_attempts + 1)th boot. No-op when `upgrades` is unset or
    // disabled. The Critical event for a roll-back is queued on the
    // event_sender and flushes on the first manager auth.
    match upgrade::run_boot_watchdog(app_config.upgrades.as_ref(), &event_sender) {
        Ok(upgrade::watchdog::WatchdogOutcome::Continue) => {}
        Ok(upgrade::watchdog::WatchdogOutcome::PendingHealth { attempt }) => {
            tracing::info!(
                "Upgrade boot watchdog: this is boot attempt {attempt} on the staged version; \
                 will be promoted to stable after the configured health window."
            );
        }
        Ok(upgrade::watchdog::WatchdogOutcome::RolledBack { from_version, to_version }) => {
            tracing::warn!(
                "Upgrade boot watchdog: rolled back from {from_version} to {to_version} on \
                 the previous boot — will surface upgrade_rolled_back on next manager auth."
            );
        }
        Err(e) => {
            tracing::warn!("upgrade boot watchdog error: {e:#}");
        }
    }

    // Install the process-wide upgrade coordinator. The manager command
    // dispatcher reads this via `upgrade::global_coordinator()`. No-op
    // when `upgrades` is unset.
    if let Some(ref up_cfg) = app_config.upgrades {
        let coord = upgrade::UpgradeCoordinator::new(
            up_cfg.clone(),
            event_sender.clone(),
            env!("CARGO_PKG_VERSION").to_string(),
        );
        upgrade::install_global_coordinator(coord);
        tracing::info!(
            "Upgrade coordinator installed (enabled={}, install_root={})",
            up_cfg.enabled,
            up_cfg.install_root.display(),
        );
    }

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
    // Make the snapshot reachable from any module that resolves an
    // operator's HW preference at flow-bring-up time (e.g. the display
    // output's `hw_decode` field). Idempotent — only the first call
    // wins for the lifetime of the process.
    engine::hardware_probe::install_static_capabilities(static_capabilities.clone());
    // Local-display enumeration. Linux-only; the cache is consulted by
    // both the capability advertisement and the HealthPayload
    // `display_devices` field. Empty when the box has no DRI nodes —
    // the `"display"` capability simply isn't advertised in that case.
    #[cfg(all(feature = "display", target_os = "linux"))]
    {
        let displays = display::init_displays();
        tracing::info!("display: enumerated {} connector(s)", displays.len());
    }
    // MXL (Media eXchange Layer) probe — try to dlopen libmxl.so at boot.
    // Returns Some on success and gates the `mxl-*` capability bits;
    // returns None gracefully on hosts without libmxl installed so the
    // boot path stays alive. M4 wires the resulting handle into capability
    // advertisement + flow spawn dispatch.
    #[cfg(feature = "mxl")]
    let mxl_domain_manager: Option<std::sync::Arc<engine::mxl::domain::MxlDomainManager>> = {
        engine::mxl::domain::MxlDomainManager::probe().map(std::sync::Arc::new)
    };
    #[cfg(feature = "mxl")]
    match mxl_domain_manager.as_ref() {
        Some(mgr) => {
            // Boot-time perf preflight: confirm /dev/shm is tmpfs (the default
            // MXL domain root). M2 will repeat this per-flow against the
            // operator-configured domain path before attach.
            let shm_tmpfs = engine::mxl::domain::is_tmpfs(std::path::Path::new("/dev/shm"));
            tracing::info!(
                "mxl: probe succeeded — libmxl loaded from {}; /dev/shm tmpfs check = {}",
                mgr.so_path().display(),
                if shm_tmpfs { "OK" } else { "MISS (libmxl perf will degrade)" }
            );
            // Install for engine spawn arms to look up.
            engine::mxl::domain::install_global(std::sync::Arc::clone(mgr));
        }
        None => tracing::info!(
            "mxl: probe found no libmxl.so — mxl-* capabilities will not be advertised"
        ),
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
    #[cfg(all(feature = "display", target_os = "linux"))]
    let display_claim_registry = display::claim_registry::DisplayClaimRegistry::new();
    // Construct the WebRTC session registry before FlowManager so the
    // manager can hold a clone for flow-lifecycle cleanup (unregister on
    // flow stop, granular unregister on hot-remove of WHIP-server inputs).
    // The same Arc is also surfaced on `AppState.webrtc_sessions` below for
    // the HTTP signalling layer.
    #[cfg(feature = "webrtc")]
    let webrtc_sessions: Arc<api::webrtc::registry::WebrtcSessionRegistry> =
        Arc::new(api::webrtc::registry::WebrtcSessionRegistry::new());
    let flow_manager = Arc::new(FlowManager::new(
        global_stats.clone(),
        ffmpeg_available,
        event_sender.clone(),
        resource_state.clone(),
        resource_action,
        Some(static_capabilities.clone()),
        #[cfg(all(feature = "display", target_os = "linux"))]
        Arc::clone(&display_claim_registry),
        #[cfg(feature = "webrtc")]
        Some(Arc::clone(&webrtc_sessions)),
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
        webrtc_sessions: Some(Arc::clone(&webrtc_sessions)),
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

    // Spawn the upgrade watchdog periodic task that promotes
    // `pending_health → stable` once the configured boot health window
    // has elapsed. No-op when `upgrades` is unset.
    if let Some(ref up_cfg) = app_config.upgrades {
        let cfg_clone = up_cfg.clone();
        let install_root = up_cfg.install_root.clone();
        let events = event_sender.clone();
        let cancel = shutdown_token.clone();
        tokio::spawn(async move {
            upgrade::watchdog::run_watchdog_periodic(install_root, cfg_clone, events, cancel).await;
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
                state.start_time,
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

    // Resolve effective bind list. `server.listen_addrs` (when set) takes
    // precedence over the legacy single-address `server.listen_addr`.
    // Default is dual-stack (`0.0.0.0,[::]`); v6 listeners get
    // IPV6_V6ONLY=1 so they coexist with the v4 listener on the same port.
    let bind_entries = app_config.server.effective_listen_addrs();
    let bind_ips = config::validation::parse_listen_addrs(&bind_entries, "server")
        .map_err(|e| anyhow::anyhow!("server bind addresses: {e}"))?;
    let listen_port = app_config.server.listen_port;
    let bind_addrs: Vec<std::net::SocketAddr> = bind_ips
        .iter()
        .map(|ip| std::net::SocketAddr::new(*ip, listen_port))
        .collect();

    // Start API server (with or without TLS)
    #[cfg(feature = "tls")]
    {
        if let Some(ref tls_config) = app_config.server.tls {
            use axum_server::tls_rustls::RustlsConfig;
            let rustls_config = RustlsConfig::from_pem_file(&tls_config.cert_path, &tls_config.key_path)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to load TLS cert/key: {e}"))?;
            tracing::info!("API server TLS enabled (cert={}, key={})", tls_config.cert_path, tls_config.key_path);

            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                shutdown_token.cancelled().await;
                shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
            });

            let mut serve_set: tokio::task::JoinSet<anyhow::Result<()>> =
                tokio::task::JoinSet::new();
            for bind_addr in &bind_addrs {
                let std_listener = build_std_tcp_listener(*bind_addr).map_err(|e| {
                    crate::util::port_error::annotate_bind_error(
                        e,
                        &bind_addr.to_string(),
                        "Edge API server (HTTPS)",
                    )
                })?;
                let router_clone = router.clone();
                let rustls = rustls_config.clone();
                let handle_clone = handle.clone();
                let bind_label = bind_addr.to_string();
                tracing::info!("Edge API server listening on {bind_label} (HTTPS)");
                serve_set.spawn(async move {
                    let server = axum_server::from_tcp_rustls(std_listener, rustls)
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Edge API server failed to wrap listener {bind_label}: {e}"
                            )
                        })?;
                    server
                        .handle(handle_clone)
                        .serve(router_clone.into_make_service_with_connect_info::<std::net::SocketAddr>())
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Edge API server failed to start on {bind_label}: {e}"
                            )
                        })
                });
            }
            // First listener to exit (success or error) tears down the
            // whole serve; dropping the JoinSet aborts the rest.
            if let Some(res) = serve_set.join_next().await {
                res??;
            }
        } else {
            run_multi_listener_plain(bind_addrs.as_slice(), router, shutdown_token).await?;
        }
    }

    #[cfg(not(feature = "tls"))]
    {
        if app_config.server.tls.is_some() {
            tracing::warn!("TLS is configured but the 'tls' feature is not enabled. Build with --features tls to enable HTTPS.");
        }
        run_multi_listener_plain(bind_addrs.as_slice(), router, shutdown_token).await?;
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

/// Build a `std::net::TcpListener` with the dual-stack contract applied:
/// v6 sockets get `IPV6_V6ONLY=1` so they don't claim the v4 address
/// space, both families get `SO_REUSEADDR`, and the listener is set to
/// non-blocking for tokio / axum_server adapters.
fn build_std_tcp_listener(addr: std::net::SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = match addr.ip() {
        std::net::IpAddr::V4(_) => Domain::IPV4,
        std::net::IpAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    if matches!(addr.ip(), std::net::IpAddr::V6(_)) {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// Build a tokio `TcpListener` with the same dual-stack socket options as
/// [`build_std_tcp_listener`].
fn build_tokio_tcp_listener(addr: std::net::SocketAddr) -> std::io::Result<TcpListener> {
    let std_listener = build_std_tcp_listener(addr)?;
    TcpListener::from_std(std_listener)
}

/// Fan out `axum::serve` across every bind address, sharing one router
/// and one shutdown token. First listener to exit terminates the whole
/// serve; dropping the JoinSet aborts the rest.
async fn run_multi_listener_plain(
    bind_addrs: &[std::net::SocketAddr],
    router: axum::Router,
    shutdown_token: CancellationToken,
) -> anyhow::Result<()> {
    let mut serve_set: tokio::task::JoinSet<anyhow::Result<()>> = tokio::task::JoinSet::new();
    for bind_addr in bind_addrs {
        let listener = build_tokio_tcp_listener(*bind_addr).map_err(|e| {
            crate::util::port_error::annotate_bind_error(
                e,
                &bind_addr.to_string(),
                "Edge API server",
            )
        })?;
        let router_clone = router.clone();
        let shutdown_clone = shutdown_token.clone();
        let bind_label = bind_addr.to_string();
        tracing::info!("Edge API server listening on {bind_label}");
        serve_set.spawn(async move {
            axum::serve(
                listener,
                router_clone.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                shutdown_clone.cancelled().await;
            })
            .await
            .map_err(|e| anyhow::anyhow!("Edge API server on {bind_label}: {e}"))
        });
    }
    if let Some(res) = serve_set.join_next().await {
        res??;
    }
    Ok(())
}

