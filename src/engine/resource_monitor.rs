// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! System resource monitoring for hardware capacity enforcement.
//!
//! Monitors CPU and RAM usage via the `sysinfo` crate and takes action
//! (alarm or gate flows) when configurable thresholds are exceeded. The
//! monitor runs as an independent Tokio task, sampling every 5 seconds.
//!
//! **Note on CPU sampling:** `sysinfo::System::global_cpu_usage()` requires
//! at least two refreshes with a delay between them to compute delta-based
//! CPU%. The 5-second loop handles this naturally — the first sample after
//! startup may return 0%, but subsequent samples are accurate.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use sysinfo::System;
use tokio::time::{Duration, interval};
use tokio_util::sync::CancellationToken;

use crate::config::models::ResourceLimitConfig;
use crate::engine::hardware_probe::{LiveUtilizationState, NvmlPoller};
use crate::manager::events::{EventSender, EventSeverity, category};

/// Lock-free shared state for system resource metrics.
///
/// All fields use `Relaxed` ordering, matching the project convention for
/// stats counters. The struct is `Arc`-wrapped and shared with the stats
/// snapshot path, the flow manager (for gating), and the Prometheus endpoint.
pub struct SystemResourceState {
    /// CPU usage as percent × 100 (e.g., 7523 = 75.23%).
    pub cpu_percent: AtomicU32,
    /// RAM usage as percent × 100.
    pub ram_percent: AtomicU32,
    /// RAM used in megabytes.
    pub ram_used_mb: AtomicU64,
    /// Total RAM in megabytes.
    pub ram_total_mb: AtomicU64,
    /// Set to `true` when any metric exceeds its critical threshold.
    /// Read by `FlowManager::create_flow` for flow gating.
    pub resources_critical: AtomicBool,
}

impl SystemResourceState {
    pub fn new() -> Self {
        Self {
            cpu_percent: AtomicU32::new(0),
            ram_percent: AtomicU32::new(0),
            ram_used_mb: AtomicU64::new(0),
            ram_total_mb: AtomicU64::new(0),
            resources_critical: AtomicBool::new(false),
        }
    }

    /// Read the current CPU usage as a float percentage.
    pub fn cpu_percent_f64(&self) -> f64 {
        self.cpu_percent.load(Ordering::Relaxed) as f64 / 100.0
    }

    /// Read the current RAM usage as a float percentage.
    pub fn ram_percent_f64(&self) -> f64 {
        self.ram_percent.load(Ordering::Relaxed) as f64 / 100.0
    }
}

/// Spawn the system resource monitor task.
///
/// Returns a `JoinHandle` that runs until the cancellation token is cancelled.
/// If `config` is `None`, the task still samples metrics (for stats/Prometheus)
/// but does not evaluate thresholds or fire events. When `live_gpu` is
/// provided, the same 5 s loop additionally samples NVIDIA NVENC / NVDEC
/// utilization via NVML — silently skipped on non-NVIDIA hosts and on
/// builds without the `hardware-monitor-nvml` feature.
pub fn spawn_resource_monitor(
    config: Option<ResourceLimitConfig>,
    state: Arc<SystemResourceState>,
    live_gpu: Option<Arc<LiveUtilizationState>>,
    event_sender: EventSender,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        resource_monitor_loop(config, state, live_gpu, event_sender, cancel).await;
    })
}

/// Per-metric threshold state machine.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ThresholdState {
    Normal,
    Warning,
    Critical,
}

async fn resource_monitor_loop(
    config: Option<ResourceLimitConfig>,
    state: Arc<SystemResourceState>,
    live_gpu: Option<Arc<LiveUtilizationState>>,
    event_sender: EventSender,
    cancel: CancellationToken,
) {
    let mut sys = System::new();

    let mut ticker = interval(Duration::from_secs(5));

    // Threshold tracking (only used when config is present)
    let mut cpu_state = ThresholdState::Normal;
    let mut ram_state = ThresholdState::Normal;
    let mut cpu_over_count: u32 = 0;
    let mut ram_over_count: u32 = 0;

    let grace = config.as_ref().map(|c| c.grace_period_secs).unwrap_or(10);

    // NVML init is a one-shot. Failure means no NVIDIA GPU / no driver —
    // a normal state on most edges. We log at debug level and move on.
    let nvml = if live_gpu.is_some() {
        NvmlPoller::try_init()
    } else {
        None
    };

    tracing::info!(
        "System resource monitor started{}",
        if config.is_some() { " (thresholds enabled)" } else { " (monitoring only, no thresholds)" }
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        // Refresh CPU and memory
        sys.refresh_cpu_all();
        sys.refresh_memory();

        let cpu_pct = sys.global_cpu_usage() as f64;
        let total_mem = sys.total_memory(); // bytes
        let used_mem = sys.used_memory(); // bytes
        let ram_pct = if total_mem > 0 {
            (used_mem as f64 / total_mem as f64) * 100.0
        } else {
            0.0
        };

        // Store atomically
        state.cpu_percent.store((cpu_pct * 100.0) as u32, Ordering::Relaxed);
        state.ram_percent.store((ram_pct * 100.0) as u32, Ordering::Relaxed);
        state.ram_used_mb.store(used_mem / (1024 * 1024), Ordering::Relaxed);
        state.ram_total_mb.store(total_mem / (1024 * 1024), Ordering::Relaxed);

        // Live GPU poll (NVIDIA only). NVML failures during steady-state
        // (device gone, driver reset) are absorbed inside `poll`; we just
        // skip the sample silently.
        if let (Some(poller), Some(gpu_state)) = (&nvml, live_gpu.as_ref()) {
            poller.poll(gpu_state);
        }

        // Evaluate thresholds if configured
        if let Some(ref cfg) = config {
            let new_cpu_state = evaluate_threshold(cpu_pct, cfg.cpu_warning_percent, cfg.cpu_critical_percent);
            let new_ram_state = evaluate_threshold(ram_pct, cfg.ram_warning_percent, cfg.ram_critical_percent);

            // CPU threshold transitions
            cpu_state = handle_metric_transition(
                "CPU",
                cpu_pct,
                cpu_state,
                new_cpu_state,
                &mut cpu_over_count,
                grace,
                cfg.cpu_warning_percent,
                cfg.cpu_critical_percent,
                &event_sender,
            );

            // RAM threshold transitions
            ram_state = handle_metric_transition(
                "RAM",
                ram_pct,
                ram_state,
                new_ram_state,
                &mut ram_over_count,
                grace,
                cfg.ram_warning_percent,
                cfg.ram_critical_percent,
                &event_sender,
            );

            // Update critical flag
            let is_critical = cpu_state == ThresholdState::Critical || ram_state == ThresholdState::Critical;
            state.resources_critical.store(is_critical, Ordering::Relaxed);
        }
    }

    // Clean up on shutdown
    state.resources_critical.store(false, Ordering::Relaxed);
    tracing::info!("System resource monitor stopped");
}

fn evaluate_threshold(value: f64, warning: f64, critical: f64) -> ThresholdState {
    if value >= critical {
        ThresholdState::Critical
    } else if value >= warning {
        ThresholdState::Warning
    } else {
        ThresholdState::Normal
    }
}

/// Handle state transitions for a single metric with grace period.
///
/// Returns the new effective state (transitions only happen after
/// `grace` consecutive samples at the new level).
fn handle_metric_transition(
    metric_name: &str,
    current_value: f64,
    current_state: ThresholdState,
    target_state: ThresholdState,
    over_count: &mut u32,
    grace: u32,
    warning_threshold: f64,
    critical_threshold: f64,
    event_sender: &EventSender,
) -> ThresholdState {
    if (target_state as u8) > (current_state as u8) {
        // Escalating — increment counter
        *over_count += 1;
        if *over_count >= grace {
            *over_count = 0;
            // Fire event for the transition
            match target_state {
                ThresholdState::Warning => {
                    event_sender.emit_with_details(
                        EventSeverity::Warning,
                        category::SYSTEM_RESOURCES,
                        format!(
                            "{metric_name} usage {current_value:.1}% exceeds warning threshold {warning_threshold:.0}%"
                        ),
                        None,
                        serde_json::json!({
                            "metric": metric_name.to_lowercase(),
                            "current_percent": current_value,
                            "warning_threshold": warning_threshold,
                            "critical_threshold": critical_threshold,
                        }),
                    );
                    tracing::warn!(
                        "{metric_name} usage {current_value:.1}% exceeds warning threshold {warning_threshold:.0}%"
                    );
                    return ThresholdState::Warning;
                }
                ThresholdState::Critical => {
                    event_sender.emit_with_details(
                        EventSeverity::Critical,
                        category::SYSTEM_RESOURCES,
                        format!(
                            "{metric_name} usage {current_value:.1}% exceeds critical threshold {critical_threshold:.0}%"
                        ),
                        None,
                        serde_json::json!({
                            "metric": metric_name.to_lowercase(),
                            "current_percent": current_value,
                            "warning_threshold": warning_threshold,
                            "critical_threshold": critical_threshold,
                        }),
                    );
                    tracing::error!(
                        "{metric_name} usage {current_value:.1}% exceeds CRITICAL threshold {critical_threshold:.0}%"
                    );
                    return ThresholdState::Critical;
                }
                ThresholdState::Normal => unreachable!(),
            }
        }
        // Not enough consecutive samples yet — stay at current state
        current_state
    } else if (target_state as u8) < (current_state as u8) {
        // De-escalating — recovery
        *over_count = 0;
        match (current_state, target_state) {
            (ThresholdState::Critical, ThresholdState::Warning) => {
                event_sender.emit_with_details(
                    EventSeverity::Warning,
                    category::SYSTEM_RESOURCES,
                    format!(
                        "{metric_name} usage {current_value:.1}% recovered below critical threshold {critical_threshold:.0}%, still above warning"
                    ),
                    None,
                    serde_json::json!({
                        "metric": metric_name.to_lowercase(),
                        "current_percent": current_value,
                    }),
                );
                tracing::info!(
                    "{metric_name} usage recovered below critical ({current_value:.1}%), still warning"
                );
                ThresholdState::Warning
            }
            (_, ThresholdState::Normal) => {
                event_sender.emit_with_details(
                    EventSeverity::Info,
                    category::SYSTEM_RESOURCES,
                    format!(
                        "{metric_name} usage {current_value:.1}% returned to normal"
                    ),
                    None,
                    serde_json::json!({
                        "metric": metric_name.to_lowercase(),
                        "current_percent": current_value,
                    }),
                );
                tracing::info!("{metric_name} usage returned to normal ({current_value:.1}%)");
                ThresholdState::Normal
            }
            _ => target_state,
        }
    } else {
        // No change — reset counter
        *over_count = 0;
        current_state
    }
}
