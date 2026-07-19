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

use std::path::{Path, PathBuf};
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

impl CardFile {
    /// `dup(2)` the card fd into an independent `CardFile`. The clone
    /// refers to the SAME open file description (so it shares this card's
    /// DRM master status), but is a distinct fd we own outright — closing
    /// it does not disturb the original. Used to build a
    /// [`MasterReleaseHandle`] the supervisor can keep even after the live
    /// `KmsDisplay` is moved into a (possibly wedge-prone) render thread.
    fn try_clone(&self) -> std::io::Result<CardFile> {
        Ok(CardFile(self.0.try_clone()?))
    }
}

/// Independent handle that can relinquish a display's DRM master lease
/// without touching the (possibly wedged) render thread that owns the live
/// [`KmsDisplay`]. Built via [`KmsDisplay::master_release_handle`] from a
/// `dup(2)` of the card fd: the dup shares the same open file description,
/// so a `DROP_MASTER` ioctl issued through it releases the master held by
/// the original fd. The display orchestrator keeps one of these so that if
/// its blocking children fail to drain on teardown (e.g. ALSA `writei`
/// wedged on a yanked sink — `spawn_blocking` threads are detached from
/// `JoinHandle::abort`), it can still free the connector for the next
/// opener instead of leaking the master until the process exits.
pub struct MasterReleaseHandle {
    card: CardFile,
}

impl MasterReleaseHandle {
    /// Best-effort `DROP_MASTER`. Safe to call when we are not (or no
    /// longer) the master — the kernel returns `EINVAL`/`EACCES`, which we
    /// swallow. Only call once the live display is being torn down: a
    /// successful drop pulls master out from under any concurrent flip on
    /// the original fd, so it must not race a healthy render path.
    pub fn force_drop_master(&self) {
        match self.card.release_master_lock() {
            Ok(()) => tracing::debug!("display: force DROP_MASTER succeeded"),
            Err(e) => tracing::debug!("display: force DROP_MASTER best-effort failed: {e}"),
        }
    }
}

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

/// Best-effort resolution of the ALSA `hw:<card>,<device>` string that
/// carries a given KMS connector's HDMI / DisplayPort audio.
///
/// **The ALSA sound-card index is a different namespace from the
/// DRM/KMS card index.** On many hosts the GPU enumerates as DRM
/// `card1` (with `card0` empty / a non-display DRI node) while its HDMI
/// audio is exposed on ALSA `card0` — the single HDA controller. The
/// previous implementation derived the card digit from
/// `/sys/class/drm/cardN-<connector>` and reused `N` directly as the
/// ALSA card index, so a host whose HDMI audio actually lives on
/// `hw:0,3` was advertised as `hw:1,3` — `snd_pcm_open` then failed
/// with `ENODEV` and the display output was silently video-only.
///
/// We now resolve the ALSA card by locating the HDA card that actually
/// exposes HDMI PCM playback devices (preferring one with a monitor
/// currently plugged in), independent of DRM numbering. The PCM
/// *device* index is still the positional `infer_alsa_pcm_for`
/// heuristic — pinning a specific connector to a specific HDMI
/// converter requires parsing the HDA codec topology
/// (`/proc/asound/card*/codec#*`), which is deferred; operators
/// override the full `hw:<card>,<device>` via the output's
/// `audio_device` field when the heuristic misses. Returns `None` only
/// when no HDMI-capable ALSA card is found at all (the operator can
/// still pick `default` or a custom device name).
fn best_alsa_device_for_connector(connector_name: &str) -> Option<String> {
    let card_idx = alsa_hdmi_card_index()?;
    Some(format!("hw:{},{}", card_idx, infer_alsa_pcm_for(connector_name)))
}

/// Locate the ALSA card index that backs HDMI / DisplayPort audio.
/// Walks `/proc/asound/card*/` for cards that expose an HDMI PCM
/// playback device; when several exist (e.g. an Intel PCH HDA plus a
/// discrete-GPU HDA), prefers the lowest-indexed card that currently
/// reports a plugged-in sink (`eld#* monitor_present 1`). Falls back to
/// the lowest HDMI-capable card index. `None` when none is found.
fn alsa_hdmi_card_index() -> Option<u32> {
    let mut hdmi_cards: Vec<u32> = Vec::new();
    let mut card_with_monitor: Option<u32> = None;

    for entry in std::fs::read_dir("/proc/asound").ok()?.flatten() {
        let fname = entry.file_name();
        let name = fname.to_string_lossy();
        // `card0`, `card1`, … — skip the `cards` summary file and any
        // non-card entries.
        let Some(idx_str) = name.strip_prefix("card") else {
            continue;
        };
        let Ok(card_idx) = idx_str.parse::<u32>() else {
            continue;
        };
        let card_dir = entry.path();
        if !card_has_hdmi_pcm(&card_dir) {
            continue;
        }
        hdmi_cards.push(card_idx);
        if card_has_present_monitor(&card_dir) {
            card_with_monitor = Some(card_with_monitor.map_or(card_idx, |c| c.min(card_idx)));
        }
    }

    hdmi_cards.sort_unstable();
    card_with_monitor.or_else(|| hdmi_cards.first().copied())
}

/// True when the ALSA card directory exposes at least one HDMI / DP PCM
/// *playback* device — detected via `pcm<N>p/info` whose `id:` line
/// names "HDMI" or "DP" (excludes the analog `ALC###` playback device).
fn card_has_hdmi_pcm(card_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(card_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let n = entry.file_name();
        let n = n.to_string_lossy();
        // Playback PCM device dirs are `pcm3p`, `pcm7p`, … (trailing `p`);
        // capture (`pcm0c`) and other nodes are skipped.
        if !(n.starts_with("pcm") && n.ends_with('p')) {
            continue;
        }
        let info = card_dir.join(&*n).join("info");
        if let Ok(text) = std::fs::read_to_string(&info) {
            if text.lines().any(|l| {
                let l = l.trim();
                l.starts_with("id:") && (l.contains("HDMI") || l.contains("DP"))
            }) {
                return true;
            }
        }
    }
    false
}

/// True when any `eld#*` node under the card reports `monitor_present
/// 1` — i.e. a sink is currently plugged into one of its HDMI/DP ports.
fn card_has_present_monitor(card_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(card_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let n = entry.file_name();
        if !n.to_string_lossy().starts_with("eld#") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            // ELD lines are whitespace-separated: `monitor_present\t\t1`.
            if text.lines().any(|l| {
                let mut it = l.split_whitespace();
                it.next() == Some("monitor_present") && it.next() == Some("1")
            }) {
                return true;
            }
        }
    }
    false
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
    /// `COLOR_ENCODING` / `COLOR_RANGE` — optional on most Intel/AMD
    /// scanout planes (VAAPI's zero-copy path has shipped without ever
    /// setting them), but some embedded VOP/VOP2-class drivers
    /// (confirmed: RK3588 Rockchip VOP2 Cluster window) reject a YUV
    /// PRIME framebuffer commit with EINVAL unless these are set
    /// explicitly — RGB commits work fine without them, which is why
    /// this was never needed before RKMPP's native NV12 zero-copy path.
    /// `None` when the property doesn't exist on this plane (older
    /// kernels / drivers that never require it); the resolved `*_value`
    /// is the numeric enum ID for "ITU-R BT.709 YCbCr" /
    /// "YCbCr limited range" looked up once at discovery time.
    color_encoding: Option<property::Handle>,
    color_encoding_value: u64,
    color_range: Option<property::Handle>,
    color_range_value: u64,
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
    /// `Some` only when the property exists AND is **mutable** — the
    /// next atomic commit then writes `primary_zpos + 1` so the bars
    /// plane composes **above** the primary (the FB carrying the
    /// video); without it, several drivers (notably some AMD `amdgpu`
    /// configurations) place an Overlay plane **under** the Primary in
    /// registration order, hiding the strip behind the video frame.
    /// `None` when the driver doesn't expose `zpos` (kernel uses
    /// registration order) or when it's **immutable** with a pinned
    /// value already above the primary (i915 pins every plane's zpos
    /// to `[v, v]`; writing an immutable property — even with its
    /// current value — EINVALs the whole commit, which is the bug that
    /// froze bars-enabled displays on Intel hosts, 2026-06-11).
    /// Immutable-and-below-primary planes are rejected at discovery.
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

/// Secondary content plane for zero-copy YUV PRIME scanout, adopted
/// reactively when the primary plane permanently rejects a linear YUV
/// framebuffer commit — confirmed on real RK3588 hardware: the VOP2
/// "Cluster" window class (frequently the plane the kernel reports as
/// `type=Primary`) accepts YUV **only** in AFBC-compressed form per the
/// upstream kernel driver's own capability description ("Cluster
/// windows: AFBC/line RGB and AFBC-only YUV support"), while "Esmart"
/// windows support linear YUV directly. See
/// [`KmsDisplay::try_promote_yuv_overlay`].
///
/// Unlike [`BarsOverlay`], this plane carries the actual video content
/// (not a translucent overlay), so it needs `COLOR_ENCODING`/
/// `COLOR_RANGE` resolved exactly like the primary plane's YUV path —
/// and it has no ping-pong buffer state of its own, since every PRIME
/// framebuffer is freshly imported per frame via `add_planar_framebuffer`
/// (same as the primary-plane PRIME path).
struct YuvOverlay {
    plane: plane::Handle,
    plane_props: PlanePropIds,
    /// `Some` only when `zpos` exists and is mutable — writes
    /// `zpos_value` so this plane composes ABOVE the primary plane's
    /// stale dumb-buffer content underneath it. `None` when the driver
    /// pins zpos (the pinned value already being above primary is
    /// assumed rather than re-verified, mirroring
    /// [`BarsOverlay::zpos`]'s immutable-and-compatible case).
    zpos: Option<property::Handle>,
    zpos_value: u64,
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
    /// Imported-framebuffer cache keyed by [`dmabuf_identity`] — see
    /// [`PrimeFbCacheEntry`]. Avoids re-running `prime_fd_to_buffer` +
    /// `add_planar_framebuffer` for a decode buffer this session has
    /// already imported (RKMPP recycles a fixed ≤16-buffer pool;
    /// confirmed on RK3588 that skipping the redundant re-import here
    /// eliminates the dominant per-frame cost on the zero-copy PRIME
    /// path — average `present_prime` time dropped from double-digit
    /// milliseconds to sub-millisecond).
    prime_fb_cache: std::collections::HashMap<(u64, u64), PrimeFbCacheEntry>,
    /// Insertion order for [`Self::prime_fb_cache`]'s FIFO eviction —
    /// see [`PRIME_FB_CACHE_CAPACITY`].
    prime_fb_cache_order: std::collections::VecDeque<(u64, u64)>,
    /// Atomic-commit page-flip state. `None` until the first PRIME
    /// present, which discovers the primary plane + caches property
    /// IDs. `use_atomic` flips to `false` (and stays there) on
    /// EOPNOTSUPP or a first-commit EINVAL — see `present_prime`.
    use_atomic: bool,
    atomic: Option<AtomicSetup>,
    /// Has ANY atomic commit succeeded this session? Discriminates a
    /// genuine "driver refuses our atomic property set" first-commit
    /// EINVAL (→ permanent legacy fallback) from a transient EINVAL on
    /// a re-modeset commit after `invalidate_atomic_modeset()` — the
    /// pre-fix classifier treated both as Unsupported, so one flaky
    /// commit at an auto-match seam (observed live on a 4K Take,
    /// 2026-06-11) permanently killed atomic AND with it the bars
    /// overlay composition, while every health flag still read green.
    atomic_ever_succeeded: bool,
    /// Consecutive `Other`-class atomic failures since the last
    /// success. A persistent post-modeset rejection escapes to the
    /// legacy `set_crtc` path (with the bars overlay torn down so the
    /// CPU-bake fallback engages) after
    /// `ATOMIC_CONSECUTIVE_FAILURE_LIMIT` instead of erroring every
    /// frame forever.
    atomic_consecutive_failures: u32,
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
    /// The operator asked for the bars overlay this session (a
    /// successful or attempted `enable_bars_overlay()`). Drives the
    /// self-heal: when `true` and `bars_overlay` is `None`,
    /// `maybe_reheal_bars_overlay` periodically re-attempts the enable
    /// instead of leaving the composition path degraded for the rest
    /// of the task's lifetime ("bars never come back until restart").
    bars_overlay_wanted: bool,
    /// When the overlay was last torn down (commit-failure arm, panel
    /// resize bail, or a failed enable). Re-enable attempts are
    /// cooldown-gated on this so a host that persistently rejects the
    /// bars plane retries once per `BARS_REHEAL_COOLDOWN`, not per
    /// frame.
    bars_overlay_lost_at: Option<std::time::Instant>,
    /// Secondary NV12-capable plane, adopted reactively after the
    /// primary plane permanently rejects a linear YUV PRIME commit.
    /// `None` until (if ever) [`Self::try_promote_yuv_overlay`]
    /// succeeds; once `Some`, every subsequent YUV `atomic_present`
    /// call targets this plane instead of the primary. See
    /// [`YuvOverlay`]'s doc comment for the RK3588 Cluster/Esmart
    /// background.
    yuv_overlay: Option<YuvOverlay>,
    /// Whether [`Self::try_promote_yuv_overlay`] has already been
    /// attempted this session — attempted at most once, on the first
    /// permanent primary-plane YUV rejection, regardless of outcome.
    /// Prevents re-running plane discovery on every subsequent frame
    /// when no suitable secondary plane exists (the VAAPI/Intel/AMD
    /// case, where the primary plane's rejection means something else
    /// entirely and promotion would never succeed).
    yuv_overlay_promotion_attempted: bool,
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

/// Upper bound on how long [`KmsDisplay::wait_page_flip`] waits for a
/// queued page flip's `DRM_EVENT_FLIP_COMPLETE` before declaring the
/// GPU/driver wedged and returning a distinct `display_flip_timeout`
/// error. The DRM fd is opened blocking (no `O_NONBLOCK`), so without a
/// bound a driver that has stopped posting vblank-completion events
/// (e.g. broken `nvidia-drm` flip IRQs — dmesg "Flip event timeout on
/// head 0") would hang the display thread forever with no event. 500 ms
/// is ~12 vblanks at 24 Hz / ~30 at 60 Hz — generous against any real
/// broadcast panel (so it never false-fires on a healthy-but-busy host)
/// while still bounding a true hang to sub-second, keeping the display
/// loop responsive to its cancel token.
const FLIP_EVENT_TIMEOUT_MS: u64 = 500;

impl KmsDisplay {
    /// Duplicate the card fd into a standalone [`MasterReleaseHandle`] so a
    /// supervisor can drop this display's DRM master even when the thread
    /// owning the live `KmsDisplay` is wedged in a blocking syscall and
    /// never drops it itself. Returns `None` if `dup(2)` fails.
    pub fn master_release_handle(&self) -> Option<MasterReleaseHandle> {
        match self.card.try_clone() {
            Ok(card) => Some(MasterReleaseHandle { card }),
            Err(e) => {
                tracing::debug!(
                    "display: could not dup card fd for master-release handle: {e}"
                );
                None
            }
        }
    }

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
        // a regular user gets it via `seat0`/logind automatically. Don't
        // silently discard the result: when a desktop compositor / display
        // manager already owns the device this fails, the modeset below then
        // returns EACCES, and we turn that into a clear `display_master_busy`
        // (see `set_crtc` handling below) instead of the misleading
        // "rejected the mode".
        if let Err(e) = card.acquire_master_lock() {
            tracing::debug!(
                "display: could not explicitly acquire DRM master on '{}' ({e}); \
                 relying on logind/seat auto-grant — a compositor-held master \
                 surfaces as EACCES at modeset",
                connector_name
            );
        }
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
        if let Err(e) = card.set_crtc(crtc, Some(a.fb), (0, 0), &[connector], Some(mode)) {
            // EACCES / EPERM here is the signature of another DRM master
            // (a desktop compositor / display manager — GDM, gnome-shell,
            // an X server) already owning the connector: the kernel refuses
            // a modeset from a non-master. Surface a distinct, actionable
            // `display_master_busy` so the operator stops the compositor or
            // runs headless, rather than the misleading "rejected the mode"
            // (which reads like a bad resolution). A genuinely unsupported
            // mode fails with EINVAL / ENOSPC and keeps `display_mode_set_failed`.
            let denied = matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM));
            return Err(anyhow::Error::new(e).context(if denied {
                format!(
                    "display_master_busy: another DRM master (a desktop compositor / \
                     display manager such as GDM) already owns connector '{}' — stop it \
                     (e.g. `sudo systemctl stop gdm`) or run the edge on a headless host; \
                     see docs/installation.md (Local-display output)",
                    connector_name
                )
            } else {
                "display_mode_set_failed: drmModeSetCrtc rejected the mode".to_string()
            }));
        }

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
            prime_fb_cache: std::collections::HashMap::new(),
            prime_fb_cache_order: std::collections::VecDeque::new(),
            use_atomic,
            atomic: atomic_setup,
            atomic_ever_succeeded: false,
            atomic_consecutive_failures: 0,
            atomic_modeset_done: false,
            atomic_fallback_reason: None,
            bars_overlay: None,
            bars_overlay_wanted: false,
            bars_overlay_lost_at: None,
            yuv_overlay: None,
            yuv_overlay_promotion_attempted: false,
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
        self.bars_overlay_wanted = true;
        let result = self.enable_bars_overlay_inner();
        match &result {
            Ok(()) => self.bars_overlay_lost_at = None,
            // Arm the reheal cooldown so `maybe_reheal_bars_overlay`
            // keeps retrying on hosts where the failure is transient
            // (EDID re-probe flake, plane briefly claimed elsewhere).
            Err(_) => self.bars_overlay_lost_at = Some(std::time::Instant::now()),
        }
        result
    }

    fn enable_bars_overlay_inner(&mut self) -> Result<()> {
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

        let (plane_h, plane_props, plane_type_name) =
            discover_bars_overlay_plane(&self.card, self.crtc, primary_plane)
                .context("bars-overlay plane discovery")?;
        let (zpos, zpos_value, pixel_blend_mode, pixel_blend_coverage_value) =
            discover_bars_overlay_blend_props(&self.card, plane_h, primary_plane)
                .context("bars-overlay zpos/blend property discovery")?;
        let dumbs = alloc_bars_dumb_pair(&self.card, self.width, strip_h)?;
        // One-shot diagnostic so operators debugging "bars don't show on
        // the panel" can tell whether the failure is in plane selection
        // (wrong type, missing zpos, missing pixel-blend-mode) versus
        // composition. Logged at info to make it visible in default
        // production logging without bumping a noisy debug filter.
        let primary_zpos_raw = read_property_u64(&self.card, primary_plane, "zpos");
        tracing::info!(
            plane_type = plane_type_name,
            bars_plane = format!("{:?}", plane_h),
            primary_plane = format!("{:?}", primary_plane),
            primary_zpos = ?primary_zpos_raw,
            // `false` is normal on i915: zpos there is immutable and
            // already pinned above the primary, so no write is needed
            // (and writing would EINVAL every commit).
            zpos_write_armed = zpos.is_some(),
            bars_zpos_value = zpos_value,
            blend_write_armed = pixel_blend_mode.is_some(),
            blend_coverage_value = pixel_blend_coverage_value,
            strip_w = self.width,
            strip_h,
            panel_w = self.width,
            panel_h = self.height,
            "audio-bars overlay enabled — plane discovery + property state"
        );
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

    /// Attempt to adopt a secondary Overlay-class plane advertising
    /// native NV12 support for the zero-copy YUV PRIME content path,
    /// in place of the primary plane. Called reactively, at most once
    /// per session, by `present_prime` the first time the primary
    /// plane's atomic commit permanently rejects a linear YUV
    /// framebuffer — see [`YuvOverlay`]'s doc comment for why this is
    /// needed on RK3588 (Cluster windows: AFBC-only YUV) and why it's
    /// safe to leave unused on hosts where the primary plane already
    /// accepts YUV directly (VAAPI on Intel/AMD, proven over many
    /// prior releases — this path is never even attempted there,
    /// since it only runs after a primary-plane failure that those
    /// hosts don't produce).
    ///
    /// On success, every subsequent `atomic_present(..., is_yuv=true)`
    /// call targets the adopted plane automatically (see its
    /// content-plane selection logic) — no further calls to this
    /// method are needed. On failure (no suitable plane — e.g. VAAPI/
    /// Intel/AMD hosts, where the primary plane's rejection means
    /// something else entirely), the caller falls through to the
    /// existing sysmem CPU-blit demotion exactly as before this
    /// method existed.
    fn try_promote_yuv_overlay(&mut self) -> Result<()> {
        if self.yuv_overlay.is_some() {
            return Ok(());
        }
        if !self.use_atomic {
            anyhow::bail!("yuv-overlay promotion needs atomic_commit");
        }
        let primary_plane = self
            .atomic
            .as_ref()
            .map(|a| a.plane)
            .context("yuv-overlay promotion requires atomic setup to already exist")?;
        let mut exclude = vec![primary_plane];
        if let Some(bars) = &self.bars_overlay {
            exclude.push(bars.plane);
        }
        let (plane_h, plane_props) = discover_yuv_overlay_plane(&self.card, self.crtc, &exclude)
            .context("yuv-overlay plane discovery")?;
        let (zpos, zpos_value) = discover_yuv_overlay_zpos(&self.card, plane_h, primary_plane)
            .context("yuv-overlay zpos discovery")?;
        tracing::info!(
            plane = format!("{:?}", plane_h),
            primary_plane = format!("{:?}", primary_plane),
            zpos_write_armed = zpos.is_some(),
            zpos_value,
            "display: promoted zero-copy YUV scanout to a secondary NV12-capable plane — \
             the primary plane rejected linear YUV (expected on Rockchip VOP2 boards whose \
             Cluster-class primary window is AFBC-only for YUV)"
        );
        self.yuv_overlay = Some(YuvOverlay {
            plane: plane_h,
            plane_props,
            zpos,
            zpos_value,
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
        // Every internal caller is a failure / degradation path (commit
        // rejection, panel shrank below the strip minimum) — arm the
        // self-heal so the overlay isn't gone for the rest of the
        // session. An operator runtime-toggle path that wants the
        // overlay to STAY off must clear `bars_overlay_wanted` too.
        self.bars_overlay_lost_at = Some(std::time::Instant::now());
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

    /// Re-attempt `enable_bars_overlay` after an earlier teardown.
    /// Returns `true` when the overlay is live on return (already
    /// enabled, or the re-enable just succeeded).
    ///
    /// Cheap no-op guards make this safe to call per frame: bars never
    /// requested → false; overlay already live → true; cooldown since
    /// the teardown not elapsed (`force = false`) → false. Pass
    /// `force = true` after a mode change — the constraint environment
    /// that rejected the plane has been rebuilt, so an immediate
    /// attempt is justified.
    ///
    /// Without this, any transient atomic-commit rejection that hit the
    /// disable-and-retry arm removed the bars strip for the remainder
    /// of the display task's lifetime — the operator-reported "bars
    /// never come back until reboot" (2026-06-11).
    pub fn maybe_reheal_bars_overlay(&mut self, force: bool) -> bool {
        if self.bars_overlay.is_some() {
            return true;
        }
        if !self.bars_overlay_wanted {
            return false;
        }
        if !force {
            let due = self
                .bars_overlay_lost_at
                .map(|t| t.elapsed() >= BARS_REHEAL_COOLDOWN)
                .unwrap_or(true);
            if !due {
                return false;
            }
        }
        match self.enable_bars_overlay() {
            Ok(()) => {
                tracing::info!(
                    "audio-bars overlay re-enabled after earlier teardown — \
                     hardware composition restored"
                );
                true
            }
            Err(e) => {
                tracing::debug!("audio-bars overlay re-enable attempt failed: {e:#}");
                false
            }
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
            // A mode change rebuilt the constraint environment that may
            // have rejected the bars plane earlier — re-attempt the
            // enable immediately (cooldown bypassed). No-op when bars
            // were never requested.
            self.maybe_reheal_bars_overlay(true);
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
        // The legacy SETCRTC can collide with a just-submitted
        // nonblocking atomic flip (EBUSY window is one vblank). One
        // short-delay retry absorbs that race; a real driver rejection
        // fails both attempts and propagates.
        let mut modeset = self.card.set_crtc(
            self.crtc,
            Some(new_a.fb),
            (0, 0),
            &[self.connector],
            Some(new_mode),
        );
        if let Err(first) = &modeset {
            tracing::warn!(
                "auto-match modeset to {}x{}@{} failed ({first}); retrying once after 50 ms",
                new_w,
                new_h,
                new_mode.vrefresh(),
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
            modeset = self.card.set_crtc(
                self.crtc,
                Some(new_a.fb),
                (0, 0),
                &[self.connector],
                Some(new_mode),
            );
        }
        if let Err(e) = modeset {
            // Free the buffers we allocated for the mode we failed to
            // reach — the current mode keeps scanning out of the
            // existing pair.
            for buf in [new_a, new_b] {
                let DumbBuffer { mapping, fb, handle } = buf;
                drop(mapping);
                let _ = self.card.destroy_framebuffer(fb);
                let _ = self.card.destroy_dumb_buffer(handle);
            }
            return Err(anyhow::Error::new(e)
                .context("display_mode_set_failed: auto-match re-modeset"));
        }
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
        self.rearm_atomic_after_mode_change();
        tracing::info!(
            "display auto-match modeset: panel now {}x{}@{}Hz",
            new_w,
            new_h,
            new_mode.vrefresh(),
        );
        if let Err(e) = self.resync_bars_overlay_to_panel() {
            tracing::warn!(
                "audio-bars overlay resync after match-source modeset failed: {e:#}"
            );
        }
        Ok(())
    }

    /// A mode change rebuilt the scanout environment — if atomic
    /// commits had been working earlier this session but escaped to the
    /// legacy fallback (persistent rejection in the PREVIOUS mode, e.g.
    /// a 4K P010 modeset commit the driver refused), give atomic
    /// another go in the new mode. Bounded: a still-broken atomic path
    /// re-escapes after `ATOMIC_CONSECUTIVE_FAILURE_LIMIT` frames.
    /// No-op on hosts where atomic never worked (genuine first-commit
    /// refusal — `atomic_ever_succeeded` is false there).
    fn rearm_atomic_after_mode_change(&mut self) {
        if !self.use_atomic && self.atomic_ever_succeeded {
            tracing::info!(
                "re-arming atomic commits after mode change (legacy fallback had engaged)"
            );
            self.use_atomic = true;
            self.atomic_consecutive_failures = 0;
        }
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
        self.rearm_atomic_after_mode_change();
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
        match self.atomic_present(new_fb, width, height, false) {
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
            Err(AtomicCommitError::Busy(e)) => {
                // Transient: a previous nonblocking commit (or a legacy
                // modeset from the auto-match path) is still in flight.
                // Drop this frame and let the next one — a frame period
                // later, long past the pending vblank — retry. Never
                // disable the bars overlay for EBUSY: the overlay can't
                // cause a busy pipeline, and tearing it down on a
                // one-frame collision permanently downgrades the
                // composition path. Persistent EBUSY stays loud via the
                // display loop's throttled blit-failure warn.
                Err(anyhow::anyhow!(
                    "display_page_flip_failed: atomic_commit busy (frame dropped): {e}"
                ))
            }
            Err(AtomicCommitError::Other { msg, bars_included }) => {
                // Mirror `present_prime`'s recovery: when the bars
                // overlay plane is part of the failing property set,
                // drop the overlay and retry the flip without it rather
                // than hard-failing every frame. On the CPU-blit path
                // this costs NOTHING visually — `blit_and_present`
                // bakes the bars + header into the primary dumb buffer
                // as a backstop on every frame, so the strip survives
                // on the primary plane; only the (redundant here)
                // hardware composition is lost. Without this arm, a
                // driver that rejects any bars-plane property froze the
                // panel on the last presented frame while audio kept
                // playing (i915 immutable-zpos, 2026-06-11). Gated on
                // `bars_included` — a failing bars-LESS commit (e.g.
                // the post-modeset re-arm) says nothing about the bars
                // plane and must not tear it down.
                if bars_included && self.bars_overlay.is_some() {
                    tracing::warn!(
                        "atomic_commit failed with bars overlay included on CPU-blit flip ({msg}) — \
                         disabling overlay and retrying without bars (strip stays via the \
                         dumb-buffer bake)"
                    );
                    self.disable_bars_overlay();
                    self.invalidate_atomic_modeset();
                    match self.atomic_present(new_fb, width, height, false) {
                        Ok(()) => {
                            self.wait_page_flip()
                                .context("display_page_flip_failed: receive_events (bars-retry)")?;
                            self.front_idx = back_idx;
                            Ok(())
                        }
                        Err(retry_err) => Err(anyhow::anyhow!(
                            "display_page_flip_failed: atomic_commit \
                             (also failed without bars): {retry_err:?}"
                        )),
                    }
                } else {
                    Err(anyhow::anyhow!(
                        "display_page_flip_failed: atomic_commit: {msg}"
                    ))
                }
            }
        }
    }

    /// Queue a vblank-synchronous page flip and block until the kernel
    /// posts `DRM_EVENT_FLIP_COMPLETE` for our CRTC. This is the proper
    /// per-frame primitive: the kernel atomically swaps the scanout source
    /// at the next vblank without reprogramming the CRTC. Cost is
    /// microseconds, not the 10–30 ms of `drmModeSetCrtc`.
    ///
    /// Completion is awaited via [`Self::wait_page_flip`], which polls the
    /// (blocking-mode) DRM fd with a [`FLIP_EVENT_TIMEOUT_MS`] bound so a
    /// driver that has stopped posting flip-completion events can't hang
    /// the display thread forever — it returns `display_flip_timeout`
    /// instead.
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
        self.wait_page_flip()?;
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
/// `true` when `refresh_hz` is a clean integer multiple of the source
/// `fps` — i.e. the panel can present the content judder-free by
/// repeating each frame a whole number of times.
///
/// The tolerance is expressed in Hz against the nearest multiple, not
/// as a ratio error: a ratio tolerance wide enough to absorb the EMA
/// fps hint's estimation error (±0.12 at rank-2 multiples) wrongly
/// classified 50 Hz as a multiple of 24 / 23.976 fps (50/24 = 2.083)
/// and steered film content onto the judderier cadence. ±0.5 Hz
/// absorbs both the hint error after 40 frames (< 0.25 Hz) and the
/// kernel's integer `vrefresh` rounding of fractional NTSC rates
/// (59.94 → 60), while rejecting 50-vs-48 (2 Hz off) cleanly.
fn refresh_is_cadence_multiple(refresh_hz: u32, fps: f32) -> bool {
    let rounded = (refresh_hz as f32 / fps).round();
    rounded >= 1.0 && (refresh_hz as f32 - rounded * fps).abs() < 0.5
}

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
        match src_fps_hint {
            Some(fps) if fps > 0.0 && !refresh_is_cadence_multiple(refresh_hz, fps) => 1,
            _ => 0,
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

/// Minimum interval between bars-overlay re-enable attempts after a
/// teardown (`maybe_reheal_bars_overlay` with `force = false`). Long
/// enough that a host which persistently rejects the bars plane pays
/// one failed plane-discovery + commit per cooldown rather than per
/// frame; short enough that a transient rejection heals without
/// operator action.
const BARS_REHEAL_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// Consecutive `Other`-class atomic-commit failures tolerated before
/// the display escapes to the legacy `set_crtc` path. ~2 s at 50 fps —
/// long enough to ride out a transient post-modeset rejection window,
/// short enough that a genuinely incompatible driver doesn't black the
/// panel for long.
const ATOMIC_CONSECUTIVE_FAILURE_LIMIT: u32 = 100;

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

/// `(handle, mutable, current_value)` for a named property, or `None`
/// when the object doesn't expose it. The mutability bit matters:
/// writing an **immutable** property in an atomic commit fails the
/// whole commit with EINVAL — even when the written value equals the
/// current one (the DRM core rejects the property *write*, not the
/// value). i915 pins every plane's `zpos` this way (range `[v, v]`,
/// immutable), which is exactly the case the bars-overlay discovery
/// has to detect rather than blindly writing `zpos` per commit.
fn optional_prop_state(
    card: &CardFile,
    handle: impl drm::control::ResourceHandle,
    name: &str,
) -> Result<Option<(property::Handle, bool, u64)>> {
    let set = card
        .get_properties(handle)
        .context("get_properties for prop-state discovery")?;
    let (ids, vals) = set.as_props_and_values();
    for (id, val) in ids.iter().zip(vals.iter()) {
        let info = card
            .get_property(*id)
            .context("get_property for prop-state discovery")?;
        if info.name().to_bytes() == name.as_bytes() {
            return Ok(Some((*id, info.mutable(), *val)));
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
    let color_encoding = optional_prop(card, plane_h, "COLOR_ENCODING")?;
    let color_encoding_value =
        read_enum_value(card, plane_h, "COLOR_ENCODING", "ITU-R BT.709 YCbCr").unwrap_or(0);
    let color_range = optional_prop(card, plane_h, "COLOR_RANGE")?;
    let color_range_value =
        read_enum_value(card, plane_h, "COLOR_RANGE", "YCbCr limited range").unwrap_or(0);
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
        color_encoding,
        color_encoding_value,
        color_range,
        color_range_value,
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
) -> Result<(plane::Handle, PlanePropIds, &'static str)> {
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
    let (plane_h, plane_type_name) = match (overlay, cursor) {
        (Some(p), _) => (p, "Overlay"),
        (None, Some(p)) => (p, "Cursor"),
        (None, None) => anyhow::bail!(
            "no Overlay or Cursor plane drivable from this CRTC supports ARGB8888 — host driver lacks multi-plane composition"
        ),
    };
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
        // The bars overlay is always ARGB8888 — never YUV — so these
        // are never read (see the `is_yuv` gate in `atomic_present`).
        color_encoding: None,
        color_encoding_value: 0,
        color_range: None,
        color_range_value: 0,
    };
    Ok((plane_h, plane_props, plane_type_name))
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
    let primary_zpos = read_property_u64(card, primary_plane, "zpos").unwrap_or(0);
    let zpos_value = primary_zpos.saturating_add(1);
    // Write `zpos` only when the property is MUTABLE. An immutable
    // `zpos` (i915 pins every plane to a fixed normalized value with
    // range `[v, v]`) EINVALs the whole atomic commit on any write —
    // including a write of the current value — which froze the display
    // on every bars-bearing commit (2026-06-11, ms02 Meteor Lake).
    // When immutable, the fixed value decides for us:
    //   - above the primary → composition is already bars-over-video;
    //     simply don't write the property.
    //   - at/below the primary → the plane would composite UNDER the
    //     video and the strip could never show. Bail so
    //     `enable_bars_overlay` reports the overlay unavailable and
    //     the caller falls back to the CPU-blit bake path.
    let zpos = match optional_prop_state(card, bars_plane, "zpos")? {
        None => None,
        Some((handle, true, _current)) => Some(handle),
        Some((_handle, false, current)) => {
            if current > primary_zpos {
                None
            } else {
                anyhow::bail!(
                    "bars plane zpos is immutable at {current} (primary zpos {primary_zpos}) — \
                     plane would composite under the video"
                );
            }
        }
    };
    // Same mutability rule for `pixel blend mode`. When immutable, the
    // pinned value decides: `Coverage` and `Pre-multiplied` both honour
    // our alpha layout (opaque bars/labels, translucent-black
    // background); a pinned `None` would scan the strip out as an
    // opaque box, so treat the plane as unusable and fall back.
    let (pixel_blend_mode, coverage_value) =
        match optional_prop_state(card, bars_plane, "pixel blend mode")? {
            None => (None, 0),
            Some((handle, true, _current)) => {
                let coverage = read_enum_value(card, bars_plane, "pixel blend mode", "Coverage")
                    .unwrap_or(0);
                (Some(handle), coverage)
            }
            Some((_handle, false, current)) => {
                let coverage = read_enum_value(card, bars_plane, "pixel blend mode", "Coverage");
                let premult =
                    read_enum_value(card, bars_plane, "pixel blend mode", "Pre-multiplied");
                if Some(current) == coverage || Some(current) == premult {
                    (None, 0)
                } else {
                    anyhow::bail!(
                        "bars plane 'pixel blend mode' is immutable at value {current} \
                         (neither Coverage nor Pre-multiplied) — strip would render opaque"
                    );
                }
            }
        };
    Ok((zpos, zpos_value, pixel_blend_mode, coverage_value))
}

/// Search for an additional plane (beyond `exclude`) drivable from
/// `crtc` that natively advertises `NV12` — the secondary content
/// plane [`KmsDisplay::try_promote_yuv_overlay`] adopts when the
/// primary plane permanently rejects linear YUV (confirmed: RK3588
/// VOP2 Cluster windows, AFBC-only for YUV; Esmart windows support
/// linear YUV directly and report `type=Overlay`, indistinguishable
/// by type alone from a bars-overlay ARGB8888 candidate — hence the
/// explicit NV12 format-list filter here rather than reusing
/// [`discover_bars_overlay_plane`]'s type-based search).
///
/// Unlike the bars-overlay search, this resolves `COLOR_ENCODING`/
/// `COLOR_RANGE` too — this plane carries real YUV pixel data, not a
/// translucent ARGB8888 strip, so it needs the same colorimetry
/// properties the primary plane's YUV path already sets.
fn discover_yuv_overlay_plane(
    card: &CardFile,
    crtc: drm::control::crtc::Handle,
    exclude: &[plane::Handle],
) -> Result<(plane::Handle, PlanePropIds)> {
    let res = card
        .resource_handles()
        .context("resource_handles for yuv-overlay discovery")?;
    let planes = card
        .plane_handles()
        .context("plane_handles for yuv-overlay discovery")?;
    let nv12 = DrmFourcc::Nv12 as u32;
    let mut found: Option<plane::Handle> = None;
    for p in planes {
        if exclude.contains(&p) {
            continue;
        }
        let info = card.get_plane(p).context("get_plane (yuv-overlay search)")?;
        if !res.filter_crtcs(info.possible_crtcs()).iter().any(|c| *c == crtc) {
            continue;
        }
        if !info.formats().iter().any(|f| *f == nv12) {
            continue;
        }
        if read_plane_type(card, p)? == PlaneType::Overlay as u64 {
            found = Some(p);
            break;
        }
    }
    let plane_h = found.context(
        "no additional Overlay-type plane drivable from this CRTC advertises NV12 — \
         no zero-copy fallback plane available on this host",
    )?;
    let names = [
        "FB_ID", "CRTC_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W",
        "CRTC_H",
    ];
    let p = collect_props(card, plane_h, &names)?;
    let color_encoding = optional_prop(card, plane_h, "COLOR_ENCODING")?;
    let color_encoding_value =
        read_enum_value(card, plane_h, "COLOR_ENCODING", "ITU-R BT.709 YCbCr").unwrap_or(0);
    let color_range = optional_prop(card, plane_h, "COLOR_RANGE")?;
    let color_range_value =
        read_enum_value(card, plane_h, "COLOR_RANGE", "YCbCr limited range").unwrap_or(0);
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
        color_encoding,
        color_encoding_value,
        color_range,
        color_range_value,
    };
    Ok((plane_h, plane_props))
}

/// Resolve the optional `zpos` property on a just-discovered
/// yuv-overlay plane, so it composes ABOVE the primary plane's stale
/// dumb-buffer content underneath it. Simplified sibling of
/// [`discover_bars_overlay_blend_props`] — no `pixel blend mode`
/// needed since YUV content is always fully opaque (no alpha plane),
/// unlike the bars overlay's ARGB8888 strip.
fn discover_yuv_overlay_zpos(
    card: &CardFile,
    yuv_plane: plane::Handle,
    primary_plane: plane::Handle,
) -> Result<(Option<property::Handle>, u64)> {
    let primary_zpos = read_property_u64(card, primary_plane, "zpos").unwrap_or(0);
    let zpos_value = primary_zpos.saturating_add(1);
    let zpos = match optional_prop_state(card, yuv_plane, "zpos")? {
        None => None,
        Some((handle, true, _current)) => Some(handle),
        Some((_handle, false, current)) => {
            if current > primary_zpos {
                None
            } else {
                anyhow::bail!(
                    "yuv-overlay plane zpos is immutable at {current} (primary zpos \
                     {primary_zpos}) — plane would composite under the primary"
                );
            }
        }
    };
    Ok((zpos, zpos_value))
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

/// `true` for the YUV/semi-planar fourccs bilbycast-edge's zero-copy
/// PRIME paths (VAAPI, RKMPP) actually produce. Drives whether
/// `COLOR_ENCODING` / `COLOR_RANGE` plane properties get set on an
/// atomic commit — see [`PlanePropIds::color_encoding`].
fn fourcc_is_yuv(fourcc: DrmFourcc) -> bool {
    matches!(
        fourcc,
        DrmFourcc::Nv12
            | DrmFourcc::Nv21
            | DrmFourcc::Nv16
            | DrmFourcc::Nv61
            | DrmFourcc::Nv24
            | DrmFourcc::Nv42
            | DrmFourcc::P010
            | DrmFourcc::P016
            | DrmFourcc::P210
            | DrmFourcc::Yuyv
            | DrmFourcc::Uyvy
    )
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

/// State retained across `present_prime` calls — the in-flight frame's
/// source-frame keepalive. Released only when the *next* page-flip
/// completes (the kernel keeps the previous scanout source live until
/// the new one has been fully scanned out at least once, and VAAPI /
/// RKMPP frames stay valid across exactly that boundary). The KMS
/// framebuffer *object* itself is no longer tracked here — see
/// [`PrimeFbCacheEntry`]; its lifetime is cache-managed instead of
/// per-frame-transition-managed.
#[derive(Default)]
struct PrimeState {
    /// Keepalive on the in-flight decoded frame (VAAPI surface or
    /// RKMPP DRM_PRIME frame).
    in_flight_keepalive: Option<Arc<dyn PrimeKeepalive>>,
}

/// Generous headroom above RKMPP's fixed 16-buffer decode pool
/// (`FRAMEGROUP_MAX_FRAMES` in FFmpeg's `rkmppdec.c`) — in steady-state
/// playback every recurring buffer fits without ever evicting;
/// eviction only engages across a decoder restart that introduces new
/// underlying buffer objects (a resolution change, or a fresh decoder
/// open after a source switch).
const PRIME_FB_CACHE_CAPACITY: usize = 32;

/// Cached KMS framebuffer for a PRIME import, keyed by the underlying
/// DMA-BUF's kernel identity — see [`dmabuf_identity`] and
/// [`KmsDisplay::prime_fb_cache`].
struct PrimeFbCacheEntry {
    fb: framebuffer::Handle,
    width: u32,
    height: u32,
    fourcc: DrmFourcc,
    modifier: Option<DrmModifier>,
}

/// Kernel-stable identity of the underlying DMA-BUF behind `fd`, used
/// to recognise "this is the same physical decode buffer as a
/// previous frame" even when the exporting decoder hands back a
/// freshly `dup()`'d fd number each time. `(st_dev, st_ino)` is the
/// standard way Linux identifies the same underlying file/object
/// across different fds (the same technique `lsof` and friends use) —
/// a DMA-BUF is backed by an anonymous inode that `fstat` reports
/// consistently for the life of the buffer object.
fn dmabuf_identity(fd: i32) -> Option<(u64, u64)> {
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) == 0 {
            Some((st.st_dev as u64, st.st_ino as u64))
        } else {
            None
        }
    }
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
/// `display_atomic_unavailable` Warning. `Busy` (EBUSY) is a transient
/// scheduling collision — a previous nonblocking commit (or a legacy
/// modeset fired by the auto-match path at an input-switch seam) was
/// still in flight; the caller drops the frame and the next attempt a
/// frame-period later goes through. It is **never** attributed to the
/// bars overlay — disabling the overlay can't unwedge a busy pipeline,
/// and doing so on the first transient EBUSY permanently downgraded
/// bars to the CPU-bake path mid-session (observed 2026-06-11).
/// `Other` is any remaining error the caller propagates as a normal
/// display-path failure; `bars_included` records whether the failing
/// request actually carried the bars-plane property set, so the
/// disable-bars-and-retry recovery only fires when the bars plane
/// could plausibly be the cause.
#[derive(Debug)]
enum AtomicCommitError {
    Unsupported(std::io::Error),
    Busy(String),
    Other { msg: String, bars_included: bool },
}

#[inline]
fn libc_einval() -> i32 {
    22
}
#[inline]
fn libc_ebusy() -> i32 {
    16
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

        // Check the import cache first — a decoder buffer pool recycles
        // a small fixed set of underlying DMA-BUFs (RKMPP: ≤16), and
        // re-running `prime_fd_to_buffer` + `add_planar_framebuffer`
        // for the SAME underlying buffer on every recurrence is real,
        // measurable overhead on some drivers: confirmed on RK3588,
        // where skipping it here dropped average `present_prime` cost
        // from double-digit milliseconds (swamping the 41.6ms/frame
        // budget at 24fps) to sub-millisecond. GStreamer's kmssink uses
        // the identical pattern for the identical reason. VAAPI's
        // zero-copy path on Intel/AMD already presents fast without
        // this (a cache hit there is just a cheap lookup instead of a
        // cheap import — no measurable difference), so this applies
        // universally rather than being gated to any one backend.
        let cache_key = descriptor.planes.first().and_then(|p| dmabuf_identity(p.fd));
        let cached_fb = cache_key.and_then(|key| self.prime_fb_cache.get(&key)).and_then(|e| {
            (e.width == descriptor.width
                && e.height == descriptor.height
                && e.fourcc == fourcc
                && e.modifier == modifier)
                .then_some(e.fb)
        });
        let new_fb = if let Some(fb) = cached_fb {
            fb
        } else {
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
            let fb = self
                .card
                .add_planar_framebuffer(&buf, flags)
                .context("display_prime_addfb_failed: add_planar_framebuffer")?;
            if let Some(key) = cache_key {
                if let Some(old) = self.prime_fb_cache.remove(&key) {
                    // Format changed for a recurring buffer object
                    // (rare — e.g. a decoder resolution change reusing
                    // the same underlying DMA-BUF). Destroy the stale
                    // entry and drop its order-queue slot so eviction
                    // bookkeeping stays consistent.
                    let _ = self.card.destroy_framebuffer(old.fb);
                    self.prime_fb_cache_order.retain(|k| *k != key);
                }
                if self.prime_fb_cache.len() >= PRIME_FB_CACHE_CAPACITY {
                    if let Some(oldest_key) = self.prime_fb_cache_order.pop_front() {
                        if let Some(evicted) = self.prime_fb_cache.remove(&oldest_key) {
                            let _ = self.card.destroy_framebuffer(evicted.fb);
                        }
                    }
                }
                self.prime_fb_cache_order.push_back(key);
                self.prime_fb_cache.insert(
                    key,
                    PrimeFbCacheEntry {
                        fb,
                        width: descriptor.width,
                        height: descriptor.height,
                        fourcc,
                        modifier,
                    },
                );
            }
            fb
        };

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
                        // Legacy set_crtc can't compose the overlay
                        // plane — tear it down so the display loop
                        // engages the CPU-bake bars fallback instead
                        // of believing bars are live.
                        self.disable_bars_overlay();
                    }
                }
            }
        }

        if self.use_atomic && self.atomic.is_some() {
            match self.atomic_present(new_fb, descriptor.width, descriptor.height, fourcc_is_yuv(fourcc)) {
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
                    // Legacy set_crtc can't compose the overlay plane —
                    // tear it down so the display loop engages the
                    // CPU-bake bars fallback (and the reheal arms for a
                    // later mode change) instead of believing bars are
                    // live while nothing composes them.
                    self.disable_bars_overlay();
                    if let Err(e2) = self.set_crtc_prime(new_fb) {
                        let _ = self.card.destroy_framebuffer(new_fb);
                        return Err(e2);
                    }
                }
                Err(AtomicCommitError::Busy(e)) => {
                    // Transient EBUSY — a previous nonblocking commit or
                    // a legacy modeset (auto-match at an input-switch
                    // seam) is still in flight. Drop this frame; the
                    // next one retries a frame period later. Never tear
                    // the bars overlay down for EBUSY: the overlay can't
                    // cause a busy pipeline, and doing so on a one-frame
                    // collision permanently downgraded the composition
                    // path mid-session (observed 2026-06-11).
                    let _ = self.card.destroy_framebuffer(new_fb);
                    return Err(anyhow::anyhow!(
                        "display_prime_page_flip_failed: atomic_commit busy (frame dropped): {e}"
                    ));
                }
                Err(AtomicCommitError::Other { msg, bars_included }) => {
                    // Disable-and-retry only when the failing request
                    // actually carried the bars plane props — a failing
                    // bars-LESS commit (e.g. the post-modeset re-arm)
                    // says nothing about the bars plane.
                    if bars_included && self.bars_overlay.is_some() {
                        tracing::warn!(
                            "atomic_commit failed with bars overlay included ({msg}) — \
                             disabling overlay and retrying without bars"
                        );
                        self.disable_bars_overlay();
                        self.invalidate_atomic_modeset();
                        match self.atomic_present(new_fb, descriptor.width, descriptor.height, fourcc_is_yuv(fourcc)) {
                            Ok(()) => {
                                if let Err(e2) = self.wait_page_flip() {
                                    let _ = self.card.destroy_framebuffer(new_fb);
                                    return Err(e2.context(
                                        "display_prime_page_flip_failed: receive_events (bars-retry)",
                                    ));
                                }
                            }
                            Err(retry_err) => {
                                let _ = self.card.destroy_framebuffer(new_fb);
                                return Err(anyhow::anyhow!(
                                    "display_prime_page_flip_failed: atomic_commit \
                                     (also failed without bars): {retry_err:?}"
                                ));
                            }
                        }
                    } else if fourcc_is_yuv(fourcc)
                        && self.yuv_overlay.is_none()
                        && !self.yuv_overlay_promotion_attempted
                    {
                        // Primary plane permanently rejected linear YUV.
                        // Try adopting a secondary NV12-capable plane
                        // ONCE (see `try_promote_yuv_overlay`'s doc
                        // comment) before giving up on zero-copy
                        // entirely for this session. On VAAPI/Intel/AMD
                        // hosts (where the primary plane's rejection
                        // means something unrelated) promotion finds no
                        // suitable plane and this falls through to the
                        // exact same error path as before this existed.
                        self.yuv_overlay_promotion_attempted = true;
                        match self.try_promote_yuv_overlay() {
                            Ok(()) => {
                                self.invalidate_atomic_modeset();
                                match self.atomic_present(
                                    new_fb,
                                    descriptor.width,
                                    descriptor.height,
                                    true,
                                ) {
                                    Ok(()) => {
                                        if let Err(e2) = self.wait_page_flip() {
                                            let _ = self.card.destroy_framebuffer(new_fb);
                                            return Err(e2.context(
                                                "display_prime_page_flip_failed: receive_events (yuv-overlay-retry)",
                                            ));
                                        }
                                    }
                                    Err(retry_err) => {
                                        // Secondary plane ALSO rejected it — don't
                                        // keep retrying a broken plane every frame.
                                        self.yuv_overlay = None;
                                        let _ = self.card.destroy_framebuffer(new_fb);
                                        return Err(anyhow::anyhow!(
                                            "display_prime_page_flip_failed: atomic_commit \
                                             (also failed on secondary NV12 plane): {retry_err:?}"
                                        ));
                                    }
                                }
                            }
                            Err(promote_err) => {
                                tracing::debug!(
                                    "display: no secondary NV12-capable plane available \
                                     ({promote_err:#}) — falling back to sysmem"
                                );
                                let _ = self.card.destroy_framebuffer(new_fb);
                                return Err(anyhow::anyhow!(
                                    "display_prime_page_flip_failed: atomic_commit: {msg}"
                                ));
                            }
                        }
                    } else {
                        let _ = self.card.destroy_framebuffer(new_fb);
                        return Err(anyhow::anyhow!(
                            "display_prime_page_flip_failed: atomic_commit: {msg}"
                        ));
                    }
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
        // `new_fb`'s object lifetime is cache-managed (see
        // `prime_fb_cache` above) rather than destroyed on every frame
        // transition — only `in_flight_keepalive` (the underlying
        // decoded-frame memory, which really does need the one-frame-
        // delayed release) still needs per-transition bookkeeping here.
        let mut state = self.prime_state.take().unwrap_or_default();
        let prev_keepalive = state.in_flight_keepalive.take();
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
        is_yuv: bool,
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
                    color_encoding: setup.plane_props.color_encoding,
                    color_encoding_value: setup.plane_props.color_encoding_value,
                    color_range: setup.plane_props.color_range,
                    color_range_value: setup.plane_props.color_range_value,
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
        // Modeset boundary: land the modeset triplet in its OWN commit
        // (with the proven-safe linear dumb FB on the primary plane)
        // before flipping the content FB. See `commit_modeset_only` for
        // why combining them EINVALs on i915 with tiled prime FBs.
        if !self.atomic_modeset_done {
            self.commit_modeset_only(plane, &plane_props, &crtc_props, &connector_props)?;
        }

        // Choose which plane actually carries the FB_ID/CRTC_ID/SRC/CRTC
        // content properties this commit. Normally that's the primary
        // plane (`plane`/`plane_props`, unchanged from before) — for
        // RGB dumb-buffer presents always, and for YUV PRIME presents
        // on every host where the primary plane accepts YUV directly
        // (VAAPI on Intel/AMD, proven over many prior releases). Once
        // `try_promote_yuv_overlay` has adopted a secondary plane
        // (RK3588 Cluster-window case), every YUV present targets it
        // instead — the primary plane is left untouched by this commit,
        // keeping whatever dumb-buffer FB it last had (occluded once
        // the yuv-overlay plane's zpos places it on top).
        let yuv_target = if is_yuv {
            self.yuv_overlay.as_ref().map(|y| {
                (
                    y.plane,
                    PlanePropIds {
                        fb_id: y.plane_props.fb_id,
                        crtc_id: y.plane_props.crtc_id,
                        src_x: y.plane_props.src_x,
                        src_y: y.plane_props.src_y,
                        src_w: y.plane_props.src_w,
                        src_h: y.plane_props.src_h,
                        crtc_x: y.plane_props.crtc_x,
                        crtc_y: y.plane_props.crtc_y,
                        crtc_w: y.plane_props.crtc_w,
                        crtc_h: y.plane_props.crtc_h,
                        color_encoding: y.plane_props.color_encoding,
                        color_encoding_value: y.plane_props.color_encoding_value,
                        color_range: y.plane_props.color_range,
                        color_range_value: y.plane_props.color_range_value,
                    },
                    y.zpos,
                    y.zpos_value,
                )
            })
        } else {
            None
        };
        let (content_plane, cp, yuv_zpos, yuv_zpos_value) = match yuv_target {
            Some((p, props, zpos, zpos_value)) => (p, props, zpos, zpos_value),
            None => (plane, plane_props, None, 0),
        };

        let pp = &cp;
        let mut req = AtomicModeReq::new();
        req.add_property(content_plane, pp.fb_id, property::Value::Framebuffer(Some(new_fb)));
        req.add_property(content_plane, pp.crtc_id, property::Value::CRTC(Some(self.crtc)));
        // SRC rectangle: full source surface in 16.16 fixed-point.
        req.add_property(content_plane, pp.src_x, property::Value::UnsignedRange(0));
        req.add_property(content_plane, pp.src_y, property::Value::UnsignedRange(0));
        req.add_property(
            content_plane,
            pp.src_w,
            property::Value::UnsignedRange((src_w as u64) << 16),
        );
        req.add_property(
            content_plane,
            pp.src_h,
            property::Value::UnsignedRange((src_h as u64) << 16),
        );
        // CRTC rectangle: full destination panel.
        req.add_property(content_plane, pp.crtc_x, property::Value::SignedRange(0));
        req.add_property(content_plane, pp.crtc_y, property::Value::SignedRange(0));
        req.add_property(
            content_plane,
            pp.crtc_w,
            property::Value::UnsignedRange(self.width as u64),
        );
        req.add_property(
            content_plane,
            pp.crtc_h,
            property::Value::UnsignedRange(self.height as u64),
        );
        // COLOR_ENCODING / COLOR_RANGE: required by some embedded
        // VOP/VOP2-class drivers (confirmed: RK3588 Cluster window) to
        // accept a YUV PRIME framebuffer commit — omitted for RGB since
        // it's meaningless there and some drivers reject a COLOR_*
        // property set on a non-YUV plane update. See the field doc on
        // `PlanePropIds::color_encoding`.
        if is_yuv {
            if let Some(enc_prop) = pp.color_encoding {
                req.add_property(
                    content_plane,
                    enc_prop,
                    property::Value::UnsignedRange(pp.color_encoding_value),
                );
            }
            if let Some(range_prop) = pp.color_range {
                req.add_property(
                    content_plane,
                    range_prop,
                    property::Value::UnsignedRange(pp.color_range_value),
                );
            }
        }
        // zpos for the secondary yuv-overlay plane, so it composes
        // above the primary's stale dumb-buffer content underneath it.
        // Not applicable when `content_plane == plane` (primary itself).
        if let Some(zpos_prop) = yuv_zpos {
            req.add_property(
                content_plane,
                zpos_prop,
                property::Value::UnsignedRange(yuv_zpos_value),
            );
        }
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
        //
        // Skip the bars plane on the very first commit (the modeset):
        // Intel i915 on Arrow Lake rejects a first ALLOW_MODESET commit
        // that simultaneously binds a new overlay plane. Adding the bars
        // on the second commit (after the modeset has landed) works.
        //
        // `bars_included` records whether THIS request actually carries
        // the bars plane props — the success/failure book-keeping below
        // must key off it, not off `self.bars_overlay.is_some()`. The
        // earlier revision marked `bars.committed = true` (and logged
        // "first atomic_commit OK") on the bars-LESS modeset commit
        // this guard skips, which made the log claim the bars plane
        // had landed on hosts where every real bars commit was failing.
        let mut bars_included = false;
        if self.atomic_modeset_done {
        if let Some(bars) = self.bars_overlay.as_mut() {
            bars_included = true;
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
        } // bars skip on first modeset commit

        match self.card.atomic_commit(flags, req) {
            Ok(()) => {
                self.atomic_modeset_done = true;
                self.atomic_ever_succeeded = true;
                self.atomic_consecutive_failures = 0;
                self.hdr_dirty = false;
                // Only count this success toward the bars plane when the
                // request actually carried the bars props — the modeset
                // commit above skips them, and crediting it here used to
                // log "first atomic_commit OK" (and set `committed`) on
                // hosts where every real bars-bearing commit was failing.
                if bars_included {
                    if let Some(bars) = self.bars_overlay.as_mut() {
                        // First successful commit that programmed the bars
                        // plane: log so an operator debugging "bars don't show
                        // on the panel" can confirm the atomic_commit path
                        // accepted the property set (vs being rejected
                        // silently — in practice the kernel surfaces EINVAL
                        // here, but a silent driver-side clip is the next
                        // failure mode if zpos / pixel-blend-mode discovery
                        // came up wrong).
                        if !bars.committed {
                            tracing::info!(
                                bars_plane = format!("{:?}", bars.plane),
                                strip_w = bars.width,
                                strip_h = bars.height,
                                strip_top_y = self.height.saturating_sub(bars.height),
                                zpos_set = bars.zpos.is_some(),
                                zpos_value = bars.zpos_value,
                                blend_mode_set = bars.pixel_blend_mode.is_some(),
                                "audio-bars overlay first atomic_commit OK"
                            );
                        }
                        bars.committed = true;
                    }
                }
                Ok(())
            }
            Err(e) => Err(self.classify_atomic_commit_err(e, bars_included)),
        }
    }

    /// Submit a modeset-only atomic commit: the modeset triplet
    /// (CRTC.ACTIVE / CRTC.MODE_ID / CONNECTOR.CRTC_ID) plus the primary
    /// plane showing the linear XRGB8888 **dumb buffer** — the FB shape
    /// the legacy `set_crtc` path has already proven against this
    /// driver. The caller then flips the real content FB (VAAPI prime,
    /// possibly tiled NV12 / P010) in a separate plain commit.
    ///
    /// Combining ALLOW_MODESET with a tiled prime FB in ONE commit is
    /// what i915 rejected on a 4K-first session start (EINVAL on the
    /// very first atomic commit → permanent legacy fallback → bars
    /// overlay dead, observed live 2026-06-11). Splitting costs one
    /// extra blocking commit per modeset boundary and one frame of the
    /// (blanked-anyway) dumb buffer on the panel.
    ///
    /// Runs BLOCKING (no NONBLOCK, no PAGE_FLIP_EVENT): re-arm commits
    /// follow hot on the heels of a legacy `set_crtc` and a nonblocking
    /// ALLOW_MODESET commit racing that in-flight modeset returns EBUSY
    /// — observed killing the bars overlay one minute into a session
    /// (2026-06-11).
    fn commit_modeset_only(
        &mut self,
        plane: drm::control::plane::Handle,
        pp: &PlanePropIds,
        crtc_props: &CrtcPropIds,
        connector_props: &ConnectorPropIds,
    ) -> std::result::Result<(), AtomicCommitError> {
        let blob_id = {
            let loaded = self.atomic.as_ref().and_then(|s| s.mode_blob_id);
            match loaded {
                Some(id) => id,
                None => {
                    let blob = self
                        .card
                        .create_property_blob(&self.mode)
                        .map_err(|e| AtomicCommitError::Other {
                            msg: io_to_string(&e),
                            bars_included: false,
                        })?;
                    let id = match blob {
                        property::Value::Blob(id) => id,
                        _ => {
                            return Err(AtomicCommitError::Other {
                                msg: "create_property_blob returned non-Blob value".into(),
                                bars_included: false,
                            })
                        }
                    };
                    if let Some(setup) = self.atomic.as_mut() {
                        setup.mode_blob_id = Some(id);
                    }
                    id
                }
            }
        };
        let dumb_fb = self.bufs[self.front_idx].fb;
        let mut req = AtomicModeReq::new();
        req.add_property(plane, pp.fb_id, property::Value::Framebuffer(Some(dumb_fb)));
        req.add_property(plane, pp.crtc_id, property::Value::CRTC(Some(self.crtc)));
        req.add_property(plane, pp.src_x, property::Value::UnsignedRange(0));
        req.add_property(plane, pp.src_y, property::Value::UnsignedRange(0));
        req.add_property(
            plane,
            pp.src_w,
            property::Value::UnsignedRange((self.width as u64) << 16),
        );
        req.add_property(
            plane,
            pp.src_h,
            property::Value::UnsignedRange((self.height as u64) << 16),
        );
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
        req.add_property(self.crtc, crtc_props.active, property::Value::Boolean(true));
        req.add_property(self.crtc, crtc_props.mode_id, property::Value::Blob(blob_id));
        req.add_property(
            self.connector,
            connector_props.crtc_id,
            property::Value::CRTC(Some(self.crtc)),
        );
        match self
            .card
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        {
            Ok(()) => {
                self.atomic_modeset_done = true;
                self.atomic_ever_succeeded = true;
                self.atomic_consecutive_failures = 0;
                Ok(())
            }
            Err(e) => Err(self.classify_atomic_commit_err(e, false)),
        }
    }

    /// Map a failed `atomic_commit` onto the [`AtomicCommitError`]
    /// taxonomy, with all the session book-keeping that goes with it
    /// (first-bars-commit diagnostics, consecutive-failure escape to
    /// the legacy path).
    fn classify_atomic_commit_err(
        &mut self,
        e: std::io::Error,
        bars_included: bool,
    ) -> AtomicCommitError {
        use std::io::ErrorKind;
        let raw = e.raw_os_error();
        let unsupported = matches!(e.kind(), ErrorKind::Unsupported)
            || raw == Some(libc_eopnotsupp())
            || raw == Some(libc_enosys())
            // First-commit-EVER EINVAL: driver / plane combo
            // refuses our property set; the plain set_crtc path
            // may still work (different ioctl). Keyed on
            // `atomic_ever_succeeded`, NOT `atomic_modeset_done`
            // — after a legacy auto-match modeset the latter is
            // false again, and the pre-fix key turned ONE
            // transient EINVAL on the re-modeset commit into a
            // permanent legacy fallback (bars overlay dead for
            // the session, observed live on a 4K Take
            // 2026-06-11). A re-modeset EINVAL now classifies
            // as `Other` and the next frame retries; persistent
            // rejection escapes via the consecutive-failure
            // limit below.
            || (!self.atomic_ever_succeeded && raw == Some(libc_einval()));
                // Surface every bars-bearing commit failure on its first
                // occurrence so an operator debugging "bars don't show"
                // can see whether the bars plane property set caused the
                // rejection. `bars.committed` flips true on the first
                // success; while still false, we haven't yet seen the
                // bars plane land on the panel. Gated on `bars_included`
                // so a failing bars-less modeset commit doesn't get
                // mis-blamed on the bars plane.
                if bars_included {
                    if let Some(bars) = self.bars_overlay.as_ref() {
                        if !bars.committed {
                            tracing::warn!(
                                bars_plane = format!("{:?}", bars.plane),
                                errno = ?raw,
                                unsupported,
                                "audio-bars overlay first atomic_commit FAILED — bars plane property set may be wrong for this driver"
                            );
                        }
                    }
                }
        if unsupported {
            AtomicCommitError::Unsupported(e)
        } else if raw == Some(libc_ebusy()) {
            AtomicCommitError::Busy(io_to_string(&e))
        } else {
            self.atomic_consecutive_failures =
                self.atomic_consecutive_failures.saturating_add(1);
            if self.atomic_consecutive_failures >= ATOMIC_CONSECUTIVE_FAILURE_LIMIT {
                // Persistent rejection (every frame for ~2 s) —
                // this driver genuinely won't take our atomic
                // requests in the current mode. Escape to the
                // legacy set_crtc path AND tear the bars
                // overlay down explicitly: legacy can't compose
                // a second plane, and leaving `bars_overlay`
                // Some makes the display loop believe bars are
                // live while nothing composes them. The
                // teardown flips the loop onto the CPU-bake
                // fallback (bars survive on ≤1080p sources) and
                // arms the reheal for a later mode change.
                tracing::warn!(
                    failures = self.atomic_consecutive_failures,
                    "atomic commits failing persistently — falling back to legacy set_crtc (bars overlay torn down, CPU-bake fallback engages)"
                );
                self.use_atomic = false;
                self.atomic_fallback_reason.get_or_insert_with(|| {
                    format!(
                        "atomic_commit failed {} consecutive times: {}",
                        self.atomic_consecutive_failures,
                        io_to_string(&e)
                    )
                });
                self.disable_bars_overlay();
                return AtomicCommitError::Unsupported(e);
            }
            AtomicCommitError::Other {
                msg: io_to_string(&e),
                bars_included,
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

    /// Block until the kernel posts `DRM_EVENT_FLIP_COMPLETE` for our
    /// CRTC, OR [`FLIP_EVENT_TIMEOUT_MS`] elapses with no completion.
    ///
    /// The DRM fd is blocking (no `O_NONBLOCK`), so a bare `receive_events()`
    /// would `read()`-block forever if the driver stopped posting the
    /// vblank-completion event (broken `nvidia-drm` flip IRQs, a wedged
    /// GPU). We `poll()` the fd against a deadline first so that case
    /// returns a distinct `display_flip_timeout` error — the display loop
    /// then drops the frame, emits a throttled `display_flip_timeout`
    /// event, and keeps trying (cancel-aware) instead of dead-locking the
    /// thread. The loop still guards against unrelated events being drained
    /// ahead of our flip.
    fn wait_page_flip(&self) -> Result<()> {
        use std::os::fd::{AsFd, AsRawFd};
        let raw = self.card.as_fd().as_raw_fd();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(FLIP_EVENT_TIMEOUT_MS);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!(
                    "display_flip_timeout: no DRM page-flip completion event within {} ms — \
                     the GPU/driver has stopped posting vblank completions (a black or frozen \
                     panel). On NVIDIA, verify `nvidia-drm.modeset=1` is set and the driver \
                     version is current; see docs/installation.md (Local-display output)",
                    FLIP_EVENT_TIMEOUT_MS
                );
            }
            // `.max(1)`: a sub-millisecond remainder must block for ~1 ms in
            // poll() rather than return instantly — `Duration::as_millis()`
            // floors, so without it the last <1 ms of the budget would
            // busy-spin (poll(timeout=0) returns immediately). The
            // top-of-loop `is_zero()` check still bounds the total wait
            // (deadline overrun ≤ one ~1 ms slice).
            let slice = remaining.as_millis().max(1).min(i32::MAX as u128) as libc::c_int;
            let mut pfd = libc::pollfd { fd: raw, events: libc::POLLIN, revents: 0 };
            // SAFETY: single initialised pollfd; the kernel only writes `revents`.
            let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, slice) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue; // EINTR — re-poll against the same deadline
                }
                return Err(anyhow::Error::new(err)
                    .context("display_page_flip_failed: poll(drm_fd)"));
            }
            if rc == 0 {
                continue; // slice expired with no event — the deadline check ends it
            }
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
            // Unrelated event(s) drained ahead of ours — re-poll for the
            // remaining budget.
        }
    }

    /// Flush any retained PRIME state — destroy every cached imported
    /// framebuffer and drop the in-flight keepalive. Idempotent: safe
    /// to call every frame on the CPU-blit path without paying for a
    /// re-modeset every vblank when there's nothing to release.
    ///
    /// Used on the runtime-demotion path: when sustained PRIME export
    /// failures push `output_display` from `vaapi-zerocopy` back to
    /// the CPU blit, the next blit calls this before writing the
    /// dumb-buffer fb so the scanout source is well-defined when
    /// `present()` flips onto the dumb-buffer. We're leaving the PRIME
    /// path for good on this decoder session at that point — a fresh
    /// decoder open (input switch, resolution change) allocates its
    /// own new buffer objects under fresh DMA-BUF identities anyway —
    /// so every cached framebuffer is flushed here rather than left to
    /// age out via `prime_fb_cache`'s normal FIFO eviction.
    pub fn release_prime_state(&mut self) {
        for (_, entry) in self.prime_fb_cache.drain() {
            let _ = self.card.destroy_framebuffer(entry.fb);
        }
        self.prime_fb_cache_order.clear();

        let Some(mut state) = self.prime_state.take() else {
            return;
        };
        if state.in_flight_keepalive.is_none() {
            // Nothing else to do — the slot was empty.
            return;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_alsa_pcm_positional_heuristic() {
        // First HDMI/DP connector → PCM 3, second → 7, others → 8.
        assert_eq!(infer_alsa_pcm_for("HDMI-A-1"), 3);
        assert_eq!(infer_alsa_pcm_for("DP-1"), 3);
        assert_eq!(infer_alsa_pcm_for("HDMI-A-2"), 7);
        assert_eq!(infer_alsa_pcm_for("HDMI-A-3"), 8);
        // No numeric suffix → default first HDMI PCM.
        assert_eq!(infer_alsa_pcm_for("HDMI-A"), 3);
    }

    #[test]
    fn alsa_hdmi_card_index_does_not_panic() {
        // Environment-dependent (reads /proc/asound) — just assert it
        // returns without panicking on whatever host runs the suite.
        let _ = alsa_hdmi_card_index();
    }

    #[test]
    fn cadence_multiple_accepts_clean_multiples() {
        assert!(refresh_is_cadence_multiple(50, 25.0));
        assert!(refresh_is_cadence_multiple(50, 50.0));
        assert!(refresh_is_cadence_multiple(60, 30.0));
        assert!(refresh_is_cadence_multiple(60, 60.0));
        assert!(refresh_is_cadence_multiple(48, 24.0));
        assert!(refresh_is_cadence_multiple(120, 24.0));
        // Kernel rounds 59.94 Hz modes to vrefresh 60 — a 59.94 fps /
        // 29.97 fps hint must still rank the "60 Hz" mode as matched.
        assert!(refresh_is_cadence_multiple(60, 59.94));
        assert!(refresh_is_cadence_multiple(60, 29.97));
    }

    #[test]
    fn cadence_multiple_rejects_pulldown_rates() {
        // The pre-fix ratio tolerance (|ratio − round| < 0.12) passed
        // all of these — film content got steered onto 50 Hz.
        assert!(!refresh_is_cadence_multiple(50, 24.0));
        assert!(!refresh_is_cadence_multiple(50, 23.976));
        assert!(!refresh_is_cadence_multiple(60, 25.0));
        assert!(!refresh_is_cadence_multiple(60, 50.0));
        assert!(!refresh_is_cadence_multiple(50, 30.0));
        // Refresh below the source rate can't present every frame.
        assert!(!refresh_is_cadence_multiple(25, 50.0));
        assert!(!refresh_is_cadence_multiple(30, 60.0));
    }
}

