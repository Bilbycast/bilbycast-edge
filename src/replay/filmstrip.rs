// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Filmstrip thumbnail writer — Stage 2 of the replay UX plan.
//!
//! Sibling broadcast subscriber on a recording flow's `broadcast_tx`. Once
//! every `filmstrip_seconds`, decodes one frame from the buffered TS and
//! writes a small JPEG to `<recording_dir>/thumbs/<pts_90khz>.jpg`. The
//! manager `/replay` page fetches a windowed batch via the new
//! `list_filmstrip` WS command and renders an image strip behind the
//! scrubber timeline canvas — closing the "operator can't see what
//! they're scrubbing" gap from Stage 1.
//!
//! # Discipline
//!
//! Same shape as the recording writer next to it ([`super::writer`]):
//!
//! - Sibling broadcast subscriber, drop-on-`Lagged`, never blocks the
//!   data path.
//! - Bounded buffer of recent TS bytes (~3 s) so a recent IDR is always
//!   in scope when the cadence tick fires. Mirrors the live thumbnail
//!   generator's [`MAX_BUFFER_BYTES`] budget — proportional to the
//!   stream bitrate, not unbounded.
//! - Decode + scale + JPEG-encode under [`tokio::task::spawn_blocking`]
//!   via the in-process `video-engine` (FFmpeg libavcodec/libswscale +
//!   HW-accel where the host probe says so). Same path the live
//!   thumbnail uses — no parallel pipeline.
//! - Atomic file write: encode → write to `<thumbs>/.tmp/<pts>.jpg` →
//!   rename onto `<thumbs>/<pts>.jpg`. SIGKILL-safe — leftover `.tmp/`
//!   entries are unlinked on the next writer init scan.
//! - Decode failures bump a counter and emit a rate-limited Warning
//!   event under category `replay`; the recording itself never tears
//!   down for a thumbnail-decode error.
//!
//! Disabled by default (opt-in via `FlowConfig.recording.filmstrip_seconds`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::{first_pcr_in_ts_buffer, strip_rtp_header};
use crate::manager::events::{EventSender, EventSeverity, category};

/// Filmstrip JPEG dimensions. Half the live-thumbnail's 320×180 — small
/// enough that 12 frames spanning a scrub window fit in ~120 KB on the
/// wire, large enough to identify a play visually.
const FRAME_WIDTH: u32 = 160;
const FRAME_HEIGHT: u32 = 90;

/// JPEG quality (1=highest, 31=lowest, libavcodec scale). 5 matches
/// `engine::thumbnail` — well within "look like the live picture" range
/// without exploding bytes-per-frame.
#[cfg(feature = "video-thumbnail")]
const FRAME_QUALITY: u32 = 5;

/// TS ring buffer cap. Same budget as the live thumbnail generator — sized
/// so a single typical-GOP keyframe is always in scope. ~32 MB ≈ 12 s at
/// 21 Mbps, ≈ 25 s at 10 Mbps. One allocation per recording, not per
/// packet.
const MAX_BUFFER_BYTES: usize = 32 * 1024 * 1024;

/// Minimum gap between `replay_filmstrip_decode_failed` events when the
/// decoder is continuously rejecting buffered TS (e.g. an audio-only
/// flow that won't ever yield video frames). Without this every cadence
/// tick spams the events feed.
const DECODE_FAIL_EVENT_INTERVAL: Duration = Duration::from_secs(60);

/// Live counters surfaced on `RecordingStats` (and through to the
/// manager's flow-card stats snapshot). `frames_written` lets the UI
/// confirm the writer is making progress; `decode_drops` surfaces a
/// stuck filmstrip without polluting the events feed.
#[derive(Debug, Default)]
pub struct FilmstripStats {
    pub frames_written: AtomicU64,
    pub bytes_written: AtomicU64,
    pub decode_drops: AtomicU64,
}

/// Spawn the filmstrip subscriber.
///
/// `interval_seconds` is the validated cadence (1–30 s). `recording_dir`
/// is the per-recording directory; `<recording_dir>/thumbs/` is created
/// on demand. The returned JoinHandle is held by the recording handle —
/// shutdown is driven by `cancel`, not by aborting it.
pub fn spawn_filmstrip_writer(
    recording_id: String,
    recording_dir: PathBuf,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    interval_seconds: u32,
    stats: Arc<FilmstripStats>,
    events: EventSender,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(filmstrip_loop(
        recording_id,
        recording_dir,
        rx,
        interval_seconds,
        stats,
        events,
        cancel,
    ))
}

async fn filmstrip_loop(
    recording_id: String,
    recording_dir: PathBuf,
    mut rx: broadcast::Receiver<RtpPacket>,
    interval_seconds: u32,
    stats: Arc<FilmstripStats>,
    events: EventSender,
    cancel: CancellationToken,
) {
    let thumbs_dir = recording_dir.join("thumbs");
    let staging = thumbs_dir.join(".tmp");
    if let Err(e) = tokio::fs::create_dir_all(&staging).await {
        events.emit_with_details(
            EventSeverity::Warning,
            category::REPLAY,
            format!(
                "Filmstrip writer for '{recording_id}' could not create thumbs dir: {e}"
            ),
            None,
            serde_json::json!({
                "error_code": "replay_filmstrip_setup_failed",
                "replay_event": "filmstrip_setup_failed",
                "recording_id": recording_id,
            }),
        );
        return;
    }
    // Crash-recovery scan: unlink any `.tmp/*` orphans from a SIGKILL
    // mid-encode. Unlike the segment writer's recovery, we don't need
    // to derive a "next id" — filmstrip frames are PTS-keyed, so the
    // next tick will pick a fresh filename naturally.
    let recovered = recover_tmp_orphans(&staging).await;
    if recovered > 0 {
        tracing::info!(
            "Filmstrip writer '{recording_id}' cleaned {recovered} .tmp orphan(s) on init"
        );
    }

    let mut interval = tokio::time::interval(Duration::from_secs(interval_seconds as u64));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // consume immediate first tick

    let mut buffer: VecDeque<Bytes> = VecDeque::new();
    let mut buffer_bytes: usize = 0;
    let mut last_decode_fail_event: Option<Instant> = None;
    // Skip the very first tick if we have no buffer yet — common at
    // flow startup before any TS has arrived.
    let mut have_data: bool = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                if !have_data || buffer.is_empty() {
                    continue;
                }
                let snapshot = collect_buffer(&buffer);
                match try_capture_frame(&snapshot).await {
                    Ok(Some((pts_90khz, jpeg))) => {
                        let path = thumbs_dir.join(format!("{pts_90khz}.jpg"));
                        if path.exists() {
                            // Same PCR window as the previous tick (the
                            // source clock didn't advance, e.g. a
                            // stalled stream). No-op rather than
                            // overwrite.
                            continue;
                        }
                        let staging_path = staging.join(format!("{pts_90khz}.jpg"));
                        match write_atomic(&staging_path, &path, &jpeg).await {
                            Ok(()) => {
                                stats.frames_written.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_written.fetch_add(jpeg.len() as u64, Ordering::Relaxed);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Filmstrip writer '{recording_id}' write failed: {e}"
                                );
                                stats.decode_drops.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Ok(None) => {
                        // No video PID found in the buffered TS — audio-only
                        // flow, or video PID still warming up. Rate-limited
                        // event so we surface a stuck filmstrip without
                        // flooding.
                        stats.decode_drops.fetch_add(1, Ordering::Relaxed);
                        emit_decode_fail_event(
                            &events,
                            &recording_id,
                            "no video stream in buffered TS",
                            &mut last_decode_fail_event,
                        );
                    }
                    Err(e) => {
                        stats.decode_drops.fetch_add(1, Ordering::Relaxed);
                        emit_decode_fail_event(
                            &events,
                            &recording_id,
                            &format!("decode error: {e}"),
                            &mut last_decode_fail_event,
                        );
                    }
                }
            }
            recv = rx.recv() => match recv {
                Ok(pkt) => {
                    // `strip_rtp_header` returns the raw TS slice for
                    // both raw-TS (returns the whole `data`) and
                    // RTP-wrapped (skips the variable header). Empty
                    // slice means a malformed RTP packet — drop it.
                    let stripped = strip_rtp_header(&pkt);
                    if stripped.is_empty() {
                        continue;
                    }
                    let bytes = Bytes::copy_from_slice(stripped);
                    buffer_bytes += bytes.len();
                    buffer.push_back(bytes);
                    have_data = true;
                    while buffer_bytes > MAX_BUFFER_BYTES {
                        if let Some(old) = buffer.pop_front() {
                            buffer_bytes -= old.len();
                        } else {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Drop-on-lag: we don't bump packets_dropped here —
                    // the recording writer next door does that authoritatively
                    // for the recording itself. Filmstrip lag just means a
                    // tick or two will see an older buffer; not worth a
                    // separate counter.
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// Concatenate the ring buffer into one contiguous slice for the
/// extraction + decode path. Same idiom as [`crate::engine::thumbnail`].
fn collect_buffer(buffer: &VecDeque<Bytes>) -> Bytes {
    let total: usize = buffer.iter().map(|b| b.len()).sum();
    let mut out = BytesMut::with_capacity(total);
    for chunk in buffer {
        out.extend_from_slice(chunk);
    }
    out.freeze()
}

/// Try to decode + JPEG-encode one frame from the snapshot, returning
/// `(pts_90khz, jpeg_bytes)` on success. `Ok(None)` means "no video
/// data in the buffer" (audio-only / warm-up); `Err` is a hard decoder
/// failure.
async fn try_capture_frame(ts_data: &Bytes) -> Result<Option<(u64, Vec<u8>)>, String> {
    // Snap the filename PTS to the first PCR observed in the buffer.
    // PCR is in 27 MHz ticks; divide by 300 → 90 kHz PTS units, matching
    // the rest of the system (`index.bin`, `clips.json`, etc.).
    let pts_27mhz = match first_pcr_in_ts_buffer(ts_data) {
        Some(p) => p,
        None => return Ok(None),
    };
    let pts_90khz = pts_27mhz / 300;

    #[cfg(feature = "video-thumbnail")]
    {
        let bytes = ts_data.to_vec();
        let jpeg = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, String> {
            use video_codec::ThumbnailConfig;
            let extracted = match crate::engine::thumbnail::extract_video_from_ts(&bytes) {
                Some(v) => v,
                None => return Ok(None),
            };
            let cfg = ThumbnailConfig {
                width: FRAME_WIDTH,
                height: FRAME_HEIGHT,
                quality: FRAME_QUALITY,
            };
            let result = video_engine::decode_thumbnail(&extracted.annex_b_data, extracted.codec, &cfg)
                .map_err(|e| format!("decode_thumbnail: {e}"))?;
            Ok(Some(result.jpeg.to_vec()))
        })
        .await
        .map_err(|e| format!("spawn_blocking panicked: {e}"))??;
        match jpeg {
            Some(j) => Ok(Some((pts_90khz, j))),
            None => Ok(None),
        }
    }
    #[cfg(not(feature = "video-thumbnail"))]
    {
        // Subprocess fallback isn't worth carrying for the filmstrip —
        // operators on `--no-default-features` builds simply won't get
        // filmstrip thumbnails (the live-thumbnail subprocess fallback
        // is enough for the live header). Treat as "not implemented"
        // rather than spawning a parallel ffmpeg per cadence tick.
        let _ = ts_data;
        let _ = pts_90khz;
        Err("filmstrip requires the `video-thumbnail` feature".to_string())
    }
}

/// Atomic JPEG write: write to staging path, fsync, rename onto final
/// path. Same idiom as `crate::media::write_atomic` and the segment
/// writer's roll path. Caller is responsible for ensuring the staging
/// directory exists.
async fn write_atomic(staging_path: &Path, final_path: &Path, jpeg: &[u8]) -> std::io::Result<()> {
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(staging_path)
            .await?;
        f.write_all(jpeg).await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(staging_path, final_path).await?;
    Ok(())
}

/// Scan the filmstrip staging directory for left-over `.tmp/*` files
/// from a SIGKILL mid-encode and unlink them. Returns the count for
/// observability.
async fn recover_tmp_orphans(staging: &Path) -> usize {
    let mut removed = 0usize;
    let mut rd = match tokio::fs::read_dir(staging).await {
        Ok(rd) => rd,
        Err(_) => return 0,
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let _ = tokio::fs::remove_file(ent.path()).await;
        removed += 1;
    }
    removed
}

/// Emit a Warning event under category `replay` for a sustained decode
/// failure, throttled by [`DECODE_FAIL_EVENT_INTERVAL`].
fn emit_decode_fail_event(
    events: &EventSender,
    recording_id: &str,
    detail: &str,
    last: &mut Option<Instant>,
) {
    let now = Instant::now();
    let should_emit = last.map(|t| now.duration_since(t) > DECODE_FAIL_EVENT_INTERVAL).unwrap_or(true);
    if !should_emit {
        return;
    }
    *last = Some(now);
    events.emit_with_details(
        EventSeverity::Warning,
        category::REPLAY,
        format!("Filmstrip writer for '{recording_id}' decode failure: {detail}"),
        None,
        serde_json::json!({
            "error_code": "replay_filmstrip_decode_failed",
            "replay_event": "filmstrip_decode_failed",
            "recording_id": recording_id,
            "detail": detail,
        }),
    );
}

/// Public read-side helper used by the WS `list_filmstrip` handler.
/// Walks `<recording_dir>/thumbs/` and returns `(pts_90khz, file_size)`
/// pairs in ascending PTS order, optionally clipped to a window. The
/// caller is responsible for slicing further (e.g. evenly-spaced N
/// across the window) and reading the JPEG bytes.
pub async fn list_frames(
    recording_dir: &Path,
    from_pts: Option<u64>,
    to_pts: Option<u64>,
) -> std::io::Result<Vec<(u64, u64)>> {
    let dir = recording_dir.join("thumbs");
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out: Vec<(u64, u64)> = Vec::new();
    while let Ok(Some(ent)) = rd.next_entry().await {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jpg") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let pts: u64 = match stem.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Some(lo) = from_pts {
            if pts < lo { continue; }
        }
        if let Some(hi) = to_pts {
            if pts > hi { continue; }
        }
        let size = ent.metadata().await.map(|m| m.len()).unwrap_or(0);
        out.push((pts, size));
    }
    out.sort_by_key(|(pts, _)| *pts);
    Ok(out)
}

/// Read one filmstrip JPEG by its exact PTS. Used by the WS
/// `get_filmstrip_frame` handler.
pub async fn read_frame(recording_dir: &Path, pts_90khz: u64) -> std::io::Result<Vec<u8>> {
    let path = recording_dir.join("thumbs").join(format!("{pts_90khz}.jpg"));
    tokio::fs::read(&path).await
}
