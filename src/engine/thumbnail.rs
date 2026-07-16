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
//!    the previous capture. A run of identical hashes spanning ~30 s raises
//!    a `"frozen"` alarm; the exact count scales with the configured capture
//!    cadence (see `resolve_thumbnail_timing`), so it is 6 captures at the
//!    5 s default and 30 at a 1 s cadence. The alarm is reset when the input
//!    ring runs dry or a capture errors so it does not stick across quiet
//!    periods.
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

/// Default thumbnail cadence (seconds) when a flow doesn't override it via
/// `FlowConfig.thumbnail_interval_secs`.
const DEFAULT_THUMBNAIL_INTERVAL_SECS: u64 = 5;

/// Wall-clock window of identical frames that defines "frozen".
const FREEZE_WINDOW_SECS: u64 = 30;

/// Freeze detection samples at least this far apart, **independent of the
/// preview capture cadence**. This is the load-bearing decoupling: a fast
/// preview (1 s) re-snapshots the buffered ring far more often than the
/// source produces a new random-access point, and the decoder legitimately
/// re-yields the *same* picture (the per-capture decode is anchored at the
/// most recent IDR and capped, so within one GOP successive fast captures
/// return an identical frame). Comparing those adjacent fast captures reads
/// a perfectly healthy feed as "frozen". Sampling freeze on a fixed floor
/// makes detection behave identically at every cadence — matching the
/// known-good 5 s default. The preview image still updates at the capture
/// cadence; only the *freeze comparison* is throttled to this floor.
const FREEZE_SAMPLE_FLOOR_SECS: u64 = 5;

/// How long the broadcast channel may stay silent before the input is
/// declared "gone" (`no_signal`). Flat — this is a data-arrival timeout, not
/// a frame-content one, so it is cadence-independent (it's evaluated on each
/// periodic tick; a slow cadence simply checks it less often). Picked so a
/// working stream's inter-packet jitter never trips it.
const NO_SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum gap between out-of-cycle captures driven by the external refresh
/// trigger. Prevents a flood of switch events from stacking decode work.
const TRIGGER_MIN_GAP: Duration = Duration::from_millis(500);

/// Per-capture decode floor — always decode at least this many AUs from the
/// anchor so even the fastest cadence produces a usable picture.
const MIN_DECODE_FRAMES: usize = 8;

/// Decode budget in AUs **per second**. The per-capture decode cap is
/// `interval_secs × this`, clamped to `[MIN_DECODE_FRAMES, MAX_DECODE_FRAMES]`,
/// so the decode *rate* stays ≈ constant (the proven 5 s load of ~10 AU/s)
/// regardless of cadence. Without this, a 1 s cadence decoded the full 50-AU
/// window every second — 5× the CPU of the 5 s default, per generator, which
/// could starve the real-time media path on a loaded host.
const DECODE_FRAMES_PER_SEC: usize = 10;

/// Per-cadence timing derived from the configured thumbnail interval.
struct ThumbnailTiming {
    /// How often to capture (and publish) a fresh preview frame.
    interval: Duration,
    /// Fixed spacing at which freeze detection samples, decoupled from the
    /// capture cadence — see [`FREEZE_SAMPLE_FLOOR_SECS`].
    freeze_sample_interval: Duration,
    /// Consecutive identical freeze *samples* required to raise "frozen".
    /// Derived from the sample spacing so "frozen" fires at ~FREEZE_WINDOW_SECS
    /// at any cadence.
    freeze_threshold: u64,
    /// Per-capture decode AU cap (media-codecs path), scaled so the decode
    /// rate stays bounded across cadences.
    decode_frames_cap: usize,
}

/// Resolve the per-flow thumbnail cadence (clamped to the validated 1..=60 s
/// range) plus the cadence-independent freeze sampling and the cadence-scaled
/// decode cap.
fn resolve_thumbnail_timing(interval_secs: Option<u32>) -> ThumbnailTiming {
    let secs = interval_secs
        .map(|s| (s as u64).clamp(1, 60))
        .unwrap_or(DEFAULT_THUMBNAIL_INTERVAL_SECS);
    // Freeze samples no faster than the floor; at slow cadences it samples
    // once per capture (it can't sample more often than captures happen).
    let sample_secs = secs.max(FREEZE_SAMPLE_FLOOR_SECS);
    // Decode cap scales with cadence so frames/sec stays ≈ constant; at the
    // 5 s default this resolves to the full MAX_DECODE_FRAMES (50), unchanged.
    let decode_frames_cap =
        (secs as usize * DECODE_FRAMES_PER_SEC).clamp(MIN_DECODE_FRAMES, MAX_DECODE_FRAMES);
    ThumbnailTiming {
        interval: Duration::from_secs(secs),
        freeze_sample_interval: Duration::from_secs(sample_secs),
        // ceil(window / sample_spacing), min 2 so a single repeat at a slow
        // cadence can't false-positive. At the 5 s sample floor this is 6,
        // matching the legacy behaviour exactly.
        freeze_threshold: FREEZE_WINDOW_SECS.div_ceil(sample_secs).max(2),
        decode_frames_cap,
    }
}

/// Apply a successful capture. The preview JPEG is published every capture,
/// and the instantaneous black check runs every capture, but freeze detection
/// only samples on the fixed [`ThumbnailTiming::freeze_sample_interval`] —
/// the "frozen" verdict persists between samples so a fast preview cadence
/// neither flickers nor false-trips the alarm.
fn process_capture(
    stats: &ThumbnailAccumulator,
    timing: &ThumbnailTiming,
    result: &InternalThumbnailResult,
    now: std::time::Instant,
    last_freeze_sample_at: &mut Option<std::time::Instant>,
    last_frozen: &mut bool,
) {
    stats.store(result.jpeg.clone());

    let freeze_due = last_freeze_sample_at
        .map(|t| now.duration_since(t) >= timing.freeze_sample_interval)
        .unwrap_or(true);
    if freeze_due {
        *last_freeze_sample_at = Some(now);
        let jpeg_hash = hash_jpeg(&result.jpeg);
        *last_frozen = stats.check_freeze(jpeg_hash, timing.freeze_threshold);
    }

    // Black is instantaneous and takes priority; otherwise carry the last
    // freeze verdict forward between samples.
    if result.luminance < BLACK_LUMINANCE_THRESHOLD {
        stats.set_alarm(Some("black".to_string()));
    } else if *last_frozen {
        stats.set_alarm(Some("frozen".to_string()));
    } else {
        stats.set_alarm(None);
    }
}

/// Clear freeze state after a capture error or a no-signal gap so a stale
/// "frozen" flag never sticks and the next good capture re-baselines cleanly.
fn reset_thumbnail_state(
    stats: &ThumbnailAccumulator,
    last_freeze_sample_at: &mut Option<std::time::Instant>,
    last_frozen: &mut bool,
) {
    stats.reset_freeze();
    *last_freeze_sample_at = None;
    *last_frozen = false;
}

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
///
/// This is the *upper bound*; the per-capture cap is scaled down at faster
/// cadences by [`resolve_thumbnail_timing`] (`decode_frames_cap`) so the
/// decode rate stays constant. Not feature-gated because the resolver
/// references it in every build.
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
/// `interval_secs` overrides the capture cadence (validated to 1..=60 s on
/// the config path); `None` keeps the [`DEFAULT_THUMBNAIL_INTERVAL_SECS`]
/// default. Freeze / no-signal detection windows scale with it.
pub fn spawn_thumbnail_generator(
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<ThumbnailAccumulator>,
    cancel: CancellationToken,
    program_number: Option<u16>,
    interval_secs: Option<u32>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(thumbnail_loop(
        rx,
        stats,
        cancel,
        program_number,
        interval_secs,
    ))
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
    interval_secs: Option<u32>,
) {
    #[cfg(feature = "media-codecs")]
    {
        video_engine::silence_ffmpeg_logs();
    }

    // The cadence is live-reconfigurable: an operator can change the flow's
    // thumbnail interval and `ThumbnailAccumulator::set_configured_interval`
    // updates a shared atomic + wakes us, so the new cadence applies without a
    // flow restart. `running_secs` tracks what `timing`/`interval` are built
    // for; the loop rebuilds them when the configured value diverges.
    stats.init_configured_interval(interval_secs);
    let mut running_secs = interval_secs;
    let mut timing = resolve_thumbnail_timing(running_secs);

    if let Some(n) = program_number {
        tracing::info!(
            "Thumbnail generator started (program filter target = {}, interval = {}s)",
            n,
            timing.interval.as_secs()
        );
    } else {
        tracing::info!(
            "Thumbnail generator started (interval = {}s)",
            timing.interval.as_secs()
        );
    }

    let mut interval = tokio::time::interval(timing.interval);
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
    // Freeze detection state — sampled on `timing.freeze_sample_interval`,
    // decoupled from the capture cadence. `last_frozen` carries the verdict
    // between samples so the alarm stays stable.
    let mut last_freeze_sample_at: Option<std::time::Instant> = None;
    let mut last_frozen = false;

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

    // Adaptive warm-decoder state (media-codecs only). A generator stays on the
    // cheap cold path until its output is observed stale on a live feed, then
    // escalates to a persistent decoder so long-GOP sources preview at cadence.
    #[cfg(feature = "media-codecs")]
    let mut warm: Option<WarmDecoder> = None;
    #[cfg(feature = "media-codecs")]
    let mut stale_count: u32 = 0;
    #[cfg(feature = "media-codecs")]
    let mut last_capture_hash: Option<u64> = None;

    loop {
        // Hot-apply a live cadence change. Checked every iteration — the loop
        // wakes frequently on `rx.recv()` while data flows, and
        // `set_configured_interval` also fires `refresh_trigger` so a change
        // applies promptly even on a silent flow. Rebuilds the timer + timing
        // (which re-derives the cadence-independent freeze sampling + decode
        // cap) and resets freeze state — a cadence change is a legitimate
        // point to re-baseline freeze detection.
        let configured_secs = stats.configured_interval();
        if configured_secs != running_secs {
            running_secs = configured_secs;
            timing = resolve_thumbnail_timing(running_secs);
            interval = tokio::time::interval(timing.interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await; // consume the immediate first tick
            last_freeze_sample_at = None;
            last_frozen = false;
            tracing::info!(
                "Thumbnail cadence changed live to {}s",
                timing.interval.as_secs()
            );
        }

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
                // Prefer a warm frame if the warm decoder has locked; else cold.
                #[cfg(feature = "media-codecs")]
                let result = match warm.as_ref().and_then(|w| w.latest()) {
                    Some(r) => Ok(r),
                    None => state.capture(timing.decode_frames_cap).await,
                };
                #[cfg(not(feature = "media-codecs"))]
                let result = state.capture(timing.decode_frames_cap).await;
                match result {
                    Ok(result) => {
                        let now = std::time::Instant::now();
                        #[cfg(feature = "media-codecs")]
                        note_capture_success(
                            &result,
                            &mut warm,
                            &mut stale_count,
                            &mut last_capture_hash,
                            state.codec(),
                            program_number,
                        );
                        process_capture(
                            &stats,
                            &timing,
                            &result,
                            now,
                            &mut last_freeze_sample_at,
                            &mut last_frozen,
                        );
                        last_capture = Some(now);
                        // Re-align the periodic interval so the next scheduled
                        // tick is one interval from now, not from whenever it
                        // last fired.
                        interval.reset();
                    }
                    Err(e) => {
                        #[cfg(feature = "media-codecs")]
                        note_capture_error(
                            &mut warm,
                            &mut stale_count,
                            state.codec(),
                            program_number,
                        );
                        tracing::debug!("Thumbnail trigger capture failed: {e}");
                        stats.record_error(e);
                        reset_thumbnail_state(
                            &stats,
                            &mut last_freeze_sample_at,
                            &mut last_frozen,
                        );
                    }
                }
            }

            _ = interval.tick() => {
                // Detect "gone" before "frozen": if the broadcast channel
                // has been silent for `NO_SIGNAL_TIMEOUT`, the input has
                // stopped — re-decoding the stale buffer would produce
                // identical JPEGs and trip the freeze alarm with the
                // wrong cause. Resetting freeze state here also gives a
                // clean slate for when the stream resumes.
                let stream_silent = last_useful_packet_at
                    .map(|t| t.elapsed() >= NO_SIGNAL_TIMEOUT)
                    .unwrap_or(true);
                if stream_silent || !state.has_data() {
                    reset_thumbnail_state(&stats, &mut last_freeze_sample_at, &mut last_frozen);
                    stats.set_alarm(Some("no_signal".to_string()));
                    // Source gone — tear down any warm decoder so a dead feed
                    // never holds a decode thread; it re-escalates if it returns.
                    #[cfg(feature = "media-codecs")]
                    {
                        if let Some(w) = warm.take() {
                            w.shutdown();
                        }
                        stale_count = 0;
                        last_capture_hash = None;
                    }
                    continue;
                }

                #[cfg(feature = "media-codecs")]
                let result = match warm.as_ref().and_then(|w| w.latest()) {
                    Some(r) => Ok(r),
                    None => state.capture(timing.decode_frames_cap).await,
                };
                #[cfg(not(feature = "media-codecs"))]
                let result = state.capture(timing.decode_frames_cap).await;
                match result {
                    Ok(result) => {
                        tracing::debug!("Thumbnail captured: {} bytes JPEG", result.jpeg.len());
                        let now = std::time::Instant::now();
                        #[cfg(feature = "media-codecs")]
                        note_capture_success(
                            &result,
                            &mut warm,
                            &mut stale_count,
                            &mut last_capture_hash,
                            state.codec(),
                            program_number,
                        );
                        process_capture(
                            &stats,
                            &timing,
                            &result,
                            now,
                            &mut last_freeze_sample_at,
                            &mut last_frozen,
                        );
                        last_capture = Some(now);
                    }
                    Err(e) => {
                        #[cfg(feature = "media-codecs")]
                        note_capture_error(
                            &mut warm,
                            &mut stale_count,
                            state.codec(),
                            program_number,
                        );
                        tracing::debug!("Thumbnail capture failed: {e}");
                        stats.record_error(e);
                        // Capture failed — we have no ground truth this tick,
                        // so clear freeze state rather than leaving a stale flag.
                        reset_thumbnail_state(
                            &stats,
                            &mut last_freeze_sample_at,
                            &mut last_frozen,
                        );
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
                            // Feed the warm decoder too (when escalated). Cheap
                            // copy, drop-on-full — never blocks the subscriber.
                            #[cfg(feature = "media-codecs")]
                            if let Some(w) = warm.as_ref() {
                                w.feed(Bytes::copy_from_slice(ts_payload));
                            }
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

    // Loop exited (cancelled / channel closed) — stop any warm decoder thread.
    #[cfg(feature = "media-codecs")]
    if let Some(w) = warm.take() {
        w.shutdown();
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
#[derive(Clone)]
struct InternalThumbnailResult {
    jpeg: Bytes,
    /// Average luminance (0.0-255.0). For black-screen detection.
    luminance: f64,
}

// ── Adaptive warm decoder (long-GOP responsiveness) ──────────────────────
//
// The cold path re-decodes from the most-recent keyframe each tick, so for a
// source with a long keyframe (IDR) interval — which an external feed can set
// arbitrarily and the operator can't control — the preview only advances when
// a fresh IDR is buffered (once per GOP, e.g. 22 s), no matter the configured
// cadence. To stay responsive for ANY source, a generator escalates to this
// warm decoder once its cold output is observed stale while packets are still
// arriving. The warm decoder keeps one libavcodec decoder alive on a dedicated
// thread, decoding every frame as it arrives (tracking the live edge
// regardless of GOP) and exposing the latest scaled+encoded JPEG in a shared
// slot the async loop samples at the cadence. Short-GOP sources never
// escalate, so the cheap cold path stays the default.

/// Warm-thread AU queue depth. Generous so transient broadcast bursts don't
/// drop AUs (a dropped AU corrupts the warm decoder until the next keyframe);
/// the thread decodes faster than real-time so it normally stays near-empty.
#[cfg(feature = "media-codecs")]
const WARM_AU_QUEUE: usize = 64;

/// Consecutive stale captures (identical output, or repeated decode failure)
/// on a feed that is still delivering packets before a generator escalates to
/// a warm decoder. At a 1 s cadence that's ~3 s of confirmed staleness.
#[cfg(feature = "media-codecs")]
const WARM_ESCALATE_STALE: u32 = 3;

/// Don't re-scale+encode faster than this on the warm thread — the async loop
/// only samples at the cadence (≥1 s), so encoding every decoded frame would
/// be wasted scale+JPEG work. Bounds the warm encode rate (~4/s) independent
/// of source fps, while staying fresher than the fastest 1 s cadence.
#[cfg(feature = "media-codecs")]
const WARM_ENCODE_MIN_GAP: Duration = Duration::from_millis(250);

#[cfg(feature = "media-codecs")]
enum WarmMsg {
    /// One stripped MPEG-TS payload to demux + decode.
    Ts(Bytes),
    Shutdown,
}

/// Handle to a warm decoder running on a dedicated `std::thread`.
#[cfg(feature = "media-codecs")]
struct WarmDecoder {
    tx: std::sync::mpsc::SyncSender<WarmMsg>,
    latest: Arc<std::sync::Mutex<Option<InternalThumbnailResult>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "media-codecs")]
impl WarmDecoder {
    fn spawn(
        codec: video_codec::VideoCodec,
        program_number: Option<u16>,
        cfg: video_codec::ThumbnailConfig,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<WarmMsg>(WARM_AU_QUEUE);
        let latest = Arc::new(std::sync::Mutex::new(None));
        let latest_thread = latest.clone();
        let handle = std::thread::Builder::new()
            .name("thumb-warm".to_string())
            .spawn(move || warm_decode_loop(codec, program_number, cfg, rx, latest_thread))
            .ok();
        Self {
            tx,
            latest,
            handle,
        }
    }

    /// Forward a stripped TS payload to the warm thread. Drop-on-full — never
    /// blocks the broadcast subscriber / data path.
    fn feed(&self, ts: Bytes) {
        let _ = self.tx.try_send(WarmMsg::Ts(ts));
    }

    /// The latest warm JPEG, or `None` until the decoder has locked (e.g. right
    /// after escalation, before the next IDR lets it start producing frames).
    fn latest(&self) -> Option<InternalThumbnailResult> {
        self.latest.lock().unwrap().clone()
    }

    fn shutdown(mut self) {
        let _ = self.tx.send(WarmMsg::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Dedicated-thread loop: owns one persistent decoder + scaler + encoder,
/// decodes every arriving frame to track the live edge, and republishes the
/// latest scaled JPEG into the shared slot at most every `WARM_ENCODE_MIN_GAP`.
#[cfg(feature = "media-codecs")]
fn warm_decode_loop(
    codec: video_codec::VideoCodec,
    program_number: Option<u16>,
    cfg: video_codec::ThumbnailConfig,
    rx: std::sync::mpsc::Receiver<WarmMsg>,
    latest: Arc<std::sync::Mutex<Option<InternalThumbnailResult>>>,
) {
    video_engine::silence_ffmpeg_logs();
    let mut demuxer = TsDemuxer::new(program_number);
    let mut decoder = match video_engine::VideoDecoder::open(codec) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Warm thumbnail decoder: open failed: {e}");
            return;
        }
    };
    // Scaler cached by source dims; rebuilt on a resolution change.
    let mut scaler: Option<(video_engine::VideoScaler, u32, u32)> = None;
    let encoder = video_engine::JpegEncoder::new(cfg.quality);
    let mut last_encode: Option<std::time::Instant> = None;

    while let Ok(msg) = rx.recv() {
        let ts = match msg {
            WarmMsg::Ts(b) => b,
            WarmMsg::Shutdown => break,
        };
        for frame in demuxer.demux(&ts) {
            let (au, pts): (Vec<u8>, u64) = match frame {
                DemuxedFrame::H264 { nalus, pts, .. } => (build_annex_b(&nalus), pts),
                DemuxedFrame::H265 { nalus, pts, .. } => (build_annex_b(&nalus), pts),
                DemuxedFrame::Mpeg2 { es, pts, .. } => (es, pts),
                _ => continue,
            };
            if au.is_empty() {
                continue;
            }
            if decoder.send_packet_with_pts(&au, pts as i64).is_err() {
                continue;
            }
            // A `DecodedFrame` is only valid until the next `receive_frame`, so
            // republish it (rate-limited) the moment it's decoded — never hold
            // it across another decode.
            loop {
                let f = match decoder.receive_frame() {
                    Ok(f) => f,
                    Err(_) => break,
                };
                let due = last_encode
                    .map(|t| t.elapsed() >= WARM_ENCODE_MIN_GAP)
                    .unwrap_or(true);
                if due {
                    if let Some(result) = warm_scale_encode(&f, &mut scaler, &encoder, &cfg) {
                        *latest.lock().unwrap() = Some(result);
                        last_encode = Some(std::time::Instant::now());
                    }
                }
            }
        }
    }
}

#[cfg(feature = "media-codecs")]
fn warm_scale_encode(
    frame: &video_engine::DecodedFrame,
    scaler: &mut Option<(video_engine::VideoScaler, u32, u32)>,
    encoder: &video_engine::JpegEncoder,
    cfg: &video_codec::ThumbnailConfig,
) -> Option<InternalThumbnailResult> {
    let (sw, sh) = (frame.width(), frame.height());
    if sw == 0 || sh == 0 {
        return None;
    }
    let need_new = !matches!(scaler, Some((_, w, h)) if *w == sw && *h == sh);
    if need_new {
        match video_engine::VideoScaler::new(sw, sh, frame.pixel_format(), cfg.width, cfg.height) {
            Ok(s) => *scaler = Some((s, sw, sh)),
            Err(e) => {
                tracing::debug!("Warm thumbnail scaler init failed: {e}");
                return None;
            }
        }
    }
    let (s, _, _) = scaler.as_ref()?;
    let scaled = s.scale(frame).ok()?;
    let jpeg = encoder.encode(&scaled).ok()?;
    Some(InternalThumbnailResult {
        jpeg,
        luminance: frame.average_luminance(),
    })
}

/// Spawn a warm decoder for this generator if not already running.
#[cfg(feature = "media-codecs")]
fn escalate_warm(
    warm: &mut Option<WarmDecoder>,
    codec: Option<video_codec::VideoCodec>,
    program_number: Option<u16>,
    reason: &str,
) {
    if warm.is_some() {
        return;
    }
    let Some(codec) = codec else {
        return; // codec not classified yet — try again next capture
    };
    tracing::info!(
        "Thumbnail: {reason} while packets arriving (likely long-GOP source) — \
         escalating to warm decoder for a responsive preview"
    );
    let cfg = video_codec::ThumbnailConfig {
        width: THUMBNAIL_WIDTH,
        height: THUMBNAIL_HEIGHT,
        quality: 5,
    };
    *warm = Some(WarmDecoder::spawn(codec, program_number, cfg));
}

/// Post-capture escalation bookkeeping on a SUCCESSFUL cold capture. Tracks
/// consecutive identical output and escalates to warm when the preview is
/// confirmed stale on a still-live feed. No-op once warm (warm frames are
/// fresh by construction).
#[cfg(feature = "media-codecs")]
fn note_capture_success(
    result: &InternalThumbnailResult,
    warm: &mut Option<WarmDecoder>,
    stale_count: &mut u32,
    last_capture_hash: &mut Option<u64>,
    codec: Option<video_codec::VideoCodec>,
    program_number: Option<u16>,
) {
    let h = hash_jpeg(&result.jpeg);
    if warm.is_some() {
        *stale_count = 0;
        *last_capture_hash = Some(h);
        return;
    }
    if *last_capture_hash == Some(h) {
        *stale_count += 1;
    } else {
        *stale_count = 0;
    }
    *last_capture_hash = Some(h);
    if *stale_count >= WARM_ESCALATE_STALE {
        escalate_warm(warm, codec, program_number, "preview output not advancing");
        *stale_count = 0;
    }
}

/// Post-capture escalation bookkeeping on a FAILED cold capture. A repeated
/// failure on a live feed means the decoder cannot re-anchor mid-GOP (the IDR
/// has scrolled out of the buffer) — also a long-GOP signature.
#[cfg(feature = "media-codecs")]
fn note_capture_error(
    warm: &mut Option<WarmDecoder>,
    stale_count: &mut u32,
    codec: Option<video_codec::VideoCodec>,
    program_number: Option<u16>,
) {
    if warm.is_some() {
        return; // warm decoder is warming up; a cold failure here is expected
    }
    *stale_count += 1;
    if *stale_count >= WARM_ESCALATE_STALE {
        escalate_warm(warm, codec, program_number, "preview decode failing");
        *stale_count = 0;
    }
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

    /// The resolved video codec, once the demuxer has classified the stream.
    /// Used to open a warm decoder on escalation.
    fn codec(&self) -> Option<video_codec::VideoCodec> {
        self.codec
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
                DemuxedFrame::Discontinuity | DemuxedFrame::Scte35(_) => {}
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
    fn snapshot_for_decode(
        &self,
        decode_cap: usize,
    ) -> Option<(Vec<u8>, Vec<(Vec<u8>, i64)>, video_codec::VideoCodec)> {
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
            .unwrap_or_else(|| len.saturating_sub(decode_cap));

        let packets: Vec<(Vec<u8>, i64)> = self
            .frames
            .iter()
            .skip(anchor)
            .take(decode_cap)
            .map(|f| (f.annex_b.clone(), f.pts))
            .collect();

        let headers = build_headers(&self.demuxer, codec);
        Some((headers, packets, codec))
    }

    async fn capture(&self, decode_cap: usize) -> Result<InternalThumbnailResult, String> {
        let (headers, packets, codec) = self
            .snapshot_for_decode(decode_cap)
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
    // One-shot snapshot (not on the live cadence path) → full decode window.
    state.snapshot_for_decode(MAX_DECODE_FRAMES)
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

    // `_decode_cap` is the media-codecs frame cap; the subprocess path pipes
    // a raw TS chunk to ffmpeg and lets it pick the frame, so it's unused here.
    async fn capture(&self, _decode_cap: usize) -> Result<InternalThumbnailResult, String> {
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
    fn configured_interval_roundtrips_and_clamps() {
        // The live-cadence channel the loop reads each iteration. None = the
        // spawn default; values clamp to the validated 1..=60 range.
        let acc = ThumbnailAccumulator::new();
        assert_eq!(acc.configured_interval(), None);
        acc.init_configured_interval(Some(2));
        assert_eq!(acc.configured_interval(), Some(2));
        acc.set_configured_interval(Some(1));
        assert_eq!(acc.configured_interval(), Some(1));
        acc.set_configured_interval(Some(30));
        assert_eq!(acc.configured_interval(), Some(30));
        acc.set_configured_interval(None);
        assert_eq!(acc.configured_interval(), None);
        // Clamp out-of-range to the bounds rather than 0 (which would mean
        // "default") or an unbounded value.
        acc.set_configured_interval(Some(0));
        assert_eq!(acc.configured_interval(), Some(1));
        acc.set_configured_interval(Some(1000));
        assert_eq!(acc.configured_interval(), Some(60));
    }

    #[test]
    fn thumbnail_timing_default_matches_legacy_5s() {
        // None must reproduce the historical hardcoded behaviour exactly.
        let t = resolve_thumbnail_timing(None);
        assert_eq!(t.interval, Duration::from_secs(5));
        assert_eq!(t.freeze_sample_interval, Duration::from_secs(5));
        assert_eq!(t.freeze_threshold, 6);
        assert_eq!(t.decode_frames_cap, 50);
    }

    #[test]
    fn freeze_sampling_floor_is_cadence_independent() {
        // The load-bearing fix: freeze sampling never goes faster than the
        // 5 s floor (so fast previews sample like the 5 s default); slow
        // cadences sample once per capture and the threshold scales down so
        // "frozen" still fires at ~FREEZE_WINDOW_SECS.
        let cases = [
            (1u32, 5u64, 6u64),
            (2, 5, 6),
            (5, 5, 6),
            (10, 10, 3),
            (30, 30, 2), // ceil(30/30)=1 → floored to 2
        ];
        for (secs, want_sample_secs, want_threshold) in cases {
            let t = resolve_thumbnail_timing(Some(secs));
            assert_eq!(
                t.freeze_sample_interval,
                Duration::from_secs(want_sample_secs),
                "freeze sample interval wrong at {secs}s"
            );
            assert_eq!(
                t.freeze_threshold, want_threshold,
                "freeze threshold wrong at {secs}s"
            );
        }
    }

    #[test]
    fn decode_cap_keeps_decode_rate_bounded() {
        // Per-capture decode scales with cadence so the decode RATE stays at
        // or below the proven 5 s load (~10 AU/s) — not the 50 AU/s a naive
        // 1 s cadence would have burned per generator.
        let cases = [(1u32, 10usize), (2, 20), (5, 50), (10, 50), (30, 50)];
        for (secs, want_cap) in cases {
            let t = resolve_thumbnail_timing(Some(secs));
            assert_eq!(t.decode_frames_cap, want_cap, "decode cap wrong at {secs}s");
            let rate = t.decode_frames_cap as f64 / secs as f64;
            assert!(rate <= 10.0 + 1e-9, "decode rate {rate} AU/s too high at {secs}s");
        }
    }

    #[test]
    fn thumbnail_timing_clamps_out_of_range() {
        // Even if an out-of-range value slips past config validation, the
        // resolver clamps to 1..=60 rather than producing a zero-duration
        // interval (which would busy-loop the timer).
        assert_eq!(resolve_thumbnail_timing(Some(0)).interval, Duration::from_secs(1));
        assert_eq!(resolve_thumbnail_timing(Some(1000)).interval, Duration::from_secs(60));
    }

    /// Drive `process_capture` directly with synthetic frames + fabricated
    /// monotonic timestamps to prove the freeze behaviour end-to-end. This is
    /// the regression guard for the 1 s false-frozen bug.
    fn run_capture_sim(
        cadence_secs: u32,
        ticks: u64,
        jpeg_for: impl Fn(u64) -> Bytes,
    ) -> (bool, Option<u64>) {
        let stats = ThumbnailAccumulator::new();
        let timing = resolve_thumbnail_timing(Some(cadence_secs));
        let base = std::time::Instant::now();
        let mut last_sample: Option<std::time::Instant> = None;
        let mut last_frozen = false;
        let mut first_frozen_tick: Option<u64> = None;
        for i in 0..ticks {
            let result = InternalThumbnailResult {
                jpeg: jpeg_for(i),
                luminance: 100.0, // not black
            };
            // Captures are `cadence_secs` apart on the wall clock.
            let now = base + Duration::from_secs(i * cadence_secs as u64);
            process_capture(&stats, &timing, &result, now, &mut last_sample, &mut last_frozen);
            if last_frozen && first_frozen_tick.is_none() {
                first_frozen_tick = Some(i);
            }
        }
        (last_frozen, first_frozen_tick)
    }

    #[test]
    fn freeze_never_trips_on_distinct_fast_captures() {
        // A healthy feed previewed at 1 s — a different frame every second —
        // must NEVER read as frozen. This is exactly what regressed.
        let (frozen, _) =
            run_capture_sim(1, 60, |i| Bytes::from(vec![i as u8, (i >> 8) as u8, 0xAB]));
        assert!(!frozen, "distinct frames at 1 s must never read as frozen");
    }

    #[test]
    fn freeze_trips_after_window_on_static_feed_at_any_cadence() {
        // A genuinely static feed must still be caught. At 1 s preview cadence
        // freeze samples every 5 s (floor) with threshold 6, so the 6th
        // identical sample lands at t=25 s — not on the 2nd 1 s capture.
        let frozen_jpeg = Bytes::from_static(b"static-frame-bytes");
        let (frozen_1s, at_1s) = run_capture_sim(1, 40, |_| frozen_jpeg.clone());
        assert!(frozen_1s, "a static feed at 1 s must eventually read frozen");
        // 6 samples at the 5 s floor: ticks 0,5,10,15,20,25 → frozen at tick 25.
        assert_eq!(at_1s, Some(25), "frozen fired at the wrong 1 s tick");

        // Same at the 5 s default: samples every tick, threshold 6 → tick 5.
        let (frozen_5s, at_5s) = run_capture_sim(5, 12, |_| frozen_jpeg.clone());
        assert!(frozen_5s);
        assert_eq!(at_5s, Some(5), "frozen fired at the wrong 5 s tick");
    }

    #[test]
    fn freeze_clears_when_a_static_feed_resumes_motion() {
        // Static for a while (trips frozen), then frames change again: the
        // next freeze sample with a different hash must clear the alarm.
        let stats = ThumbnailAccumulator::new();
        let timing = resolve_thumbnail_timing(Some(1)); // sample every 5 s
        let base = std::time::Instant::now();
        let mut last_sample = None;
        let mut last_frozen = false;
        let frozen_jpeg = Bytes::from_static(b"frozen");
        // 35 s of static → frozen by t=25 s.
        for i in 0..36u64 {
            let r = InternalThumbnailResult { jpeg: frozen_jpeg.clone(), luminance: 100.0 };
            process_capture(&stats, &timing, &r, base + Duration::from_secs(i), &mut last_sample, &mut last_frozen);
        }
        assert!(last_frozen, "should be frozen after 35 s static");
        // Motion resumes: distinct frames. The next freeze sample (≥5 s after
        // the last) sees a new hash and clears.
        for i in 36..46u64 {
            let r = InternalThumbnailResult { jpeg: Bytes::from(vec![i as u8]), luminance: 100.0 };
            process_capture(&stats, &timing, &r, base + Duration::from_secs(i), &mut last_sample, &mut last_frozen);
        }
        assert!(!last_frozen, "frozen must clear once motion resumes");
        assert_ne!(stats.current_alarm(), Some("frozen".to_string()));
    }

    /// Read the committed long-GOP asset (22 s keyframe interval), or None to
    /// skip when it isn't present (fresh checkout without the testbed asset).
    fn load_long_gop_asset() -> Option<Vec<u8>> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testbed/assets/longgop_g550_25fps.ts"
        );
        match std::fs::read(path) {
            Ok(b) => Some(b),
            Err(_) => {
                eprintln!("SKIP long-GOP empirical test: asset not found at {path}");
                None
            }
        }
    }

    /// Empirical, real-decode proof of the long-GOP fix. Feeds a moving source
    /// with a 22 s keyframe interval through the COLD path (which pins to a
    /// frame just after each keyframe) and the WARM decode logic (a persistent
    /// decoder that tracks every frame), and asserts the warm path yields far
    /// more distinct previews. Deterministic — no threads or wall-clock timing.
    #[test]
    fn empirical_warm_refreshes_long_gop_while_cold_pins() {
        use std::collections::HashSet;
        let Some(ts) = load_long_gop_asset() else {
            return;
        };
        video_engine::silence_ffmpeg_logs();
        let cfg = video_codec::ThumbnailConfig {
            width: THUMBNAIL_WIDTH,
            height: THUMBNAIL_HEIGHT,
            quality: 5,
        };

        // ── COLD: progressive ingest + capture from the most-recent keyframe.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut cold_hashes: HashSet<u64> = HashSet::new();
        rt.block_on(async {
            let mut state = LiveState::new(None);
            for chunk in ts.chunks(188 * 280) {
                state.ingest(chunk);
                if let Ok(r) = state.capture(50).await {
                    cold_hashes.insert(hash_jpeg(&r.jpeg));
                }
            }
        });

        // ── WARM decode logic: one persistent decoder over the whole stream,
        // republishing the latest frame as it advances (what the warm thread
        // does, exercised synchronously so the measurement is deterministic).
        let mut warm_hashes: HashSet<u64> = HashSet::new();
        let mut demuxer = TsDemuxer::new(None);
        let mut decoder = video_engine::VideoDecoder::open(video_codec::VideoCodec::H264).unwrap();
        let mut scaler: Option<(video_engine::VideoScaler, u32, u32)> = None;
        let encoder = video_engine::JpegEncoder::new(cfg.quality);
        let mut frame_no = 0u32;
        for chunk in ts.chunks(188 * 7) {
            for frame in demuxer.demux(chunk) {
                let (au, pts): (Vec<u8>, u64) = match frame {
                    DemuxedFrame::H264 { nalus, pts, .. } => (build_annex_b(&nalus), pts),
                    _ => continue,
                };
                if decoder.send_packet_with_pts(&au, pts as i64).is_err() {
                    continue;
                }
                while let Ok(f) = decoder.receive_frame() {
                    frame_no += 1;
                    // Sample roughly the rate the async loop would at ~1 s.
                    if frame_no % 25 == 0 {
                        if let Some(r) = warm_scale_encode(&f, &mut scaler, &encoder, &cfg) {
                            warm_hashes.insert(hash_jpeg(&r.jpeg));
                        }
                    }
                }
            }
        }

        eprintln!(
            "long-GOP (22 s keyframe interval): COLD distinct previews = {}, WARM distinct previews = {} (over ~{} decoded frames)",
            cold_hashes.len(),
            warm_hashes.len(),
            frame_no
        );
        // Cold pins to a couple of frames (one short burst per 22 s keyframe);
        // warm tracks the moving picture and refreshes throughout.
        assert!(
            warm_hashes.len() >= 10,
            "warm should produce many distinct previews on a long-GOP source (got {})",
            warm_hashes.len()
        );
        assert!(
            warm_hashes.len() > cold_hashes.len() * 3,
            "warm ({}) should refresh far more often than cold ({}) on a long-GOP source",
            warm_hashes.len(),
            cold_hashes.len()
        );
    }

    /// Smoke test for the THREADED warm decoder end-to-end: feed the asset and
    /// confirm the dedicated thread locks and republishes at least a couple of
    /// distinct previews into the shared slot. Tolerant of timing/drops.
    #[test]
    fn warm_decoder_thread_produces_previews() {
        use std::collections::HashSet;
        let Some(ts) = load_long_gop_asset() else {
            return;
        };
        let cfg = video_codec::ThumbnailConfig {
            width: THUMBNAIL_WIDTH,
            height: THUMBNAIL_HEIGHT,
            quality: 5,
        };
        let warm = WarmDecoder::spawn(video_codec::VideoCodec::H264, None, cfg);
        let mut hashes: HashSet<u64> = HashSet::new();
        for chunk in ts.chunks(188 * 7) {
            warm.feed(Bytes::copy_from_slice(chunk));
            // Pace gently so the bounded channel isn't constantly overrun and
            // the wall-clock encode gate produces distinct samples.
            std::thread::sleep(std::time::Duration::from_millis(2));
            if let Some(r) = warm.latest() {
                hashes.insert(hash_jpeg(&r.jpeg));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
        if let Some(r) = warm.latest() {
            hashes.insert(hash_jpeg(&r.jpeg));
        }
        warm.shutdown();
        eprintln!("threaded warm decoder distinct previews = {}", hashes.len());
        assert!(
            hashes.len() >= 2,
            "threaded warm decoder should republish multiple distinct previews (got {})",
            hashes.len()
        );
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
        let (_headers, packets, _codec) = state.snapshot_for_decode(MAX_DECODE_FRAMES).unwrap();
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
        let (_headers, packets, _codec) = state.snapshot_for_decode(MAX_DECODE_FRAMES).unwrap();
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
        let (_headers, packets, _codec) = state.snapshot_for_decode(MAX_DECODE_FRAMES).unwrap();
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
        let (_headers, packets, _codec) = state.snapshot_for_decode(MAX_DECODE_FRAMES).unwrap();
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
        let (_headers, packets, _codec) = state.snapshot_for_decode(MAX_DECODE_FRAMES).unwrap();
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
        let (_headers, packets, _codec) = state.snapshot_for_decode(MAX_DECODE_FRAMES).unwrap();
        assert_eq!(packets.len(), MAX_DECODE_FRAMES);
        assert_eq!(packets[0].1, (total - MAX_DECODE_FRAMES + 1) as i64);
        assert_eq!(packets.last().unwrap().1, total as i64);
    }

    #[test]
    fn snapshot_returns_none_when_codec_unknown() {
        let state = LiveState::new(None);
        assert!(state.snapshot_for_decode(MAX_DECODE_FRAMES).is_none());
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
