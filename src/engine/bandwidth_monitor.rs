// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-flow bandwidth monitoring for SMPTE RP 2129 trust boundary enforcement.
//!
//! Monitors a flow's input bitrate against a configured limit and takes action
//! (alarm or block) when the limit is exceeded for the configured grace period.
//! The monitor runs as an independent Tokio task, sampling the bitrate every
//! second from the flow's [`FlowStatsAccumulator`].

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::time::{Duration, interval};
use tokio_util::sync::CancellationToken;

use crate::config::models::{BandwidthLimitAction, BandwidthLimitConfig};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

/// Spawn the bandwidth monitor task for a flow.
///
/// Returns a `JoinHandle` that runs until the flow's cancellation token is
/// cancelled. The task checks bitrate once per second and triggers actions
/// after the grace period is exceeded.
pub fn spawn_bandwidth_monitor(
    flow_id: String,
    config: BandwidthLimitConfig,
    stats: Arc<FlowStatsAccumulator>,
    event_sender: EventSender,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        bandwidth_monitor_loop(flow_id, config, stats, event_sender, cancel).await;
    })
}

async fn bandwidth_monitor_loop(
    flow_id: String,
    config: BandwidthLimitConfig,
    stats: Arc<FlowStatsAccumulator>,
    event_sender: EventSender,
    cancel: CancellationToken,
) {
    let limit_bps = (config.max_bitrate_mbps * 1_000_000.0) as u64;
    let grace = config.grace_period_secs;

    let mut ticker = interval(Duration::from_secs(1));
    // Track consecutive seconds over the limit
    let mut over_count: u32 = 0;
    // Whether we have already triggered (to avoid repeated events)
    let mut triggered = false;
    // For block action: track seconds since block started for probe cycle
    let mut blocked_secs: u32 = 0;
    // Whether we are currently in a probe window (unblocked to sample real bitrate)
    let mut probing = false;

    tracing::info!(
        "Bandwidth monitor started for flow '{}': limit {} Mbps, action {:?}, grace {} s",
        flow_id, config.max_bitrate_mbps, config.action, grace
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        // Sample the current input bitrate from the throughput estimator.
        // We read input_bytes and call the throughput estimator via a snapshot,
        // but to avoid the cost of a full snapshot, we read the throughput
        // estimator directly.
        let current_bps = {
            let bytes = stats.input_bytes.load(Ordering::Relaxed);
            stats.input_throughput.sample(bytes)
        };

        let is_over = current_bps > limit_bps;

        // Handle block action probe cycle
        if config.action == BandwidthLimitAction::Block && triggered {
            if probing {
                // We unblocked for 1 second to sample real bitrate
                if is_over {
                    // Still over limit — re-block immediately
                    stats.bandwidth_blocked.store(true, Ordering::Relaxed);
                    probing = false;
                    blocked_secs = 0;
                    tracing::debug!(
                        "Flow '{}' bandwidth still over limit during probe ({} bps > {} bps), re-blocking",
                        flow_id, current_bps, limit_bps
                    );
                } else {
                    // Bandwidth is within limits — recovery!
                    stats.bandwidth_exceeded.store(false, Ordering::Relaxed);
                    stats.bandwidth_blocked.store(false, Ordering::Relaxed);
                    triggered = false;
                    over_count = 0;
                    blocked_secs = 0;
                    probing = false;
                    let current_mbps = current_bps as f64 / 1_000_000.0;
                    event_sender.emit_flow_with_details(
                        EventSeverity::Info,
                        category::BANDWIDTH,
                        format!(
                            "Flow '{}' bandwidth returned to normal, unblocked ({:.1} Mbps <= {:.1} Mbps)",
                            flow_id, current_mbps, config.max_bitrate_mbps
                        ),
                        &flow_id,
                        serde_json::json!({
                            "current_mbps": current_mbps,
                            "limit_mbps": config.max_bitrate_mbps,
                        }),
                    );
                    tracing::info!(
                        "Flow '{}' bandwidth returned to normal ({:.1} Mbps), unblocked",
                        flow_id, current_mbps
                    );
                }
                continue;
            }

            // Currently blocked — increment blocked timer
            blocked_secs += 1;
            if blocked_secs >= grace {
                // Time to probe: briefly unblock for 1 second to sample real bitrate
                stats.bandwidth_blocked.store(false, Ordering::Relaxed);
                probing = true;
                blocked_secs = 0;
                tracing::debug!(
                    "Flow '{}' probing bandwidth (unblocked for 1s to sample)",
                    flow_id
                );
            }
            continue;
        }

        if is_over {
            over_count += 1;
            if over_count >= grace && !triggered {
                triggered = true;
                let current_mbps = current_bps as f64 / 1_000_000.0;
                stats.bandwidth_exceeded.store(true, Ordering::Relaxed);

                match config.action {
                    BandwidthLimitAction::Alarm => {
                        event_sender.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::BANDWIDTH,
                            format!(
                                "Flow '{}' bandwidth exceeded limit ({:.1} Mbps > {:.1} Mbps)",
                                flow_id, current_mbps, config.max_bitrate_mbps
                            ),
                            &flow_id,
                            serde_json::json!({
                                "current_mbps": current_mbps,
                                "limit_mbps": config.max_bitrate_mbps,
                                "action": "alarm",
                            }),
                        );
                        tracing::warn!(
                            "Flow '{}' bandwidth exceeded limit: {:.1} Mbps > {:.1} Mbps",
                            flow_id, current_mbps, config.max_bitrate_mbps
                        );
                    }
                    BandwidthLimitAction::Block => {
                        stats.bandwidth_blocked.store(true, Ordering::Relaxed);
                        blocked_secs = 0;
                        event_sender.emit_flow_with_details(
                            EventSeverity::Critical,
                            category::BANDWIDTH,
                            format!(
                                "Flow '{}' blocked: bandwidth exceeded limit ({:.1} Mbps > {:.1} Mbps)",
                                flow_id, current_mbps, config.max_bitrate_mbps
                            ),
                            &flow_id,
                            serde_json::json!({
                                "current_mbps": current_mbps,
                                "limit_mbps": config.max_bitrate_mbps,
                                "action": "block",
                            }),
                        );
                        tracing::warn!(
                            "Flow '{}' BLOCKED: bandwidth {:.1} Mbps > limit {:.1} Mbps",
                            flow_id, current_mbps, config.max_bitrate_mbps
                        );
                    }
                }
            }
        } else {
            // Bitrate is within limits
            if triggered {
                // Recovery from alarm mode
                let current_mbps = current_bps as f64 / 1_000_000.0;
                stats.bandwidth_exceeded.store(false, Ordering::Relaxed);
                triggered = false;
                event_sender.emit_flow_with_details(
                    EventSeverity::Info,
                    category::BANDWIDTH,
                    format!(
                        "Flow '{}' bandwidth returned to normal ({:.1} Mbps <= {:.1} Mbps)",
                        flow_id, current_mbps, config.max_bitrate_mbps
                    ),
                    &flow_id,
                    serde_json::json!({
                        "current_mbps": current_mbps,
                        "limit_mbps": config.max_bitrate_mbps,
                    }),
                );
                tracing::info!(
                    "Flow '{}' bandwidth returned to normal ({:.1} Mbps)",
                    flow_id, current_mbps
                );
            }
            over_count = 0;
        }
    }

    // Clean up on shutdown
    stats.bandwidth_exceeded.store(false, Ordering::Relaxed);
    stats.bandwidth_blocked.store(false, Ordering::Relaxed);
}
