// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

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
        self.send(Event {
            severity,
            category: category.to_string(),
            message: message.into(),
            details: Some(details),
            flow_id: Some(flow_id.to_string()),
        });
    }
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
    serde_json::json!({
        "type": "event",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": payload
    })
}
