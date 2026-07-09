//! DeckLink device probe + process-wide manager (mirrors `engine::mxl::domain`).
//!
//! The boot probe (run in `main.rs`) enumerates DeckLink devices via the
//! Blackmagic DeckLink SDK. On success the `sdi-decklink` capability bit joins
//! `HealthPayload.capabilities` and the flow spawn arms can resolve the
//! manager via [`global`]. Unlike libmxl, there is no persistent native handle
//! to own — the manager just holds the enumerated device list for validation
//! and capability reporting.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{info, warn};

/// Set to `true` by [`DecklinkDeviceManager::probe`] on success. Read when
/// deciding whether to advertise the `sdi-decklink` capability bit.
pub static DECKLINK_PROBE_OK: AtomicBool = AtomicBool::new(false);

/// Has the boot probe run successfully?
///
/// The capability gate for `manager::client::edge_capabilities` to advertise
/// `sdi-decklink`, mirroring `mxl::domain::probe_succeeded`.
pub fn probe_succeeded() -> bool {
    DECKLINK_PROBE_OK.load(Ordering::Relaxed)
}

/// Process-wide handle installed at boot; the flow spawn arms look it up when
/// wiring an SDI input. `None` means Desktop Video is not installed (no
/// `libDeckLinkAPI.so`), so the spawn arm refuses the flow.
static GLOBAL_DECKLINK_MANAGER: OnceLock<Arc<DecklinkDeviceManager>> = OnceLock::new();

/// Install the manager returned by [`DecklinkDeviceManager::probe`]. Called
/// once from `main.rs`. Subsequent calls are no-ops.
pub fn install_global(mgr: Arc<DecklinkDeviceManager>) {
    let _ = GLOBAL_DECKLINK_MANAGER.set(mgr);
}

/// Look up the global manager (set by [`install_global`]). `None` when the
/// probe failed or `install_global` hasn't run.
pub fn global() -> Option<Arc<DecklinkDeviceManager>> {
    GLOBAL_DECKLINK_MANAGER.get().cloned()
}

/// Owns the enumerated DeckLink device list for the lifetime of the process.
pub struct DecklinkDeviceManager {
    devices: Vec<decklink_rs::DecklinkDeviceInfo>,
}

impl DecklinkDeviceManager {
    /// Enumerate DeckLink devices via the SDK.
    ///
    /// `enumerate_devices` is infallible: it yields an empty list when Desktop
    /// Video is not installed or no card is fitted. Both are reported as a
    /// successful probe with zero devices rather than `None`, because the
    /// capture task — not the boot probe — is the authority on whether a device
    /// can be opened. A `None` here would refuse a flow on a host whose card
    /// appears after boot (hot-plugged Thunderbolt, delayed driver load).
    pub fn probe() -> Option<Arc<Self>> {
        let devices = decklink_rs::enumerate_devices();

        if devices.is_empty() {
            warn!(
                target: "sdi.decklink",
                "SDI (DeckLink) capability enabled but no devices enumerated — \
                 is Blackmagic Desktop Video installed and a card fitted?"
            );
        } else {
            for d in &devices {
                info!(target: "sdi.decklink", "  [{}] {}", d.index, d.name);
            }
            info!(
                target: "sdi.decklink",
                "SDI (DeckLink) capability enabled — {} device(s)",
                devices.len()
            );
        }

        DECKLINK_PROBE_OK.store(true, Ordering::Relaxed);
        Some(Arc::new(Self { devices }))
    }

    /// The enumerated devices, for the hardware-capabilities payload.
    ///
    /// Deliberately no `has_device()` helper. Enumeration names are the SDK's
    /// display names, while a config's `device` field may legitimately be an
    /// index or a differently-spelled alias; gating a config on a name match
    /// here would refuse valid setups. The capture task is the authority on
    /// whether a device can be opened.
    pub fn devices(&self) -> &[decklink_rs::DecklinkDeviceInfo] {
        &self.devices
    }
}
