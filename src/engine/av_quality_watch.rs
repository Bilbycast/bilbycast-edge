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
//!   "av_sync" metric pretended to be. Watched on the active input's
//!   path (`FlowStats.av_skew`, flow-scoped event) **and on every
//!   output that re-encodes** (`OutputStats.av_skew`, output-scoped
//!   event with `output_id` in its details), each latched on its own so
//!   one bad output never masks another's recovery.
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
use crate::stats::models::AvSkewStats;

const POLL_INTERVAL_SECS: u64 = 5;
/// EBU R37 error threshold for edge-added skew.
const SKEW_ALERT_MS: i64 = 40;
const SKEW_RECOVER_MS: i64 = 30;
/// Interleave beyond a consumer player's default caching.
const INTERLEAVE_ALERT_MS: i64 = 1200;
const INTERLEAVE_RECOVER_MS: i64 = 900;
/// Consecutive over-threshold polls before alerting (grace).
const GRACE_POLLS: u32 = 2;

/// What one skew poll changes.
#[derive(Debug, PartialEq, Eq)]
enum SkewTransition {
    /// Over the limit for [`GRACE_POLLS`] consecutive polls.
    Exceeded,
    /// Back under the recovery threshold after an alert.
    Recovered,
}

/// Hysteresis for one skew reporter: alert after [`GRACE_POLLS`]
/// consecutive polls beyond ±[`SKEW_ALERT_MS`], recover under
/// ±[`SKEW_RECOVER_MS`].
#[derive(Debug, Default)]
struct SkewLatch {
    over: u32,
    alerted: bool,
}

impl SkewLatch {
    fn observe(&mut self, s: &AvSkewStats) -> Option<SkewTransition> {
        let measured = s.mode == "measured";
        if measured && s.skew_ms.abs() > SKEW_ALERT_MS {
            self.over = self.over.saturating_add(1);
            if self.over >= GRACE_POLLS && !self.alerted {
                self.alerted = true;
                return Some(SkewTransition::Exceeded);
            }
        } else {
            // Below the alert threshold: any dwell (including the 30-40 ms
            // hysteresis dead band) breaks the "consecutive polls"
            // requirement.
            self.over = 0;
        }
        if measured && s.skew_ms.abs() < SKEW_RECOVER_MS && self.alerted {
            self.alerted = false;
            return Some(SkewTransition::Recovered);
        }
        None
    }
}

fn skew_direction(s: &AvSkewStats) -> &'static str {
    if s.skew_ms > 0 { "late" } else { "early" }
}

pub fn spawn_av_quality_watch(
    flow_id: String,
    flow_stats: Arc<FlowStatsAccumulator>,
    events: EventSender,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut flow_skew = SkewLatch::default();
        let mut output_skew: std::collections::HashMap<String, SkewLatch> =
            std::collections::HashMap::new();
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

            // ── Edge-added A/V skew: the active input's path ──
            if let Some(s) = flow_stats.active_av_skew_snapshot() {
                match flow_skew.observe(&s) {
                    Some(SkewTransition::Exceeded) => {
                        let msg = format!(
                            "Edge-added A/V skew {} ms on flow '{}' exceeds the \
                             EBU R37 ±{} ms limit (audio {} vs video, {} ms of \
                             which is the configured lipsync trim)",
                            s.skew_ms,
                            flow_id,
                            SKEW_ALERT_MS,
                            skew_direction(&s),
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
                    Some(SkewTransition::Recovered) => {
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
                    None => {}
                }
            }

            // ── Edge-added A/V skew: each re-encoding output ──
            let outputs = flow_stats.output_av_skew_snapshots();
            output_skew.retain(|id, _| outputs.iter().any(|(o, _)| o == id));
            for (output_id, s) in &outputs {
                let latch = output_skew.entry(output_id.clone()).or_default();
                match latch.observe(s) {
                    Some(SkewTransition::Exceeded) => {
                        let msg = format!(
                            "Edge-added A/V skew {} ms on output '{}' (flow '{}') exceeds \
                             the EBU R37 ±{} ms limit (audio {} vs video): the output's \
                             own transcode shifted its audio against its video",
                            s.skew_ms,
                            output_id,
                            flow_id,
                            SKEW_ALERT_MS,
                            skew_direction(s),
                        );
                        tracing::warn!("{msg}");
                        events.emit_output_with_details(
                            EventSeverity::Warning,
                            category::FLOW,
                            msg,
                            output_id,
                            serde_json::json!({
                                "error_code": "av_skew_exceeded",
                                "output_id": output_id,
                                "flow_id": flow_id,
                                "skew_ms": s.skew_ms,
                                "worst_abs_ms": s.worst_abs_ms,
                                "threshold_ms": SKEW_ALERT_MS,
                            }),
                        );
                    }
                    Some(SkewTransition::Recovered) => {
                        events.emit_output_with_details(
                            EventSeverity::Info,
                            category::FLOW,
                            format!(
                                "Edge-added A/V skew on output '{}' (flow '{}') recovered to {} ms",
                                output_id, flow_id, s.skew_ms
                            ),
                            output_id,
                            serde_json::json!({
                                "error_code": "av_skew_recovered",
                                "output_id": output_id,
                                "flow_id": flow_id,
                                "skew_ms": s.skew_ms,
                            }),
                        );
                    }
                    None => {}
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
                if p95 < INTERLEAVE_RECOVER_MS
                    && il_alerted {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skew(ms: i64) -> AvSkewStats {
        AvSkewStats { skew_ms: ms, worst_abs_ms: ms.abs(), lipsync_trim_ms: 0, mode: "measured".into() }
    }

    #[test]
    fn a_skew_latch_alerts_after_two_polls_and_recovers_below_30_ms() {
        let mut l = SkewLatch::default();
        assert_eq!(l.observe(&skew(64)), None, "one poll is grace");
        assert_eq!(l.observe(&skew(64)), Some(SkewTransition::Exceeded));
        assert_eq!(l.observe(&skew(64)), None, "latched");
        assert_eq!(l.observe(&skew(35)), None, "inside the hysteresis band");
        assert_eq!(l.observe(&skew(-5)), Some(SkewTransition::Recovered));
        assert_eq!(l.observe(&skew(-5)), None);
        // A passthrough reporter never alarms.
        let mut p = SkewLatch::default();
        let mut pass = skew(100);
        pass.mode = "passthrough".into();
        assert_eq!(p.observe(&pass), None);
        assert_eq!(p.observe(&pass), None);
    }

    /// Each re-encoding output's own reporter is visible to the watcher
    /// (before, only the active input's was, so an output transcode 64 ms
    /// off never alarmed).
    #[test]
    fn output_skew_reporters_are_enumerated() {
        let flow = FlowStatsAccumulator::new("f".into(), "flow".into(), "udp".into());
        let a = flow.register_output("out-a".into(), "a".into(), "udp".into());
        let _b = flow.register_output("out-b".into(), "b".into(), "udp".into());
        let rep = std::sync::Arc::new(crate::stats::av_skew::AvSkewReporter::new());
        a.set_av_skew_reporter(rep.clone());
        rep.set_audio_delta(64 * 90);
        let got = flow.output_av_skew_snapshots();
        assert_eq!(got.len(), 1, "only outputs that re-encode report");
        assert_eq!(got[0].0, "out-a");
        assert_eq!(got[0].1.skew_ms, 64);
        let mut latch = SkewLatch::default();
        latch.observe(&got[0].1);
        assert_eq!(latch.observe(&got[0].1), Some(SkewTransition::Exceeded));
    }
}
