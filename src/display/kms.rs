// KMS / DRM video backend for the local-display output.
//
// Owns:
// - `enumerate_displays_kms()` — startup probe that walks every
//   `/dev/dri/cardN` and returns a `DisplayDevice` per connector. Empty
//   on access-denied / no-driver / no-DRI hosts.
// - `KmsDisplay` — owns the master lease on a single connector + CRTC,
//   a pair of dumb buffers for double-buffered page flips, and the
//   output's chosen mode. Constructed by `output_display` once the
//   feature is enabled and the operator has picked a connector.
//
// All KMS work is sw-only today (dumb buffer + libswscale-driven
// YUV→BGRA scale into the mapped framebuffer). The hardware-decode
// path (DMA-BUF-zerocopy via VAAPI/NVDEC) lands as an additive backend
// behind the `video-decoder-vaapi` / `video-decoder-nvdec` Cargo
// features documented in `Cargo.toml`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use drm::buffer::{Buffer, DrmFourcc, DrmModifier, PlanarBuffer};
use drm::control::{
    atomic::AtomicModeReq, connector::State as ConnectorState, dumbbuffer::DumbMapping,
    framebuffer, plane, property, AtomicCommitFlags, Device as ControlDevice, Event, FbCmd2Flags,
    PageFlipFlags, PlaneType,
};
use drm::{ClientCapability, Device};

use super::{DisplayDevice, DisplayKind, DisplayMode};

/// Minimal `drm::Device + drm::control::Device` newtype around an
/// owned `std::fs::File` for `/dev/dri/cardN`. The `drm` crate's
/// blanket impls do the rest.
pub struct CardFile(std::fs::File);

impl AsRef<std::os::fd::OwnedFd> for CardFile {
    fn as_ref(&self) -> &std::os::fd::OwnedFd {
        // SAFETY: borrowing the OwnedFd-shaped view of File. drm-rs
        // expects `AsFd` on stable; we model that via OwnedFd here.
        unsafe { std::mem::transmute(&self.0) }
    }
}

impl std::os::fd::AsFd for CardFile {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.0)
    }
}

impl Device for CardFile {}
impl ControlDevice for CardFile {}

fn open_card(path: &PathBuf) -> Result<CardFile> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok(CardFile(file))
}

/// Return all `/dev/dri/cardN` paths sorted by `N`. Modern kernels can
/// start the card numbering at non-zero (mixed iGPU+dGPU systems, hot-add
/// ordering, container passthrough), so we scan the directory rather than
/// walking a contiguous index from 0. Returns an empty vec on access-denied
/// or missing-DRI hosts.
fn enumerate_card_paths() -> Vec<PathBuf> {
    let dir = match std::fs::read_dir("/dev/dri") {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut cards: Vec<(u32, PathBuf)> = Vec::new();
    for entry in dir.flatten() {
        let fname = entry.file_name();
        let name = fname.to_string_lossy();
        if let Some(rest) = name.strip_prefix("card") {
            if let Ok(n) = rest.parse::<u32>() {
                cards.push((n, entry.path()));
            }
        }
    }
    cards.sort_by_key(|(n, _)| *n);
    cards.into_iter().map(|(_, p)| p).collect()
}

/// Read `/dev/dri/` and merge the connectors of every `cardN` node
/// into one `DisplayDevice` list.
pub fn enumerate_displays_kms() -> Vec<DisplayDevice> {
    let mut out = Vec::new();
    for path in enumerate_card_paths() {
        match enumerate_card(&path) {
            Ok(devs) => out.extend(devs),
            Err(e) => {
                tracing::warn!("display: skipping {} — {}", path.display(), e);
            }
        }
    }
    out
}

fn enumerate_card(path: &PathBuf) -> Result<Vec<DisplayDevice>> {
    let card = open_card(path)?;
    let resources = card.resource_handles().context("resource_handles")?;
    let mut out = Vec::new();
    for &handle in resources.connectors() {
        let info = card.get_connector(handle, true).context("get_connector")?;
        let kind = DisplayKind::from_drm_connector_type(info.interface() as u32);
        // Canonical name: `<protocol>-<id>` matches what `drmModeGetConnectorName`
        // would print.
        let name = format!(
            "{}-{}",
            connector_protocol_label(info.interface() as u32),
            info.interface_id()
        );
        let connected = matches!(info.state(), ConnectorState::Connected);
        let mut modes: Vec<DisplayMode> = info
            .modes()
            .iter()
            .filter(|m| m.size().0 as u32 <= 4096 && m.size().1 as u32 <= 2160)
            .map(|m| {
                let (w, h) = m.size();
                DisplayMode {
                    width: w as u32,
                    height: h as u32,
                    refresh_hz: m.vrefresh() as u32,
                    preferred: (m.mode_type() & drm::control::ModeTypeFlags::PREFERRED)
                        == drm::control::ModeTypeFlags::PREFERRED,
                }
            })
            .collect();
        // Dedup by (w, h, hz) preserving the first occurrence so the
        // preferred flag wins.
        modes.sort_by(|a, b| {
            (b.preferred, a.width, a.height, a.refresh_hz)
                .cmp(&(a.preferred, b.width, b.height, b.refresh_hz))
        });
        modes.dedup_by(|a, b| {
            a.width == b.width && a.height == b.height && a.refresh_hz == b.refresh_hz
        });

        // EDID parsing for `has_audio` is a v2 nicety — for v1 we
        // assume HDMI / DisplayPort connectors *can* carry audio and
        // let the operator pick `audio_device = null` to mute.
        let has_audio = matches!(kind, DisplayKind::Hdmi | DisplayKind::DisplayPort);
        let alsa_device = best_alsa_device_for_connector(&name);

        out.push(DisplayDevice {
            name,
            kind,
            connected,
            modes,
            has_audio,
            alsa_device,
        });
    }
    Ok(out)
}

fn connector_protocol_label(t: u32) -> &'static str {
    match t {
        1 => "VGA",
        2 => "DVI-I",
        3 => "DVI-D",
        4 => "DVI-A",
        5 => "Composite",
        6 => "S-Video",
        7 => "LVDS",
        8 => "Component",
        9 => "9-PinDIN",
        10 => "DP",
        11 => "HDMI-A",
        12 => "HDMI-B",
        13 => "TV",
        14 => "eDP",
        15 => "Virtual",
        16 => "DSI",
        17 => "DPI",
        18 => "Writeback",
        19 => "SPI",
        20 => "USB",
        _ => "Unknown",
    }
}

/// Best-effort sysfs walk to find the ALSA card+device pair that backs
/// a given KMS connector. Implementation: enumerate
/// `/sys/class/drm/card*-<connector>` to confirm presence, then scan
/// `/sys/class/sound/cardN/eldM` for the matching ELD payload (the
/// PCM index `M` is what `hw:N,M` references). Returns `None` on any
/// failure — the operator can still pick `default` or supply a custom
/// device name.
fn best_alsa_device_for_connector(connector_name: &str) -> Option<String> {
    let drm_dir = std::fs::read_dir("/sys/class/drm").ok()?;
    let mut card_idx: Option<u32> = None;
    for entry in drm_dir.flatten() {
        let fname = entry.file_name();
        let name = fname.to_string_lossy();
        if !name.contains(connector_name) {
            continue;
        }
        // Walk back: `cardN-HDMI-A-1` → N
        if let Some(prefix) = name.strip_prefix("card") {
            if let Some((n_str, _rest)) = prefix.split_once('-') {
                if let Ok(n) = n_str.parse::<u32>() {
                    card_idx = Some(n);
                    break;
                }
            }
        }
    }
    let _ = card_idx?;
    // Fallback: most modern hosts route HDMI audio through the GPU's
    // own ALSA card with PCM index `3` for the first HDMI port. We
    // could refine this by scanning `eld*` files, but for v1 the
    // operator can override via `audio_device` on the output config.
    Some(format!(
        "hw:{},{}",
        card_idx.unwrap_or(0),
        infer_alsa_pcm_for(connector_name)
    ))
}

fn infer_alsa_pcm_for(connector_name: &str) -> u32 {
    // Per Intel + AMD KMS conventions, the first HDMI connector lands
    // on PCM 3, second on PCM 7, third on PCM 8 (rough heuristic;
    // operators with non-standard layouts override via the form).
    if let Some(suffix) = connector_name.rsplit('-').next() {
        if let Ok(n) = suffix.parse::<u32>() {
            return match n {
                1 => 3,
                2 => 7,
                _ => 8,
            };
        }
    }
    3
}

// ── Runtime renderer (page-flip + dumb-buffer blit) ────────────────────

/// Cached lookups for the atomic-commit page-flip path. Built lazily on
/// the first PRIME present — both DRM client capabilities (UniversalPlanes
/// + Atomic) must succeed and a primary plane attached to our CRTC must
/// be discoverable via `plane_handles()`. Property IDs are stable for the
/// lifetime of the DRM fd, so we resolve them once and reuse on every
/// subsequent flip — the atomic commit path is per-frame allocation-free
/// modulo the one Vec inside `AtomicModeReq` (which `drm-rs 0.14` allocates
/// internally on each `add_property` call but reuses no memory across
/// flips; building four short Vecs of < 16 entries each is below the
/// noise floor of any media data path).
struct AtomicSetup {
    plane: plane::Handle,
    plane_props: PlanePropIds,
    crtc_props: CrtcPropIds,
    connector_props: ConnectorPropIds,
    /// Property-blob handle for the current `Mode`, kept alive across
    /// flips so the kernel can reference it in MODE_ID. Allocated
    /// lazily on the first atomic commit and freed when the mode
    /// changes (legacy `set_crtc` modeset paths invalidate it via
    /// `KmsDisplay::invalidate_atomic_modeset`).
    mode_blob_id: Option<u64>,
}

struct PlanePropIds {
    fb_id: property::Handle,
    crtc_id: property::Handle,
    src_x: property::Handle,
    src_y: property::Handle,
    src_w: property::Handle,
    src_h: property::Handle,
    crtc_x: property::Handle,
    crtc_y: property::Handle,
    crtc_w: property::Handle,
    crtc_h: property::Handle,
}

struct CrtcPropIds {
    mode_id: property::Handle,
    active: property::Handle,
}

struct ConnectorPropIds {
    crtc_id: property::Handle,
    /// `HDR_OUTPUT_METADATA` blob property. `None` when the host
    /// kernel / driver doesn't expose it on this connector — older
    /// kernels (pre-5.14 amdgpu) and some virtual / non-HDR-aware
    /// drivers fall here. When set, [`KmsDisplay::set_hdr_output_metadata`]
    /// allocates an `hdr_output_metadata` blob and the next atomic
    /// commit programs it onto the connector so the HDMI / DP sink
    /// transmits the right Dynamic Range and Mastering InfoFrame and
    /// the panel switches into HDR mode.
    hdr_output_metadata: Option<property::Handle>,
}

/// Cached overlay-plane state for the audio-bars confidence strip.
/// Allocated by [`KmsDisplay::enable_bars_overlay`] when the operator
/// has set `show_audio_bars: true` on a VAAPI / atomic-commit display
/// output. Composes onto a second KMS plane via the same atomic commit
/// that flips the prime FB onto the primary plane — eliminates the
/// CPU-blit demotion the v1 prime path used to take whenever
/// `show_audio_bars` was on.
struct BarsOverlay {
    plane: plane::Handle,
    plane_props: PlanePropIds,
    /// Ping-pong pair of ARGB8888 dumb buffers. The rasteriser always
    /// writes to the **back** buffer (`dumbs[1 ^ front_idx]`); the next
    /// atomic commit references it, and once the kernel posts
    /// `DRM_EVENT_FLIP_COMPLETE` the caller advances `front_idx` so the
    /// just-flipped buffer becomes "front" (currently scanning out) and
    /// the previous front is safe to overwrite. Two reasons we ping-pong
    /// instead of reusing a single buffer:
    ///   1. **Write/scanout race.** A single buffer is being read by the
    ///      scanout engine *while* the CPU rasterises the next frame.
    ///      The kernel can read torn / partially-written content; on
    ///      Intel i915 (Arrow Lake) this surfaces as the strip
    ///      vanishing entirely most frames, with the bars + header
    ///      "popping up" only when the CPU happens to finish writing
    ///      between scanouts.
    ///   2. **FB-id diff fast paths.** Some KMS plane-state diff paths
    ///      treat back-to-back commits with an identical FB id as
    ///      no-ops on the FB content. A fresh FB id every commit
    ///      forces the kernel to re-fetch.
    /// The pair mirrors the primary plane's existing
    /// `KmsDisplay::bufs: [DumbBuffer; 2]` ping-pong (`front_idx`
    /// advance after `wait_page_flip`).
    dumbs: [DumbBuffer; 2],
    /// Which `dumbs[]` entry is currently being scanned out. Toggles
    /// (`^= 1`) after every successful atomic commit + flip-complete.
    /// `bars_overlay_buffer()` returns the OTHER index for the next
    /// rasterise. Reset to `0` whenever both buffers are reallocated
    /// (panel mode change in `resync_bars_overlay_to_panel`).
    front_idx: usize,
    /// Strip width / height in pixels. Width = panel width; height
    /// matches `audio_bars::compute_strip_height(panel_h)`.
    width: u32,
    height: u32,
    /// Has at least one atomic commit programmed this plane onto our
    /// CRTC? Kept so a pending modeset that fails before the bars
    /// plane is ever programmed doesn't leave the kernel scanning out
    /// from a stale FB after a teardown.
    committed: bool,
    /// Optional `zpos` (Z-position / stacking order) property handle.
    /// `None` when the driver doesn't expose `zpos` on this plane —
    /// in that case the kernel uses registration-order zpos and the
    /// bars composition relies on driver default. Set this property
    /// to `primary_zpos + 1` so the bars plane is **above** the
    /// primary plane (the VAAPI prime FB carrying the video) — without
    /// it, several drivers (notably some AMD `amdgpu` configurations
    /// and certain Intel `i915` revisions) place an Overlay plane
    /// **under** the Primary plane in registration order, hiding the
    /// bars + header strip behind the video frame even though the
    /// rasterise wrote opaque pixels to the buffer.
    zpos: Option<property::Handle>,
    /// Z-position value to drive into `zpos`. Computed at discovery
    /// time as `primary_zpos.saturating_add(1)` — small, positive,
    /// always above the primary plane.
    zpos_value: u64,
    /// Optional `pixel blend mode` property handle. Defaults vary by
    /// driver — Intel `i915` defaults to `Pre-multiplied` (correct for
    /// our straight-alpha ARGB8888 because we write opaque alpha=0xFF
    /// where pixels are visible), AMD `amdgpu` defaults to `None`
    /// (which makes alpha-blending do nothing → invisible plane).
    /// When the property exists we explicitly set `Coverage` (straight
    /// alpha = honour per-pixel alpha, blend with primary below).
    pixel_blend_mode: Option<property::Handle>,
    /// Enum value for `Coverage` on the discovered property. The drm
    /// API uses enum values whose numeric IDs are driver-assigned,
    /// not constants; we capture the right one at discovery time.
    pixel_blend_coverage_value: u64,
}

/// Holder for the active KMS master + double-buffered framebuffers.
/// One per display output. Created by `output_display` after mode-set;
/// owns the lifetime of the dumb buffers and the CRTC lease.
pub struct KmsDisplay {
    card: CardFile,
    crtc: drm::control::crtc::Handle,
    connector: drm::control::connector::Handle,
    mode: drm::control::Mode,
    width: u32,
    height: u32,
    // Two framebuffers we ping-pong between for tear-free page flips.
    bufs: [DumbBuffer; 2],
    front_idx: usize,
    /// PRIME scanout state — set when `present_prime` has flipped a
    /// VAAPI-decoded surface onto the CRTC. `None` while the CPU-blit
    /// path is active. Holding the previous-frame keepalive here means
    /// the underlying VAAPI surface stays valid until the *next* flip
    /// completes, eliminating the green-flash / stuck-frame artefacts
    /// the VA pool causes when surfaces recycle mid-scanout.
    prime_state: Option<PrimeState>,
    /// Atomic-commit page-flip state. `None` until the first PRIME
    /// present, which discovers the primary plane + caches property
    /// IDs. `use_atomic` flips to `false` (and stays there) on
    /// EOPNOTSUPP or a first-commit EINVAL — see `present_prime`.
    use_atomic: bool,
    atomic: Option<AtomicSetup>,
    /// Has the connector seen at least one successful atomic commit
    /// with `ALLOW_MODESET`? Subsequent flips skip the
    /// CRTC.ACTIVE / CRTC.MODE_ID / CONNECTOR.CRTC_ID writes (the
    /// kernel keeps that state). Reset by legacy modeset paths
    /// (`match_source_resolution` etc.) so the next atomic re-arms.
    atomic_modeset_done: bool,
    /// One-shot reason string the caller drains via
    /// `take_atomic_fallback_reason()` to emit a single
    /// `display_atomic_unavailable` Warning event. Set when
    /// `present_prime` falls back from atomic to `set_crtc`.
    atomic_fallback_reason: Option<String>,
    /// Audio-bars confidence-strip state. `Some` after a successful
    /// `enable_bars_overlay()` call; the next `present_prime` adds the
    /// overlay-plane property writes to the same atomic commit. `None`
    /// when bars are off, when the host has no overlay plane drivable
    /// from our CRTC, or when the legacy `set_crtc` fallback is in
    /// effect (multi-plane composition needs atomic commit).
    bars_overlay: Option<BarsOverlay>,
    /// Currently-programmed HDR output metadata. The next atomic
    /// commit writes this onto the connector's `HDR_OUTPUT_METADATA`
    /// property. `None` when SDR (the connector is unset by writing
    /// blob id 0). `Some(HdrState { blob_id, eotf, .. })` when an
    /// HDR signalling blob has been allocated and is in force.
    hdr_state: Option<HdrState>,
    /// Has `hdr_state` changed since the last atomic commit? When
    /// true the next commit re-programs `HDR_OUTPUT_METADATA` (with
    /// `ALLOW_MODESET` because EOTF transitions trigger a brief
    /// HDMI / DP re-train on the panel side). Cleared by
    /// `atomic_present` after a successful commit.
    hdr_dirty: bool,
}

/// Snapshot of the HDR signalling state currently programmed on the
/// connector. The `blob_id` is the kernel handle returned by
/// `create_property_blob`; we keep ownership of it across flips and
/// free it (`destroy_property_blob`) only when transitioning to a
/// different EOTF or back to SDR.
struct HdrState {
    blob_id: u64,
    eotf: HdrEotf,
}

struct DumbBuffer {
    // Field declaration order = drop order. `mapping` drops first
    // (munmaps the persistent CPU mapping); `handle` and `fb` are
    // released by the kernel when the DRM fd closes (CardFile drops at
    // KmsDisplay::Drop) or by explicit `destroy_*` calls in the
    // mode-change paths below.
    //
    // The drm-rs `DumbMapping<'a>` lifetime parameter is purely phantom
    // (`PhantomData<&'a ()>` — verified in drm-0.14/src/control/dumbbuffer.rs):
    // the inner `&'a mut [u8]` borrows the mmap'd kernel slice, not the
    // `DumbBuffer` struct, so extending the lifetime to `'static` here
    // is sound as long as `mapping` is dropped before any
    // `destroy_dumb_buffer(handle)` call (enforced by field order
    // and the explicit destructure pattern in the cleanup loops).
    mapping: DumbMapping<'static>,
    handle: drm::control::dumbbuffer::DumbBuffer,
    fb: framebuffer::Handle,
}

impl KmsDisplay {
    /// Open the KMS card backing `connector_name`, drm-master the device,
    /// pick the requested mode (or the connector's preferred mode when
    /// `(width, height, refresh_hz)` is `None`), allocate two XRGB8888
    /// dumb buffers, and program the initial mode-set. Returns an error
    /// (lifted by the caller onto `command_ack.error_code`) when:
    /// - the connector is not enumerated (`display_device_invalid`),
    /// - the requested mode is not supported (`display_resolution_unsupported`),
    /// - the modeset fails (`display_mode_set_failed`).
    pub fn open(
        connector_name: &str,
        width: Option<u32>,
        height: Option<u32>,
        refresh_hz: Option<u32>,
    ) -> Result<Self> {
        // Locate the card the connector lives on.
        let (card_path, connector) = locate_connector(connector_name)?;
        let card = open_card(&card_path)?;
        // drm-master is required for atomic modeset; on most distributions
        // a regular user gets it via `seat0` automatically.
        let _ = card.acquire_master_lock();
        // Universal planes + atomic must both be enabled before
        // `plane_handles()` returns Primary planes and before the
        // atomic_commit ioctl is willing to accept our requests. Either
        // failing is non-fatal — we fall back to the legacy `set_crtc`
        // path on every flip and report `panel_hdr_capable = false`.
        let atomic_caps_ok = card
            .set_client_capability(ClientCapability::UniversalPlanes, true)
            .is_ok()
            && card
                .set_client_capability(ClientCapability::Atomic, true)
                .is_ok();

        let info = card
            .get_connector(connector, true)
            .context("get_connector after locate")?;
        if !matches!(info.state(), ConnectorState::Connected) {
            anyhow::bail!(
                "display_device_invalid: connector '{}' is not connected",
                connector_name
            );
        }
        let mode = pick_mode(&info, width, height, refresh_hz)?;

        let resources = card.resource_handles().context("resources")?;
        let crtc = resources
            .crtcs()
            .first()
            .copied()
            .context("no CRTCs available on this card")?;

        let (w, h) = mode.size();
        let mut a = alloc_dumb_buffer(&card, w as u32, h as u32)?;
        let mut b = alloc_dumb_buffer(&card, w as u32, h as u32)?;

        // Program the initial mode-set on framebuffer A.
        card.set_crtc(crtc, Some(a.fb), (0, 0), &[connector], Some(mode))
            .context("display_mode_set_failed: drmModeSetCrtc rejected the mode")?;

        // Drop the unused borrow guard so the helper compiles; the
        // dumb-buffer destructors clean up on `Drop`.
        let _ = (&mut a, &mut b);

        // Eager atomic discovery: walk plane / connector properties
        // now (rather than lazily on first present_prime) so the
        // upstream decode task can read `panel_hdr_capable()`
        // synchronously when deciding whether HDR sources need a
        // sysmem download for CPU tonemap. Failure isn't fatal —
        // the lazy retry inside `present_prime` may still succeed
        // once a real prime FB has been built; in the meantime
        // `use_atomic` is flipped off.
        let mut atomic_setup: Option<AtomicSetup> = None;
        let mut use_atomic = atomic_caps_ok;
        if use_atomic {
            match discover_atomic_setup(&card, crtc, connector) {
                Ok(s) => atomic_setup = Some(s),
                Err(e) => {
                    tracing::info!(
                        "atomic discovery deferred — falling back to legacy set_crtc on first flip: {e:#}"
                    );
                    use_atomic = false;
                }
            }
        }

        Ok(Self {
            card,
            crtc,
            connector,
            mode,
            width: w as u32,
            height: h as u32,
            bufs: [a, b],
            front_idx: 0,
            prime_state: None,
            use_atomic,
            atomic: atomic_setup,
            atomic_modeset_done: false,
            atomic_fallback_reason: None,
            bars_overlay: None,
            hdr_state: None,
            hdr_dirty: false,
        })
    }

    /// `true` when the connector exposes the `HDR_OUTPUT_METADATA`
    /// atomic property — meaning the kernel + driver can transmit
    /// the HDR Dynamic Range and Mastering InfoFrame to the panel,
    /// so HDR sources can scan out as HDR (instead of being
    /// downloaded to sysmem and CPU-tonemapped to SDR). On modern
    /// Linux this is true on every i915 / amdgpu / nouveau-driven
    /// HDMI 2.0a+ or DP 1.4+ connector. Returns `false` on hosts
    /// that haven't yet probed atomic — the discovery happens
    /// lazily on first `present_prime`, so a fresh `KmsDisplay`
    /// reports `false` until the first VAAPI flip.
    pub fn panel_hdr_capable(&self) -> bool {
        self.atomic
            .as_ref()
            .is_some_and(|a| a.connector_props.hdr_output_metadata.is_some())
    }

    /// Program the connector's `HDR_OUTPUT_METADATA` so the next
    /// atomic commit transmits a Dynamic Range and Mastering
    /// InfoFrame to the panel. `eotf` selects PQ (HDR10 family) or
    /// HLG. `mastering_max_nits` / `mastering_min_units` /
    /// `max_cll_nits` / `max_fall_nits` set the static-metadata
    /// luminance fields (units: nits, except `mastering_min` which
    /// is the kernel's 1/10 000 nit unit). When `mastering_max_nits`
    /// is `None` the call falls back to a sensible HDR10 default
    /// (1000 / 0.005 / 1000 / 400) — sufficient for live broadcast
    /// HDR sources where the operator hasn't supplied a Mastering
    /// Display SEI.
    ///
    /// Idempotent within an EOTF: a second call with the same EOTF
    /// reuses the existing blob (no reallocation, no `_dirty` set).
    /// Transitioning EOTFs (PQ → HLG, or any → SDR via
    /// [`Self::clear_hdr_output_metadata`]) frees the old blob and
    /// allocates a fresh one. The next atomic commit carries
    /// `ALLOW_MODESET` because the HDMI / DP sink re-trains on EOTF
    /// change.
    ///
    /// Returns `Err` when the connector doesn't expose
    /// `HDR_OUTPUT_METADATA` (call [`Self::panel_hdr_capable`]
    /// first) or when the kernel rejects the blob allocation. The
    /// caller falls back to the CPU-blit + sysmem-tonemap path.
    pub fn set_hdr_output_metadata(
        &mut self,
        eotf: HdrEotf,
        mastering_max_nits: Option<u16>,
        mastering_min_units: Option<u16>,
        max_cll_nits: Option<u16>,
        max_fall_nits: Option<u16>,
    ) -> Result<()> {
        if !self.panel_hdr_capable() {
            anyhow::bail!(
                "HDR_OUTPUT_METADATA not supported on this connector — panel/driver lacks HDR signalling"
            );
        }
        if let Some(state) = self.hdr_state.as_ref() {
            if state.eotf == eotf {
                // Already programmed with this EOTF — keep the
                // existing blob; static-metadata refinements are
                // not yet plumbed (would require freeing + re-
                // allocating). Acceptable for v1: a single source's
                // mastering metadata is constant.
                return Ok(());
            }
        }
        let bytes = build_hdr_output_metadata_blob(
            eotf,
            bt2020_primaries(),
            d65_white_point(),
            mastering_max_nits.unwrap_or(1000),
            mastering_min_units.unwrap_or(50),
            max_cll_nits.unwrap_or(1000),
            max_fall_nits.unwrap_or(400),
        );
        let blob = self
            .card
            .create_property_blob(&bytes)
            .context("create_property_blob (HDR_OUTPUT_METADATA)")?;
        let blob_id = match blob {
            property::Value::Blob(id) => id,
            _ => anyhow::bail!("create_property_blob returned non-Blob value"),
        };
        if let Some(prev) = self.hdr_state.take() {
            let _ = self.card.destroy_property_blob(prev.blob_id);
        }
        self.hdr_state = Some(HdrState { blob_id, eotf });
        self.hdr_dirty = true;
        Ok(())
    }

    /// Detach the HDR metadata from the connector — the next atomic
    /// commit writes `HDR_OUTPUT_METADATA = 0`, which the kernel
    /// translates to "no DRMI InfoFrame", and the panel falls back
    /// to SDR / Rec.709 signalling. Frees the blob the kernel was
    /// referencing. Idempotent: returns immediately when no HDR
    /// state is held. Carries `ALLOW_MODESET` on the next commit
    /// because the EOTF transition re-trains the link.
    pub fn clear_hdr_output_metadata(&mut self) {
        let Some(prev) = self.hdr_state.take() else {
            return;
        };
        let _ = self.card.destroy_property_blob(prev.blob_id);
        self.hdr_dirty = true;
    }

    /// Drain a one-shot reason string set when `present_prime` falls
    /// back from atomic_commit to legacy `set_crtc`. The caller emits
    /// a single `display_atomic_unavailable` Warning event. Subsequent
    /// calls return `None` — the disable is sticky.
    pub fn take_atomic_fallback_reason(&mut self) -> Option<String> {
        self.atomic_fallback_reason.take()
    }

    /// Allocate the audio-bars overlay plane. Walks `plane_handles()`
    /// for an Overlay (or Cursor as fallback) plane drivable from our
    /// CRTC and advertising `DRM_FORMAT_ARGB8888`, allocates an
    /// ARGB8888 dumb buffer sized `panel_w × strip_h`, and resolves
    /// the plane's atomic property IDs. Subsequent `present_prime`
    /// calls compose the overlay onto the bottom strip of the panel
    /// via a single atomic commit alongside the prime FB on the
    /// primary plane.
    ///
    /// Pre-conditions: atomic-commit is available (the discovery path
    /// uses universal-planes + atomic-class properties), and the host
    /// driver advertises at least one Overlay or Cursor plane drivable
    /// from this CRTC. Hosts that don't satisfy either fall back to
    /// the CPU-blit bars-rasterise path inside the dumb buffer (the
    /// caller checks the return value and routes accordingly).
    pub fn enable_bars_overlay(&mut self) -> Result<()> {
        if self.bars_overlay.is_some() {
            return Ok(());
        }
        if !self.use_atomic {
            anyhow::bail!(
                "bars-overlay needs atomic_commit for multi-plane composition; legacy set_crtc supports only one plane"
            );
        }
        // The atomic discovery path is shared with `present_prime` —
        // we trigger it here so the primary-plane setup is ready
        // before we go looking for a sibling overlay.
        if self.atomic.is_none() {
            let setup = discover_atomic_setup(&self.card, self.crtc, self.connector)
                .context("atomic setup discovery (bars overlay prereq)")?;
            self.atomic = Some(setup);
        }
        let primary_plane = self
            .atomic
            .as_ref()
            .map(|a| a.plane)
            .expect("atomic just set");

        let strip_h = match super::audio_bars::compute_strip_height(self.height) {
            Some(h) => h,
            None => anyhow::bail!(
                "panel too short ({}px) to host audio-bars overlay strip",
                self.height
            ),
        };

        let (plane_h, plane_props) =
            discover_bars_overlay_plane(&self.card, self.crtc, primary_plane)
                .context("bars-overlay plane discovery")?;
        let (zpos, zpos_value, pixel_blend_mode, pixel_blend_coverage_value) =
            discover_bars_overlay_blend_props(&self.card, plane_h, primary_plane)
                .context("bars-overlay zpos/blend property discovery")?;
        let dumbs = alloc_bars_dumb_pair(&self.card, self.width, strip_h)?;
        self.bars_overlay = Some(BarsOverlay {
            plane: plane_h,
            plane_props,
            dumbs,
            front_idx: 0,
            width: self.width,
            height: strip_h,
            committed: false,
            zpos,
            zpos_value,
            pixel_blend_mode,
            pixel_blend_coverage_value,
        });
        Ok(())
    }

    /// Tear down the audio-bars overlay plane — clear the plane on
    /// the next atomic commit, destroy the FB, drop the dumb buffer.
    /// Idempotent. The caller invokes this when the operator clears
    /// `show_audio_bars` mid-flow or when the display task shuts down.
    /// Normal shutdown also reclaims the FB through `Drop` on the
    /// embedded `DumbBuffer` plus the kernel-side reap when the DRM
    /// fd closes; this method is the explicit-teardown path for
    /// runtime toggles.
    #[allow(dead_code)]
    pub fn disable_bars_overlay(&mut self) {
        let Some(bars) = self.bars_overlay.take() else {
            return;
        };
        if bars.committed {
            // Detach the plane from our CRTC: a tiny atomic commit
            // with FB_ID = 0 + CRTC_ID = 0. Failure is non-fatal —
            // destroying the FBs drops the kernel-side reference.
            let mut req = AtomicModeReq::new();
            req.add_property(bars.plane, bars.plane_props.fb_id, property::Value::Framebuffer(None));
            req.add_property(bars.plane, bars.plane_props.crtc_id, property::Value::CRTC(None));
            let _ = self
                .card
                .atomic_commit(AtomicCommitFlags::empty(), req);
        }
        let [d0, d1] = bars.dumbs;
        for d in [d0, d1] {
            let DumbBuffer { mapping, fb, handle } = d;
            drop(mapping);
            let _ = self.card.destroy_framebuffer(fb);
            let _ = self.card.destroy_dumb_buffer(handle);
        }
    }

    /// Reallocate the bars-overlay dumb buffer to match the panel's
    /// **current** width and a strip height derived from the current
    /// panel height. No-op when the overlay isn't enabled or when the
    /// new dims match the existing buffer. Tears the overlay down when
    /// the new panel is too short to host a strip at all.
    ///
    /// Called from `match_source_resolution` and `set_monitor_native_mode`
    /// after a panel-size mode change. Without this, the overlay buffer
    /// stays sized to the previous mode's width — the rasteriser then
    /// centres bars using the buffer's (old, larger) width while the
    /// panel only scans out the leftmost panel-width slice, pushing the
    /// bars off-screen to the right and giving the operator a sliver of
    /// the leftmost bar(s) instead of the full strip.
    fn resync_bars_overlay_to_panel(&mut self) -> Result<()> {
        if self.bars_overlay.is_none() {
            return Ok(());
        }
        let new_strip_h = match super::audio_bars::compute_strip_height(self.height) {
            Some(h) => h,
            None => {
                // Panel shrank below the minimum strip-host height
                // — tear the overlay down. The next compatible mode
                // change won't re-enable it (enable_bars_overlay is
                // a one-shot at task startup); operators on this code
                // path are already on a tiny panel where the bars
                // wouldn't be readable anyway.
                self.disable_bars_overlay();
                return Ok(());
            }
        };
        {
            let bars = self.bars_overlay.as_ref().expect("just checked Some");
            if bars.width == self.width && bars.height == new_strip_h {
                return Ok(());
            }
        }
        let new_dumbs = alloc_bars_dumb_pair(&self.card, self.width, new_strip_h)
            .context("bars-overlay re-allocation after panel mode change")?;
        let bars = self.bars_overlay.as_mut().expect("just checked Some");
        let old_dumbs = std::mem::replace(&mut bars.dumbs, new_dumbs);
        bars.width = self.width;
        bars.height = new_strip_h;
        bars.front_idx = 0;
        // Force the next atomic commit to re-program every plane
        // property — the kernel's last-seen FB ID + SRC/CRTC rects
        // referenced the destroyed dumb buffers.
        bars.committed = false;
        // End the `bars` borrow before touching `self.card` for the
        // destroy syscalls.
        let _ = bars;
        let [old0, old1] = old_dumbs;
        for d in [old0, old1] {
            let DumbBuffer { mapping, fb, handle } = d;
            drop(mapping);
            let _ = self.card.destroy_framebuffer(fb);
            let _ = self.card.destroy_dumb_buffer(handle);
        }
        Ok(())
    }

    /// Borrow the overlay-plane bars buffer for a fresh rasterise.
    /// `None` when bars haven't been enabled. The caller writes
    /// ARGB8888 pixels via [`super::audio_bars::rasterise_overlay`];
    /// the next `present_prime` call composes the buffer onto the
    /// strip via atomic commit.
    ///
    /// Returns the **back** half of the ping-pong pair — the buffer
    /// the kernel is *not* currently scanning out — so the rasteriser
    /// never races the scanout engine. After `present_prime`'s atomic
    /// commit lands and the flip-complete event fires,
    /// `advance_bars_overlay_front` swaps front/back so the next call
    /// here hands back the freshly-freed buffer.
    pub fn bars_overlay_buffer(&mut self) -> Option<DumbBufferMap<'_>> {
        let bars = self.bars_overlay.as_mut()?;
        let back = bars.front_idx ^ 1;
        let pitch = bars.dumbs[back].handle.pitch();
        Some(DumbBufferMap {
            slice: bars.dumbs[back].mapping.as_mut(),
            pitch,
            width: bars.width,
            height: bars.height,
        })
    }

    /// Advance the bars-overlay ping-pong: the back buffer that the
    /// last atomic commit referenced has now been flipped to and is
    /// the new "front" (currently scanning out); the buffer that *was*
    /// front is safe to overwrite. Called from `present_prime` after
    /// `wait_page_flip` confirms the flip event arrived. No-op when
    /// the overlay isn't enabled.
    fn advance_bars_overlay_front(&mut self) {
        if let Some(bars) = self.bars_overlay.as_mut() {
            bars.front_idx ^= 1;
        }
    }

    /// `(width_px, strip_height_px)` of the audio-bars overlay buffer,
    /// or `None` when the overlay isn't enabled. The caller passes
    /// these to [`super::audio_bars::rasterise_overlay`] so layout
    /// math matches the buffer the kernel will actually scan out.
    pub fn bars_overlay_dims(&self) -> Option<(u32, u32)> {
        self.bars_overlay.as_ref().map(|b| (b.width, b.height))
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn refresh_hz(&self) -> u32 {
        self.mode.vrefresh() as u32
    }

    /// Re-program the connector to the best mode covering source dims
    /// `(src_w, src_h)`. When `src_fps_hint` is `Some`, the picker
    /// prefers refresh rates that are integer multiples of the source
    /// fps so each source frame holds for a whole number of vblanks —
    /// **this eliminates the 2:3 / 1:2 pulldown judder a 25 fps source
    /// produces on a 60 Hz panel and a 50 fps source produces on a 60 Hz
    /// panel**, which on real broadcast content reads to the operator
    /// as "the picture is just slightly stuttery". When `src_fps_hint`
    /// is `None` (the source's frame period hasn't stabilised yet) the
    /// picker falls back to highest-refresh-rate to drive the panel at
    /// its native cadence.
    ///
    /// Strategy:
    /// 1. Smallest mode whose dims are both ≥ source — avoids the 4K
    ///    upscale CPU spike when a 1080p source lands on a 4K panel.
    /// 2. Among same-dim candidates: cleanly-divisible refresh wins
    ///    (a 50 Hz mode for 25/50 fps content, 60 Hz for 30/60 fps,
    ///    100 Hz for 25/50 fps if the panel offers it). Tie-break on
    ///    highest refresh (panel's preferred / native is least likely
    ///    to flicker).
    /// 3. If every mode is smaller than the source, pick the largest.
    ///
    /// Drops + re-allocates the dumb-buffer pair if the new mode size
    /// differs. No-op when the new mode is identical to the current one.
    pub fn match_source_resolution(
        &mut self,
        src_w: u32,
        src_h: u32,
        src_fps_hint: Option<f32>,
    ) -> Result<()> {
        let info = self
            .card
            .get_connector(self.connector, true)
            .context("get_connector for auto-match")?;
        let new_mode = pick_mode_for_source_dims_only(&info, src_w, src_h, src_fps_hint)?;
        let (new_w_i16, new_h_i16) = new_mode.size();
        let new_w = new_w_i16 as u32;
        let new_h = new_h_i16 as u32;
        let same_size = new_w == self.width && new_h == self.height;
        let same_rate = new_mode.vrefresh() as u32 == self.mode.vrefresh() as u32;
        if same_size && same_rate {
            return Ok(());
        }

        if same_size {
            // Refresh-only change: re-program existing buffers.
            self.card
                .set_crtc(
                    self.crtc,
                    Some(self.bufs[self.front_idx].fb),
                    (0, 0),
                    &[self.connector],
                    Some(new_mode),
                )
                .context("display_mode_set_failed: auto-match refresh re-modeset")?;
            self.mode = new_mode;
            self.invalidate_atomic_modeset();
            return Ok(());
        }

        let new_a = alloc_dumb_buffer(&self.card, new_w, new_h)?;
        let new_b = alloc_dumb_buffer(&self.card, new_w, new_h)?;
        self.card
            .set_crtc(
                self.crtc,
                Some(new_a.fb),
                (0, 0),
                &[self.connector],
                Some(new_mode),
            )
            .context("display_mode_set_failed: auto-match re-modeset")?;
        let old_bufs = std::mem::replace(&mut self.bufs, [new_a, new_b]);
        for old in old_bufs {
            // Drop the persistent mapping (munmap) before destroying the
            // kernel-side dumb buffer. Destructure with `..` is fine —
            // `DumbBuffer` has no `Drop` impl, so partial moves are
            // permitted.
            let DumbBuffer { mapping, fb, handle } = old;
            drop(mapping);
            let _ = self.card.destroy_framebuffer(fb);
            let _ = self.card.destroy_dumb_buffer(handle);
        }
        self.mode = new_mode;
        self.width = new_w;
        self.height = new_h;
        self.front_idx = 0;
        self.invalidate_atomic_modeset();
        if let Err(e) = self.resync_bars_overlay_to_panel() {
            tracing::warn!(
                "audio-bars overlay resync after match-source modeset failed: {e:#}"
            );
        }
        Ok(())
    }

    /// A legacy `set_crtc` modeset has just landed. Free the cached
    /// mode-blob (it's now stale) and clear `atomic_modeset_done` so
    /// the next atomic flip re-asserts CRTC.ACTIVE / CRTC.MODE_ID /
    /// CONNECTOR.CRTC_ID with `ALLOW_MODESET`. No-op when the atomic
    /// path has never been used. Also drops the HDR metadata blob —
    /// a legacy modeset clears connector signalling state on most
    /// drivers, so the next HDR-bearing flip re-allocates the blob.
    fn invalidate_atomic_modeset(&mut self) {
        self.atomic_modeset_done = false;
        if let Some(setup) = self.atomic.as_mut() {
            if let Some(blob) = setup.mode_blob_id.take() {
                let _ = self.card.destroy_property_blob(blob);
            }
        }
        if let Some(prev) = self.hdr_state.take() {
            let _ = self.card.destroy_property_blob(prev.blob_id);
            self.hdr_dirty = true;
        }
    }

    /// Re-program the connector to its preferred (panel-native) mode.
    /// Used by the `MonitorNative` scaling mode at display-task startup
    /// to hold the panel at its EDID-preferred resolution / refresh and
    /// rely on libswscale to upscale source frames into the dumb buffer.
    ///
    /// Falls back to the first listed mode when EDID has no PREFERRED
    /// flag — pure-OSS amdgpu / nouveau builds and some captured EDIDs
    /// in the wild leave it unset.
    ///
    /// Drops + re-allocates the dumb-buffer pair if the new mode size
    /// differs from the current one. No-op when identical.
    pub fn set_monitor_native_mode(&mut self) -> Result<()> {
        let info = self
            .card
            .get_connector(self.connector, true)
            .context("get_connector for monitor-native")?;
        let new_mode = pick_preferred_mode(&info);
        let (new_w_i16, new_h_i16) = new_mode.size();
        let new_w = new_w_i16 as u32;
        let new_h = new_h_i16 as u32;
        let same_size = new_w == self.width && new_h == self.height;
        let same_rate = new_mode.vrefresh() as u32 == self.mode.vrefresh() as u32;
        if same_size && same_rate {
            return Ok(());
        }
        if same_size {
            self.card
                .set_crtc(
                    self.crtc,
                    Some(self.bufs[self.front_idx].fb),
                    (0, 0),
                    &[self.connector],
                    Some(new_mode),
                )
                .context("display_mode_set_failed: monitor-native refresh re-modeset")?;
            self.mode = new_mode;
            self.invalidate_atomic_modeset();
            return Ok(());
        }
        let new_a = alloc_dumb_buffer(&self.card, new_w, new_h)?;
        let new_b = alloc_dumb_buffer(&self.card, new_w, new_h)?;
        self.card
            .set_crtc(
                self.crtc,
                Some(new_a.fb),
                (0, 0),
                &[self.connector],
                Some(new_mode),
            )
            .context("display_mode_set_failed: monitor-native re-modeset")?;
        let old_bufs = std::mem::replace(&mut self.bufs, [new_a, new_b]);
        for old in old_bufs {
            let DumbBuffer { mapping, fb, handle } = old;
            drop(mapping);
            let _ = self.card.destroy_framebuffer(fb);
            let _ = self.card.destroy_dumb_buffer(handle);
        }
        self.mode = new_mode;
        self.width = new_w;
        self.height = new_h;
        self.front_idx = 0;
        self.invalidate_atomic_modeset();
        if let Err(e) = self.resync_bars_overlay_to_panel() {
            tracing::warn!(
                "audio-bars overlay resync after monitor-native modeset failed: {e:#}"
            );
        }
        Ok(())
    }

    /// Hand the back buffer's CPU mapping to the caller for direct
    /// pixel writes. The caller writes a W×H XRGB8888 block into the
    /// returned slice (stride = buffer pitch). The mapping is **persistent**
    /// — it was mmap'd once at alloc time and lives as long as the
    /// `DumbBuffer`. The returned `DumbBufferMap` only borrows the slice;
    /// dropping it just releases the borrow (no syscall).
    pub fn back_buffer(&mut self) -> Result<DumbBufferMap<'_>> {
        let back_idx = 1 - self.front_idx;
        let width = self.width;
        let height = self.height;
        let pitch = self.bufs[back_idx].handle.pitch();
        Ok(DumbBufferMap {
            slice: self.bufs[back_idx].mapping.as_mut(),
            pitch,
            width,
            height,
        })
    }

    /// CPU-blit sibling of [`Self::present_prime`]. Flips the dumb-buffer
    /// back FB onto the primary plane via atomic_commit so the bars
    /// overlay plane gets re-armed in the same commit — without this,
    /// a single VAAPI-fail frame that falls through to legacy
    /// `page_flip` can implicitly detach non-primary planes on some
    /// drivers, vanishing the bars overlay until the next prime frame
    /// re-arms it. Falls back to legacy `present()` when atomic isn't
    /// available (host lacks atomic, or atomic_commit hit
    /// EOPNOTSUPP / EINVAL earlier in the session and `use_atomic`
    /// got flipped off).
    pub fn present_cpu_atomic(&mut self) -> Result<()> {
        if !self.use_atomic || self.atomic.is_none() {
            return self.present();
        }
        let back_idx = 1 - self.front_idx;
        let new_fb = self.bufs[back_idx].fb;
        let width = self.width;
        let height = self.height;
        match self.atomic_present(new_fb, width, height) {
            Ok(()) => {
                self.wait_page_flip()
                    .context("display_page_flip_failed: receive_events")?;
                // Same ping-pong promotion the prime path does — the
                // bars overlay back buffer just submitted has been
                // flipped to, so the rasteriser must land on the other
                // half on the next iteration.
                self.advance_bars_overlay_front();
                self.front_idx = back_idx;
                Ok(())
            }
            Err(AtomicCommitError::Unsupported(e)) => {
                // Driver refused atomic for this commit shape. Don't
                // permanently latch `use_atomic = false`: a subsequent
                // prime frame may still atomic-commit successfully on
                // some drivers. Fall back to legacy for this frame
                // only — the bars overlay will re-arm on the next
                // successful prime atomic commit.
                tracing::debug!(
                    "atomic_commit unsupported on CPU-blit flip; falling back to legacy page_flip: {e}"
                );
                self.present()
            }
            Err(AtomicCommitError::Other(e)) => Err(anyhow::anyhow!(
                "display_page_flip_failed: atomic_commit: {e}"
            )),
        }
    }

    /// Queue a vblank-synchronous page flip and block until the kernel
    /// posts `DRM_EVENT_FLIP_COMPLETE` for our CRTC. This is the proper
    /// per-frame primitive: the kernel atomically swaps the scanout source
    /// at the next vblank without reprogramming the CRTC. Cost is
    /// microseconds, not the 10–30 ms of `drmModeSetCrtc`.
    ///
    /// The DRM fd is opened in blocking mode (no `O_NONBLOCK`), so the
    /// `read()` inside `receive_events()` blocks until events arrive.
    /// The loop guards against unrelated events (e.g. a vblank request
    /// from another consumer) being drained ahead of our flip event.
    pub fn present(&mut self) -> Result<()> {
        let back_idx = 1 - self.front_idx;
        self.card
            .page_flip(
                self.crtc,
                self.bufs[back_idx].fb,
                PageFlipFlags::EVENT,
                None,
            )
            .context("display_page_flip_failed: page_flip queue")?;
        loop {
            let events = self
                .card
                .receive_events()
                .context("display_page_flip_failed: receive_events")?;
            let mut flipped = false;
            for ev in events {
                if let Event::PageFlip(p) = ev {
                    if p.crtc == self.crtc {
                        flipped = true;
                    }
                }
            }
            if flipped {
                break;
            }
        }
        self.front_idx = back_idx;
        Ok(())
    }
}

/// Borrowed view into a dumb buffer's persistent CPU mapping. Dropping
/// just releases the `&mut` borrow on the underlying `DumbBuffer` — no
/// syscall, no munmap. The mapping itself is created once at
/// `alloc_dumb_buffer` time and lives for the buffer's lifetime.
pub struct DumbBufferMap<'a> {
    slice: &'a mut [u8],
    pitch: u32,
    width: u32,
    height: u32,
}

impl<'a> DumbBufferMap<'a> {
    pub fn pitch(&self) -> u32 {
        self.pitch
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn as_mut(&mut self) -> &mut [u8] {
        self.slice
    }
}

fn alloc_dumb_buffer(card: &CardFile, w: u32, h: u32) -> Result<DumbBuffer> {
    let mut handle = card
        .create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32)
        .context("create_dumb_buffer")?;
    let fb = card
        .add_framebuffer(&handle, 24, 32)
        .context("add_framebuffer")?;
    let mapping = card
        .map_dumb_buffer(&mut handle)
        .context("map_dumb_buffer")?;
    // SAFETY: lifetime extension. `DumbMapping<'a>` carries an mmap'd
    // kernel slice independent of `handle`'s Rust-side struct (the `'a`
    // is `PhantomData`). We bind `mapping` and `handle` together inside
    // the same `DumbBuffer` and the field declaration order ensures
    // `mapping` is dropped (munmapped) before `handle` is consumed by
    // `destroy_dumb_buffer` in the cleanup paths.
    let mapping: DumbMapping<'static> = unsafe { std::mem::transmute(mapping) };
    Ok(DumbBuffer { mapping, handle, fb })
}

fn pick_mode(
    info: &drm::control::connector::Info,
    width: Option<u32>,
    height: Option<u32>,
    refresh_hz: Option<u32>,
) -> Result<drm::control::Mode> {
    let modes = info.modes();
    if modes.is_empty() {
        anyhow::bail!("display_resolution_unsupported: connector has no modes");
    }
    // 1. Exact (W,H,Hz) match if all three were specified.
    if let (Some(w), Some(h), Some(hz)) = (width, height, refresh_hz) {
        if let Some(m) = modes.iter().find(|m| {
            m.size().0 as u32 == w && m.size().1 as u32 == h && m.vrefresh() as u32 == hz
        }) {
            return Ok(*m);
        }
    }
    // 2. (W, H) match — pick highest refresh.
    if let (Some(w), Some(h)) = (width, height) {
        let mut candidates: Vec<&drm::control::Mode> = modes
            .iter()
            .filter(|m| m.size().0 as u32 == w && m.size().1 as u32 == h)
            .collect();
        candidates.sort_by_key(|m| std::cmp::Reverse(m.vrefresh()));
        if let Some(m) = candidates.first() {
            return Ok(**m);
        }
    }
    // 3. Connector-preferred (panel-native) mode, then first-listed.
    Ok(pick_preferred_mode(info))
}

/// Return the connector's preferred (panel-native) mode if EDID flagged
/// one, otherwise the first mode the connector reports. Both branches are
/// safe to call only after the caller has already verified `info.modes()`
/// is non-empty (see `pick_mode` / `pick_mode_for_source_dims_only`).
fn pick_preferred_mode(info: &drm::control::connector::Info) -> drm::control::Mode {
    let modes = info.modes();
    if let Some(m) = modes.iter().find(|m| {
        (m.mode_type() & drm::control::ModeTypeFlags::PREFERRED)
            == drm::control::ModeTypeFlags::PREFERRED
    }) {
        return *m;
    }
    modes[0]
}

/// Pick the smallest connector mode whose dims are both ≥ source
/// `(src_w, src_h)`. Tie-break on highest refresh (panel-native is
/// what works without flicker). Falls back to the largest available
/// mode when every mode is smaller than the source.
///
/// When `src_fps_hint` is supplied, candidate modes whose refresh rate
/// is an integer multiple of the source fps (within ±0.5 Hz tolerance)
/// rank ahead of others — eliminates the 2:3 / 1:2 pulldown judder a
/// 25 fps source produces on a 60 Hz panel.
fn pick_mode_for_source_dims_only(
    info: &drm::control::connector::Info,
    src_w: u32,
    src_h: u32,
    src_fps_hint: Option<f32>,
) -> Result<drm::control::Mode> {
    let modes = info.modes();
    if modes.is_empty() {
        anyhow::bail!("display_resolution_unsupported: connector has no modes");
    }
    // Returns `0` if the mode's refresh is a clean integer multiple of
    // the source fps (rank A — lowest sort key wins), else `1` (rank B).
    // No fps hint → all modes rank A so we fall through to plain
    // highest-refresh ordering.
    let cadence_rank = |refresh_hz: u32| -> u8 {
        let Some(fps) = src_fps_hint else { return 0 };
        if fps <= 0.0 {
            return 0;
        }
        let ratio = refresh_hz as f32 / fps;
        let rounded = ratio.round();
        if rounded >= 1.0 && (ratio - rounded).abs() < 0.05 {
            0
        } else {
            1
        }
    };
    let exact: Vec<&drm::control::Mode> = modes
        .iter()
        .filter(|m| m.size().0 as u32 == src_w && m.size().1 as u32 == src_h)
        .collect();
    if !exact.is_empty() {
        let mut em = exact;
        em.sort_by_key(|m| {
            (
                cadence_rank(m.vrefresh() as u32),
                std::cmp::Reverse(m.vrefresh()),
            )
        });
        return Ok(*em[0]);
    }
    let mut at_or_above: Vec<&drm::control::Mode> = modes
        .iter()
        .filter(|m| m.size().0 as u32 >= src_w && m.size().1 as u32 >= src_h)
        .collect();
    if !at_or_above.is_empty() {
        at_or_above.sort_by_key(|m| {
            (
                (m.size().0 as u64) * (m.size().1 as u64),
                cadence_rank(m.vrefresh() as u32) as u64,
                std::cmp::Reverse(m.vrefresh() as u64),
            )
        });
        return Ok(*at_or_above[0]);
    }
    let mut all: Vec<&drm::control::Mode> = modes.iter().collect();
    all.sort_by_key(|m| {
        (
            std::cmp::Reverse((m.size().0 as u64) * (m.size().1 as u64)),
            cadence_rank(m.vrefresh() as u32) as u64,
            std::cmp::Reverse(m.vrefresh() as u64),
        )
    });
    Ok(*all[0])
}

fn locate_connector(name: &str) -> Result<(PathBuf, drm::control::connector::Handle)> {
    for path in enumerate_card_paths() {
        let card = match open_card(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let resources = match card.resource_handles() {
            Ok(r) => r,
            Err(_) => continue,
        };
        for &h in resources.connectors() {
            if let Ok(info) = card.get_connector(h, false) {
                let candidate = format!(
                    "{}-{}",
                    connector_protocol_label(info.interface() as u32),
                    info.interface_id()
                );
                if candidate == name {
                    return Ok((path, h));
                }
            }
        }
    }
    anyhow::bail!(
        "display_device_invalid: connector '{}' not found under /dev/dri/",
        name
    )
}

/// Electro-optical transfer function for HDR output signalling, per the
/// `hdr_metadata_infoframe.eotf` field of the kernel's
/// `hdr_output_metadata` blob. Drives the EOTF byte we send to the
/// panel via the connector's `HDR_OUTPUT_METADATA` property — the
/// panel reads it (over the HDMI Dynamic Range and Mastering
/// InfoFrame) and switches into the matching pipeline (typical UHD TV
/// behaviour: PQ → "HDR" mode, HLG → "HLG HDR" mode, traditional →
/// SDR / "BT.2020 video" mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrEotf {
    /// Traditional gamma SDR (Rec.709-style EOTF). EOTF byte = 0.
    /// The default when no HDR metadata is set. Held for symmetry
    /// with the kernel's `hdr_metadata_infoframe.eotf` enum;
    /// `clear_hdr_output_metadata` is the operator-facing API for
    /// returning to SDR rather than passing this variant through
    /// `set_hdr_output_metadata`.
    #[allow(dead_code)]
    SdrGamma,
    /// SMPTE ST 2084 (PQ) absolute-luminance EOTF, 0–10 000 nits.
    /// EOTF byte = 2. Standard HDR10 / HDR10+ / Dolby Vision base
    /// layer source signalling.
    Pq,
    /// ARIB STD-B67 / Rec.2100 HLG (Hybrid Log-Gamma), display-
    /// relative. EOTF byte = 3. Standard for live broadcast HDR
    /// (BBC, NHK, DVB) — backwards-compatible with SDR Rec.709
    /// receivers that ignore the metadata.
    Hlg,
}

impl HdrEotf {
    /// EOTF byte exactly as written into the kernel's
    /// `hdr_metadata_infoframe.eotf` field.
    fn byte(self) -> u8 {
        match self {
            HdrEotf::SdrGamma => 0,
            HdrEotf::Pq => 2,
            HdrEotf::Hlg => 3,
        }
    }
}

/// Wire layout of the kernel's `hdr_output_metadata` struct (see
/// `linux/include/uapi/drm/drm_mode.h`). 30 bytes packed; the kernel
/// requires a `__packed__` layout. We render the blob into a raw
/// `[u8; 30]` and pass it to `create_property_blob` rather than rely
/// on a `#[repr(C, packed)]` struct + bytemuck, both because the
/// drm-rs `create_property_blob` already takes a `&T` it
/// reinterprets as bytes via `mem::size_of::<T>()`, and because the
/// explicit byte layout means a future change to the kernel struct
/// (e.g. metadata_type variants beyond Type 1) only touches one
/// place. CTA-861-G primary / white-point quantisation: x = round(x ×
/// 50 000); luminance is in nits (max) or 1/10 000 nit (min).
const HDR_METADATA_BLOB_SIZE: usize = 30;

#[allow(clippy::too_many_arguments)]
fn build_hdr_output_metadata_blob(
    eotf: HdrEotf,
    primaries: [(u16, u16); 3],
    white_point: (u16, u16),
    max_display_mastering_luminance_nits: u16,
    min_display_mastering_luminance_units: u16,
    max_cll_nits: u16,
    max_fall_nits: u16,
) -> [u8; HDR_METADATA_BLOB_SIZE] {
    let mut blob = [0u8; HDR_METADATA_BLOB_SIZE];
    // u32 metadata_type = 0 (HDMI Type 1) — already zeroed.
    blob[4] = eotf.byte();
    blob[5] = 0; // metadata_type (inner) = 0 (HDMI Type 1)
    let mut o = 6;
    for (x, y) in primaries.iter() {
        blob[o..o + 2].copy_from_slice(&x.to_le_bytes());
        blob[o + 2..o + 4].copy_from_slice(&y.to_le_bytes());
        o += 4;
    }
    blob[o..o + 2].copy_from_slice(&white_point.0.to_le_bytes());
    blob[o + 2..o + 4].copy_from_slice(&white_point.1.to_le_bytes());
    o += 4;
    blob[o..o + 2].copy_from_slice(&max_display_mastering_luminance_nits.to_le_bytes());
    blob[o + 2..o + 4].copy_from_slice(&min_display_mastering_luminance_units.to_le_bytes());
    o += 4;
    blob[o..o + 2].copy_from_slice(&max_cll_nits.to_le_bytes());
    blob[o + 2..o + 4].copy_from_slice(&max_fall_nits.to_le_bytes());
    blob
}

/// BT.2020 / Rec.2100 RGB primaries quantised per CTA-861-G
/// (`x = round(x × 50000)`). Used as the default mastering
/// primaries when the source bitstream doesn't carry a Mastering
/// Display SEI — every UHD panel knows BT.2020, so the metadata
/// remains semantically correct even with the placeholder values.
fn bt2020_primaries() -> [(u16, u16); 3] {
    // Red:   (0.708, 0.292)  → (35400, 14600)
    // Green: (0.170, 0.797)  → ( 8500, 39850)
    // Blue:  (0.131, 0.046)  → ( 6550,  2300)
    [(35400, 14600), (8500, 39850), (6550, 2300)]
}

/// D65 white point, quantised per CTA-861-G (`x = round(x × 50000)`).
/// `(0.3127, 0.3290) → (15635, 16450)`.
fn d65_white_point() -> (u16, u16) {
    (15635, 16450)
}

// ── Atomic-commit helpers ─────────────────────────────────────────────
//
// The atomic-commit page-flip path (used by `KmsDisplay::present_prime`
// for VAAPI zero-copy scanout) needs cached property IDs for every
// kernel object we touch on each flip — plane (FB_ID, CRTC_ID, SRC_*,
// CRTC_*), CRTC (ACTIVE, MODE_ID), and connector (CRTC_ID). The kernel
// keeps property IDs stable for the lifetime of the DRM fd, so we look
// them up once on the first PRIME present and reuse from there.
//
// Discovery walks `card.plane_handles()` (universal-planes cap is
// enabled in `KmsDisplay::open`), inspects each plane's `type` property
// for the `Primary` enum value, and keeps the first one whose
// `pos_crtcs` bitmask includes our CRTC's index in `resources.crtcs()`.
// On the rare host with multiple primary planes per CRTC the kernel's
// own enumeration order is what compositors rely on — same here.

/// Look up `property::Handle` IDs by name for an object, returning a
/// missing-name error so the caller can fall back rather than panic.
fn collect_props(
    card: &CardFile,
    handle: impl drm::control::ResourceHandle,
    names: &[&str],
) -> Result<Vec<property::Handle>> {
    let set = card
        .get_properties(handle)
        .context("get_properties for atomic discovery")?;
    let (ids, _vals) = set.as_props_and_values();
    let mut out: Vec<Option<property::Handle>> = vec![None; names.len()];
    for &id in ids {
        let info = card
            .get_property(id)
            .context("get_property for atomic discovery")?;
        let nm = info.name();
        let nm_bytes = nm.to_bytes();
        for (i, want) in names.iter().enumerate() {
            if out[i].is_none() && nm_bytes == want.as_bytes() {
                out[i] = Some(id);
            }
        }
    }
    let mut filled = Vec::with_capacity(names.len());
    for (i, slot) in out.into_iter().enumerate() {
        let h = slot.with_context(|| {
            format!(
                "atomic property '{}' not found on object — kernel too old or driver lacks atomic support",
                names[i]
            )
        })?;
        filled.push(h);
    }
    Ok(filled)
}

/// Look up a single property by name on an object, returning `None`
/// (rather than `Err`) when it doesn't exist. Used for atomic
/// properties that are optional on a given driver / kernel — for
/// example `HDR_OUTPUT_METADATA`, which only newer kernels (5.14+ for
/// amdgpu, much earlier on i915) advertise on connectors.
fn optional_prop(
    card: &CardFile,
    handle: impl drm::control::ResourceHandle,
    name: &str,
) -> Result<Option<property::Handle>> {
    let set = card
        .get_properties(handle)
        .context("get_properties for optional prop discovery")?;
    let (ids, _vals) = set.as_props_and_values();
    for id in ids {
        let info = card
            .get_property(*id)
            .context("get_property for optional prop discovery")?;
        if info.name().to_bytes() == name.as_bytes() {
            return Ok(Some(*id));
        }
    }
    Ok(None)
}

/// Read the `type` enum value off a plane. Returns the raw value
/// (0 = Overlay, 1 = Primary, 2 = Cursor per `DRM_PLANE_TYPE_*`).
fn read_plane_type(card: &CardFile, plane_h: plane::Handle) -> Result<u64> {
    let set = card
        .get_properties(plane_h)
        .context("get_properties on plane")?;
    let (ids, vals) = set.as_props_and_values();
    for (id, val) in ids.iter().zip(vals.iter()) {
        let info = card
            .get_property(*id)
            .context("get_property on plane.type")?;
        if info.name().to_bytes() == b"type" {
            return Ok(*val);
        }
    }
    anyhow::bail!("plane has no 'type' property")
}

fn discover_atomic_setup(
    card: &CardFile,
    crtc: drm::control::crtc::Handle,
    connector: drm::control::connector::Handle,
) -> Result<AtomicSetup> {
    let res = card
        .resource_handles()
        .context("resource_handles for atomic discovery")?;
    if !res.crtcs().iter().any(|c| *c == crtc) {
        anyhow::bail!("CRTC handle not found in resource enumeration");
    }

    let planes = card
        .plane_handles()
        .context("plane_handles — UniversalPlanes capability rejected?")?;
    let mut chosen: Option<plane::Handle> = None;
    for p in planes {
        let info = card.get_plane(p).context("get_plane")?;
        // `filter_crtcs` decodes the plane's `pos_crtcs` bitmask
        // against the resource enumeration order — the only way to
        // read it without poking at private fields.
        if !res.filter_crtcs(info.possible_crtcs()).iter().any(|c| *c == crtc) {
            continue;
        }
        let ty = read_plane_type(card, p)?;
        if ty == PlaneType::Primary as u64 {
            chosen = Some(p);
            break;
        }
    }
    let plane_h = chosen
        .context("no primary plane reported as drivable by our CRTC — atomic setup impossible")?;

    let plane_names = [
        "FB_ID", "CRTC_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W",
        "CRTC_H",
    ];
    let p = collect_props(card, plane_h, &plane_names)?;
    let plane_props = PlanePropIds {
        fb_id: p[0],
        crtc_id: p[1],
        src_x: p[2],
        src_y: p[3],
        src_w: p[4],
        src_h: p[5],
        crtc_x: p[6],
        crtc_y: p[7],
        crtc_w: p[8],
        crtc_h: p[9],
    };

    let c = collect_props(card, crtc, &["MODE_ID", "ACTIVE"])?;
    let crtc_props = CrtcPropIds {
        mode_id: c[0],
        active: c[1],
    };

    let cn = collect_props(card, connector, &["CRTC_ID"])?;
    let hdr_prop = optional_prop(card, connector, "HDR_OUTPUT_METADATA")?;
    let connector_props = ConnectorPropIds {
        crtc_id: cn[0],
        hdr_output_metadata: hdr_prop,
    };

    Ok(AtomicSetup {
        plane: plane_h,
        plane_props,
        crtc_props,
        connector_props,
        mode_blob_id: None,
    })
}

/// Find an Overlay-class plane (or a Cursor plane as fallback) drivable
/// from `crtc`, supporting `DRM_FORMAT_ARGB8888`, distinct from the
/// primary plane already cached for prime scanout. Returns its handle
/// and the same property-id table the primary plane uses (FB_ID,
/// CRTC_ID, SRC_*, CRTC_*) — the audio-bars overlay touches the same
/// properties on every flip.
fn discover_bars_overlay_plane(
    card: &CardFile,
    crtc: drm::control::crtc::Handle,
    exclude: plane::Handle,
) -> Result<(plane::Handle, PlanePropIds)> {
    let res = card
        .resource_handles()
        .context("resource_handles for bars overlay discovery")?;
    let planes = card
        .plane_handles()
        .context("plane_handles for bars overlay discovery")?;
    // Prefer Overlay over Cursor — overlay planes commonly support
    // ARGB8888 at the panel's full width without the platform-specific
    // cursor-size cap (which on AMD is 256×256 and would clip a 4K
    // strip badly). Cursor is the fallback when the host driver
    // doesn't expose any Overlay planes (rare; some virtual drivers).
    let argb = DrmFourcc::Argb8888 as u32;
    let mut overlay: Option<plane::Handle> = None;
    let mut cursor: Option<plane::Handle> = None;
    for p in planes {
        if p == exclude {
            continue;
        }
        let info = card.get_plane(p).context("get_plane (overlay search)")?;
        if !res.filter_crtcs(info.possible_crtcs()).iter().any(|c| *c == crtc) {
            continue;
        }
        if !info.formats().iter().any(|f| *f == argb) {
            continue;
        }
        match read_plane_type(card, p)? {
            t if t == PlaneType::Overlay as u64 => {
                overlay.get_or_insert(p);
            }
            t if t == PlaneType::Cursor as u64 => {
                cursor.get_or_insert(p);
            }
            _ => {}
        }
    }
    let plane_h = overlay.or(cursor).context(
        "no Overlay or Cursor plane drivable from this CRTC supports ARGB8888 — host driver lacks multi-plane composition",
    )?;
    let names = [
        "FB_ID", "CRTC_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W",
        "CRTC_H",
    ];
    let p = collect_props(card, plane_h, &names)?;
    let plane_props = PlanePropIds {
        fb_id: p[0],
        crtc_id: p[1],
        src_x: p[2],
        src_y: p[3],
        src_w: p[4],
        src_h: p[5],
        crtc_x: p[6],
        crtc_y: p[7],
        crtc_w: p[8],
        crtc_h: p[9],
    };
    Ok((plane_h, plane_props))
}

/// Discover the optional `zpos` / `pixel blend mode` properties on the
/// bars overlay plane and the primary plane's current zpos so we can
/// drive the bars plane **above** the primary in atomic commits.
///
/// Without this, on drivers where Overlay planes default to registration-
/// order zpos below Primary (some `amdgpu`, certain `i915` revisions,
/// older `imx-drm`), the bars rasterise lands on a plane the kernel
/// composites **under** the VAAPI prime FB — the strip is allocated,
/// programmed, and updated every frame, but the operator sees only the
/// video frame because the bars plane is hidden by the primary.
///
/// `pixel blend mode` is set to `Coverage` (straight alpha) so the
/// per-pixel alpha lane drives the blend — `None` means "ignore alpha,
/// plane is fully opaque" which would make the strip a 100 % opaque
/// black box at the bottom of the screen wherever rasterise didn't
/// write. `Pre-multiplied` is also acceptable for our straight ARGB
/// (we write `(R, G, B, 0xFF)` for visible pixels, `0` everywhere
/// else, which is the same as pre-multiplied for these alpha values),
/// but `Coverage` is the simpler and more universally correct choice.
fn discover_bars_overlay_blend_props(
    card: &CardFile,
    bars_plane: plane::Handle,
    primary_plane: plane::Handle,
) -> Result<(Option<property::Handle>, u64, Option<property::Handle>, u64)> {
    let zpos = optional_prop(card, bars_plane, "zpos")?;
    let primary_zpos = read_property_u64(card, primary_plane, "zpos").unwrap_or(0);
    let zpos_value = primary_zpos.saturating_add(1);
    let pixel_blend_mode = optional_prop(card, bars_plane, "pixel blend mode")?;
    let coverage_value = pixel_blend_mode
        .map(|_| read_enum_value(card, bars_plane, "pixel blend mode", "Coverage").unwrap_or(0))
        .unwrap_or(0);
    Ok((zpos, zpos_value, pixel_blend_mode, coverage_value))
}

/// Read a u64-valued property by name, returning `None` when the
/// property is absent on the object. Used to snapshot the primary
/// plane's `zpos` so the bars plane can be ordered above it.
fn read_property_u64(
    card: &CardFile,
    handle: impl drm::control::ResourceHandle,
    name: &str,
) -> Option<u64> {
    let set = card.get_properties(handle).ok()?;
    let (ids, vals) = set.as_props_and_values();
    for (id, val) in ids.iter().zip(vals.iter()) {
        let info = card.get_property(*id).ok()?;
        if info.name().to_bytes() == name.as_bytes() {
            return Some(*val);
        }
    }
    None
}

/// Resolve an enum value by name on a property. KMS enum values are
/// driver-assigned numeric IDs that map to symbolic names (e.g. for
/// `pixel blend mode`: `None` / `Pre-multiplied` / `Coverage`); we
/// look up the numeric ID by string match.
fn read_enum_value(
    card: &CardFile,
    handle: impl drm::control::ResourceHandle,
    prop_name: &str,
    enum_name: &str,
) -> Option<u64> {
    let set = card.get_properties(handle).ok()?;
    let (ids, _vals) = set.as_props_and_values();
    for id in ids {
        let info = card.get_property(*id).ok()?;
        if info.name().to_bytes() != prop_name.as_bytes() {
            continue;
        }
        if let property::ValueType::Enum(enums) = info.value_type() {
            // `enums.values()` returns a `(values: &[u64], defs: &[EnumValue])`
            // pair: each `EnumValue` carries `name()` + `value()`. Match
            // by name and return the numeric ID for atomic-commit use.
            let (_raw_values, defs) = enums.values();
            for e in defs.iter() {
                if e.name().to_bytes() == enum_name.as_bytes() {
                    return Some(e.value() as u64);
                }
            }
        }
        return None;
    }
    None
}

/// ARGB8888 sibling of [`alloc_dumb_buffer`]. Allocates a packed
/// ARGB8888 dumb buffer for the audio-bars overlay strip and creates
/// a KMS framebuffer for it. `depth = 32` (alpha-bearing) so the
/// kernel-side compositor honours the alpha lane during plane blend
/// — `depth = 24` would tag it XRGB and the alpha would be ignored,
/// which on most drivers means the strip would composite as fully
/// opaque black instead of the 50 %-translucent dim the rasteriser
/// writes.
/// Allocate the two ARGB8888 dumb buffers backing the audio-bars overlay
/// ping-pong. On second-alloc failure, tears down the first allocation
/// before returning the error so the kernel doesn't leak the GEM /
/// framebuffer until the DRM fd closes. Used by `enable_bars_overlay`
/// at startup and `resync_bars_overlay_to_panel` after a panel mode
/// change.
fn alloc_bars_dumb_pair(card: &CardFile, w: u32, h: u32) -> Result<[DumbBuffer; 2]> {
    let b0 = alloc_argb_dumb_buffer(card, w, h)
        .context("bars-overlay ARGB8888 dumb buffer (front)")?;
    match alloc_argb_dumb_buffer(card, w, h) {
        Ok(b1) => Ok([b0, b1]),
        Err(e) => {
            let DumbBuffer { mapping, fb, handle } = b0;
            drop(mapping);
            let _ = card.destroy_framebuffer(fb);
            let _ = card.destroy_dumb_buffer(handle);
            Err(e.context("bars-overlay ARGB8888 dumb buffer (back)"))
        }
    }
}

fn alloc_argb_dumb_buffer(card: &CardFile, w: u32, h: u32) -> Result<DumbBuffer> {
    let mut handle = card
        .create_dumb_buffer((w, h), DrmFourcc::Argb8888, 32)
        .context("create_dumb_buffer (ARGB8888)")?;
    let fb = card
        .add_framebuffer(&handle, 32, 32)
        .context("add_framebuffer (ARGB8888)")?;
    let mapping = card
        .map_dumb_buffer(&mut handle)
        .context("map_dumb_buffer (ARGB8888)")?;
    // SAFETY: same lifetime-extension argument as `alloc_dumb_buffer`
    // above — `DumbMapping<'a>` carries an mmap'd kernel slice
    // independent of `handle`'s Rust struct, and `DumbBuffer` field
    // declaration order ensures `mapping` is dropped (munmapped)
    // before `handle` is consumed by `destroy_dumb_buffer` in the
    // teardown paths.
    let mapping: DumbMapping<'static> = unsafe { std::mem::transmute(mapping) };
    Ok(DumbBuffer { mapping, handle, fb })
}

// ── VAAPI zero-copy scanout (DRM PRIME framebuffers) ──────────────────
//
// Inputs from `video_engine::vaapi::DrmPrimeFrame`: a single DMA-BUF fd
// for the whole NV12 / P010 / etc. surface, the `DRM_FORMAT_*` fourcc,
// the `DRM_FORMAT_MOD_*` modifier (Intel iHD often returns Y-tiled or
// the 4-tile gen12 variants; Mesa radeonsi typically returns
// `DRM_FORMAT_MOD_LINEAR` or `INVALID`), per-plane offsets/strides, and
// an Arc keepalive that — when finally dropped — closes the DMA-BUF fd
// and releases the source VAAPI surface back into the decoder's pool.
//
// Pipeline:
//
// 1. `prime_fd_to_buffer(fd)` imports the DMA-BUF into our DRM card's
//    GEM-handle space. The returned `buffer::Handle` is a per-card
//    name; closing it requires `gem_close` (drm-rs lacks a direct
//    helper, so we delete the FB and let the kernel garbage-collect
//    the GEM after the FB destruction unparks the last reference).
// 2. `add_planar_framebuffer(&PrimeBuffer, FbCmd2Flags::MODIFIERS)`
//    issues the `DRM_IOCTL_MODE_ADDFB2_WITH_MODIFIERS` ioctl. Failure
//    here (typical: modifier is one the scanout plane doesn't
//    advertise — Mesa's KMS plane format list excludes some Y-tiled
//    layouts) lifts a hard error so `output_display` demotes to the
//    CPU-blit path on the next frame.
// 3. `page_flip(crtc, fb_id, EVENT, None)` queues the flip and the
//    block-on-event loop in `present_prime` waits for
//    `DRM_EVENT_FLIP_COMPLETE`. The keepalive is held on the stack
//    across that wait so the source VA surface can't be recycled
//    mid-scanout.
// 4. After flip-complete: replace the previous-front FB with the new
//    one in `prime_state` (and destroy the old FB so the kernel can
//    drop the previous-frame GEM reference).

/// Carries the bits of a `video_engine::vaapi::DrmPrimeFrame` that the
/// KMS path actually consumes. Decoupled from `video-engine` types so
/// the `display` module doesn't need to depend on `video-engine` for
/// non-VAAPI builds.
#[derive(Debug, Clone)]
pub struct DrmPrimeDescriptor {
    pub width: u32,
    pub height: u32,
    /// `DRM_FORMAT_*` fourcc (NV12 = 0x3231564E, P010 = 0x30313050, ...).
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_*` modifier. `0` (`DRM_FORMAT_MOD_LINEAR`) and
    /// `(1 << 56) - 1` (`DRM_FORMAT_MOD_INVALID`) are both treated as
    /// "no tiling info"; the importer drops the MODIFIERS flag in that
    /// case so the kernel doesn't reject the FB on a plane that hasn't
    /// advertised the modifier.
    pub modifier: u64,
    /// Per-plane DMA-BUF view. Currently every VAAPI mapping we've
    /// observed uses a single shared DMA-BUF — every plane references
    /// the same `fd` with different offsets — so we expect 1–3 entries
    /// pointing at the same fd. Multi-fd mappings are rejected at the
    /// `video-engine` boundary.
    pub planes: Vec<DrmPrimePlaneDesc>,
}

#[derive(Debug, Clone)]
pub struct DrmPrimePlaneDesc {
    pub fd: i32,
    pub offset: u32,
    pub pitch: u32,
}

/// Type-erased keepalive on the source VAAPI surface (typically an
/// `Arc<video_engine::vaapi::DrmPrimeKeepalive>`). KMS holds it across
/// the page-flip wait — dropping it earlier hands the surface back to
/// the VAAPI pool while the kernel is still scanning out from it,
/// which on Mesa radeonsi reads as a green flash and on Intel iHD as
/// garbage chroma. `Send + Sync` so the display task can move it
/// freely between the decode and present halves.
pub trait PrimeKeepalive: Send + Sync + 'static {}
impl<T: Send + Sync + 'static + ?Sized> PrimeKeepalive for T {}

/// `PlanarBuffer` adapter for `DrmModeAddFB2WithModifiers`. Wraps a set
/// of pre-imported GEM `buffer::Handle`s alongside the format /
/// modifier / size info `add_planar_framebuffer` needs. One per
/// in-flight DRM PRIME fb — short-lived; created in `present_prime`,
/// dropped after the FB is destroyed.
struct PrimePlanarBuffer {
    width: u32,
    height: u32,
    fourcc: DrmFourcc,
    modifier: Option<DrmModifier>,
    handles: [Option<drm::buffer::Handle>; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
}

impl PlanarBuffer for PrimePlanarBuffer {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        self.fourcc
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier
    }
    fn pitches(&self) -> [u32; 4] {
        self.pitches
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        self.handles
    }
    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
}

/// State retained across `present_prime` calls — the previous frame's
/// KMS framebuffer + its source-frame keepalive. Released only when the
/// *next* page-flip completes (the kernel keeps the previous FB live
/// until the new one has been fully scanned out at least once, and
/// VAAPI surfaces stay valid across exactly that boundary).
#[derive(Default)]
struct PrimeState {
    /// FB-id of the framebuffer the kernel is currently scanning out.
    /// We never destroy the in-flight FB; instead we destroy the
    /// **previous** FB after the new flip completes.
    in_flight_fb: Option<framebuffer::Handle>,
    /// Keepalive on the in-flight VAAPI surface.
    in_flight_keepalive: Option<Arc<dyn PrimeKeepalive>>,
}

/// `DRM_FORMAT_MOD_INVALID` — sentinel returned by VAAPI drivers when
/// the surface has no defined tiling layout. drm-rs already filters
/// this in `add_planar_framebuffer` but we double-up so we don't pass
/// `FbCmd2Flags::MODIFIERS` for it (some KMS planes reject the flag
/// when the modifier is INVALID).
const DRM_FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Outcome of an `atomic_commit` attempt. `Unsupported` triggers the
/// permanent fallback to legacy `set_crtc` with a one-shot
/// `display_atomic_unavailable` Warning. `Other` is a transient error
/// the caller propagates as a normal display-path failure (the FB is
/// torn down, the keepalive is held briefly, and the next frame
/// retries the same atomic path).
enum AtomicCommitError {
    Unsupported(std::io::Error),
    Other(String),
}

#[inline]
fn libc_einval() -> i32 {
    22
}
#[inline]
fn libc_enosys() -> i32 {
    38
}
#[inline]
fn libc_eopnotsupp() -> i32 {
    // Linux: 95 (EOPNOTSUPP == ENOTSUP). Same numeric value across
    // every kernel-supported arch we run on.
    95
}

#[inline]
fn io_to_string(e: &std::io::Error) -> String {
    format!("{e}")
}

impl KmsDisplay {
    /// Zero-copy page-flip onto a VAAPI-decoded surface exported as a
    /// DRM PRIME descriptor. Returns the previous-frame keepalive so
    /// the caller can drop it after the *next* flip — the kernel
    /// guarantees the previous scanout source stays referenced until
    /// the new one has been promoted, but our DMA-BUF / VAAPI pool is
    /// only safe to recycle after the kernel-side reference goes away.
    ///
    /// Returns `Err` when the FB-import path fails — caller demotes to
    /// the CPU-blit path. Common failures:
    ///
    /// * `prime_fd_to_buffer` returns `EINVAL` — DMA-BUF was created
    ///   on a driver our card can't import (rare on iGPU+iGPU; more
    ///   common on iGPU+dGPU mixed setups).
    /// * `add_planar_framebuffer` returns `EINVAL` — the requested
    ///   modifier isn't in the scanout plane's format list. Some Intel
    ///   gen12 4-tile modifiers fall here on older kernels.
    /// * `page_flip` returns `EBUSY` — a previous flip hasn't drained
    ///   yet (caller is over-driving the pipeline).
    pub fn present_prime(
        &mut self,
        descriptor: &DrmPrimeDescriptor,
        keepalive: Arc<dyn PrimeKeepalive>,
    ) -> Result<Option<Arc<dyn PrimeKeepalive>>> {
        if descriptor.planes.is_empty() {
            anyhow::bail!("display_prime_invalid: descriptor has no planes");
        }

        let fourcc = DrmFourcc::try_from(descriptor.fourcc).map_err(|_| {
            anyhow::anyhow!(
                "display_prime_invalid: unknown fourcc 0x{:08x}",
                descriptor.fourcc
            )
        })?;
        let modifier_raw = descriptor.modifier;
        let modifier = if modifier_raw == DRM_FORMAT_MOD_INVALID
            || modifier_raw == DRM_FORMAT_MOD_LINEAR
        {
            None
        } else {
            Some(DrmModifier::from(modifier_raw))
        };

        // Import every distinct DMA-BUF fd into a per-card GEM handle.
        // The VAAPI mapping packs every plane into one fd, but we
        // dedup defensively in case a future driver returns a multi-fd
        // descriptor (Intel iHD has been known to do this for P010
        // when the chroma plane lives on a separate object).
        let mut handles: [Option<drm::buffer::Handle>; 4] = [None; 4];
        let mut pitches: [u32; 4] = [0; 4];
        let mut offsets: [u32; 4] = [0; 4];

        for (i, plane) in descriptor.planes.iter().enumerate() {
            if i >= 4 {
                anyhow::bail!(
                    "display_prime_invalid: too many planes ({})",
                    descriptor.planes.len()
                );
            }
            // SAFETY: descriptor.planes[i].fd is owned by the
            // `keepalive` Arc; the borrowed FD here lives only across
            // this call to `prime_fd_to_buffer`, which dups the fd
            // internally on the kernel side via the PRIME ioctl.
            let borrowed = unsafe {
                std::os::fd::BorrowedFd::borrow_raw(plane.fd)
            };
            let handle = self
                .card
                .prime_fd_to_buffer(borrowed)
                .with_context(|| {
                    format!(
                        "display_prime_addfb_failed: prime_fd_to_buffer for plane {} (fd={})",
                        i, plane.fd
                    )
                })?;
            handles[i] = Some(handle);
            pitches[i] = plane.pitch;
            offsets[i] = plane.offset;
        }

        let buf = PrimePlanarBuffer {
            width: descriptor.width,
            height: descriptor.height,
            fourcc,
            modifier,
            handles,
            pitches,
            offsets,
        };

        let flags = if modifier.is_some() {
            FbCmd2Flags::MODIFIERS
        } else {
            FbCmd2Flags::empty()
        };
        let new_fb = self
            .card
            .add_planar_framebuffer(&buf, flags)
            .context("display_prime_addfb_failed: add_planar_framebuffer")?;

        // Promote the new prime FB onto the CRTC.
        //
        // Preferred path: `drmModeAtomicCommit` flips at vblank with
        // `DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK`. Cross-
        // modifier flips (linear XRGB8888 dumb buffer ↔ tiled NV12/P010,
        // or one tiled VAAPI surface ↔ the next) are accepted at vblank
        // with no full-plane reconfigure — microseconds, not the 10–
        // 30 ms `set_crtc` budget that pinned 1080p sources at ~10 fps
        // on a 60 Hz panel. The first commit on a fresh `KmsDisplay`
        // (or after a legacy `set_crtc` modeset) carries
        // `ALLOW_MODESET` plus the CRTC.ACTIVE / CRTC.MODE_ID /
        // CONNECTOR.CRTC_ID writes; subsequent commits skip those — the
        // kernel keeps the modeset state.
        //
        // Fallback path: legacy `set_crtc`. Used when:
        //   - DRM atomic client cap was rejected at `KmsDisplay::open`,
        //   - `discover_atomic_setup` couldn't find a primary plane
        //     drivable from our CRTC,
        //   - the very first `atomic_commit` returns EOPNOTSUPP (very
        //     old kernel) or EINVAL (driver / plane combo refuses our
        //     property set).
        // The first failure flips `use_atomic = false` for the lifetime
        // of this `KmsDisplay`. One `display_atomic_unavailable`
        // Warning event is emitted by the caller via
        // `take_atomic_fallback_reason()` so an operator sees why.
        if self.use_atomic {
            if self.atomic.is_none() {
                match discover_atomic_setup(&self.card, self.crtc, self.connector) {
                    Ok(setup) => self.atomic = Some(setup),
                    Err(e) => {
                        self.use_atomic = false;
                        self.atomic_fallback_reason
                            .get_or_insert_with(|| format!("atomic discovery failed: {e:#}"));
                    }
                }
            }
        }

        if self.use_atomic && self.atomic.is_some() {
            match self.atomic_present(new_fb, descriptor.width, descriptor.height) {
                Ok(()) => {
                    // Wait for the kernel-posted `DRM_EVENT_FLIP_COMPLETE`
                    // for our CRTC so the next iteration can safely
                    // destroy the previously-flipped FB. `receive_events`
                    // blocks until events arrive — same loop the legacy
                    // `present()` path uses.
                    if let Err(e) = self.wait_page_flip() {
                        let _ = self.card.destroy_framebuffer(new_fb);
                        return Err(e.context("display_prime_page_flip_failed: receive_events"));
                    }
                    // The bars-overlay back buffer just submitted has
                    // been flipped to — promote it to the new front so
                    // the next rasterise lands on the OTHER half of the
                    // pair (no scanout-vs-write race).
                    self.advance_bars_overlay_front();
                }
                Err(AtomicCommitError::Unsupported(e)) => {
                    self.use_atomic = false;
                    self.atomic_fallback_reason
                        .get_or_insert_with(|| format!("atomic_commit unsupported: {e}"));
                    if let Err(e2) = self.set_crtc_prime(new_fb) {
                        let _ = self.card.destroy_framebuffer(new_fb);
                        return Err(e2);
                    }
                }
                Err(AtomicCommitError::Other(e)) => {
                    let _ = self.card.destroy_framebuffer(new_fb);
                    return Err(anyhow::anyhow!(
                        "display_prime_page_flip_failed: atomic_commit: {e}"
                    ));
                }
            }
        } else {
            if let Err(e) = self.set_crtc_prime(new_fb) {
                let _ = self.card.destroy_framebuffer(new_fb);
                return Err(e);
            }
        }

        // The kernel has promoted `new_fb` to the active scanout
        // source. The *previous* FB (whatever was active before this
        // call) is now safe to destroy — the new flip has retired its
        // reference. We return its keepalive to the caller; the caller
        // drops it on a side task to keep the per-frame budget tight
        // (`av_frame_free` can take ~1 ms on the first call after a
        // session warmup).
        let mut state = self.prime_state.take().unwrap_or_default();
        let prev_keepalive = state.in_flight_keepalive.take();
        if let Some(prev_fb) = state.in_flight_fb.take() {
            let _ = self.card.destroy_framebuffer(prev_fb);
        }
        state.in_flight_fb = Some(new_fb);
        state.in_flight_keepalive = Some(keepalive);
        self.prime_state = Some(state);

        Ok(prev_keepalive)
    }

    /// Build + submit the atomic-commit page flip onto the cached
    /// primary plane. Sets the SRC rectangle in 16.16 fixed point (full
    /// source) and the CRTC rectangle to the current panel size. On the
    /// first commit (or first commit after a legacy modeset), allocates
    /// a property blob for `self.mode` and adds the modeset triplet
    /// (CRTC.ACTIVE / CRTC.MODE_ID / CONNECTOR.CRTC_ID) with
    /// `ALLOW_MODESET`.
    fn atomic_present(
        &mut self,
        new_fb: framebuffer::Handle,
        src_w: u32,
        src_h: u32,
    ) -> std::result::Result<(), AtomicCommitError> {
        // Copy out every Copy field we need from `self.atomic` up-
        // front. This keeps the rest of the function free to take
        // additional `&mut self.bars_overlay` / `&mut self.hdr_state`
        // borrows without colliding with a long-lived `&mut self.atomic`.
        let (plane, plane_props, crtc_props, connector_props, mode_blob_loaded, hdr_prop) = {
            let setup = self
                .atomic
                .as_ref()
                .expect("atomic_present called without AtomicSetup");
            (
                setup.plane,
                PlanePropIds {
                    fb_id: setup.plane_props.fb_id,
                    crtc_id: setup.plane_props.crtc_id,
                    src_x: setup.plane_props.src_x,
                    src_y: setup.plane_props.src_y,
                    src_w: setup.plane_props.src_w,
                    src_h: setup.plane_props.src_h,
                    crtc_x: setup.plane_props.crtc_x,
                    crtc_y: setup.plane_props.crtc_y,
                    crtc_w: setup.plane_props.crtc_w,
                    crtc_h: setup.plane_props.crtc_h,
                },
                CrtcPropIds {
                    mode_id: setup.crtc_props.mode_id,
                    active: setup.crtc_props.active,
                },
                ConnectorPropIds {
                    crtc_id: setup.connector_props.crtc_id,
                    hdr_output_metadata: setup.connector_props.hdr_output_metadata,
                },
                setup.mode_blob_id,
                setup.connector_props.hdr_output_metadata,
            )
        };
        let pp = &plane_props;
        let mut req = AtomicModeReq::new();
        req.add_property(plane, pp.fb_id, property::Value::Framebuffer(Some(new_fb)));
        req.add_property(plane, pp.crtc_id, property::Value::CRTC(Some(self.crtc)));
        // SRC rectangle: full source surface in 16.16 fixed-point.
        req.add_property(plane, pp.src_x, property::Value::UnsignedRange(0));
        req.add_property(plane, pp.src_y, property::Value::UnsignedRange(0));
        req.add_property(
            plane,
            pp.src_w,
            property::Value::UnsignedRange((src_w as u64) << 16),
        );
        req.add_property(
            plane,
            pp.src_h,
            property::Value::UnsignedRange((src_h as u64) << 16),
        );
        // CRTC rectangle: full destination panel.
        req.add_property(plane, pp.crtc_x, property::Value::SignedRange(0));
        req.add_property(plane, pp.crtc_y, property::Value::SignedRange(0));
        req.add_property(
            plane,
            pp.crtc_w,
            property::Value::UnsignedRange(self.width as u64),
        );
        req.add_property(
            plane,
            pp.crtc_h,
            property::Value::UnsignedRange(self.height as u64),
        );
        let _ = connector_props; // suppress unused warning; kept for symmetry with the other prop tables
        let _ = mode_blob_loaded;

        let mut flags = AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK;

        // HDR_OUTPUT_METADATA: write the connector's HDR signalling
        // blob when the source has transitioned in/out of HDR or
        // changed EOTF. The first commit carrying a metadata change
        // also takes ALLOW_MODESET because the HDMI / DP sink
        // re-trains on EOTF change.
        if self.hdr_dirty {
            if let Some(hdr_prop_id) = hdr_prop {
                let blob_id = self.hdr_state.as_ref().map(|s| s.blob_id).unwrap_or(0);
                req.add_property(self.connector, hdr_prop_id, property::Value::Blob(blob_id));
                flags |= AtomicCommitFlags::ALLOW_MODESET;
            }
        }

        // Audio-bars overlay plane composition. Same atomic commit, no
        // second flip event — the kernel posts one PageFlip event for
        // the whole batch. The strip lives at the bottom of the panel.
        // SRC rectangle covers the full overlay buffer (= panel width
        // × strip_h); CRTC rectangle places it `panel_h - strip_h`
        // pixels down the panel.
        if let Some(bars) = self.bars_overlay.as_mut() {
            let bp = &bars.plane_props;
            // Reference the BACK buffer — the one the rasteriser just
            // wrote into and that the kernel hasn't been scanning out.
            // `advance_bars_overlay_front` will promote it to front
            // after the flip event arrives.
            let back = bars.front_idx ^ 1;
            req.add_property(
                bars.plane,
                bp.fb_id,
                property::Value::Framebuffer(Some(bars.dumbs[back].fb)),
            );
            req.add_property(bars.plane, bp.crtc_id, property::Value::CRTC(Some(self.crtc)));
            req.add_property(bars.plane, bp.src_x, property::Value::UnsignedRange(0));
            req.add_property(bars.plane, bp.src_y, property::Value::UnsignedRange(0));
            req.add_property(
                bars.plane,
                bp.src_w,
                property::Value::UnsignedRange((bars.width as u64) << 16),
            );
            req.add_property(
                bars.plane,
                bp.src_h,
                property::Value::UnsignedRange((bars.height as u64) << 16),
            );
            req.add_property(bars.plane, bp.crtc_x, property::Value::SignedRange(0));
            let strip_top = self.height.saturating_sub(bars.height) as i64;
            req.add_property(
                bars.plane,
                bp.crtc_y,
                property::Value::SignedRange(strip_top),
            );
            req.add_property(
                bars.plane,
                bp.crtc_w,
                property::Value::UnsignedRange(bars.width as u64),
            );
            req.add_property(
                bars.plane,
                bp.crtc_h,
                property::Value::UnsignedRange(bars.height as u64),
            );
            // Program zpos so the bars plane composes ABOVE the primary
            // plane carrying the VAAPI prime FB. Without this, drivers
            // that default to registration-order zpos can place an
            // Overlay plane below Primary — the rasterised bars are
            // hidden by the video frame on top, even though every
            // atomic commit programmed the plane perfectly.
            if let Some(zpos_prop) = bars.zpos {
                req.add_property(
                    bars.plane,
                    zpos_prop,
                    property::Value::UnsignedRange(bars.zpos_value),
                );
            }
            // Program pixel blend mode = Coverage so the per-pixel alpha
            // lane drives the composite. Drivers that default to None
            // would scan the strip out as a fully-opaque box (whatever
            // the buffer holds, including the all-transparent regions
            // outside bars and header text → operator sees a black
            // rectangle); Coverage honours alpha=0 → primary shows
            // through, alpha=0xFF → bars/header opaque on top.
            if let Some(blend_prop) = bars.pixel_blend_mode {
                // KMS enum properties are wire-encoded as u64; the
                // typed `property::Value::Enum` variant carries an
                // `&EnumValue` reference whose lifetime we'd have to
                // re-fetch every commit. The kernel accepts the raw
                // value through `UnsignedRange` (which is what the
                // ioctl serialiser writes for both enum + range
                // properties — same on-the-wire encoding). This is
                // also what mesa / weston / hwcomposer-drm-android
                // use for the same purpose.
                req.add_property(
                    bars.plane,
                    blend_prop,
                    property::Value::UnsignedRange(bars.pixel_blend_coverage_value),
                );
            }
        }

        if !self.atomic_modeset_done {
            // First commit (or first after a legacy modeset): re-arm
            // the modeset triplet and ask the kernel for permission to
            // do a modeset under this commit.
            let blob_id = match mode_blob_loaded {
                Some(id) => id,
                None => {
                    let blob = self
                        .card
                        .create_property_blob(&self.mode)
                        .map_err(|e| AtomicCommitError::Other(io_to_string(&e)))?;
                    let id = match blob {
                        property::Value::Blob(id) => id,
                        _ => {
                            return Err(AtomicCommitError::Other(
                                "create_property_blob returned non-Blob value".into(),
                            ))
                        }
                    };
                    if let Some(setup) = self.atomic.as_mut() {
                        setup.mode_blob_id = Some(id);
                    }
                    id
                }
            };
            req.add_property(self.crtc, crtc_props.active, property::Value::Boolean(true));
            req.add_property(self.crtc, crtc_props.mode_id, property::Value::Blob(blob_id));
            req.add_property(
                self.connector,
                connector_props.crtc_id,
                property::Value::CRTC(Some(self.crtc)),
            );
            flags |= AtomicCommitFlags::ALLOW_MODESET;
        }

        match self.card.atomic_commit(flags, req) {
            Ok(()) => {
                self.atomic_modeset_done = true;
                self.hdr_dirty = false;
                if let Some(bars) = self.bars_overlay.as_mut() {
                    bars.committed = true;
                }
                Ok(())
            }
            Err(e) => {
                use std::io::ErrorKind;
                let raw = e.raw_os_error();
                let unsupported = matches!(e.kind(), ErrorKind::Unsupported)
                    || raw == Some(libc_eopnotsupp())
                    || raw == Some(libc_enosys())
                    // First-commit EINVAL: driver / plane combo refuses
                    // our property set. The plain set_crtc path may
                    // still work (different ioctl).
                    || (!self.atomic_modeset_done && raw == Some(libc_einval()));
                if unsupported {
                    Err(AtomicCommitError::Unsupported(e))
                } else {
                    Err(AtomicCommitError::Other(io_to_string(&e)))
                }
            }
        }
    }

    fn set_crtc_prime(&mut self, new_fb: framebuffer::Handle) -> Result<()> {
        self.card
            .set_crtc(
                self.crtc,
                Some(new_fb),
                (0, 0),
                &[self.connector],
                Some(self.mode),
            )
            .map_err(|e| anyhow::Error::new(e).context("display_prime_page_flip_failed: set_crtc"))
    }

    fn wait_page_flip(&self) -> Result<()> {
        loop {
            let events = self
                .card
                .receive_events()
                .context("display_page_flip_failed: receive_events")?;
            for ev in events {
                if let Event::PageFlip(p) = ev {
                    if p.crtc == self.crtc {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Flush any retained PRIME state — destroy the in-flight FB and
    /// drop the keepalive. Idempotent: returns immediately when no
    /// PRIME state is held, so the CPU-blit path can call this once
    /// per frame without paying for a re-modeset every vblank.
    ///
    /// Used on the runtime-demotion path: when sustained PRIME export
    /// failures push `output_display` from `vaapi-zerocopy` back to
    /// the CPU blit, the next blit calls this before writing the
    /// dumb-buffer fb so the scanout source is well-defined when
    /// `present()` flips onto the dumb-buffer.
    pub fn release_prime_state(&mut self) {
        let Some(mut state) = self.prime_state.take() else {
            return;
        };
        if state.in_flight_fb.is_none() && state.in_flight_keepalive.is_none() {
            // Nothing to do — the slot was empty.
            return;
        }
        if let Some(fb) = state.in_flight_fb.take() {
            let _ = self.card.destroy_framebuffer(fb);
        }
        state.in_flight_keepalive = None;
        // Re-arm the CRTC against the current dumb-buffer fb so the
        // scanout source is well-defined again. Best-effort: failure
        // here means the next `present()` will re-modeset anyway.
        let _ = self.card.set_crtc(
            self.crtc,
            Some(self.bufs[self.front_idx].fb),
            (0, 0),
            &[self.connector],
            Some(self.mode),
        );
    }
}

