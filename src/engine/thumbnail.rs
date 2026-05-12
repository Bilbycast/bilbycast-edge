// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Thumbnail generation module.
//!
//! Subscribes to a flow's broadcast channel as an independent consumer,
//! buffers recent video access units, and periodically generates a small
//! JPEG thumbnail. Like the media analyzer, this module **cannot** block
//! the hot path — if it falls behind, it receives `Lagged(n)` and silently
//! skips packets.
//!
//! ## Backend selection
//!
//! When the `media-codecs` feature is enabled (default), thumbnails are
//! generated **in-process** via `video-engine` (FFmpeg libavcodec/libswscale).
//! A long-lived [`TsDemuxer`] owned by the loop turns the inbound
//! MPEG-TS stream into demuxed access units (one per PES), each tagged with
//! its presentation timestamp. On every cadence tick the loop walks the
//! retained AUs, prepends the demuxer's cached parameter sets (SPS/PPS for
//! H.264, VPS+SPS+PPS for HEVC), and feeds the decoder one packet per AU
//! via [`video_engine::decode_thumbnail_packets`]. This per-AU framing is
//! what the mpegts demuxer inside ffmpeg provides — without it, open-GOP
//! broadcast streams that signal random access through `recovery_point`
//! SEIs (rather than IDR NAL units) never decode a frame.
//!
//! When the feature is disabled, the module falls back to piping the
//! buffered TS data to an external **ffmpeg subprocess** (the legacy
//! path), which carries its own mpegts demuxer.
//!
//! ## Independence from content analysis
//!
//! Thumbnail generation is unconditionally on whenever `FlowConfig.thumbnail`
//! is true and the in-process backend (or external ffmpeg) is available.
//! Toggling any of the `content_analysis` tiers (lite / audio_full /
//! video_full) **never** affects thumbnail capture — they are separate
//! broadcast subscribers with independent lifecycles. The owning loop
//! holds its own `TsDemuxer`, so the analysers can be off entirely and
//! thumbnails still render.
//!
//! ## Freeze-frame and black-screen detection
//!
//! After each JPEG capture the module performs two lightweight checks:
//!
//! 1. **Freeze detection** — the JPEG bytes are hashed and compared with
//!    the previous capture. Six consecutive identical hashes (~30 s at
//!    the default 5 s interval) raise a `"frozen"` alarm. The alarm is
//!    reset when the input ring runs dry or a capture errors so it does
//!    not stick across quiet periods.
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

use bytes::Bytes;
#[cfg(not(feature = "media-codecs"))]
use bytes::BytesMut;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::stats::collector::ThumbnailAccumulator;

use super::packet::RtpPacket;
use super::ts_parse::strip_rtp_header;
#[cfg(feature = "media-codecs")]
use super::ts_demux::{DemuxedFrame, TsDemuxer};

/// How often to generate a thumbnail (seconds).
const THUMBNAIL_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum gap between out-of-cycle captures driven by the external refresh
/// trigger. Prevents a flood of switch events from stacking decode work.
const TRIGGER_MIN_GAP: Duration = Duration::from_millis(500);

/// Minimum elapsed time between two captures before their JPEG hashes are
/// eligible for freeze comparison. Back-to-back captures (e.g. a trigger
/// firing right after a periodic tick, or a catch-up tick after a stall) run
/// on nearly-identical buffers and reliably produce matching hashes — so
/// we feed `reset_freeze()` in that case rather than letting six such
/// samples trip the alarm.
const FREEZE_MIN_GAP: Duration = Duration::from_millis(4_500);

/// Maximum demuxed video bytes to retain in the in-process ring. Sized so
/// a typical-GOP keyframe is always in scope — broadcast GOPs run 1–2 s,
/// pre-encoded MP4 sources with 10 s GOPs are still covered: at 21 Mbps,
/// 8 MB is ~3 s; at 2 Mbps it's ~32 s. The byte budget is a *retention*
/// cap, not a per-tick decode budget — see [`MAX_DECODE_FRAMES`].
#[cfg(feature = "media-codecs")]
const MAX_AU_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of access units to feed the decoder per tick. Bounds
/// per-tick CPU regardless of source pattern:
/// - Short-GOP broadcast streams: anchor lands at the latest keyframe a
///   few frames before the end of the ring, well under this cap.
/// - Open-GOP H.264 with `recovery_point` SEIs (DVB-T 1080i25, etc.):
///   the no-keyframe fallback feeds the most recent `MAX_DECODE_FRAMES`
///   AUs so libavcodec has a couple of GOP boundaries' worth of context.
/// - Long-GOP MP4 (10 s GOP at 25 fps = 250 frames between IDRs): the
///   anchor is clamped forward so we decode at most this many frames —
///   the resulting thumbnail trails by ~ MAX_DECODE_FRAMES / fps but
///   never burns multi-second decodes per tick.
///
/// 50 ≈ 2 s at 25 fps / 1 s at 50 fps / 0.83 s at 60 fps — enough for an
/// SBS-style open-GOP stream (SPS/I-slice every ~2.5 s) to lock on
/// reliably, and small enough that 6 concurrent thumbnail loops decoding
/// at ~15 ms / frame stay under ~25 % CPU steady-state.
#[cfg(feature = "media-codecs")]
const MAX_DECODE_FRAMES: usize = 50;

/// Subprocess-fallback raw TS budget. Larger than the AU budget because
/// the legacy ffmpeg-pipe path needs PAT/PMT + a keyframe in the same
/// chunk it streams to ffmpeg's mpegts demuxer.
#[cfg(not(feature = "media-codecs"))]
const MAX_BUFFER_BYTES: usize = 32 * 1024 * 1024;

/// Thumbnail output dimensions.
const THUMBNAIL_WIDTH: u32 = 320;
const THUMBNAIL_HEIGHT: u32 = 180;

/// MPEG-TS sync byte.
const TS_SYNC_BYTE: u8 = 0x47;

/// MPEG-TS packet size.
const TS_PACKET_SIZE: usize = 188;

/// Average luminance (0–255) below which a frame is considered black.
const BLACK_LUMINANCE_THRESHOLD: f64 = 16.0;

/// How long the broadcast channel may stay silent before we declare the
/// input "gone" (`no_signal`). Picked so a working stream's normal
/// inter-packet jitter never trips it, while a stopped stream is flagged
/// before the freeze counter could reach the 30 s frozen threshold and
/// raise the wrong alarm.
const NO_SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);

// ── Subprocess fallback constants (used when media-codecs is disabled) ──

/// ffmpeg subprocess timeout.
#[cfg(not(feature = "media-codecs"))]
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for the lightweight black-detection ffmpeg invocation.
#[cfg(not(feature = "media-codecs"))]
const BLACK_DETECT_TIMEOUT: Duration = Duration::from_secs(3);

// ── Public API ───────────────────────────────────────────────────────────

/// Spawn the thumbnail generator as an independent broadcast subscriber.
///
/// `program_number` selects which MPEG-TS program to render when the input
/// is an MPTS. `None` keeps the default (lowest program_number in the PAT);
/// `Some(N)` filters the buffered TS to only show program N. The selection
/// is delegated to the loop's owned [`TsDemuxer`] — there is no separate
/// program-filter pass.
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
/// With `media-codecs` feature: always `true` (compiled in).
/// Without: checks for external ffmpeg binary.
pub fn check_thumbnail_available() -> bool {
    #[cfg(feature = "media-codecs")]
    {
        true
    }
    #[cfg(not(feature = "media-codecs"))]
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
    #[cfg(feature = "media-codecs")]
    {
        video_engine::silence_ffmpeg_logs();
    }

    if let Some(n) = program_number {
        tracing::info!("Thumbnail generator started (program filter target = {})", n);
    } else {
        tracing::info!("Thumbnail generator started");
    }

    let mut interval = tokio::time::interval(THUMBNAIL_INTERVAL);
    // Skip-don't-burst: if the runtime stalls and several ticks accumulate,
    // we don't want to fire them back-to-back on the same buffered data —
    // that yields identical JPEG hashes and false-positive "frozen" alarms.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // consume first immediate tick

    // External refresh trigger — switch_active_input calls notify_one() on
    // the newly-active input's accumulator so its thumbnail refreshes within
    // `TRIGGER_MIN_GAP` instead of waiting up to the full 5s interval.
    let refresh_trigger = stats.refresh_trigger.clone();
    let mut last_capture: Option<std::time::Instant> = None;

    // Buffer state. The in-process path keeps a ring of demuxed access
    // units; the subprocess fallback keeps a ring of raw TS chunks because
    // it pipes them straight into ffmpeg's mpegts demuxer.
    #[cfg(feature = "media-codecs")]
    let mut state = LiveState::new(program_number);
    #[cfg(not(feature = "media-codecs"))]
    let mut state = SubprocessState::new();

    // Wall-clock of the most recent *useful* broadcast packet — i.e. one
    // that contains at least one non-NULL TS payload. The post-fixer flow
    // channel emits NULL-PID (0x1FFF) keepalive padding every 250 ms while
    // the active input is silent (so downstream UDP sockets stay alive); a
    // naive "any packet recently?" timer would never trip `no_signal` in
    // that case and the freeze counter would latch FROZEN on stale buffer
    // data instead. Tracking only useful packets makes the broadcast look
    // genuinely silent from the thumbnail generator's point of view.
    let mut last_useful_packet_at: Option<std::time::Instant> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("Thumbnail generator stopping (cancelled)");
                break;
            }

            _ = refresh_trigger.notified() => {
                // External trigger — capture now if we've got buffered data
                // and the cooldown has elapsed. Skip silently otherwise; the
                // next 5s interval tick will still fire.
                let cooldown_ok = last_capture
                    .map(|t| t.elapsed() >= TRIGGER_MIN_GAP)
                    .unwrap_or(true);
                if !cooldown_ok || !state.has_data() {
                    continue;
                }
                match state.capture().await {
                    Ok(thumbnail_result) => {
                        stats.store(thumbnail_result.jpeg.clone());
                        let jpeg_hash = hash_jpeg(&thumbnail_result.jpeg);
                        let back_to_back = last_capture
                            .map(|t| t.elapsed() < FREEZE_MIN_GAP)
                            .unwrap_or(false);
                        let is_frozen = if back_to_back {
                            stats.reset_freeze();
                            false
                        } else {
                            stats.check_freeze(jpeg_hash)
                        };
                        let is_black = thumbnail_result.luminance < BLACK_LUMINANCE_THRESHOLD;
                        if is_black {
                            stats.set_alarm(Some("black".to_string()));
                        } else if is_frozen {
                            stats.set_alarm(Some("frozen".to_string()));
                        } else {
                            stats.set_alarm(None);
                        }
                        last_capture = Some(std::time::Instant::now());
                        // Re-align the periodic interval so the next scheduled
                        // tick is 5s from now, not from whenever it last fired.
                        interval.reset();
                    }
                    Err(e) => {
                        tracing::debug!("Thumbnail trigger capture failed: {e}");
                        stats.record_error(e);
                        stats.reset_freeze();
                    }
                }
            }

            _ = interval.tick() => {
                // Detect "gone" before "frozen": if the broadcast channel
                // has been silent for `NO_SIGNAL_TIMEOUT`, the input has
                // stopped — re-decoding the stale buffer would produce
                // identical JPEGs and trip the freeze alarm with the
                // wrong cause. Resetting the freeze counter here also
                // gives a clean slate for when the stream resumes.
                let stream_silent = last_useful_packet_at
                    .map(|t| t.elapsed() >= NO_SIGNAL_TIMEOUT)
                    .unwrap_or(true);
                if stream_silent || !state.has_data() {
                    stats.reset_freeze();
                    stats.set_alarm(Some("no_signal".to_string()));
                    continue;
                }

                match state.capture().await {
                    Ok(thumbnail_result) => {
                        tracing::debug!(
                            "Thumbnail captured: {} bytes JPEG",
                            thumbnail_result.jpeg.len()
                        );
                        stats.store(thumbnail_result.jpeg.clone());

                        // ── Freeze / black detection ──
                        let jpeg_hash = hash_jpeg(&thumbnail_result.jpeg);
                        let back_to_back = last_capture
                            .map(|t| t.elapsed() < FREEZE_MIN_GAP)
                            .unwrap_or(false);
                        let is_frozen = if back_to_back {
                            stats.reset_freeze();
                            false
                        } else {
                            stats.check_freeze(jpeg_hash)
                        };
                        let is_black = thumbnail_result.luminance < BLACK_LUMINANCE_THRESHOLD;

                        if is_black {
                            stats.set_alarm(Some("black".to_string()));
                        } else if is_frozen {
                            stats.set_alarm(Some("frozen".to_string()));
                        } else {
                            stats.set_alarm(None);
                        }
                        last_capture = Some(std::time::Instant::now());
                    }
                    Err(e) => {
                        tracing::debug!("Thumbnail capture failed: {e}");
                        stats.record_error(e);
                        // Capture failed — we have no ground truth this
                        // tick, so clear the alarm rather than leaving
                        // a stale "frozen" / "black" flag on screen.
                        stats.reset_freeze();
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

                        // Only consume data that looks like valid MPEG-TS.
                        if ts_payload.len() >= TS_PACKET_SIZE && ts_payload[0] == TS_SYNC_BYTE {
                            // Walk the 188-byte packets in this chunk and
                            // count any that carry a non-NULL PID. A chunk
                            // that contains nothing but 0x1FFF padding is
                            // the input forwarder's keepalive — it keeps
                            // downstream sockets warm but contributes no
                            // decodable video, so it must not reset the
                            // no-signal timer.
                            if has_useful_pid(ts_payload) {
                                last_useful_packet_at = Some(std::time::Instant::now());
                            }

                            state.ingest(ts_payload);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("Thumbnail generator lagged, skipped {n} packets");
                        // The demuxer's PES assemblers self-resync on the
                        // next PUSI, so no explicit reset is required.
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

/// Return true if any 188-byte TS packet in this chunk carries a non-NULL
/// PID. Used to distinguish keepalive padding (all 0x1FFF) from real data.
fn has_useful_pid(ts_payload: &[u8]) -> bool {
    let mut off = 0;
    while off + TS_PACKET_SIZE <= ts_payload.len() {
        if ts_payload[off] != TS_SYNC_BYTE {
            break;
        }
        let pid = (((ts_payload[off + 1] & 0x1F) as u16) << 8) | ts_payload[off + 2] as u16;
        if pid != 0x1FFF {
            return true;
        }
        off += TS_PACKET_SIZE;
    }
    false
}

// ── Unified thumbnail result ────────────────────────────────────────────

/// Result from either the in-process or subprocess thumbnail path.
struct InternalThumbnailResult {
    jpeg: Bytes,
    /// Average luminance (0.0-255.0). For black-screen detection.
    luminance: f64,
}

// ════════════════════════════════════════════════════════════════════════
// In-process thumbnail backend (media-codecs feature)
// ════════════════════════════════════════════════════════════════════════

/// Anchor classification for a buffered access unit.
///
/// Streams that re-emit SPS/PPS mid-GOP (some broadcast contribution
/// encoders do this every few seconds for resync robustness) make a
/// pure `has_sps` heuristic mis-fire — picking a non-IDR SPS-bearing
/// AU as anchor means the decoder is fed a non-RAP starting point and
/// produces green / pixelated output until enough P-frames have flowed.
/// `snapshot_for_decode` therefore prefers `Idr` over `ParamSet` over
/// `None`, falling through only when the stronger marker is absent.
#[cfg(feature = "media-codecs")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorKind {
    /// Codec-defined random-access point — H.264 IDR (NAL 5), HEVC
    /// NAL 19/20/21 (IDR_W_RADL / IDR_N_LP / CRA_NUT), MPEG-2
    /// I-picture. The decoder can start cold from this AU.
    Idr,
    /// Parameter-set-only marker — H.264 SPS (NAL 7), HEVC
    /// VPS/SPS/PPS (NAL 32/33/34), MPEG-2 sequence_header. Open-GOP
    /// broadcast streams that never emit IDR rely on these to mark
    /// recovery points; preferred only when no `Idr` exists in the
    /// buffer.
    ParamSet,
    /// Not an anchor — VCL only, references prior frames.
    None,
}

#[cfg(feature = "media-codecs")]
struct ThumbnailFrame {
    /// Concatenated Annex B NALs (one access unit) ready to feed the decoder.
    annex_b: Vec<u8>,
    /// Presentation timestamp from the source PES (90 kHz ticks).
    pts: i64,
    /// Random-access classification — see [`AnchorKind`].
    anchor: AnchorKind,
}

/// Live-thumbnail loop state for the in-process backend.
///
/// Owns its own `TsDemuxer` so thumbnail generation stays independent of
/// any other subscriber on the broadcast channel — toggling `content_analysis`
/// (or any future broadcast-tap feature) cannot affect this path.
#[cfg(feature = "media-codecs")]
struct LiveState {
    demuxer: TsDemuxer,
    /// Demuxed access units in arrival order (oldest at front).
    frames: VecDeque<ThumbnailFrame>,
    /// Sum of `annex_b.len()` across `frames` for the byte-budget eviction.
    frames_bytes: usize,
    /// Cached video codec resolved from the demuxer's PMT scan.
    codec: Option<video_codec::VideoCodec>,
}

#[cfg(feature = "media-codecs")]
impl LiveState {
    fn new(program_number: Option<u16>) -> Self {
        Self {
            demuxer: TsDemuxer::new(program_number),
            frames: VecDeque::new(),
            frames_bytes: 0,
            codec: None,
        }
    }

    fn has_data(&self) -> bool {
        !self.frames.is_empty()
    }

    fn ingest(&mut self, ts_payload: &[u8]) {
        let demuxed = self.demuxer.demux(ts_payload);
        for frame in demuxed {
            match frame {
                DemuxedFrame::H264 {
                    nalus,
                    pts,
                    is_keyframe,
                } => {
                    self.codec = Some(video_codec::VideoCodec::H264);
                    // Classify two-tiered: codec-defined IDR (NAL 5) is
                    // a clean random-access point. SPS (NAL 7) is only a
                    // hint — some broadcast encoders re-emit SPS mid-GOP
                    // every few seconds, so picking an SPS-only AU as
                    // anchor mis-decodes the stream. The snapshotter
                    // prefers IDR; ParamSet is the open-GOP fallback.
                    let has_sps = nalus.iter().any(|n| h264_nal_type(n) == 7);
                    let anchor = if is_keyframe {
                        AnchorKind::Idr
                    } else if has_sps {
                        AnchorKind::ParamSet
                    } else {
                        AnchorKind::None
                    };
                    let annex_b = build_annex_b(&nalus);
                    self.push_frame(annex_b, pts, anchor);
                }
                DemuxedFrame::H265 {
                    nalus,
                    pts,
                    is_keyframe,
                } => {
                    self.codec = Some(video_codec::VideoCodec::Hevc);
                    // VPS (32) / SPS (33) / PPS (34) at GOP boundary —
                    // same two-tier logic as H.264. Prefer codec-defined
                    // IDR_W_RADL / IDR_N_LP / CRA_NUT (NAL 19/20/21,
                    // already folded into `is_keyframe`); fall back to
                    // a parameter-set-bearing AU only when no IDR has
                    // been seen.
                    let has_param_set =
                        nalus.iter().any(|n| matches!(h265_nal_type(n), 32 | 33 | 34));
                    let anchor = if is_keyframe {
                        AnchorKind::Idr
                    } else if has_param_set {
                        AnchorKind::ParamSet
                    } else {
                        AnchorKind::None
                    };
                    let annex_b = build_annex_b(&nalus);
                    self.push_frame(annex_b, pts, anchor);
                }
                DemuxedFrame::Mpeg2 {
                    es,
                    pts,
                    is_keyframe,
                } => {
                    self.codec = Some(video_codec::VideoCodec::Mpeg2);
                    // MPEG-2: an AU opening with `sequence_header`
                    // (0x000001B3) is a random-access point with the
                    // I-picture inside the same AU. Prefer the codec-
                    // defined I-picture flag (`is_keyframe`); fall back
                    // to sequence_header for streams that don't surface
                    // it through `is_keyframe`.
                    let has_seq = mpeg2_starts_with_sequence_header(&es);
                    let anchor = if is_keyframe {
                        AnchorKind::Idr
                    } else if has_seq {
                        AnchorKind::ParamSet
                    } else {
                        AnchorKind::None
                    };
                    self.push_frame(es, pts, anchor);
                }
                // Audio variants are not consumed for thumbnails — drop them.
                DemuxedFrame::Opus { .. }
                | DemuxedFrame::Aac { .. }
                | DemuxedFrame::OtherAudio { .. } => {}
                // Stream discontinuity is metadata for stateful consumers
                // (the local-display decoder); the thumbnail generator
                // re-anchors on the next anchor frame regardless.
                DemuxedFrame::Discontinuity => {}
            }
        }
    }

    fn push_frame(&mut self, annex_b: Vec<u8>, pts: u64, anchor: AnchorKind) {
        if annex_b.is_empty() {
            return;
        }
        let bytes = annex_b.len();
        // PTS in the source is unsigned 33-bit; cast to i64 is lossless.
        let pts_i64 = pts as i64;
        self.frames.push_back(ThumbnailFrame {
            annex_b,
            pts: pts_i64,
            anchor,
        });
        self.frames_bytes += bytes;
        while self.frames_bytes > MAX_AU_BUFFER_BYTES {
            match self.frames.pop_front() {
                Some(old) => self.frames_bytes -= old.annex_b.len(),
                None => break,
            }
        }
    }

    /// Snapshot the ring into a `(headers, packets, codec)` tuple ready
    /// for `decode_thumbnail_packets`. Anchor + send-count strategy:
    ///
    /// - **Anchor**: prefer the **most recent IDR** (`AnchorKind::Idr`)
    ///   — codec-defined random-access point, decoder starts cold and
    ///   produces clean output. Some broadcast contribution encoders
    ///   re-emit SPS mid-GOP every few seconds; mistaking those for
    ///   random-access points means feeding the decoder a non-RAP and
    ///   the result is a green / pixelated frame. Only when **no IDR
    ///   exists in the buffer** (open-GOP DVB-T-style streams) do we
    ///   fall back to the most recent `AnchorKind::ParamSet`. As a
    ///   last resort, anchor at `len - MAX_DECODE_FRAMES`.
    /// - **Send count**: at most [`MAX_DECODE_FRAMES`] AUs from the
    ///   anchor forward. Bounds per-tick decoder work regardless of
    ///   GOP length, so a long-GOP MP4 source doesn't decode 250
    ///   frames each tick. The thumbnail trails the live edge by at
    ///   most `MAX_DECODE_FRAMES / fps` seconds in this case.
    fn snapshot_for_decode(&self) -> Option<(Vec<u8>, Vec<(Vec<u8>, i64)>, video_codec::VideoCodec)> {
        let codec = self.codec?;
        if self.frames.is_empty() {
            return None;
        }
        let len = self.frames.len();
        let anchor = self
            .frames
            .iter()
            .rposition(|f| f.anchor == AnchorKind::Idr)
            .or_else(|| {
                self.frames
                    .iter()
                    .rposition(|f| f.anchor == AnchorKind::ParamSet)
            })
            .unwrap_or_else(|| len.saturating_sub(MAX_DECODE_FRAMES));

        let packets: Vec<(Vec<u8>, i64)> = self
            .frames
            .iter()
            .skip(anchor)
            .take(MAX_DECODE_FRAMES)
            .map(|f| (f.annex_b.clone(), f.pts))
            .collect();

        let headers = build_headers(&self.demuxer, codec);
        Some((headers, packets, codec))
    }

    async fn capture(&self) -> Result<InternalThumbnailResult, String> {
        let (headers, packets, codec) = self
            .snapshot_for_decode()
            .ok_or_else(|| "no video frames buffered".to_string())?;

        let cfg = video_codec::ThumbnailConfig {
            width: THUMBNAIL_WIDTH,
            height: THUMBNAIL_HEIGHT,
            quality: 5,
        };

        let result = tokio::task::spawn_blocking(move || {
            video_engine::decode_thumbnail_packets(&headers, &packets, codec, &cfg)
                .map_err(|e| format!("in-process thumbnail decode failed: {e}"))
        })
        .await
        .map_err(|e| format!("thumbnail task panicked: {e}"))??;

        Ok(InternalThumbnailResult {
            jpeg: result.jpeg,
            luminance: result.luminance,
        })
    }
}

/// H.264 NAL unit type from the 1-byte header. Returns 0xFF for empty
/// NALUs so caller checks (e.g. `== 7`) never accidentally match.
#[cfg(feature = "media-codecs")]
#[inline]
fn h264_nal_type(nalu: &[u8]) -> u8 {
    if nalu.is_empty() {
        0xFF
    } else {
        nalu[0] & 0x1F
    }
}

/// HEVC NAL unit type from the 1-byte header. Returns 0xFF on empty.
#[cfg(feature = "media-codecs")]
#[inline]
fn h265_nal_type(nalu: &[u8]) -> u8 {
    if nalu.is_empty() {
        0xFF
    } else {
        (nalu[0] >> 1) & 0x3F
    }
}

/// Whether an MPEG-2 access unit starts with `sequence_header`
/// (`0x000001B3`). MPEG-2 broadcasts emit the sequence header at every
/// GoP boundary so any AU carrying one is a safe random-access anchor
/// even before we look for the I-picture.
#[cfg(feature = "media-codecs")]
#[inline]
fn mpeg2_starts_with_sequence_header(es: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 <= es.len() {
        if es[i] == 0x00 && es[i + 1] == 0x00 && es[i + 2] == 0x01 {
            return es[i + 3] == 0xB3;
        }
        i += 1;
        if i > 32 {
            // Sequence header should land within the first few bytes of
            // an anchor AU; bail past that to avoid scanning a whole
            // payload looking for one that isn't there.
            break;
        }
    }
    false
}

/// Concatenate split NAL units (no start codes) into Annex B form by
/// prepending a 4-byte `00 00 00 01` start code in front of each NAL.
#[cfg(feature = "media-codecs")]
fn build_annex_b(nalus: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = nalus.iter().map(|n| n.len() + 4).sum();
    let mut out = Vec::with_capacity(total);
    for nal in nalus {
        if nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(nal);
    }
    out
}

/// Build the parameter-set blob the decoder needs before any VCL slice.
/// For H.264: SPS+PPS in Annex B; for HEVC: VPS+SPS+PPS. Returns an empty
/// Vec when the demuxer hasn't seen the corresponding NALUs yet — the
/// decoder will pick them up off the first AU that carries them inline.
#[cfg(feature = "media-codecs")]
fn build_headers(demuxer: &TsDemuxer, codec: video_codec::VideoCodec) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    let push = |out: &mut Vec<u8>, nalu: Option<&[u8]>| {
        if let Some(n) = nalu {
            if !n.is_empty() {
                out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                out.extend_from_slice(n);
            }
        }
    };
    match codec {
        video_codec::VideoCodec::H264 => {
            push(&mut out, demuxer.cached_sps());
            push(&mut out, demuxer.cached_pps());
        }
        video_codec::VideoCodec::Hevc => {
            push(&mut out, demuxer.cached_h265_vps());
            push(&mut out, demuxer.cached_h265_sps());
            push(&mut out, demuxer.cached_h265_pps());
        }
        video_codec::VideoCodec::Mpeg2 => {
            // MPEG-2 carries its sequence_header / sequence_extension
            // inline in the bitstream at every GoP boundary; there are
            // no out-of-band parameter sets the demuxer needs to inject.
        }
    }
    out
}

/// One-shot helper for callers that have a buffer of TS bytes (e.g. the
/// replay filmstrip writer). Spins up a fresh `TsDemuxer`, walks the
/// buffer, and returns a snapshot suitable for `decode_thumbnail_packets`
/// — same primitive as the live thumbnail loop, just without the long-
/// lived ring.
#[cfg(feature = "media-codecs")]
pub(crate) fn demux_snapshot_for_decode(
    ts_data: &[u8],
    program_number: Option<u16>,
) -> Option<(Vec<u8>, Vec<(Vec<u8>, i64)>, video_codec::VideoCodec)> {
    let mut state = LiveState::new(program_number);
    state.ingest(ts_data);
    state.snapshot_for_decode()
}

// ════════════════════════════════════════════════════════════════════════
// ffmpeg subprocess fallback (when media-codecs feature is disabled)
// ════════════════════════════════════════════════════════════════════════

#[cfg(not(feature = "media-codecs"))]
struct SubprocessState {
    buffer: VecDeque<Bytes>,
    buffer_bytes: usize,
}

#[cfg(not(feature = "media-codecs"))]
impl SubprocessState {
    fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            buffer_bytes: 0,
        }
    }

    fn has_data(&self) -> bool {
        self.buffer_bytes > 0
    }

    fn ingest(&mut self, ts_payload: &[u8]) {
        let chunk = Bytes::copy_from_slice(ts_payload);
        let chunk_len = chunk.len();
        self.buffer.push_back(chunk);
        self.buffer_bytes += chunk_len;
        while self.buffer_bytes > MAX_BUFFER_BYTES {
            match self.buffer.pop_front() {
                Some(old) => self.buffer_bytes -= old.len(),
                None => break,
            }
        }
    }

    async fn capture(&self) -> Result<InternalThumbnailResult, String> {
        let total: usize = self.buffer.iter().map(|b| b.len()).sum();
        let mut out = BytesMut::with_capacity(total);
        for chunk in &self.buffer {
            out.extend_from_slice(chunk);
        }
        let ts_data = out.freeze();
        generate_thumbnail_subprocess(&ts_data).await
    }
}

#[cfg(not(feature = "media-codecs"))]
fn check_ffmpeg_subprocess_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(feature = "media-codecs"))]
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
#[cfg(not(feature = "media-codecs"))]
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

#[cfg(all(test, feature = "media-codecs"))]
mod tests {
    use super::*;

    #[test]
    fn build_annex_b_prepends_start_codes() {
        let nalus = vec![vec![0x67, 0x42, 0x00], vec![0x68, 0xCE], vec![0x65, 0x88]];
        let out = build_annex_b(&nalus);
        // Expect 0x00000001 prefix in front of each NAL.
        assert_eq!(&out[0..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&out[4..7], &[0x67, 0x42, 0x00]);
        assert_eq!(&out[7..11], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&out[11..13], &[0x68, 0xCE]);
        assert_eq!(&out[13..17], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&out[17..19], &[0x65, 0x88]);
    }

    #[test]
    fn build_annex_b_skips_empty_nalus() {
        let nalus: Vec<Vec<u8>> = vec![Vec::new(), vec![0x67, 0x42], Vec::new()];
        let out = build_annex_b(&nalus);
        // One NAL emitted: 4-byte start code + 2-byte body.
        assert_eq!(out.len(), 6);
        assert_eq!(&out[0..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&out[4..6], &[0x67, 0x42]);
    }

    #[test]
    fn snapshot_anchors_at_latest_keyframe_when_recent() {
        let mut state = LiveState::new(None);
        state.codec = Some(video_codec::VideoCodec::H264);
        // Four frames: idr, non-kf, idr, non-kf — latest IDR is well
        // within the recent-window cap so the anchor lands on it.
        for (i, kind) in [
            (1, AnchorKind::Idr),
            (2, AnchorKind::None),
            (3, AnchorKind::Idr),
            (4, AnchorKind::None),
        ] {
            state.frames.push_back(ThumbnailFrame {
                annex_b: vec![i as u8],
                pts: i,
                anchor: kind,
            });
            state.frames_bytes += 1;
        }
        let (_headers, packets, _codec) = state.snapshot_for_decode().unwrap();
        // Anchor at the second keyframe (index 2) → 2 packets remain.
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].1, 3);
        assert_eq!(packets[1].1, 4);
    }

    #[test]
    fn snapshot_keeps_anchor_but_bounds_send_count_for_long_gop() {
        // Long-GOP source (e.g. MP4 with 10 s GOP): IDR is at index 0
        // of a much longer ring. Anchor stays at the IDR so the decoder
        // has a real random-access point, but the send count is capped
        // at MAX_DECODE_FRAMES so we don't decode the whole GOP.
        let mut state = LiveState::new(None);
        state.codec = Some(video_codec::VideoCodec::H264);
        let total = MAX_DECODE_FRAMES + 50;
        for i in 1..=total {
            state.frames.push_back(ThumbnailFrame {
                annex_b: vec![(i & 0xFF) as u8],
                pts: i as i64,
                anchor: if i == 1 { AnchorKind::Idr } else { AnchorKind::None },
            });
            state.frames_bytes += 1;
        }
        let (_headers, packets, _codec) = state.snapshot_for_decode().unwrap();
        assert_eq!(packets.len(), MAX_DECODE_FRAMES);
        assert_eq!(packets[0].1, 1);
        assert_eq!(packets.last().unwrap().1, MAX_DECODE_FRAMES as i64);
    }

    #[test]
    fn snapshot_prefers_idr_over_paramset_when_both_present() {
        // Broadcast contribution streams that re-emit SPS mid-GOP: the
        // most recent ParamSet AU is mid-GOP and would mis-decode if
        // chosen as anchor. Verify the snapshotter walks back to the
        // most recent IDR instead, even when ParamSet markers exist
        // closer to the live edge.
        let mut state = LiveState::new(None);
        state.codec = Some(video_codec::VideoCodec::H264);
        // Layout: [IDR, ..., ParamSet, ..., ParamSet, ...] with last
        // ParamSet at index 18 and last IDR at index 5.
        let total = 30;
        for i in 0..total {
            let kind = match i {
                5 => AnchorKind::Idr,
                12 | 18 => AnchorKind::ParamSet,
                _ => AnchorKind::None,
            };
            state.frames.push_back(ThumbnailFrame {
                annex_b: vec![(i & 0xFF) as u8],
                pts: i as i64,
                anchor: kind,
            });
            state.frames_bytes += 1;
        }
        let (_headers, packets, _codec) = state.snapshot_for_decode().unwrap();
        // Must anchor at index 5 (the IDR), not at 18 (latest ParamSet).
        assert_eq!(packets[0].1, 5);
    }

    #[test]
    fn snapshot_falls_back_to_paramset_when_no_idr() {
        // Pure open-GOP broadcast (e.g. SBS DVB-T): only ParamSet AUs.
        // Anchor lands on the most recent ParamSet — the only random-
        // access marker the decoder can lock onto.
        let mut state = LiveState::new(None);
        state.codec = Some(video_codec::VideoCodec::H264);
        let total = 100;
        let sps_idx = 20;
        for i in 0..total {
            let kind = if i == sps_idx { AnchorKind::ParamSet } else { AnchorKind::None };
            state.frames.push_back(ThumbnailFrame {
                annex_b: vec![(i & 0xFF) as u8],
                pts: i as i64,
                anchor: kind,
            });
            state.frames_bytes += 1;
        }
        let (_headers, packets, _codec) = state.snapshot_for_decode().unwrap();
        assert_eq!(packets.len(), MAX_DECODE_FRAMES);
        assert_eq!(packets[0].1, sps_idx as i64);
        assert_eq!(packets.last().unwrap().1, (sps_idx + MAX_DECODE_FRAMES - 1) as i64);
    }

    #[test]
    fn snapshot_anchors_at_oldest_when_no_anchor_within_cap() {
        // Stream with no IDR or ParamSet markers ever surfaced. With
        // fewer frames in the ring than `MAX_DECODE_FRAMES`, the
        // fallback anchor is the oldest AU (full retained window).
        let mut state = LiveState::new(None);
        state.codec = Some(video_codec::VideoCodec::H264);
        for i in 1..=3 {
            state.frames.push_back(ThumbnailFrame {
                annex_b: vec![i as u8],
                pts: i as i64,
                anchor: AnchorKind::None,
            });
            state.frames_bytes += 1;
        }
        let (_headers, packets, _codec) = state.snapshot_for_decode().unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].1, 1);
    }

    #[test]
    fn snapshot_caps_fallback_to_recent_window() {
        // Stream with a long retention but no anchor markers ever
        // surfaced: only the last `MAX_DECODE_FRAMES` AUs are sent.
        let mut state = LiveState::new(None);
        state.codec = Some(video_codec::VideoCodec::H264);
        let total = MAX_DECODE_FRAMES + 50;
        for i in 1..=total {
            state.frames.push_back(ThumbnailFrame {
                annex_b: vec![(i & 0xFF) as u8],
                pts: i as i64,
                anchor: AnchorKind::None,
            });
            state.frames_bytes += 1;
        }
        let (_headers, packets, _codec) = state.snapshot_for_decode().unwrap();
        assert_eq!(packets.len(), MAX_DECODE_FRAMES);
        assert_eq!(packets[0].1, (total - MAX_DECODE_FRAMES + 1) as i64);
        assert_eq!(packets.last().unwrap().1, total as i64);
    }

    #[test]
    fn snapshot_returns_none_when_codec_unknown() {
        let state = LiveState::new(None);
        assert!(state.snapshot_for_decode().is_none());
    }

    #[test]
    fn ring_evicts_oldest_when_byte_budget_exceeded() {
        let mut state = LiveState::new(None);
        state.codec = Some(video_codec::VideoCodec::H264);
        // Push frames totalling > MAX_AU_BUFFER_BYTES via a single
        // oversized synthetic AU — easier than spinning up real ones.
        let big = vec![0u8; MAX_AU_BUFFER_BYTES / 2 + 1];
        state.push_frame(big.clone(), 1, AnchorKind::None);
        state.push_frame(big.clone(), 2, AnchorKind::None);
        state.push_frame(big, 3, AnchorKind::None);
        // Oldest two should have been evicted; only the last remains.
        assert_eq!(state.frames.len(), 1);
        assert_eq!(state.frames[0].pts, 3);
        assert!(state.frames_bytes <= MAX_AU_BUFFER_BYTES);
    }
}
