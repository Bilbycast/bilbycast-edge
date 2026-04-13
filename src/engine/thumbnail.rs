// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Thumbnail generation module.
//!
//! Subscribes to a flow's broadcast channel as an independent consumer,
//! buffers recent MPEG-TS packets, and periodically generates a small JPEG
//! thumbnail. Like the media analyzer, this module **cannot** block the hot
//! path — if it falls behind, it receives `Lagged(n)` and silently skips
//! packets.
//!
//! ## Backend selection
//!
//! When the `video-thumbnail` feature is enabled (default), thumbnails are
//! generated **in-process** via `video-engine` (FFmpeg libavcodec/libswscale).
//! Video NAL units are extracted from the buffered MPEG-TS, decoded to a raw
//! frame, scaled, and encoded as JPEG — all without spawning a subprocess.
//!
//! When the feature is disabled, the module falls back to piping the buffered
//! TS data to an external **ffmpeg subprocess** (the legacy path), which
//! requires ffmpeg to be installed on the device.
//!
//! ## Freeze-frame and black-screen detection
//!
//! After each JPEG capture the module performs two lightweight checks:
//!
//! 1. **Freeze detection** — the JPEG bytes are hashed and compared with
//!    the previous capture. Three consecutive identical hashes (~30 s at
//!    the default 10 s interval) raise a `"frozen"` alarm.
//!
//! 2. **Black-screen detection** — when in-process, the average luminance
//!    is computed from the decoded Y plane (no second subprocess). When
//!    using the ffmpeg subprocess fallback, a secondary ffmpeg invocation
//!    decodes the JPEG to a tiny 4×4 grayscale image.
//!
//! Alarm state is stored in the [`ThumbnailAccumulator`] and included in
//! the per-flow stats snapshot sent to the manager every second.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::stats::collector::ThumbnailAccumulator;

use super::packet::RtpPacket;
use super::ts_parse::strip_rtp_header;
use super::ts_program_filter::TsProgramFilter;

/// How often to generate a thumbnail (seconds).
const THUMBNAIL_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum TS data to buffer (~3 seconds at 10 Mbps ≈ 3.75 MB).
const MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Thumbnail output dimensions.
const THUMBNAIL_WIDTH: u32 = 320;
const THUMBNAIL_HEIGHT: u32 = 180;

/// MPEG-TS sync byte.
const TS_SYNC_BYTE: u8 = 0x47;

/// MPEG-TS packet size.
const TS_PACKET_SIZE: usize = 188;

/// Average luminance (0–255) below which a frame is considered black.
const BLACK_LUMINANCE_THRESHOLD: f64 = 16.0;

// ── Subprocess fallback constants (used when video-thumbnail is disabled) ──

/// ffmpeg subprocess timeout.
#[cfg(not(feature = "video-thumbnail"))]
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for the lightweight black-detection ffmpeg invocation.
#[cfg(not(feature = "video-thumbnail"))]
const BLACK_DETECT_TIMEOUT: Duration = Duration::from_secs(3);

// ── Public API ───────────────────────────────────────────────────────────

/// Spawn the thumbnail generator as an independent broadcast subscriber.
///
/// `program_number` selects which MPEG-TS program to render when the input
/// is an MPTS. `None` keeps the default (first program); `Some(N)` filters
/// the buffered TS to only show program N.
pub fn spawn_thumbnail_generator(
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<ThumbnailAccumulator>,
    cancel: CancellationToken,
    program_number: Option<u16>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(thumbnail_loop(rx, stats, cancel, program_number))
}

/// Check whether thumbnail generation is available on this system.
///
/// With `video-thumbnail` feature: always `true` (compiled in).
/// Without: checks for external ffmpeg binary.
pub fn check_thumbnail_available() -> bool {
    #[cfg(feature = "video-thumbnail")]
    {
        true
    }
    #[cfg(not(feature = "video-thumbnail"))]
    {
        check_ffmpeg_subprocess_available()
    }
}

/// Legacy name — kept for backward compatibility with external call sites.
#[allow(dead_code)]
pub fn check_ffmpeg_available() -> bool {
    check_thumbnail_available()
}

// ── Thumbnail Loop ──────────────────────────────────────────────────────

async fn thumbnail_loop(
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<ThumbnailAccumulator>,
    cancel: CancellationToken,
    program_number: Option<u16>,
) {
    #[cfg(feature = "video-thumbnail")]
    {
        video_engine::silence_ffmpeg_logs();
    }

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
                    // it first so the decoder only sees the selected program.
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

                    match generate_thumbnail_dispatch(&ts_data).await {
                        Ok(thumbnail_result) => {
                            tracing::debug!(
                                "Thumbnail captured: {} bytes JPEG from {} bytes TS",
                                thumbnail_result.jpeg.len(),
                                ts_data.len()
                            );
                            stats.store(thumbnail_result.jpeg.clone());

                            // ── Freeze / black detection ──
                            let jpeg_hash = hash_jpeg(&thumbnail_result.jpeg);
                            let is_frozen = stats.check_freeze(jpeg_hash);
                            let is_black = thumbnail_result.luminance < BLACK_LUMINANCE_THRESHOLD;

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

// ── Unified thumbnail result ────────────────────────────────────────────

/// Result from either the in-process or subprocess thumbnail path.
struct InternalThumbnailResult {
    jpeg: Bytes,
    /// Average luminance (0.0-255.0). For black-screen detection.
    luminance: f64,
}

/// Dispatch to the appropriate thumbnail backend.
async fn generate_thumbnail_dispatch(ts_data: &[u8]) -> Result<InternalThumbnailResult, String> {
    #[cfg(feature = "video-thumbnail")]
    {
        generate_thumbnail_inprocess(ts_data).await
    }
    #[cfg(not(feature = "video-thumbnail"))]
    {
        generate_thumbnail_subprocess(ts_data).await
    }
}

// ════════════════════════════════════════════════════════════════════════
// In-process thumbnail backend (video-thumbnail feature)
// ════════════════════════════════════════════════════════════════════════

#[cfg(feature = "video-thumbnail")]
async fn generate_thumbnail_inprocess(ts_data: &[u8]) -> Result<InternalThumbnailResult, String> {
    use video_codec::ThumbnailConfig;

    // Extract video NAL units from TS on a blocking thread
    let ts_owned = ts_data.to_vec();
    let result = tokio::task::spawn_blocking(move || {
        // 1. Extract video NAL data from MPEG-TS
        let extracted = extract_video_from_ts(&ts_owned)
            .ok_or_else(|| "no video stream found in TS data".to_string())?;

        // 2. Decode, scale, and encode as JPEG
        let config = ThumbnailConfig {
            width: THUMBNAIL_WIDTH,
            height: THUMBNAIL_HEIGHT,
            quality: 5,
        };

        video_engine::decode_thumbnail(&extracted.annex_b_data, extracted.codec, &config)
            .map_err(|e| format!("in-process thumbnail decode failed: {e}"))
    })
    .await
    .map_err(|e| format!("thumbnail task panicked: {e}"))??;

    Ok(InternalThumbnailResult {
        jpeg: result.jpeg,
        luminance: result.luminance,
    })
}

/// Video data extracted from an MPEG-TS buffer.
#[cfg(feature = "video-thumbnail")]
struct ExtractedVideo {
    /// Annex B encoded NAL units (with 0x00000001 start codes).
    annex_b_data: Vec<u8>,
    /// Detected video codec.
    codec: video_codec::VideoCodec,
}

/// Extract video elementary stream data from raw MPEG-TS bytes.
///
/// Parses PAT → PMT to discover the video PID and codec type, then
/// reassembles PES packets for the video PID and returns the raw
/// elementary stream data (Annex B NAL units with start codes).
///
/// This is a lightweight, self-contained extraction that reuses the
/// `ts_parse` helpers without depending on `TsDemuxer`.
#[cfg(feature = "video-thumbnail")]
fn extract_video_from_ts(ts_data: &[u8]) -> Option<ExtractedVideo> {
    use super::ts_parse::{ts_pid, ts_pusi, ts_has_payload,
                          ts_payload_offset, parse_pat_programs, PAT_PID};
    use video_codec::VideoCodec;

    const STREAM_TYPE_H264: u8 = 0x1B;
    const STREAM_TYPE_H265: u8 = 0x24;

    // ── Pass 1: Find PMT PID from PAT ──
    let mut pmt_pid: Option<u16> = None;

    let mut offset = 0;
    while offset + TS_PACKET_SIZE <= ts_data.len() {
        let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
        if pkt[0] == TS_SYNC_BYTE && ts_pid(pkt) == PAT_PID && ts_pusi(pkt) {
            let mut programs = parse_pat_programs(pkt);
            if !programs.is_empty() {
                programs.sort_by_key(|(num, _)| *num);
                pmt_pid = Some(programs[0].1);
                break;
            }
        }
        offset += TS_PACKET_SIZE;
    }
    let pmt_pid = pmt_pid?;

    // ── Pass 2: Find video PID and stream type from PMT ──
    let mut video_pid: Option<u16> = None;
    let mut video_stream_type: u8 = 0;

    offset = 0;
    while offset + TS_PACKET_SIZE <= ts_data.len() {
        let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
        if pkt[0] == TS_SYNC_BYTE && ts_pid(pkt) == pmt_pid && ts_pusi(pkt) {
            // Parse PMT to find video ES
            if let Some((pid, st)) = parse_pmt_video_pid(pkt) {
                video_pid = Some(pid);
                video_stream_type = st;
                break;
            }
        }
        offset += TS_PACKET_SIZE;
    }
    let video_pid = video_pid?;

    let codec = match video_stream_type {
        STREAM_TYPE_H264 => VideoCodec::H264,
        STREAM_TYPE_H265 => VideoCodec::Hevc,
        _ => return None,
    };

    // ── Pass 3: Reassemble PES payloads for video PID ──
    // Collect raw elementary stream data (PES payload = Annex B NAL units).
    let mut es_data = Vec::with_capacity(256 * 1024);
    let mut pes_started = false;
    let mut found_keyframe_pes = false;

    offset = 0;
    while offset + TS_PACKET_SIZE <= ts_data.len() {
        let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
        offset += TS_PACKET_SIZE;

        if pkt[0] != TS_SYNC_BYTE || ts_pid(pkt) != video_pid || !ts_has_payload(pkt) {
            continue;
        }

        let pusi = ts_pusi(pkt);
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE {
            continue;
        }
        let payload = &pkt[payload_start..];

        if pusi {
            // New PES packet starting. If we already collected a keyframe,
            // we have enough data — stop here.
            if found_keyframe_pes && !es_data.is_empty() {
                break;
            }

            // Strip PES header to get to elementary stream data
            if payload.len() >= 9
                && payload[0] == 0x00
                && payload[1] == 0x00
                && payload[2] == 0x01
            {
                let header_data_len = payload[8] as usize;
                let es_start = 9 + header_data_len;
                if es_start < payload.len() {
                    let es_payload = &payload[es_start..];
                    es_data.extend_from_slice(es_payload);

                    // Check if this PES contains a keyframe
                    if contains_keyframe(es_payload, codec) {
                        found_keyframe_pes = true;
                    }
                }
            }
            pes_started = true;
        } else if pes_started {
            // Continuation of current PES
            es_data.extend_from_slice(payload);

            // Check continuation data for keyframe markers too
            if !found_keyframe_pes && contains_keyframe(payload, codec) {
                found_keyframe_pes = true;
            }
        }
    }

    if es_data.is_empty() {
        return None;
    }

    Some(ExtractedVideo {
        annex_b_data: es_data,
        codec,
    })
}

/// Parse a PMT packet to find the first video elementary stream.
/// Returns `(es_pid, stream_type)` or `None`.
#[cfg(feature = "video-thumbnail")]
fn parse_pmt_video_pid(pkt: &[u8]) -> Option<(u16, u8)> {
    use super::ts_parse::ts_has_adaptation;

    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;

    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 {
        return None; // Not PMT
    }

    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);

    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let stream_type = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_length =
            (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

        if stream_type == 0x1B || stream_type == 0x24 {
            return Some((es_pid, stream_type));
        }

        pos += 5 + es_info_length;
    }

    None
}

/// Check if Annex B data contains a keyframe NAL unit.
#[cfg(feature = "video-thumbnail")]
fn contains_keyframe(data: &[u8], codec: video_codec::VideoCodec) -> bool {
    // Scan for start codes and check NAL types
    let mut i = 0;
    while i + 4 < data.len() {
        // Look for 0x000001 or 0x00000001
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            let (nalu_start, found) = if i + 3 < data.len()
                && data[i + 2] == 0x00
                && data[i + 3] == 0x01
            {
                (i + 4, true)
            } else if data[i + 2] == 0x01 {
                (i + 3, true)
            } else {
                (0, false)
            };

            if found && nalu_start < data.len() {
                let nalu_header = data[nalu_start];
                match codec {
                    video_codec::VideoCodec::H264 => {
                        let nalu_type = nalu_header & 0x1F;
                        // IDR = 5, SPS = 7
                        if nalu_type == 5 || nalu_type == 7 {
                            return true;
                        }
                    }
                    video_codec::VideoCodec::Hevc => {
                        let nalu_type = (nalu_header >> 1) & 0x3F;
                        // IDR_W_RADL = 19, IDR_N_LP = 20, CRA = 21, VPS = 32, SPS = 33
                        if matches!(nalu_type, 19 | 20 | 21 | 32 | 33) {
                            return true;
                        }
                    }
                }
                i = nalu_start + 1;
                continue;
            }
        }
        i += 1;
    }
    false
}

// ════════════════════════════════════════════════════════════════════════
// ffmpeg subprocess fallback (when video-thumbnail feature is disabled)
// ════════════════════════════════════════════════════════════════════════

#[cfg(not(feature = "video-thumbnail"))]
fn check_ffmpeg_subprocess_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(feature = "video-thumbnail"))]
async fn generate_thumbnail_subprocess(ts_data: &[u8]) -> Result<InternalThumbnailResult, String> {
    use tokio::io::AsyncWriteExt;

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

    let jpeg = Bytes::from(output.stdout);

    // Black-screen detection via secondary ffmpeg subprocess
    let luminance = detect_black_luminance_subprocess(&jpeg).await;

    Ok(InternalThumbnailResult { jpeg, luminance })
}

/// Detect average luminance via a lightweight ffmpeg invocation (subprocess fallback).
#[cfg(not(feature = "video-thumbnail"))]
async fn detect_black_luminance_subprocess(jpeg: &[u8]) -> f64 {
    use tokio::io::AsyncWriteExt;

    let result: Result<f64, String> = async {
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

        let pixels = &output.stdout;
        let sum: f64 = pixels.iter().map(|&p| p as f64).sum();
        Ok(sum / pixels.len() as f64)
    }.await;

    match result {
        Ok(lum) => lum,
        Err(e) => {
            tracing::debug!("Black-screen detection failed: {e}");
            255.0 // assume not black on failure
        }
    }
}

// ── Detection Helpers ───────────────────────────────────────────────────

/// Compute a 64-bit hash of the JPEG bytes for freeze-frame comparison.
///
/// Identical video frames produce identical JPEG output from the same
/// settings, so a byte-level hash is a reliable equality test.
fn hash_jpeg(data: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}
