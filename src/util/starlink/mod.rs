// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Starlink dish telemetry — read-only link state for a bond leg (or any
//! interface) that egresses over a Starlink terminal.
//!
//! Sibling to [`crate::util::cellular`]: a slow background poller samples the
//! dish over its local **gRPC** API (the `SpaceX.API.Device.Device/Handle`
//! `get_status` call, cleartext HTTP/2 on `192.168.100.1:9200`), normalizes the
//! `DishGetStatusResponse` into a unified [`StarlinkMetrics`] shape, and writes
//! it into a lock-free [`StarlinkCache`] keyed by kernel netdev.
//! [`crate::util::network_interfaces::NetworkSampler`] joins that cache onto
//! each `NetworkInterfaceInfo.starlink` at health-tick time (off the data path),
//! and `manager/client.rs` advertises the `"starlink"` capability whenever the
//! poller has at least one source.
//!
//! The dish gRPC is **unauthenticated** on the LAN, so — unlike the RutOS
//! cellular source — there is **no credential and no secret**. The only config
//! is the interface to annotate and the dish address (default
//! `192.168.100.1:9200`). The protobuf request/response is hand-rolled in
//! [`grpc`] (no tonic/prost dependency): the request is a fixed 8-byte frame and
//! the response decoder is a tolerant field walker that skips anything it does
//! not recognise, so newer dish firmware that adds fields degrades cleanly.
//!
//! Wire shape is additive + backward-compatible: every field is `Option`
//! (or a `skip_serializing_if`-empty `Vec`), the [`StarlinkState`] enum carries
//! a `#[serde(other)]` catch-all, and no `WS_PROTOCOL_VERSION` bump is required.
//! The manager mirrors these structs in `manager_core::models::ws_protocol` —
//! keep field names + serde defaults in sync.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::models::AppConfig;
use crate::manager::events::{EventSender, EventSeverity, category};

pub mod grpc;

// ── Tuning constants (slow path; not data-path-sensitive) ──

/// How often the poller samples every dish. Faster than the ~15 s health tick
/// so a fresh snapshot is usually waiting when the tick joins the cache.
const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Per-source sample budget. The gRPC round-trip is wrapped in this timeout so a
/// hung / unreachable dish never stalls the poll cycle (or the data path — the
/// poller is wholly off it). Slightly longer than the cellular budget because a
/// cold HTTP/2 connection + `get_status` is a touch heavier than a D-Bus read.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(6);
/// A cached snapshot older than this is evicted, so the UI shows "no data"
/// (acquiring) rather than a frozen value when a dish goes dark. Several poll
/// intervals of hysteresis avoids flapping on a single transient miss.
const STALE_AFTER: Duration = Duration::from_secs(60);
/// Consecutive poll failures before a single `starlink_uplink_unreachable`
/// Warning is emitted. The poll loop is the retry; no per-sample retries.
const UNREACHABLE_FAILS: u32 = 3;

// ── Wire data model (mirrored in manager-core::models::ws_protocol) ──

/// Live link state for one Starlink dish, attached to the kernel netdev it
/// annotates via `NetworkInterfaceInfo.starlink`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StarlinkMetrics {
    /// Normalized dish connectivity state.
    pub state: StarlinkState,
    /// Fraction of the rolling window the dish was obstructed (0.0..=1.0).
    /// Lower is better; the headline "signal" axis (the satellite-link analog
    /// of cellular RSRP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obstruction_fraction: Option<f32>,
    /// Whether the dish has a sky obstruction right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currently_obstructed: Option<bool>,
    /// Instantaneous downlink throughput, bits/sec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downlink_bps: Option<f32>,
    /// Instantaneous uplink throughput, bits/sec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uplink_bps: Option<f32>,
    /// Round-trip latency to the Starlink PoP, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pop_ping_latency_ms: Option<f32>,
    /// Fraction of PoP pings dropped (0.0..=1.0) — link reliability. Lower is
    /// better; the satellite-link analog of cellular SINR for bar derivation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pop_ping_drop_rate: Option<f32>,
    /// Predicted seconds until the next obstruction clears the field of view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_to_first_nonempty_slot: Option<f32>,
    /// Signal-to-noise ratio. **Legacy** — deprecated on recent firmware and
    /// usually absent; surfaced when present for older dishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr: Option<f32>,
    /// Dish uptime in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    /// Active hardware alerts (e.g. `"thermal_throttle"`, `"motors_stuck"`).
    /// Empty when none are raised.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<String>,
    /// Dish hardware id (opaque string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Dish hardware version (e.g. `"rev3_proto2"`, `"mini"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_version: Option<String>,
    /// Dish software / firmware version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub software_version: Option<String>,
    /// Two-letter country code the dish reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    /// Normalized 0..=5 quality bars (worst-of ping-drop / obstruction, gated on
    /// connectivity state). The UI reads this for the glyph; raw figures drive
    /// colour + hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bars: Option<u8>,
    /// Last poll-failure reason for a configured dish that isn't responding
    /// (unreachable / wrong endpoint / unexpected response). When set, the UI
    /// renders the row as errored with this cause instead of a blank "no data"
    /// row. `None` on a healthy sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Unix milliseconds when this snapshot was sampled (drives staleness + the
    /// UI "age" hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at_unix_ms: Option<u64>,
}

impl StarlinkMetrics {
    /// Failure placeholder for a configured dish whose poll failed, so the UI
    /// surfaces the configured terminal with its error cause instead of a blank
    /// row. Carries only state + reason + timestamp.
    pub fn unreachable(reason: String, sampled_at_unix_ms: u64) -> Self {
        Self {
            state: StarlinkState::Offline,
            obstruction_fraction: None,
            currently_obstructed: None,
            downlink_bps: None,
            uplink_bps: None,
            pop_ping_latency_ms: None,
            pop_ping_drop_rate: None,
            seconds_to_first_nonempty_slot: None,
            snr: None,
            uptime_s: None,
            alerts: Vec::new(),
            device_id: None,
            hardware_version: None,
            software_version: None,
            country_code: None,
            bars: Some(0),
            last_error: Some(reason),
            sampled_at_unix_ms: Some(sampled_at_unix_ms),
        }
    }
}

/// Normalized dish connectivity state, derived from the `DishState` enum in
/// `DishGetStatusResponse` (with a synthetic [`StarlinkState::Offline`] for the
/// unreachable placeholder).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StarlinkState {
    /// Online and passing traffic.
    Connected,
    /// Acquiring / searching for satellites.
    Searching,
    /// Powering up.
    Booting,
    /// Synthetic — the dish did not respond to the poll (not a wire value).
    Offline,
    /// `UNKNOWN`/`0` from the dish, or a state this build does not map.
    #[serde(other)]
    Unknown,
}

impl StarlinkState {
    /// Map the dish's `DishState` protobuf enum value. Only the values verified
    /// across the reverse-engineered proto trees are mapped; anything else
    /// degrades to [`StarlinkState::Unknown`] (forward-compatible with firmware
    /// that adds states like `STOWED` / `THERMAL_SHUTDOWN` / `NO_SATS`).
    pub fn from_proto(v: i64) -> Self {
        match v {
            1 => StarlinkState::Connected,
            2 => StarlinkState::Searching,
            3 => StarlinkState::Booting,
            _ => StarlinkState::Unknown,
        }
    }

    /// Lower-case wire tag (for events / logs).
    pub fn as_str(self) -> &'static str {
        match self {
            StarlinkState::Connected => "connected",
            StarlinkState::Searching => "searching",
            StarlinkState::Booting => "booting",
            StarlinkState::Offline => "offline",
            StarlinkState::Unknown => "unknown",
        }
    }
}

// ── Lock-free cache ──

/// Lock-free per-interface cache the poller writes and the health-tick reads.
///
/// `has_sources()` is keyed off the count of dishes the poller is *managing*
/// (configured `starlink_uplinks`), not off whether the map currently holds
/// fresh data — so the `"starlink"` capability stays stable while snapshots age
/// out and refresh.
#[derive(Debug, Default)]
pub struct StarlinkCache {
    map: DashMap<String, StarlinkMetrics>,
    source_count: AtomicUsize,
}

impl StarlinkCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clone the latest snapshot for an interface, if any (non-stale entries
    /// only — the poller evicts stale ones each cycle).
    pub fn get(&self, iface: &str) -> Option<StarlinkMetrics> {
        self.map.get(iface).map(|e| e.value().clone())
    }

    /// Insert / replace the snapshot for an interface.
    pub fn insert(&self, iface: String, metrics: StarlinkMetrics) {
        self.map.insert(iface, metrics);
    }

    /// Drop entries whose `sampled_at_unix_ms` is older than `cutoff_ms`
    /// (entries with no timestamp are kept defensively).
    pub fn evict_older_than(&self, cutoff_ms: u64) {
        self.map
            .retain(|_, m| m.sampled_at_unix_ms.map(|t| t >= cutoff_ms).unwrap_or(true));
    }

    /// Record how many dishes the poller is managing this cycle.
    pub fn set_source_count(&self, n: usize) {
        self.source_count.store(n, Ordering::Relaxed);
    }

    /// True when the poller has at least one source — drives the `"starlink"`
    /// capability advertisement.
    pub fn has_sources(&self) -> bool {
        self.source_count.load(Ordering::Relaxed) > 0
    }
}

// ── Shared helpers ──

/// Current wall-clock in unix milliseconds (saturating; 0 before the epoch).
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Derive a 0..=5 quality bar count from a sampled snapshot. Worst-of the
/// PoP-ping drop rate and the obstruction fraction (both 0..1, lower = better),
/// gated on connectivity state. `None` when connected but no quality figures
/// are available; `Some(0)` when not connected.
pub fn derive_bars(m: &StarlinkMetrics) -> Option<u8> {
    match m.state {
        StarlinkState::Connected => {}
        // Not online → no usable link. Distinguish "off" from "acquiring" only
        // by the state pill; bars are 0 either way.
        _ => return Some(0),
    }
    fn drop_bars(v: f32) -> u8 {
        if v <= 0.005 {
            5
        } else if v <= 0.02 {
            4
        } else if v <= 0.05 {
            3
        } else if v <= 0.10 {
            2
        } else if v <= 0.25 {
            1
        } else {
            0
        }
    }
    fn obstruction_bars(v: f32) -> u8 {
        if v <= 0.001 {
            5
        } else if v <= 0.005 {
            4
        } else if v <= 0.01 {
            3
        } else if v <= 0.02 {
            2
        } else if v <= 0.05 {
            1
        } else {
            0
        }
    }
    let mut bars: Option<u8> = None;
    let mut fold = |b: u8| bars = Some(bars.map_or(b, |cur| cur.min(b)));
    if let Some(v) = m.pop_ping_drop_rate {
        fold(drop_bars(v.clamp(0.0, 1.0)));
    }
    if let Some(v) = m.obstruction_fraction {
        fold(obstruction_bars(v.clamp(0.0, 1.0)));
    }
    bars
}

// ── Background poller ──

/// Spawn the Starlink poller under the app's cancellation tree. Returns the task
/// handle (held for the process lifetime). Cheap when no dish is configured —
/// `starlink_uplinks` is empty by default, so the loop is a no-op.
pub fn spawn_starlink_poller(
    app_config: Arc<RwLock<AppConfig>>,
    cache: Arc<StarlinkCache>,
    events: EventSender,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        starlink_poller_loop(app_config, cache, events, cancel).await;
    })
}

/// Per-interface state the poller carries across cycles to debounce events.
#[derive(Default)]
struct DishEventState {
    last_state: Option<StarlinkState>,
    /// True while the link is in the "obstructed" state (hysteresis).
    obstructed: bool,
    /// Set of alert names currently raised (so a new alert fires once).
    alerts: HashSet<String>,
    /// Consecutive sample failures.
    fail_count: u32,
    /// True while an unreachable Warning is outstanding (so we emit once).
    unreachable: bool,
}

/// The `/24` network address (subnet) for an IPv4.
fn slash24(ip: std::net::Ipv4Addr) -> std::net::Ipv4Addr {
    let o = ip.octets();
    std::net::Ipv4Addr::new(o[0], o[1], o[2], 0)
}

/// `.1` of an IPv4's `/24` — the conventional Wi-Fi/router gateway address.
fn gateway_dot_one(ip: std::net::Ipv4Addr) -> std::net::Ipv4Addr {
    let o = ip.octets();
    std::net::Ipv4Addr::new(o[0], o[1], o[2], 1)
}

/// Parse a little-endian hex IPv4 (the form in `/proc/net/route`).
fn le_hex_to_ipv4(hex: &str) -> Option<std::net::Ipv4Addr> {
    let raw = u32::from_str_radix(hex, 16).ok()?;
    let [a, b, c, d] = raw.to_le_bytes();
    Some(std::net::Ipv4Addr::new(a, b, c, d))
}

/// Read the interface's REAL default-route gateway from the kernel
/// (`/proc/net/route`) — network-agnostic, no subnet/prefix assumption,
/// works on any customer's network. `None` when the interface has no
/// installed default route (e.g. a `use-routes: false` Wi-Fi leg) or the
/// table can't be read.
fn kernel_gateway_for(interface: &str) -> Option<std::net::Ipv4Addr> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in table.lines().skip(1) {
        let mut f = line.split_whitespace();
        let (iface, dest, gw) = (f.next()?, f.next()?, f.next()?);
        if iface == interface && dest == "00000000" && gw != "00000000" {
            return le_hex_to_ipv4(gw);
        }
    }
    None
}

/// Resolve a dish's Wi-Fi gateway, network-agnostically: the interface's REAL
/// default-route next-hop from the kernel, falling back to `.1` of its IPv4
/// /24 ONLY when no default route is installed (a `use-routes: false` leg).
/// An explicit `gateway` in config overrides both (resolved at the call site).
/// `None` if the interface is absent / has no IPv4 yet.
fn derive_gateway(interface: &str) -> Option<std::net::Ipv4Addr> {
    if let Some(gw) = kernel_gateway_for(interface) {
        return Some(gw);
    }
    let nics = crate::util::network_interfaces::enumerate();
    let nic = nics.iter().find(|n| n.name == interface)?;
    let ip = nic
        .ipv4
        .iter()
        .find_map(|a| a.split('/').next().and_then(|s| s.trim().parse().ok()))?;
    Some(gateway_dot_one(ip))
}

/// Ensure the host route to a dish's management subnet exists — the edge owns
/// it (CAP_NET_ADMIN) so the operator never re-adds it by hand after the Wi-Fi
/// link cycles. Idempotent (add-or-replace) + best-effort: logs at debug and
/// moves on if it can't program (interface down, no derivable gateway, no
/// privilege, non-IPv4 dish address).
async fn ensure_dish_route(uplink: &crate::config::models::StarlinkUplinkConfig) {
    use std::net::Ipv4Addr;
    if uplink.interface.trim().is_empty() {
        return;
    }
    let host = uplink
        .address
        .split(':')
        .next()
        .unwrap_or(uplink.address.as_str());
    let Ok(dish_ip) = host.trim().parse::<Ipv4Addr>() else {
        return;
    };
    let subnet = slash24(dish_ip);
    let Some(gateway) = uplink
        .gateway
        .as_deref()
        .and_then(|g| g.trim().parse::<Ipv4Addr>().ok())
        .or_else(|| derive_gateway(&uplink.interface))
    else {
        return;
    };
    match crate::engine::bond_routing::BondRouteManager::global().await {
        Ok(mgr) => {
            if let Err(e) = mgr
                .program_dish_route(subnet, 24, gateway, &uplink.interface)
                .await
            {
                tracing::debug!(
                    "starlink '{}': dish route {subnet}/24 via {gateway} not programmed: {e}",
                    uplink.interface
                );
            }
        }
        Err(e) => tracing::debug!("starlink: bond route manager unavailable: {e}"),
    }
}

async fn starlink_poller_loop(
    app_config: Arc<RwLock<AppConfig>>,
    cache: Arc<StarlinkCache>,
    events: EventSender,
    cancel: CancellationToken,
) {
    // Sources are rebuilt only when the config changes (rebuilding a reqwest
    // HTTP/2 client per cycle is wasteful); keyed by interface.
    let mut sources: HashMap<String, grpc::StarlinkSource> = HashMap::new();
    let mut sources_sig: Option<String> = None;
    let mut ev_state: HashMap<String, DishEventState> = HashMap::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        // 1. Read the live config (ground truth; picked up without any
        //    UpdateConfig hook — flows are never touched).
        let uplinks = { app_config.read().await.starlink_uplinks.clone() };

        // 2. Rebuild sources when the config changed.
        let sig = serde_json::to_string(&uplinks).unwrap_or_default();
        if sources_sig.as_deref() != Some(sig.as_str()) {
            sources = grpc::build_sources(&uplinks);
            sources_sig = Some(sig);
        }

        // 2.5. Keep each dish's host route programmed (the edge owns it so the
        //      operator never re-adds it by hand after a Wi-Fi link cycle —
        //      the dish sits off-subnet via the Wi-Fi gateway). Idempotent.
        for u in &uplinks {
            ensure_dish_route(u).await;
        }

        let mut produced: Vec<(String, StarlinkMetrics)> = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        let mut source_ifaces: HashSet<String> = HashSet::new();

        // 3. Poll each dish. Each sample is time-bounded; a failure is cached as
        //    an error placeholder (so the UI shows the configured dish with its
        //    cause, not a blank row) and feeds the unreachable-event debounce.
        for (iface, src) in sources.iter() {
            source_ifaces.insert(iface.clone());
            match tokio::time::timeout(SAMPLE_TIMEOUT, src.sample()).await {
                Ok(Ok(m)) => {
                    produced.push((iface.clone(), m));
                }
                Ok(Err(reason)) => {
                    tracing::warn!("starlink uplink '{iface}': {reason}");
                    failed.push((iface.clone(), reason));
                    note_unreachable(&events, &mut ev_state, iface);
                }
                Err(_) => {
                    failed.push((
                        iface.clone(),
                        format!("no response within {}s", SAMPLE_TIMEOUT.as_secs()),
                    ));
                    note_unreachable(&events, &mut ev_state, iface);
                }
            }
        }

        // 4. Publish snapshots + emit state-change events.
        let now_ms = now_unix_ms();
        for (iface, mut m) in produced {
            m.sampled_at_unix_ms = Some(now_ms);
            note_success(&events, &mut ev_state, &iface, &m);
            cache.insert(iface, m);
        }
        // Publish failure placeholders so a configured-but-unreachable dish
        // surfaces its error cause in the UI instead of vanishing to "no data".
        for (iface, reason) in failed {
            cache.insert(iface, StarlinkMetrics::unreachable(reason, now_ms));
        }

        // 5. Age out stale snapshots; publish the managed-source count.
        let cutoff = now_ms.saturating_sub(STALE_AFTER.as_millis() as u64);
        cache.evict_older_than(cutoff);
        cache.set_source_count(source_ifaces.len());
        // Forget event state for interfaces no longer managed.
        ev_state.retain(|k, _| source_ifaces.contains(k));
    }
    tracing::debug!("starlink poller stopped");
}

/// On a successful sample: emit state-change + obstruction + alert events
/// (debounced) and clear any outstanding unreachable Warning.
fn note_success(
    events: &EventSender,
    ev_state: &mut HashMap<String, DishEventState>,
    iface: &str,
    m: &StarlinkMetrics,
) {
    let st = ev_state.entry(iface.to_string()).or_default();

    // Recovery from unreachable.
    if st.unreachable {
        events.emit_with_details(
            EventSeverity::Info,
            category::STARLINK,
            format!("starlink uplink '{iface}' reachable again"),
            None,
            serde_json::json!({ "error_code": "starlink_uplink_recovered", "interface": iface }),
        );
        st.unreachable = false;
    }
    st.fail_count = 0;

    // Connectivity-state transition.
    if st.last_state != Some(m.state) {
        // Skip the very first observation (no prior state → not a transition).
        if let Some(prev) = st.last_state {
            let sev = match m.state {
                StarlinkState::Connected => EventSeverity::Info,
                StarlinkState::Searching | StarlinkState::Booting => EventSeverity::Info,
                StarlinkState::Offline | StarlinkState::Unknown => EventSeverity::Warning,
            };
            events.emit_with_details(
                sev,
                category::STARLINK,
                format!(
                    "starlink uplink '{iface}' {} → {}",
                    prev.as_str(),
                    m.state.as_str()
                ),
                None,
                serde_json::json!({
                    "error_code": "starlink_state_changed",
                    "interface": iface,
                    "from": prev.as_str(),
                    "to": m.state.as_str(),
                }),
            );
        }
        st.last_state = Some(m.state);
    }

    // Obstruction with hysteresis: enter on currently_obstructed, leave when
    // clear AND the rolling fraction is low (so a flapping obstruction doesn't
    // spam the log).
    let obstructed_now = m.currently_obstructed == Some(true);
    let frac = m.obstruction_fraction.unwrap_or(0.0);
    if obstructed_now && !st.obstructed {
        st.obstructed = true;
        events.emit_with_details(
            EventSeverity::Warning,
            category::STARLINK,
            format!("starlink uplink '{iface}' obstructed"),
            None,
            serde_json::json!({
                "error_code": "starlink_obstructed",
                "interface": iface,
                "obstruction_fraction": m.obstruction_fraction,
            }),
        );
    } else if !obstructed_now && st.obstructed && frac < 0.01 {
        st.obstructed = false;
        events.emit_with_details(
            EventSeverity::Info,
            category::STARLINK,
            format!("starlink uplink '{iface}' obstruction cleared"),
            None,
            serde_json::json!({
                "error_code": "starlink_obstruction_cleared",
                "interface": iface,
            }),
        );
    }

    // Hardware alerts: fire a Warning once per newly-raised alert; track the set
    // so a steady alert doesn't re-fire, and a cleared one can re-fire later.
    let current: HashSet<String> = m.alerts.iter().cloned().collect();
    for alert in current.difference(&st.alerts) {
        events.emit_with_details(
            EventSeverity::Warning,
            category::STARLINK,
            format!("starlink uplink '{iface}' alert: {alert}"),
            None,
            serde_json::json!({
                "error_code": "starlink_alert",
                "interface": iface,
                "alert": alert,
            }),
        );
    }
    st.alerts = current;
}

/// On a failed sample: bump the consecutive-failure counter and emit a single
/// Warning once it crosses [`UNREACHABLE_FAILS`].
fn note_unreachable(
    events: &EventSender,
    ev_state: &mut HashMap<String, DishEventState>,
    iface: &str,
) {
    let st = ev_state.entry(iface.to_string()).or_default();
    st.fail_count = st.fail_count.saturating_add(1);
    if st.fail_count >= UNREACHABLE_FAILS && !st.unreachable {
        st.unreachable = true;
        events.emit_with_details(
            EventSeverity::Warning,
            category::STARLINK,
            format!("starlink uplink '{iface}' unreachable"),
            None,
            serde_json::json!({
                "error_code": "starlink_uplink_unreachable",
                "interface": iface,
                "consecutive_failures": st.fail_count,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn dish_route_subnet_and_gateway_derivation() {
        // Dish 192.168.100.1 → /24 subnet 192.168.100.0, gateway .1.
        let dish: Ipv4Addr = "192.168.100.1".parse().unwrap();
        assert_eq!(slash24(dish), Ipv4Addr::new(192, 168, 100, 0));
        // wlo5's address 192.168.4.102 → .1 fallback gateway 192.168.4.1.
        let wlo5_ip: Ipv4Addr = "192.168.4.102".parse().unwrap();
        assert_eq!(gateway_dot_one(wlo5_ip), Ipv4Addr::new(192, 168, 4, 1));
        // The real gateway read from /proc/net/route is little-endian hex —
        // network-agnostic, works on any customer subnet (here 192.168.4.1).
        assert_eq!(le_hex_to_ipv4("0104A8C0"), Some(Ipv4Addr::new(192, 168, 4, 1)));
        // A non-.1 gateway is handled too (10.20.30.254).
        assert_eq!(le_hex_to_ipv4("FE1E140A"), Some(Ipv4Addr::new(10, 20, 30, 254)));
    }

    fn connected(drop_rate: Option<f32>, obstruction: Option<f32>) -> StarlinkMetrics {
        StarlinkMetrics {
            state: StarlinkState::Connected,
            obstruction_fraction: obstruction,
            currently_obstructed: Some(false),
            downlink_bps: Some(120_000_000.0),
            uplink_bps: Some(12_000_000.0),
            pop_ping_latency_ms: Some(28.0),
            pop_ping_drop_rate: drop_rate,
            seconds_to_first_nonempty_slot: Some(0.0),
            snr: None,
            uptime_s: Some(123_456),
            alerts: vec![],
            device_id: Some("ut01".into()),
            hardware_version: Some("rev3".into()),
            software_version: Some("2025.10.03.mr61821".into()),
            country_code: Some("AU".into()),
            bars: None,
            last_error: None,
            sampled_at_unix_ms: Some(1_000),
        }
    }

    #[test]
    fn bars_worst_of_drop_and_obstruction() {
        // Clean link → 5 bars.
        assert_eq!(derive_bars(&connected(Some(0.0), Some(0.0))), Some(5));
        // Good drop rate but heavy obstruction → worst-of pulls it down.
        assert_eq!(derive_bars(&connected(Some(0.0), Some(0.06))), Some(0));
        // Moderate drop rate, no obstruction figure.
        assert_eq!(derive_bars(&connected(Some(0.03), None)), Some(3));
        // Connected but no quality figures → unknown.
        assert_eq!(derive_bars(&connected(None, None)), None);
    }

    #[test]
    fn bars_zero_when_not_connected() {
        let mut m = connected(Some(0.0), Some(0.0));
        m.state = StarlinkState::Searching;
        assert_eq!(derive_bars(&m), Some(0));
        m.state = StarlinkState::Offline;
        assert_eq!(derive_bars(&m), Some(0));
    }

    #[test]
    fn state_from_proto_mapping() {
        assert_eq!(StarlinkState::from_proto(1), StarlinkState::Connected);
        assert_eq!(StarlinkState::from_proto(2), StarlinkState::Searching);
        assert_eq!(StarlinkState::from_proto(3), StarlinkState::Booting);
        assert_eq!(StarlinkState::from_proto(0), StarlinkState::Unknown);
        // Forward-compat: an unmapped firmware state degrades to Unknown.
        assert_eq!(StarlinkState::from_proto(7), StarlinkState::Unknown);
    }

    #[test]
    fn cache_staleness_and_sources() {
        let cache = StarlinkCache::new();
        assert!(!cache.has_sources());
        cache.set_source_count(1);
        assert!(cache.has_sources());
        let mut m = connected(Some(0.0), Some(0.0));
        cache.insert("wlo5".into(), m.clone());
        assert!(cache.get("wlo5").is_some());
        // Cutoff after the sample time → evicted.
        cache.evict_older_than(2_000);
        assert!(cache.get("wlo5").is_none());
        // Fresh sample survives the same cutoff.
        m.sampled_at_unix_ms = Some(3_000);
        cache.insert("wlo5".into(), m);
        cache.evict_older_than(2_000);
        assert!(cache.get("wlo5").is_some());
    }

    #[test]
    fn metrics_serde_roundtrip_and_catchall() {
        let m = connected(Some(0.01), Some(0.002));
        let json = serde_json::to_string(&m).unwrap();
        let back: StarlinkMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        // Unknown state tags fall through to the catch-all arm.
        let s: StarlinkState = serde_json::from_str("\"thermal_shutdown\"").unwrap();
        assert_eq!(s, StarlinkState::Unknown);
        // Empty alerts vec is omitted on the wire (skip_serializing_if).
        assert!(!json.contains("alerts"));
    }

    #[test]
    fn unreachable_placeholder_shape() {
        let m = StarlinkMetrics::unreachable("connection refused".into(), 42);
        assert_eq!(m.state, StarlinkState::Offline);
        assert_eq!(m.last_error.as_deref(), Some("connection refused"));
        assert_eq!(m.bars, Some(0));
        assert_eq!(m.sampled_at_unix_ms, Some(42));
    }
}
