// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Event sender for forwarding operational events to the manager.
//!
//! Components throughout the edge (flow engine, SRT, tunnels, etc.) hold an
//! `EventSender` clone and call its helper methods to report state changes.
//! Events are queued in an unbounded mpsc channel and drained by the manager
//! WebSocket client loop.
//!
//! When not connected to the manager, events accumulate in the channel and are
//! sent once the connection is (re-)established. The channel is unbounded because
//! events are infrequent relative to stats — this avoids backpressure on callers.

use tokio::sync::mpsc;

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

/// Clonable handle for sending events from any component.
///
/// Sending never blocks or fails — if the receiver is dropped (manager client
/// not running), events are silently discarded.
#[derive(Debug, Clone)]
pub struct EventSender {
    tx: mpsc::UnboundedSender<Event>,
}

impl EventSender {
    /// Send an event to the manager.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
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
    /// Tunnel port conflict events.
    pub const PORT_CONFLICT: &str = "port_conflict";
}

/// Create an event sender/receiver pair.
///
/// The sender is cloned into components; the receiver is consumed by the
/// manager WebSocket client loop.
pub fn event_channel() -> (EventSender, mpsc::UnboundedReceiver<Event>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (EventSender { tx }, rx)
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
