// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Video Full content-analysis tier.
//!
//! Subscribes to the flow's broadcast channel, buffers recent MPEG-TS
//! data, and at the configured sample cadence decodes one video frame
//! via the in-process FFmpeg video decoder ([`video_engine::VideoDecoder`]).
//! The decoded Y plane feeds a pack of cheap pure-Rust pixel metrics:
//!
//! - **SAD freeze**: sum-of-absolute-differences against the previous
//!   decoded Y plane, normalised to 0..255. Near-zero ⇒ freeze.
//! - **Blockiness**: mean absolute gradient across 8×8 DCT block
//!   boundaries (Wang / Sheikh style). Higher ⇒ over-compressed.
//! - **Blur**: variance of the 3×3 Laplacian. Lower ⇒ blurry.
//! - **Letterbox / pillarbox**: count of rows / columns whose mean
//!   luma is below the black threshold on the top-bottom / left-right
//!   edges.
//! - **Colour-bar detection**: looks for SMPTE colour-bar column-
//!   uniformity (7 vertical stripes of per-column near-constant Y).
//! - **Slate**: high freeze score + low motion over several samples.
//!
//! Heavy FFmpeg work runs in `tokio::task::block_in_place` so the
//! analyser task doesn't hold the tokio runtime during decode. On
//! broadcast lag we increment `video_full_drops` and skip.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::manager::events::{Event, EventSender, EventSeverity};
use crate::stats::collector::ContentAnalysisAccumulator;

use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::{strip_rtp_header, TS_PACKET_SIZE, TS_SYNC_BYTE};

const DEFAULT_HZ: f32 = 1.0;
const MAX_HZ: f32 = 30.0;
/// Keep ~2 s of TS at 10 Mbps (~2.5 MB). Plenty of runway for one keyframe.
const TS_BUFFER_CAP: usize = 3 * 1024 * 1024;
/// Black-edge detection threshold — row/col is "black" when mean Y ≤ this.
const BLACK_Y_THRESHOLD: u8 = 20;
/// SAD threshold below which we consider a frame identical to the previous.
const FREEZE_SAD_THRESHOLD: f32 = 0.75;
const FREEZE_DEBOUNCE: Duration = Duration::from_secs(3);
const EVENT_RATELIMIT: Duration = Duration::from_secs(30);

pub fn spawn_content_analysis_video_full(
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<ContentAnalysisAccumulator>,
    event_sender: EventSender,
    flow_id: String,
    sample_hz: Option<f32>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(video_full_loop(
        rx, stats, event_sender, flow_id, sample_hz, cancel,
    ))
}

async fn video_full_loop(
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<ContentAnalysisAccumulator>,
    events: EventSender,
    flow_id: String,
    sample_hz: Option<f32>,
    cancel: CancellationToken,
) {
    let hz = sample_hz.unwrap_or(DEFAULT_HZ).clamp(0.1, MAX_HZ) as f64;
    let sample_period = Duration::from_secs_f64(1.0 / hz);
    tracing::info!(flow_id = %flow_id, hz = hz, "content-analysis Video Full analyser started");

    let mut state = VideoFullState::new();
    let mut sample_interval = tokio::time::interval(sample_period);
    sample_interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(flow_id = %flow_id, "content-analysis Video Full stopping (cancelled)");
                break;
            }

            _ = sample_interval.tick() => {
                state.sample(&flow_id, &events);
                let snap = state.snapshot(hz);
                *stats.video_full.lock().unwrap() = Some(snap);
            }

            result = rx.recv() => {
                match result {
                    Ok(packet) => state.ingest(&packet),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.video_full_drops.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            flow_id = %flow_id,
                            "content-analysis Video Full: broadcast closed"
                        );
                        break;
                    }
                }
            }
        }
    }
}

// ── State ──────────────────────────────────────────────────────────────────

struct VideoFullState {
    /// Rolling TS byte ring.
    ts_ring: VecDeque<u8>,

    /// Last observed Y plane for SAD freeze detection.
    prev_y: Option<YPlane>,

    /// Published per-frame metrics.
    last_metrics: Option<FrameMetrics>,
    /// How many samples we've attempted.
    samples_taken: u64,
    samples_decoded: u64,

    // Alarm state
    freeze_since: Option<Instant>,
    freeze_active: bool,
    last_freeze_event_at: Option<Instant>,
}

#[derive(Clone)]
struct YPlane {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct FrameMetrics {
    width: u32,
    height: u32,
    mean_y: f32,
    sad_score: Option<f32>,
    blur_variance: f32,
    blockiness: f32,
    letterbox_rows: u32,
    pillarbox_cols: u32,
    colour_bar: bool,
    slate: bool,
}

impl VideoFullState {
    fn new() -> Self {
        Self {
            ts_ring: VecDeque::with_capacity(TS_BUFFER_CAP),
            prev_y: None,
            last_metrics: None,
            samples_taken: 0,
            samples_decoded: 0,
            freeze_since: None,
            freeze_active: false,
            last_freeze_event_at: None,
        }
    }

    fn ingest(&mut self, packet: &RtpPacket) {
        let payload = strip_rtp_header(packet);
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= payload.len() {
            let pkt = &payload[offset..offset + TS_PACKET_SIZE];
            if pkt[0] == TS_SYNC_BYTE {
                if self.ts_ring.len() + TS_PACKET_SIZE > TS_BUFFER_CAP {
                    // Drop oldest TS_PACKET_SIZE bytes
                    for _ in 0..TS_PACKET_SIZE {
                        self.ts_ring.pop_front();
                    }
                }
                self.ts_ring.extend(pkt.iter().copied());
            }
            offset += TS_PACKET_SIZE;
        }
    }

    fn sample(&mut self, flow_id: &str, events: &EventSender) {
        self.samples_taken += 1;
        if self.ts_ring.is_empty() {
            return;
        }
        // Snapshot the TS ring into a contiguous buffer for the decoder.
        let mut ts_snapshot = Vec::with_capacity(self.ts_ring.len());
        ts_snapshot.extend(self.ts_ring.iter().copied());
        let prev_y = self.prev_y.clone();

        // Decode + metrics run on the current thread via block_in_place so
        // the tokio reactor isn't blocked. Keep the critical section small:
        // we only produce `FrameMetrics` + an updated `YPlane`.
        let (metrics, new_y) = tokio::task::block_in_place(|| decode_and_metrics(&ts_snapshot, prev_y.as_ref()));

        if let Some(y) = new_y {
            self.prev_y = Some(y);
            self.samples_decoded += 1;
        }

        // Freeze alarm
        if let Some(ref m) = metrics {
            let now = Instant::now();
            if m.sad_score.map_or(false, |s| s < FREEZE_SAD_THRESHOLD) {
                if self.freeze_since.is_none() {
                    self.freeze_since = Some(now);
                }
                if let Some(since) = self.freeze_since {
                    if now.duration_since(since) >= FREEZE_DEBOUNCE && !self.freeze_active {
                        self.freeze_active = true;
                        let fire = self.last_freeze_event_at
                            .map_or(true, |t| t.elapsed() >= EVENT_RATELIMIT);
                        if fire {
                            events.send(Event {
                                severity: EventSeverity::Warning,
                                category: "content_analysis_video_freeze".into(),
                                message: format!(
                                    "Video frozen (YUV-SAD = {:.2}) for {:.1}s",
                                    m.sad_score.unwrap_or(0.0),
                                    FREEZE_DEBOUNCE.as_secs_f32(),
                                ),
                                details: Some(serde_json::json!({
                                    "error_code": "content_analysis_video_freeze",
                                    "sad_score": m.sad_score,
                                    "mean_y": m.mean_y,
                                })),
                                flow_id: Some(flow_id.into()),
                                input_id: None,
                                output_id: None,
                            });
                            self.last_freeze_event_at = Some(now);
                        }
                    }
                }
            } else {
                self.freeze_since = None;
                self.freeze_active = false;
            }
        }
        self.last_metrics = metrics;
    }

    fn snapshot(&self, hz: f64) -> serde_json::Value {
        let m = self.last_metrics.clone().unwrap_or_default();
        serde_json::json!({
            "tier": "video_full",
            "version": 2,
            "sample_hz": hz,
            "samples_taken": self.samples_taken,
            "samples_decoded": self.samples_decoded,
            "width": m.width,
            "height": m.height,
            "mean_y": round(m.mean_y, 2),
            "yuv_sad_freeze": m.sad_score.map(|v| round(v, 3)),
            "freeze_active": self.freeze_active,
            "blur_variance": round(m.blur_variance, 2),
            "blockiness": round(m.blockiness, 3),
            "letterbox_rows": m.letterbox_rows,
            "pillarbox_cols": m.pillarbox_cols,
            "colour_bar": m.colour_bar,
            "slate": m.slate,
        })
    }
}

fn round(v: f32, dp: u32) -> f32 {
    let f = 10_f32.powi(dp as i32);
    (v * f).round() / f
}

// ── Decode + metrics ──────────────────────────────────────────────────────

#[cfg(feature = "video-thumbnail")]
fn decode_and_metrics(
    ts: &[u8],
    prev_y: Option<&YPlane>,
) -> (Option<FrameMetrics>, Option<YPlane>) {
    use video_engine::VideoDecoder;

    let (annex_b, codec) = match extract_video(ts) {
        Some(v) => v,
        None => return (None, None),
    };
    let mut decoder = match VideoDecoder::open(codec) {
        Ok(d) => d,
        Err(_) => return (None, None),
    };
    if decoder.send_packet(&annex_b).is_err() {
        return (None, None);
    }
    let _ = decoder.send_packet(&[]); // signal EOS so receive_frame flushes
    // Iterate through all frames returned — decode until we have the
    // most recent one or the decoder signals need-more-input / EOF.
    let mut latest: Option<video_engine::DecodedFrame> = None;
    loop {
        match decoder.receive_frame() {
            Ok(f) => latest = Some(f),
            Err(_) => break,
        }
    }
    let frame = match latest {
        Some(f) => f,
        None => return (None, None),
    };
    let Some((y, y_stride, _u, _u_stride, _v, _v_stride)) = frame.yuv_planes() else {
        return (None, None);
    };
    let w = frame.width();
    let h = frame.height();
    if w < 16 || h < 16 {
        return (None, None);
    }

    // Copy Y plane (tight — no stride padding) into an owned buffer so we
    // can retain it across samples after the decoder's AVFrame is dropped.
    let mut y_tight = Vec::with_capacity((w * h) as usize);
    for row in 0..h as usize {
        let start = row * y_stride;
        y_tight.extend_from_slice(&y[start..start + w as usize]);
    }

    let metrics = compute_metrics(&y_tight, w, h, prev_y);

    let new_y = YPlane {
        width: w,
        height: h,
        data: y_tight,
    };
    let _ = codec; // silence unused warning on some branches
    (Some(metrics), Some(new_y))
}

#[cfg(not(feature = "video-thumbnail"))]
fn decode_and_metrics(
    _ts: &[u8],
    _prev_y: Option<&YPlane>,
) -> (Option<FrameMetrics>, Option<YPlane>) {
    // No in-process decoder available — publish nothing.
    (None, None)
}

/// Pull recent video NAL data out of the TS buffer and return the codec.
/// A very-trimmed cousin of [`crate::engine::thumbnail::extract_video_from_ts`]
/// — kept local so follow-up edits to the thumbnail pipeline don't perturb
/// the analyser.
#[cfg(feature = "video-thumbnail")]
fn extract_video(ts: &[u8]) -> Option<(Vec<u8>, video_codec::VideoCodec)> {
    use crate::engine::ts_parse::{
        parse_pat_programs, ts_has_payload, ts_payload_offset, ts_pid, ts_pusi, PAT_PID,
    };
    use video_codec::VideoCodec;

    const STREAM_H264: u8 = 0x1B;
    const STREAM_H265: u8 = 0x24;

    // Find PMT PID
    let mut pmt_pid = None;
    let mut i = 0;
    while i + TS_PACKET_SIZE <= ts.len() {
        let pkt = &ts[i..i + TS_PACKET_SIZE];
        if pkt[0] == TS_SYNC_BYTE && ts_pid(pkt) == PAT_PID && ts_pusi(pkt) {
            let progs = parse_pat_programs(pkt);
            if let Some((_, pid)) = progs.into_iter().next() {
                pmt_pid = Some(pid);
                break;
            }
        }
        i += TS_PACKET_SIZE;
    }
    let pmt_pid = pmt_pid?;

    // Find video PID from PMT
    let mut video_pid = None;
    let mut codec = None;
    let mut j = 0;
    while j + TS_PACKET_SIZE <= ts.len() {
        let pkt = &ts[j..j + TS_PACKET_SIZE];
        if pkt[0] == TS_SYNC_BYTE && ts_pid(pkt) == pmt_pid && ts_pusi(pkt) {
            let payload_start = ts_payload_offset(pkt);
            if payload_start < TS_PACKET_SIZE {
                let payload = &pkt[payload_start..];
                if let Some((pid, st)) = parse_pmt_first_video(payload) {
                    video_pid = Some(pid);
                    codec = Some(match st {
                        STREAM_H264 => VideoCodec::H264,
                        STREAM_H265 => VideoCodec::Hevc,
                        _ => return None,
                    });
                    break;
                }
            }
        }
        j += TS_PACKET_SIZE;
    }
    let video_pid = video_pid?;
    let codec = codec?;

    // Reassemble PES from video PID
    let mut es = Vec::with_capacity(128 * 1024);
    let mut k = 0;
    let mut started = false;
    while k + TS_PACKET_SIZE <= ts.len() {
        let pkt = &ts[k..k + TS_PACKET_SIZE];
        k += TS_PACKET_SIZE;
        if pkt[0] != TS_SYNC_BYTE || ts_pid(pkt) != video_pid || !ts_has_payload(pkt) {
            continue;
        }
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE {
            continue;
        }
        let payload = &pkt[payload_start..];
        let pusi = ts_pusi(pkt);
        if pusi {
            if payload.len() >= 9 && payload[0] == 0 && payload[1] == 0 && payload[2] == 1 {
                let hdr_data_len = payload[8] as usize;
                let es_start = 9 + hdr_data_len;
                if es_start < payload.len() {
                    es.extend_from_slice(&payload[es_start..]);
                    started = true;
                }
            }
        } else if started {
            es.extend_from_slice(payload);
        }
    }
    if es.is_empty() {
        return None;
    }
    Some((es, codec))
}

#[cfg(feature = "video-thumbnail")]
fn parse_pmt_first_video(payload: &[u8]) -> Option<(u16, u8)> {
    if payload.is_empty() {
        return None;
    }
    let pointer = payload[0] as usize;
    if 1 + pointer + 12 > payload.len() {
        return None;
    }
    let section = &payload[1 + pointer..];
    if section.is_empty() || section[0] != 0x02 {
        return None;
    }
    let section_length = (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
    if 3 + section_length > section.len() || section_length < 13 {
        return None;
    }
    let program_info_length = (((section[10] as usize) & 0x0F) << 8) | section[11] as usize;
    let es_start = 12 + program_info_length;
    let body_end = 3 + section_length - 4;
    let mut i = es_start;
    while i + 5 <= body_end {
        let stream_type = section[i];
        let es_pid = (((section[i + 1] as u16) & 0x1F) << 8) | section[i + 2] as u16;
        let es_info_length = (((section[i + 3] as usize) & 0x0F) << 8) | section[i + 4] as usize;
        if matches!(stream_type, 0x1B | 0x24) {
            return Some((es_pid, stream_type));
        }
        i += 5 + es_info_length;
    }
    None
}

// ── Pixel metrics ──────────────────────────────────────────────────────────

fn compute_metrics(y: &[u8], width: u32, height: u32, prev: Option<&YPlane>) -> FrameMetrics {
    let w = width as usize;
    let h = height as usize;
    let total = (w * h) as f64;

    // Mean luma
    let sum_y: u64 = y.iter().map(|&p| p as u64).sum();
    let mean_y = (sum_y as f64 / total) as f32;

    // SAD vs previous (normalised 0..255)
    let sad = prev.and_then(|p| {
        if p.width == width && p.height == height && p.data.len() == y.len() {
            let s: u64 = y
                .iter()
                .zip(p.data.iter())
                .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
                .sum();
            Some((s as f64 / total) as f32)
        } else {
            None
        }
    });

    let blur = laplacian_variance(y, w, h);
    let block = blockiness_8x8(y, w, h);
    let (letterbox_rows, pillarbox_cols) = letterbox_detect(y, w, h);
    let cbar = detect_colour_bars(y, w, h);
    // Slate: high freeze + low motion + high mean (static brightness) for N samples.
    let slate = sad.map_or(false, |s| s < FREEZE_SAD_THRESHOLD) && mean_y > 40.0 && mean_y < 220.0;

    FrameMetrics {
        width,
        height,
        mean_y,
        sad_score: sad,
        blur_variance: blur,
        blockiness: block,
        letterbox_rows: letterbox_rows as u32,
        pillarbox_cols: pillarbox_cols as u32,
        colour_bar: cbar,
        slate,
    }
}

/// Variance of a 3×3 Laplacian kernel applied to the Y plane. Lower values
/// indicate a blurrier frame. Normalised to pixel-count so the absolute
/// scale is framerate-stable.
fn laplacian_variance(y: &[u8], w: usize, h: usize) -> f32 {
    if w < 3 || h < 3 {
        return 0.0;
    }
    // Downsample for speed: stride-4 iteration gives ~25 % coverage,
    // plenty for a variance estimate on a 1080p frame in ~2 ms.
    let mut sum: f64 = 0.0;
    let mut sumsq: f64 = 0.0;
    let mut n: usize = 0;
    let mut row = 1;
    while row < h - 1 {
        let mut col = 1;
        while col < w - 1 {
            let c = y[row * w + col] as i32;
            let t = y[(row - 1) * w + col] as i32;
            let b = y[(row + 1) * w + col] as i32;
            let l = y[row * w + col - 1] as i32;
            let r = y[row * w + col + 1] as i32;
            let lap = (t + b + l + r - 4 * c) as f64;
            sum += lap;
            sumsq += lap * lap;
            n += 1;
            col += 4;
        }
        row += 4;
    }
    if n == 0 {
        return 0.0;
    }
    let mean = sum / n as f64;
    let var = sumsq / n as f64 - mean * mean;
    var.max(0.0) as f32
}

/// 8×8 DCT-block boundary strength — average absolute step across block
/// boundaries minus the average absolute step inside blocks. A positive
/// value indicates blockiness; near-zero means blocks are invisible.
fn blockiness_8x8(y: &[u8], w: usize, h: usize) -> f32 {
    if w < 16 || h < 16 {
        return 0.0;
    }
    let mut boundary_sum: f64 = 0.0;
    let mut boundary_n: usize = 0;
    let mut interior_sum: f64 = 0.0;
    let mut interior_n: usize = 0;
    // Horizontal neighbour differences row by row
    for row in 0..h {
        let base = row * w;
        for col in 1..w {
            let d = (y[base + col] as i32 - y[base + col - 1] as i32).abs() as f64;
            if col % 8 == 0 {
                boundary_sum += d;
                boundary_n += 1;
            } else {
                interior_sum += d;
                interior_n += 1;
            }
        }
    }
    // Vertical neighbour differences column by column
    for col in 0..w {
        for row in 1..h {
            let d =
                (y[row * w + col] as i32 - y[(row - 1) * w + col] as i32).abs() as f64;
            if row % 8 == 0 {
                boundary_sum += d;
                boundary_n += 1;
            } else {
                interior_sum += d;
                interior_n += 1;
            }
        }
    }
    if boundary_n == 0 || interior_n == 0 {
        return 0.0;
    }
    let b = boundary_sum / boundary_n as f64;
    let i = interior_sum / interior_n as f64;
    (b - i).max(0.0) as f32
}

/// Count black rows at top+bottom (letterbox) and black columns at
/// left+right (pillarbox). Rows/cols are black when mean Y ≤
/// [`BLACK_Y_THRESHOLD`].
fn letterbox_detect(y: &[u8], w: usize, h: usize) -> (usize, usize) {
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let mut top_rows = 0usize;
    for row in 0..(h / 4) {
        let mean =
            y[row * w..(row + 1) * w].iter().map(|&p| p as u32).sum::<u32>() / w as u32;
        if mean as u8 <= BLACK_Y_THRESHOLD {
            top_rows += 1;
        } else {
            break;
        }
    }
    let mut bottom_rows = 0usize;
    for row in (h / 2)..h {
        let r = h - 1 - (row - h / 2);
        if r >= h {
            continue;
        }
        let mean = y[r * w..(r + 1) * w].iter().map(|&p| p as u32).sum::<u32>() / w as u32;
        if mean as u8 <= BLACK_Y_THRESHOLD {
            bottom_rows += 1;
        } else {
            break;
        }
    }
    let letter = top_rows + bottom_rows;

    let mut left_cols = 0usize;
    for col in 0..(w / 4) {
        let mut sum: u64 = 0;
        for row in 0..h {
            sum += y[row * w + col] as u64;
        }
        let mean = (sum / h as u64) as u8;
        if mean <= BLACK_Y_THRESHOLD {
            left_cols += 1;
        } else {
            break;
        }
    }
    let mut right_cols = 0usize;
    for col in (w / 2)..w {
        let c = w - 1 - (col - w / 2);
        if c >= w {
            continue;
        }
        let mut sum: u64 = 0;
        for row in 0..h {
            sum += y[row * w + c] as u64;
        }
        let mean = (sum / h as u64) as u8;
        if mean <= BLACK_Y_THRESHOLD {
            right_cols += 1;
        } else {
            break;
        }
    }
    (letter, left_cols + right_cols)
}

/// Colour-bar heuristic: SMPTE EG 1-1990 bars are seven vertical stripes
/// of near-uniform colour, each ~1/7 of the frame width. We check that
/// each column has low within-column Y variance — a strong indicator of
/// vertical-stripe content. Colour-bar tends to produce very low
/// variance down every column regardless of hue.
fn detect_colour_bars(y: &[u8], w: usize, h: usize) -> bool {
    if w < 64 || h < 64 {
        return false;
    }
    // Sample up to 64 evenly-spaced columns to bound work.
    let col_step = (w / 64).max(1);
    let mut low_variance_cols = 0usize;
    let mut total_sampled = 0usize;
    let mut col = 0;
    while col < w {
        let mut sum: f64 = 0.0;
        let mut sumsq: f64 = 0.0;
        let mut n = 0usize;
        let row_step = (h / 64).max(1);
        let mut row = 0;
        while row < h {
            let v = y[row * w + col] as f64;
            sum += v;
            sumsq += v * v;
            n += 1;
            row += row_step;
        }
        if n > 0 {
            let mean = sum / n as f64;
            let var = sumsq / n as f64 - mean * mean;
            // Threshold chosen so moving content fails it easily but
            // colour-bar columns (very uniform Y) pass.
            if var < 25.0 {
                low_variance_cols += 1;
            }
            total_sampled += 1;
        }
        col += col_step;
    }
    // Colour bar has ALL columns nearly uniform; require ≥ 80 %.
    total_sampled > 0 && low_variance_cols * 10 >= total_sampled * 8
}
