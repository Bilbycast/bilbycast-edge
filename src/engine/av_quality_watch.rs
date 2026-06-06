// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-flow A/V quality threshold watcher.
//!
//! Polls two flow-level metrics every 5 s and emits transition events
//! (state-machine with a 2-poll grace, mirroring
//! `engine::resource_monitor`):
//!
//! - **Edge-added A/V skew** (`stats::av_skew`, exact lip-sync error
//!   from the PTS-touching stages): |skew| > 40 ms (EBU R37 error
//!   threshold) sustained over two polls → Warning
//!   `av_skew_exceeded`; recovery below 30 ms → Info
//!   `av_skew_recovered`. This is the alarm the old mux-position
//!   "av_sync" metric pretended to be.
//!
//! - **A/V mux interleave** (`stats::av_interleave`): windowed p95
//!   above 1200 ms sustained → Warning `av_interleave_deep` — not a
//!   lip-sync fault, but consumer players whose caching is shallower
//!   than the interleave (VLC defaults ~1 s) starve their audio queue
//!   and drop out. Recovery below 900 ms → Info
//!   `av_interleave_recovered`. Hysteresis prevents flapping on
//!   bursty sources.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

const POLL_INTERVAL_SECS: u64 = 5;
/// EBU R37 error threshold for edge-added skew.
const SKEW_ALERT_MS: i64 = 40;
const SKEW_RECOVER_MS: i64 = 30;
/// Interleave beyond a consumer player's default caching.
const INTERLEAVE_ALERT_MS: i64 = 1200;
const INTERLEAVE_RECOVER_MS: i64 = 900;
/// Consecutive over-threshold polls before alerting (grace).
const GRACE_POLLS: u32 = 2;

pub fn spawn_av_quality_watch(
    flow_id: String,
    flow_stats: Arc<FlowStatsAccumulator>,
    events: EventSender,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut skew_over: u32 = 0;
        let mut skew_alerted = false;
        let mut il_over: u32 = 0;
        let mut il_alerted = false;
        let mut tick =
            tokio::time::interval(std::time::Duration::from_secs(POLL_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }

            // ── Edge-added A/V skew ──
            let skew = flow_stats.active_av_skew_snapshot();
            if let Some(s) = skew {
                if s.mode == "measured" && s.skew_ms.abs() > SKEW_ALERT_MS {
                    skew_over = skew_over.saturating_add(1);
                    if skew_over >= GRACE_POLLS && !skew_alerted {
                        skew_alerted = true;
                        let msg = format!(
                            "Edge-added A/V skew {} ms on flow '{}' exceeds the \
                             EBU R37 ±{} ms limit (audio {} vs video, {} ms of \
                             which is the configured lipsync trim)",
                            s.skew_ms,
                            flow_id,
                            SKEW_ALERT_MS,
                            if s.skew_ms > 0 { "late" } else { "early" },
                            s.lipsync_trim_ms,
                        );
                        tracing::warn!("{msg}");
                        events.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::FLOW,
                            msg,
                            &flow_id,
                            serde_json::json!({
                                "error_code": "av_skew_exceeded",
                                "skew_ms": s.skew_ms,
                                "worst_abs_ms": s.worst_abs_ms,
                                "lipsync_trim_ms": s.lipsync_trim_ms,
                                "threshold_ms": SKEW_ALERT_MS,
                            }),
                        );
                    }
                } else {
                    // Below the alert threshold: any dwell (including the
                    // 30-40 ms hysteresis dead band) breaks the
                    // "consecutive polls" requirement.
                    skew_over = 0;
                }
                if s.mode == "measured" && s.skew_ms.abs() < SKEW_RECOVER_MS {
                    if skew_alerted {
                        skew_alerted = false;
                        events.emit_flow_with_details(
                            EventSeverity::Info,
                            category::FLOW,
                            format!(
                                "Edge-added A/V skew on flow '{}' recovered to {} ms",
                                flow_id, s.skew_ms
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "av_skew_recovered",
                                "skew_ms": s.skew_ms,
                            }),
                        );
                    }
                }
            }

            // ── A/V mux interleave depth ──
            let il_p95 = flow_stats.worst_av_interleave_window_p95_ms();
            if let Some(p95) = il_p95 {
                if p95 > INTERLEAVE_ALERT_MS {
                    il_over = il_over.saturating_add(1);
                    if il_over >= GRACE_POLLS && !il_alerted {
                        il_alerted = true;
                        let msg = format!(
                            "A/V mux interleave p95 {} ms on flow '{}' — not a \
                             lip-sync fault, but receivers buffering less than \
                             this (consumer players default ~1 s) will starve \
                             their audio queue",
                            p95, flow_id,
                        );
                        tracing::warn!("{msg}");
                        events.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::FLOW,
                            msg,
                            &flow_id,
                            serde_json::json!({
                                "error_code": "av_interleave_deep",
                                "window_p95_abs_ms": p95,
                                "threshold_ms": INTERLEAVE_ALERT_MS,
                            }),
                        );
                    }
                } else {
                    il_over = 0;
                }
                if p95 < INTERLEAVE_RECOVER_MS {
                    if il_alerted {
                        il_alerted = false;
                        events.emit_flow_with_details(
                            EventSeverity::Info,
                            category::FLOW,
                            format!(
                                "A/V mux interleave on flow '{}' recovered to p95 {} ms",
                                flow_id, p95
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "av_interleave_recovered",
                                "window_p95_abs_ms": p95,
                            }),
                        );
                    }
                }
            }
        }
    })
}
