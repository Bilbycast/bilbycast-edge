//! Per-port DeckLink hardware status, cached for the health tick.
//!
//! `IDeckLinkStatus` needs no open handle, so every SDI port on a card can be
//! reported — including ports with no flow, and ports another process holds.
//! That is what lets the manager answer "which SDI inputs have signal?" for the
//! whole card rather than only for running flows.
//!
//! A full 8-device sweep measures ~25 ms on a DeckLink Quad 2 (each
//! `device_status` call walks the SDK's device iterator). That is far too much
//! to run on the reactor during a health tick, so this mirrors
//! `display::spawn_display_poller` and `util::cellular`: a slow background
//! poller on a blocking thread swaps a lock-free snapshot, and the health tick
//! just clones it.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use arc_swap::ArcSwap;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// Re-probe cadence. A little faster than the ~15 s health tick so a fresh
/// snapshot is usually waiting when the tick reads it, and fast enough that an
/// operator patching a cable sees it within one cycle.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// One SDI port's hardware status, as reported to the manager.
///
/// Every status field is `Option` and omitted from the wire when absent: the
/// card answers per-field, and an unlocked port genuinely does not know its
/// raster or colorimetry. **`None` means "the card did not say", never "no".**
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SdiDeviceStatus {
    /// Enumeration index, as used by `SdiInputConfig.device` when numeric.
    pub index: u32,
    /// SDK display name, e.g. `"DeckLink Quad (1)"`.
    pub name: String,
    /// Connector number parsed from the name, e.g. `1` for `"... (1)"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdi_channel: Option<u8>,

    /// Input is locked to an SDI signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_locked: Option<bool>,
    /// Locked to house reference (genlock).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_locked: Option<bool>,
    /// Ancillary data stream is locked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancillary_locked: Option<bool>,
    /// Device is held open by some process — this edge included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<bool>,

    /// Detected raster as a DeckLink mode FourCC, e.g. `"Hi50"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_mode: Option<String>,
    /// Detected colorimetry, e.g. `"r709"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_colorspace: Option<String>,
    /// Detected field dominance, e.g. `"uppr"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_field_dominance: Option<String>,
    /// SDI link configuration, e.g. `"lcsl"` (single link).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdi_link_config: Option<String>,
    /// Raster of the house reference signal, when one is patched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_mode: Option<String>,
    /// Detected dynamic range (`BMDDynamicRange`); `0` = SDR, non-zero = an HDR
    /// transfer (HLG / PQ). Lets the manager flag an HDR feed on an SDR chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_dynamic_range: Option<i32>,

    /// PCIe generation the card negotiated. A card in an undersized slot is a
    /// common, and otherwise invisible, cause of dropped capture frames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pcie_link_speed: Option<i32>,
    /// PCIe lanes the card negotiated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pcie_link_width: Option<i32>,
}

/// Latest per-port snapshot. Seeded by [`init`] at startup and swapped by
/// [`spawn_poller`]. The `OnceLock` only guards lazy creation of the `ArcSwap`
/// container; the list inside is mutable for the process lifetime.
static CACHED: OnceLock<ArcSwap<Vec<SdiDeviceStatus>>> = OnceLock::new();

fn store() -> &'static ArcSwap<Vec<SdiDeviceStatus>> {
    CACHED.get_or_init(|| ArcSwap::from_pointee(Vec::new()))
}

/// Latest snapshot — a lock-free atomic load, cheap on every health tick.
/// Empty before [`init`] runs and on hosts with no DeckLink card.
pub fn cached() -> Arc<Vec<SdiDeviceStatus>> {
    store().load_full()
}

/// Probe every device once, synchronously. Called at startup so the first
/// health tick has data before the poller's first cycle.
pub fn init() -> Arc<Vec<SdiDeviceStatus>> {
    store().store(Arc::new(probe_all()));
    store().load_full()
}

/// Blocking sweep of every enumerated device. A device that refuses status
/// (older model, or a transient SDK error) is still listed — with every status
/// field `None` — because omitting it would misreport the card's port count.
fn probe_all() -> Vec<SdiDeviceStatus> {
    decklink_rs::enumerate_devices()
        .into_iter()
        .map(|d| {
            let s = decklink_rs::device_status(d.index).ok();
            SdiDeviceStatus {
                index: d.index,
                name: d.name,
                sdi_channel: d.sdi_channel,
                signal_locked: s.as_ref().and_then(|s| s.signal_locked),
                reference_locked: s.as_ref().and_then(|s| s.reference_locked),
                ancillary_locked: s.as_ref().and_then(|s| s.ancillary_locked),
                busy: s.as_ref().and_then(|s| s.busy),
                detected_mode: s.as_ref().and_then(|s| s.detected_mode.clone()),
                detected_colorspace: s.as_ref().and_then(|s| s.detected_colorspace.clone()),
                detected_field_dominance: s
                    .as_ref()
                    .and_then(|s| s.detected_field_dominance.clone()),
                sdi_link_config: s.as_ref().and_then(|s| s.sdi_link_config.clone()),
                reference_mode: s.as_ref().and_then(|s| s.reference_mode.clone()),
                detected_dynamic_range: s.as_ref().and_then(|s| s.detected_dynamic_range),
                pcie_link_speed: s.as_ref().and_then(|s| s.pcie_link_speed),
                pcie_link_width: s.as_ref().and_then(|s| s.pcie_link_width),
            }
        })
        .collect()
}

/// Background re-probe so the manager tracks cable patches and signal changes
/// without an edge restart.
///
/// The sweep is blocking SDK work (~25 ms for 8 ports), so it runs on a
/// blocking thread and never stalls the reactor. Status reads do not open or
/// reserve a device, so this is safe to run against ports carrying live flows.
pub fn spawn_poller(cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            let devs = match tokio::task::spawn_blocking(probe_all).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(target: "sdi.decklink", "SDI status re-probe task failed: {e}");
                    continue;
                }
            };
            let prev = store().load_full();
            if *prev != devs {
                let locked = devs
                    .iter()
                    .filter(|d| d.signal_locked == Some(true))
                    .count();
                tracing::info!(
                    target: "sdi.decklink",
                    "SDI port status changed — {} port(s), {locked} with signal",
                    devs.len(),
                );
                store().store(Arc::new(devs));
            }
        }
    })
}
