// Local-display output backend. Linux-only — gated on the `display`
// Cargo feature; main.rs only declares this module under
// `cfg(all(feature = "display", target_os = "linux"))`.
//
// Owns:
// - `DisplayDevice` / `DisplayMode` / `DisplayKind` types (mirror of the
//   manager-side enumeration shape; serialised onto
//   `HealthPayload.display_devices`).
// - `enumerate_displays()` — a single KMS connector probe (walk every
//   `/dev/dri/cardN`, force-probe each connector). Returns an empty Vec on
//   a headless build server so the `"display"` capability isn't advertised
//   when there's nothing to render to.
// - `cached_displays()` — lock-free snapshot of the latest probe
//   (`ArcSwap<Vec<DisplayDevice>>`). Seeded synchronously at startup by
//   `init_displays()` and refreshed every `POLL_INTERVAL` by the background
//   `spawn_display_poller` task, so a monitor hot-plugged after boot shows
//   up in `HealthPayload.display_devices` (and the manager UI connector
//   picker) without an edge restart. Consumed by `manager::client`
//   (HealthPayload + the `"display"` capability gate).
// - `spawn_display_poller()` — the background re-enumeration task.
// - `kms` — KMS / DRM CRTC + dumb-buffer + page-flip rendering loop
//   used by `engine::output_display`.
// - `audio` — ALSA blocking PCM writer that *is* the master clock.
// - `clock` — lock-free `AudioClock` shared between display + audio
//   children.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub mod audio;
pub mod audio_bars;
pub mod audio_meter;
pub mod claim_registry;
pub mod clock;
pub mod hdr_tonemap;
pub mod kms;
#[cfg(feature = "rga-transfer")]
pub mod rga_transfer;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisplayKind {
    Hdmi,
    DisplayPort,
    Dvi,
    Vga,
    Composite,
    Other,
}

impl DisplayKind {
    /// Map a libdrm connector type to our coarse kind. Mirrors the
    /// `drmModeConnector::connector_type` integer set documented in
    /// `drm_mode.h`.
    pub fn from_drm_connector_type(t: u32) -> Self {
        // Constants from `libdrm` headers:
        // DRM_MODE_CONNECTOR_HDMIA = 11, HDMIB = 12,
        // DRM_MODE_CONNECTOR_DisplayPort = 10, eDP = 14,
        // DVI variants = 2 / 3 / 4, VGA = 1, Composite = 5.
        match t {
            11 | 12 => Self::Hdmi,
            10 | 14 => Self::DisplayPort,
            2 | 3 | 4 => Self::Dvi,
            1 => Self::Vga,
            5 => Self::Composite,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// `true` for the connector's preferred mode (KMS
    /// `DRM_MODE_TYPE_PREFERRED`).
    pub preferred: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayDevice {
    /// Canonical KMS connector name (`"HDMI-A-1"`, `"DP-2"`, …).
    pub name: String,
    pub kind: DisplayKind,
    /// `true` while a monitor is plugged into this connector.
    pub connected: bool,
    /// Modes advertised by the EDID (deduped and clamped to ≤ 4096×2160).
    pub modes: Vec<DisplayMode>,
    /// `true` when the EDID's CEA-861 audio data block reports a sink.
    pub has_audio: bool,
    /// Best-effort guess at the matching ALSA device (`"hw:N,M"`).
    /// Populated by walking `/sys/class/sound/cardN/eldN`. `None` on
    /// connectors without an HDMI audio sink or when the sysfs lookup
    /// fails.
    pub alsa_device: Option<String>,
}

/// How often [`spawn_display_poller`] re-enumerates KMS connectors —
/// matches the cellular / starlink telemetry cadence, a little faster than
/// the ~15 s health tick so a fresh snapshot is usually waiting when the
/// tick reads it. Each pass is one short open + force-probe of every
/// `/dev/dri/cardN` (~1–5 ms), wholly off the data path.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Latest connector snapshot. Seeded by [`init_displays`] at startup and
/// swapped by [`spawn_display_poller`] each poll cycle. The `OnceLock` only
/// guards lazy creation of the `ArcSwap` container itself — the connector
/// list inside is fully mutable across the process lifetime.
static CACHED_DISPLAYS: OnceLock<ArcSwap<Vec<DisplayDevice>>> = OnceLock::new();

fn store() -> &'static ArcSwap<Vec<DisplayDevice>> {
    CACHED_DISPLAYS.get_or_init(|| ArcSwap::from_pointee(Vec::new()))
}

/// Return the latest connector snapshot — a lock-free atomic load + `Arc`
/// clone, cheap enough to call on every health tick. Empty before
/// [`init_displays`] runs, on platforms where the feature is off, on Linux
/// without `/dev/dri/cardN`, or on boxes with no graphics adapter at all.
/// Callers treat emptiness as authoritative — the `"display"` capability is
/// gated on this being non-empty.
pub fn cached_displays() -> Arc<Vec<DisplayDevice>> {
    store().load_full()
}

/// Probe the local KMS subsystem synchronously and publish the result into
/// the snapshot. Called once at startup so the first health tick + the
/// capability advertisement have data before the background poller's first
/// cycle. Returns the freshly probed snapshot.
pub fn init_displays() -> Arc<Vec<DisplayDevice>> {
    let store = store();
    store.store(Arc::new(enumerate_displays()));
    store.load_full()
}

/// Walk every `/dev/dri/cardN` under the system, query each connector,
/// and return the merged device list. Returns an empty Vec on any
/// failure (no DRI nodes, permission denied, no driver) — operators on
/// headless build boxes simply won't see the `"display"` capability.
pub fn enumerate_displays() -> Vec<DisplayDevice> {
    kms::enumerate_displays_kms()
}

/// Spawn the background connector-refresh poller under the app's
/// cancellation tree. Re-enumerates KMS connectors every [`POLL_INTERVAL`]
/// and swaps the result into the snapshot when it changes, so the manager
/// UI's connector list and the `"display"` capability track monitor
/// hot-plug / unplug without an edge restart. Enumeration is blocking
/// syscalls (`open(/dev/dri/cardN)` + DRM ioctls), so each pass runs on a
/// blocking thread and never stalls the async runtime. No-op cost on
/// headless hosts — `enumerate_displays` returns empty and an unchanged
/// snapshot is not re-stored.
pub fn spawn_display_poller(cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            // Enumeration is blocking file / ioctl work — keep it off the
            // reactor. A panic inside the `drm` crate must not take the
            // poller (or the runtime) down with it.
            let devs = match tokio::task::spawn_blocking(enumerate_displays).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("display: connector re-enumeration task failed: {e}");
                    continue;
                }
            };
            let prev = store().load_full();
            if *prev != devs {
                tracing::info!(
                    "display: connector list changed — {} connector(s), {} with a monitor attached",
                    devs.len(),
                    devs.iter().filter(|d| d.connected).count(),
                );
                store().store(Arc::new(devs));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_connector_type_mapping_covers_common_cases() {
        assert_eq!(DisplayKind::from_drm_connector_type(11), DisplayKind::Hdmi);
        assert_eq!(DisplayKind::from_drm_connector_type(10), DisplayKind::DisplayPort);
        assert_eq!(DisplayKind::from_drm_connector_type(14), DisplayKind::DisplayPort);
        assert_eq!(DisplayKind::from_drm_connector_type(2), DisplayKind::Dvi);
        assert_eq!(DisplayKind::from_drm_connector_type(1), DisplayKind::Vga);
        assert_eq!(DisplayKind::from_drm_connector_type(99), DisplayKind::Other);
    }
}
