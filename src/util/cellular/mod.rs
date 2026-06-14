// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cellular uplink telemetry — read-only radio state for bond legs that
//! egress over a mobile modem or a RutOS router.
//!
//! Two sources, one unified [`CellularMetrics`] shape:
//!   - [`modem_manager`] — any USB cellular modem ModemManager owns, read
//!     over D-Bus (`org.freedesktop.ModemManager1`). **Zero config**, Linux
//!     only, unprivileged (published-property reads).
//!   - [`rutos`] — a Teltonika RutOS router (RUT / RUTX / OTD), read over its
//!     HTTP API (ubus JSON-RPC or the RutOS 7.x REST API). **Opt-in** via a
//!     per-interface `cellular_uplinks` config entry + a read-only credential
//!     stored in `secrets.json`.
//!
//! A single background [`spawn_cellular_poller`] task samples every source on
//! a slow cadence (off the data path, each sample time-bounded) and writes the
//! latest snapshot into a lock-free [`CellularCache`] keyed by kernel netdev
//! name. [`crate::util::network_interfaces::NetworkSampler`] joins that cache
//! onto each `NetworkInterfaceInfo` at health-tick time (zero added latency),
//! and `manager/client.rs` advertises the `"cellular"` capability whenever the
//! poller has at least one source.
//!
//! Wire shape is additive + backward-compatible: every field is `Option` with
//! `skip_serializing_if`, enums carry `#[serde(other)]` catch-all arms, and no
//! `WS_PROTOCOL_VERSION` bump is required (new fields, not new message types).
//! The manager mirrors these structs in
//! `manager_core::models::ws_protocol` — keep field names + serde defaults in
//! sync.

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

pub mod modem_manager;
pub mod rutos;

// ── Tuning constants (slow path; not data-path-sensitive) ──

/// How often the poller samples every source. Faster than the ~15 s health
/// tick so a fresh snapshot is usually waiting when the tick joins the cache.
const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Per-source sample budget. The HTTP / D-Bus read is wrapped in this timeout
/// so one hung source never stalls the poll cycle (or the data path — the
/// poller is wholly off it).
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(4);
/// A cached snapshot older than this is evicted, so the UI shows "no data"
/// (acquiring) rather than a frozen value when a source goes dark. Several
/// poll intervals of hysteresis avoids flapping on a single transient miss.
const STALE_AFTER: Duration = Duration::from_secs(60);
/// Consecutive RutOS poll failures before a single `cellular_uplink_unreachable`
/// Warning is emitted. The poll loop is the retry; no per-sample retries.
const UNREACHABLE_FAILS: u32 = 3;

// ── Wire data model (mirrored in manager-core::models::ws_protocol) ──

/// Live radio state for one cellular uplink, attached to the kernel netdev it
/// annotates via `NetworkInterfaceInfo.cellular`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellularMetrics {
    /// Provenance — drives the UI's source badge (modem vs router).
    pub source: CellularSourceKind,
    /// Normalized registration / SIM state.
    pub state: CellularRegState,
    /// Radio access technology, normalized: `"5gnr_sa"` | `"5gnr_nsa"` |
    /// `"lte"` | `"umts"` | `"hspa"` | `"gsm"` | … `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_tech: Option<String>,
    /// Operator display name (e.g. `"Telstra"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    /// PLMN — MCC+MNC, e.g. `"50501"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plmn: Option<String>,
    /// Serving band label, e.g. `"n78"`, `"B3"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<String>,
    /// Serving cell id (opaque string — can be long / hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    /// Signal strength / quality figures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<CellularSignal>,
    /// Whether the modem is roaming (extra-cost link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roaming: Option<bool>,
    /// Active SIM slot, when the source reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_slot: Option<u8>,
    /// Modem temperature in °C, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f32>,
    /// Session / period data usage in bytes, when the source reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_used_bytes: Option<u64>,
    /// Configured data cap in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_limit_bytes: Option<u64>,
    /// Unix milliseconds when this snapshot was sampled (drives staleness +
    /// the UI "age" hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at_unix_ms: Option<u64>,
}

/// Per-metric signal figures. All `Option` — sources populate what they have.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CellularSignal {
    /// Reference Signal Received Power (dBm). Primary strength figure for LTE/NR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsrp_dbm: Option<f32>,
    /// Reference Signal Received Quality (dB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsrq_db: Option<f32>,
    /// Signal-to-Interference-plus-Noise Ratio (dB). ModemManager reports this
    /// as `snr`; RutOS as `sinr` — both land here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sinr_db: Option<f32>,
    /// Received Signal Strength Indicator (dBm). Wideband; the only figure on
    /// 2G/3G.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi_dbm: Option<f32>,
    /// Normalized 0..=5 bar count derived from the worst-of RSRP/SINR (or RSSI
    /// as a fallback). The UI reads this for the glyph; it also has the raw
    /// figures for color + hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bars: Option<u8>,
}

/// Which source produced a [`CellularMetrics`] snapshot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CellularSourceKind {
    /// ModemManager (local D-Bus, USB / PCIe modem).
    ModemManager,
    /// RutOS router HTTP API.
    Rutos,
    #[serde(other)]
    Unknown,
}

/// Normalized registration / SIM state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CellularRegState {
    /// Registered on the home network.
    RegisteredHome,
    /// Registered while roaming.
    RegisteredRoaming,
    /// Scanning for a network.
    Searching,
    /// Registration denied by the network.
    Denied,
    /// No SIM present.
    SimMissing,
    /// SIM present but locked (PIN/PUK required).
    SimPinRequired,
    /// Radio disabled.
    Disabled,
    #[serde(other)]
    Unknown,
}

impl CellularRegState {
    /// Lower-case wire tag (for events / logs).
    pub fn as_str(self) -> &'static str {
        match self {
            CellularRegState::RegisteredHome => "registered_home",
            CellularRegState::RegisteredRoaming => "registered_roaming",
            CellularRegState::Searching => "searching",
            CellularRegState::Denied => "denied",
            CellularRegState::SimMissing => "sim_missing",
            CellularRegState::SimPinRequired => "sim_pin_required",
            CellularRegState::Disabled => "disabled",
            CellularRegState::Unknown => "unknown",
        }
    }
}

// ── Lock-free cache ──

/// Lock-free per-interface cache the poller writes and the health-tick reads.
///
/// `has_sources()` is keyed off the count of sources the poller is *managing*
/// (configured RutOS uplinks + auto-detected modems), not off whether the map
/// currently holds fresh data — so the `"cellular"` capability stays stable
/// while individual snapshots age out and refresh.
#[derive(Debug, Default)]
pub struct CellularCache {
    map: DashMap<String, CellularMetrics>,
    source_count: AtomicUsize,
}

impl CellularCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clone the latest snapshot for an interface, if any (non-stale entries
    /// only — the poller evicts stale ones each cycle).
    pub fn get(&self, iface: &str) -> Option<CellularMetrics> {
        self.map.get(iface).map(|e| e.value().clone())
    }

    /// Insert / replace the snapshot for an interface.
    pub fn insert(&self, iface: String, metrics: CellularMetrics) {
        self.map.insert(iface, metrics);
    }

    /// Drop entries whose `sampled_at_unix_ms` is older than `cutoff_ms`
    /// (entries with no timestamp are kept defensively).
    pub fn evict_older_than(&self, cutoff_ms: u64) {
        self.map
            .retain(|_, m| m.sampled_at_unix_ms.map(|t| t >= cutoff_ms).unwrap_or(true));
    }

    /// Record how many sources the poller is managing this cycle.
    pub fn set_source_count(&self, n: usize) {
        self.source_count.store(n, Ordering::Relaxed);
    }

    /// True when the poller has at least one source — drives the `"cellular"`
    /// capability advertisement.
    pub fn has_sources(&self) -> bool {
        self.source_count.load(Ordering::Relaxed) > 0
    }
}

// ── Shared mapping helpers ──

/// Current wall-clock in unix milliseconds (saturating; 0 before the epoch).
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Derive a 0..=5 bar count from the available signal figures. Worst-of
/// RSRP/SINR (both physical-layer quality axes); RSSI is the 2G/3G fallback.
/// Thresholds match the UI color bands (RSRP good > −90, SINR good > 13).
pub fn derive_bars(sig: &CellularSignal) -> Option<u8> {
    fn rsrp_bars(v: f32) -> u8 {
        if v >= -80.0 {
            5
        } else if v >= -90.0 {
            4
        } else if v >= -100.0 {
            3
        } else if v >= -105.0 {
            2
        } else if v >= -115.0 {
            1
        } else {
            0
        }
    }
    fn sinr_bars(v: f32) -> u8 {
        if v >= 20.0 {
            5
        } else if v >= 13.0 {
            4
        } else if v >= 6.0 {
            3
        } else if v >= 0.0 {
            2
        } else if v >= -5.0 {
            1
        } else {
            0
        }
    }
    fn rssi_bars(v: f32) -> u8 {
        if v >= -65.0 {
            5
        } else if v >= -75.0 {
            4
        } else if v >= -85.0 {
            3
        } else if v >= -95.0 {
            2
        } else if v >= -105.0 {
            1
        } else {
            0
        }
    }
    let mut bars: Option<u8> = None;
    let mut fold = |b: u8| bars = Some(bars.map_or(b, |cur| cur.min(b)));
    if let Some(v) = sig.rsrp_dbm {
        fold(rsrp_bars(v));
    }
    if let Some(v) = sig.sinr_db {
        fold(sinr_bars(v));
    }
    // RSSI only counts when there's no RSRP (LTE/NR RSSI is noisy / less useful
    // than RSRP, but on 2G/3G it's all we have).
    if sig.rsrp_dbm.is_none() {
        if let Some(v) = sig.rssi_dbm {
            fold(rssi_bars(v));
        }
    }
    bars
}

/// Normalize a vendor radio-access-technology string to the canonical tag set.
/// Tolerant of the many spellings RutOS / AT firmwares emit.
pub fn normalize_access_tech(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let compact: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    // 5G NR — distinguish SA from NSA where the firmware tells us.
    let is_5g = compact.contains("5g") || compact.contains("nr5g") || compact.contains("5gnr");
    if is_5g {
        if compact.contains("nsa") {
            return Some("5gnr_nsa".into());
        }
        if compact.contains("sa") {
            return Some("5gnr_sa".into());
        }
        return Some("5gnr".into());
    }
    if compact.contains("lte") || compact.contains("4g") {
        return Some("lte".into());
    }
    if compact.contains("hspa") || compact.contains("hsdpa") || compact.contains("hsupa") {
        return Some("hspa".into());
    }
    if compact.contains("umts") || compact.contains("wcdma") || compact.contains("3g") {
        return Some("umts".into());
    }
    if compact.contains("edge") {
        return Some("edge".into());
    }
    if compact.contains("gprs") {
        return Some("gprs".into());
    }
    if compact.contains("gsm") || compact.contains("2g") {
        return Some("gsm".into());
    }
    // Unknown but non-empty — surface it raw so the operator at least sees it.
    Some(s)
}

// ── Background poller ──

/// Spawn the cellular poller under the app's cancellation tree. Returns the
/// task handle (held for the process lifetime). Cheap to run when no source is
/// present — ModemManager auto-detection is one D-Bus call and `cellular_uplinks`
/// is empty by default, so the loop is a no-op on installs without modems.
pub fn spawn_cellular_poller(
    app_config: Arc<RwLock<AppConfig>>,
    cache: Arc<CellularCache>,
    events: EventSender,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        cellular_poller_loop(app_config, cache, events, cancel).await;
    })
}

/// Per-interface state the poller carries across cycles to debounce events.
#[derive(Default)]
struct CellEventState {
    last_reg: Option<CellularRegState>,
    /// True while the link is in the "degraded signal" state (hysteresis).
    degraded: bool,
    /// Consecutive sample failures (RutOS sources only).
    fail_count: u32,
    /// True while an unreachable Warning is outstanding (so we emit once).
    unreachable: bool,
}

async fn cellular_poller_loop(
    app_config: Arc<RwLock<AppConfig>>,
    cache: Arc<CellularCache>,
    events: EventSender,
    cancel: CancellationToken,
) {
    // RutOS sources are rebuilt only when the config changes (rebuilding a
    // reqwest client per cycle would re-do TLS setup needlessly); keyed by
    // interface. ModemManager is re-enumerated each cycle (cheap, catches
    // hot-plug). The D-Bus connection is established lazily and dropped on
    // error so the next cycle reconnects.
    let mut rutos_sources: HashMap<String, rutos::RutosSource> = HashMap::new();
    let mut rutos_sig: Option<String> = None;
    let mut ev_state: HashMap<String, CellEventState> = HashMap::new();
    #[cfg(target_os = "linux")]
    let mut mm_conn: Option<zbus::Connection> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        // 1. Read the live config (ground truth; picked up without any
        //    UpdateConfig hook — flows are never touched).
        let uplinks = { app_config.read().await.cellular_uplinks.clone() };

        // 2. Rebuild RutOS sources when the config changed.
        let sig = serde_json::to_string(&uplinks).unwrap_or_default();
        if rutos_sig.as_deref() != Some(sig.as_str()) {
            rutos_sources = rutos::build_sources(&uplinks);
            rutos_sig = Some(sig);
        }

        let mut produced: Vec<(String, CellularMetrics)> = Vec::new();
        let mut source_ifaces: HashSet<String> = HashSet::new();

        // 3. ModemManager (Linux, zero-config auto-detection).
        #[cfg(target_os = "linux")]
        {
            if mm_conn.is_none() {
                mm_conn = modem_manager::connect().await;
            }
            if let Some(conn) = mm_conn.clone() {
                match tokio::time::timeout(SAMPLE_TIMEOUT, modem_manager::sample_all(&conn)).await {
                    Ok(Ok(list)) => {
                        for (iface, m) in list {
                            source_ifaces.insert(iface.clone());
                            produced.push((iface, m));
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::debug!("ModemManager sample failed: {e}; will reconnect");
                        mm_conn = None;
                    }
                    Err(_) => {
                        tracing::debug!("ModemManager sample timed out; will reconnect");
                        mm_conn = None;
                    }
                }
            }
        }

        // 4. RutOS routers (opt-in). Each sample is time-bounded; a failure
        //    drops the snapshot (it ages out → UI shows "no data") and feeds
        //    the unreachable-event debounce.
        for (iface, src) in rutos_sources.iter() {
            source_ifaces.insert(iface.clone());
            let res = tokio::time::timeout(SAMPLE_TIMEOUT, src.sample()).await;
            match res {
                Ok(Some(m)) => {
                    produced.push((iface.clone(), m));
                }
                Ok(None) | Err(_) => {
                    note_unreachable(&events, &mut ev_state, iface);
                }
            }
        }

        // 5. Publish snapshots + emit state-change events.
        let now_ms = now_unix_ms();
        for (iface, mut m) in produced {
            m.sampled_at_unix_ms = Some(now_ms);
            note_success(&events, &mut ev_state, &iface, &m);
            cache.insert(iface, m);
        }

        // 6. Age out stale snapshots; publish the managed-source count.
        let cutoff = now_ms.saturating_sub(STALE_AFTER.as_millis() as u64);
        cache.evict_older_than(cutoff);
        cache.set_source_count(source_ifaces.len());
        // Forget event state for interfaces no longer managed.
        ev_state.retain(|k, _| source_ifaces.contains(k));
    }
    tracing::debug!("cellular poller stopped");
}

/// On a successful sample: emit registration-change + signal-degraded events
/// (debounced) and clear any outstanding unreachable Warning.
fn note_success(
    events: &EventSender,
    ev_state: &mut HashMap<String, CellEventState>,
    iface: &str,
    m: &CellularMetrics,
) {
    let st = ev_state.entry(iface.to_string()).or_default();

    // Recovery from unreachable.
    if st.unreachable {
        events.emit_with_details(
            EventSeverity::Info,
            category::CELLULAR,
            format!("cellular uplink '{iface}' reachable again"),
            None,
            serde_json::json!({ "error_code": "cellular_uplink_recovered", "interface": iface }),
        );
        st.unreachable = false;
    }
    st.fail_count = 0;

    // Registration-state transition.
    if st.last_reg != Some(m.state) {
        // Skip the very first observation (no prior state → not a transition).
        if let Some(prev) = st.last_reg {
            let sev = match m.state {
                CellularRegState::Denied
                | CellularRegState::SimMissing
                | CellularRegState::SimPinRequired => EventSeverity::Warning,
                _ => EventSeverity::Info,
            };
            events.emit_with_details(
                sev,
                category::CELLULAR,
                format!(
                    "cellular uplink '{iface}' registration {} → {}",
                    prev.as_str(),
                    m.state.as_str()
                ),
                None,
                serde_json::json!({
                    "error_code": "cellular_registration_changed",
                    "interface": iface,
                    "from": prev.as_str(),
                    "to": m.state.as_str(),
                    "operator": m.operator,
                    "access_tech": m.access_tech,
                    "roaming": m.roaming,
                }),
            );
        }
        st.last_reg = Some(m.state);
    }

    // Signal-degraded with hysteresis: enter at ≤1 bar, leave at ≥3 bars.
    if let Some(bars) = m.signal.as_ref().and_then(|s| s.bars) {
        if bars <= 1 && !st.degraded {
            st.degraded = true;
            events.emit_with_details(
                EventSeverity::Warning,
                category::CELLULAR,
                format!("cellular uplink '{iface}' signal degraded ({bars}/5 bars)"),
                None,
                serde_json::json!({
                    "error_code": "cellular_signal_degraded",
                    "interface": iface,
                    "bars": bars,
                    "rsrp_dbm": m.signal.as_ref().and_then(|s| s.rsrp_dbm),
                    "sinr_db": m.signal.as_ref().and_then(|s| s.sinr_db),
                }),
            );
        } else if bars >= 3 && st.degraded {
            st.degraded = false;
            events.emit_with_details(
                EventSeverity::Info,
                category::CELLULAR,
                format!("cellular uplink '{iface}' signal recovered ({bars}/5 bars)"),
                None,
                serde_json::json!({
                    "error_code": "cellular_signal_recovered",
                    "interface": iface,
                    "bars": bars,
                }),
            );
        }
    }
}

/// On a failed RutOS sample: bump the consecutive-failure counter and emit a
/// single Warning once it crosses [`UNREACHABLE_FAILS`].
fn note_unreachable(
    events: &EventSender,
    ev_state: &mut HashMap<String, CellEventState>,
    iface: &str,
) {
    let st = ev_state.entry(iface.to_string()).or_default();
    st.fail_count = st.fail_count.saturating_add(1);
    if st.fail_count >= UNREACHABLE_FAILS && !st.unreachable {
        st.unreachable = true;
        events.emit_with_details(
            EventSeverity::Warning,
            category::CELLULAR,
            format!("cellular uplink '{iface}' unreachable"),
            None,
            serde_json::json!({
                "error_code": "cellular_uplink_unreachable",
                "interface": iface,
                "consecutive_failures": st.fail_count,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_worst_of_rsrp_sinr() {
        // Good RSRP but terrible SINR → worst-of pulls bars down.
        let sig = CellularSignal {
            rsrp_dbm: Some(-75.0),
            sinr_db: Some(-3.0),
            ..Default::default()
        };
        assert_eq!(derive_bars(&sig), Some(1));
        // Both strong.
        let sig = CellularSignal {
            rsrp_dbm: Some(-78.0),
            sinr_db: Some(22.0),
            ..Default::default()
        };
        assert_eq!(derive_bars(&sig), Some(5));
        // Nothing → None.
        assert_eq!(derive_bars(&CellularSignal::default()), None);
        // RSSI-only (2G/3G) fallback.
        let sig = CellularSignal {
            rssi_dbm: Some(-70.0),
            ..Default::default()
        };
        assert_eq!(derive_bars(&sig), Some(4));
    }

    #[test]
    fn access_tech_normalization() {
        assert_eq!(normalize_access_tech("5G-SA").as_deref(), Some("5gnr_sa"));
        assert_eq!(normalize_access_tech("5G-NSA").as_deref(), Some("5gnr_nsa"));
        assert_eq!(normalize_access_tech("NR5G").as_deref(), Some("5gnr"));
        assert_eq!(normalize_access_tech("LTE").as_deref(), Some("lte"));
        assert_eq!(normalize_access_tech("WCDMA").as_deref(), Some("umts"));
        assert_eq!(normalize_access_tech("").as_deref(), None);
    }

    #[test]
    fn cache_staleness_and_sources() {
        let cache = CellularCache::new();
        assert!(!cache.has_sources());
        cache.set_source_count(2);
        assert!(cache.has_sources());
        let mut m = CellularMetrics {
            source: CellularSourceKind::Rutos,
            state: CellularRegState::RegisteredHome,
            access_tech: None,
            operator: None,
            plmn: None,
            band: None,
            cell_id: None,
            signal: None,
            roaming: None,
            sim_slot: None,
            temperature_c: None,
            data_used_bytes: None,
            data_limit_bytes: None,
            sampled_at_unix_ms: Some(1_000),
        };
        cache.insert("eno4".into(), m.clone());
        assert!(cache.get("eno4").is_some());
        // Cutoff after the sample time → evicted.
        cache.evict_older_than(2_000);
        assert!(cache.get("eno4").is_none());
        // Fresh sample survives the same cutoff.
        m.sampled_at_unix_ms = Some(3_000);
        cache.insert("eno4".into(), m);
        cache.evict_older_than(2_000);
        assert!(cache.get("eno4").is_some());
    }

    #[test]
    fn metrics_serde_roundtrip_and_catchall() {
        let m = CellularMetrics {
            source: CellularSourceKind::ModemManager,
            state: CellularRegState::RegisteredRoaming,
            access_tech: Some("lte".into()),
            operator: Some("Telstra".into()),
            plmn: Some("50501".into()),
            band: Some("B28".into()),
            cell_id: Some("0x1A2B3C".into()),
            signal: Some(CellularSignal {
                rsrp_dbm: Some(-95.0),
                rsrq_db: Some(-11.0),
                sinr_db: Some(8.0),
                rssi_dbm: Some(-67.0),
                bars: Some(3),
            }),
            roaming: Some(true),
            sim_slot: Some(1),
            temperature_c: Some(41.5),
            data_used_bytes: Some(1234),
            data_limit_bytes: None,
            sampled_at_unix_ms: Some(42),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: CellularMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        // Unknown enum tags fall through to the catch-all arms.
        let v: CellularSourceKind = serde_json::from_str("\"future_radio\"").unwrap();
        assert_eq!(v, CellularSourceKind::Unknown);
        let s: CellularRegState = serde_json::from_str("\"some_new_state\"").unwrap();
        assert_eq!(s, CellularRegState::Unknown);
    }
}
