// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Event sender for forwarding operational events to the manager.
//!
//! Components throughout the edge (flow engine, SRT, tunnels, etc.) hold an
//! `EventSender` clone and call its helper methods to report state changes.
//! Events are queued in a bounded mpsc channel and drained by the manager
//! WebSocket client loop.
//!
//! When not connected to the manager, events accumulate up to
//! [`EVENT_CHANNEL_CAPACITY`] and then start dropping with tail-drop semantics
//! (oldest events stay; newest are discarded). A sampled `tracing::warn!` is
//! emitted at most once per second carrying the running drop count, so a
//! sustained event storm does not spam the log. Sending never blocks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use bonding_transport::{PathDeadReason, PathEvent, PathEventKind, PathId};

/// Capacity of the manager event channel. Events are infrequent relative to
/// stats; this size lets a manager outage of several minutes pass without
/// drops at typical event rates, while bounding worst-case memory under a
/// pathological error storm + sustained manager disconnect.
pub const EVENT_CHANNEL_CAPACITY: usize = 4096;

/// Event severity levels matching the manager's `EventSeverity` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSeverity {
    Info,
    Warning,
    Critical,
}

impl EventSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// A single event to be sent to the manager.
#[derive(Debug, Clone)]
pub struct Event {
    pub severity: EventSeverity,
    pub category: String,
    pub message: String,
    pub details: Option<serde_json::Value>,
    pub flow_id: Option<String>,
    pub input_id: Option<String>,
    pub output_id: Option<String>,
}

/// Snapshot of a recent critical event captured by the in-process recent-event
/// tracker. Used by command handlers (`create_flow`, `update_flow`,
/// `update_input`, `update_output`) to surface a runtime port-conflict or
/// bind-failure that happened just after the spawn returned `Ok` — so the
/// `command_ack` reflects the real outcome.
#[derive(Debug, Clone)]
pub struct RecentCritical {
    pub event: Event,
    pub error_code: Option<String>,
    pub captured_at: Instant,
}

/// Per-scope ring of the most recent critical event keyed by flow id.
/// Old entries are evicted lazily when a fresher one is pushed for the same
/// flow — there is no background sweeper, since the only consumer (command
/// handler post-spawn polling) clears its own scope on read.
#[derive(Debug, Default)]
struct RecentCriticalTracker {
    by_flow: std::sync::RwLock<HashMap<String, RecentCritical>>,
}

/// Clonable handle for sending events from any component.
///
/// Sending never blocks. If the receiver is dropped (manager client not
/// running) or the bounded channel is full, events are silently discarded —
/// channel-full events bump a sampled drop counter that emits one
/// `tracing::warn!` per second so a storm is visible without log spam.
#[derive(Debug, Clone)]
pub struct EventSender {
    tx: mpsc::Sender<Event>,
    recent: std::sync::Arc<RecentCriticalTracker>,
    drop_state: std::sync::Arc<EventDropState>,
}

/// Sampled drop accounting for the bounded event channel.
#[derive(Debug, Default)]
struct EventDropState {
    /// Total events dropped because the channel was full.
    total_dropped: AtomicU64,
    /// Drops accumulated since the last warning was logged.
    pending_dropped: AtomicU64,
    /// Microsecond timestamp of the last warning, used to rate-limit logs to
    /// at most one per second.
    last_warn_us: AtomicU64,
}

impl EventSender {
    /// Send an event to the manager. Critical events are also stashed in the
    /// in-process recent-event tracker so command handlers can poll for the
    /// outcome of a freshly spawned input/output.
    pub fn send(&self, event: Event) {
        // Stash flow-scoped Critical events so the WS command handler can
        // poll for a runtime bind failure that happened just after spawn.
        // We only track the flow scope today — the input/output scope would
        // need an eviction policy and isn't read by anything yet.
        if event.severity == EventSeverity::Critical
            && let Some(fid) = &event.flow_id
        {
            let code = event
                .details
                .as_ref()
                .and_then(|d| d.get("error_code").and_then(|v| v.as_str()).map(String::from));
            let entry = RecentCritical {
                event: event.clone(),
                error_code: code,
                captured_at: Instant::now(),
            };
            if let Ok(mut g) = self.recent.by_flow.write() {
                g.insert(fid.clone(), entry);
            }
        }
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(event) {
            self.drop_state.note_dropped();
        }
    }

    /// Consume and return the most recent critical event for `flow_id` whose
    /// `captured_at >= since`. Used by command handlers to wait for the result
    /// of a spawn that races the runtime bind path.
    pub fn take_recent_critical_for_flow(&self, flow_id: &str, since: Instant) -> Option<RecentCritical> {
        let mut g = self.recent.by_flow.write().ok()?;
        if let Some(entry) = g.get(flow_id).cloned()
            && entry.captured_at >= since
        {
            g.remove(flow_id);
            return Some(entry);
        }
        None
    }


    /// Convenience: send an event with just severity, category, and message.
    pub fn emit(&self, severity: EventSeverity, category: &str, message: impl Into<String>) {
        self.send(Event {
            severity,
            category: category.to_string(),
            message: message.into(),
            details: None,
            flow_id: None,
            input_id: None,
            output_id: None,
        });
    }

    /// Convenience: send a flow-scoped event.
    pub fn emit_flow(
        &self,
        severity: EventSeverity,
        category: &str,
        message: impl Into<String>,
        flow_id: &str,
    ) {
        self.send(Event {
            severity,
            category: category.to_string(),
            message: message.into(),
            details: None,
            flow_id: Some(flow_id.to_string()),
            input_id: None,
            output_id: None,
        });
    }

    /// Convenience: send an input-scoped event.
    pub fn emit_input(
        &self,
        severity: EventSeverity,
        category: &str,
        message: impl Into<String>,
        input_id: &str,
    ) {
        self.send(Event {
            severity,
            category: category.to_string(),
            message: message.into(),
            details: None,
            flow_id: None,
            input_id: Some(input_id.to_string()),
            output_id: None,
        });
    }

    /// Convenience: send an output-scoped event.
    #[allow(dead_code)]
    pub fn emit_output(
        &self,
        severity: EventSeverity,
        category: &str,
        message: impl Into<String>,
        output_id: &str,
    ) {
        self.send(Event {
            severity,
            category: category.to_string(),
            message: message.into(),
            details: None,
            flow_id: None,
            input_id: None,
            output_id: Some(output_id.to_string()),
        });
    }

    /// Convenience: send an input-scoped event with structured details.
    #[allow(dead_code)]
    pub fn emit_input_with_details(
        &self,
        severity: EventSeverity,
        category: &str,
        message: impl Into<String>,
        input_id: &str,
        details: serde_json::Value,
    ) {
        self.send(Event {
            severity,
            category: category.to_string(),
            message: message.into(),
            details: Some(details),
            flow_id: None,
            input_id: Some(input_id.to_string()),
            output_id: None,
        });
    }

    /// Convenience: send an output-scoped event with structured details.
    #[allow(dead_code)]
    pub fn emit_output_with_details(
        &self,
        severity: EventSeverity,
        category: &str,
        message: impl Into<String>,
        output_id: &str,
        details: serde_json::Value,
    ) {
        self.send(Event {
            severity,
            category: category.to_string(),
            message: message.into(),
            details: Some(details),
            flow_id: None,
            input_id: None,
            output_id: Some(output_id.to_string()),
        });
    }

    /// Convenience: send a flow-scoped event with structured details.
    pub fn emit_flow_with_details(
        &self,
        severity: EventSeverity,
        category: &str,
        message: impl Into<String>,
        flow_id: &str,
        details: serde_json::Value,
    ) {
        self.emit_with_details(severity, category, message, Some(flow_id), details);
    }

    /// Send an event with structured details and an optional flow scope.
    /// Used by ST 2110 producers (PTP, network leg, SCTE-104) where the
    /// flow_id may or may not be known at emission time.
    pub fn emit_with_details(
        &self,
        severity: EventSeverity,
        category: &str,
        message: impl Into<String>,
        flow_id: Option<&str>,
        details: serde_json::Value,
    ) {
        self.send(Event {
            severity,
            category: category.to_string(),
            message: message.into(),
            details: Some(details),
            flow_id: flow_id.map(|s| s.to_string()),
            input_id: None,
            output_id: None,
        });
    }

    /// Emit a Critical `port_conflict` event (OS-level "address already in use"
    /// at runtime, or an internal collision detected at bind time). Carries a
    /// stable `error_code = "port_conflict"` in `details` so the manager UI
    /// can attribute and highlight the offending field.
    pub fn emit_port_conflict(
        &self,
        component: &str,
        addr: impl std::fmt::Display,
        proto: BindProto,
        scope: BindScope<'_>,
        error: impl std::fmt::Display,
    ) {
        let addr = addr.to_string();
        let message = format!(
            "Port conflict: {component} could not bind to {proto} {addr} — address already in use"
        );
        let details = serde_json::json!({
            "error_code": "port_conflict",
            "component": component,
            "addr": addr,
            "protocol": proto.as_str(),
            "error": error.to_string(),
        });
        self.send(scope.into_event(
            EventSeverity::Critical,
            category::PORT_CONFLICT,
            message,
            details,
        ));
    }

    /// Emit a Critical `bind_failed` event for any bind error other than
    /// EADDRINUSE (e.g. permission denied, no such device, multicast group
    /// rejected). For EADDRINUSE use [`emit_port_conflict`] instead.
    pub fn emit_bind_failed(
        &self,
        component: &str,
        addr: impl std::fmt::Display,
        proto: BindProto,
        scope: BindScope<'_>,
        error: impl std::fmt::Display,
    ) {
        let addr = addr.to_string();
        let error_str = error.to_string();
        let message = format!("{component} failed to bind to {proto} {addr}: {error_str}");
        let details = serde_json::json!({
            "error_code": "bind_failed",
            "component": component,
            "addr": addr,
            "protocol": proto.as_str(),
            "error": error_str,
        });
        self.send(scope.into_event(
            EventSeverity::Critical,
            category::BIND_FAILED,
            message,
            details,
        ));
    }
}

/// Protocol label attached to bind-failure events so operators can tell a UDP
/// listener collision from a TCP one. Matches the same enum the config-load
/// validator uses for cross-entity port-conflict detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindProto {
    Udp,
    Tcp,
}

impl BindProto {
    pub fn as_str(self) -> &'static str {
        match self {
            BindProto::Udp => "UDP",
            BindProto::Tcp => "TCP",
        }
    }
}

impl std::fmt::Display for BindProto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Scope of a bind failure: which entity does it belong to. Used by the
/// `emit_port_conflict` / `emit_bind_failed` helpers so callers can specify
/// any combination of flow/input/output without juggling four overloads.
#[derive(Debug, Clone, Copy, Default)]
pub struct BindScope<'a> {
    pub flow_id: Option<&'a str>,
    pub input_id: Option<&'a str>,
    pub output_id: Option<&'a str>,
}

impl<'a> BindScope<'a> {
    pub fn input(input_id: &'a str) -> Self {
        Self { input_id: Some(input_id), ..Self::default() }
    }
    pub fn flow(flow_id: &'a str) -> Self {
        Self { flow_id: Some(flow_id), ..Self::default() }
    }
    /// Test-only convenience used by `emit_port_conflict_event_shape` to
    /// pin both flow and input ids — the rest of the code paths populate
    /// the struct directly.
    #[cfg(test)]
    pub fn flow_input(flow_id: &'a str, input_id: &'a str) -> Self {
        Self { flow_id: Some(flow_id), input_id: Some(input_id), output_id: None }
    }
    /// Test-only: empty scope, used to verify scopeless bind-failure events.
    #[cfg(test)]
    pub fn none() -> Self { Self::default() }

    fn into_event(
        self,
        severity: EventSeverity,
        category: &str,
        message: String,
        details: serde_json::Value,
    ) -> Event {
        Event {
            severity,
            category: category.to_string(),
            message,
            details: Some(details),
            flow_id: self.flow_id.map(|s| s.to_string()),
            input_id: self.input_id.map(|s| s.to_string()),
            output_id: self.output_id.map(|s| s.to_string()),
        }
    }
}

/// Event category strings used by the edge.
///
/// The manager's `events` table stores `category` as a free-form string, so
/// adding new categories does not require a database migration. These
/// constants exist so producers (and the unit tests) cannot diverge on
/// spelling, and so the new ST 2110 categories are documented in code
/// alongside the existing ones.
#[allow(dead_code)]
pub mod category {
    pub const SRT: &str = "srt";
    pub const RTMP: &str = "rtmp";
    pub const RTSP: &str = "rtsp";
    pub const HLS: &str = "hls";
    /// CMAF / CMAF-LL output lifecycle (ingest connect, segment upload
    /// failure, manifest upload failure, encryption init).
    pub const CMAF: &str = "cmaf";
    pub const TUNNEL: &str = "tunnel";
    pub const FLOW: &str = "flow";
    pub const BANDWIDTH: &str = "bandwidth";
    pub const WEBRTC: &str = "webrtc";
    // ── ST 2110 / NMOS event categories ──
    /// PTP lock state changes (acquired, lost, holdover, reporter unavailable).
    pub const PTP: &str = "ptp";
    /// SMPTE 2022-7 Red/Blue leg state (single-leg, dual-leg, leg switch).
    pub const NETWORK_LEG: &str = "network_leg";
    /// NMOS IS-04/IS-05/IS-08/BCP-004 lifecycle (registration, activation,
    /// channel-map updates).
    pub const NMOS: &str = "nmos";
    /// SCTE-104 ad-marker decoding events surfaced from ANC (-40) flows.
    pub const SCTE104: &str = "scte104";
    /// ffmpeg-sidecar audio encoder lifecycle (started / failed / restarted).
    /// Emitted by the per-output encoder bridges in `engine::audio_encode`
    /// and the build helpers in output_rtmp / output_hls / output_webrtc.
    pub const AUDIO_ENCODE: &str = "audio_encode";
    /// System resource threshold events (CPU/RAM warning, critical, recovery).
    /// Emitted by `engine::resource_monitor`.
    pub const SYSTEM_RESOURCES: &str = "system_resources";
    // ── Protocol and infrastructure categories ──
    /// RIST Simple Profile connection lifecycle.
    pub const RIST: &str = "rist";
    /// RTP input bind and lifecycle events.
    pub const RTP: &str = "rtp";
    /// UDP input bind and lifecycle events.
    pub const UDP: &str = "udp";
    /// Video transcoding lifecycle (TsVideoReplacer start / fail).
    pub const VIDEO_ENCODE: &str = "video_encode";
    /// Media-aware bonding lifecycle (bond socket bind, path failover,
    /// NACK-driven retransmits exhausted). Scoped to flows that carry a
    /// `bonded` input or output.
    pub const MEDIA: &str = "media";
    /// Bonded input/output lifecycle (kept distinct from `media` so
    /// operators can filter the event feed).
    pub const BOND: &str = "bond";
    /// Flow group start/stop events.
    pub const FLOW_GROUP: &str = "flow_group";
    /// Configuration change events.
    pub const CONFIG: &str = "config";
    /// Manager WebSocket connection lifecycle.
    pub const MANAGER: &str = "manager";
    /// SMPTE 2022-7 SRT redundancy leg events.
    pub const REDUNDANCY: &str = "redundancy";
    /// Standby SRT listener port management.
    pub const STANDBY: &str = "standby";
    /// Port conflict events — internal collision between two configured
    /// entities or an OS-level "address already in use" at runtime.
    /// Always paired with `details.error_code = "port_conflict"`.
    pub const PORT_CONFLICT: &str = "port_conflict";
    /// Generic bind failure (permission denied, no such device, multicast
    /// group rejected, etc.). For EADDRINUSE prefer `PORT_CONFLICT`.
    /// Always paired with `details.error_code = "bind_failed"`.
    pub const BIND_FAILED: &str = "bind_failed";
    /// Replay-server lifecycle (recording started/stopped, clip created/
    /// deleted, playback started/stopped/EOF, writer lagged, disk full,
    /// index corrupt). Always paired with `details.replay_event = ...` and,
    /// for failure events, `details.error_code = ...`.
    pub const REPLAY: &str = "replay";
}

/// Scope label passed to [`run_bond_event_forwarder`] so generated
/// event messages read "bonded input" or "bonded output".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondEventScope {
    Input,
    Output,
}

impl BondEventScope {
    fn label(self) -> &'static str {
        match self {
            BondEventScope::Input => "bonded input",
            BondEventScope::Output => "bonded output",
        }
    }
}

/// Rate-limit / flap grace window. A per-path alive↔dead transition
/// arriving within this window after the opposite transition is
/// suppressed so a flapping link can't drown the events feed. Tuned
/// to match the existing `engine::resource_monitor` grace-period
/// convention; longer would hide genuine short outages, shorter would
/// re-admit noise from cellular re-handshakes.
const BOND_EVENT_FLAP_GRACE: Duration = Duration::from_secs(2);

/// Forward `PathEvent`s from a bonding socket into the manager event
/// feed. Runs until `cancel` fires, the broadcast sender closes, or
/// the subscriber can't keep up (lagged receiver — we reconcile on
/// next transition since the health monitor still owns canonical
/// state via `PathStats.dead`).
///
/// `target_id` identifies the scope; for `BondEventScope::Input` /
/// `BondEventScope::Output` it is the input/output id, and the event
/// is emitted flow-scoped so the operator sees it in the flow events
/// stream. Per-path transitions are flap-deduped with a 2 s grace;
/// bond-aggregate transitions always emit.
pub async fn run_bond_event_forwarder(
    mut rx: broadcast::Receiver<PathEvent>,
    events: EventSender,
    flow_id: String,
    scope: BondEventScope,
    target_id: String,
    cancel: CancellationToken,
) {
    let mut last_per_path: HashMap<PathId, (PathEventKindMarker, Instant)> = HashMap::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            res = rx.recv() => match res {
                Ok(ev) => {
                    if let Some(event) = translate_path_event(
                        &ev,
                        scope,
                        &target_id,
                        &flow_id,
                        &mut last_per_path,
                    ) {
                        events.send(event);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Slow subscriber — events are already reflected
                    // in PathStats.dead; next transition will catch us
                    // back up.
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathEventKindMarker {
    Alive,
    Dead,
}

fn translate_path_event(
    ev: &PathEvent,
    scope: BondEventScope,
    target_id: &str,
    flow_id: &str,
    last_per_path: &mut HashMap<PathId, (PathEventKindMarker, Instant)>,
) -> Option<Event> {
    let now = Instant::now();
    let scope_label = scope.label();
    let path_label = if ev.path_name.is_empty() {
        format!("#{}", ev.path_id)
    } else {
        format!("'{}' (#{})", ev.path_name, ev.path_id)
    };

    let (severity, message, details_extra) = match &ev.kind {
        PathEventKind::PathAlive { alive_count, total } => {
            // Flap-dedup: if the last transition for this path was
            // Dead within the grace window, treat this revival as
            // a blip and suppress.
            if let Some((PathEventKindMarker::Dead, at)) = last_per_path.get(&ev.path_id) {
                if now.saturating_duration_since(*at) < BOND_EVENT_FLAP_GRACE {
                    last_per_path.insert(ev.path_id, (PathEventKindMarker::Alive, now));
                    return None;
                }
            }
            last_per_path.insert(ev.path_id, (PathEventKindMarker::Alive, now));
            (
                EventSeverity::Info,
                format!(
                    "{scope_label} path {path_label} alive ({alive_count}/{total} paths up)"
                ),
                serde_json::json!({ "alive_count": alive_count, "total": total }),
            )
        }
        PathEventKind::PathDead {
            reason,
            alive_count,
            total,
        } => {
            if let Some((PathEventKindMarker::Alive, at)) = last_per_path.get(&ev.path_id) {
                if now.saturating_duration_since(*at) < BOND_EVENT_FLAP_GRACE {
                    last_per_path.insert(ev.path_id, (PathEventKindMarker::Dead, now));
                    return None;
                }
            }
            last_per_path.insert(ev.path_id, (PathEventKindMarker::Dead, now));
            let reason_str = reason.as_str();
            (
                EventSeverity::Warning,
                format!(
                    "{scope_label} path {path_label} dead: {reason_str} \
                     ({alive_count}/{total} paths up)"
                ),
                serde_json::json!({
                    "reason": path_dead_reason_tag(reason),
                    "alive_count": alive_count,
                    "total": total,
                }),
            )
        }
        PathEventKind::BondDegraded { alive_count, total } => (
            EventSeverity::Warning,
            format!(
                "{scope_label} degraded — {alive_count}/{total} paths up \
                 (redundancy lost)"
            ),
            serde_json::json!({ "alive_count": alive_count, "total": total }),
        ),
        PathEventKind::BondDown { total } => (
            EventSeverity::Critical,
            format!(
                "{scope_label} down — 0/{total} paths up (media plane offline)"
            ),
            serde_json::json!({ "alive_count": 0, "total": total }),
        ),
        PathEventKind::BondRecovered { alive_count, total } => (
            EventSeverity::Info,
            format!(
                "{scope_label} recovered — {alive_count}/{total} paths up"
            ),
            serde_json::json!({ "alive_count": alive_count, "total": total }),
        ),
    };

    let mut details = serde_json::json!({
        "path_id": ev.path_id,
        "path_name": ev.path_name,
        "scope": match scope {
            BondEventScope::Input => "input",
            BondEventScope::Output => "output",
        },
    });
    if let serde_json::Value::Object(ref mut base) = details {
        if let serde_json::Value::Object(extra) = details_extra {
            for (k, v) in extra {
                base.insert(k, v);
            }
        }
    }
    let (input_id, output_id) = match scope {
        BondEventScope::Input => (Some(target_id.to_string()), None),
        BondEventScope::Output => (None, Some(target_id.to_string())),
    };

    Some(Event {
        severity,
        category: category::BOND.to_string(),
        message,
        details: Some(details),
        flow_id: Some(flow_id.to_string()),
        input_id,
        output_id,
    })
}

fn path_dead_reason_tag(reason: &PathDeadReason) -> &'static str {
    match reason {
        PathDeadReason::KeepaliveTimeout => "keepalive_timeout",
        PathDeadReason::ReceiveTimeout => "receive_timeout",
        PathDeadReason::TransportError => "transport_error",
    }
}

impl EventDropState {
    fn note_dropped(&self) {
        self.total_dropped.fetch_add(1, Ordering::Relaxed);
        self.pending_dropped.fetch_add(1, Ordering::Relaxed);
        let now_us = crate::util::time::now_us();
        let last = self.last_warn_us.load(Ordering::Relaxed);
        if now_us.saturating_sub(last) < 1_000_000 {
            return;
        }
        if self
            .last_warn_us
            .compare_exchange(last, now_us, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let pending = self.pending_dropped.swap(0, Ordering::Relaxed);
        let total = self.total_dropped.load(Ordering::Relaxed);
        tracing::warn!(
            dropped = pending,
            total_dropped = total,
            capacity = EVENT_CHANNEL_CAPACITY,
            "manager event channel full — dropping newest events",
        );
    }
}

/// Create an event sender/receiver pair.
///
/// The sender is cloned into components; the receiver is consumed by the
/// manager WebSocket client loop. The channel is bounded — see
/// [`EVENT_CHANNEL_CAPACITY`].
pub fn event_channel() -> (EventSender, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    (
        EventSender {
            tx,
            recent: std::sync::Arc::new(RecentCriticalTracker::default()),
            drop_state: std::sync::Arc::new(EventDropState::default()),
        },
        rx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `emit_port_conflict` produces a Critical event on the
    /// `port_conflict` category with `details.error_code = "port_conflict"`.
    /// Manager UI parsers depend on this exact shape to highlight the
    /// offending field.
    #[test]
    fn emit_port_conflict_event_shape() {
        let (sender, mut rx) = event_channel();
        sender.emit_port_conflict(
            "SRT input listener",
            "0.0.0.0:9527",
            BindProto::Udp,
            BindScope::flow_input("flow-1", "in-1"),
            "address already in use",
        );
        let ev = rx.try_recv().expect("event delivered");
        assert_eq!(ev.severity, EventSeverity::Critical);
        assert_eq!(ev.category, category::PORT_CONFLICT);
        assert_eq!(ev.flow_id.as_deref(), Some("flow-1"));
        assert_eq!(ev.input_id.as_deref(), Some("in-1"));
        assert!(ev.message.contains("Port conflict"));
        assert!(ev.message.contains("UDP"));
        assert!(ev.message.contains("0.0.0.0:9527"));
        let details = ev.details.expect("details present");
        assert_eq!(details["error_code"], "port_conflict");
        assert_eq!(details["component"], "SRT input listener");
        assert_eq!(details["addr"], "0.0.0.0:9527");
        assert_eq!(details["protocol"], "UDP");
    }

    /// `emit_bind_failed` is the non-EADDRINUSE counterpart — same shape but
    /// `bind_failed` category and error_code.
    #[test]
    fn emit_bind_failed_event_shape() {
        let (sender, mut rx) = event_channel();
        sender.emit_bind_failed(
            "Edge API server",
            "0.0.0.0:80",
            BindProto::Tcp,
            BindScope::none(),
            "permission denied",
        );
        let ev = rx.try_recv().expect("event delivered");
        assert_eq!(ev.severity, EventSeverity::Critical);
        assert_eq!(ev.category, category::BIND_FAILED);
        assert_eq!(ev.flow_id, None);
        let details = ev.details.expect("details present");
        assert_eq!(details["error_code"], "bind_failed");
        assert_eq!(details["protocol"], "TCP");
    }

    fn make_event(path_id: PathId, kind: PathEventKind) -> PathEvent {
        PathEvent {
            path_id,
            path_name: format!("p{path_id}"),
            kind,
        }
    }

    #[test]
    fn per_path_flap_within_grace_is_suppressed() {
        let mut last: HashMap<PathId, (PathEventKindMarker, Instant)> = HashMap::new();

        // First Dead — emits.
        let dead = make_event(
            0,
            PathEventKind::PathDead {
                reason: PathDeadReason::KeepaliveTimeout,
                alive_count: 0,
                total: 1,
            },
        );
        let ev = translate_path_event(
            &dead,
            BondEventScope::Input,
            "in-1",
            "flow-1",
            &mut last,
        );
        assert!(ev.is_some());
        assert_eq!(ev.as_ref().unwrap().severity, EventSeverity::Warning);
        assert_eq!(ev.as_ref().unwrap().category, category::BOND);

        // Rapid Alive — opposite direction within 2s window → suppressed.
        let alive = make_event(
            0,
            PathEventKind::PathAlive {
                alive_count: 1,
                total: 1,
            },
        );
        let ev = translate_path_event(
            &alive,
            BondEventScope::Input,
            "in-1",
            "flow-1",
            &mut last,
        );
        assert!(ev.is_none(), "flap should be suppressed within grace");
    }

    #[test]
    fn per_path_opposite_after_grace_emits() {
        let mut last: HashMap<PathId, (PathEventKindMarker, Instant)> = HashMap::new();

        // Seed a stale Dead transition 5 s in the past.
        let past = Instant::now() - Duration::from_secs(5);
        last.insert(0, (PathEventKindMarker::Dead, past));

        let alive = make_event(
            0,
            PathEventKind::PathAlive {
                alive_count: 1,
                total: 1,
            },
        );
        let ev = translate_path_event(
            &alive,
            BondEventScope::Output,
            "out-1",
            "flow-1",
            &mut last,
        );
        assert!(ev.is_some(), "transition after grace must emit");
        let ev = ev.unwrap();
        assert_eq!(ev.severity, EventSeverity::Info);
        assert_eq!(ev.output_id.as_deref(), Some("out-1"));
        assert!(ev.message.contains("alive"));
    }

    #[test]
    fn bond_aggregate_events_always_emit() {
        let mut last: HashMap<PathId, (PathEventKindMarker, Instant)> = HashMap::new();

        // Aggregate transitions are never flap-deduped — seed a stale
        // per-path marker and drive a bond-down anyway.
        last.insert(0, (PathEventKindMarker::Dead, Instant::now()));

        let down = make_event(0, PathEventKind::BondDown { total: 2 });
        let ev = translate_path_event(
            &down,
            BondEventScope::Output,
            "out-1",
            "flow-1",
            &mut last,
        );
        assert!(ev.is_some());
        let ev = ev.unwrap();
        assert_eq!(ev.severity, EventSeverity::Critical);
        assert!(ev.message.contains("down"));

        let degraded = make_event(
            0,
            PathEventKind::BondDegraded {
                alive_count: 1,
                total: 2,
            },
        );
        let ev = translate_path_event(
            &degraded,
            BondEventScope::Output,
            "out-1",
            "flow-1",
            &mut last,
        );
        assert!(ev.is_some());
        assert_eq!(ev.unwrap().severity, EventSeverity::Warning);

        let recovered = make_event(
            0,
            PathEventKind::BondRecovered {
                alive_count: 2,
                total: 2,
            },
        );
        let ev = translate_path_event(
            &recovered,
            BondEventScope::Output,
            "out-1",
            "flow-1",
            &mut last,
        );
        assert!(ev.is_some());
        let ev = ev.unwrap();
        assert_eq!(ev.severity, EventSeverity::Info);
        assert!(ev.message.contains("recovered"));
    }

    #[test]
    fn path_event_carries_flow_and_scope_ids() {
        let mut last = HashMap::new();
        let dead = make_event(
            7,
            PathEventKind::PathDead {
                reason: PathDeadReason::ReceiveTimeout,
                alive_count: 1,
                total: 2,
            },
        );
        let ev = translate_path_event(
            &dead,
            BondEventScope::Input,
            "in-abc",
            "flow-xyz",
            &mut last,
        )
        .unwrap();
        assert_eq!(ev.flow_id.as_deref(), Some("flow-xyz"));
        assert_eq!(ev.input_id.as_deref(), Some("in-abc"));
        assert!(ev.output_id.is_none());
        let details = ev.details.unwrap();
        assert_eq!(details["path_id"], 7);
        assert_eq!(details["scope"], "input");
        assert_eq!(details["reason"], "receive_timeout");
        assert_eq!(details["alive_count"], 1);
        assert_eq!(details["total"], 2);
    }

    #[test]
    fn two_paths_flap_independently() {
        let mut last: HashMap<PathId, (PathEventKindMarker, Instant)> = HashMap::new();

        // Path 0 goes dead.
        let ev = translate_path_event(
            &make_event(
                0,
                PathEventKind::PathDead {
                    reason: PathDeadReason::KeepaliveTimeout,
                    alive_count: 1,
                    total: 2,
                },
            ),
            BondEventScope::Output,
            "out-1",
            "flow-1",
            &mut last,
        );
        assert!(ev.is_some());

        // Path 1 goes dead — unrelated to path 0's grace window, must emit.
        let ev = translate_path_event(
            &make_event(
                1,
                PathEventKind::PathDead {
                    reason: PathDeadReason::KeepaliveTimeout,
                    alive_count: 0,
                    total: 2,
                },
            ),
            BondEventScope::Output,
            "out-1",
            "flow-1",
            &mut last,
        );
        assert!(ev.is_some(), "different path must not inherit another path's grace");
    }
}

/// Build a WebSocket event envelope from an `Event`.
pub fn build_event_envelope(event: &Event) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "severity": event.severity.as_str(),
        "category": event.category,
        "message": event.message,
    });
    if let Some(ref details) = event.details {
        payload["details"] = details.clone();
    }
    if let Some(ref flow_id) = event.flow_id {
        payload["flow_id"] = serde_json::Value::String(flow_id.clone());
    }
    if let Some(ref input_id) = event.input_id {
        payload["input_id"] = serde_json::Value::String(input_id.clone());
    }
    if let Some(ref output_id) = event.output_id {
        payload["output_id"] = serde_json::Value::String(output_id.clone());
    }
    serde_json::json!({
        "type": "event",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": payload
    })
}
