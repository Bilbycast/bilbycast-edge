// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-flow continuous-degradation monitoring.
//!
//! Lifecycle events (input-started, output-failed-to-bind) are emitted by the
//! individual input/output tasks. This monitor watches the flow's running
//! state for *continuous* degradation an operator cares about but which no
//! single packet-handler is positioned to see:
//!
//! - Input stall: `input_bytes` has not advanced for N seconds while the flow
//!   is running — emits a warning, then recovery-info when bytes flow again.
//! - FEC recovery rate: percentage of received packets that needed FEC
//!   recovery over a sliding window is above threshold — indicates the link
//!   is running near its loss budget even if no hard errors show yet.
//!
//! Runs as a single per-flow task, polling once per second.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::time::{Duration, interval};
use tokio_util::sync::CancellationToken;

use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

/// Seconds of zero input-byte delta before firing the stall warning.
const STALL_THRESHOLD_SECS: u32 = 5;
/// Packets recovered by FEC / packets received, averaged over the window.
/// 2% is well above the expected steady-state recovery rate on a clean link.
const FEC_RATE_ALARM_THRESHOLD: f64 = 0.02;
/// Sliding window for FEC rate in seconds.
const FEC_RATE_WINDOW_SECS: u32 = 10;

pub fn spawn_degradation_monitor(
    flow_id: String,
    stats: Arc<FlowStatsAccumulator>,
    event_sender: EventSender,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        degradation_monitor_loop(flow_id, stats, event_sender, cancel).await;
    })
}

async fn degradation_monitor_loop(
    flow_id: String,
    stats: Arc<FlowStatsAccumulator>,
    event_sender: EventSender,
    cancel: CancellationToken,
) {
    let mut ticker = interval(Duration::from_secs(1));
    // Stall tracking
    let mut last_bytes: u64 = stats.input_bytes.load(Ordering::Relaxed);
    let mut stall_secs: u32 = 0;
    let mut stall_fired = false;
    // FEC rate tracking — sliding window via simple circular buffer.
    let mut window_recv: [u64; FEC_RATE_WINDOW_SECS as usize] = [0; FEC_RATE_WINDOW_SECS as usize];
    let mut window_fec: [u64; FEC_RATE_WINDOW_SECS as usize] = [0; FEC_RATE_WINDOW_SECS as usize];
    let mut window_idx: usize = 0;
    let mut window_filled: u32 = 0;
    let mut last_recv = stats.input_packets.load(Ordering::Relaxed);
    let mut last_fec = stats.fec_recovered.load(Ordering::Relaxed);
    let mut fec_alarm_fired = false;
    // Thumbnail black/frozen alarm transitions. We mirror the accumulator's
    // alarm string and emit events on each transition — polled rather than
    // callback-driven because the thumbnail task doesn't have an EventSender
    // and plumbing one through would touch the data-path subscribers.
    let mut last_thumb_alarm: Option<String> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        // ── Input stall check ──
        let now_bytes = stats.input_bytes.load(Ordering::Relaxed);
        if now_bytes == last_bytes {
            stall_secs = stall_secs.saturating_add(1);
            if stall_secs == STALL_THRESHOLD_SECS && !stall_fired {
                stall_fired = true;
                event_sender.emit_flow_with_details(
                    EventSeverity::Warning,
                    category::FLOW,
                    format!(
                        "Flow '{flow_id}' input stalled — no bytes received in {STALL_THRESHOLD_SECS} seconds"
                    ),
                    &flow_id,
                    serde_json::json!({
                        "subsystem": "degradation",
                        "stall_seconds": STALL_THRESHOLD_SECS,
                    }),
                );
            }
        } else {
            if stall_fired {
                event_sender.emit_flow_with_details(
                    EventSeverity::Info,
                    category::FLOW,
                    format!("Flow '{flow_id}' input resumed"),
                    &flow_id,
                    serde_json::json!({
                        "subsystem": "degradation",
                        "stalled_for_secs": stall_secs,
                    }),
                );
            }
            stall_secs = 0;
            stall_fired = false;
            last_bytes = now_bytes;
        }

        // ── FEC recovery rate check ──
        let now_recv = stats.input_packets.load(Ordering::Relaxed);
        let now_fec = stats.fec_recovered.load(Ordering::Relaxed);
        let d_recv = now_recv.saturating_sub(last_recv);
        let d_fec = now_fec.saturating_sub(last_fec);
        window_recv[window_idx] = d_recv;
        window_fec[window_idx] = d_fec;
        window_idx = (window_idx + 1) % window_recv.len();
        if window_filled < FEC_RATE_WINDOW_SECS {
            window_filled += 1;
        }
        last_recv = now_recv;
        last_fec = now_fec;

        if window_filled == FEC_RATE_WINDOW_SECS {
            let sum_recv: u64 = window_recv.iter().sum();
            let sum_fec: u64 = window_fec.iter().sum();
            // Require a floor of packets/sec so we don't alarm on a trickle
            // link recovering its only packet.
            if sum_recv >= 1_000 {
                let rate = sum_fec as f64 / sum_recv as f64;
                if rate >= FEC_RATE_ALARM_THRESHOLD && !fec_alarm_fired {
                    fec_alarm_fired = true;
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning,
                        category::FLOW,
                        format!(
                            "Flow '{flow_id}' FEC recovery rate elevated ({:.1}%) — link is lossy, headroom may be low",
                            rate * 100.0
                        ),
                        &flow_id,
                        serde_json::json!({
                            "subsystem": "degradation",
                            "fec_rate_percent": rate * 100.0,
                            "window_secs": FEC_RATE_WINDOW_SECS,
                            "window_packets": sum_recv,
                            "window_fec_recovered": sum_fec,
                        }),
                    );
                } else if rate < FEC_RATE_ALARM_THRESHOLD / 2.0 && fec_alarm_fired {
                    // Hysteresis: clear only when recovery rate drops well
                    // below the alarm threshold.
                    fec_alarm_fired = false;
                    event_sender.emit_flow_with_details(
                        EventSeverity::Info,
                        category::FLOW,
                        format!(
                            "Flow '{flow_id}' FEC recovery rate back to normal ({:.1}%)",
                            rate * 100.0
                        ),
                        &flow_id,
                        serde_json::json!({
                            "subsystem": "degradation",
                            "fec_rate_percent": rate * 100.0,
                        }),
                    );
                }
            }
        }

        // ── Thumbnail black/frozen alarm transitions ──
        let thumb_alarm = stats
            .thumbnail
            .get()
            .and_then(|t| t.current_alarm());
        if thumb_alarm != last_thumb_alarm {
            match (&last_thumb_alarm, &thumb_alarm) {
                (_, Some(kind)) => {
                    let msg = match kind.as_str() {
                        "black" => format!("Flow '{flow_id}' video is black (thumbnail luminance below threshold)"),
                        "frozen" => format!("Flow '{flow_id}' video is frozen (identical thumbnail across multiple captures)"),
                        other => format!("Flow '{flow_id}' thumbnail alarm: {other}"),
                    };
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning,
                        category::FLOW,
                        msg,
                        &flow_id,
                        serde_json::json!({
                            "subsystem": "media_analysis",
                            "alarm": kind,
                        }),
                    );
                }
                (Some(prev), None) => {
                    event_sender.emit_flow_with_details(
                        EventSeverity::Info,
                        category::FLOW,
                        format!("Flow '{flow_id}' video alarm cleared (was {prev})"),
                        &flow_id,
                        serde_json::json!({
                            "subsystem": "media_analysis",
                            "cleared_alarm": prev,
                        }),
                    );
                }
                _ => {}
            }
            last_thumb_alarm = thumb_alarm;
        }
    }
}
