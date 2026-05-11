// Audio-bars overlay for the local-display output.
//
// Two pieces:
// - `MeterSnapshot` + `update_levels()` — lock-free shared state the
//   meter task writes per audio block and the display task reads per
//   video frame.
// - `rasterise()` — paints the snapshot onto the BGRA dumb buffer
//   directly above the existing video blit, before `kms.present()`.
//
// Layout (broadcast confidence convention):
//   ┌─────────────────────────────────────────────────────────┐
//   │                                                         │
//   │                       <video>                           │
//   │                                                         │
//   │                                                         │
//   ├──────────[0x100 AAC 2.0]─[0x101 AC3 5.1]────────────────┤  ← strip
//   │           ▌▌            ▌▌▌▌▌▌  green base, yellow      │
//   │           ▌▌            ▌▌▌▌▌▌  >-18 dBFS, red >-3,     │
//   │                                 white peak-hold tick    │
//   └─────────────────────────────────────────────────────────┘
//
// Each PID becomes a fixed-width "block" (capped at BLOCK_MAX_W) and
// the row of blocks is centred — a single-PID stream lands as a
// compact strip in the middle of the screen rather than sprawling
// across the full frame width.
//
// All work is CPU. At 1080p the strip is ~130 px tall × 1920 px wide ×
// 4 B = ~1 MB per present. Negligible on any host that already runs
// libswscale Lanczos for the main blit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

/// Hold a peak-hold value for 1.5 s before letting it decay back to the
/// instantaneous RMS. Matches the BBC R128 / EBU recommended hold time
/// for confidence meters.
pub const PEAK_HOLD_DURATION: Duration = Duration::from_millis(1500);

/// Floor for dBFS readings — silence reads as exactly this value, no
/// `-inf` / NaN ever leaves the meter.
pub const DBFS_FLOOR: f32 = -120.0;

/// Industry-standard headroom thresholds. Bars above these light up
/// yellow / red.
pub const YELLOW_THRESHOLD_DBFS: f32 = -18.0;
pub const RED_THRESHOLD_DBFS: f32 = -3.0;

#[derive(Clone, Copy, Debug)]
pub struct ChannelLevel {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    pub peak_hold_dbfs: f32,
    pub peak_hold_until: Instant,
}

impl Default for ChannelLevel {
    fn default() -> Self {
        Self {
            rms_dbfs: DBFS_FLOOR,
            peak_dbfs: DBFS_FLOOR,
            peak_hold_dbfs: DBFS_FLOOR,
            peak_hold_until: Instant::now(),
        }
    }
}

#[derive(Debug)]
pub struct PidMeter {
    pub pid: u16,
    pub codec_label: &'static str,
    pub channels: Vec<ChannelLevel>,
    pub last_update: Instant,
    /// Pre-formatted `"0xNNN CODEC X.Y"` label. Recomputed only when
    /// the PID is created or `channels.len()` changes — the rasterise
    /// path reads `&pid.cached_label` instead of `format!()`-ing per
    /// frame, so a 60 fps display doing 3 PIDs no longer churns ~180
    /// short-lived `String`s/sec.
    pub cached_label: String,
}

impl Clone for PidMeter {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            codec_label: self.codec_label,
            channels: self.channels.clone(),
            last_update: self.last_update,
            cached_label: self.cached_label.clone(),
        }
    }

    /// Allocation-reusing `clone_from`. The default `Clone::clone_from`
    /// is `*self = source.clone()`, which drops `self`'s heap and
    /// allocates fresh — defeating the whole point of the
    /// [`MeterPublisher`] reclaim path. Implementing this manually
    /// lets `Vec<PidMeter>::clone_from` (called by
    /// `MeterSnapshot::clone_from` on the reclaim path) keep the
    /// inner `channels: Vec<ChannelLevel>` and `cached_label: String`
    /// heap allocations: `Vec::clone_from` for the now-`Copy`
    /// [`ChannelLevel`] is one memcpy that reuses the existing buffer
    /// when capacity permits, and `String::clone_from` likewise
    /// reuses its heap for same-length labels.
    fn clone_from(&mut self, source: &Self) {
        self.pid = source.pid;
        self.codec_label = source.codec_label;
        self.channels.clone_from(&source.channels);
        self.last_update = source.last_update;
        self.cached_label.clone_from(&source.cached_label);
    }
}

#[derive(Debug, Default)]
pub struct MeterSnapshot {
    pub per_pid: Vec<PidMeter>,
    pub generation: u64,
}

impl Clone for MeterSnapshot {
    fn clone(&self) -> Self {
        Self {
            per_pid: self.per_pid.clone(),
            generation: self.generation,
        }
    }

    /// Allocation-reusing `clone_from` — delegates to
    /// `Vec<PidMeter>::clone_from`, which reuses the outer Vec's
    /// allocation when capacity permits and calls
    /// [`PidMeter::clone_from`] per matching element to reuse each
    /// PidMeter's inner heap allocations. Steady-state (PMT stable)
    /// publishes do **zero** `Vec`/`String` allocations beyond the
    /// `Arc::new` header itself.
    fn clone_from(&mut self, source: &Self) {
        self.per_pid.clone_from(&source.per_pid);
        self.generation = source.generation;
    }
}

/// Lock-free hand-off of the meter snapshot from the meter task to the
/// display task. The meter task owns its own mutable `MeterSnapshot` and
/// publishes a fresh `Arc<MeterSnapshot>` after each update; the display
/// task does an atomic load on the hot path — no mutex, no contention,
/// no per-frame allocation.
pub type SharedMeter = Arc<ArcSwap<MeterSnapshot>>;

pub fn new_shared_meter() -> SharedMeter {
    Arc::new(ArcSwap::from_pointee(MeterSnapshot::default()))
}

/// Wraps the meter task's working `MeterSnapshot` together with the
/// `SharedMeter` it publishes onto, plus a single-slot Arc reclaim
/// buffer so consecutive publishes don't allocate a fresh
/// `Arc<MeterSnapshot>` every block.
///
/// Steady-state behaviour: `publish()` calls `ArcSwap::swap` on each
/// publish to atomically take back the previous Arc, then attempts
/// `Arc::try_unwrap` on it. When the display task has already dropped
/// its read guard (the common case at 50 Hz publish vs 60 fps display
/// — roughly half of the publishes hit a window where the display
/// just ran), `try_unwrap` succeeds and we copy the working state
/// into the reclaimed allocation in place via `Vec::clone_from`. The
/// outer `Vec<PidMeter>` allocation + the `Arc` header allocation are
/// reused, so the per-publish heap activity drops from
/// `(Arc::new + Vec::with_capacity + N × Vec::clone)` to just
/// `(N × Vec::clone)` for the inner `Vec<ChannelLevel>` per PID.
///
/// On `try_unwrap` failure (display task currently holds the guard —
/// the small window between `ArcSwap::load` and the rasteriser
/// finishing) we fall back to `Arc::new(local.clone())` and stash the
/// still-held Arc as the next round's spare. The next publish retries
/// `try_unwrap` on it; the display task always drops the guard within
/// one frame, so the spare is always reclaimable on the next attempt.
///
/// **Lock-free**: `ArcSwap::swap` is a single atomic; `try_unwrap`
/// reads + CAS-decrements the refcount; no blocking, no spinning.
pub struct MeterPublisher {
    /// Working snapshot — mutated in place by [`update_levels`] /
    /// [`prune_stale`]. Public so test code can inspect it after a
    /// publish without going through the published Arc.
    pub local: MeterSnapshot,
    /// Single-slot reclaim buffer. `None` on first publish; `Some` on
    /// every subsequent publish, holding the Arc most-recently swapped
    /// out of `shared`.
    spare: Option<Arc<MeterSnapshot>>,
    /// The shared `ArcSwap` the display task reads from.
    pub shared: SharedMeter,
    /// Monotonic counter incremented on every successful `publish()`.
    /// Read by the meter task to bump the operator-facing
    /// `DisplayStatsCounters.meter_publishes` — gives the dashboard a
    /// "is the meter alive and decoding" signal that doesn't depend
    /// on whether `per_pid` happens to be non-empty at snapshot time.
    publish_count: u64,
}

impl MeterPublisher {
    /// Construct with an empty working snapshot. Pair with an existing
    /// `SharedMeter` so the display task can already be reading even
    /// before the first publish (it observes the default-empty
    /// `MeterSnapshot` until the first `publish()` lands).
    pub fn new(shared: SharedMeter) -> Self {
        Self {
            local: MeterSnapshot::default(),
            spare: None,
            shared,
            publish_count: 0,
        }
    }

    /// Monotonic count of successful `publish()` calls. The meter task
    /// snapshots this each loop iteration to forward deltas into the
    /// `DisplayStatsCounters.meter_publishes` AtomicU64.
    pub fn publish_count(&self) -> u64 {
        self.publish_count
    }

    /// Publish the working snapshot. Prefer this over manual
    /// `ArcSwap::store` calls — it does the `try_unwrap` reclaim that
    /// makes the per-block allocation footprint roughly an order of
    /// magnitude smaller in steady state.
    pub fn publish(&mut self) {
        let new_arc = match self.spare.take() {
            Some(arc) => match Arc::try_unwrap(arc) {
                Ok(mut reclaimed) => {
                    // Heap reuse: `MeterSnapshot::clone_from` (manual
                    // impl above) delegates to
                    // `Vec<PidMeter>::clone_from`, which reuses the
                    // outer Vec's allocation and per-PidMeter calls
                    // `PidMeter::clone_from` (manual impl above) so
                    // each PidMeter's inner `channels` Vec and
                    // `cached_label` String reuse their heap too.
                    // Steady-state allocation footprint per publish
                    // is just `Arc::new` (header + payload size); no
                    // inner Vec / String allocations on a stable PMT.
                    reclaimed.clone_from(&self.local);
                    Arc::new(reclaimed)
                }
                Err(_still_held) => {
                    // Display task still holds the guard. Drop the
                    // unreclaimable Arc — display will release it
                    // shortly, but we have only one `spare` slot and
                    // the freshly-swapped Arc (assigned below) is a
                    // strictly better candidate: it just left
                    // `shared` so any reader still hanging on to it
                    // is racing with a frame from before this
                    // publish, which by the time of the next publish
                    // (one audio block ≈ 21 ms later) is overwhelmingly
                    // likely to have been dropped.
                    Arc::new(self.local.clone())
                }
            },
            None => Arc::new(self.local.clone()),
        };
        self.spare = Some(self.shared.swap(new_arc));
        self.publish_count = self.publish_count.wrapping_add(1);
    }
}

/// Mutate the working snapshot in place with new dBFS levels for
/// `pid`, then publish the fresh state via [`MeterPublisher::publish`].
/// The meter task owns the working snapshot exclusively (no lock); the
/// display task observes only the published `Arc<MeterSnapshot>` via
/// `ArcSwap::load`.
///
/// `planar` is one slice per channel. All samples are expected to be in
/// `[-1.0, 1.0]` — anything beyond is clipped at `RED_THRESHOLD_DBFS`.
pub fn update_levels(
    planar: &[Vec<f32>],
    pid: u16,
    codec_label: &'static str,
    publisher: &mut MeterPublisher,
) {
    if planar.is_empty() {
        return;
    }
    let snapshot = &mut publisher.local;
    let now = Instant::now();
    snapshot.generation = snapshot.generation.wrapping_add(1);
    let entry_idx = snapshot.per_pid.iter().position(|p| p.pid == pid);
    let pid_meter = match entry_idx {
        Some(idx) => {
            let p = &mut snapshot.per_pid[idx];
            let codec_changed = p.codec_label != codec_label;
            let prev_ch_count = p.channels.len();
            p.codec_label = codec_label;
            p.channels.resize_with(planar.len(), ChannelLevel::default);
            // Recompute the cached label only when something visible
            // in it actually changed — channel count drives the
            // "1.0/2.0/5.1/7.1/NCH" suffix and the codec label is
            // baked in too. Steady-state (channel count stable) skips
            // the format! and string allocation entirely.
            if prev_ch_count != planar.len() || codec_changed {
                compose_pid_label_into(&mut p.cached_label, pid, codec_label, planar.len());
            }
            p.last_update = now;
            p
        }
        None => {
            let mut cached_label = String::new();
            compose_pid_label_into(&mut cached_label, pid, codec_label, planar.len());
            snapshot.per_pid.push(PidMeter {
                pid,
                codec_label,
                channels: vec![ChannelLevel::default(); planar.len()],
                last_update: now,
                cached_label,
            });
            snapshot.per_pid.sort_by_key(|p| p.pid);
            snapshot
                .per_pid
                .iter_mut()
                .find(|p| p.pid == pid)
                .expect("just inserted")
        }
    };
    for (ch, plane) in pid_meter.channels.iter_mut().zip(planar.iter()) {
        let (peak_dbfs, rms_dbfs) = if plane.is_empty() {
            (DBFS_FLOOR, DBFS_FLOOR)
        } else {
            let mut peak = 0.0f32;
            let mut sum_sq = 0.0f64;
            for &s in plane {
                let a = s.abs();
                if a > peak {
                    peak = a;
                }
                sum_sq += (s as f64) * (s as f64);
            }
            let rms = (sum_sq / plane.len() as f64).sqrt() as f32;
            (amp_to_dbfs(peak), amp_to_dbfs(rms))
        };
        ch.peak_dbfs = peak_dbfs;
        ch.rms_dbfs = rms_dbfs;
        if peak_dbfs >= ch.peak_hold_dbfs || now >= ch.peak_hold_until {
            ch.peak_hold_dbfs = peak_dbfs;
            ch.peak_hold_until = now + PEAK_HOLD_DURATION;
        }
    }
    publisher.publish();
}

/// Zero the levels of any PID whose decoder hasn't produced a block in
/// `stale_after`, leaving the PID itself in the snapshot. The per-block
/// label (`"0x100 AAC 2.0"`) and the empty bar slots stay visible so a
/// brief audio decoder gap doesn't blink the entire confidence strip
/// off and back on — operators see "PID still present, currently
/// silent" instead of "PID vanished". Genuine PID removal is driven by
/// [`remove_pid`] / [`clear_all_pids`] from the PMT/PAT parsers, not
/// by elapsed time.
pub fn silence_stale(publisher: &mut MeterPublisher, stale_after: Duration) {
    let now = Instant::now();
    let mut changed = false;
    for pid in publisher.local.per_pid.iter_mut() {
        if now.duration_since(pid.last_update) < stale_after {
            continue;
        }
        for ch in pid.channels.iter_mut() {
            if ch.rms_dbfs != DBFS_FLOOR
                || ch.peak_dbfs != DBFS_FLOOR
                || ch.peak_hold_dbfs != DBFS_FLOOR
            {
                ch.rms_dbfs = DBFS_FLOOR;
                ch.peak_dbfs = DBFS_FLOOR;
                ch.peak_hold_dbfs = DBFS_FLOOR;
                changed = true;
            }
        }
    }
    if changed {
        publisher.local.generation = publisher.local.generation.wrapping_add(1);
        publisher.publish();
    }
}

/// Remove a single PID from the published snapshot. Called by the meter
/// task when a PMT update drops a PID from the program — i.e. the PID
/// is genuinely gone from the stream, not merely audio-silent.
pub fn remove_pid(publisher: &mut MeterPublisher, pid: u16) {
    let before = publisher.local.per_pid.len();
    publisher.local.per_pid.retain(|p| p.pid != pid);
    if publisher.local.per_pid.len() != before {
        publisher.local.generation = publisher.local.generation.wrapping_add(1);
        publisher.publish();
    }
}

/// Drop every PID from the published snapshot. Called by the meter task
/// on a PAT-driven program change so the new program's PMT can
/// repopulate cleanly without carrying stale entries from the previous
/// program.
pub fn clear_all_pids(publisher: &mut MeterPublisher) {
    if publisher.local.per_pid.is_empty() {
        return;
    }
    publisher.local.per_pid.clear();
    publisher.local.generation = publisher.local.generation.wrapping_add(1);
    publisher.publish();
}

/// Format the cached `"0xNNN CODEC X.Y"` label into `dst` without
/// allocating a fresh `String`. Reuses `dst`'s heap allocation when
/// capacity permits — `String::clear()` keeps the buffer; `write!`
/// extends it. Used by [`update_levels`] when it (re)builds the
/// per-PID cached label, and by tests that pre-format expected
/// values.
fn compose_pid_label_into(dst: &mut String, pid: u16, codec_label: &str, channels_len: usize) {
    use std::fmt::Write;
    dst.clear();
    let _ = match channels_len {
        1 => write!(dst, "0x{pid:X} {codec_label} 1.0"),
        2 => write!(dst, "0x{pid:X} {codec_label} 2.0"),
        6 => write!(dst, "0x{pid:X} {codec_label} 5.1"),
        8 => write!(dst, "0x{pid:X} {codec_label} 7.1"),
        n => write!(dst, "0x{pid:X} {codec_label} {n}CH"),
    };
}

fn amp_to_dbfs(amp: f32) -> f32 {
    if amp <= 0.0 {
        return DBFS_FLOOR;
    }
    let db = 20.0 * amp.log10();
    if db.is_nan() || db < DBFS_FLOOR {
        DBFS_FLOOR
    } else if db > 0.0 {
        0.0
    } else {
        db
    }
}

// ── Rasteriser ────────────────────────────────────────────────────

const BAR_GAP_PX: u32 = 3;
const BAR_MAX_W: u32 = 36;
const BLOCK_GAP_PX: u32 = 16;
const BLOCK_MIN_W: u32 = 80;
const BLOCK_MAX_W: u32 = 200;
const SIDE_MARGIN_PX: u32 = 16;
/// Vertical pixels reserved for the per-block label row. The 5×7
/// glyph is rendered at scale=2, which produces a 14-px-tall bitmap
/// (`5 rows × 2 + (7-1) row-pixels worth of vertical extent` — see
/// `draw_text`). Earlier value of 12 px under-counted by 2, leaving
/// the label and bars touching with no breathing room.
const LABEL_HEIGHT_PX: u32 = 14;
/// Padding above the label, between label and bars, and below the
/// bars — separated so adjustments stay coherent.
const LABEL_TOP_PADDING_PX: u32 = 4;
const LABEL_BARS_GAP_PX: u32 = 2;
const BARS_BOTTOM_PADDING_PX: u32 = 4;
const STRIP_PCT: u32 = 9;
const STRIP_MIN_PX: u32 = 80;
const STRIP_MAX_PX: u32 = 140;

/// Rasterise the meter strip onto the bottom of a packed BGRA buffer.
/// `dst_pitch` is bytes per row (≥ `dst_w * 4`). The translucent strip
/// background is always drawn when the buffer is large enough so the
/// stream-info header (painted separately by [`rasterise_header`])
/// stays legible against the video; the per-PID labels + bars layer on
/// top only once at least one audio PID has decoded a block.
pub fn rasterise(snapshot: &MeterSnapshot, dst: &mut [u8], dst_pitch: usize, dst_w: u32, dst_h: u32) {
    if dst_w < 64 || dst_h < 96 {
        return;
    }
    let strip_h = ((dst_h * STRIP_PCT) / 100).clamp(STRIP_MIN_PX, STRIP_MAX_PX);
    let strip_top = dst_h.saturating_sub(strip_h);

    // 1. Translucent black background under the strip (alpha-blend ~50 %).
    //    Drawn unconditionally so the header (rendered separately on
    //    top) is legible on video-only feeds with no audio PIDs and on
    //    the brief warm-up window before the first audio block decodes.
    blend_strip_background(dst, dst_pitch, dst_w, strip_top, strip_h);

    // No audio PIDs locked yet — leave the strip as background only and
    // skip the divide-by-`pid_count` layout below.
    if snapshot.per_pid.is_empty() {
        return;
    }

    // 2. Per-PID block layout. Each block is sized just wide enough for
    //    its label + bars (capped at BLOCK_MAX_W so a single-PID stream
    //    doesn't sprawl across the whole screen). The row of blocks is
    //    centred horizontally so the strip looks like a confidence
    //    meter rather than a left-aligned monolith.
    let pid_count = snapshot.per_pid.len() as u32;
    let usable_w = dst_w.saturating_sub(SIDE_MARGIN_PX * 2);
    let total_gaps = BLOCK_GAP_PX * pid_count.saturating_sub(1);
    if usable_w <= total_gaps {
        return;
    }
    let auto_block_w = (usable_w - total_gaps) / pid_count;
    let block_w = auto_block_w.clamp(BLOCK_MIN_W, BLOCK_MAX_W);
    if block_w < BLOCK_MIN_W {
        return;
    }
    let total_used = block_w * pid_count + total_gaps;
    let strip_left = SIDE_MARGIN_PX + usable_w.saturating_sub(total_used) / 2;
    let label_y = strip_top + LABEL_TOP_PADDING_PX;
    let bars_top = label_y + LABEL_HEIGHT_PX + LABEL_BARS_GAP_PX;
    let bars_h = strip_h
        .saturating_sub(LABEL_TOP_PADDING_PX + LABEL_HEIGHT_PX + LABEL_BARS_GAP_PX + BARS_BOTTOM_PADDING_PX);
    if bars_h < 16 {
        return;
    }

    let now = Instant::now();
    for (i, pid) in snapshot.per_pid.iter().enumerate() {
        let block_x = strip_left + (block_w + BLOCK_GAP_PX) * (i as u32);

        // Label row: "0xNNN CODEC X.Y" — short enough to fit the
        // capped block width even at scale-2 of the 5×7 font.
        let label = format_pid_label(pid);
        let label_px = label_pixel_width(label);
        let label_x = block_x + block_w.saturating_sub(label_px) / 2;
        draw_text(dst, dst_pitch, dst_w, dst_h, label_x, label_y, label, 0xE0, 0xE0, 0xE0);

        // Channels.
        let ch_count = pid.channels.len() as u32;
        if ch_count == 0 {
            continue;
        }
        let ch_total_gaps = BAR_GAP_PX * ch_count.saturating_sub(1);
        let inner_w = block_w.saturating_sub(4);
        if inner_w <= ch_total_gaps {
            continue;
        }
        let auto_bar_w = (inner_w - ch_total_gaps) / ch_count;
        let bar_w = auto_bar_w.min(BAR_MAX_W);
        if bar_w == 0 {
            continue;
        }
        // Centre the bars horizontally inside the block so the unused
        // slack (when bar_w is capped) sits evenly on either side.
        let bars_total_w = bar_w * ch_count + ch_total_gaps;
        let bars_x_start = block_x + block_w.saturating_sub(bars_total_w) / 2;
        for (ch_idx, ch) in pid.channels.iter().enumerate() {
            let bar_x = bars_x_start + (bar_w + BAR_GAP_PX) * (ch_idx as u32);
            draw_channel_bar(
                dst, dst_pitch, dst_w, dst_h, bar_x, bars_top, bar_w, bars_h, ch, now,
            );
        }
    }
}

/// Strip height (px) for an overlay-plane bars buffer on a panel of the
/// given vertical resolution. Mirrors the math in [`rasterise`] —
/// 9 % of the panel height clamped to `[80, 140]` px — so the existing
/// layout proportions hold whether the bars compose via CPU-blit (the
/// dumb-buffer path) or via a dedicated KMS overlay plane (the VAAPI
/// zero-copy path). Returns `None` when the panel is too small to
/// usefully host bars at all (the rasterisers no-op below 96 px tall).
pub fn compute_strip_height(panel_h: u32) -> Option<u32> {
    if panel_h < 96 {
        return None;
    }
    Some(((panel_h * STRIP_PCT) / 100).clamp(STRIP_MIN_PX, STRIP_MAX_PX))
}

/// Rasterise the meter strip onto a **dedicated overlay buffer** sized
/// exactly to the strip — `dst_w × dst_h` packed ARGB8888, where
/// `dst_h` is the full strip height (use [`compute_strip_height`] to
/// pick the right value relative to the panel). Fills the buffer with
/// a translucent black background (alpha = 0x80) so the underlying
/// primary plane (video) shows through ~50 %, then draws labels + bars
/// at strip-relative coordinates with alpha = 0xFF (fully opaque).
///
/// Drives the KMS overlay-plane composition path on the VAAPI
/// zero-copy display output: the bars compose at vblank in hardware
/// — no CPU blit onto the dumb buffer, no zero-copy demotion when
/// `show_audio_bars` is on. `dst_pitch` is bytes per row (≥
/// `dst_w * 4`). The translucent background is always painted when
/// the buffer is large enough so the stream-info header
/// ([`rasterise_header_overlay`]) stays legible — including on
/// video-only feeds and during the brief warm-up before the first
/// audio block decodes; per-PID labels + bars layer on once
/// at least one PID has reported levels.
pub fn rasterise_overlay(
    snapshot: &MeterSnapshot,
    dst: &mut [u8],
    dst_pitch: usize,
    dst_w: u32,
    dst_h: u32,
) {
    if dst_w < 64 || dst_h < 80 {
        return;
    }
    // Translucent black background. Alpha = 0x80 is the same effective
    // dim as the legacy `blend_strip_background` path achieves by
    // halving every BGR lane in place — but here the kernel does the
    // alpha-blend against the primary plane at scanout, so the dim is
    // applied only to the strip area and only against the live video.
    fill_rect_argb(dst, dst_pitch, dst_w, dst_h, 0, 0, dst_w, dst_h, 0, 0, 0, 0x80);

    // No audio PIDs locked yet — leave the buffer at background-only.
    if snapshot.per_pid.is_empty() {
        return;
    }

    let strip_top: u32 = 0;
    let strip_h = dst_h;
    let pid_count = snapshot.per_pid.len() as u32;
    let usable_w = dst_w.saturating_sub(SIDE_MARGIN_PX * 2);
    let total_gaps = BLOCK_GAP_PX * pid_count.saturating_sub(1);
    if usable_w <= total_gaps {
        return;
    }
    let auto_block_w = (usable_w - total_gaps) / pid_count;
    let block_w = auto_block_w.clamp(BLOCK_MIN_W, BLOCK_MAX_W);
    if block_w < BLOCK_MIN_W {
        return;
    }
    let total_used = block_w * pid_count + total_gaps;
    let strip_left = SIDE_MARGIN_PX + usable_w.saturating_sub(total_used) / 2;
    let label_y = strip_top + LABEL_TOP_PADDING_PX;
    let bars_top = label_y + LABEL_HEIGHT_PX + LABEL_BARS_GAP_PX;
    let bars_h = strip_h
        .saturating_sub(LABEL_TOP_PADDING_PX + LABEL_HEIGHT_PX + LABEL_BARS_GAP_PX + BARS_BOTTOM_PADDING_PX);
    if bars_h < 16 {
        return;
    }

    let now = Instant::now();
    for (i, pid) in snapshot.per_pid.iter().enumerate() {
        let block_x = strip_left + (block_w + BLOCK_GAP_PX) * (i as u32);
        let label = format_pid_label(pid);
        let label_px = label_pixel_width(label);
        let label_x = block_x + block_w.saturating_sub(label_px) / 2;
        draw_text(dst, dst_pitch, dst_w, dst_h, label_x, label_y, label, 0xE0, 0xE0, 0xE0);
        let ch_count = pid.channels.len() as u32;
        if ch_count == 0 {
            continue;
        }
        let ch_total_gaps = BAR_GAP_PX * ch_count.saturating_sub(1);
        let inner_w = block_w.saturating_sub(4);
        if inner_w <= ch_total_gaps {
            continue;
        }
        let auto_bar_w = (inner_w - ch_total_gaps) / ch_count;
        let bar_w = auto_bar_w.min(BAR_MAX_W);
        if bar_w == 0 {
            continue;
        }
        let bars_total_w = bar_w * ch_count + ch_total_gaps;
        let bars_x_start = block_x + block_w.saturating_sub(bars_total_w) / 2;
        for (ch_idx, ch) in pid.channels.iter().enumerate() {
            let bar_x = bars_x_start + (bar_w + BAR_GAP_PX) * (ch_idx as u32);
            draw_channel_bar(
                dst, dst_pitch, dst_w, dst_h, bar_x, bars_top, bar_w, bars_h, ch, now,
            );
        }
    }
}

// ── Stream-info header (broadcast-pro confidence overlay) ─────────
//
// Renders a left-aligned, single-line stream descriptor in the same
// top label row the per-PID audio block labels live in. Lives on the
// strip's left side so the centred audio meter blocks (which are the
// primary information surface) take precedence visually — the header
// auto-truncates to fit the gap before the leftmost block. Drawn in a
// distinct cyan-tinted colour so a glance separates "what is this
// stream" (header) from "how loud is each PID" (block labels +
// bars).

/// Pre-formatted per-frame stream descriptor. The caller composes a
/// single line of "what is this stream" — codec, dims, fps, HDR
/// signal, video / audio PIDs, program — into `text` so the
/// rasteriser does no allocation, parsing, or branching beyond the
/// bitmap-font glyph lookups. Empty `text` renders nothing. The
/// header lives on the strip's left side and auto-truncates to fit
/// the gap before the leftmost audio meter block.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamHeader<'a> {
    pub text: &'a str,
}

/// Strip-relative left-edge X (in pixels) at which the centred audio
/// block region begins, given the snapshot's PID count and the
/// overlay-buffer width. Returns `dst_w` when no audio blocks would
/// be drawn (no PIDs, or PID count too high to fit) — the header
/// then gets the whole strip width to itself.
fn strip_audio_left_edge(snapshot: &MeterSnapshot, dst_w: u32) -> u32 {
    let pid_count = snapshot.per_pid.len() as u32;
    if pid_count == 0 {
        return dst_w;
    }
    let usable_w = dst_w.saturating_sub(SIDE_MARGIN_PX * 2);
    let total_gaps = BLOCK_GAP_PX * pid_count.saturating_sub(1);
    if usable_w <= total_gaps {
        return dst_w;
    }
    let auto_block_w = (usable_w - total_gaps) / pid_count;
    let block_w = auto_block_w.clamp(BLOCK_MIN_W, BLOCK_MAX_W);
    if block_w < BLOCK_MIN_W {
        return dst_w;
    }
    let total_used = block_w * pid_count + total_gaps;
    SIDE_MARGIN_PX + usable_w.saturating_sub(total_used) / 2
}

/// Truncate `text` so its rendered pixel width fits within
/// `avail_px`. Char-aligned (no mid-glyph clip), no ellipsis (would
/// need to budget its own width — keeps the routine alloc-free and
/// branch-light).
fn truncate_to_pixel_width(text: &str, avail_px: u32) -> &str {
    const SCALE: u32 = 2;
    const PER_CHAR_PX: u32 = 5 * SCALE + SCALE; // 12 px (incl. trailing pad)
    if PER_CHAR_PX == 0 {
        return text;
    }
    let max_chars = (avail_px / PER_CHAR_PX) as usize;
    if max_chars == 0 {
        return "";
    }
    if text.chars().count() <= max_chars {
        return text;
    }
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

const HEADER_AUDIO_GAP_PX: u32 = 12; // breathing room before leftmost block

fn draw_header_at(
    header: &StreamHeader,
    dst: &mut [u8],
    dst_pitch: usize,
    dst_w: u32,
    dst_h: u32,
    strip_top: u32,
    strip_left: u32,
) {
    if header.text.is_empty() {
        return;
    }
    let avail_px = strip_left.saturating_sub(SIDE_MARGIN_PX + HEADER_AUDIO_GAP_PX);
    // Need room for at least ~6 chars (e.g. "HEVC"). Anything tighter
    // is unreadable — skip rather than render half a word.
    if avail_px < 72 {
        return;
    }
    let line_y = strip_top + LABEL_TOP_PADDING_PX;
    let trimmed = truncate_to_pixel_width(header.text, avail_px);
    if trimmed.is_empty() {
        return;
    }
    // Warm-amber tint — visually distinct from the white per-block
    // audio labels (bars sit beneath those, so colour-coding header
    // vs. labels lets a confidence-monitor operator separate "what
    // is this stream" from "how loud is each PID" at a glance).
    draw_text(
        dst, dst_pitch, dst_w, dst_h,
        SIDE_MARGIN_PX, line_y, trimmed,
        0xC0, 0xE0, 0xFF,
    );
}

/// Render the stream-info header onto a CPU-blit dumb buffer (the
/// fallback path used when no overlay plane is available). Computes
/// the strip top-edge from the panel height the same way `rasterise`
/// does. Caller invokes this **after** `rasterise` so the centred
/// per-block audio labels stay on top when the header would otherwise
/// crowd them.
pub fn rasterise_header(
    snapshot: &MeterSnapshot,
    header: &StreamHeader,
    dst: &mut [u8],
    dst_pitch: usize,
    dst_w: u32,
    dst_h: u32,
) {
    if header.text.is_empty() || dst_w < 64 || dst_h < 96 {
        return;
    }
    let strip_h = ((dst_h * STRIP_PCT) / 100).clamp(STRIP_MIN_PX, STRIP_MAX_PX);
    let strip_top = dst_h.saturating_sub(strip_h);
    let strip_left = strip_audio_left_edge(snapshot, dst_w);
    draw_header_at(header, dst, dst_pitch, dst_w, dst_h, strip_top, strip_left);
}

/// Render the stream-info header onto the dedicated ARGB8888 overlay
/// buffer (the zero-copy KMS-overlay path). Strip occupies the entire
/// buffer (`strip_top = 0`, `strip_h = dst_h`). Caller invokes this
/// **after** `rasterise_overlay`.
pub fn rasterise_header_overlay(
    snapshot: &MeterSnapshot,
    header: &StreamHeader,
    dst: &mut [u8],
    dst_pitch: usize,
    dst_w: u32,
    dst_h: u32,
) {
    if header.text.is_empty() || dst_w < 64 || dst_h < 80 {
        return;
    }
    let strip_left = strip_audio_left_edge(snapshot, dst_w);
    draw_header_at(header, dst, dst_pitch, dst_w, dst_h, 0, strip_left);
}

/// `fill_rect`'s alpha-aware sibling for the ARGB8888 overlay buffer.
/// `fill_rect` is hard-coded to `0xFF000000 | RGB` because the
/// legacy XRGB8888 dumb-buffer path ignores the alpha lane — but a
/// KMS overlay plane uses the lane to alpha-blend against the
/// primary plane at scanout, so we let the caller pick `a`.
fn fill_rect_argb(
    dst: &mut [u8],
    pitch: usize,
    dst_w: u32,
    dst_h: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    b: u8,
    g: u8,
    r: u8,
    a: u8,
) {
    if x >= dst_w || y >= dst_h {
        return;
    }
    let x_end = (x + w).min(dst_w);
    let y_end = (y + h).min(dst_h);
    let packed: u32 =
        ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
    let bytes = packed.to_le_bytes();
    for row in y..y_end {
        let row_start = (row as usize) * pitch + (x as usize) * 4;
        let row_end = row_start + ((x_end - x) as usize) * 4;
        if row_end > dst.len() {
            break;
        }
        let slice = &mut dst[row_start..row_end];
        let (head, body, tail) = unsafe { slice.align_to_mut::<u32>() };
        body.fill(packed);
        for px in head.chunks_exact_mut(4).chain(tail.chunks_exact_mut(4)) {
            px.copy_from_slice(&bytes);
        }
    }
}

/// Pixel width of `text` rendered by [`draw_text`], at the same 5×7-
/// font scale and inter-glyph padding the rasteriser uses. Each glyph
/// occupies `5 * scale` pixels and is followed by `scale` pixels of
/// padding; the trailing padding is included so a label that exactly
/// fills the block doesn't have its rightmost glyph clipped.
fn label_pixel_width(text: &str) -> u32 {
    const SCALE: u32 = 2;
    const GLYPH_W: u32 = 5 * SCALE;
    let n = text.chars().count() as u32;
    n * (GLYPH_W + SCALE)
}

/// Borrow the cached `"0xNNN CODEC X.Y"` label. The string itself is
/// composed by [`compose_pid_label_into`] inside `update_levels`
/// whenever the channel count or codec label changes — steady-state
/// reads here are zero-allocation.
fn format_pid_label(pid: &PidMeter) -> &str {
    &pid.cached_label
}

fn draw_channel_bar(
    dst: &mut [u8],
    pitch: usize,
    dst_w: u32,
    dst_h: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    ch: &ChannelLevel,
    now: Instant,
) {
    if x + w > dst_w || y + h > dst_h {
        return;
    }
    // Slot frame: dark grey background under the bar — drawn once
    // (full slot) and then overwritten by the colour bands within the
    // RMS fill area.
    fill_rect(dst, pitch, dst_w, dst_h, x, y, w, h, 0x18, 0x18, 0x18);

    let rms_frac = dbfs_to_frac(ch.rms_dbfs);
    let peak_hold_frac = if now < ch.peak_hold_until {
        dbfs_to_frac(ch.peak_hold_dbfs)
    } else {
        rms_frac
    };
    let fill_h = ((h as f32) * rms_frac) as u32;
    let bar_top = y + h.saturating_sub(fill_h);
    let bar_x = x + 1;
    let bar_w = w.saturating_sub(2);

    // Pre-compute the two colour-band boundaries once. In screen-row
    // order (smaller y is higher on screen):
    //   y          ──── red   ────────────────  ← bar slot top
    //   red_split  ──── yellow ───────────────
    //   yel_split  ──── green  ───────────────
    //   y + h      ────────────────────────────  ← bar slot bottom
    //
    // `red_split` is the boundary between red above and yellow below,
    // `yel_split` between yellow above and green below. The fill spans
    // `[bar_top, bar_bottom)`; we paint each band as the intersection
    // of its row range with the fill range — single `fill_rect` per
    // band, no per-row work.
    let bar_bottom = y + h;
    let red_split = y + h.saturating_sub(((h as f32) * dbfs_to_frac(RED_THRESHOLD_DBFS)) as u32);
    let yel_split = y + h.saturating_sub(((h as f32) * dbfs_to_frac(YELLOW_THRESHOLD_DBFS)) as u32);

    // Red band: [bar_top, min(bar_bottom, red_split)).
    let red_end = bar_bottom.min(red_split);
    if red_end > bar_top {
        fill_rect(
            dst, pitch, dst_w, dst_h,
            bar_x, bar_top, bar_w, red_end - bar_top,
            0x30, 0x30, 0xF0,
        );
    }
    // Yellow band: [max(bar_top, red_split), min(bar_bottom, yel_split)).
    let yellow_start = bar_top.max(red_split);
    let yellow_end = bar_bottom.min(yel_split);
    if yellow_end > yellow_start {
        fill_rect(
            dst, pitch, dst_w, dst_h,
            bar_x, yellow_start, bar_w, yellow_end - yellow_start,
            0x30, 0xC8, 0xF0,
        );
    }
    // Green band: [max(bar_top, yel_split), bar_bottom).
    let green_start = bar_top.max(yel_split);
    if bar_bottom > green_start {
        fill_rect(
            dst, pitch, dst_w, dst_h,
            bar_x, green_start, bar_w, bar_bottom - green_start,
            0x30, 0xE0, 0x30,
        );
    }
    // Peak-hold tick: 2-px white line at peak_hold_frac of the bar.
    let hold_pixels = ((h as f32) * peak_hold_frac) as u32;
    if hold_pixels >= 1 {
        let hold_y = y + h.saturating_sub(hold_pixels);
        let tick_h = 2.min(h);
        let tick_h_clamped = (hold_y + tick_h).min(bar_bottom).saturating_sub(hold_y);
        if tick_h_clamped > 0 {
            fill_rect(
                dst,
                pitch,
                dst_w,
                dst_h,
                bar_x,
                hold_y,
                bar_w,
                tick_h_clamped,
                0xFF,
                0xFF,
                0xFF,
            );
        }
    }
}

/// Map a dBFS value to a `[0, 1]` fraction of bar height.
/// Curve: linear from -60 dBFS (= 0) to 0 dBFS (= 1). Anything below
/// -60 dBFS folds to 0; above 0 dBFS clamps to 1.
fn dbfs_to_frac(dbfs: f32) -> f32 {
    let clipped = dbfs.clamp(-60.0, 0.0);
    (clipped + 60.0) / 60.0
}

// ── Drawing primitives ────────────────────────────────────────────

fn blend_strip_background(dst: &mut [u8], pitch: usize, dst_w: u32, y: u32, h: u32) {
    let row_bytes = (dst_w as usize) * 4;
    // Halve B/G/R in one u32-wide bitwise op per pixel: mask the low
    // bit of each 8-bit lane (so the right shift doesn't bleed across
    // lane boundaries), shift right by 1, then re-stamp the alpha lane
    // back to `0xFF`. Roughly 4× the throughput of the byte-by-byte
    // divide loop the compiler emitted before, and it lets the
    // optimiser auto-vectorise the row across XMM/YMM registers.
    const HALVE_MASK: u32 = 0xFEFE_FEFE;
    const ALPHA_FF: u32 = 0xFF00_0000;
    for row in y..(y + h) {
        let row_start = (row as usize) * pitch;
        if row_start + row_bytes > dst.len() {
            break;
        }
        let row_slice = &mut dst[row_start..row_start + row_bytes];
        // SAFETY: the row was sliced to a multiple of 4 bytes; XRGB8888
        // dumb buffers are u32-aligned (allocator returns page-aligned
        // memory and pitch is always a multiple of 4).
        let (head, body, tail) = unsafe { row_slice.align_to_mut::<u32>() };
        debug_assert!(head.is_empty() && tail.is_empty(),
            "BGRA row was not u32-aligned");
        for word in body {
            *word = ((*word & HALVE_MASK) >> 1) | ALPHA_FF;
        }
        // Cover the corner case where the slice wasn't u32-aligned at
        // either end (shouldn't happen on real KMS dumb buffers, but we
        // keep the byte fallback so a hand-constructed test buffer still
        // renders correctly).
        for px in head.chunks_exact_mut(4).chain(tail.chunks_exact_mut(4)) {
            px[0] >>= 1;
            px[1] >>= 1;
            px[2] >>= 1;
        }
    }
}

fn fill_rect(
    dst: &mut [u8],
    pitch: usize,
    dst_w: u32,
    dst_h: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    b: u8,
    g: u8,
    r: u8,
) {
    if x >= dst_w || y >= dst_h {
        return;
    }
    let x_end = (x + w).min(dst_w);
    let y_end = (y + h).min(dst_h);
    for row in y..y_end {
        fill_row(dst, pitch, dst_w, x, row, x_end - x, (b, g, r));
    }
}

fn fill_row(dst: &mut [u8], pitch: usize, dst_w: u32, x: u32, y: u32, w: u32, color: (u8, u8, u8)) {
    if x >= dst_w {
        return;
    }
    let w_clamped = w.min(dst_w - x);
    let row_start = (y as usize) * pitch + (x as usize) * 4;
    let row_end = row_start + (w_clamped as usize) * 4;
    if row_end > dst.len() {
        return;
    }
    let slice = &mut dst[row_start..row_end];
    // Pre-pack the BGRA as a single u32 and use slice::fill — turns the
    // per-pixel 4-byte write loop into a single rep-stosd / SIMD memset
    // on the XRGB8888 dumb buffer.
    let packed: u32 =
        (0xFFu32 << 24) | ((color.2 as u32) << 16) | ((color.1 as u32) << 8) | (color.0 as u32);
    let (head, body, tail) = unsafe { slice.align_to_mut::<u32>() };
    body.fill(packed);
    let bytes = packed.to_le_bytes();
    for px in head.chunks_exact_mut(4).chain(tail.chunks_exact_mut(4)) {
        px.copy_from_slice(&bytes);
    }
}

// ── 5×7 bitmap font (printable ASCII subset for PID labels) ───────
//
// Each glyph is 7 bytes; bit 4 (0x10) = leftmost column, bit 0 (0x01)
// = column 5 (unused). Public-domain "tom-thumb" / IBM-like 5×7
// characters covering [A-Z 0-9 ' ' '.' 'x' '/' '-' ':' '_' '(' ')'].

#[rustfmt::skip]
fn glyph_5x7(c: char) -> [u8; 7] {
    match c {
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x08],
        ':' => [0x00, 0x04, 0x00, 0x00, 0x04, 0x00, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x1E, 0x00, 0x00, 0x00],
        '/' => [0x01, 0x02, 0x04, 0x08, 0x10, 0x00, 0x00],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        'x' | 'X' => [0x00, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x00],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x06, 0x08, 0x10, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    }
}

/// Render an ASCII string into the BGRA buffer at `(x, y)` using the
/// 5×7 font scaled 1.5× to ~7×10 pixels per glyph (rounded up). Each
/// glyph is followed by 1 px of padding.
fn draw_text(
    dst: &mut [u8],
    pitch: usize,
    dst_w: u32,
    dst_h: u32,
    x: u32,
    y: u32,
    text: &str,
    b: u8,
    g: u8,
    r: u8,
) {
    let scale = 2u32; // 5×7 → 10×14, comfortably readable on 1080p+
    let glyph_w = 5 * scale;
    let mut cx = x;
    for ch in text.chars() {
        let upper = ch.to_ascii_uppercase();
        let bits = glyph_5x7(upper);
        if cx + glyph_w > dst_w {
            break;
        }
        for (row_idx, row_bits) in bits.iter().enumerate() {
            for col in 0..5u32 {
                if (row_bits >> (4 - col)) & 1 == 1 {
                    let px_x = cx + col * scale;
                    let px_y = y + (row_idx as u32) * scale;
                    fill_rect(dst, pitch, dst_w, dst_h, px_x, px_y, scale, scale, b, g, r);
                }
            }
        }
        cx += glyph_w + scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silent_block(channels: usize, samples: usize) -> Vec<Vec<f32>> {
        vec![vec![0.0f32; samples]; channels]
    }

    fn full_scale_block(channels: usize, samples: usize) -> Vec<Vec<f32>> {
        vec![vec![1.0f32; samples]; channels]
    }

    /// Drive `update_levels` against a freshly-created publisher and
    /// return it. Mirrors the meter task's pattern from
    /// `audio_meter::run_meter` (single `MeterPublisher` owning both
    /// the working snapshot and the SharedMeter).
    fn drive(planar: &[Vec<f32>], pid: u16, codec: &'static str) -> MeterPublisher {
        let mut publisher = MeterPublisher::new(new_shared_meter());
        update_levels(planar, pid, codec, &mut publisher);
        publisher
    }

    #[test]
    fn dbfs_clamp_silence() {
        let p = drive(&silent_block(2, 1024), 0x100, "AAC");
        let s = &p.local;
        assert_eq!(s.per_pid.len(), 1);
        assert_eq!(s.per_pid[0].channels.len(), 2);
        for ch in &s.per_pid[0].channels {
            assert_eq!(ch.peak_dbfs, DBFS_FLOOR);
            assert_eq!(ch.rms_dbfs, DBFS_FLOOR);
        }
    }

    #[test]
    fn dbfs_full_scale_reads_zero() {
        let p = drive(&full_scale_block(1, 256), 0x100, "AAC");
        let s = &p.local;
        assert_eq!(s.per_pid[0].channels[0].peak_dbfs, 0.0);
        assert!(s.per_pid[0].channels[0].rms_dbfs.abs() < 0.01);
    }

    #[test]
    fn peak_hold_decay() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&full_scale_block(1, 64), 0x100, "AAC", &mut p);
        let hold_until_first = p.local.per_pid[0].channels[0].peak_hold_until;
        update_levels(&silent_block(1, 64), 0x100, "AAC", &mut p);
        assert_eq!(p.local.per_pid[0].channels[0].peak_hold_dbfs, 0.0);
        assert_eq!(p.local.per_pid[0].channels[0].peak_hold_until, hold_until_first);
    }

    #[test]
    fn multi_pid_sorted() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(2, 64), 0x200, "AC3", &mut p);
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        assert_eq!(p.local.per_pid.len(), 2);
        assert_eq!(p.local.per_pid[0].pid, 0x100);
        assert_eq!(p.local.per_pid[1].pid, 0x200);
    }

    #[test]
    fn silence_stale_zeroes_levels_keeping_pid_visible() {
        // Confidence-monitor convention: when the decoder briefly stops
        // producing blocks (audio gap, decoder lag, lagged broadcast),
        // the label stays visible and the bars sink to silence rather
        // than the whole block vanishing.
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&full_scale_block(2, 64), 0x100, "AAC", &mut p);
        // Confirm we have non-floor levels before going stale.
        assert!(p.local.per_pid[0].channels[0].rms_dbfs > DBFS_FLOOR);
        let baseline_label = p.local.per_pid[0].cached_label.clone();
        let before_publishes = p.publish_count();

        // Backdate so the PID is "stale" by silence_stale's reckoning.
        p.local.per_pid[0].last_update = Instant::now() - Duration::from_secs(10);
        silence_stale(&mut p, Duration::from_secs(5));

        // PID still present.
        assert_eq!(p.local.per_pid.len(), 1);
        assert_eq!(p.local.per_pid[0].pid, 0x100);
        assert_eq!(p.local.per_pid[0].cached_label, baseline_label);
        // Levels zeroed to the dBFS floor on every channel.
        for ch in &p.local.per_pid[0].channels {
            assert_eq!(ch.rms_dbfs, DBFS_FLOOR);
            assert_eq!(ch.peak_dbfs, DBFS_FLOOR);
            assert_eq!(ch.peak_hold_dbfs, DBFS_FLOOR);
        }
        // Snapshot got republished exactly once for the transition.
        assert_eq!(p.publish_count(), before_publishes + 1);

        // A second call is idempotent — already-silent PIDs don't
        // trigger spurious publishes that would churn the display
        // task's ArcSwap reads for no visible change.
        silence_stale(&mut p, Duration::from_secs(5));
        assert_eq!(p.publish_count(), before_publishes + 1);
    }

    #[test]
    fn silence_stale_skips_fresh_pids() {
        // Recently-updated PIDs aren't touched — the bars stay at their
        // live levels even while a sibling PID has gone silent.
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&full_scale_block(2, 64), 0x100, "AAC", &mut p);
        let rms_before = p.local.per_pid[0].channels[0].rms_dbfs;
        silence_stale(&mut p, Duration::from_secs(5));
        assert_eq!(p.local.per_pid[0].channels[0].rms_dbfs, rms_before);
    }

    #[test]
    fn remove_pid_drops_from_snapshot() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        update_levels(&silent_block(2, 64), 0x101, "AC3", &mut p);
        remove_pid(&mut p, 0x100);
        assert_eq!(p.local.per_pid.len(), 1);
        assert_eq!(p.local.per_pid[0].pid, 0x101);
        // Removing a PID that's not in the snapshot is a no-op (no
        // spurious publish).
        let before = p.publish_count();
        remove_pid(&mut p, 0x999);
        assert_eq!(p.publish_count(), before);
    }

    #[test]
    fn clear_all_pids_empties_snapshot() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        update_levels(&silent_block(2, 64), 0x101, "AC3", &mut p);
        clear_all_pids(&mut p);
        assert!(p.local.per_pid.is_empty());
        // Idempotent — clearing an empty snapshot doesn't republish.
        let before = p.publish_count();
        clear_all_pids(&mut p);
        assert_eq!(p.publish_count(), before);
    }

    #[test]
    fn silenced_pid_still_renders_label_and_empty_bars() {
        // Verifies the user-visible payoff of silence_stale: with a
        // single silent PID the overlay buffer still gets labels +
        // empty bar slots drawn, not just background. Previously, a
        // stale-pruned snapshot fell through to background-only.
        let w: u32 = 1920;
        let h: u32 = 140;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&full_scale_block(2, 64), 0x100, "AAC", &mut p);
        p.local.per_pid[0].last_update = Instant::now() - Duration::from_secs(10);
        silence_stale(&mut p, Duration::from_secs(5));
        rasterise_overlay(&p.local, &mut buf, pitch, w, h);
        // The label row should still carry fully-opaque text pixels
        // somewhere in the centred block region — silence_stale kept
        // the PID, so rasterise_overlay drew labels + slot frames.
        let probe_y = (LABEL_TOP_PADDING_PX + 6) as usize;
        let row_start = probe_y * pitch;
        let row_end = row_start + (w as usize) * 4;
        let any_fully_opaque = buf[row_start..row_end]
            .chunks_exact(4)
            .any(|px| px[3] == 0xFF);
        assert!(
            any_fully_opaque,
            "silenced PID must still render label pixels, not background-only"
        );
    }

    #[test]
    fn shared_load_returns_latest_snapshot() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&full_scale_block(2, 32), 0x100, "AAC", &mut p);
        let observed = p.shared.load();
        assert_eq!(observed.per_pid.len(), 1);
        assert_eq!(observed.per_pid[0].pid, 0x100);
    }

    /// Cached label is composed once at PID creation and reused on
    /// subsequent updates while the channel count stays the same. The
    /// rasterise path reads `&pid.cached_label` so this is the
    /// allocation that #2 in the audio-bars review eliminated.
    #[test]
    fn cached_label_populated_and_reused() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        assert_eq!(p.local.per_pid[0].cached_label, "0x100 AAC 2.0");
        // Same PID, same channel count, same codec — the cached label
        // should not be recomposed (we can't observe the lack-of-alloc
        // directly, but we can pin the value: it stays exactly equal).
        let pre_addr = p.local.per_pid[0].cached_label.as_ptr();
        let pre_cap = p.local.per_pid[0].cached_label.capacity();
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        let post_addr = p.local.per_pid[0].cached_label.as_ptr();
        let post_cap = p.local.per_pid[0].cached_label.capacity();
        // Same allocation — String wasn't touched.
        assert_eq!(pre_addr, post_addr);
        assert_eq!(pre_cap, post_cap);
        assert_eq!(p.local.per_pid[0].cached_label, "0x100 AAC 2.0");
    }

    /// When the channel count changes (PMT update mid-stream), the
    /// cached label is recomposed with the new "X.Y" suffix.
    #[test]
    fn cached_label_updates_on_channel_count_change() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        assert_eq!(p.local.per_pid[0].cached_label, "0x100 AAC 2.0");
        update_levels(&silent_block(6, 64), 0x100, "AAC", &mut p);
        assert_eq!(p.local.per_pid[0].cached_label, "0x100 AAC 5.1");
    }

    /// Channel counts not in the broadcast preset list (1.0 / 2.0 / 5.1
    /// / 7.1) fall back to the "NCH" suffix so a 3-channel stream
    /// still gets a meaningful label.
    #[test]
    fn cached_label_handles_uncommon_channel_counts() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(3, 64), 0x100, "AAC", &mut p);
        assert_eq!(p.local.per_pid[0].cached_label, "0x100 AAC 3CH");
    }

    /// Allocation-reuse path: when no reader holds a guard, the
    /// publisher reclaims a previously-published Arc via
    /// `Arc::try_unwrap` and reuses its **inner** heap allocations
    /// (the `channels: Vec<ChannelLevel>` and `cached_label: String`
    /// on each PidMeter) — `Arc::new` itself still allocates the
    /// header per publish, but the payload Vecs/Strings keep their
    /// heap.
    ///
    /// Verification: take address of the inner `channels` Vec at
    /// publish 1, do enough additional publishes that the publisher's
    /// `spare` slot rotates around to hold publish 1's Arc again
    /// (publish 2 → spare = empty default; publish 3 → spare =
    /// publish 1's Arc, reclaim fires, channels heap reused).
    #[test]
    fn publisher_reuses_inner_heap_when_reader_idle() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p); // publish 1
        let publish1_channels_addr = {
            let arc = p.shared.load_full();
            let ptr = arc.per_pid[0].channels.as_ptr() as usize;
            // arc dropped here so the published refcount is 1 again
            ptr
        };
        let publish1_label_addr = {
            let arc = p.shared.load_full();
            arc.per_pid[0].cached_label.as_ptr() as usize
        };
        // Publish 2: spare was the empty-default Arc, so it gets
        // reclaimed but `clone_from` has to grow its empty per_pid
        // Vec — fresh inner allocations on this round.
        update_levels(&full_scale_block(2, 64), 0x100, "AAC", &mut p);
        // Publish 3: spare is now publish-1's Arc (refcount 1, no
        // reader). `try_unwrap` succeeds; `clone_from` does in-place
        // overwrite on the matching PID, reusing publish-1's inner
        // heap allocations.
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        let arc3 = p.shared.load_full();
        let publish3_channels_addr = arc3.per_pid[0].channels.as_ptr() as usize;
        let publish3_label_addr = arc3.per_pid[0].cached_label.as_ptr() as usize;
        assert_eq!(
            publish1_channels_addr, publish3_channels_addr,
            "expected channels Vec heap to be reused via try_unwrap reclaim"
        );
        assert_eq!(
            publish1_label_addr, publish3_label_addr,
            "expected cached_label String heap to be reused via try_unwrap reclaim"
        );
    }

    /// Fallback path: if a reader holds a load guard when the
    /// publisher tries to reclaim, `try_unwrap` fails. The held Arc is
    /// dropped (the publisher only has one spare slot and the
    /// freshly-swapped Arc is a better candidate) — but reclaim
    /// resumes from the next publish onward.
    #[test]
    fn publisher_holds_spare_when_reader_blocks_reclaim() {
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p); // publish 1
        // Reader pins publish 1's Arc.
        let held_guard = p.shared.load_full();
        let publish1_channels_addr = held_guard.per_pid[0].channels.as_ptr() as usize;

        // Publish 2: spare = empty-default Arc; try_unwrap on it
        // succeeds (held_guard pins publish 1, not the empty default).
        // The new Arc inherits the empty-default's outer Vec heap.
        // After publish 2: spare = publish 1's Arc (held_guard pins
        // it too).
        update_levels(&full_scale_block(2, 64), 0x100, "AAC", &mut p);
        let publish2_channels_addr = {
            let arc = p.shared.load_full();
            arc.per_pid[0].channels.as_ptr() as usize
        };

        // Publish 3: spare = publish 1's Arc, but held_guard still
        // pins it → try_unwrap fails. The old spare Arc is dropped
        // (publish 1's heap will be freed once the test drops
        // held_guard); a fresh Arc is allocated. swap returns
        // publish 2's Arc (refcount 1, no reader pins it), which
        // becomes the new spare.
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        let publish3_channels_addr = {
            let arc = p.shared.load_full();
            arc.per_pid[0].channels.as_ptr() as usize
        };
        assert_ne!(
            publish1_channels_addr, publish3_channels_addr,
            "expected fresh channels heap while reader pins the spare Arc"
        );

        drop(held_guard);

        // Publish 4: spare = publish 2's Arc, refcount 1. Reclaim
        // fires; publish 2's inner heap is reused — *not* publish 1's
        // (that was dropped in the failed reclaim above).
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        let publish4_channels_addr = {
            let arc = p.shared.load_full();
            arc.per_pid[0].channels.as_ptr() as usize
        };
        assert_eq!(
            publish2_channels_addr, publish4_channels_addr,
            "expected reclaim to fire on the freshly-swapped Arc once the reader dropped"
        );
    }

    #[test]
    fn rasterise_does_not_panic_on_small_buffer() {
        // A 1280×720 BGRA buffer.
        let w: u32 = 1280;
        let h: u32 = 720;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&full_scale_block(2, 64), 0x100, "AAC", &mut p);
        update_levels(&silent_block(6, 64), 0x101, "AC3", &mut p);
        let s = p.local.clone();
        rasterise(&s, &mut buf, pitch, w, h);
        // Some row inside the strip (top 12 % of dst_h is the strip)
        // should have BGRA pixels written by the rasteriser.
        let strip_top = (h as usize) - 96; // strip_h is clamped to >= 96
        let probe_row = strip_top + 60; // well inside the bars area
        let row_start = probe_row * pitch;
        let any_pixel_set = buf[row_start..row_start + pitch]
            .chunks_exact(4)
            .any(|px| px[3] == 0xFF);
        assert!(any_pixel_set, "expected rasterised pixels inside the strip");
    }

    #[test]
    fn header_truncation_respects_pixel_width() {
        // Header is rendered only when there's at least 72 px of room
        // before the leftmost audio block. Too-tight scenarios skip it
        // entirely (no half-word render).
        assert_eq!(truncate_to_pixel_width("HEVC 1920x1080", 72), "HEVC 1");
        // 12 px per char including trailing pad → 6 chars × 12 = 72.
        assert_eq!(truncate_to_pixel_width("HEVC 1920x1080", 36), "HEV");
        // Empty avail → empty result.
        assert_eq!(truncate_to_pixel_width("HEVC", 0), "");
        // No truncation when text fits.
        assert_eq!(truncate_to_pixel_width("OK", 1000), "OK");
    }

    #[test]
    fn header_overlay_writes_pixels_with_room() {
        // Single-PID stereo on a 1920×140 ARGB strip leaves ~720 px on
        // the left for the header — plenty for a "HEVC 1920x1080 50p"
        // line.
        let w: u32 = 1920;
        let h: u32 = 140;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let mut p = MeterPublisher::new(new_shared_meter());
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        let snap = p.local.clone();
        rasterise_overlay(&snap, &mut buf, pitch, w, h);
        let header = StreamHeader { text: "HEVC 1920x1080 50p HDR-PQ V:0x100 A:0x101" };
        rasterise_header_overlay(&snap, &header, &mut buf, pitch, w, h);
        // Probe the header label-row (LABEL_TOP_PADDING_PX=4, 14 px tall)
        // on the LEFT side. Must contain at least one fully-opaque
        // ARGB pixel — proves header was drawn.
        let probe_y = (LABEL_TOP_PADDING_PX + 6) as usize;
        let row_start = probe_y * pitch + (SIDE_MARGIN_PX as usize) * 4;
        let row_end = row_start + 200 * 4; // first 200 px of the row
        let any_alpha_full = buf[row_start..row_end]
            .chunks_exact(4)
            .any(|px| px[3] == 0xFF);
        assert!(
            any_alpha_full,
            "expected header text alpha pixels in left margin row"
        );
    }

    #[test]
    fn header_overlay_skips_when_no_room() {
        // Many PIDs leave too little space on the left — header skips
        // rather than crowd the audio block labels.
        let w: u32 = 320;
        let h: u32 = 100;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let mut p = MeterPublisher::new(new_shared_meter());
        // 3 PIDs at 320 px wide → blocks crowd the strip.
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut p);
        update_levels(&silent_block(2, 64), 0x101, "AAC", &mut p);
        update_levels(&silent_block(6, 64), 0x102, "AC3", &mut p);
        let snap = p.local.clone();
        let header = StreamHeader { text: "HEVC 1920x1080 50p" };
        // Capture the buffer state after only the bars rasterise so we
        // can compare what the header rasterise added (or didn't).
        rasterise_overlay(&snap, &mut buf, pitch, w, h);
        let after_bars = buf.clone();
        rasterise_header_overlay(&snap, &header, &mut buf, pitch, w, h);
        // Header should be a no-op on this geometry.
        assert_eq!(after_bars, buf, "header should skip when avail_w too tight");
        let _ = after_bars;
    }

    #[test]
    fn rasterise_empty_snapshot_paints_bg_only() {
        // Empty per_pid: rows above the strip stay untouched, but the
        // strip area gets the translucent-black background so the
        // separately-rendered stream header remains legible against
        // arbitrary video on video-only feeds.
        let w: u32 = 640;
        let h: u32 = 480;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let snap = MeterSnapshot::default();
        rasterise(&snap, &mut buf, pitch, w, h);

        let strip_h = ((h * STRIP_PCT) / 100).clamp(STRIP_MIN_PX, STRIP_MAX_PX);
        let strip_top = (h - strip_h) as usize;
        // Every row above the strip must remain at the original zero fill.
        let above = strip_top * pitch;
        assert!(
            buf[..above].iter().all(|&b| b == 0),
            "rows above the strip must remain untouched"
        );
        // Inside the strip the background blend re-stamps alpha=0xFF on
        // every pixel (the BGR halve is a no-op on the zero source).
        let probe_y = strip_top + 4;
        let probe_row = &buf[probe_y * pitch..probe_y * pitch + (w as usize) * 4];
        let alpha_ff = probe_row.chunks_exact(4).all(|px| px[3] == 0xFF);
        assert!(alpha_ff, "strip background should leave alpha=0xFF");
    }

    #[test]
    fn rasterise_overlay_empty_snapshot_paints_bg_only() {
        // Mirror coverage for the dedicated overlay-plane path: empty
        // per_pid still fills the ARGB buffer with the translucent
        // background so header text stays legible.
        let w: u32 = 1920;
        let h: u32 = 140;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let snap = MeterSnapshot::default();
        rasterise_overlay(&snap, &mut buf, pitch, w, h);
        // Probe a pixel inside the strip — alpha lane should be 0x80.
        let probe = &buf[pitch + 64..pitch + 68];
        assert_eq!(probe[3], 0x80, "overlay strip bg should set alpha=0x80");
    }
}
