// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Thumbnail generation module.
//!
//! Subscribes to a flow's broadcast channel as an independent consumer,
//! buffers recent MPEG-TS packets, and periodically pipes them to an
//! ffmpeg subprocess to produce a small JPEG thumbnail. Like the media
//! analyzer, this module **cannot** block the hot path — if it falls
//! behind, it receives `Lagged(n)` and silently skips packets.
//!
//! Thumbnail generation is optional: it requires ffmpeg to be installed
//! on the device. When ffmpeg is not available, this task is never spawned.
//!
//! ## Freeze-frame and black-screen detection
//!
//! After each JPEG capture the module performs two lightweight checks:
//!
//! 1. **Freeze detection** — the JPEG bytes are hashed and compared with
//!    the previous capture. Three consecutive identical hashes (~30 s at
//!    the default 10 s interval) raise a `"frozen"` alarm.
//!
//! 2. **Black-screen detection** — a secondary ffmpeg invocation decodes
//!    the JPEG to a tiny 4×4 grayscale image (16 bytes). If the average
//!    luminance falls below a threshold the `"black"` alarm is raised.
//!
//! Alarm state is stored in the [`ThumbnailAccumulator`] and included in
//! the per-flow stats snapshot sent to the manager every second.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::stats::collector::ThumbnailAccumulator;

use super::packet::RtpPacket;
use super::ts_parse::strip_rtp_header;
use super::ts_program_filter::TsProgramFilter;

/// How often to generate a thumbnail (seconds).
const THUMBNAIL_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum TS data to buffer (~3 seconds at 10 Mbps ≈ 3.75 MB).
const MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Thumbnail output dimensions.
const THUMBNAIL_WIDTH: u32 = 320;
const THUMBNAIL_HEIGHT: u32 = 180;

/// ffmpeg subprocess timeout.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(5);

/// MPEG-TS sync byte.
const TS_SYNC_BYTE: u8 = 0x47;

/// MPEG-TS packet size.
const TS_PACKET_SIZE: usize = 188;

/// Average luminance (0–255) below which a frame is considered black.
const BLACK_LUMINANCE_THRESHOLD: f64 = 16.0;

/// Timeout for the lightweight black-detection ffmpeg invocation.
const BLACK_DETECT_TIMEOUT: Duration = Duration::from_secs(3);

// ── Public API ───────────────────────────────────────────────────────────

/// Spawn the thumbnail generator as an independent broadcast subscriber.
///
/// `program_number` selects which MPEG-TS program to render when the input
/// is an MPTS. `None` keeps the current behaviour (raw TS to ffmpeg, which
/// picks the first program); `Some(N)` runs the buffered TS through a
/// `TsProgramFilter` so ffmpeg only sees the elementary streams of program N.
pub fn spawn_thumbnail_generator(
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<ThumbnailAccumulator>,
    cancel: CancellationToken,
    program_number: Option<u16>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(thumbnail_loop(rx, stats, cancel, program_number))
}

/// Check whether ffmpeg is available on this system.
pub fn check_ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── Thumbnail Loop ──────────────────────────────────────────────────────

async fn thumbnail_loop(
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<ThumbnailAccumulator>,
    cancel: CancellationToken,
    program_number: Option<u16>,
) {
    if let Some(n) = program_number {
        tracing::info!("Thumbnail generator started (program filter target = {})", n);
    } else {
        tracing::info!("Thumbnail generator started");
    }

    let mut interval = tokio::time::interval(THUMBNAIL_INTERVAL);
    interval.tick().await; // consume first immediate tick

    // Ring buffer of recent TS packet data
    let mut ts_buffer: VecDeque<Bytes> = VecDeque::new();
    let mut buffer_bytes: usize = 0;
    let mut program_filter = program_number.map(TsProgramFilter::new);
    let mut filter_scratch: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("Thumbnail generator stopping (cancelled)");
                break;
            }

            _ = interval.tick() => {
                if buffer_bytes > 0 {
                    // Collect buffered TS data and generate thumbnail.
                    // When a program filter is set, run the buffer through
                    // it first so ffmpeg only sees the selected program.
                    let raw_ts = collect_buffer(&ts_buffer);
                    let ts_data: Bytes = if let Some(ref mut filter) = program_filter {
                        filter_scratch.clear();
                        filter.filter_into(&raw_ts, &mut filter_scratch);
                        if filter_scratch.is_empty() {
                            // Selected program isn't in this buffer window —
                            // wait for the next tick.
                            continue;
                        }
                        Bytes::copy_from_slice(&filter_scratch)
                    } else {
                        raw_ts
                    };
                    match generate_thumbnail(&ts_data).await {
                        Ok(jpeg) => {
                            tracing::debug!(
                                "Thumbnail captured: {} bytes JPEG from {} bytes TS",
                                jpeg.len(),
                                ts_data.len()
                            );
                            stats.store(jpeg.clone());

                            // ── Freeze / black detection ──
                            let jpeg_hash = hash_jpeg(&jpeg);
                            let is_frozen = stats.check_freeze(jpeg_hash);
                            let is_black = detect_black_screen(&jpeg).await;

                            if is_black {
                                stats.set_alarm(Some("black".to_string()));
                            } else if is_frozen {
                                stats.set_alarm(Some("frozen".to_string()));
                            } else {
                                stats.set_alarm(None);
                            }
                        }
                        Err(e) => {
                            tracing::debug!("Thumbnail capture failed: {e}");
                            stats.record_error();
                        }
                    }
                }
            }

            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        let ts_payload = strip_rtp_header(&packet);
                        if ts_payload.is_empty() {
                            continue;
                        }

                        // Only buffer data that looks like valid MPEG-TS
                        if ts_payload.len() >= TS_PACKET_SIZE && ts_payload[0] == TS_SYNC_BYTE {
                            let chunk = Bytes::copy_from_slice(ts_payload);
                            let chunk_len = chunk.len();
                            ts_buffer.push_back(chunk);
                            buffer_bytes += chunk_len;

                            // Evict old data to stay within budget
                            while buffer_bytes > MAX_BUFFER_BYTES {
                                if let Some(old) = ts_buffer.pop_front() {
                                    buffer_bytes -= old.len();
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("Thumbnail generator lagged, skipped {n} packets");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("Thumbnail generator: broadcast channel closed");
                        break;
                    }
                }
            }
        }
    }
}

/// Concatenate the ring buffer into a single contiguous byte slice.
fn collect_buffer(buffer: &VecDeque<Bytes>) -> Bytes {
    let total: usize = buffer.iter().map(|b| b.len()).sum();
    let mut out = BytesMut::with_capacity(total);
    for chunk in buffer {
        out.extend_from_slice(chunk);
    }
    out.freeze()
}

/// Spawn ffmpeg to extract a single JPEG frame from MPEG-TS data.
async fn generate_thumbnail(ts_data: &[u8]) -> Result<Bytes, String> {
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-f", "mpegts",
            "-i", "pipe:0",
            "-vframes", "1",
            "-s", &format!("{THUMBNAIL_WIDTH}x{THUMBNAIL_HEIGHT}"),
            "-q:v", "5",
            "-f", "mjpeg",
            "-an",             // no audio
            "-loglevel", "error",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;

    // Write TS data to ffmpeg stdin
    if let Some(mut stdin) = child.stdin.take() {
        let ts_bytes = Bytes::copy_from_slice(ts_data);
        tokio::spawn(async move {
            let _ = stdin.write_all(&ts_bytes).await;
            let _ = stdin.shutdown().await;
        });
    }

    // Wait for ffmpeg with timeout
    let output = tokio::time::timeout(FFMPEG_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "ffmpeg timed out".to_string())?
        .map_err(|e| format!("ffmpeg failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg exited with {}: {}", output.status, stderr.trim()));
    }

    if output.stdout.is_empty() {
        return Err("ffmpeg produced no output".to_string());
    }

    Ok(Bytes::from(output.stdout))
}

// ── Detection Helpers ───────────────────────────────────────────────────

/// Compute a 64-bit hash of the JPEG bytes for freeze-frame comparison.
///
/// Identical video frames produce identical JPEG output from the same
/// ffmpeg settings, so a byte-level hash is a reliable equality test.
fn hash_jpeg(data: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

/// Detect whether the JPEG thumbnail depicts a (near-)black frame.
///
/// Pipes the JPEG through a lightweight ffmpeg invocation that outputs a
/// tiny 4×4 grayscale raw image (16 bytes). If the average luminance of
/// those 16 samples is below [`BLACK_LUMINANCE_THRESHOLD`] the frame is
/// considered black.
async fn detect_black_screen(jpeg: &[u8]) -> bool {
    let result = detect_black_screen_inner(jpeg).await;
    match result {
        Ok(is_black) => is_black,
        Err(e) => {
            tracing::debug!("Black-screen detection failed: {e}");
            false
        }
    }
}

async fn detect_black_screen_inner(jpeg: &[u8]) -> Result<bool, String> {
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-f", "mjpeg",
            "-i", "pipe:0",
            "-vframes", "1",
            "-s", "4x4",
            "-pix_fmt", "gray",
            "-f", "rawvideo",
            "-loglevel", "error",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg for black detection: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let jpeg_bytes = Bytes::copy_from_slice(jpeg);
        tokio::spawn(async move {
            let _ = stdin.write_all(&jpeg_bytes).await;
            let _ = stdin.shutdown().await;
        });
    }

    let output = tokio::time::timeout(BLACK_DETECT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "black-detect ffmpeg timed out".to_string())?
        .map_err(|e| format!("black-detect ffmpeg failed: {e}"))?;

    if !output.status.success() || output.stdout.is_empty() {
        return Err("black-detect ffmpeg produced no output".to_string());
    }

    // output.stdout contains raw grayscale pixels (expected: 16 bytes for 4x4)
    let pixels = &output.stdout;
    if pixels.is_empty() {
        return Ok(false);
    }

    let sum: f64 = pixels.iter().map(|&p| p as f64).sum();
    let avg = sum / pixels.len() as f64;

    Ok(avg < BLACK_LUMINANCE_THRESHOLD)
}
