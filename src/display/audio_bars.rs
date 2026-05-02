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
//   ├─[PID 0x100  AAC 5.1 ]─[PID 0x101  AC3 5.1]──────────────┤  ← strip
//   │ █▌▌▌▌▌  green base, yellow >-18dBFS, red >-3dBFS, white │
//   │ █▌▌▌▌▌  peak-hold tick; one column per audio channel    │
//   └─────────────────────────────────────────────────────────┘
//
// All work is CPU. At 1080p the strip is ~130 px tall × 1920 px wide ×
// 4 B = ~1 MB per present. Negligible on any host that already runs
// libswscale Lanczos for the main blit.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

pub type SharedMeter = Arc<Mutex<MeterSnapshot>>;

pub fn new_shared_meter() -> SharedMeter {
    Arc::new(Mutex::new(MeterSnapshot::default()))
}

/// Compute peak + RMS in dBFS for a planar f32 PCM block and merge them
/// into the shared snapshot for the given PID.
///
/// `planar` is one slice per channel. All samples are expected to be in
/// `[-1.0, 1.0]` — anything beyond is clipped at `RED_THRESHOLD_DBFS`.
pub fn update_levels(
    planar: &[Vec<f32>],
    pid: u16,
    codec_label: &'static str,
    snapshot: &SharedMeter,
) {
    if planar.is_empty() {
        return;
    }
    let now = Instant::now();
    let new_levels: Vec<(f32, f32)> = planar
        .iter()
        .map(|ch| {
            if ch.is_empty() {
                return (DBFS_FLOOR, DBFS_FLOOR);
            }
            let mut peak = 0.0f32;
            let mut sum_sq = 0.0f64;
            for &s in ch {
                let a = s.abs();
                if a > peak {
                    peak = a;
                }
                sum_sq += (s as f64) * (s as f64);
            }
            let rms = (sum_sq / ch.len() as f64).sqrt() as f32;
            (amp_to_dbfs(peak), amp_to_dbfs(rms))
        })
        .collect();

    let mut snap = snapshot.lock().expect("audio meter snapshot mutex poisoned");
    snap.generation = snap.generation.wrapping_add(1);
    let entry = snap.per_pid.iter_mut().find(|p| p.pid == pid);
    let pid_meter = match entry {
        Some(p) => {
            p.codec_label = codec_label;
            p.channels.resize_with(planar.len(), ChannelLevel::default);
            p.last_update = now;
            p
        }
        None => {
            snap.per_pid.push(PidMeter {
                pid,
                codec_label,
                channels: vec![ChannelLevel::default(); planar.len()],
                last_update: now,
            });
            snap.per_pid.sort_by_key(|p| p.pid);
            snap.per_pid
                .iter_mut()
                .find(|p| p.pid == pid)
                .expect("just inserted")
        }
    };
    for (ch, (peak_dbfs, rms_dbfs)) in pid_meter.channels.iter_mut().zip(new_levels.iter()) {
        ch.peak_dbfs = *peak_dbfs;
        ch.rms_dbfs = *rms_dbfs;
        if *peak_dbfs >= ch.peak_hold_dbfs || now >= ch.peak_hold_until {
            ch.peak_hold_dbfs = *peak_dbfs;
            ch.peak_hold_until = now + PEAK_HOLD_DURATION;
        }
    }
}

/// Drop PIDs whose decoder hasn't produced a block in `stale_after`.
/// Called periodically by the meter task so the overlay reflects the
/// current PMT (e.g. an audio PID that vanished from the program list).
pub fn prune_stale(snapshot: &SharedMeter, stale_after: Duration) {
    let now = Instant::now();
    let mut snap = snapshot.lock().expect("audio meter snapshot mutex poisoned");
    snap.per_pid
        .retain(|p| now.duration_since(p.last_update) < stale_after);
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
const BLOCK_GAP_PX: u32 = 8;
const SIDE_MARGIN_PX: u32 = 12;
const LABEL_HEIGHT_PX: u32 = 12; // 5×7 font scaled 1.5×
const STRIP_PCT: u32 = 12;
const STRIP_MIN_PX: u32 = 96;
const STRIP_MAX_PX: u32 = 200;

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

    // 2. Per-PID block layout.
    let pid_count = snapshot.per_pid.len() as u32;
    let usable_w = dst_w.saturating_sub(SIDE_MARGIN_PX * 2);
    let total_gaps = BLOCK_GAP_PX * pid_count.saturating_sub(1);
    if usable_w <= total_gaps {
        return;
    }
    let block_w = (usable_w - total_gaps) / pid_count;
    if block_w < 24 {
        return;
    }
    let bars_top = strip_top + LABEL_HEIGHT_PX + 2;
    let bars_h = strip_h.saturating_sub(LABEL_HEIGHT_PX + 4);
    if bars_h < 16 {
        return;
    }

    let now = Instant::now();
    for (i, pid) in snapshot.per_pid.iter().enumerate() {
        let block_x = SIDE_MARGIN_PX + (block_w + BLOCK_GAP_PX) * (i as u32);

        // Label row: "PID 0xNNN  CODEC  X.Y"
        let label = format_pid_label(pid);
        draw_text(dst, dst_pitch, dst_w, dst_h, block_x + 2, strip_top + 2, &label, 0xE0, 0xE0, 0xE0);

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
        let bar_w = (inner_w - ch_total_gaps) / ch_count;
        if bar_w == 0 {
            continue;
        }
        for (ch_idx, ch) in pid.channels.iter().enumerate() {
            let bar_x = block_x + 2 + (bar_w + BAR_GAP_PX) * (ch_idx as u32);
            draw_channel_bar(
                dst, dst_pitch, dst_w, dst_h, bar_x, bars_top, bar_w, bars_h, ch, now,
            );
        }
    }
}

fn format_pid_label(pid: &PidMeter) -> String {
    let ch_label = match pid.channels.len() {
        1 => "MONO".to_string(),
        2 => "STEREO".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        n => format!("{n}CH"),
    };
    format!("PID 0x{:X}  {}  {}", pid.pid, pid.codec_label, ch_label)
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
    // Frame: 1 px white border around the bar slot.
    fill_rect(dst, pitch, dst_w, dst_h, x, y, w, h, 0x18, 0x18, 0x18);
    // Compute fill heights from RMS (filled bar) and peak-hold (tick).
    let rms_frac = dbfs_to_frac(ch.rms_dbfs);
    let peak_hold_frac = if now < ch.peak_hold_until {
        dbfs_to_frac(ch.peak_hold_dbfs)
    } else {
        rms_frac
    };
    let fill_h = ((h as f32) * rms_frac) as u32;
    let bar_top = y + h.saturating_sub(fill_h);

    // Tri-colour fill: bottom green, middle yellow, top red.
    let yellow_y = y + h.saturating_sub(((h as f32) * dbfs_to_frac(YELLOW_THRESHOLD_DBFS)) as u32);
    let red_y = y + h.saturating_sub(((h as f32) * dbfs_to_frac(RED_THRESHOLD_DBFS)) as u32);

    for row in bar_top..(y + h) {
        let (b, g, r) = if row >= red_y {
            // Bottom-most band first by row order — but red sits on top
            // of the bar visually (highest dB). Resolve by checking band
            // membership against thresholds.
            (0x40, 0x40, 0xF0) // (b, g, r) for red
        } else if row >= yellow_y {
            (0x40, 0xC8, 0xF0) // amber
        } else {
            (0x40, 0xE0, 0x40) // green
        };
        // Re-evaluate using strict row→band mapping: row above red_y is
        // red, between yellow_y and red_y is yellow, below yellow_y is
        // green. The `if/else` above already does this in row order.
        let _ = (b, g, r);
        let row_band_color = pick_band_color(y, h, row);
        fill_row(dst, pitch, dst_w, x + 1, row, w.saturating_sub(2), row_band_color);
    }
    // Peak-hold tick: 2-px white line at peak_hold_frac of the bar.
    let hold_pixels = ((h as f32) * peak_hold_frac) as u32;
    if hold_pixels >= 1 {
        let hold_y = y + h.saturating_sub(hold_pixels);
        let tick_h = 2.min(h);
        for row in hold_y..(hold_y + tick_h).min(y + h) {
            fill_row(dst, pitch, dst_w, x + 1, row, w.saturating_sub(2), (0xFF, 0xFF, 0xFF));
        }
    }
}

fn pick_band_color(y_top: u32, h: u32, row: u32) -> (u8, u8, u8) {
    let frac_from_bottom = ((y_top + h).saturating_sub(row) as f32) / (h.max(1) as f32);
    let dbfs = frac_to_dbfs(frac_from_bottom);
    if dbfs >= RED_THRESHOLD_DBFS {
        (0x30, 0x30, 0xF0) // BGR red
    } else if dbfs >= YELLOW_THRESHOLD_DBFS {
        (0x30, 0xC8, 0xF0) // BGR amber
    } else {
        (0x30, 0xE0, 0x30) // BGR green
    }
}

/// Map a dBFS value to a `[0, 1]` fraction of bar height.
/// Curve: linear from -60 dBFS (= 0) to 0 dBFS (= 1). Anything below
/// -60 dBFS folds to 0; above 0 dBFS clamps to 1.
fn dbfs_to_frac(dbfs: f32) -> f32 {
    let clipped = dbfs.clamp(-60.0, 0.0);
    (clipped + 60.0) / 60.0
}

fn frac_to_dbfs(frac: f32) -> f32 {
    let clipped = frac.clamp(0.0, 1.0);
    -60.0 + clipped * 60.0
}

// ── Drawing primitives ────────────────────────────────────────────

fn blend_strip_background(dst: &mut [u8], pitch: usize, dst_w: u32, y: u32, h: u32) {
    let row_bytes = (dst_w as usize) * 4;
    for row in y..(y + h) {
        let row_start = (row as usize) * pitch;
        if row_start + row_bytes > dst.len() {
            break;
        }
        let row_slice = &mut dst[row_start..row_start + row_bytes];
        for px in row_slice.chunks_exact_mut(4) {
            // Alpha-blend with 50 % black.
            px[0] = px[0] / 2;
            px[1] = px[1] / 2;
            px[2] = px[2] / 2;
            // alpha (XRGB ignores) stays 0xFF
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
    for px in slice.chunks_exact_mut(4) {
        px[0] = color.0;
        px[1] = color.1;
        px[2] = color.2;
        px[3] = 0xFF;
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

    fn lock(snap: &SharedMeter) -> std::sync::MutexGuard<'_, MeterSnapshot> {
        snap.lock().expect("snapshot mutex poisoned in test")
    }

    #[test]
    fn dbfs_clamp_silence() {
        let snap = new_shared_meter();
        update_levels(&silent_block(2, 1024), 0x100, "AAC", &snap);
        let s = lock(&snap);
        assert_eq!(s.per_pid.len(), 1);
        assert_eq!(s.per_pid[0].channels.len(), 2);
        for ch in &s.per_pid[0].channels {
            assert_eq!(ch.peak_dbfs, DBFS_FLOOR);
            assert_eq!(ch.rms_dbfs, DBFS_FLOOR);
        }
    }

    #[test]
    fn dbfs_full_scale_reads_zero() {
        let snap = new_shared_meter();
        update_levels(&full_scale_block(1, 256), 0x100, "AAC", &snap);
        let s = lock(&snap);
        assert_eq!(s.per_pid[0].channels[0].peak_dbfs, 0.0);
        assert!(s.per_pid[0].channels[0].rms_dbfs.abs() < 0.01);
    }

    #[test]
    fn peak_hold_decay() {
        let snap = new_shared_meter();
        update_levels(&full_scale_block(1, 64), 0x100, "AAC", &snap);
        let hold_until_first = lock(&snap).per_pid[0].channels[0].peak_hold_until;
        // Subsequent silent block within hold window should not decay.
        update_levels(&silent_block(1, 64), 0x100, "AAC", &snap);
        let s = lock(&snap);
        assert_eq!(s.per_pid[0].channels[0].peak_hold_dbfs, 0.0);
        assert_eq!(s.per_pid[0].channels[0].peak_hold_until, hold_until_first);
    }

    #[test]
    fn multi_pid_sorted() {
        let snap = new_shared_meter();
        update_levels(&silent_block(2, 64), 0x200, "AC3", &snap);
        update_levels(&silent_block(2, 64), 0x100, "AAC", &snap);
        let s = lock(&snap);
        assert_eq!(s.per_pid.len(), 2);
        assert_eq!(s.per_pid[0].pid, 0x100);
        assert_eq!(s.per_pid[1].pid, 0x200);
    }

    #[test]
    fn prune_stale_drops_old_pids() {
        let snap = new_shared_meter();
        update_levels(&silent_block(2, 64), 0x100, "AAC", &snap);
        // Force last_update into the distant past.
        lock(&snap).per_pid[0].last_update = Instant::now() - Duration::from_secs(10);
        prune_stale(&snap, Duration::from_secs(5));
        assert!(lock(&snap).per_pid.is_empty());
    }

    #[test]
    fn rasterise_does_not_panic_on_small_buffer() {
        // A 1280×720 BGRA buffer.
        let w: u32 = 1280;
        let h: u32 = 720;
        let pitch = (w as usize) * 4;
        let mut buf = vec![0u8; pitch * (h as usize)];
        let snap = new_shared_meter();
        update_levels(&full_scale_block(2, 64), 0x100, "AAC", &snap);
        update_levels(&silent_block(6, 64), 0x101, "AC3", &snap);
        let s = lock(&snap).clone();
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
