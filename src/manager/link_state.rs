// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Device-local manager-link state.
//!
//! Tracks whether this edge currently has an authenticated WebSocket session
//! with its manager, surfaced on the edge's OWN local surfaces — the local
//! REST `/health` endpoint and the operator monitor dashboard — so an operator
//! standing at the device can tell at a glance that the manager link is down.
//!
//! This is purely a device-side indicator. It does NOT alter the health payload
//! the edge sends UP to the manager (that stays `"ok"`), and it carries no
//! protocol surface — nothing here rides the manager↔node WebSocket.
//!
//! Lock-free: a single `AtomicBool` for the connected flag plus an
//! `AtomicI64` carrying the epoch-millis of the last connect/disconnect
//! transition. Both are written from the manager-client loop and read from the
//! HTTP/monitor handlers without any lock on the data or request path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Lock-free record of the edge's link to its manager.
///
/// Shared as an `Arc` between the manager-client loop (writer) and the local
/// HTTP `/health` + monitor dashboard handlers (readers).
#[derive(Debug)]
pub struct ManagerLinkState {
    /// `true` while an authenticated WS session is live, `false` otherwise.
    connected: AtomicBool,
    /// Epoch-millis of the most recent connected↔disconnected transition.
    /// `0` means "no transition has happened yet" (boot before first auth).
    last_transition_ms: AtomicI64,
    /// Whether the manager client is configured/enabled at all on this node.
    /// When `false`, the indicator is "not managed" rather than "disconnected"
    /// — a standalone edge with no manager configured should not show a red
    /// alarm. Set once at startup.
    enabled: AtomicBool,
}

impl ManagerLinkState {
    /// Construct a fresh link state. `enabled` reflects whether a manager
    /// client is configured on this node at all.
    pub fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            connected: AtomicBool::new(false),
            last_transition_ms: AtomicI64::new(0),
            enabled: AtomicBool::new(enabled),
        })
    }

    fn now_ms() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Mark the link as connected (called right after a successful auth /
    /// register ack). Stamps the transition time only on an actual edge from
    /// disconnected → connected so repeated calls within one session don't
    /// reset the timer.
    pub fn set_connected(&self) {
        let was = self.connected.swap(true, Ordering::Relaxed);
        if !was {
            self.last_transition_ms.store(Self::now_ms(), Ordering::Relaxed);
        }
    }

    /// Mark the link as disconnected (called when the session loop breaks or a
    /// connection attempt fails). Stamps the transition time only on an actual
    /// edge from connected → disconnected.
    pub fn set_disconnected(&self) {
        let was = self.connected.swap(false, Ordering::Relaxed);
        if was {
            self.last_transition_ms.store(Self::now_ms(), Ordering::Relaxed);
        }
    }

    /// Whether a manager client is configured/enabled on this node.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Whether an authenticated session is currently live.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Whether the link is in the "reconnecting" state — a manager is enabled
    /// on this node but no authenticated session is currently live, so the
    /// client loop is always retrying. Single source of truth for the predicate
    /// rendered on both local health surfaces (REST `/health` + monitor
    /// dashboard).
    pub fn reconnecting(&self) -> bool {
        self.is_enabled() && !self.is_connected()
    }

    /// Seconds since the link went down, or `None` when connected (or when no
    /// transition has been recorded yet). Saturating — never negative.
    pub fn disconnected_secs(&self) -> Option<u64> {
        if self.is_connected() {
            return None;
        }
        let last = self.last_transition_ms.load(Ordering::Relaxed);
        if last == 0 {
            // Never connected yet (boot): report uptime-since-boot is not
            // tracked here, so report None — the caller renders "connecting".
            return None;
        }
        let now = Self::now_ms();
        Some(((now - last).max(0) / 1000) as u64)
    }

    /// Typed snapshot of the link state. Canonical form for both local health
    /// surfaces — the REST `/health` endpoint embeds it directly, and the
    /// monitor `/api/health` endpoint serializes it (via [`snapshot_json`]) so
    /// both present an identical JSON shape under the same `"manager"` key.
    ///
    /// [`snapshot_json`]: Self::snapshot_json
    pub fn status(&self) -> crate::api::models::ManagerLinkStatus {
        crate::api::models::ManagerLinkStatus {
            enabled: self.is_enabled(),
            connected: self.is_connected(),
            disconnected_secs: self.disconnected_secs(),
            reconnecting: self.reconnecting(),
        }
    }

    /// Render a snapshot suitable for embedding in a JSON response, serialized
    /// from the canonical typed [`status`] so it matches the REST `/health`
    /// shape exactly (including `disconnected_secs` being omitted when `None`).
    ///
    /// [`status`]: Self::status
    pub fn snapshot_json(&self) -> serde_json::Value {
        serde_json::to_value(self.status()).unwrap_or(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_disconnected() {
        let s = ManagerLinkState::new(true);
        assert!(!s.is_connected());
        assert!(s.is_enabled());
        // No transition yet → None (renders "connecting", not a stale duration).
        assert_eq!(s.disconnected_secs(), None);
    }

    #[test]
    fn connect_then_disconnect_stamps_timer() {
        let s = ManagerLinkState::new(true);
        s.set_connected();
        assert!(s.is_connected());
        assert_eq!(s.disconnected_secs(), None);
        s.set_disconnected();
        assert!(!s.is_connected());
        // A transition has now happened, so a duration is available (>= 0).
        assert!(s.disconnected_secs().is_some());
    }

    #[test]
    fn redundant_connect_is_idempotent() {
        let s = ManagerLinkState::new(true);
        s.set_connected();
        let first = s.last_transition_ms.load(Ordering::Relaxed);
        // Second connect within the same session must not move the timer.
        s.set_connected();
        let second = s.last_transition_ms.load(Ordering::Relaxed);
        assert_eq!(first, second);
    }

    #[test]
    fn snapshot_shape_when_disabled() {
        let s = ManagerLinkState::new(false);
        let v = s.snapshot_json();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["connected"], false);
        // Not managed → not "reconnecting".
        assert_eq!(v["reconnecting"], false);
    }

    #[test]
    fn snapshot_reconnecting_when_enabled_and_down() {
        let s = ManagerLinkState::new(true);
        let v = s.snapshot_json();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["connected"], false);
        assert_eq!(v["reconnecting"], true);
    }
}
