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

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct PidMeter {
    pub pid: u16,
    pub codec_label: &'static str,
    pub channels: Vec<ChannelLevel>,
    pub last_update: Instant,
}

#[derive(Clone, Debug, Default)]
pub struct MeterSnapshot {
    pub per_pid: Vec<PidMeter>,
    pub generation: u64,
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

/// Mutate `snapshot` in place with new dBFS levels for `pid`, then
/// publish the fresh state via `shared`. The meter task owns
/// `snapshot` exclusively (no lock); the display task observes only
/// the published `Arc<MeterSnapshot>` via `ArcSwap::load`.
///
/// `planar` is one slice per channel. All samples are expected to be in
/// `[-1.0, 1.0]` — anything beyond is clipped at `RED_THRESHOLD_DBFS`.
pub fn update_levels(
    planar: &[Vec<f32>],
    pid: u16,
    codec_label: &'static str,
    snapshot: &mut MeterSnapshot,
    shared: &SharedMeter,
) {
    if planar.is_empty() {
        return;
    }
    let now = Instant::now();
    snapshot.generation = snapshot.generation.wrapping_add(1);
    let entry_idx = snapshot.per_pid.iter().position(|p| p.pid == pid);
    let pid_meter = match entry_idx {
        Some(idx) => {
            let p = &mut snapshot.per_pid[idx];
            p.codec_label = codec_label;
            p.channels.resize_with(planar.len(), ChannelLevel::default);
            p.last_update = now;
            p
        }
        None => {
            snapshot.per_pid.push(PidMeter {
                pid,
                codec_label,
                channels: vec![ChannelLevel::default(); planar.len()],
                last_update: now,
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
    shared.store(Arc::new(snapshot.clone()));
}

/// Drop PIDs whose decoder hasn't produced a block in `stale_after`.
/// Called periodically by the meter task so the overlay reflects the
/// current PMT (e.g. an audio PID that vanished from the program list).
pub fn prune_stale(snapshot: &mut MeterSnapshot, shared: &SharedMeter, stale_after: Duration) {
    let now = Instant::now();
    let before = snapshot.per_pid.len();
    snapshot
        .per_pid
        .retain(|p| now.duration_since(p.last_update) < stale_after);
    if snapshot.per_pid.len() != before {
        shared.store(Arc::new(snapshot.clone()));
    }
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

const BAR_GAP_PX: u32 = 2;
const BAR_MAX_W: u32 = 28;
const BLOCK_GAP_PX: u32 = 12;
const BLOCK_MIN_W: u32 = 48;
const BLOCK_MAX_W: u32 = 240;
const SIDE_MARGIN_PX: u32 = 12;
const LABEL_HEIGHT_PX: u32 = 12; // 5×7 font scaled 1.5×
const STRIP_PCT: u32 = 9;
const STRIP_MIN_PX: u32 = 80;
const STRIP_MAX_PX: u32 = 140;

/// Rasterise the meter strip onto the bottom of a packed BGRA buffer.
/// `dst_pitch` is bytes per row (≥ `dst_w * 4`). Writes nothing when the
/// snapshot has no PIDs (so the operator sees the bare picture instead
/// of a half-empty strip while audio is still warming up).
pub fn rasterise(snapshot: &MeterSnapshot, dst: &mut [u8], dst_pitch: usize, dst_w: u32, dst_h: u32) {
    if snapshot.per_pid.is_empty() || dst_w < 64 || dst_h < 96 {
        return;
    }
    let strip_h = ((dst_h * STRIP_PCT) / 100).clamp(STRIP_MIN_PX, STRIP_MAX_PX);
    let strip_top = dst_h.saturating_sub(strip_h);

    // 1. Translucent black background under the strip (alpha-blend ~50 %).
    blend_strip_background(dst, dst_pitch, dst_w, strip_top, strip_h);

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
    let bars_top = strip_top + LABEL_HEIGHT_PX + 2;
    let bars_h = strip_h.saturating_sub(LABEL_HEIGHT_PX + 4);
    if bars_h < 16 {
        return;
    }

    let now = Instant::now();
    for (i, pid) in snapshot.per_pid.iter().enumerate() {
        let block_x = strip_left + (block_w + BLOCK_GAP_PX) * (i as u32);

        // Label row: "0xNNN CODEC X.Y" — short enough to fit the
        // capped block width even at scale-2 of the 5×7 font.
        let label = format_pid_label(pid);
        let label_px = label_pixel_width(&label);
        let label_x = block_x + block_w.saturating_sub(label_px) / 2;
        draw_text(dst, dst_pitch, dst_w, dst_h, label_x, strip_top + 2, &label, 0xE0, 0xE0, 0xE0);

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

fn format_pid_label(pid: &PidMeter) -> String {
    let ch_label = match pid.channels.len() {
        1 => "1.0".to_string(),
        2 => "2.0".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        n => format!("{n}CH"),
    };
    // Compact "0xNNN CODEC X.Y" — fits the capped block width
    // (BLOCK_MAX_W = 240 px) at the rasteriser's 5×7 font scale.
    format!("0x{:X} {} {}", pid.pid, pid.codec_label, ch_label)
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

    /// Drive `update_levels` against a freshly-created meter pair and
    /// return the published snapshot. Mirrors the meter task's
    /// (mut local + shared publish) pattern from `audio_meter::run_meter`.
    fn drive(planar: &[Vec<f32>], pid: u16, codec: &'static str) -> (MeterSnapshot, SharedMeter) {
        let shared = new_shared_meter();
        let mut local = MeterSnapshot::default();
        update_levels(planar, pid, codec, &mut local, &shared);
        (local, shared)
    }

    #[test]
    fn dbfs_clamp_silence() {
        let (s, _shared) = drive(&silent_block(2, 1024), 0x100, "AAC");
        assert_eq!(s.per_pid.len(), 1);
        assert_eq!(s.per_pid[0].channels.len(), 2);
        for ch in &s.per_pid[0].channels {
            assert_eq!(ch.peak_dbfs, DBFS_FLOOR);
            assert_eq!(ch.rms_dbfs, DBFS_FLOOR);
        }
    }

    #[test]
    fn dbfs_full_scale_reads_zero() {
        let (s, _shared) = drive(&full_scale_block(1, 256), 0x100, "AAC");
        assert_eq!(s.per_pid[0].channels[0].peak_dbfs, 0.0);
        assert!(s.per_pid[0].channels[0].rms_dbfs.abs() < 0.01);
    }

    #[test]
    fn peak_hold_decay() {
        let shared = new_shared_meter();
        let mut local = MeterSnapshot::default();
        update_levels(&full_scale_block(1, 64), 0x100, "AAC", &mut local, &shared);
        let hold_until_first = local.per_pid[0].channels[0].peak_hold_until;
        update_levels(&silent_block(1, 64), 0x100, "AAC", &mut local, &shared);
        assert_eq!(local.per_pid[0].channels[0].peak_hold_dbfs, 0.0);
        assert_eq!(local.per_pid[0].channels[0].peak_hold_until, hold_until_first);
    }

    #[test]
    fn multi_pid_sorted() {
        let shared = new_shared_meter();
        let mut local = MeterSnapshot::default();
        update_levels(&silent_block(2, 64), 0x200, "AC3", &mut local, &shared);
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut local, &shared);
        assert_eq!(local.per_pid.len(), 2);
        assert_eq!(local.per_pid[0].pid, 0x100);
        assert_eq!(local.per_pid[1].pid, 0x200);
    }

    #[test]
    fn prune_stale_drops_old_pids() {
        let shared = new_shared_meter();
        let mut local = MeterSnapshot::default();
        update_levels(&silent_block(2, 64), 0x100, "AAC", &mut local, &shared);
        local.per_pid[0].last_update = Instant::now() - Duration::from_secs(10);
        prune_stale(&mut local, &shared, Duration::from_secs(5));
        assert!(local.per_pid.is_empty());
    }

    #[test]
    fn shared_load_returns_latest_snapshot() {
        let shared = new_shared_meter();
        let mut local = MeterSnapshot::default();
        update_levels(&full_scale_block(2, 32), 0x100, "AAC", &mut local, &shared);
        let observed = shared.load();
        assert_eq!(observed.per_pid.len(), 1);
        assert_eq!(observed.per_pid[0].pid, 0x100);
    }

    #[test]
    fn rasterise_does_not_panic_on_small_buffer() {
        // A 1280×720 BGRA buffer.
        let w: u32 = 1280;
        let h: u32 = 720;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let shared = new_shared_meter();
        let mut local = MeterSnapshot::default();
        update_levels(&full_scale_block(2, 64), 0x100, "AAC", &mut local, &shared);
        update_levels(&silent_block(6, 64), 0x101, "AC3", &mut local, &shared);
        let s = local.clone();
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
    fn rasterise_no_op_on_empty_snapshot() {
        let w: u32 = 640;
        let h: u32 = 480;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let snap = MeterSnapshot::default();
        rasterise(&snap, &mut buf, pitch, w, h);
        assert!(buf.iter().all(|&b| b == 0));
    }
}
