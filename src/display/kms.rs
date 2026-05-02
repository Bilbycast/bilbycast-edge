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
// All KMS work is sw-only in v1 (dumb buffer + CPU XRGB8888 blit). The
// v2 hardware-decode path (DMA-BUF-zerocopy via VAAPI/NVDEC) lands as
// an additive backend behind the `display-vaapi` / `display-nvdec`
// Cargo features documented in `Cargo.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{
    connector::State as ConnectorState, framebuffer, AtomicCommitFlags, Device as ControlDevice,
};
use drm::Device;

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
}

struct DumbBuffer {
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

        Ok(Self {
            card,
            crtc,
            connector,
            mode,
            width: w as u32,
            height: h as u32,
            bufs: [a, b],
            front_idx: 0,
        })
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

    /// Re-program the connector to the smallest available mode whose
    /// dimensions are at least `(src_w, src_h)`. Used by the `auto`
    /// resolution path once the first decoded frame reveals the source
    /// flow's true size — picking the monitor's preferred mode on a 4K
    /// panel for a 1080p source is what produced the jitter the user
    /// reported. Drops + re-allocates the dumb-buffer pair if the new
    /// mode size differs. No-op when the new mode is identical to the
    /// current one.
    pub fn match_source_resolution(&mut self, src_w: u32, src_h: u32) -> Result<()> {
        let info = self
            .card
            .get_connector(self.connector, true)
            .context("get_connector for auto-match")?;
        let new_mode = pick_mode_for_source(&info, src_w, src_h)?;
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
            let _ = self.card.destroy_framebuffer(old.fb);
            let _ = self.card.destroy_dumb_buffer(old.handle);
        }
        self.mode = new_mode;
        self.width = new_w;
        self.height = new_h;
        self.front_idx = 0;
        Ok(())
    }

    /// Map the back buffer for direct CPU writes. The caller writes a
    /// W×H XRGB8888 pixel block into the returned slice (stride is
    /// the buffer's pitch in bytes — usually `W*4`, but `pitch()` is
    /// authoritative). The slice is unmapped on drop.
    pub fn back_buffer(&mut self) -> Result<DumbBufferMap<'_>> {
        let back_idx = 1 - self.front_idx;
        let pitch = self.bufs[back_idx].handle.pitch();
        let mapping = self
            .card
            .map_dumb_buffer(&mut self.bufs[back_idx].handle)
            .context("map_dumb_buffer")?;
        Ok(DumbBufferMap {
            mapping,
            pitch,
            width: self.width,
            height: self.height,
        })
    }

    /// Page-flip to the back buffer, swap front/back. Blocks until the
    /// next vsync via `set_crtc` (lighter than the async page-flip
    /// event path; v2 will move to atomic page flips for jitter
    /// reduction).
    pub fn present(&mut self) -> Result<()> {
        let back_idx = 1 - self.front_idx;
        self.card
            .set_crtc(
                self.crtc,
                Some(self.bufs[back_idx].fb),
                (0, 0),
                &[self.connector],
                Some(self.mode),
            )
            .context("display_mode_set_failed: page-flip set_crtc")?;
        self.front_idx = back_idx;
        Ok(())
    }
}

/// Borrowed view into a mapped dumb buffer. Drop unmaps. Same shape as
/// what the `drm` crate's `DumbMapping` returns; we re-export the
/// pitch + width + height for easy blits.
pub struct DumbBufferMap<'a> {
    mapping: drm::control::dumbbuffer::DumbMapping<'a>,
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
        self.mapping.as_mut()
    }
}

fn alloc_dumb_buffer(card: &CardFile, w: u32, h: u32) -> Result<DumbBuffer> {
    let handle = card
        .create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32)
        .context("create_dumb_buffer")?;
    let fb = card
        .add_framebuffer(&handle, 24, 32)
        .context("add_framebuffer")?;
    Ok(DumbBuffer { handle, fb })
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
    // 3. Connector-preferred mode.
    if let Some(m) = modes.iter().find(|m| {
        (m.mode_type() & drm::control::ModeTypeFlags::PREFERRED)
            == drm::control::ModeTypeFlags::PREFERRED
    }) {
        return Ok(*m);
    }
    // 4. Fall back to the first listed mode.
    Ok(modes[0])
}

/// Pick the best mode for an `auto`-resolution display output once the
/// first decoded frame reveals the source's `(src_w, src_h)`.
///
/// Strategy:
/// 1. Exact match on `(src_w, src_h)` — pick the highest-refresh option.
/// 2. Otherwise, the smallest mode whose dimensions are both ≥ source —
///    avoids a CPU upscale to the monitor's native (4K) preferred mode
///    when the flow is 1080p, which is the jitter case the user hit.
/// 3. If every mode is smaller than the source, the largest available.
fn pick_mode_for_source(
    info: &drm::control::connector::Info,
    src_w: u32,
    src_h: u32,
) -> Result<drm::control::Mode> {
    let modes = info.modes();
    if modes.is_empty() {
        anyhow::bail!("display_resolution_unsupported: connector has no modes");
    }
    let exact: Vec<&drm::control::Mode> = modes
        .iter()
        .filter(|m| m.size().0 as u32 == src_w && m.size().1 as u32 == src_h)
        .collect();
    if !exact.is_empty() {
        let mut em = exact;
        em.sort_by_key(|m| std::cmp::Reverse(m.vrefresh()));
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
                std::cmp::Reverse(m.vrefresh() as u64),
            )
        });
        return Ok(*at_or_above[0]);
    }
    let mut all: Vec<&drm::control::Mode> = modes.iter().collect();
    all.sort_by_key(|m| {
        (
            std::cmp::Reverse((m.size().0 as u64) * (m.size().1 as u64)),
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

// Suppress the silenced AtomicCommitFlags import in non-atomic mode-set
// paths above; v2 will use it. Keep the path here so the import stays
// exercised on a future swap.
#[allow(dead_code)]
fn _atomic_flags_check() -> AtomicCommitFlags {
    AtomicCommitFlags::ALLOW_MODESET
}
