// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! ModemManager cellular source — auto-detected, zero-config.
//!
//! Reads published, **unprivileged** properties from `org.freedesktop.ModemManager1`
//! over the system D-Bus (pure-Rust `zbus`, no `mmcli`, no C deps). One
//! `GetManagedObjects` call enumerates modems and maps each to its kernel
//! netdev (via the modem's `net`-type port); typed property reads then fill a
//! plain [`ModemSnapshot`] which [`snapshot_to_metrics`] maps to the unified
//! [`CellularMetrics`] shape.
//!
//! The mapping ([`snapshot_to_metrics`] + helpers) is pure and unit-tested
//! against synthetic snapshots (LTE / 5G-SA / 5G-NSA / missing-SIM /
//! PIN-required); only the D-Bus extraction is Linux-gated and untested
//! without a live modem.
//!
//! D-Bus references:
//!   - <https://www.freedesktop.org/software/ModemManager/doc/latest/ModemManager/>
//!   - Enums below are the stable `MMModemState` / `MMModemAccessTechnology` /
//!     `MMModem3gppRegistrationState` / `MMModemBand` / `MMModemPortType`
//!     numeric values.

use super::{CellularMetrics, CellularRegState, CellularSignal, CellularSourceKind, derive_bars};

// ── MMModemState (`State`, type 'i') ──
const MM_STATE_FAILED: i32 = -1;
const MM_STATE_LOCKED: i32 = 2;
const MM_STATE_DISABLED: i32 = 3;
const MM_STATE_DISABLING: i32 = 4;
const MM_STATE_ENABLING: i32 = 5;
const MM_STATE_ENABLED: i32 = 6;
const MM_STATE_SEARCHING: i32 = 7;
const MM_STATE_REGISTERED: i32 = 8;
const MM_STATE_DISCONNECTING: i32 = 9;
const MM_STATE_CONNECTING: i32 = 10;
const MM_STATE_CONNECTED: i32 = 11;
const MM_STATE_INITIALIZING: i32 = 1;

// ── MMModemStateFailedReason (`StateFailedReason`, type 'u') ──
const MM_FAILED_REASON_SIM_MISSING: u32 = 2;

// ── MMModemLock (`UnlockRequired`, type 'u') — SIM credential locks ──
// 0=unknown, 1=none, 2=sim-pin, 3=sim-pin2, 4=sim-puk, 5=sim-puk2 (then
// service-provider / corporate locks). 2..=5 are the SIM PIN/PUK locks.
const MM_LOCK_SIM_PIN: u32 = 2;
const MM_LOCK_SIM_PUK2: u32 = 5;

// ── MMModemAccessTechnology bitmask (`AccessTechnologies`, type 'u') ──
const MM_AT_GSM: u32 = 1 << 1;
const MM_AT_GSM_COMPACT: u32 = 1 << 2;
const MM_AT_GPRS: u32 = 1 << 3;
const MM_AT_EDGE: u32 = 1 << 4;
const MM_AT_UMTS: u32 = 1 << 5;
const MM_AT_HSDPA: u32 = 1 << 6;
const MM_AT_HSUPA: u32 = 1 << 7;
const MM_AT_HSPA: u32 = 1 << 8;
const MM_AT_HSPA_PLUS: u32 = 1 << 9;
const MM_AT_LTE: u32 = 1 << 14;
const MM_AT_5GNR: u32 = 1 << 15;

// ── MMModem3gppRegistrationState (`RegistrationState`, type 'u') ──
const MM_REG_HOME: u32 = 1;
const MM_REG_SEARCHING: u32 = 2;
const MM_REG_DENIED: u32 = 3;
const MM_REG_ROAMING: u32 = 5;
const MM_REG_HOME_SMS_ONLY: u32 = 6;
const MM_REG_ROAMING_SMS_ONLY: u32 = 7;
const MM_REG_HOME_CSFB: u32 = 9;
const MM_REG_ROAMING_CSFB: u32 = 10;

// ── MMModemPortType (`Ports`, the 'u' in each `(su)`) ──
#[cfg(target_os = "linux")]
const MM_PORT_TYPE_NET: u32 = 2;

/// Plain intermediate captured from the modem's D-Bus properties. Decoupled
/// from zbus so [`snapshot_to_metrics`] is pure + unit-testable.
#[derive(Debug, Clone, Default)]
pub struct ModemSnapshot {
    pub state: i32,
    pub state_failed_reason: u32,
    /// `MMModemLock` — non-zero (and not `none`=1) means the SIM is locked.
    pub unlock_required: u32,
    pub access_tech: u32,
    /// `SignalQuality` percent (0..=100); the recent-flag is dropped.
    pub signal_quality_pct: Option<u8>,
    pub current_bands: Vec<u32>,
    pub has_sim: bool,
    pub reg_state: Option<u32>,
    pub operator_name: Option<String>,
    pub operator_code: Option<String>,
    pub sim_slot: Option<u8>,
    pub rsrp_dbm: Option<f32>,
    pub rsrq_db: Option<f32>,
    pub snr_db: Option<f32>,
    pub rssi_dbm: Option<f32>,
}

/// Map the snapshot to the unified wire shape (pure).
pub fn snapshot_to_metrics(s: &ModemSnapshot) -> CellularMetrics {
    let state = derive_state(s);
    let roaming = s.reg_state.map(is_roaming_reg);
    let signal = build_signal(s);
    CellularMetrics {
        source: CellularSourceKind::ModemManager,
        state,
        access_tech: access_tech_label(s.access_tech),
        operator: s.operator_name.clone().filter(|s| !s.is_empty()),
        plmn: s.operator_code.clone().filter(|s| !s.is_empty()),
        band: band_label(&s.current_bands, s.access_tech),
        cell_id: None, // requires Location Setup + polkit — out of scope (unprivileged read only)
        signal,
        roaming,
        sim_slot: s.sim_slot,
        temperature_c: None, // not exposed by the standard MM interfaces
        data_used_bytes: None,
        data_limit_bytes: None,
        last_error: None, // a present modem isn't a poll failure
        keeper_active: None, // stamped by the poller (host-keeper liveness)
        sampled_at_unix_ms: None, // stamped by the poller
    }
}

fn is_roaming_reg(reg: u32) -> bool {
    matches!(
        reg,
        MM_REG_ROAMING | MM_REG_ROAMING_SMS_ONLY | MM_REG_ROAMING_CSFB
    )
}

fn derive_state(s: &ModemSnapshot) -> CellularRegState {
    // No SIM short-circuits everything else.
    if !s.has_sim || s.state_failed_reason == MM_FAILED_REASON_SIM_MISSING {
        return CellularRegState::SimMissing;
    }
    // A SIM PIN/PUK lock blocks registration — but only trust `UnlockRequired`
    // while the modem hasn't gotten past enable. Some firmwares leave a stale
    // sim-pin `UnlockRequired` set on an already-connected modem; a modem that
    // is ENABLED/REGISTERED/CONNECTED demonstrably has an unlocked SIM (you
    // can't register past a locked one), so the connection state wins — else a
    // working modem mis-reports as `sim_pin_required`.
    let past_enable = matches!(
        s.state,
        MM_STATE_ENABLED
            | MM_STATE_REGISTERED
            | MM_STATE_DISCONNECTING
            | MM_STATE_CONNECTING
            | MM_STATE_CONNECTED
    );
    if !past_enable && (MM_LOCK_SIM_PIN..=MM_LOCK_SIM_PUK2).contains(&s.unlock_required) {
        return CellularRegState::SimPinRequired;
    }
    match s.state {
        MM_STATE_FAILED => CellularRegState::Unknown,
        // Locked for a non-SIM reason (service-provider / corporate) — still
        // needs an unlock before it can register.
        MM_STATE_LOCKED => CellularRegState::SimPinRequired,
        MM_STATE_DISABLED | MM_STATE_DISABLING => CellularRegState::Disabled,
        MM_STATE_SEARCHING => CellularRegState::Searching,
        MM_STATE_INITIALIZING | MM_STATE_ENABLING => CellularRegState::Searching,
        // ENABLED(6), REGISTERED(8), DISCONNECTING(9), CONNECTING(10),
        // CONNECTED(11) → registered; refine with the 3GPP reg state.
        // (SEARCHING(7) is handled above; DISABLED/DISABLING below 6.)
        MM_STATE_ENABLED
        | MM_STATE_REGISTERED
        | MM_STATE_DISCONNECTING
        | MM_STATE_CONNECTING
        | MM_STATE_CONNECTED => match s.reg_state {
            Some(MM_REG_SEARCHING) => CellularRegState::Searching,
            Some(MM_REG_DENIED) => CellularRegState::Denied,
            Some(r) if is_roaming_reg(r) => CellularRegState::RegisteredRoaming,
            Some(MM_REG_HOME | MM_REG_HOME_SMS_ONLY | MM_REG_HOME_CSFB) => {
                CellularRegState::RegisteredHome
            }
            // Registered/connected per the modem state but no 3GPP detail →
            // assume home.
            _ => CellularRegState::RegisteredHome,
        },
        _ => CellularRegState::Unknown,
    }
}

/// Best radio-access-technology label from the bitmask. 5G presence alongside
/// LTE means NSA (LTE anchor + NR); 5G alone is SA.
fn access_tech_label(at: u32) -> Option<String> {
    if at == 0 {
        return None;
    }
    let has_5g = at & MM_AT_5GNR != 0;
    let has_lte = at & MM_AT_LTE != 0;
    if has_5g && has_lte {
        return Some("5gnr_nsa".into());
    }
    if has_5g {
        return Some("5gnr_sa".into());
    }
    if has_lte {
        return Some("lte".into());
    }
    if at & (MM_AT_HSPA_PLUS | MM_AT_HSPA | MM_AT_HSUPA | MM_AT_HSDPA) != 0 {
        return Some("hspa".into());
    }
    if at & MM_AT_UMTS != 0 {
        return Some("umts".into());
    }
    if at & MM_AT_EDGE != 0 {
        return Some("edge".into());
    }
    if at & MM_AT_GPRS != 0 {
        return Some("gprs".into());
    }
    if at & (MM_AT_GSM | MM_AT_GSM_COMPACT) != 0 {
        return Some("gsm".into());
    }
    None
}

/// Label a serving band from `CurrentBands`, preferring an NR band when 5G is
/// active. Only the EUTRAN (LTE) and NGRAN (5G NR) ranges are labelled — the
/// bands operators care about. `MM_MODEM_BAND_EUTRAN_1 = 31`,
/// `MM_MODEM_BAND_NGRAN_1 = 301`.
fn band_label(bands: &[u32], access_tech: u32) -> Option<String> {
    let nr = |v: u32| (301..=599).contains(&v).then(|| format!("n{}", v - 300));
    let eutran = |v: u32| (31..=130).contains(&v).then(|| format!("B{}", v - 30));
    let prefer_nr = access_tech & MM_AT_5GNR != 0;
    if prefer_nr {
        if let Some(b) = bands.iter().find_map(|&v| nr(v)) {
            return Some(b);
        }
    }
    if let Some(b) = bands.iter().find_map(|&v| eutran(v)) {
        return Some(b);
    }
    // Fall back to any NR band even when the access-tech bit wasn't set.
    bands.iter().find_map(|&v| nr(v))
}

// ModemManager reports an out-of-range sentinel (e.g. i16::MIN → -32768, or
// -3276.8 once the 0.1 scale is applied) for any metric it isn't currently
// measuring — typically before extended signal sampling (`--signal-setup`) has
// warmed up, or for an idle 5G-NSA NR carrier whose secondary cell the network
// hasn't added (no data demand). These ranges are the physically-plausible
// window for each metric; readings outside are dropped to None so the UI shows
// "—" instead of a bogus -32768 dBm (which would also poison the bar derivation
// and the degraded-signal alarm).
const RSRP_RANGE: (f32, f32) = (-156.0, -30.0);
const RSRQ_RANGE: (f32, f32) = (-45.0, 0.0);
const SINR_RANGE: (f32, f32) = (-30.0, 50.0);
const RSSI_RANGE: (f32, f32) = (-130.0, -30.0);

/// Keep `v` only when it falls inside `range`; otherwise it's a sentinel → None.
fn in_range(v: Option<f32>, range: (f32, f32)) -> Option<f32> {
    v.filter(|x| (range.0..=range.1).contains(x))
}

/// One radio tech's raw `(rsrp, rsrq, snr, rssi)` readings as published by
/// ModemManager's per-tech `Modem.Signal` dict.
pub(crate) type SignalReads = (Option<f32>, Option<f32>, Option<f32>, Option<f32>);

/// Pick the first tech (candidates are in preference order, e.g.
/// `Nr5g ≻ Lte ≻ Umts ≻ Gsm`) carrying at least one physically-plausible
/// reading. Returns its raw readings, or `None` when no tech has a usable
/// figure (extended sampling not warmed up). This is what lets an idle 5G-NSA
/// modem — whose NR dict is present but all-sentinel while the LTE anchor is
/// live — fall through to LTE instead of blanking the figures.
pub(crate) fn pick_plausible_tech(candidates: &[SignalReads]) -> Option<SignalReads> {
    candidates.iter().copied().find(|&(rsrp, rsrq, snr, rssi)| {
        in_range(rsrp, RSRP_RANGE).is_some()
            || in_range(rsrq, RSRQ_RANGE).is_some()
            || in_range(snr, SINR_RANGE).is_some()
            || in_range(rssi, RSSI_RANGE).is_some()
    })
}

fn build_signal(s: &ModemSnapshot) -> Option<CellularSignal> {
    let mut sig = CellularSignal {
        rsrp_dbm: in_range(s.rsrp_dbm, RSRP_RANGE),
        rsrq_db: in_range(s.rsrq_db, RSRQ_RANGE),
        sinr_db: in_range(s.snr_db, SINR_RANGE),
        rssi_dbm: in_range(s.rssi_dbm, RSSI_RANGE),
        bars: None,
    };
    sig.bars = derive_bars(&sig);
    // If the per-tech signal dict was empty, fall back to the coarse
    // SignalQuality percentage for the bar glyph.
    if sig.bars.is_none() {
        if let Some(pct) = s.signal_quality_pct {
            sig.bars = Some(((pct as f32 / 20.0).round() as u8).min(5));
        }
    }
    let empty = sig.rsrp_dbm.is_none()
        && sig.rsrq_db.is_none()
        && sig.sinr_db.is_none()
        && sig.rssi_dbm.is_none()
        && sig.bars.is_none();
    (!empty).then_some(sig)
}

// ───────────────────────── D-Bus extraction (Linux) ─────────────────────────

#[cfg(target_os = "linux")]
mod dbus {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    const MM_DEST: &str = "org.freedesktop.ModemManager1";
    const MM_ROOT: &str = "/org/freedesktop/ModemManager1";
    const IFACE_MODEM: &str = "org.freedesktop.ModemManager1.Modem";
    const IFACE_3GPP: &str = "org.freedesktop.ModemManager1.Modem.Modem3gpp";
    const IFACE_SIGNAL: &str = "org.freedesktop.ModemManager1.Modem.Signal";

    type ManagedObjects =
        HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

    /// Connect to the system bus. Returns `None` when D-Bus / ModemManager is
    /// absent (containers, non-systemd hosts) — the caller treats that as
    /// "no modem source".
    pub async fn connect() -> Option<zbus::Connection> {
        match zbus::Connection::system().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::debug!("system D-Bus unavailable ({e}); no ModemManager telemetry");
                None
            }
        }
    }

    /// Enumerate every modem and return `(netdev, metrics)` for each that maps
    /// to a kernel `net` port.
    pub async fn sample_all(
        conn: &zbus::Connection,
    ) -> anyhow::Result<Vec<(String, CellularMetrics)>> {
        let om = zbus::Proxy::new(
            conn,
            MM_DEST,
            MM_ROOT,
            "org.freedesktop.DBus.ObjectManager",
        )
        .await?;
        let managed: ManagedObjects = om.call("GetManagedObjects", &()).await?;

        let mut out = Vec::new();
        for (path, ifaces) in managed {
            if !ifaces.contains_key(IFACE_MODEM) {
                continue;
            }
            match sample_modem(conn, path.as_str()).await {
                Ok(Some((netdev, metrics))) => out.push((netdev, metrics)),
                Ok(None) => {} // no net port → nothing to annotate
                Err(e) => tracing::debug!("modem {} sample failed: {e}", path.as_str()),
            }
        }
        Ok(out)
    }

    async fn sample_modem(
        conn: &zbus::Connection,
        path: &str,
    ) -> anyhow::Result<Option<(String, CellularMetrics)>> {
        let modem = zbus::Proxy::new(conn, MM_DEST, path, IFACE_MODEM).await?;

        // Map this modem to its kernel netdev via the `net`-type port.
        let ports: Vec<(String, u32)> = modem.get_property("Ports").await.unwrap_or_default();
        let Some(netdev) = ports
            .into_iter()
            .find(|(_, ty)| *ty == MM_PORT_TYPE_NET)
            .map(|(name, _)| name)
        else {
            return Ok(None);
        };

        let mut snap = ModemSnapshot {
            state: modem.get_property::<i32>("State").await.unwrap_or(0),
            state_failed_reason: modem
                .get_property::<u32>("StateFailedReason")
                .await
                .unwrap_or(0),
            unlock_required: modem.get_property::<u32>("UnlockRequired").await.unwrap_or(0),
            access_tech: modem
                .get_property::<u32>("AccessTechnologies")
                .await
                .unwrap_or(0),
            current_bands: modem.get_property::<Vec<u32>>("CurrentBands").await.unwrap_or_default(),
            ..Default::default()
        };

        if let Ok((pct, _recent)) = modem.get_property::<(u32, bool)>("SignalQuality").await {
            snap.signal_quality_pct = Some(pct.min(100) as u8);
        }

        // SIM presence + active slot.
        if let Ok(sim) = modem.get_property::<OwnedObjectPath>("Sim").await {
            snap.has_sim = sim.as_str() != "/";
        }
        if let Ok(slot) = modem.get_property::<u32>("PrimarySimSlot").await {
            if slot > 0 {
                snap.sim_slot = Some(slot.min(u8::MAX as u32) as u8);
            }
        }

        // 3GPP registration + operator (absent until registered).
        if let Ok(m3gpp) = zbus::Proxy::new(conn, MM_DEST, path, IFACE_3GPP).await {
            snap.reg_state = m3gpp.get_property::<u32>("RegistrationState").await.ok();
            snap.operator_name = m3gpp.get_property::<String>("OperatorName").await.ok();
            snap.operator_code = m3gpp.get_property::<String>("OperatorCode").await.ok();
        }

        // Extended signal (only populated when signal polling is enabled).
        if let Ok(signal) = zbus::Proxy::new(conn, MM_DEST, path, IFACE_SIGNAL).await {
            fill_signal(&signal, &mut snap).await;
        }

        Ok(Some((netdev, snapshot_to_metrics(&snap))))
    }

    /// Read the per-tech signal dicts in preference order (`Nr5g` ≻ `Lte` ≻
    /// `Umts` ≻ `Gsm`) and keep the first that carries a real reading. Reading
    /// is unprivileged; if nothing plausible is published (sampling not warmed
    /// up), best-effort `Setup` to enable polling for next time (ignoring
    /// PermissionDenied).
    ///
    /// We collect all techs rather than committing to the first non-empty dict,
    /// because an idle 5G-NSA modem publishes its NR carrier as all-sentinel
    /// (the network only adds the NR secondary cell under data demand) while the
    /// LTE anchor reports live figures — [`pick_plausible_tech`] then falls
    /// through NR → LTE instead of blanking RSRP/RSRQ/SINR.
    async fn fill_signal(signal: &zbus::Proxy<'_>, snap: &mut ModemSnapshot) {
        let mut candidates: Vec<SignalReads> = Vec::with_capacity(4);
        for tech in ["Nr5g", "Lte", "Umts", "Gsm"] {
            let Ok(dict) = signal
                .get_property::<HashMap<String, OwnedValue>>(tech)
                .await
            else {
                continue;
            };
            if dict.is_empty() {
                continue;
            }
            candidates.push((
                dict_f32(&dict, "rsrp"),
                dict_f32(&dict, "rsrq"),
                dict_f32(&dict, "snr"),
                dict_f32(&dict, "rssi"),
            ));
        }
        if let Some((rsrp, rsrq, snr, rssi)) = pick_plausible_tech(&candidates) {
            snap.rsrp_dbm = rsrp;
            snap.rsrq_db = rsrq;
            snap.snr_db = snr;
            snap.rssi_dbm = rssi;
            return;
        }
        // Nothing plausible published — try to enable extended sampling (5 s).
        let _ = signal.call::<_, _, ()>("Setup", &(5u32)).await;
    }

    fn dict_f32(d: &HashMap<String, OwnedValue>, key: &str) -> Option<f32> {
        d.get(key).and_then(|v| value_to_f64(v)).map(|f| f as f32)
    }

    /// Numeric coercion across the variant types ModemManager uses (`d` mostly,
    /// but some firmwares report integers). Version-stable variant match.
    fn value_to_f64(v: &OwnedValue) -> Option<f64> {
        match &**v {
            Value::F64(x) => Some(*x),
            Value::I16(x) => Some(*x as f64),
            Value::U16(x) => Some(*x as f64),
            Value::I32(x) => Some(*x as f64),
            Value::U32(x) => Some(*x as f64),
            Value::I64(x) => Some(*x as f64),
            Value::U64(x) => Some(*x as f64),
            Value::U8(x) => Some(*x as f64),
            _ => None,
        }
    }
}

#[cfg(target_os = "linux")]
pub use dbus::{connect, sample_all};

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ModemSnapshot {
        ModemSnapshot {
            has_sim: true,
            ..Default::default()
        }
    }

    #[test]
    fn lte_home_registered() {
        let mut s = base();
        s.state = MM_STATE_CONNECTED;
        s.access_tech = MM_AT_LTE;
        s.reg_state = Some(MM_REG_HOME);
        s.current_bands = vec![34 /* EUTRAN B4 */];
        s.rsrp_dbm = Some(-95.0);
        s.rsrq_db = Some(-11.0);
        s.snr_db = Some(8.0);
        s.operator_name = Some("Telstra".into());
        s.operator_code = Some("50501".into());
        let m = snapshot_to_metrics(&s);
        assert_eq!(m.state, CellularRegState::RegisteredHome);
        assert_eq!(m.access_tech.as_deref(), Some("lte"));
        assert_eq!(m.band.as_deref(), Some("B4"));
        assert_eq!(m.operator.as_deref(), Some("Telstra"));
        assert_eq!(m.plmn.as_deref(), Some("50501"));
        assert_eq!(m.roaming, Some(false));
        let sig = m.signal.unwrap();
        assert_eq!(sig.rsrp_dbm, Some(-95.0));
        assert_eq!(sig.sinr_db, Some(8.0));
        assert!(sig.bars.is_some());
    }

    #[test]
    fn nsa_vs_sa() {
        assert_eq!(access_tech_label(MM_AT_5GNR | MM_AT_LTE).as_deref(), Some("5gnr_nsa"));
        assert_eq!(access_tech_label(MM_AT_5GNR).as_deref(), Some("5gnr_sa"));
        assert_eq!(access_tech_label(MM_AT_LTE).as_deref(), Some("lte"));
        assert_eq!(access_tech_label(MM_AT_UMTS).as_deref(), Some("umts"));
        assert_eq!(access_tech_label(0), None);
    }

    #[test]
    fn nr_band_preferred_when_5g() {
        // n78 (378) + B3 (33) present, 5G active → prefer NR.
        let label = band_label(&[33, 378], MM_AT_5GNR);
        assert_eq!(label.as_deref(), Some("n78"));
        // LTE only → EUTRAN band.
        let label = band_label(&[33, 378], MM_AT_LTE);
        assert_eq!(label.as_deref(), Some("B3"));
    }

    #[test]
    fn roaming_registered() {
        let mut s = base();
        s.state = MM_STATE_CONNECTED;
        s.reg_state = Some(MM_REG_ROAMING);
        let m = snapshot_to_metrics(&s);
        assert_eq!(m.state, CellularRegState::RegisteredRoaming);
        assert_eq!(m.roaming, Some(true));
    }

    #[test]
    fn missing_sim() {
        let mut s = base();
        s.has_sim = false;
        s.state = MM_STATE_FAILED;
        s.state_failed_reason = MM_FAILED_REASON_SIM_MISSING;
        let m = snapshot_to_metrics(&s);
        assert_eq!(m.state, CellularRegState::SimMissing);
    }

    #[test]
    fn pin_required() {
        let mut s = base();
        s.state = MM_STATE_LOCKED;
        s.unlock_required = 2; // SIM-PIN
        let m = snapshot_to_metrics(&s);
        assert_eq!(m.state, CellularRegState::SimPinRequired);
    }

    #[test]
    fn stale_pin_lock_on_connected_modem_is_ignored() {
        // A connected modem that still reports a residual sim-pin UnlockRequired
        // must read as registered, NOT sim_pin_required — you can't register a
        // locked SIM, so the connection state is authoritative.
        let mut s = base();
        s.state = MM_STATE_CONNECTED;
        s.unlock_required = 2; // stale SIM-PIN flag
        s.reg_state = Some(MM_REG_HOME);
        assert_eq!(snapshot_to_metrics(&s).state, CellularRegState::RegisteredHome);
        // But a genuinely locked modem (not past enable) still surfaces the lock.
        s.state = MM_STATE_DISABLED;
        assert_eq!(snapshot_to_metrics(&s).state, CellularRegState::SimPinRequired);
    }

    #[test]
    fn searching_and_disabled() {
        let mut s = base();
        s.state = MM_STATE_SEARCHING;
        assert_eq!(snapshot_to_metrics(&s).state, CellularRegState::Searching);
        s.state = MM_STATE_DISABLED;
        assert_eq!(snapshot_to_metrics(&s).state, CellularRegState::Disabled);
    }

    #[test]
    fn signal_quality_fallback_to_bars() {
        // No dBm figures, but a SignalQuality percent → bars only.
        let mut s = base();
        s.state = MM_STATE_CONNECTED;
        s.signal_quality_pct = Some(80);
        let m = snapshot_to_metrics(&s);
        let sig = m.signal.unwrap();
        assert_eq!(sig.rsrp_dbm, None);
        assert_eq!(sig.bars, Some(4));
    }

    #[test]
    fn signal_falls_through_sentinel_nr_to_lte() {
        // Idle 5G-NSA: the NR carrier publishes only sentinels while the LTE
        // anchor is live. We must skip NR and report the real LTE figures.
        let nr_idle: SignalReads = (
            Some(-32768.0),
            Some(-3276.8),
            Some(-3276.8),
            Some(-32768.0),
        );
        let lte_live: SignalReads = (Some(-100.0), Some(-13.0), Some(-2.0), Some(-70.0));
        assert_eq!(pick_plausible_tech(&[nr_idle, lte_live]), Some(lte_live));

        // When the NR carrier IS measured, it's preferred (first in order).
        let nr_live: SignalReads = (Some(-85.0), Some(-11.0), Some(15.0), Some(-60.0));
        assert_eq!(pick_plausible_tech(&[nr_live, lte_live]), Some(nr_live));

        // All sentinel → None (caller then best-effort re-arms sampling).
        assert_eq!(pick_plausible_tech(&[nr_idle]), None);
        assert_eq!(pick_plausible_tech(&[]), None);
    }
}
