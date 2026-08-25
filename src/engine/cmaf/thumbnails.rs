// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Thumbnail track for the CMAF output — sprite sheets plus a WebVTT index.
//!
//! # Why this exists
//!
//! Dragging a scrub bar issues roughly twenty seeks a second. A seek into
//! buffered media is immediate; every other position costs a segment fetch, so
//! outside the player's back buffer a drag shows nothing until it stops.
//!
//! Measured on a live 1080p feed with `requestVideoFrameCallback`, forty seeks
//! over two seconds presented **0–1** frames — at LAN speed, at 25 Mbit/s and
//! at 8 Mbit/s alike, and identically on the low-resolution all-intra proxy.
//! The cost is the fetch, not the decode: 2.0 MB per two-second segment for
//! the main rendition against 656 KB for the proxy. An all-intra 640x360
//! rendition is not a cheap seek target, so it cannot rescue this.
//!
//! One sprite sheet is about the size of *one* media segment and covers a
//! hundred positions. That is the whole idea.
//!
//! # Discipline
//!
//! Same shape as [`crate::replay::filmstrip`], which this reuses rather than
//! duplicating:
//!
//! - Sibling broadcast subscriber, drop-on-`Lagged`, never blocks the data
//!   path.
//! - Bounded ring buffer of recent TS so a decodable frame is in scope when
//!   the cadence tick fires.
//! - Decode + scale + JPEG-encode under `spawn_blocking` via the in-process
//!   video engine — the same path the live thumbnail uses.
//! - A failure here never tears down the CMAF output. Thumbnails are a
//!   convenience; the media is not.
//!
//! # The clock
//!
//! Cue times are offsets from an explicit UTC epoch declared in the file's own
//! header, because the player and this generator share no other clock: hls.js
//! zeroes its timeline at whichever fragment it happened to load first. The
//! player converts a scrub position to wall clock through
//! `#EXT-X-PROGRAM-DATE-TIME` on the media playlist, then to an offset from
//! this epoch. Anchoring cues to a media timeline instead would make the
//! preview wrong by an amount that depends on when the viewer joined.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::strip_rtp_header;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::replay::filmstrip::{CaptureSpec, MAX_BUFFER_BYTES, collect_buffer, try_capture_frame};

use super::upload::http_put;

/// Columns in a sprite sheet.
///
/// Sheets are laid out row-major in a fixed number of columns rather than one
/// long strip: a 100-frame strip at 160 px wide is 16 000 px across, past the
/// maximum texture size on plenty of the Android hardware this targets, and a
/// browser that refuses the image shows no preview at all.
const SHEET_COLUMNS: u32 = 10;

/// A frame captured for the track, with the wall clock it depicts.
struct CapturedFrame {
    at: DateTime<Utc>,
    jpeg: Vec<u8>,
}

/// One published sheet, retained so the index can describe it.
#[derive(Clone)]
pub(crate) struct SheetRecord {
    pub(crate) uri: String,
    /// Wall clock of the first frame on the sheet.
    pub(crate) first_at: DateTime<Utc>,
    /// Cadence the frames were captured at.
    pub(crate) interval: Duration,
    pub(crate) frame_count: u32,
    pub(crate) frame_width: u32,
    pub(crate) frame_height: u32,
}

/// Live counters, mirroring [`crate::replay::filmstrip::FilmstripStats`].
#[derive(Debug, Default)]
pub struct ThumbnailStats {
    pub frames_captured: AtomicU64,
    pub sheets_published: AtomicU64,
    pub bytes_published: AtomicU64,
    pub capture_drops: AtomicU64,
}

/// Compose captured frames into one sprite sheet.
///
/// Returns the encoded JPEG. Frames are decoded from their individual JPEGs
/// and re-encoded once as a sheet — the decode is of a 160x90 image and is
/// negligible beside the video decode that produced it, and it keeps this
/// module off the video engine's raw-frame API.
#[cfg(feature = "media-codecs")]
fn compose_sheet(
    frames: &[CapturedFrame],
    frame_w: u32,
    frame_h: u32,
) -> Result<Vec<u8>, String> {
    use image::{ImageEncoder, RgbImage};

    let (cols, rows) = sheet_grid(frames.len() as u32);
    let mut sheet = RgbImage::new(cols * frame_w, rows * frame_h);

    for (i, f) in frames.iter().enumerate() {
        let img = image::load_from_memory(&f.jpeg)
            .map_err(|e| format!("decode captured frame {i}: {e}"))?
            .to_rgb8();
        // The capture asked for exactly this size, but a decoder that rounded
        // to a macroblock would otherwise write outside the tile.
        let (x, y) = tile_origin(i as u32, frame_w, frame_h);
        for (px, py, pixel) in img.enumerate_pixels() {
            if px >= frame_w || py >= frame_h {
                continue;
            }
            sheet.put_pixel(x + px, y + py, *pixel);
        }
    }

    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
        .write_image(
            sheet.as_raw(),
            sheet.width(),
            sheet.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| format!("encode sheet: {e}"))?;
    Ok(out)
}

/// Grid shape for `n` frames: fixed columns, as many rows as needed.
fn sheet_grid(n: u32) -> (u32, u32) {
    if n == 0 {
        return (0, 0);
    }
    let cols = n.min(SHEET_COLUMNS);
    let rows = n.div_ceil(SHEET_COLUMNS);
    (cols, rows)
}

/// Top-left pixel of tile `i`.
fn tile_origin(i: u32, frame_w: u32, frame_h: u32) -> (u32, u32) {
    ((i % SHEET_COLUMNS) * frame_w, (i / SHEET_COLUMNS) * frame_h)
}

/// Render the WebVTT index over the sheets currently published.
///
/// Cue times are offsets from `epoch`, which is written into the file so the
/// player does not have to infer it. `epoch` is the first frame of the oldest
/// sheet still present — as sheets age out the epoch moves with them, exactly
/// as `#EXT-X-PROGRAM-DATE-TIME` moves with the playlist window.
pub(crate) fn render_vtt(sheets: &[SheetRecord]) -> String {
    let mut out = String::from("WEBVTT\n");
    let Some(epoch) = sheets.first().map(|s| s.first_at) else {
        return out;
    };
    out.push_str(&format!(
        "X-BILBYCAST-EPOCH: {}\n",
        epoch.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    ));

    for sheet in sheets {
        for i in 0..sheet.frame_count {
            let start = (sheet.first_at - epoch).num_milliseconds() as f64 / 1000.0
                + sheet.interval.as_secs_f64() * i as f64;
            let end = start + sheet.interval.as_secs_f64();
            let (x, y) = tile_origin(i, sheet.frame_width, sheet.frame_height);
            out.push_str(&format!(
                "\n{} --> {}\n{}#xywh={},{},{},{}\n",
                vtt_time(start),
                vtt_time(end),
                sheet.uri,
                x,
                y,
                sheet.frame_width,
                sheet.frame_height
            ));
        }
    }
    out
}

/// `HH:MM:SS.mmm`, the only timestamp form WebVTT cues take here.
fn vtt_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    let total_ms = (secs * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        total_s / 3600,
        (total_s % 3600) / 60,
        total_s % 60,
        ms
    )
}

/// Spawn the thumbnail track subscriber.
///
/// `interval` is the capture cadence, `frames_per_sheet` how many are packed
/// before a sheet is published, and `window` the DVR depth — a sheet is
/// dropped from the index once its newest frame is older than that.
///
/// Pruning by **age** rather than by a sheet count is deliberate. A count has
/// to be derived from the window, and the arithmetic is easy to get slightly
/// wrong in the direction that hurts: a list one sheet too long always names
/// an object the origin has already evicted, so the oldest stretch of the bar
/// is permanently blank. Age is the thing the origin actually evicts on.
#[allow(clippy::too_many_arguments)]
pub fn spawn_thumbnail_track(
    output_id: String,
    ingest_url: String,
    auth_token: Option<String>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    spec: CaptureSpec,
    interval: Duration,
    frames_per_sheet: u32,
    window: Duration,
    stats: Arc<ThumbnailStats>,
    events: EventSender,
    flow_id: String,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(thumbnail_loop(
        output_id,
        ingest_url,
        auth_token,
        rx,
        spec,
        interval,
        frames_per_sheet,
        window,
        stats,
        events,
        flow_id,
        cancel,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn thumbnail_loop(
    output_id: String,
    ingest_url: String,
    auth_token: Option<String>,
    mut rx: broadcast::Receiver<RtpPacket>,
    spec: CaptureSpec,
    interval: Duration,
    frames_per_sheet: u32,
    window: Duration,
    stats: Arc<ThumbnailStats>,
    events: EventSender,
    flow_id: String,
    cancel: CancellationToken,
) {
    let base = ingest_url.trim_end_matches('/').to_string();
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick

    let mut buffer: VecDeque<Bytes> = VecDeque::new();
    let mut buffer_bytes: usize = 0;
    let mut pending: Vec<CapturedFrame> = Vec::new();
    let mut sheets: VecDeque<SheetRecord> = VecDeque::new();
    let mut next_sheet: u64 = 0;
    let mut warned = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                if buffer.is_empty() {
                    continue;
                }
                let snapshot = collect_buffer(&buffer);
                match try_capture_frame(&snapshot, spec).await {
                    Ok(Some((_pts, jpeg))) => {
                        stats.frames_captured.fetch_add(1, Ordering::Relaxed);
                        pending.push(CapturedFrame { at: Utc::now(), jpeg });
                    }
                    Ok(None) => { stats.capture_drops.fetch_add(1, Ordering::Relaxed); }
                    Err(e) => {
                        stats.capture_drops.fetch_add(1, Ordering::Relaxed);
                        // One warning per output, not one per tick: a stream
                        // with no video yields nothing on every single tick.
                        if !warned {
                            warned = true;
                            events.emit_flow(
                                EventSeverity::Warning,
                                category::CMAF,
                                format!(
                                    "CMAF output '{output_id}': thumbnail capture failed \
                                     ({e}); the scrub preview will be missing"
                                ),
                                &flow_id,
                            );
                        }
                    }
                }

                if pending.len() as u32 >= frames_per_sheet {
                    publish_sheet(
                        &base,
                        auth_token.as_deref(),
                        &output_id,
                        &mut pending,
                        &mut sheets,
                        &mut next_sheet,
                        spec,
                        interval,
                        window,
                        &stats,
                    )
                    .await;
                }
            }
            recv = rx.recv() => match recv {
                Ok(pkt) => {
                    let stripped = strip_rtp_header(&pkt);
                    if stripped.is_empty() {
                        continue;
                    }
                    let bytes = Bytes::copy_from_slice(stripped);
                    buffer_bytes += bytes.len();
                    buffer.push_back(bytes);
                    while buffer_bytes > MAX_BUFFER_BYTES {
                        match buffer.pop_front() {
                            Some(old) => buffer_bytes -= old.len(),
                            None => break,
                        }
                    }
                }
                // Drop-on-lag: a late tick sees a slightly older buffer, which
                // costs one preview frame and nothing else.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// Compose, upload and index one sheet. Never propagates an error: a failed
/// sheet costs a preview, and the media output must not notice.
#[allow(clippy::too_many_arguments)]
async fn publish_sheet(
    base: &str,
    auth_token: Option<&str>,
    output_id: &str,
    pending: &mut Vec<CapturedFrame>,
    sheets: &mut VecDeque<SheetRecord>,
    next_sheet: &mut u64,
    spec: CaptureSpec,
    interval: Duration,
    window: Duration,
    stats: &ThumbnailStats,
) {
    let frames = std::mem::take(pending);
    let Some(first) = frames.first() else { return };
    let first_at = first.at;
    let count = frames.len() as u32;

    #[cfg(feature = "media-codecs")]
    let composed = tokio::task::spawn_blocking(move || {
        compose_sheet(&frames, spec.width, spec.height)
    })
    .await;
    #[cfg(not(feature = "media-codecs"))]
    let composed: Result<Result<Vec<u8>, String>, tokio::task::JoinError> = {
        let _ = &frames;
        Ok(Err("thumbnail track requires the `media-codecs` feature".into()))
    };

    let jpeg = match composed {
        Ok(Ok(j)) => j,
        Ok(Err(e)) => {
            tracing::warn!("CMAF output '{output_id}': sheet compose failed: {e}");
            stats.capture_drops.fetch_add(count as u64, Ordering::Relaxed);
            return;
        }
        Err(e) => {
            tracing::warn!("CMAF output '{output_id}': sheet compose panicked: {e}");
            stats.capture_drops.fetch_add(count as u64, Ordering::Relaxed);
            return;
        }
    };

    let uri = format!("thumbs-{:05}.jpg", *next_sheet);
    *next_sheet += 1;
    let url = format!("{base}/{uri}");
    if let Err(e) = http_put(&url, jpeg.clone(), "image/jpeg", auth_token).await {
        tracing::warn!("CMAF output '{output_id}': sheet PUT failed: {e}");
        stats.capture_drops.fetch_add(count as u64, Ordering::Relaxed);
        return;
    }
    stats.sheets_published.fetch_add(1, Ordering::Relaxed);
    stats.bytes_published.fetch_add(jpeg.len() as u64, Ordering::Relaxed);

    sheets.push_back(SheetRecord {
        uri,
        first_at,
        interval,
        frame_count: count,
        frame_width: spec.width,
        frame_height: spec.height,
    });
    // The index must describe only what the origin still holds. A cue naming
    // an evicted sheet is a broken image at exactly the moment the operator
    // is looking for a picture.
    prune_expired(sheets, Utc::now(), window);

    let vtt = render_vtt(sheets.make_contiguous());
    if let Err(e) = http_put(
        &format!("{base}/thumbs.vtt"),
        vtt.into_bytes(),
        "text/vtt",
        auth_token,
    )
    .await
    {
        tracing::warn!("CMAF output '{output_id}': thumbnail index PUT failed: {e}");
    }
}

/// Drop sheets whose newest frame has aged out of the DVR window.
pub(crate) fn prune_expired(
    sheets: &mut VecDeque<SheetRecord>,
    now: DateTime<Utc>,
    window: Duration,
) {
    let Ok(window) = chrono::Duration::from_std(window) else {
        return;
    };
    while let Some(front) = sheets.front() {
        let last_frame = front.first_at
            + chrono::Duration::milliseconds(
                (front.interval.as_secs_f64() * 1000.0 * (front.frame_count.saturating_sub(1)) as f64)
                    as i64,
            );
        if now - last_frame > window {
            sheets.pop_front();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_787_000_000 + secs, 0).unwrap()
    }

    fn sheet(uri: &str, first: i64, frames: u32) -> SheetRecord {
        SheetRecord {
            uri: uri.to_string(),
            first_at: at(first),
            interval: Duration::from_secs(2),
            frame_count: frames,
            frame_width: 160,
            frame_height: 90,
        }
    }

    /// A sheet must not be one long strip.
    ///
    /// 100 frames at 160 px is 16 000 px across, past the maximum texture
    /// size on plenty of Android hardware. The browser refuses the image and
    /// the preview shows nothing at all — with no error the player can see.
    #[test]
    fn a_sheet_stays_within_a_sane_texture_size() {
        let (cols, rows) = sheet_grid(100);
        assert_eq!((cols, rows), (10, 10));
        assert!(cols * 160 <= 4096, "sheet is {} px wide", cols * 160);
        // Partial sheets keep the same column count, so tile maths is one rule.
        assert_eq!(sheet_grid(7), (7, 1));
        assert_eq!(sheet_grid(23), (10, 3));
        assert_eq!(sheet_grid(0), (0, 0));
    }

    /// Tiles must not overlap, or every preview shows a neighbouring frame.
    #[test]
    fn tiles_are_laid_out_row_major_without_overlap() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..100 {
            assert!(seen.insert(tile_origin(i, 160, 90)), "tile {i} collides");
        }
        assert_eq!(tile_origin(0, 160, 90), (0, 0));
        assert_eq!(tile_origin(9, 160, 90), (1440, 0));
        assert_eq!(tile_origin(10, 160, 90), (0, 90));
    }

    /// Cue times must be offsets from the epoch the file itself declares.
    ///
    /// The player has no other way to relate a cue to a picture: hls.js zeroes
    /// its timeline at whichever fragment it loaded first, so a cue anchored
    /// to a media timeline is wrong by an amount that depends on when the
    /// viewer joined — and wrong silently, showing a plausible frame from the
    /// wrong moment.
    #[test]
    fn cue_times_are_offsets_from_the_declared_epoch() {
        let vtt = render_vtt(&[sheet("thumbs-00000.jpg", 0, 3)]);
        assert!(
            vtt.contains("X-BILBYCAST-EPOCH: 2026-08-17T"),
            "no epoch declared: {vtt}"
        );
        assert!(vtt.contains("00:00:00.000 --> 00:00:02.000"), "{vtt}");
        assert!(vtt.contains("00:00:04.000 --> 00:00:06.000"), "{vtt}");
        assert!(
            vtt.contains("thumbs-00000.jpg#xywh=0,0,160,90"),
            "no sprite region: {vtt}"
        );
        assert!(vtt.contains("thumbs-00000.jpg#xywh=320,0,160,90"), "{vtt}");
    }

    /// The epoch moves with the window, and later sheets stay relative to it.
    ///
    /// Sheets age out of the origin. If the epoch stayed at the first sheet
    /// ever published, every cue would drift further from the picture it
    /// names for as long as the session ran — the same failure the playlist's
    /// `#EXT-X-PROGRAM-DATE-TIME` avoids by moving with the window.
    #[test]
    fn the_epoch_follows_the_window_and_later_sheets_stay_relative_to_it() {
        let all = [
            sheet("thumbs-00000.jpg", 0, 2),
            sheet("thumbs-00001.jpg", 4, 2),
            sheet("thumbs-00002.jpg", 8, 2),
        ];
        let full = render_vtt(&all);
        assert!(full.contains("X-BILBYCAST-EPOCH: 2026-08-17T20:53:20.000Z"), "{full}");
        // Third sheet starts 8 s after the epoch.
        assert!(full.contains("00:00:08.000 --> 00:00:10.000"), "{full}");

        // Drop the oldest, as eviction does.
        let rolled = render_vtt(&all[1..]);
        assert!(
            rolled.contains("X-BILBYCAST-EPOCH: 2026-08-17T20:53:24.000Z"),
            "epoch did not move with the window: {rolled}"
        );
        assert!(
            rolled.contains("thumbs-00001.jpg#xywh=0,0,160,90"),
            "{rolled}"
        );
        // ...and the now-first sheet starts at zero again.
        assert!(rolled.contains("00:00:00.000 --> 00:00:02.000"), "{rolled}");
        assert!(
            !rolled.contains("thumbs-00000.jpg"),
            "index still names an evicted sheet: {rolled}"
        );
    }

    /// A sheet must leave the index no later than the origin evicts it.
    ///
    /// This was a count derived from the window, and the arithmetic was one
    /// sheet too generous: 11 sheets of 30 s against a 300 s window meant the
    /// oldest was always already gone. Measured on the live rig — 11 named,
    /// 10 present — and the symptom is the oldest stretch of the scrub bar
    /// permanently blank while the index insists it is covered. Erring the
    /// other way costs nothing: one sheet of preview dropped slightly early.
    #[test]
    fn a_sheet_leaves_the_index_before_the_origin_evicts_it() {
        let window = Duration::from_secs(300);
        let mut sheets: VecDeque<SheetRecord> = (0..11)
            .map(|i| sheet(&format!("thumbs-{i:05}.jpg"), i * 30, 15))
            .collect();

        // `now` is the moment after the last sheet's final frame.
        let now = at(10 * 30 + 28);
        prune_expired(&mut sheets, now, window);

        // The property is about the *object*, not its first frame. A sheet is
        // stored on the origin when it is published — that is, when its last
        // frame was captured — and evicted `retention` after that. So its
        // oldest frame is legitimately older than the window by the sheet's
        // own span, and asserting on `first_at` here would condemn correct
        // behaviour.
        let oldest = sheets.front().expect("some sheets must survive");
        let last_frame = oldest.first_at
            + chrono::Duration::seconds(
                (oldest.interval.as_secs() * (oldest.frame_count - 1) as u64) as i64,
            );
        let age = (now - last_frame).num_seconds();
        assert!(
            age <= 300,
            "oldest sheet was published {age}s ago against a 300s window — the origin has it gone"
        );
        assert!(sheets.len() >= 9, "pruned far more than the window needed: {}", sheets.len());

        // The boundary itself: a sheet published just past the window must be
        // gone, and one published just inside must stay. Without a case in
        // this band, a bound that is merely *close* passes.
        let mut band: VecDeque<SheetRecord> = vec![
            sheet("thumbs-00100.jpg", 0, 15),   // published at +28
            sheet("thumbs-00101.jpg", 30, 15),  // published at +58
        ]
        .into();
        prune_expired(&mut band, at(28 + 301), window);
        assert_eq!(
            band.len(),
            1,
            "a sheet published 301s ago is still named against a 300s window"
        );
        assert_eq!(band.front().unwrap().uri, "thumbs-00101.jpg");

        // Nothing survives a window it is wholly outside of.
        let mut old: VecDeque<SheetRecord> = vec![sheet("thumbs-00000.jpg", 0, 15)].into();
        prune_expired(&mut old, at(1000), window);
        assert!(old.is_empty(), "a sheet an age past the window was kept");

        // And a fresh sheet is never dropped.
        let mut fresh: VecDeque<SheetRecord> = vec![sheet("thumbs-00042.jpg", 900, 15)].into();
        prune_expired(&mut fresh, at(928), window);
        assert_eq!(fresh.len(), 1, "the newest sheet was pruned");
    }

    /// No sheets, no index — rather than a header promising cues that are not
    /// there.
    #[test]
    fn an_empty_index_declares_no_epoch() {
        let vtt = render_vtt(&[]);
        assert_eq!(vtt, "WEBVTT\n");
    }

    #[test]
    fn vtt_timestamps_carry_hours_and_milliseconds() {
        assert_eq!(vtt_time(0.0), "00:00:00.000");
        assert_eq!(vtt_time(1.5), "00:00:01.500");
        assert_eq!(vtt_time(3661.25), "01:01:01.250");
        // A three-hour DVR window is the stated target, so hours must not wrap.
        assert_eq!(vtt_time(10_800.0), "03:00:00.000");
    }
}
