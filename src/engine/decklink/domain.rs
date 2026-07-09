//! DeckLink device probe + process-wide manager (mirrors `engine::mxl::domain`).
//!
//! The boot probe (run in `main.rs`) enumerates DeckLink devices via FFmpeg's
//! avdevice layer. On success the `sdi-decklink` capability bit joins
//! `HealthPayload.capabilities` and the flow spawn arms can resolve the
//! manager via [`global`]. Unlike libmxl, there is no persistent native handle
//! to own — the manager just holds the enumerated device list for validation
//! and capability reporting.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::info;

/// Set to `true` by [`DecklinkDeviceManager::probe`] on success. Read when
/// deciding whether to advertise the `sdi-decklink` capability bit.
pub static DECKLINK_PROBE_OK: AtomicBool = AtomicBool::new(false);

/// Has the boot probe run successfully?
///
/// Not yet consumed: this is the capability gate for
/// `manager::client::edge_capabilities` to advertise `sdi-decklink`, mirroring
/// `mxl::domain::probe_succeeded`. Kept so the wiring lands in one place.
#[allow(dead_code)]
pub fn probe_succeeded() -> bool {
    DECKLINK_PROBE_OK.load(Ordering::Relaxed)
}

/// Process-wide handle installed at boot; the flow spawn arms look it up when
/// wiring an SDI input. `None` means no DeckLink device (or an FFmpeg without
/// `--enable-decklink`), so the spawn arm refuses the flow.
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
    /// Enumerate DeckLink devices via FFmpeg's avdevice layer. Returns `None`
    /// when none are visible so callers can gate the SDI capability and the
    /// spawn arms refuse gracefully (parallels `MxlDomainManager::probe`
    /// returning `None` when libmxl is missing).
    pub fn probe() -> Option<Arc<Self>> {
        // NOTE(decklink-enumerate-wedge): `enumerate_devices()` currently
        // wedges the DeckLink device for the lifetime of the process —
        // FFmpeg's decklink discovery API is not fully released by
        // `avdevice_free_list_devices`, so a later `DecklinkCapture::open` on
        // the same device returns EIO. Since the enumeration is only used for
        // capability display (never for capture), skip it at boot and
        // advertise SDI availability whenever the feature is compiled in.
        // Enumeration can be run on demand once the release path is fixed.
        info!(
            target: "sdi.decklink",
            "SDI (DeckLink) capability enabled (device enumeration deferred — see decklink-enumerate-wedge)"
        );
        DECKLINK_PROBE_OK.store(true, Ordering::Relaxed);
        Some(Arc::new(Self {
            devices: Vec::new(),
        }))
    }

    /// The enumerated devices, for the hardware-capabilities payload.
    ///
    /// **Currently always empty** — [`Self::probe`] skips enumeration (see
    /// `decklink-enumerate-wedge`). Treat an empty list as "unknown", never as
    /// "no devices present": the card is opened lazily by the capture task.
    ///
    /// Deliberately no `has_device()` helper: with enumeration deferred it
    /// would report `false` for every real device and silently refuse valid
    /// configs.
    pub fn devices(&self) -> &[decklink_rs::DecklinkDeviceInfo] {
        &self.devices
    }
}
