// Local-display output backend. Linux-only — gated on the `display`
// Cargo feature; main.rs only declares this module under
// `cfg(all(feature = "display", target_os = "linux"))`.
//
// Owns:
// - `DisplayDevice` / `DisplayMode` / `DisplayKind` types (mirror of the
//   manager-side enumeration shape; serialised onto
//   `HealthPayload.display_devices`).
// - `enumerate_displays()` — one-shot KMS connector probe at edge
//   startup. Returns an empty Vec on a headless build server so the
//   `"display"` capability isn't advertised when there's nothing to
//   render to.
// - `cached_displays()` — `OnceLock<Vec<DisplayDevice>>` populated once
//   at startup; consumed by `manager::client` (HealthPayload) and
//   the REST endpoint.
// - `kms` — KMS / DRM CRTC + dumb-buffer + page-flip rendering loop
//   used by `engine::output_display`.
// - `audio` — ALSA blocking PCM writer that *is* the master clock.
// - `clock` — lock-free `AudioClock` shared between display + audio
//   children.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub mod audio;
pub mod audio_bars;
pub mod audio_meter;
pub mod clock;
pub mod kms;

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

static CACHED_DISPLAYS: OnceLock<Vec<DisplayDevice>> = OnceLock::new();

/// Return the startup-time cached connector list. Empty on platforms
/// where the feature is off, on Linux without `/dev/dri/cardN`, or on
/// boxes with no graphics adapter at all. Callers should treat the
/// emptiness as authoritative — the `"display"` capability advertisement
/// is gated on this being non-empty.
pub fn cached_displays() -> &'static [DisplayDevice] {
    CACHED_DISPLAYS
        .get()
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

/// Probe the local KMS subsystem once and cache the result. Idempotent —
/// safe to call multiple times. Subsequent calls return the same slice
/// reference.
pub fn init_displays() -> &'static [DisplayDevice] {
    CACHED_DISPLAYS.get_or_init(enumerate_displays).as_slice()
}

/// Walk every `/dev/dri/cardN` under the system, query each connector,
/// and return the merged device list. Returns an empty Vec on any
/// failure (no DRI nodes, permission denied, no driver) — operators on
/// headless build boxes simply won't see the `"display"` capability.
pub fn enumerate_displays() -> Vec<DisplayDevice> {
    kms::enumerate_displays_kms()
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
