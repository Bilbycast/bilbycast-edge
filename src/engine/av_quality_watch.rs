// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-flow A/V quality threshold watcher.
//!
//! Polls three metrics every 5 s and emits transition events
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
//!
//! - **PCR send jitter of a forward-paced video re-encode**
//!   (`OutputStats.pcr_trust`, per output): a UDP/RTP output that
//!   re-encodes video stays on `forward` egress pacing under `auto`
//!   (unless the flow has a bonded input), so its packets leave as the
//!   transcode chain emits them, and its
//!   `pcr_trust` p99 can sit above the 30 ms gate-4 tier-2 limit. Over
//!   it for two polls → Warning `egress_pcr_accuracy_exceeded`, naming
//!   `egress_pacing: "pcr"` as the fix and its latency cost (or, when
//!   the stalls are longer than `pcr` pacing absorbs, saying so).
//!   Recovery below 20 ms, or the output restarting as anything other
//!   than a forward-paced video re-encode → Info
//!   `egress_pcr_accuracy_recovered`. Judged only once the reservoir has
//!   rotated its startup out, so an encoder's open never raises it.
//!   Advisory: it changes no pacing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::EgressPacingMode;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};
use crate::stats::models::{AvSkewStats, PcrTrustStats};
use crate::stats::pcr_trust::PCR_TRUST_RESERVOIR_SIZE;

const POLL_INTERVAL_SECS: u64 = 5;
/// EBU R37 error threshold for edge-added skew.
const SKEW_ALERT_MS: i64 = 40;
const SKEW_RECOVER_MS: i64 = 30;
/// Interleave beyond a consumer player's default caching.
const INTERLEAVE_ALERT_MS: i64 = 1200;
const INTERLEAVE_RECOVER_MS: i64 = 900;
/// Consecutive over-threshold polls before alerting (grace).
const GRACE_POLLS: u32 = 2;
/// PCR send jitter (`pcr_trust` p99) limit: the operational tier 2 of
/// quality gate 4, "p99 ≤ 30 000 µs"
/// (`testbed/BROADCAST_QUALITY_GATES.md`). A reading of exactly 30 ms
/// passes the gate, so only a reading above it counts as over. Not an
/// ETR 290 PCR_AC bound: the sampler measures consecutive-PCR send
/// spacing in whole microseconds at the edge's own socket.
const PCR_AC_ALERT_US: u64 = 30_000;
/// Hysteresis: back under this clears the warning (the 2:3 shape of the
/// skew thresholds). Readings from 20 to 30 ms are a dead band.
const PCR_AC_RECOVER_US: u64 = 20_000;
/// Samples the reservoir must have evicted before its p99 is judged —
/// about 40 s at 25 PCR/s. An encoder's open (a hardware encoder's
/// context creation can take a second) queues the PCR carriers behind
/// it, and they then leave in one batch, each scored as a whole PCR
/// interval of jitter. Judged with those still in the reservoir, a dozen
/// of them would be its p99 at 1024 samples. Once the reservoir has
/// rotated them out, p99 is about the 41st-worst of 4096, which one
/// later burst reaches only if it holds 40 PCRs or more (1.6 s of them at
/// 25 PCR/s).
const PCR_AC_STARTUP_EVICT_SAMPLES: u64 = 1_024;
/// Lifetime samples before the p99 is judged: a full reservoir, with its
/// first [`PCR_AC_STARTUP_EVICT_SAMPLES`] gone.
const PCR_AC_WARMUP_SAMPLES: u64 = PCR_TRUST_RESERVOIR_SIZE as u64 + PCR_AC_STARTUP_EVICT_SAMPLES;
/// `engine::wire_emit::PREROLL_NS` in ms: the queue `pcr` pacing starts
/// with, so the least latency it adds. That constant is private to
/// wire_emit; keep the two in step.
const PCR_PACING_PREROLL_MS: u64 = 50;
/// The longest transcode stall `pcr` pacing rides out:
/// `engine::wire_emit::PCR_ANCHOR_STALE_NS` re-anchors the pacer after
/// 200 ms with no PCR, and `MAX_FUTURE_LOOKAHEAD_NS` caps how far ahead
/// it may run, also 200 ms. Both private to wire_emit; keep in step.
const PCR_PACING_ABSORBS_US: u64 = 200_000;
/// A forward-paced p99 at or above this says the stalls themselves reach
/// what `pcr` pacing absorbs: on `forward` a stall of S reads as about
/// S − ΔPCR, and ΔPCR is at most about 40 ms on the sources that reach
/// this watch. So `pcr` is not expected to bring such an output under the
/// limit, and the warning says to reduce the encoder's load instead.
const PCR_AC_BEYOND_PCR_PACING_US: u64 = PCR_PACING_ABSORBS_US - 40_000;

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

/// Where an output stands for the PCR send-jitter watch, before its
/// reservoir is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PcrAcGate {
    /// No egress pacing mode registered: not a wire-emit UDP/RTP output
    /// (SRT, RIST, RTMP, WebRTC, ST 2110 …), or one caught in the moment
    /// between its stats registering and its pacing resolving.
    NotPaced,
    /// Re-paced (`pcr` / `servo`), or forward-paced and sending with no
    /// video re-encode.
    NotApplicable,
    /// A forward-paced output not yet sending (its transcode chain may
    /// still be building, so whether it re-encodes is not yet known), or
    /// a forward-paced video re-encode whose reservoir still holds its
    /// startup.
    Warming,
    /// A forward-paced video re-encode with a settled reservoir: read it.
    Measure,
}

/// Classify one output from what its stats say. `cumulative_samples` is
/// its PCR sampler's lifetime count (cheap; nothing is sorted).
fn pcr_accuracy_gate(
    pacing: Option<EgressPacingMode>,
    reencodes_video: bool,
    cumulative_samples: u64,
) -> PcrAcGate {
    match pacing {
        None => PcrAcGate::NotPaced,
        Some(EgressPacingMode::Pcr | EgressPacingMode::Servo) => PcrAcGate::NotApplicable,
        // The send loop starts only once the transcode chain is built, so
        // an output with no sample yet may still turn out to re-encode.
        Some(EgressPacingMode::Forward) if cumulative_samples == 0 => PcrAcGate::Warming,
        Some(EgressPacingMode::Forward) if !reencodes_video => PcrAcGate::NotApplicable,
        Some(EgressPacingMode::Forward) if cumulative_samples < PCR_AC_WARMUP_SAMPLES => {
            PcrAcGate::Warming
        }
        Some(EgressPacingMode::Forward) => PcrAcGate::Measure,
    }
}

/// One poll's reading of one output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PcrAcObservation {
    NotPaced,
    NotApplicable,
    Warming,
    Measured { p99_us: u64 },
}

/// What one PCR send-jitter poll changes.
#[derive(Debug, PartialEq, Eq)]
enum PcrAcTransition {
    /// Over the limit for [`GRACE_POLLS`] consecutive polls.
    Exceeded,
    /// Back under [`PCR_AC_RECOVER_US`] after an alert.
    Recovered,
    /// The alerted output is no longer a forward-paced video re-encode
    /// (restarted on `pcr` / `servo`, without its re-encode, or as an
    /// output that is not UDP/RTP at all).
    NoLongerApplies,
}

/// Hysteresis for one output's PCR send jitter: alert after
/// [`GRACE_POLLS`] consecutive polls above [`PCR_AC_ALERT_US`], recover
/// under [`PCR_AC_RECOVER_US`] or when the output stops being one this
/// watch applies to. Latched: no repeat while alerted.
#[derive(Debug, Default)]
struct PcrAccuracyLatch {
    over: u32,
    /// Consecutive [`PcrAcObservation::NotPaced`] polls while alerted —
    /// two, not one, so the moment between an output's stats registering
    /// and its pacing resolving never closes an alert.
    unpaced: u32,
    alerted: bool,
}

impl PcrAccuracyLatch {
    fn observe(&mut self, obs: PcrAcObservation) -> Option<PcrAcTransition> {
        if obs != PcrAcObservation::NotPaced {
            self.unpaced = 0;
        }
        match obs {
            PcrAcObservation::Measured { p99_us } if p99_us > PCR_AC_ALERT_US => {
                self.over = self.over.saturating_add(1);
                if self.over >= GRACE_POLLS && !self.alerted {
                    self.alerted = true;
                    return Some(PcrAcTransition::Exceeded);
                }
                None
            }
            PcrAcObservation::Measured { p99_us } => {
                self.over = 0;
                if self.alerted && p99_us < PCR_AC_RECOVER_US {
                    self.alerted = false;
                    return Some(PcrAcTransition::Recovered);
                }
                None
            }
            PcrAcObservation::Warming => {
                self.over = 0;
                None
            }
            PcrAcObservation::NotApplicable => {
                self.over = 0;
                if self.alerted {
                    self.alerted = false;
                    return Some(PcrAcTransition::NoLongerApplies);
                }
                None
            }
            PcrAcObservation::NotPaced => {
                self.over = 0;
                if !self.alerted {
                    return None;
                }
                self.unpaced = self.unpaced.saturating_add(1);
                if self.unpaced >= GRACE_POLLS {
                    self.unpaced = 0;
                    self.alerted = false;
                    return Some(PcrAcTransition::NoLongerApplies);
                }
                None
            }
        }
    }
}

/// One PCR send-jitter poll over every output of the flow. `latches`
/// lives as long as the flow's watch task. An alerted output's latch
/// outlives the output itself, because an edit restarts an output by
/// removing it and adding it again under the same id: when the operator
/// applies the advice, the restarted output closes the alert.
fn poll_egress_pcr_accuracy(
    flow_id: &str,
    flow_stats: &FlowStatsAccumulator,
    latches: &mut HashMap<String, PcrAccuracyLatch>,
    events: &EventSender,
) {
    // Collected first, so no DashMap shard is held while a sampler's
    // lock (shared with the output's wire thread) is taken.
    let outputs: Vec<(String, Arc<OutputStatsAccumulator>)> = flow_stats
        .output_stats
        .iter()
        .map(|e| (e.key().clone(), Arc::clone(e.value())))
        .collect();
    for (output_id, out) in &outputs {
        let pacing = out.egress_pacing_mode();
        let reencodes_video = out.reencodes_video();
        let alerted = latches.get(output_id).is_some_and(|l| l.alerted);
        // Only a forward-paced video re-encode can raise the warning; any
        // other output is read only to close one it raised before.
        if !alerted && !(pacing == Some(EgressPacingMode::Forward) && reencodes_video) {
            latches.remove(output_id);
            continue;
        }
        let cumulative = if pacing == Some(EgressPacingMode::Forward) {
            out.pcr_trust_sampler().cumulative_samples()
        } else {
            0
        };
        let mut snap = None;
        let obs = match pcr_accuracy_gate(pacing, reencodes_video, cumulative) {
            PcrAcGate::NotPaced => PcrAcObservation::NotPaced,
            PcrAcGate::NotApplicable => PcrAcObservation::NotApplicable,
            PcrAcGate::Warming => PcrAcObservation::Warming,
            PcrAcGate::Measure => {
                snap = out.pcr_trust_sampler().snapshot();
                match &snap {
                    Some(s) => PcrAcObservation::Measured { p99_us: s.p99_us },
                    None => PcrAcObservation::Warming,
                }
            }
        };
        let latch = latches.entry(output_id.clone()).or_default();
        match latch.observe(obs) {
            Some(PcrAcTransition::Exceeded) => {
                if let Some(s) = &snap {
                    emit_pcr_accuracy_exceeded(events, flow_id, output_id, out, s);
                }
            }
            Some(PcrAcTransition::Recovered) => {
                emit_pcr_accuracy_recovered(events, flow_id, output_id, out, snap.as_ref(), false);
            }
            Some(PcrAcTransition::NoLongerApplies) => {
                // A one-off read for the record; nothing is judged on it.
                let s = out.pcr_trust_sampler().snapshot();
                emit_pcr_accuracy_recovered(events, flow_id, output_id, out, s.as_ref(), true);
            }
            None => {}
        }
    }
    let present: HashSet<&str> = outputs.iter().map(|(id, _)| id.as_str()).collect();
    latches.retain(|id, l| l.alerted || present.contains(id.as_str()));
}

fn emit_pcr_accuracy_exceeded(
    events: &EventSender,
    flow_id: &str,
    output_id: &str,
    out: &OutputStatsAccumulator,
    s: &PcrTrustStats,
) {
    let pcr_reaches_limit = s.p99_us < PCR_AC_BEYOND_PCR_PACING_US;
    // The message is constant per output (per variant): the manager
    // coalesces a repeat into one row with a count. The numbers ride in
    // `details` and the log line.
    let msg = if pcr_reaches_limit {
        format!(
            "Output '{output_id}' (flow '{flow_id}'): PCR send jitter (pcr_trust p99) is above \
             the 30 ms gate-4 tier-2 limit. The output re-encodes video and is forward-paced, \
             so its packets leave as the transcode chain emits them; egress_pacing \"pcr\" \
             re-paces them onto the PCR timeline, at the cost of added egress latency (at \
             least 50 ms, plus up to the longest transcode stall it absorbs, at most about \
             200 ms)"
        )
    } else {
        format!(
            "Output '{output_id}' (flow '{flow_id}'): PCR send jitter (pcr_trust p99) is above \
             the 30 ms gate-4 tier-2 limit, and the output's transcode chain stalls for about \
             200 ms or longer, past what egress_pacing \"pcr\" absorbs, so pcr pacing alone is \
             not expected to bring it under the limit. Reduce the encoder's load first (a \
             faster preset, a lower resolution or a hardware encoder)"
        )
    };
    let pacing = out.egress_pacing_effective.get().cloned();
    let tier = out.wire_pacing_tier.get().cloned();
    tracing::warn!(
        "{msg} [p99 {} us, p95 {} us, max {} us over {} samples; pacing {}, tier {}]",
        s.p99_us,
        s.p95_us,
        s.max_us,
        s.samples,
        pacing.as_deref().unwrap_or("-"),
        tier.as_deref().unwrap_or("-"),
    );
    let transcode_pcr = out.transcode_pcr_snapshot().map(|t| {
        serde_json::json!({
            "offset_ms": t.offset_ms,
            "late_frames": t.late_frames,
            "offset_raises": t.offset_raises,
        })
    });
    events.emit_output_with_details(
        EventSeverity::Warning,
        category::FLOW,
        msg,
        output_id,
        serde_json::json!({
            "error_code": "egress_pcr_accuracy_exceeded",
            "output_id": output_id,
            "flow_id": flow_id,
            "egress_pacing_effective": pacing,
            "wire_pacing_tier": tier,
            "p99_us": s.p99_us,
            "p95_us": s.p95_us,
            "max_us": s.max_us,
            "samples": s.samples,
            "threshold_us": PCR_AC_ALERT_US,
            "recover_below_us": PCR_AC_RECOVER_US,
            "pcr_expected_to_reach_limit": pcr_reaches_limit,
            "suggested_egress_pacing": if pcr_reaches_limit { Some("pcr") } else { None },
            "estimated_added_latency_ms":
                PCR_PACING_PREROLL_MS + s.max_us.min(PCR_PACING_ABSORBS_US) / 1_000,
            "transcode_pcr": transcode_pcr,
        }),
    );
}

fn emit_pcr_accuracy_recovered(
    events: &EventSender,
    flow_id: &str,
    output_id: &str,
    out: &OutputStatsAccumulator,
    s: Option<&PcrTrustStats>,
    no_longer_applies: bool,
) {
    let (reason, msg) = if no_longer_applies {
        (
            "no_longer_applies",
            format!(
                "Output '{output_id}' (flow '{flow_id}'): no longer a forward-paced video \
                 re-encode, so its PCR send jitter warning is cleared. On egress_pacing \"pcr\", \
                 watch its PCR p99 against the 30 ms limit, and its latency and egress_shed as \
                 well: pcr pacing can add latency over time that p99 does not show"
            ),
        )
    } else {
        (
            "within_limit",
            format!(
                "Output '{output_id}' (flow '{flow_id}'): PCR send jitter (pcr_trust p99) is \
                 back under 20 ms"
            ),
        )
    };
    events.emit_output_with_details(
        EventSeverity::Info,
        category::FLOW,
        msg,
        output_id,
        serde_json::json!({
            "error_code": "egress_pcr_accuracy_recovered",
            "output_id": output_id,
            "flow_id": flow_id,
            "reason": reason,
            "egress_pacing_effective": out.egress_pacing_effective.get(),
            "p99_us": s.map(|s| s.p99_us),
            "samples": s.map(|s| s.samples),
        }),
    );
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
        let mut pcr_accuracy: HashMap<String, PcrAccuracyLatch> = HashMap::new();
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

            // ── PCR send jitter of each forward-paced video re-encode ──
            poll_egress_pcr_accuracy(&flow_id, &flow_stats, &mut pcr_accuracy, &events);
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

    // ── PCR send jitter of forward-paced video re-encodes ──

    use crate::manager::events::Event;
    use tokio::sync::mpsc::Receiver;

    fn measured(ms: u64) -> PcrAcObservation {
        PcrAcObservation::Measured { p99_us: ms * 1_000 }
    }

    #[test]
    fn pcr_accuracy_latch_alerts_after_two_polls_and_recovers_under_20_ms() {
        let mut l = PcrAccuracyLatch::default();
        assert_eq!(l.observe(measured(40)), None, "one poll is grace");
        assert_eq!(l.observe(measured(40)), Some(PcrAcTransition::Exceeded));
        assert_eq!(l.observe(measured(40)), None, "latched: no repeat");
        assert_eq!(l.observe(measured(25)), None, "dead band");
        assert_eq!(l.observe(measured(19)), Some(PcrAcTransition::Recovered));
        assert_eq!(l.observe(measured(31)), None, "grace again");
        assert_eq!(l.observe(measured(31)), Some(PcrAcTransition::Exceeded));
        // Exactly 30 ms passes the gate, so it never counts as over.
        let mut l = PcrAccuracyLatch::default();
        for _ in 0..5 {
            assert_eq!(l.observe(PcrAcObservation::Measured { p99_us: PCR_AC_ALERT_US }), None);
        }
        // A reading hovering on the limit never alerts: over only on
        // consecutive polls.
        let mut l = PcrAccuracyLatch::default();
        for _ in 0..10 {
            assert_eq!(l.observe(PcrAcObservation::Measured { p99_us: 30_100 }), None);
            assert_eq!(l.observe(PcrAcObservation::Measured { p99_us: 29_900 }), None);
        }
    }

    #[test]
    fn pcr_accuracy_latch_warming_not_applicable_and_not_paced() {
        let mut l = PcrAccuracyLatch::default();
        for _ in 0..5 {
            assert_eq!(l.observe(PcrAcObservation::Warming), None);
        }
        // Warming breaks the run.
        assert_eq!(l.observe(measured(40)), None);
        assert_eq!(l.observe(PcrAcObservation::Warming), None);
        assert_eq!(l.observe(measured(40)), None);
        // Neither closes anything when nothing is alerted.
        let mut l = PcrAccuracyLatch::default();
        assert_eq!(l.observe(PcrAcObservation::NotApplicable), None);
        assert_eq!(l.observe(PcrAcObservation::NotPaced), None);
        assert_eq!(l.observe(PcrAcObservation::NotPaced), None);
        // Once alerted, NotApplicable closes it once.
        l.observe(measured(40));
        assert_eq!(l.observe(measured(40)), Some(PcrAcTransition::Exceeded));
        assert_eq!(l.observe(PcrAcObservation::NotApplicable), Some(PcrAcTransition::NoLongerApplies));
        assert_eq!(l.observe(PcrAcObservation::NotApplicable), None);
        // NotPaced closes it on the second consecutive poll only: one
        // could be the instant between registering and resolving.
        l.observe(measured(40));
        assert_eq!(l.observe(measured(40)), Some(PcrAcTransition::Exceeded));
        assert_eq!(l.observe(PcrAcObservation::NotPaced), None);
        assert_eq!(l.observe(PcrAcObservation::Warming), None, "resolved after all");
        assert_eq!(l.observe(PcrAcObservation::NotPaced), None, "the count restarted");
        assert_eq!(l.observe(PcrAcObservation::NotPaced), Some(PcrAcTransition::NoLongerApplies));
        assert_eq!(l.observe(PcrAcObservation::NotPaced), None);
    }

    #[test]
    fn pcr_accuracy_gate_table() {
        use EgressPacingMode::*;
        let full = PCR_AC_WARMUP_SAMPLES;
        for video in [false, true] {
            assert_eq!(pcr_accuracy_gate(None, video, full), PcrAcGate::NotPaced);
            assert_eq!(pcr_accuracy_gate(Some(Pcr), video, full), PcrAcGate::NotApplicable);
            assert_eq!(pcr_accuracy_gate(Some(Servo), video, 0), PcrAcGate::NotApplicable);
            // Not sending yet: whether it re-encodes is not settled.
            assert_eq!(pcr_accuracy_gate(Some(Forward), video, 0), PcrAcGate::Warming);
        }
        assert_eq!(pcr_accuracy_gate(Some(Forward), false, 1), PcrAcGate::NotApplicable);
        assert_eq!(pcr_accuracy_gate(Some(Forward), false, full), PcrAcGate::NotApplicable);
        assert_eq!(pcr_accuracy_gate(Some(Forward), true, 1), PcrAcGate::Warming);
        // A full reservoir still holding its startup is not judged.
        assert_eq!(
            pcr_accuracy_gate(Some(Forward), true, PCR_TRUST_RESERVOIR_SIZE as u64),
            PcrAcGate::Warming
        );
        assert_eq!(pcr_accuracy_gate(Some(Forward), true, full - 1), PcrAcGate::Warming);
        assert_eq!(pcr_accuracy_gate(Some(Forward), true, full), PcrAcGate::Measure);
    }

    /// Feed `n` PCR sends at a 40 ms PCR spacing; every 10th leaves
    /// `late_ms` late against its PCR (so p99 = `late_ms`), the rest on
    /// time. `late_ms = 0` is a clean feed. `t` carries the (pcr, wall)
    /// position across calls; `None` primes the sampler first.
    fn feed(out: &OutputStatsAccumulator, t: &mut Option<(u64, u64)>, n: u64, late_ms: u64) {
        let (mut pcr, mut wall) = t.unwrap_or_else(|| {
            out.record_pcr_egress(0, 0);
            (0, 0)
        });
        for i in 0..n {
            pcr += 40 * 27_000;
            wall += 40_000 + if i % 10 == 9 { late_ms * 1_000 } else { 0 };
            out.record_pcr_egress(pcr, wall);
        }
        *t = Some((pcr, wall));
    }

    fn drain(rx: &mut Receiver<Event>) -> Vec<Event> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }

    fn video(out: &OutputStatsAccumulator) {
        out.set_video_encode_stats(
            Arc::new(crate::engine::ts_video_replace::VideoEncodeStats::default()),
            "h264",
            "h264",
            1920,
            1080,
            25.0,
            5_000,
            "x264",
        );
    }

    fn register(
        flow: &FlowStatsAccumulator,
        id: &str,
        ty: &str,
        pacing: Option<(EgressPacingMode, bool)>,
        reencodes_video: bool,
    ) -> Arc<OutputStatsAccumulator> {
        let o = flow.register_output(id.into(), id.into(), ty.into());
        if let Some((mode, auto)) = pacing {
            o.set_egress_pacing_effective(mode, auto);
        }
        if reencodes_video {
            video(&o);
        }
        o
    }

    fn details(e: &Event) -> &serde_json::Value {
        e.details.as_ref().expect("details")
    }

    /// The warning fires for a forward-paced (auto or explicit) video
    /// re-encode over the limit, and for nothing else: not a re-paced
    /// (explicit pcr) output, not an audio-only transcode, not a
    /// passthrough, not an SRT output. Once each, latched.
    #[test]
    fn poll_raises_one_warning_per_forward_paced_video_transcode() {
        use EgressPacingMode::*;
        let flow = FlowStatsAccumulator::new("f".into(), "flow".into(), "udp".into());
        let (events, mut rx) = crate::manager::events::event_channel();
        let outs = [
            register(&flow, "tx", "udp", Some((Forward, true)), true),
            register(&flow, "explicit", "rtp", Some((Forward, false)), true),
            register(&flow, "pass", "udp", Some((Forward, true)), false),
            register(&flow, "paced", "rtp", Some((Pcr, false)), true),
            register(&flow, "srt", "srt", None, true),
        ];
        let aonly = register(&flow, "aonly", "udp", Some((Forward, true)), false);
        aonly.set_encode_stats(
            Arc::new(crate::engine::audio_encode::EncodeStats::default()),
            "aac_lc",
            48_000,
            2,
            128,
        );
        let mut latches = HashMap::new();
        for o in outs.iter().chain(std::iter::once(&aonly)) {
            feed(o, &mut None, PCR_AC_WARMUP_SAMPLES + 10, 40);
        }
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        assert!(drain(&mut rx).is_empty(), "one poll is grace");
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        let mut got = drain(&mut rx);
        got.sort_by(|a, b| a.output_id.cmp(&b.output_id));
        assert_eq!(got.len(), 2, "{got:?}");
        for (e, (id, label)) in got.iter().zip([("explicit", "forward"), ("tx", "auto (forward)")]) {
            assert_eq!(e.output_id.as_deref(), Some(id));
            assert_eq!(e.flow_id, None, "output-scoped; the flow rides in details");
            assert_eq!(e.severity, EventSeverity::Warning);
            assert_eq!(e.category, category::FLOW);
            let d = details(e);
            assert_eq!(d["error_code"], "egress_pcr_accuracy_exceeded");
            assert_eq!(d["flow_id"], "f");
            assert_eq!(d["egress_pacing_effective"], label);
            assert_eq!(d["suggested_egress_pacing"], "pcr");
            assert_eq!(d["pcr_expected_to_reach_limit"], true);
            assert_eq!(d["threshold_us"], 30_000);
            assert_eq!(d["recover_below_us"], 20_000);
            assert!(d["p99_us"].as_u64().unwrap() > 30_000, "{d}");
            assert_eq!(d["samples"], PCR_TRUST_RESERVOIR_SIZE as u64);
            assert_eq!(d["estimated_added_latency_ms"], 90, "50 ms preroll + the 40 ms worst");
            assert!(e.message.contains("egress_pacing \"pcr\""), "{}", e.message);
            assert!(!e.message.contains("40"), "numbers stay out of the message: {}", e.message);
        }
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        assert!(drain(&mut rx).is_empty(), "latched");
        assert_eq!(latches.len(), 2, "only the two candidates keep a latch");
    }

    /// A dozen PCRs leaving in one batch at the start (an encoder's open)
    /// never alert: the reservoir is judged only once they are evicted.
    #[test]
    fn a_startup_burst_never_alerts() {
        let flow = FlowStatsAccumulator::new("f".into(), "flow".into(), "udp".into());
        let (events, mut rx) = crate::manager::events::event_channel();
        let o = register(&flow, "tx", "udp", Some((EgressPacingMode::Forward, true)), true);
        let mut latches = HashMap::new();
        // 24 PCRs queued behind a ~1 s open leave together: Δwall 0
        // against ΔPCR 40 ms.
        o.record_pcr_egress(0, 0);
        let mut pcr = 0u64;
        for _ in 0..24 {
            pcr += 40 * 27_000;
            o.record_pcr_egress(pcr, 1);
        }
        let mut t = Some((pcr, 1u64));
        for _ in 0..12 {
            feed(&o, &mut t, PCR_AC_WARMUP_SAMPLES / 10, 0);
            poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        }
        assert!(o.pcr_trust_sampler().cumulative_samples() > PCR_AC_WARMUP_SAMPLES);
        assert!(drain(&mut rx).is_empty());
    }

    /// The operator follows the advice: the edit restarts the output
    /// (removed, then added under the same id) on pcr pacing, and the
    /// restarted output closes the alert.
    #[test]
    fn poll_clears_the_alert_when_the_output_restarts_pcr_paced() {
        use EgressPacingMode::*;
        let flow = FlowStatsAccumulator::new("f".into(), "flow".into(), "udp".into());
        let (events, mut rx) = crate::manager::events::event_channel();
        let tx = register(&flow, "tx", "udp", Some((Forward, true)), true);
        let other = register(&flow, "other", "udp", Some((Forward, true)), true);
        let mut latches = HashMap::new();
        feed(&tx, &mut None, PCR_AC_WARMUP_SAMPLES, 40);
        feed(&other, &mut None, 10, 0);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        assert_eq!(drain(&mut rx).len(), 1);

        flow.unregister_output("tx");
        flow.unregister_output("other");
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        assert!(drain(&mut rx).is_empty(), "absent: nothing to say yet");
        assert!(latches.get("tx").is_some_and(|l| l.alerted), "the alerted latch survives");
        assert!(!latches.contains_key("other"), "an output gone for good leaves nothing");

        let tx = register(&flow, "tx", "udp", Some((Pcr, false)), true);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].severity, EventSeverity::Info);
        assert_eq!(got[0].output_id.as_deref(), Some("tx"));
        let d = details(&got[0]);
        assert_eq!(d["error_code"], "egress_pcr_accuracy_recovered");
        assert_eq!(d["reason"], "no_longer_applies");
        assert_eq!(d["egress_pacing_effective"], "pcr");
        assert!(d["p99_us"].is_null(), "nothing sent yet");
        assert!(got[0].message.contains("egress_shed"), "{}", got[0].message);
        feed(&tx, &mut None, 50, 40);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        assert!(drain(&mut rx).is_empty());
        assert!(latches.is_empty(), "a pcr-paced output is not watched");
    }

    /// An alerted output re-added under its id as a different type (no
    /// pacing) closes the alert after two polls.
    #[test]
    fn poll_clears_the_alert_when_the_output_comes_back_unpaced() {
        let flow = FlowStatsAccumulator::new("f".into(), "flow".into(), "udp".into());
        let (events, mut rx) = crate::manager::events::event_channel();
        let tx = register(&flow, "tx", "udp", Some((EgressPacingMode::Forward, true)), true);
        let mut latches = HashMap::new();
        feed(&tx, &mut None, PCR_AC_WARMUP_SAMPLES, 40);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        assert_eq!(drain(&mut rx).len(), 1);
        flow.unregister_output("tx");
        register(&flow, "tx", "srt", None, true);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        assert!(drain(&mut rx).is_empty(), "one unpaced poll could be a spawn in progress");
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1);
        assert_eq!(details(&got[0])["reason"], "no_longer_applies");
        assert!(details(&got[0])["egress_pacing_effective"].is_null());
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        assert!(drain(&mut rx).is_empty());
        assert!(latches.is_empty());
    }

    /// Back under 20 ms recovers; a second excursion warns again with a
    /// byte-identical message, so the manager coalesces the two.
    #[test]
    fn poll_recovers_within_limit_and_messages_are_constant() {
        let flow = FlowStatsAccumulator::new("f".into(), "flow".into(), "udp".into());
        let (events, mut rx) = crate::manager::events::event_channel();
        let tx = register(&flow, "tx", "udp", Some((EgressPacingMode::Forward, true)), true);
        let mut latches = HashMap::new();
        let mut t = None;
        feed(&tx, &mut t, PCR_AC_WARMUP_SAMPLES, 40);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        let first = drain(&mut rx);
        assert_eq!(first.len(), 1);

        feed(&tx, &mut t, PCR_TRUST_RESERVOIR_SIZE as u64, 0);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].severity, EventSeverity::Info);
        let d = details(&got[0]);
        assert_eq!(d["reason"], "within_limit");
        assert_eq!(d["p99_us"], 0);

        feed(&tx, &mut t, PCR_TRUST_RESERVOIR_SIZE as u64, 60);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        let second = drain(&mut rx);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].message, first[0].message, "constant message coalesces");
        assert_ne!(details(&second[0])["p99_us"], details(&first[0])["p99_us"]);
    }

    /// Stalls past what pcr pacing absorbs get the other message: pcr is
    /// not offered as the fix.
    #[test]
    fn stalls_beyond_pcr_pacing_say_so() {
        let flow = FlowStatsAccumulator::new("f".into(), "flow".into(), "udp".into());
        let (events, mut rx) = crate::manager::events::event_channel();
        let tx = register(&flow, "tx", "udp", Some((EgressPacingMode::Forward, true)), true);
        let mut latches = HashMap::new();
        feed(&tx, &mut None, PCR_AC_WARMUP_SAMPLES, 230);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        poll_egress_pcr_accuracy("f", &flow, &mut latches, &events);
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1);
        let d = details(&got[0]);
        assert_eq!(d["error_code"], "egress_pcr_accuracy_exceeded");
        assert_eq!(d["pcr_expected_to_reach_limit"], false);
        assert!(d["suggested_egress_pacing"].is_null());
        assert_eq!(d["estimated_added_latency_ms"], 250, "capped at what pcr absorbs");
        assert!(got[0].message.contains("Reduce the encoder's load"), "{}", got[0].message);
    }
}
