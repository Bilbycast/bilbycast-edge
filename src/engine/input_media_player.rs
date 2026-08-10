// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! File-backed media-player input.
//!
//! Plays one or more local assets — pre-encoded MPEG-TS, MP4 / MOV
//! containers, or still images — as a paced fresh MPEG-TS feed onto the
//! per-input broadcast channel. Designed for the "fallback while live is
//! gone" use case: combine a media-player input with a live primary on a
//! PID-bus Hitless leg of an Assembled flow and the assembler's 200 ms
//! stall threshold cuts over to the player automatically.
//!
//! Files live under the edge's media-library directory (default
//! `~/.bilbycast/media/`, override via `BILBYCAST_MEDIA_DIR`). The manager
//! UI uploads files via chunked WS commands; see `crate::media`.
//!
//! # Source kinds
//!
//! | Kind | Pacing | Codecs | Features required |
//! |------|--------|--------|--------------------|
//! | `ts`    | embedded PCR (or `paced_bitrate_bps`) | passthrough — anything inside the TS | none |
//! | `mp4`   | sample PTS              | H.264 / HEVC video, AAC audio | none (pure-Rust mp4 demux + TsMuxer) |
//! | `image` | encoder frame rate      | re-encode to H.264 + silent AAC | `media-codecs` + `fdk-aac` |
//!
//! The TS path is the lowest-CPU, lowest-latency option — recommended for
//! pre-baked broadcast slates. MP4 and image paths exist for operator
//! convenience (most slate assets live as MP4 in production media stores
//! and as PNG / JPEG marketing artwork).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::config::models::{MediaPlayerInputConfig, MediaPlayerSource};
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{FlowStatsAccumulator, MediaPlayerStats, media_player_state};

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
mod image_slate;
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
mod mp4_demux;

// The playlist compatibility planner is pure logic over asset manifests with
// no codec/demuxer dependency, so it is always available (the manager needs
// it to validate a playlist regardless of which media features this edge was
// built with).
pub mod planner;

// The transition state machine is the pure-logic core of the Phase 3
// controller (state graph, generation counter, operator-Next idempotency).
pub mod transition;

// Phase 3b: the controller command surface (operator-Next channel, readiness
// check, runtime flag). The controller LOOP lives in this file
// (`run_controlled`) because it needs the private play_source/PlayerSession;
// this module holds the independent, testable pieces.
pub mod controller;

// Phase 6: explicit rational frame-rate conversion cadence for normalised
// playback. Pure integer arithmetic with no codec/hardware dependency (the
// vendored FFmpeg has `--disable-avfilter`, so there is no fps filter to
// delegate to). Driven by the normalisation pipeline once that lands.
pub mod frame_rate;

/// 188-byte MPEG-TS packet size.
const TS_PACKET: usize = 188;
/// MPEG-TS sync byte (every TS packet starts with this).
const SYNC_BYTE: u8 = 0x47;
/// Maximum trailer length across supported TS packet-size variants:
/// 188 (none), 192-byte M2TS / AVCHD / Blu-ray BDAV (4-byte timestamp prefix
/// — see note in [`detect_ts_packet_size_in_buf`] re: prefix vs. suffix),
/// 204-byte DVB Reed-Solomon (16-byte parity suffix). 16 bytes covers all
/// known broadcast variants.
const MAX_TS_TRAILER: usize = 16;
/// Bundle 7 × 188 = 1316 bytes per `RtpPacket` — the standard MPEG-TS-over-RTP
/// payload framing the rest of the engine already assumes.
pub(super) const PACKETS_PER_BUNDLE: usize = 7;
pub(super) const BUNDLE_SIZE: usize = TS_PACKET * PACKETS_PER_BUNDLE;
/// Fallback when the file has no PCR and no `paced_bitrate_bps` override.
const DEFAULT_FALLBACK_BITRATE_BPS: u64 = 4_000_000;

/// Async-to-OS-thread queue depth for the TS-file pacer. Sized to absorb
/// short scheduler stalls on the producer side without growing wire-side
/// latency: 16 × 1316 B ≈ 21 KB ≈ 34 ms of buffer at 5 Mbps, ≈ 56 µs at
/// 3 Gbps. Producer-side `try_send` is the failure path on overflow — the
/// async loop yields back to tokio and retries so backpressure flows
/// through the channel rather than dropping bundles upstream of the pacer.
const PACER_QUEUE_CAP: usize = 16;

/// Messages from the file-reading async task to the OS-thread pacer.
///
/// Each message carries a fully-prepared `RtpPacket` (transcoder + post-
/// process already applied on the async side under `block_in_place`) plus
/// the producer's latest cumulative-bitrate estimate. Embedding the rate
/// in every bundle (rather than a separate `Bitrate` variant) sidesteps
/// the producer's channel-full backpressure: when the queue is saturated
/// — the steady state for a fast file reader and a 5 Mbps wire pace —
/// an out-of-band rate update would be dropped by `try_send`, leaving
/// the OS thread stuck on its initial rate. Carrying the rate on each
/// bundle guarantees the OS thread sees the current estimate within
/// one bundle period of any change.
struct PacerMsg {
    pkt: crate::engine::packet::RtpPacket,
    /// **Explicit absolute emission deadline** in the pacer's monotonic
    /// clock domain (`crate::engine::wire_emit::monotonic_now_ns`).
    ///
    /// - `Some(t)` — the producer has computed the exact wall time this
    ///   bundle must hit the wire. The OS thread sleeps to `t` and emits,
    ///   ignoring `bitrate_bps`/`content_src_bytes`. Used by the **MP4
    ///   path** (`mp4_demux::play_demuxed`), which anchors each video
    ///   sample's first (PCR-bearing) bundle to the sample's true
    ///   presentation time and spreads the sample's remaining bundles
    ///   across the sample's own duration. This keeps PCR-packet arrival
    ///   exactly linear in PCR value (TR-101290 PCR_AC / PCR_repetition
    ///   pass) while still smoothing a large sample's packet burst — the
    ///   byte-rate mode below can't do both, because byte-rate pacing
    ///   decouples packet arrival from the sample-timestamp PCR clock and
    ///   drifts on VBR content (verified on hardware: byte-rate pacing an
    ///   MP4 grew `pcr_accuracy_errors` continuously, this deadline mode
    ///   holds them at zero).
    /// - `None` — byte-rate mode: the OS thread computes the deadline from
    ///   `bitrate_bps` + `content_src_bytes`.
    ///
    /// The TS-passthrough path (`play_ts_file`) used to be the `None` case,
    /// on the reasoning that a TS has no per-sample presentation timeline
    /// and "real TS files are well-behaved / near-CBR". The second half was
    /// the load-bearing half and it is false: on VBR the estimated rate is
    /// wrong by construction, and byte-rate mode integrates that error
    /// forward without bound. It now anchors on the source's own PCR
    /// instead — which *is* its presentation timeline, and was in the bytes
    /// the whole time — so a TS message carries `Some` from its first PCR
    /// onward. `BILBYCAST_MEDIA_PLAYER_PCR_DEADLINES=0` restores the old
    /// behaviour.
    ///
    /// **A TS pacer thread therefore sees both modes in sequence**: byte-rate
    /// until the first PCR, deadlines after. Anything hoisted out of the
    /// receive loop must survive that transition — `next_target_wall` in
    /// particular is frozen at its pre-PCR value for the rest of the file.
    target_ns: Option<u64>,
    /// Producer's current bitrate estimate (bps) for the file. The OS
    /// thread re-anchors `iter_start_wall` whenever this differs from
    /// its last-seen value. Ignored when `target_ns` is `Some`.
    bitrate_bps: u64,
    /// Number of **source** TS bytes this message represents — i.e. how
    /// much file-content was consumed to produce `pkt`, *including* bytes
    /// from any preceding source bundles the transcoder buffered without
    /// emitting (decode reorder + encoder latency). The pacer derives this
    /// message's wall-clock duration as `content_src_bytes * 8 / bitrate_bps`.
    ///
    /// **Why not pace by a fixed `BUNDLE_SIZE`?** In passthrough every
    /// emitted message is exactly one `BUNDLE_SIZE` source bundle, so a
    /// fixed slice was accidentally correct. With an input-level transcoder
    /// the emitted-message count diverges from the source-bundle count
    /// (the transcoder buffers, then flushes several frames in one call),
    /// so a fixed-`BUNDLE_SIZE` slice under-times the merged messages and
    /// the file plays ~2 % fast (measured: Sky-Witness 1080i25 looped in
    /// ~167.5 s instead of 171.5 s; wire PCR ran +20 000 ppm vs wallclock,
    /// breaking receiver clock recovery -> audio dropout). Crediting each
    /// message with the actual source bytes it carries conserves content
    /// time exactly, independent of how the transcoder chunks its output.
    content_src_bytes: u64,
}

/// Public entry point. Matches the signature shape of `input_test_pattern`
/// and `input_bonded` so the flow runtime calls it through the same
/// per-input forwarder pipeline.
#[allow(clippy::too_many_arguments)]
pub fn spawn_media_player_input(
    config: MediaPlayerInputConfig,
    per_input_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: EventSender,
    flow_id: String,
    input_id: String,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    // Operator-command channel (Phase 3b). `None` when the flow runtime did
    // not wire one (the controller is opt-in); the legacy loop ignores it.
    cmd_rx: Option<tokio::sync::mpsc::Receiver<controller::MediaPlayerCommand>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run(
            config,
            per_input_tx,
            stats,
            cancel,
            events,
            flow_id,
            input_id,
            av_sync_pacer,
            cmd_rx,
        )
        .await;
    })
}

/// Resolve the media-library directory. Order:
///
/// 1. `BILBYCAST_MEDIA_DIR` env var (operator override)
/// 2. `$XDG_DATA_HOME/bilbycast/media/` (Linux desktop convention)
/// 3. `$HOME/.bilbycast/media/` (Unix fallback)
/// 4. `./media/` (final fallback when running detached from a $HOME)
///
/// The directory may not exist yet — callers that need to write to it
/// should `tokio::fs::create_dir_all` first.
pub fn media_dir() -> PathBuf {
    if let Ok(p) = std::env::var("BILBYCAST_MEDIA_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("bilbycast").join("media");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".bilbycast").join("media");
    }
    PathBuf::from("./media")
}

/// Resolve a media filename inside the media directory, rejecting any
/// attempt to escape it via traversal. The validator already does most
/// of the work at config save time; this is a defence-in-depth check
/// against operators editing config.json by hand.
pub fn resolve_media_path(name: &str) -> Result<PathBuf> {
    if name.contains('/') || name.contains('\\') || name.contains('\0') || name.contains("..") {
        return Err(anyhow!("media filename '{name}' is not allowed"));
    }
    Ok(media_dir().join(name))
}

#[allow(clippy::too_many_arguments)]
async fn run(
    config: MediaPlayerInputConfig,
    per_input_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: EventSender,
    flow_id: String,
    input_id: String,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    cmd_rx: Option<tokio::sync::mpsc::Receiver<controller::MediaPlayerCommand>>,
) {
    events.emit_flow(
        EventSeverity::Info,
        category::FLOW,
        format!(
            "Media-player input '{input_id}' started ({} source(s), loop={}, shuffle={})",
            config.sources.len(),
            config.loop_playback,
            config.shuffle,
        ),
        &flow_id,
    );

    // Build the ingress transcoder once for the whole playlist lifetime so
    // PMT discovery and codec buffers persist across file transitions.
    let mut transcoder = match crate::engine::input_transcode::InputTranscoder::new(
        config.audio_encode.as_ref(),
        config.transcode.as_ref(),
        config.video_encode.as_ref(),
        None,
    ) {
        Ok(t) => {
            if let Some(ref t) = t {
                tracing::info!("Media-player input: ingress transcode active — {}", t.describe());
            }
            t
        }
        Err(e) => {
            events.emit_flow(
                EventSeverity::Critical,
                category::FLOW,
                format!("Media-player input transcode disabled: {e}"),
                &flow_id,
            );
            None
        }
    };
    // **Per-input** PCR forward-jump signal channel. Built once here in
    // the input pipeline and shared with the input's audio replacer
    // (via `InputTranscoder::set_pcr_jump_signal`) and its
    // `TsPtsRewriter` (via `InputPostProcessConfig.pcr_jump_signal`).
    // Each input owns its own counter so cross-input loop wraps can't
    // pollute the active input's audio (the bug that motivated this
    // design — see `bilbycast-edge` commit 48ea5dc 0.84.0 v2).
    //
    // Critical for media_player: every loop boundary in a multi-source
    // playlist (or single-source loop) emits a forward PCR jump as the
    // splice re-anchors. Without this signal, an input-side audio
    // re-encoder runs through the jump verbatim — its `samples_since_
    // anchor` keeps advancing while the wire-time clock leaps ahead,
    // and output audio falls progressively behind video by the
    // cumulative loop-wrap distance.
    let pcr_jump_signal: Arc<std::sync::atomic::AtomicI64> =
        Arc::new(std::sync::atomic::AtomicI64::new(0));
    if let Some(t) = transcoder.as_mut() {
        t.set_pcr_jump_signal(pcr_jump_signal.clone());
        // NB: `av_sync_pacer` is intentionally NOT wired here. Every
        // catch-up configuration tested in this session (50 ms cap +
        // 100 ms threshold, 32 ms cap + 100 ms threshold, 32 ms cap +
        // 35 ms threshold) made wire audio drift seconds AHEAD of wire
        // PCR within 5 minutes — fires per second far exceeded what
        // `lag = master_elapsed - output_pts_elapsed` would predict.
        // The per-fire output advance (one extra encoded silence
        // frame) is reflected in `samples_since_anchor`, but somehow
        // the resulting wire PES PTS lands further ahead of wire PCR
        // than the silence amount alone explains. Until that
        // accounting gap is closed (probably requires tracing what
        // `pts_rewriter` writes for each emitted PES vs what
        // `samples_since_anchor` says), the safer default is to skip
        // the catch-up here and accept the natural source-asymmetry
        // drift (~3 ms/sec for CNEWS = ~5 minutes within VLC's 1 s
        // tolerance, ~3 minutes within broadcast ±500 ms).
    }
    crate::engine::input_transcode::register_ingress_stats(
        stats.as_ref(),
        &input_id,
        transcoder.as_mut(),
        config.audio_encode.as_ref(),
        config.video_encode.as_ref(),
        &events,
    );
    // Media-player CAN play MPTS files — full post-process chain wires
    // up program_filter / pid_overrides_rewriter / pid_map. Synthetic-TS
    // file types (image / mp4 → TsMuxer) get pid_overrides handled by
    // the muxer; for TS-passthrough files, the post-process rewriter
    // re-PIDs them.
    let passthrough_clock = config.passthrough_clock.unwrap_or(false);
    let av_skew_for_post = stats.as_ref().av_skew_reporter_for_input(&input_id);
    let mut post = crate::engine::input_post_process::InputPostProcess::from_config(
        &crate::engine::input_post_process::InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: config.pid_overrides.as_ref(),
            pid_map: config.pid_map.as_ref(),
            passthrough_clock,
            av_sync_pacer: av_sync_pacer.as_ref(),
            pcr_jump_signal: Some(&pcr_jump_signal),
            av_skew: Some(&av_skew_for_post),
        },
    );
    if let Some(ref _p) = post {
        tracing::info!(
            "Media-player input '{input_id}': ingress post-process active (passthrough_clock={passthrough_clock})"
        );
    }

    let mut seq_num: u16 = 0;
    // One continuity state for the whole playlist's lifetime — carried
    // across loops and file transitions so the on-wire CC + PTS + PCR
    // sequence is continuous regardless of how many times we wrap around
    // or move between sources.
    let mut cont = SpliceContinuity::default();
    // Per-input demux cache — persists across the playlist loop so a looping
    // MP4 is demuxed once, not re-read into RAM every loop. See
    // `mp4_demux::DemuxCache` / `DemuxCacheField`.
    let mut demux_cache = DemuxCacheField::default();

    // Playout telemetry for the whole input's lifetime — see
    // `MediaPlayerStats` doc comment for why this exists independently of
    // `play_source()`'s `Result`. Registered once so the manager can read
    // it via `InputStats.media_player_stats` immediately, even before the
    // first source starts.
    let media_stats = Arc::new(MediaPlayerStats::default());
    stats.set_media_player_stats(&input_id, media_stats.clone());

    // ── Phase 3b controller path (default ON, per-input opt-out) ─────────
    // When the controller is enabled AND the flow runtime wired a command
    // channel, drive playout through the transition state machine so operator
    // `Next` works and natural EOS / loop / Next all commit through one path.
    // Otherwise fall straight through to the legacy sequential loop below,
    // which is left byte-for-byte unchanged (the rollback guarantee).
    if controller::controller_enabled(config.operator_control) {
        if let Some(cmd_rx) = cmd_rx {
            run_controlled(
                &config, &per_input_tx, &stats, &cancel, &events, &flow_id, &input_id,
                &mut seq_num, &mut cont, &mut transcoder, &mut post, &mut demux_cache,
                &media_stats, &pcr_jump_signal, cmd_rx,
            )
            .await;
            return;
        }
    }

    loop {
        let mut order: Vec<usize> = (0..config.sources.len()).collect();
        if config.shuffle && order.len() > 1 {
            shuffle_indices(&mut order);
        }

        // Datagram packetization: bundle at most this many bytes (an integer
        // multiple of 188) into each emitted RtpPacket. Default 7 × 188 =
        // 1316 B (SRT payload size / internet-safe MTU). See
        // `MediaPlayerInputConfig::ts_packets_per_datagram`.
        let bundle_size = (config.ts_packets_per_datagram.max(1) as usize) * TS_PACKET;
        for (pos, &idx) in order.iter().enumerate() {
            if cancel.is_cancelled() {
                return;
            }
            let source = &config.sources[idx];
            let gap_signal = if transcoder.is_some() {
                Some(&pcr_jump_signal)
            } else {
                None
            };
            media_stats.current_source_index.store(idx as u64, Ordering::Relaxed);
            media_stats.state.store(media_player_state::STARTING, Ordering::Relaxed);
            // Phase 5 transport: the next item is the following playlist entry,
            // or (when looping) the head of the next pass — best-effort under
            // shuffle, which re-orders on wrap.
            let next_source = if pos + 1 < order.len() {
                Some(&config.sources[order[pos + 1]])
            } else if config.loop_playback {
                Some(&config.sources[order[0]])
            } else {
                None
            };
            begin_source_telemetry(media_stats.as_ref(), next_source);
            let mut session = PlayerSession {
                seq_num: &mut seq_num,
                per_input_tx: &per_input_tx,
                stats: &stats,
                cancel: &cancel,
                cont: &mut cont,
                transcoder: &mut transcoder,
                pid_overrides: config.pid_overrides.as_ref(),
                post: &mut post,
                splice_gap_signal: gap_signal,
                bundle_size,
                media_stats: &media_stats,
                events: &events,
                flow_id: &flow_id,
                input_id: &input_id,
                demux_cache: &mut demux_cache,
            };
            let result = play_source(source, &config, &mut session).await;
            if let Err(e) = result {
                media_stats.state.store(media_player_state::FAILED, Ordering::Relaxed);
                let error_code = classify_playback_error(&e);
                events.emit_flow_with_details(
                    EventSeverity::Critical,
                    category::FLOW,
                    format!(
                        "Media-player input '{input_id}': source[{idx}] failed: {e}"
                    ),
                    &flow_id,
                    serde_json::json!({
                        "error_code": error_code,
                        "flow_id": flow_id,
                        "input_id": input_id,
                        "source_index": idx,
                        "source_kind": source_kind_str(source),
                        "source_name": source_name_str(source),
                        "error": e.to_string(),
                    }),
                );
                // Don't tight-loop on a bad file — sleep before moving on.
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        }

        if !config.loop_playback {
            media_stats.state.store(media_player_state::EXHAUSTED, Ordering::Relaxed);
            events.emit_flow(
                EventSeverity::Info,
                category::FLOW,
                format!(
                    "Media-player input '{input_id}': playlist exhausted (loop=false), idle until cancelled"
                ),
                &flow_id,
            );
            cancel.cancelled().await;
            return;
        }
    }
}

/// Per-iteration handle the source players use to publish bundles, advance
/// the wire sequence counter, and check cancellation. Borrowed (not owned)
/// so each playlist iteration sees a fresh borrow without re-allocating.
pub(super) struct PlayerSession<'a> {
    pub(super) seq_num: &'a mut u16,
    pub(super) per_input_tx: &'a broadcast::Sender<RtpPacket>,
    pub(super) stats: &'a Arc<FlowStatsAccumulator>,
    pub(super) cancel: &'a CancellationToken,
    /// Continuity carried across loop and playlist boundaries — see
    /// [`SpliceContinuity`].
    pub(super) cont: &'a mut SpliceContinuity,
    /// Optional ingress transcoder. None ⇒ passthrough. Borrowed from
    /// the task-level state so it persists across the playlist.
    pub(super) transcoder: &'a mut Option<crate::engine::input_transcode::InputTranscoder>,
    /// Optional PID overrides — when set, synthesised TS muxers (mp4,
    /// image-slate) use these PIDs for PAT/PMT/PCR/video/audio so the
    /// media-player output PID layout aligns with the rest of the flow.
    pub(super) pid_overrides: Option<&'a crate::config::models::TsPidOverridesMap>,
    /// Optional ingress post-process (program_filter / pid_overrides /
    /// pid_map) applied to every emitted TS chunk before publishing onto
    /// the broadcast channel. None ⇒ no post-processing (zero cost).
    pub(super) post: &'a mut Option<crate::engine::input_post_process::InputPostProcess>,
    /// Per-input PCR-jump signal shared with the `TsAudioReplacer` on
    /// this input's pipeline. `play_ts_file` writes the splice-boundary
    /// video-audio gap here so the audio replacer can silence-pad its
    /// output PTS to match the video timeline at the loop boundary.
    /// `None` in tests or when no transcoder is active.
    pub(super) splice_gap_signal: Option<&'a std::sync::Arc<std::sync::atomic::AtomicI64>>,
    /// Maximum size in bytes of each emitted datagram (`RtpPacket`) — always
    /// an integer multiple of 188 (the TS packet size). Set once from
    /// [`crate::config::models::MediaPlayerInputConfig::ts_packets_per_datagram`]
    /// (default 7 × 188 = 1316 B, the SRT payload size / internet-safe MTU).
    /// The per-source bundling loops flush at this threshold, so it caps every
    /// datagram uniformly across the ts / mp4 / image source kinds: smaller →
    /// more, smaller datagrams for a low-MTU / tunnel path; larger → jumbo
    /// datagrams for a LAN. Tests construct with the module [`BUNDLE_SIZE`].
    pub(super) bundle_size: usize,
    /// Playout telemetry for the whole input's lifetime — see
    /// [`MediaPlayerStats`] doc comment. Shared (not per-source) so progress
    /// counters and pacer-lateness history survive across playlist entries.
    pub(super) media_stats: &'a Arc<MediaPlayerStats>,
    /// Event sender + scope, used for the pacer-lateness latch alarm
    /// (`media_player_pacer_lagging` / `_recovered`) alongside the existing
    /// `media_player_source_*` failure events emitted in `run()`.
    pub(super) events: &'a EventSender,
    pub(super) flow_id: &'a str,
    pub(super) input_id: &'a str,
    /// Per-input demux cache — lets a looping MP4 be demuxed once instead of
    /// re-read into RAM every loop (see `mp4_demux::DemuxCache`). Borrowed
    /// from the task-level state so it persists across the playlist. `()` on
    /// builds without MP4 support (the `mp4` source kind errors there anyway).
    pub(super) demux_cache: &'a mut DemuxCacheField,
}

/// The demux-cache type threaded through [`PlayerSession`]. Real cache with
/// MP4 support compiled in; a zero-sized `()` placeholder otherwise (the
/// `mp4` source kind returns an error without the codecs, so it's never used).
/// A cfg-selected alias keeps `PlayerSession` and its construction sites free
/// of `#[cfg]` noise — both variants implement `Default`, so callers just
/// write `DemuxCacheField::default()`.
#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
pub(super) type DemuxCacheField = mp4_demux::DemuxCache;
#[cfg(not(all(feature = "media-codecs", feature = "fdk-aac")))]
pub(super) type DemuxCacheField = ();

/// 30 ms gap inserted between the previous file's last emitted PTS and the
/// next file's first emitted PTS. ≥ 1 video frame at every common rate
/// (24 / 25 / 30 / 50 / 60 fps), enough to keep some hardware decoders
/// from treating a same-tick splice as a stuck timestamp.
const SPLICE_GUARD_TICKS_90K: u64 = 2_700;

/// The inter-file splice guard expressed in nanoseconds of wall time.
/// Keeps the wall gap between looped files equal to the PTS gap.
pub(super) fn splice_guard_ns() -> u64 {
    // 90 kHz ticks -> ns
    SPLICE_GUARD_TICKS_90K * (1_000_000_000 / 90_000)
}


/// Continuity state threaded through every `play_*_file` call so the wire
/// stream stays smooth across loop and playlist boundaries.
///
/// Without this, every loop or file transition would reset CC to 0, jump
/// PTS/DTS/PCR backward, and leave receivers re-initialising their decoder
/// state on a 1–2 s glitch. With it, the on-wire MPEG-TS is one continuous
/// stream from the receiver's perspective, regardless of how many files
/// the playlist contains or how many times any of them loops.
///
/// Allocation policy: the only allocator activity on the data path is the
/// first-seen-PID insert into [`Self::last_cc`] (≤ ~10 entries across a
/// playlist's lifetime). Per-packet rewriting is in-place on the existing
/// 188-byte buffer.
#[derive(Default)]
pub(super) struct SpliceContinuity {
    /// Per-PID continuity counter as last sent on the wire. A fresh
    /// `play_*_file` call rewrites every emitted packet's CC so the
    /// sequence picks up immediately after the previous file's, instead
    /// of resetting to whatever the source happened to start at.
    last_cc: HashMap<u16, u8>,

    /// 90 kHz PTS value the next file should emit on its first PES.
    /// Initially 0 (cold start); after each file closes, set to
    /// `last_emitted_output_pts_90k + SPLICE_GUARD_TICKS_90K`.
    next_target_output_pts_90k: u64,

    /// Last 90 kHz PTS emitted on the wire by the most recent file.
    /// Drives the next file's [`Self::next_target_output_pts_90k`].
    last_emitted_output_pts_90k: u64,

    /// Monotonic wall-clock deadline (ns) of the last bundle the previous
    /// file scheduled, or 0 on a cold start.
    ///
    /// Lets the next file place its epoch so the WALL gap between loops
    /// equals the PTS gap. Without it each loop re-anchored to
    /// `now + PREROLL`, so wall advanced by `file_span + preroll +
    /// teardown` while PTS advanced by only `file_span +
    /// SPLICE_GUARD_TICKS_90K` (30 ms) — the emitted timeline ran
    /// systematically SHORT of real time every loop, draining the
    /// receiver's buffer monotonically. On a 24/7 slate that is tens of
    /// seconds a day on a stream that looks clean over any short window.
    last_scheduled_deadline_ns: u64,

    /// Identity of the previous file. Distinguishes a same-file loop
    /// (no real boundary, no need to flag) from a playlist transition
    /// (different file → set discontinuity_indicator).
    last_source_id: Option<String>,

    /// Codec/PID layout of the previous file. Used by MP4 + image paths
    /// to decide whether to bump the synthesised PMT `version_number`
    /// and whether to flag the splice as a real discontinuity.
    last_layout: Option<StreamLayout>,

    /// Monotonic synthesised-PMT `version_number`, mod 32. Bumped only
    /// on a real layout change — gratuitous bumps cause receiver decoder
    /// re-init (audible audio dropouts on Apple decoders specifically).
    pmt_version: u8,

    /// One-shot flag: set at file open when the upcoming first packet
    /// should carry `discontinuity_indicator=1`. Consumed (cleared) by
    /// the first emitted packet that has space for the flag.
    pending_discontinuity: bool,

    /// True after the very first file has played. Used to gate
    /// [`Self::pending_discontinuity`] — the cold-start file has no
    /// preceding stream to be discontinuous from.
    has_played_at_least_one_file: bool,

    /// Converged cumulative bitrate from the previous `play_ts_file` /
    /// `play_demuxed` call. Carried across loops so the OS-thread pacer
    /// starts at the known-good rate instead of the head-scan estimate
    /// (which overshoots by 10-20% on the first second of every loop,
    /// accumulating wire_tx excess that never drains).
    ///
    /// **Only valid for a loop of the same source** — see
    /// [`Self::last_converged_bitrate_source`]. Read this field directly
    /// only through a check against that source id; a bare read here would
    /// reintroduce issue #68 (a playlist transition to a different file
    /// with a different bitrate starting its pacer at the previous file's
    /// rate).
    pub(super) last_converged_bitrate_bps: Option<u64>,
    /// Source identity (matches [`source_name_str`]) that produced
    /// [`Self::last_converged_bitrate_bps`]. `SpliceContinuity` persists
    /// for the whole playlist's lifetime, so without this scope check the
    /// carried rate leaks across a transition to a genuinely different
    /// file — a materially different bitrate, frame rate, or GOP structure
    /// then starts the new file's pacer at the wrong rate, producing pacer
    /// queue growth / excess lateness / bursty delivery right at the
    /// playlist boundary (issue #68). `None` until the first file closes.
    pub(super) last_converged_bitrate_source: Option<String>,
}

impl SpliceContinuity {
    /// Called by `play_source` before dispatching to the per-format
    /// player. Compares the new source against the previous one and
    /// arms `pending_discontinuity` if the layout (or the source itself)
    /// has changed.
    pub(super) fn open_file(&mut self, source_id: &str) {
        let is_transition = self
            .last_source_id
            .as_deref()
            .is_some_and(|prev| prev != source_id);
        // First-ever file: never flag — there's no preceding stream to be
        // discontinuous from. Self-loop (same source_id): no flag — our
        // CC + PTS rewriting keeps the splice clean. Real transition: flag.
        //
        // (Reverted from 2189c62's "always flag" behaviour: that change
        // armed DI=1 every loop iteration and, paired with the last-PCR
        // close_file anchor below, introduced a small `SPLICE_GUARD`-sized
        // PCR jump every ~170 s loop. VLC's audio decoder did not
        // re-zero its buffer fill on every DI=1, so the small jumps
        // compounded into perceptible A/V drift over 5+ minutes. The
        // pre-2189c62 behaviour — anchor on max-emitted-PTS so PCR
        // continues from where the file naturally ended — kept audio in
        // sync across loops on the Sky Witness 1080i25 testbed.)
        if self.has_played_at_least_one_file && is_transition {
            self.pending_discontinuity = true;
        }
        self.last_source_id = Some(source_id.to_string());
    }

    /// Called by each per-format player at the end of a file to update
    /// the target PTS for the *next* file's first emit. `last_pts_90k`
    /// is the high-water-mark 90 kHz PTS the caller wants the next file's
    /// timeline to continue from.
    ///
    /// **TS-passthrough callers should prefer `max_audio_pts` over
    /// `max_video_pts`.** Real broadcast captures end with the last video
    /// PES PTS ~1 s past the last audio PES PTS (mux buffering — video is
    /// muxed ahead of decode time). Anchoring on `max_video_pts` shoves
    /// the next loop's first PCR forward by that delta, leaving a ~1 s
    /// audio gap on the wire every loop (receiver buffer underruns →
    /// audible silence per loop). Anchoring on `max_audio_pts` keeps the
    /// audio timeline tight: the next loop's first audio PES starts
    /// `SPLICE_GUARD + audio_mux_delay` after the previous loop's last
    /// audio sample (~200 ms total) instead of ~1 s.
    ///
    /// Synthesised paths (`play_mp4`, `play_image_file`) where audio and
    /// video are muxed in lockstep have a single max-PTS and pass it
    /// directly.
    /// Wall-clock deadline (ns) of the last bundle the previous file
    /// scheduled, or 0 if none (cold start, or a byte-rate-paced entry).
    pub(super) fn last_scheduled_deadline_ns(&self) -> u64 {
        self.last_scheduled_deadline_ns
    }

    pub(super) fn close_file(&mut self, last_pts_90k: u64) {
        self.close_file_with_deadline(last_pts_90k, 0);
    }

    /// As [`Self::close_file`], but also records the wall-clock deadline
    /// of the last bundle scheduled, so the next file can continue the
    /// wall timeline instead of re-anchoring to `now`. Pass 0 when the
    /// caller does not pace against absolute deadlines (the byte-rate
    /// TS path), which leaves the next file to cold-start its epoch.
    pub(super) fn close_file_with_deadline(
        &mut self,
        last_pts_90k: u64,
        last_deadline_ns: u64,
    ) {
        self.last_scheduled_deadline_ns = last_deadline_ns;
        self.last_emitted_output_pts_90k = last_pts_90k;
        // 33-bit wrap: PTS is 33 bits, target rolls over with it.
        self.next_target_output_pts_90k = last_pts_90k
            .wrapping_add(SPLICE_GUARD_TICKS_90K)
            & 0x1_FFFF_FFFF;
        self.has_played_at_least_one_file = true;
    }

    /// Update [`Self::last_layout`]; if it changed, bump `pmt_version`.
    /// Returns true if the layout changed (i.e. this is a real layout
    /// boundary, not just a same-file loop).
    pub(super) fn update_layout(&mut self, new_layout: StreamLayout) -> bool {
        let changed = self.last_layout.as_ref() != Some(&new_layout);
        if changed {
            self.pmt_version = self.pmt_version.wrapping_add(1) & 0x1F;
            self.last_layout = Some(new_layout);
        }
        changed
    }

    /// The carried pacer bitrate, scoped to a loop of `source_id` — `None`
    /// on a genuine transition to a different source, even though
    /// [`Self::last_converged_bitrate_bps`] itself is still populated from
    /// whatever source played last. See the field doc comment and issue
    /// #68 for why this scope check exists.
    pub(super) fn carried_bitrate_for(&self, source_id: &str) -> Option<u64> {
        if self.last_converged_bitrate_source.as_deref() == Some(source_id) {
            self.last_converged_bitrate_bps
        } else {
            None
        }
    }

    /// Record the converged pacer bitrate for `source_id`, called at the
    /// end of each per-format player. Pairs with
    /// [`Self::carried_bitrate_for`] — always write both fields together so
    /// they can never disagree about which source the rate belongs to.
    pub(super) fn set_converged_bitrate(&mut self, source_id: &str, bps: u64) {
        self.last_converged_bitrate_bps = Some(bps);
        self.last_converged_bitrate_source = Some(source_id.to_string());
    }
}

/// Stream-layout fingerprint used by the MP4 + image paths to decide
/// whether a file boundary is a real layout change (codec / PID / audio
/// added or removed). Match → no PMT version bump. Mismatch → bump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StreamLayout {
    pub video_pid: Option<u16>,
    pub video_stream_type: Option<u8>,
    pub audio_pid: Option<u16>,
    pub audio_stream_type: Option<u8>,
}

/// Map a playback failure onto a stable `error_code` string carried in the
/// `Media-player source failed` Critical event's `details`. The manager UI
/// keys off these codes (rather than the free-form message) to attribute
/// failures and decide whether to highlight the file in the library picker.
///
/// Emit `media_player_transition_started` (Phase 3b) — a boundary was armed
/// and is about to commit.
fn emit_transition_started(
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transition_id: &str,
    generation: u64,
    from_index: usize,
    to_index: usize,
    trigger: &str,
) {
    events.emit_flow_with_details(
        EventSeverity::Info,
        category::FLOW,
        format!("Media-player input '{input_id}': transition {from_index}→{to_index} ({trigger})"),
        flow_id,
        serde_json::json!({
            "error_code": "media_player_transition_started",
            "flow_id": flow_id,
            "input_id": input_id,
            "transition_id": transition_id,
            "generation": generation,
            "from_index": from_index,
            "to_index": to_index,
            "trigger": trigger,
        }),
    );
}

/// Emit `media_player_transition_completed` (Phase 3b) — the new source's
/// generation is now on air.
fn emit_transition_completed(
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    cr: &transition::CommitResult,
) {
    events.emit_flow_with_details(
        EventSeverity::Info,
        category::FLOW,
        format!(
            "Media-player input '{input_id}': transition complete {}→{} (gen {})",
            cr.previous_index, cr.new_index, cr.new_generation
        ),
        flow_id,
        serde_json::json!({
            "error_code": "media_player_transition_completed",
            "flow_id": flow_id,
            "input_id": input_id,
            "transition_id": cr.transition_id,
            "generation": cr.new_generation,
            "previous_index": cr.previous_index,
            "current_index": cr.new_index,
        }),
    );
}

/// Emit `media_player_next_prepare_failed` (Phase 3b) — an operator `Next`
/// target could not be prepared, so the current source stays on air.
fn emit_next_prepare_failed(events: &EventSender, flow_id: &str, input_id: &str, target_index: usize) {
    events.emit_flow_with_details(
        EventSeverity::Warning,
        category::FLOW,
        format!("Media-player input '{input_id}': Next target[{target_index}] not ready — keeping current source"),
        flow_id,
        serde_json::json!({
            "error_code": "media_player_next_prepare_failed",
            "flow_id": flow_id,
            "input_id": input_id,
            "target_index": target_index,
        }),
    );
}

/// Walks `anyhow`'s error chain looking for an `io::Error` first (file
/// open / read failures preserve `io::Error` via `.with_context`), then
/// falls back to message-pattern matching for parse-level errors that
/// originate as plain `anyhow!` macros.
pub(super) fn classify_playback_error(err: &anyhow::Error) -> &'static str {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            return match io_err.kind() {
                std::io::ErrorKind::NotFound => "media_player_source_missing",
                std::io::ErrorKind::PermissionDenied => "media_player_source_missing",
                std::io::ErrorKind::InvalidData => "media_player_source_unsupported",
                _ => "media_player_source_render_failed",
            };
        }
    }
    let msg = err.to_string();
    let msg_lc = msg.to_lowercase();
    if msg_lc.contains("no sync byte")
        || msg_lc.contains("not mpeg-ts")
        || msg_lc.contains("severely corrupt")
        || msg_lc.contains("fragmented mp4")
    {
        return "media_player_source_unsupported";
    }
    if msg_lc.contains("requires the 'media-codecs'") {
        return "media_player_source_codec_unsupported";
    }
    "media_player_source_failed"
}

fn source_kind_str(source: &MediaPlayerSource) -> &'static str {
    match source {
        MediaPlayerSource::Ts { .. } => "ts",
        MediaPlayerSource::Mp4 { .. } => "mp4",
        MediaPlayerSource::Image { .. } => "image",
    }
}

fn source_name_str(source: &MediaPlayerSource) -> &str {
    match source {
        MediaPlayerSource::Ts { name, .. } => name,
        MediaPlayerSource::Mp4 { name } => name,
        MediaPlayerSource::Image { name, .. } => name,
    }
}

/// Phase 3b controller loop. Replaces the legacy sequential playlist loop
/// when the controller is enabled, driving every boundary — natural EOS,
/// loop wrap, and operator `Next` — through the transition state machine.
///
/// Structure: for the active source, run `play_source` on a per-source child
/// cancel token while `select!`ing on the operator-command channel. A `Next`
/// that the state machine accepts and whose target is openable cancels the
/// child token (cutting the current source), then commits; natural completion
/// arms and commits the next boundary. The transition machine owns the
/// generation counter and the double-click / hold semantics; this loop only
/// performs the I/O each decision implies.
#[allow(clippy::too_many_arguments)]
async fn run_controlled(
    config: &MediaPlayerInputConfig,
    per_input_tx: &broadcast::Sender<RtpPacket>,
    stats: &Arc<FlowStatsAccumulator>,
    cancel: &CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    seq_num: &mut u16,
    cont: &mut SpliceContinuity,
    transcoder: &mut Option<crate::engine::input_transcode::InputTranscoder>,
    post: &mut Option<crate::engine::input_post_process::InputPostProcess>,
    demux_cache: &mut DemuxCacheField,
    media_stats: &Arc<MediaPlayerStats>,
    pcr_jump_signal: &Arc<std::sync::atomic::AtomicI64>,
    mut cmd_rx: tokio::sync::mpsc::Receiver<controller::MediaPlayerCommand>,
) {
    use transition::{NextOutcome, NextRequest, TransitionMachine, TransitionTrigger};

    let bundle_size = (config.ts_packets_per_datagram.max(1) as usize) * TS_PACKET;
    // Draw the first permutation here; each loop wrap re-permutes at the commit
    // that lands back on index 0 (see `reshuffle_on_wrap` below). `order` holds
    // indices INTO `config.sources`, while the state machine's indices are into
    // `order`, so permuting it never disturbs the machine's stable index space.
    let mut order: Vec<usize> = (0..config.sources.len()).collect();
    if config.shuffle && order.len() > 1 {
        shuffle_indices(&mut order);
    }
    if order.is_empty() {
        cancel.cancelled().await;
        return;
    }

    let mut sm = TransitionMachine::new(order.len(), config.loop_playback);
    let mut tid_counter: u64 = 0;
    sm.begin_playing();
    events.emit_flow(
        EventSeverity::Info,
        category::FLOW,
        format!("Media-player input '{input_id}': controller path active (operator Next enabled)"),
        flow_id,
    );

    loop {
        if cancel.is_cancelled() {
            return;
        }
        let active = sm.active_index();
        let cfg_idx = order[active];
        let source = &config.sources[cfg_idx];
        let gap_signal = if transcoder.is_some() { Some(&*pcr_jump_signal) } else { None };
        media_stats.current_source_index.store(cfg_idx as u64, Ordering::Relaxed);
        media_stats.generation.store(sm.generation(), Ordering::Relaxed);
        media_stats.state.store(media_player_state::STARTING, Ordering::Relaxed);
        // Phase 5 transport: the next item the state machine would advance to
        // (next in order, or the head when looping).
        let next_source = {
            let next_active = if active + 1 < order.len() {
                Some(active + 1)
            } else if config.loop_playback {
                Some(0)
            } else {
                None
            };
            next_active.map(|na| &config.sources[order[na]])
        };
        begin_source_telemetry(media_stats.as_ref(), next_source);

        // Per-source child token: cancelling it cuts only this source (for an
        // operator Next); the input-wide `cancel` still stops everything.
        let source_cancel = cancel.child_token();
        let mut session = PlayerSession {
            seq_num: &mut *seq_num,
            per_input_tx,
            stats,
            cancel: &source_cancel,
            cont: &mut *cont,
            transcoder: &mut *transcoder,
            pid_overrides: config.pid_overrides.as_ref(),
            post: &mut *post,
            splice_gap_signal: gap_signal,
            bundle_size,
            media_stats,
            events,
            flow_id,
            input_id,
            demux_cache: &mut *demux_cache,
        };
        let play_fut = play_source(source, config, &mut session);
        tokio::pin!(play_fut);

        let mut was_cut = false;
        let mut play_result: Result<()> = Ok(());
        let mut cmd_closed = false;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => { return; }
                res = &mut play_fut => { play_result = res; break; }
                maybe_cmd = cmd_rx.recv(), if !cmd_closed => {
                    let cmd = match maybe_cmd {
                        Some(c) => c,
                        None => { cmd_closed = true; continue; }
                    };
                    match cmd {
                        controller::MediaPlayerCommand::Next { expected_generation, request_id, reply } => {
                            tid_counter += 1;
                            let tid_val = tid_counter;
                            let outcome = sm.request_next(
                                &NextRequest { expected_generation, request_id },
                                || format!("mp-{tid_val}"),
                            );
                            // Pull out target + transition id without holding a
                            // borrow across the `reply.send(outcome)` move.
                            let accepted = match &outcome {
                                NextOutcome::Accepted { target_index, transition_id, .. } => {
                                    Some((*target_index, transition_id.clone()))
                                }
                                _ => None,
                            };
                            match accepted {
                                Some((target, tid_str)) => {
                                    if controller::source_openable(&config.sources[order[target]]) {
                                        let cur_gen = sm.generation();
                                        let _ = reply.send(outcome);
                                        emit_transition_started(events, flow_id, input_id, &tid_str, cur_gen, active, target, "operator_next");
                                        source_cancel.cancel();
                                        was_cut = true;
                                        break;
                                    } else {
                                        sm.cancel_armed();
                                        let _ = reply.send(NextOutcome::Rejected { code: "media_player_next_not_ready" });
                                        emit_next_prepare_failed(events, flow_id, input_id, target);
                                    }
                                }
                                None => {
                                    let _ = reply.send(outcome);
                                }
                            }
                        }
                    }
                }
            }
        }

        if was_cut {
            // Let the cancelled source finish returning (releases file/codec
            // state) before committing the boundary.
            let _ = play_fut.as_mut().await;
            let cr = sm.commit();
            media_stats.generation.store(cr.new_generation, Ordering::Relaxed);
            reshuffle_on_wrap(&config, &mut order, &cr);
            emit_transition_completed(events, flow_id, input_id, &cr);
            sm.settle_playing();
            continue;
        }

        // Natural completion (or error).
        if let Err(e) = &play_result {
            media_stats.state.store(media_player_state::FAILED, Ordering::Relaxed);
            let error_code = classify_playback_error(e);
            events.emit_flow_with_details(
                EventSeverity::Critical,
                category::FLOW,
                format!("Media-player input '{input_id}': source[{cfg_idx}] failed: {e}"),
                flow_id,
                serde_json::json!({
                    "error_code": error_code,
                    "flow_id": flow_id,
                    "input_id": input_id,
                    "source_index": cfg_idx,
                    "source_kind": source_kind_str(source),
                    "source_name": source_name_str(source),
                    "error": e.to_string(),
                }),
            );
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }

        let trigger = if active + 1 >= order.len() {
            TransitionTrigger::Loop
        } else {
            TransitionTrigger::NaturalEos
        };
        tid_counter += 1;
        let tid_val = tid_counter;
        match sm.arm_natural(trigger, || format!("mp-{tid_val}")) {
            Some(_target) => {
                // Best-effort readiness: if the next file is missing, play_source
                // will error and the loop's error handler sleeps 2 s, matching
                // the legacy loop's behaviour.
                sm.set_next_ready(true);
                if sm.on_current_exhausted() {
                    let cr = sm.commit();
                    media_stats.generation.store(cr.new_generation, Ordering::Relaxed);
                    reshuffle_on_wrap(&config, &mut order, &cr);
                    sm.settle_playing();
                }
            }
            None => {
                media_stats.state.store(media_player_state::EXHAUSTED, Ordering::Relaxed);
                events.emit_flow(
                    EventSeverity::Info,
                    category::FLOW,
                    format!("Media-player input '{input_id}': playlist exhausted (loop=false), idle until cancelled"),
                    flow_id,
                );
                // Keep answering `Next` while parked. `cmd_rx` is owned by this
                // task and the only `recv()` lives in the per-source select!
                // that has already exited, so simply awaiting cancellation here
                // strands every subsequent Next: `cmd_tx.send()` still succeeds
                // (nothing removes the entry from `media_player_command_txs`),
                // the buffered message keeps its oneshot::Sender alive, and the
                // dispatcher's unbounded `reply_rx.await` never resolves — no
                // command_ack, and one parked handler task per press holding
                // Arc clones until the flow stops.
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        cmd = cmd_rx.recv() => match cmd {
                            Some(controller::MediaPlayerCommand::Next { reply, .. }) => {
                                let _ = reply.send(NextOutcome::Rejected {
                                    code: transition::reject_code::PLAYLIST_EXHAUSTED,
                                });
                            }
                            None => {
                                cancel.cancelled().await;
                                return;
                            }
                        },
                    }
                }
            }
        }
    }
}

/// Phase 5 transport telemetry, called as each source begins. Records the
/// wall-clock start (the snapshot derives elapsed-in-source from it), clears the
/// per-source known duration (an MP4 sets it once opened; TS/image leave it
/// unknown), and evaluates whether the next playlist item can be opened right
/// now so the UI can flag a `Next` that would cut to dead air. Cheap: one clock
/// read plus one stat of the next file, once per source transition.
fn begin_source_telemetry(media_stats: &MediaPlayerStats, next_source: Option<&MediaPlayerSource>) {
    let now_ms = crate::util::time::now_us() / 1000;
    media_stats.current_source_started_ms.store(now_ms, Ordering::Relaxed);
    media_stats.current_source_duration_ms.store(0, Ordering::Relaxed);
    // Reset video-presence to "unknown" so a previous source's value never
    // lingers; the per-format player stamps it once it knows (MP4 sets it
    // accurately from the demux, the TS path leaves it unknown).
    media_stats.current_source_has_video.store(0, Ordering::Relaxed);
    let ready = match next_source {
        Some(s) if controller::source_openable(s) => 1,
        Some(_) => 2,
        None => 0,
    };
    media_stats.next_source_ready.store(ready, Ordering::Relaxed);
}

async fn play_source(
    source: &MediaPlayerSource,
    cfg: &MediaPlayerInputConfig,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    // Source identity used by SpliceContinuity to distinguish a same-file
    // loop (no flag) from a real playlist transition (flag).
    let source_id = source_name_str(source).to_string();
    session.cont.open_file(&source_id);

    match source {
        MediaPlayerSource::Ts { name, program_number } => {
            let path = resolve_media_path(name)?;
            play_ts_file(&path, cfg.paced_bitrate_bps, *program_number, session).await
        }
        MediaPlayerSource::Mp4 { name } => {
            let path = resolve_media_path(name)?;
            play_mp4(&path, cfg.paced_bitrate_bps, session).await
        }
        MediaPlayerSource::Image {
            name,
            fps,
            bitrate_kbps,
            audio_silence,
            duration_secs,
        } => {
            let path = resolve_media_path(name)?;
            play_image(&path, *fps, *bitrate_kbps, *audio_silence, *duration_secs, session).await
        }
    }
}

// ── TS file player ───────────────────────────────────────────────────────

/// Play one MPEG-TS file end-to-end. Pacing prefers embedded PCR; if the
/// file has no PCR-bearing packets in the first second of reading, falls
/// back to `paced_bitrate_bps` (or [`DEFAULT_FALLBACK_BITRATE_BPS`] when
/// the operator left that unset).
///
/// When `program_number` is `Some(n)`, every packet is run through a
/// [`TsProgramFilter`] before the per-packet rewriters and pacing — only
/// the target program's PAT (rewritten to single-program form), PMT, PCR,
/// and ES PIDs reach the bundle. Other-program packets are dropped before
/// pacing so the wire pace tracks the target program's PCR cadence rather
/// than the cross-program average. When `None`, the file passes through
/// unchanged (full MPTS or SPTS, identical to legacy behaviour).
async fn play_ts_file(
    path: &Path,
    paced_bitrate_bps: Option<u64>,
    program_number: Option<u16>,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    session.media_stats.reader_mode.store(
        crate::stats::collector::media_player_reader_mode::TS,
        Ordering::Relaxed,
    );
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;

    // Auto-detect the on-disk TS stride (188 / 192 / 204) from the head.
    // 188 = canonical MPEG-TS. 192 = M2TS / AVCHD / Blu-ray BDAV (4-byte
    // timestamp prefix per packet). 204 = DVB Reed-Solomon parity suffix
    // (broadcast captures from professional DVB tuners). Anything else
    // falls back to 188 + the existing resync loop.
    // Single 512 KB head probe used for both stride detection AND
    // bitrate pre-scan. Sized to cover ~200 ms of source content even
    // at 10 Mbps, which is comfortably above the worst-case 40-ms
    // inter-PCR cadence — and on a multi-program MPTS the per-PID
    // PCR cadence still fits because the file's first PCR sits within
    // the first ~10 KB of any well-formed broadcast capture.
    //
    // Pre-scanning eliminates the per-loop slow-start that otherwise
    // leaves the pacer below source rate for several seconds every
    // time a media-player file restarts — the dominant source of
    // PCR-vs-wallclock drift across loops on short files (Ten.ts loops
    // every 63 s, so a 50-s slow-start covers most of the loop).
    // When a program filter is active, the filter needs PAT + PMT
    // discovery before it starts producing output. On a typical DVB-T
    // MPTS the PAT appears within the first ~100 KB, and the PMT
    // shortly after. With a 512 KB head, only ~50 KB of filtered
    // content survives — too little for scan_head_bitrate's 100 ms
    // minimum PCR span. 2 MB gives ~500 KB of filtered content on a
    // 4-program MPTS (660 ms at 5.5 Mbps), comfortably above the
    // threshold. Still fast on SSD (<5 ms).
    let head_size = if program_number.is_some() { 2 * 1024 * 1024 } else { 512 * 1024 };
    let mut head = vec![0u8; head_size];
    let head_len = file
        .read(&mut head)
        .await
        .with_context(|| format!("probe head of {}", path.display()))?;
    head.truncate(head_len);
    let stride = detect_ts_packet_size_in_buf(&head);
    let head_bitrate_raw = scan_head_bitrate(&head, stride);
    let head_bitrate = if let Some(pn) = program_number {
        // MPTS: scan_head_bitrate on the raw file measures the total mux
        // rate across all programs, not the target program's rate. That
        // overestimate causes the OS-thread pacer to emit 2-3× too fast
        // during the first second of every file loop, flooding the wire_tx
        // channel with excess packets that never drain (input rate equals
        // output rate after the cumulative estimator converges, so the
        // per-loop surplus accumulates indefinitely). Fix: filter the head
        // to the target program before scanning so the bitrate matches the
        // post-filter byte rate the wire emitter will actually pace.
        let mut filter = super::ts_program_filter::TsProgramFilter::new(pn);
        let mut filtered = Vec::with_capacity(head.len());
        let mut scratch = Vec::with_capacity(TS_PACKET);
        let mut pos = 0usize;
        while pos + TS_PACKET <= head.len() {
            if head[pos] == SYNC_BYTE {
                scratch.clear();
                filter.filter_into(&head[pos..pos + TS_PACKET], &mut scratch);
                filtered.extend_from_slice(&scratch);
                pos += stride;
            } else {
                pos += 1;
            }
        }
        let filtered_br = scan_head_bitrate(&filtered, TS_PACKET);
        tracing::info!(
            "media-player head bitrate: raw={} bps, filtered(prog {})={} bps, filtered_bytes={}/{}",
            head_bitrate_raw.unwrap_or(0),
            pn,
            filtered_br.unwrap_or(0),
            filtered.len(),
            head.len(),
        );
        filtered_br
    } else {
        head_bitrate_raw
    };
    // Phase 5 transport: estimate the TS file's playback duration for the
    // progress head from its total size and the head-probed *whole-mux* rate
    // (not the per-program filtered one — the file plays for its full wall
    // duration regardless of program down-selection). VBR makes this
    // approximate; it's a progress hint, not a seek index. `None`/0 leaves the
    // duration unknown → the UI shows an indeterminate "live" head.
    if let (Some(total_bps), Ok(meta)) = (head_bitrate_raw, tokio::fs::metadata(path).await) {
        if total_bps > 0 && meta.len() > 0 {
            let dur_ms = (meta.len() as u128 * 8 * 1000 / total_bps as u128) as u64;
            session
                .media_stats
                .current_source_duration_ms
                .store(dur_ms, Ordering::Relaxed);
        }
    }
    drop(head);
    file.seek(std::io::SeekFrom::Start(0))
        .await
        .with_context(|| format!("rewind {} after head probe", path.display()))?;
    let mut reader = BufReader::new(file);

    let trailer_len = stride - TS_PACKET;
    let mut trailer_buf = [0u8; MAX_TS_TRAILER];

    let mut bundle = BytesMut::with_capacity(session.bundle_size);
    let mut packet = [0u8; TS_PACKET];

    // ── OS-thread pacer (CLOCK_TAI + clock_nanosleep on SCHED_FIFO) ──
    //
    // History: a per-bundle `tokio::time::sleep_until(target)` (~1.07 ms
    // bundle period at 5 Mbps) was unreliable below ~1 ms — slip of 100s
    // of µs per sleep accumulated through PCR re-anchors to -19 000 ppm
    // drift over 4 min on Sky Witness 1080i25 (2026-05-11). A previous
    // "skip short sleeps and burst" attempt overflowed wire_emit's UDP-
    // side mpsc with 5-8 % drops and 1 s of latency.
    //
    // The fix mirrors `engine::wire_emit`: a dedicated SCHED_FIFO std
    // thread sleeps on absolute CLOCK_TAI deadlines via
    // `clock_nanosleep(TIMER_ABSTIME)`. Absolute targets mean per-sleep
    // slip does not accumulate — each bundle fires within ~50 µs of its
    // computed deadline regardless of the previous bundle's actual fire
    // time. Pacing math lives on the thread (single `iter_start` anchor
    // for the file; bitrate refined from cumulative inter-PCR observations
    // sent across a control channel), so the async producer never re-
    // anchors against its own scheduler latency.
    // Priority: operator override > previous loop's converged rate >
    // head scan > default fallback. The carried rate eliminates the
    // 50-second convergence transient that otherwise overshoots on every
    // loop restart and accumulates wire_tx excess.
    //
    // The carried rate is scoped to a loop of *this exact source* — see
    // `SpliceContinuity::carried_bitrate_for`. `SpliceContinuity` persists
    // for the whole playlist, so a bare read of `last_converged_bitrate_bps`
    // would leak the previous (possibly very different) file's rate across
    // a playlist transition, starting the new file's pacer at the wrong
    // rate right at the boundary (issue #68).
    let source_id = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    let carried = session.cont.carried_bitrate_for(source_id);
    let pacer_initial_bitrate = paced_bitrate_bps
        .or(carried)
        .or(head_bitrate)
        .unwrap_or(DEFAULT_FALLBACK_BITRATE_BPS);
    tracing::info!(
        "media-player pacer rate selection: carried={:?}, head={:?}, chosen={} bps",
        carried, head_bitrate, pacer_initial_bitrate,
    );
    let (pacer_tx, pacer_rx) =
        std::sync::mpsc::sync_channel::<PacerMsg>(PACER_QUEUE_CAP);
    let broadcast_for_pacer = session.per_input_tx.clone();
    let cancel_for_pacer = session.cancel.clone();
    let media_stats_for_pacer = session.media_stats.clone();
    let pacer_thread_name = format!("media-pacer-{source_id}");
    let pacer_thread = std::thread::Builder::new()
        .name(pacer_thread_name.clone())
        .spawn(move || {
            run_paced_emitter(
                pacer_thread_name,
                pacer_rx,
                broadcast_for_pacer,
                pacer_initial_bitrate,
                cancel_for_pacer,
                media_stats_for_pacer,
            );
        })
        .with_context(|| "spawn media-player pacer thread")?;

    // Windowed bitrate observation. A 2-second sliding window tracks
    // the instantaneous PCR-implied rate, re-anchoring every 2 seconds
    // so VBR content doesn't bias the estimate (the old cumulative
    // estimator measured from file-start, producing a running average
    // that overshot the instantaneous rate during the low-bitrate second
    // half of VBR files and caused the wire_tx channel to fill).
    let mut window_pcr_27mhz: Option<u64> = None;
    let mut window_byte_pos: u64 = 0;
    let mut current_bitrate_bps: u64 = pacer_initial_bitrate;
    // ── PCR-anchored emission deadlines (TS) ───────────────────────────
    // The byte-rate mode alone paces from an *estimated* rate. On VBR
    // content that estimate is wrong by construction, and an overestimate
    // makes the pacer emit faster than real time. Worse, the converged
    // rate is carried into the next loop (`carried_bitrate_for`), so on a
    // looping file the overshoot compounds every iteration.
    //
    // Hardware-diagnosed on bilby-pi (rk3568) with a VBR 6.6-10 Mbps
    // source: frame loss ran away (4 -> 1413 over 2 min) with 62
    // `send_packet` failures as the flood overran the RKMPP task queue
    // (kernel `force_dequeue`), which then tripped a permanent HW->CPU
    // demote. Pinning `paced_bitrate_bps` fixed it (62 -> 4 failures,
    // loss flat) — which is what identified the rate as the variable.
    //
    // Fix: anchor each bundle's absolute deadline on the PCR timeline the
    // source already carries, re-anchoring on every PCR. The estimated
    // rate is then only used to interpolate *within* one PCR interval, so
    // an overestimate can run ahead by at most that interval instead of
    // compounding without bound. This is the same deadline mode the MP4
    // path uses (`PacerMsg::target_ns`), including its
    // `MIN_BUNDLE_SPACING_NS` clamp, and that path has never exhibited
    // this failure.
    let mut pcr_epoch_27mhz: Option<u64> = None;
    let mut pacer_epoch_ns: Option<u64> = None;
    let mut last_pcr_target_ns: Option<u64> = None;
    let mut last_anchor_pcr_27mhz: Option<u64> = None;
    let mut bytes_since_pcr: u64 = 0;
    // Rollback lever for the whole PCR-anchored scheme, matching the two
    // levers `input_media_player::controller` already carries. The failure
    // modes this mode can have are asset- and host-dependent (a spliced
    // asset, a stalling disk, a clock step), which is exactly the class
    // that shows up on one customer node and nowhere in the lab — and the
    // only other remedy is rolling the whole release back, which also
    // takes away the black-panel fix this exists to deliver.
    let pcr_deadlines_enabled = pcr_deadline_pacing_enabled();
    let mut bytes_emitted: u64 = 0;
    // Source bytes the transcoder buffered without emitting yet. Credited to
    // the next emitted message so the pacer's content clock stays conserved
    // even though the transcoder emits fewer messages than source bundles.
    let mut pending_src_bytes: u64 = 0;
    // MPTS guard: lock onto the first PCR-bearing PID for every
    // pacing/anchoring computation. An MPTS file has one independent
    // 27 MHz clock per program; mixing PCRs from multiple PIDs into a
    // single anchor produces apparent multi-second discontinuities at
    // every PID switch and collapses the bitrate estimator (observed
    // values from PIDs whose clocks are offset by seconds blow well past
    // the 100 kbps–10 Gbps sanity bounds or oscillate the estimate
    // catastrophically). Mirrors `engine::wire_emit`'s `anchor_pid`
    // behaviour at the egress. `None` until the first PCR is seen;
    // every PCR-carrying packet on any other PID is then transparent
    // to the bitrate / splice-offset / max-PCR trackers (its PCR bytes
    // still pass through `rewrite_pcr_in_place` since `splice_offset_27m`
    // is a flat shift applied uniformly).
    let mut anchor_pcr_pid: Option<u16> = None;
    // Optional MPTS → SPTS down-select. Pre-allocated 188-byte scratch so
    // the per-packet path stays allocation-free (the filter writes 0 or
    // 188 bytes per input packet).
    let mut program_filter = program_number.map(super::ts_program_filter::TsProgramFilter::new);
    let mut filter_scratch: Vec<u8> = Vec::with_capacity(TS_PACKET);

    // ── Smooth-splice state ─────────────────────────────────────────────
    // The first PCR we encounter anchors the per-file splice offset:
    //   offset_27m = (target_first_output_pts × 300) − first_input_pcr
    // which is then applied to every PCR, PTS, and DTS we emit so the
    // wire timeline continues smoothly from the previous file. Pacing
    // math still uses the *raw* PCR deltas (wall-clock relative to the
    // file's own PCR baseline) — adding a constant offset preserves
    // those deltas, so pacing and rewriting are independent.
    //
    // We track the highest emitted PTS on the **audio** PIDs (preferred)
    // and fall back to the highest emitted PTS across all PIDs when no
    // audio PID has been observed yet. Anchoring `close_file` on the
    // audio high-water-mark keeps the next loop's first audio PES
    // tightly adjacent to the previous loop's last audio sample —
    // critical because real broadcast captures end with last video PES
    // PTS ~1 s past the last audio PES PTS (mux buffering), and
    // anchoring on the overall max-PTS would shove the next file's
    // first PCR forward by that ~1 s delta, leaving an audible per-loop
    // gap on receivers. Anchoring on last-PCR is documented broken on
    // Sky Witness 1080i25 — see memory
    // `project_splice_di1_pcr_anchor_known_broken.md`.
    let target_pts_90k = session.cont.next_target_output_pts_90k;
    let mut splice_offset_27m: Option<i64> = None;
    let mut max_emitted_pts_90k: u64 = target_pts_90k;
    let mut max_emitted_audio_pts_90k: Option<u64> = None;
    // Track the highest emitted PCR (in 90 kHz units) on this file so
    // `close_file` can anchor the next loop's target ≥ the previous loop's
    // last PCR. Without this, real broadcast captures (whose video PES PTS
    // ~1 s past the last AUDIO PES PTS at EOF, due to mux look-ahead) put
    // the next loop's first PCR ~60 ms BEFORE the previous loop's last
    // PCR — a backward PCR jump that's below the rewriter's 500 ms
    // discontinuity threshold so it slips through and the wire pacer
    // cumulatively falls behind wallclock at ~1000 ppm. See the audio-
    // anchor reasoning in `SpliceContinuity::close_file` below.
    let mut max_emitted_pcr_90k: u64 = target_pts_90k;

    // Audio PIDs discovered by parsing the program's PMT during playback.
    // The PMT lives at `pmt_pid` (extracted from the PAT on its first
    // PUSI=1 packet). The audio PID set is rebuilt every time a fresh
    // PMT is seen so PMT-version bumps that add/remove audio tracks are
    // tracked. Capacity bounded by the number of audio tracks per program
    // (rare > 4 in broadcast).
    let mut pat_first_pmt_pid: Option<u16> = None;
    let mut audio_pids: HashSet<u16> = HashSet::with_capacity(4);

    loop {
        if session.cancel.is_cancelled() {
            break;
        }

        match reader.read_exact(&mut packet).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("read {}", path.display())));
            }
        }

        if packet[0] != SYNC_BYTE {
            // Best-effort resync — scan one byte at a time until 0x47 is the
            // 0th byte of a complete 188-byte slot. Bail after 1 MiB so a
            // malformed file doesn't burn CPU forever.
            if !resync_to_sync_byte(&mut reader, &mut packet).await? {
                break;
            }
        }

        // Discard the per-packet trailer for non-188 strides. 192-byte M2TS
        // technically prefixes a 4-byte timestamp; for our wire-output use
        // (we just need the 188-byte TS body and don't propagate the BDAV
        // arrival timestamp) we treat it as a trailer to avoid an extra
        // realignment step. 204-byte DVB packets carry 16 bytes of RS
        // parity which the wire output never reproduces. Both cases just
        // need to be skipped.
        if trailer_len > 0 {
            match reader.read_exact(&mut trailer_buf[..trailer_len]).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    return Err(anyhow::Error::from(e)
                        .context(format!("trailer read {}", path.display())));
                }
            }
        }

        // MPTS → SPTS down-select runs first so other-program packets are
        // dropped before pacing / CC rewriting / PCR rewriting touch them.
        // The filter emits exactly one of: 0 bytes (drop), 188 bytes (keep
        // unchanged), or 188 bytes (PAT → synthetic single-program PAT).
        if let Some(ref mut filter) = program_filter {
            filter_scratch.clear();
            filter.filter_into(&packet, &mut filter_scratch);
            if filter_scratch.is_empty() {
                continue;
            }
            // The filter is byte-level; for a single-input packet it never
            // emits more than one output packet, so this slice copy stays
            // bounded at TS_PACKET bytes.
            packet.copy_from_slice(&filter_scratch[..TS_PACKET]);
        }

        // Read the raw PCR (pre-rewrite) for pacing and offset anchoring.
        // Filter to the anchor PCR PID so MPTS files (multiple independent
        // 27 MHz clocks, one per program) anchor consistently — the first
        // PCR-bearing packet's PID becomes the anchor and PCRs from other
        // PIDs are ignored for pacing/anchoring. (Their bytes still pass
        // through `rewrite_pcr_in_place` with the constant splice offset
        // because that's a flat shift applied uniformly to every PCR.)
        let raw_pcr_any = extract_pcr_27mhz(&packet);
        let pcr_pid_early = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        if raw_pcr_any.is_some() && anchor_pcr_pid.is_none() {
            anchor_pcr_pid = Some(pcr_pid_early);
        }
        let raw_pcr = if anchor_pcr_pid == Some(pcr_pid_early) {
            raw_pcr_any
        } else {
            None
        };

        // Anchor the splice offset on the very first PCR we see in this
        // file. Until we have an offset, packets stream through with CC
        // rewriting only — fine for the brief lead-in before the first
        // PCR-bearing packet (typically arrives within the first few
        // packets of every well-formed TS file).
        if splice_offset_27m.is_none() {
            if let Some(pcr) = raw_pcr {
                splice_offset_27m =
                    Some((target_pts_90k as i128 * 300 - pcr as i128) as i64);
            }
        }

        if let Some(pcr) = raw_pcr {
            // Windowed bitrate estimation. The window re-anchors every
            // 5 seconds of PCR-time so the estimate tracks VBR rate
            // changes within the file instead of biasing toward the
            // front-loaded I-frame data. The wire emitter paces at the
            // instantaneous PCR rate — the pacer must match it, not the
            // file-long average. Window of 2 s is long enough to smooth
            // per-PCR-pair noise (~0.4 % at 188 B / 50 KB granularity)
            // while short enough to track a 25 fps GOP structure (1-2
            // seconds per I-frame cycle).
            const WINDOW_27MHZ: u64 = 27_000_000 * 2; // 2 seconds
            const MIN_SPAN_27MHZ: u64 = 27_000_000;   // 1 second
            let pcr_byte_pos = bytes_emitted + bundle.len() as u64;
            match window_pcr_27mhz {
                None => {
                    window_pcr_27mhz = Some(pcr);
                    window_byte_pos = pcr_byte_pos;
                }
                Some(anchor_pcr) => {
                    let pcr_delta = pcr.wrapping_sub(anchor_pcr);
                    let bytes_delta = pcr_byte_pos.saturating_sub(window_byte_pos);
                    // Discontinuity (backward or > 60 s forward): reset.
                    if pcr_delta > 27_000_000 * 60 || (pcr_delta as i64) < 0 || bytes_delta == 0 {
                        window_pcr_27mhz = Some(pcr);
                        window_byte_pos = pcr_byte_pos;
                    } else if pcr_delta >= MIN_SPAN_27MHZ {
                        let pcr_us = pcr_delta / 27;
                        let observed_bps =
                            bytes_delta.saturating_mul(8 * 1_000_000) / pcr_us;
                        if (100_000..=10_000_000_000).contains(&observed_bps) {
                            let prev = current_bitrate_bps;
                            current_bitrate_bps = observed_bps;
                            tracing::debug!(
                                "media-player windowed bitrate: {} -> {} bps (span={} ms, bytes={})",
                                prev, observed_bps, pcr_us / 1000, bytes_delta,
                            );
                        }
                        // Re-anchor when the window exceeds 5 seconds
                        // so the next estimate reflects recent content.
                        if pcr_delta >= WINDOW_27MHZ {
                            window_pcr_27mhz = Some(pcr);
                            window_byte_pos = pcr_byte_pos;
                        }
                    }
                }
            }

            // Re-anchor the emission deadline on this PCR. Only the anchor
            // PID participates: an MPTS carries one independent 27 MHz
            // clock per program, and mixing them would step the deadline
            // by the inter-program skew (often seconds) at every PID
            // switch — the same trap the bitrate estimator guards against.
            if pcr_deadlines_enabled && anchor_pcr_pid == Some(pcr_pid_early) {
                let epoch = *pcr_epoch_27mhz.get_or_insert(pcr);
                let e_ns = *pacer_epoch_ns.get_or_insert_with(
                    crate::engine::wire_emit::monotonic_now_ns,
                );
                // Discontinuity is a property of the *step* between
                // consecutive PCRs, not of the distance from the epoch.
                // Testing the epoch span instead conflates two unrelated
                // things: it calls a perfectly continuous PCR a
                // discontinuity once an asset has simply played for long
                // enough, and — far worse — it accepts a genuine forward
                // splice of up to the span bound as a real deadline, so
                // the pacer sleeps out the whole jump and playout freezes
                // for its duration. Byte-rate mode could not do that.
                //
                // MPEG-TS requires a PCR at least every 100 ms, so a step
                // beyond `PCR_STEP_DISCONTINUITY_27MHZ` is never the
                // source's own clock advancing. Sign covers the 42-bit
                // wrap (~26.5 h) and a backward splice in one test.
                let continuous = last_anchor_pcr_27mhz.is_none_or(|prev| {
                    let step = pcr.wrapping_sub(prev) as i64;
                    (0..=PCR_STEP_DISCONTINUITY_27MHZ).contains(&step)
                });
                let d = pcr.wrapping_sub(epoch);
                if continuous && (d as i64) >= 0 {
                    last_pcr_target_ns = Some(e_ns.saturating_add((d / 27) * 1_000));
                } else {
                    pcr_epoch_27mhz = Some(pcr);
                    pacer_epoch_ns = Some(crate::engine::wire_emit::monotonic_now_ns());
                    last_pcr_target_ns = pacer_epoch_ns;
                }
                last_anchor_pcr_27mhz = Some(pcr);
                bytes_since_pcr = 0;
            }
        }

        // ── PAT/PMT inspection (audio-PID discovery for splice anchor) ─
        // PAT lives on PID 0x0000. The first PMT PID it lists is our
        // single-program target (media-player is SPTS-after-filter).
        // PMT lives on the discovered PMT PID and lists every ES PID
        // with its `stream_type` byte — we pick the audio streams.
        let pkt_pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        let pkt_pusi = (packet[1] & 0x40) != 0;
        if pkt_pid == 0x0000 && pkt_pusi {
            let programs = crate::engine::ts_parse::parse_pat_programs(&packet);
            if let Some((_, pmt_pid)) = programs.first() {
                pat_first_pmt_pid = Some(*pmt_pid);
            }
        } else if let Some(pmt_pid) = pat_first_pmt_pid {
            if pkt_pid == pmt_pid && pkt_pusi {
                refresh_audio_pids_from_pmt(&packet, &mut audio_pids);
            }
        }

        // ── Smooth-splice rewriters (in place, no allocation) ──────────
        rewrite_cc(&mut packet, session.cont);
        if let Some(off_27m) = splice_offset_27m {
            rewrite_pcr_in_place(&mut packet, off_27m);
            // After in-place rewrite, the packet carries the output PCR
            // value. Re-extract and track the high-water-mark — the
            // close_file anchor needs `max(last_pcr, audio_max)` so the
            // next loop's first PCR is ≥ this file's last PCR (otherwise
            // a backward PCR jump <500 ms slips below the rewriter's
            // discontinuity threshold and the wire pacer drifts).
            //
            // MPTS guard: only track PCR high-water-mark on the anchor
            // PCR PID — other PIDs in an MPTS have independent clocks
            // whose absolute values are not comparable. Tracking the
            // cross-PID max would push the next loop's target forward
            // by the inter-PID clock skew (often seconds), corrupting
            // smooth-splice arithmetic.
            if anchor_pcr_pid == Some(pcr_pid_early) {
                if let Some(out_pcr_27m) = extract_pcr_27mhz(&packet) {
                    let out_pcr_90k = (out_pcr_27m / 300) & 0x1_FFFF_FFFF;
                    if out_pcr_90k > max_emitted_pcr_90k {
                        max_emitted_pcr_90k = out_pcr_90k;
                    }
                }
            }
            let off_90k = off_27m / 300;
            // Track the high-water-mark emitted PTS overall *and* on
            // audio PIDs separately. `close_file` prefers the audio
            // high-water mark — see `SpliceContinuity::close_file` doc
            // comment for the audio-gap-per-loop reasoning.
            if let Some(new_pts) =
                rewrite_pes_timestamps_in_place(&mut packet, off_90k)
            {
                if new_pts > max_emitted_pts_90k {
                    max_emitted_pts_90k = new_pts;
                }
                if audio_pids.contains(&pkt_pid) {
                    let cur = max_emitted_audio_pts_90k.unwrap_or(0);
                    if new_pts > cur {
                        max_emitted_audio_pts_90k = Some(new_pts);
                    }
                }
            }
        }
        if session.cont.pending_discontinuity
            && try_set_discontinuity_indicator(&mut packet)
        {
            session.cont.pending_discontinuity = false;
        }

        bundle.extend_from_slice(&packet);
        if bundle.len() >= session.bundle_size {
            // Credit the exact bytes about to leave (== session.bundle_size at
            // this threshold, since appends are whole 188-byte packets) so the
            // windowed bitrate estimator stays correct for any datagram size.
            let blen = bundle.len() as u64;
            let target_ns = ts_bundle_deadline(
                last_pcr_target_ns,
                bytes_since_pcr,
                current_bitrate_bps,
            );
            bytes_since_pcr = bytes_since_pcr.saturating_add(blen);
            bytes_emitted = bytes_emitted.saturating_add(blen);
            let data = std::mem::replace(
                &mut bundle,
                BytesMut::with_capacity(session.bundle_size),
            )
            .freeze();
            if !emit_to_pacer(
                data,
                session,
                &pacer_tx,
                current_bitrate_bps,
                &mut pending_src_bytes,
                target_ns,
            )
            .await
            {
                break;
            }
        }
    }

    // Flush the trailing partial bundle so we don't lose the file's tail.
    if !bundle.is_empty() {
        let data = std::mem::replace(
            &mut bundle,
            BytesMut::with_capacity(session.bundle_size),
        )
        .freeze();
        let _ = emit_to_pacer(
            data,
            session,
            &pacer_tx,
            current_bitrate_bps,
            &mut pending_src_bytes,
            ts_bundle_deadline(last_pcr_target_ns, bytes_since_pcr, current_bitrate_bps),
        )
        .await;
    }

    // Carry the converged cumulative rate to the next loop of *this same
    // source* so the pacer starts at the known-good rate instead of the
    // head-scan estimate (see `carried_bitrate_for`/issue #68 above).
    session.cont.set_converged_bitrate(source_id, current_bitrate_bps);

    // Tear down the OS-thread pacer: dropping the sender lets the thread's
    // `recv_timeout` poll see `Disconnected` on its next iteration (within
    // 50 ms). Join from a blocking-friendly context so this thread isn't
    // pinned on a tokio worker while the pacer drains its remaining queue.
    drop(pacer_tx);
    let _ = tokio::task::spawn_blocking(move || pacer_thread.join()).await;

    // Hand off the splice anchor to `SpliceContinuity`.
    //
    // The anchor must satisfy two properties:
    //
    // 1. The next loop's first audio PES PTS should be tightly adjacent
    //    to this loop's last audio PTS — anchoring on
    //    `max_emitted_audio_pts_90k` keeps the audio gap to ~200 ms
    //    instead of ~1 s on real broadcast captures.
    // 2. The next loop's first PCR must be ≥ this loop's last PCR.
    //    Otherwise the output PCR sequence has a small backward jump at
    //    every loop boundary — typically ~60 ms on captures where video
    //    PES PTS ~1 s past audio PES PTS (mux look-ahead). That jump is
    //    below the rewriter's 500 ms discontinuity threshold so it
    //    slips through, and the wire pacer cumulatively falls behind
    //    wallclock at ~1000 ppm.
    //
    // We satisfy both by taking `max(audio_max, pcr_max)` (each falling
    // back as needed). On real broadcast captures with the video-mux-
    // ahead-of-audio shape `pcr_max > audio_max` so `pcr_max` wins,
    // costing an extra ~90 ms of audio gap per loop in exchange for
    // eliminating the 60 ms PCR drift. On synthetic / ffmpeg-generated
    // tracks where audio and video end together `audio_max ≈ pcr_max`
    // so behaviour is unchanged.
    let anchor_pts = match max_emitted_audio_pts_90k {
        Some(a) => a.max(max_emitted_pcr_90k),
        None => max_emitted_pts_90k.max(max_emitted_pcr_90k),
    };

    // Signal the splice-boundary video-audio gap to the TsAudioReplacer
    // so it can silence-pad output audio PTS to match the video timeline.
    //
    // On real broadcast captures (Sky Witness, Seven, etc.) the file ends
    // with `pcr_max > audio_max` by ~200 ms (video mux look-ahead). The
    // splice anchor is `max(audio_max, pcr_max) + GUARD`, so the next
    // loop's first audio PES starts ~(pcr_max − audio_max + GUARD) past
    // this loop's last audio PES — but the TsAudioReplacer's encoder
    // stamps output at steady sample-rate cadence and only sees a ~4.5 ms
    // audio-PES gap. Without this signal, audio PTS falls behind video
    // PTS by ~(pcr_max − audio_max) per loop (~-7 ms/min on Sky Witness).
    //
    // The signal rides on the same `pcr_jump_signal` channel that
    // `TsPtsRewriter` uses for > 500 ms PCR jumps. `TsAudioReplacer`
    // drains it via `swap(0)` on the next `consume_pes` and routes the
    // value through `pending_silence_27mhz` → silence frames → encoder
    // sample advance → output PTS catches up. No changes needed in
    // `ts_audio_replace.rs`.
    // Signal the per-loop video-audio gap to the TsAudioReplacer.
    // The signal value is delivered into `pending_silence_27mhz` on
    // the audio replacer's next `consume_pes`. The PES that arrives
    // on the next call (loop 2's first audio PES, jumped forward by
    // `next_target − audio_max + audio_offset`) triggers the > 500 ms
    // forward-PTS discontinuity branch — re-anchor sets `out_pts_90k`
    // to the new source PTS and resets `samples_since_anchor = 0` so
    // the monotonicity guard sees the pristine state, NOT the
    // post-silence-pad state. Then the pending silence drains into
    // the accumulator: the encoder emits silence frames at the new
    // anchor, filling the per-loop audio-video gap. Audio output
    // resumes in sync with PCR every loop.
    if let Some(signal) = session.splice_gap_signal.as_ref() {
        let audio_max = max_emitted_audio_pts_90k.unwrap_or(max_emitted_pts_90k);
        if max_emitted_pcr_90k > audio_max {
            let gap_90k = max_emitted_pcr_90k - audio_max;
            let gap_27m = (gap_90k as i64).saturating_mul(300);
            signal.fetch_add(gap_27m, std::sync::atomic::Ordering::Release);
        }
    }

    session.cont.close_file(anchor_pts);
    Ok(())
}

/// Walk a PMT TS packet's program-element loop, returning the set of
/// PIDs carrying an audio elementary stream. Recognised stream_types:
/// MPEG-1 audio (0x03), MPEG-2 audio (0x04), AAC ADTS (0x0F), AAC LATM
/// (0x11), AC-3 (0x81), DTS (0x82), DTS-HD (0x88), E-AC-3 (0x87), and
/// stream_type 0x06 (private) *when* its ES descriptors include the
/// AC-3 (`0x6A`) / E-AC-3 (`0x7A`) descriptor or a `registration_descriptor`
/// with format_identifier `"AC-3"` / `"EAC3"` — DVB carries AC-3 via this
/// path (BSkyB Sky Witness HD captures use it on the 1080i25 testbed file).
///
/// `out` is cleared and repopulated each call so PMT-version bumps that
/// add or remove audio tracks are tracked. No allocations on the steady
/// state — `HashSet::clear` keeps the existing capacity.
pub(crate) fn refresh_audio_pids_from_pmt(pkt: &[u8; TS_PACKET], out: &mut HashSet<u16>) {
    let mut offset = 4usize;
    if (pkt[3] >> 4) & 0b11 == 0b11 {
        // adaptation_field present — skip it
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET {
        return;
    }
    // PSI pointer_field
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET {
        return;
    }
    if pkt[offset] != 0x02 {
        // Not a PMT (table_id 0x02). Could happen if a PMT-version
        // bump shifts the section across packets and the second packet
        // arrives first — leave the audio set as-is and re-discover
        // on the next PUSI=1 PMT.
        return;
    }
    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let mut pos = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET)
        .saturating_sub(4);

    out.clear();
    while pos + 5 <= data_end {
        let stream_type = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        let is_audio = match stream_type {
            0x03 | 0x04 | 0x0F | 0x11 | 0x81 | 0x82 | 0x87 | 0x88 => true,
            0x06 => private_es_descriptors_indicate_audio(
                pkt,
                pos + 5,
                (pos + 5 + es_info_len).min(data_end),
            ),
            _ => false,
        };
        if is_audio {
            out.insert(es_pid);
        }
        pos += 5 + es_info_len;
    }
}

/// Inspect a `stream_type=0x06` ES's descriptor loop and decide whether
/// it carries audio. Returns `true` if any descriptor identifies it as
/// AC-3 / E-AC-3 (DVB tags `0x6A` / `0x7A`, or a `registration_descriptor`
/// 0x05 with format_identifier `"AC-3"` / `"EAC3"`). Returns `false` for
/// teletext / subtitle private streams which also use type 0x06.
fn private_es_descriptors_indicate_audio(
    pkt: &[u8; TS_PACKET],
    start: usize,
    end: usize,
) -> bool {
    let mut p = start;
    while p + 2 <= end {
        let tag = pkt[p];
        let len = pkt[p + 1] as usize;
        if p + 2 + len > end {
            return false;
        }
        match tag {
            0x6A | 0x7A => return true,
            0x05 if len == 4 => {
                let fmt = &pkt[p + 2..p + 6];
                if fmt == b"AC-3" || fmt == b"EAC3" {
                    return true;
                }
            }
            _ => {}
        }
        p += 2 + len;
    }
    false
}

/// Detect the on-disk MPEG-TS packet stride for the given head buffer.
/// Returns one of `188` (canonical), `192` (M2TS / AVCHD / Blu-ray BDAV —
/// 4-byte timestamp per packet), or `204` (DVB Reed-Solomon parity-suffixed
/// broadcast capture). Falls back to `188` for anything inconclusive — the
/// caller's existing 188-byte resync loop then handles malformed files.
///
/// Algorithm: for each candidate stride, scan every byte offset in the
/// buffer for a sync byte and walk forward in `stride` increments. The
/// first stride that yields `MIN_STRIDE_HITS` consecutive sync bytes wins.
/// 188 is tried first so canonical files take the cheapest detection path.
///
/// Pure function — unit-tested.
pub(super) fn detect_ts_packet_size_in_buf(buf: &[u8]) -> usize {
    /// Minimum consecutive sync hits at the candidate stride. Five gives
    /// effectively zero false positives — a random byte sequence of length
    /// `5 × 204` ≈ 1 KiB would hit 0x47 at five exact stride offsets with
    /// probability `(1/256)^4` ≈ 10⁻¹⁰, and real TS files lock in within
    /// the first PAT/PMT/PCR cycle (≈ first 1-2 KiB).
    const MIN_STRIDE_HITS: usize = 5;
    /// Order matters: 188 is canonical and the cheapest case to confirm,
    /// so it's tried first to short-circuit the probe on the dominant
    /// majority of files.
    const CANDIDATES: [usize; 3] = [TS_PACKET, 192, 204];

    for &stride in &CANDIDATES {
        let needed = (MIN_STRIDE_HITS - 1).saturating_mul(stride) + 1;
        if buf.len() < needed {
            continue;
        }
        let max_start = buf.len() - needed;
        for start in 0..=max_start {
            if buf[start] != SYNC_BYTE {
                continue;
            }
            let mut hits = 1usize;
            for i in 1..MIN_STRIDE_HITS {
                let pos = start + i * stride;
                if pos >= buf.len() || buf[pos] != SYNC_BYTE {
                    break;
                }
                hits += 1;
            }
            if hits >= MIN_STRIDE_HITS {
                return stride;
            }
        }
    }
    TS_PACKET
}

/// Find the next sync byte and align — read one byte at a time, when 0x47
/// shows up, treat it as the start of a new packet and read the remaining
/// 187 bytes. Bails after 1 MiB of slop.
async fn resync_to_sync_byte<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    packet: &mut [u8; TS_PACKET],
) -> Result<bool> {
    let mut byte = [0u8; 1];
    for _ in 0..(1 << 20) {
        match reader.read_exact(&mut byte).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(anyhow::Error::from(e).context("resync read")),
        }
        if byte[0] == SYNC_BYTE {
            packet[0] = SYNC_BYTE;
            reader
                .read_exact(&mut packet[1..])
                .await
                .map_err(|e| anyhow::Error::from(e).context("resync follow-on read"))?;
            return Ok(true);
        }
    }
    Err(anyhow!(
        "TS file has no sync byte in 1 MiB of slop — file is not MPEG-TS or is severely corrupt"
    ))
}

/// Scan a file-head buffer for the first two PCR-bearing packets on the
/// same PID and compute the file's source bitrate from `(bytes_between)
/// × 8 / pcr_delta`. Returns `None` when the head doesn't carry two
/// PCRs (e.g. extremely low-bitrate streams, or a buffer too small to
/// reach the second PCR). Skips PCR pairs whose delta is implausible
/// (< 1 ms or > 2 s — guards against multi-program PCRs interleaving
/// or a malformed file).
///
/// Used to seed the OS-thread pacer's initial bitrate, eliminating the
/// per-loop slow-start transient that would otherwise leave each file
/// restart pacing below source rate for several seconds.
fn scan_head_bitrate(buf: &[u8], stride: usize) -> Option<u64> {
    if stride < TS_PACKET || buf.len() < stride {
        return None;
    }
    // Walk the buffer and record FIRST + LAST PCR on the first PCR-bearing
    // PID we see. Using first-to-last (rather than first-to-second-PCR)
    // averages over the entire head window so initial-mux idiosyncrasies
    // (PSI bursts, occasional NULL padding bursts, encoder warmup
    // jitter) don't bias the estimate by 10-15 %. Previous behaviour:
    // returned on the very first valid PCR pair. For a 512 KB head on
    // a 4 Mbps file (~ 1 s of source content), that meant a 35 ms sample
    // window — one bad inter-PCR delta could shift the estimate ±15 %
    // and the OS pacer would carry the wrong initial rate for the first
    // second of every file loop (= cumulative drift on short loops).
    let mut first: Option<(u16, u64, usize)> = None;
    let mut last_same_pid: Option<(u64, usize)> = None;
    let mut pos = 0usize;
    while pos + TS_PACKET <= buf.len() {
        if buf[pos] != SYNC_BYTE {
            // Try resyncing on the next byte; misaligned files are
            // already handled by the main loop's `resync_to_sync_byte`,
            // but for a head scan we just walk forward.
            pos += 1;
            continue;
        }
        let mut pkt = [0u8; TS_PACKET];
        pkt.copy_from_slice(&buf[pos..pos + TS_PACKET]);
        if let Some(pcr) = extract_pcr_27mhz(&pkt) {
            let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
            match first {
                None => first = Some((pid, pcr, pos)),
                Some((first_pid, _, _)) if first_pid == pid => {
                    last_same_pid = Some((pcr, pos));
                }
                _ => {
                    // Different PID — keep the original anchor (the
                    // first PID's cadence is what `play_ts_file` will
                    // ultimately pace from after the program filter).
                }
            }
        }
        pos += stride;
    }
    let (_pid, first_pcr, first_pos) = first?;
    let (last_pcr, last_pos) = last_same_pid?;
    let pcr_delta = last_pcr.wrapping_sub(first_pcr);
    let pcr_us = pcr_delta / 27;
    // Sanity: total head span must be ≥ 100 ms; covers > 3 inter-PCR
    // periods at a typical 40 ms cadence so the average is meaningful.
    if !(100_000..=10_000_000).contains(&pcr_us) {
        return None;
    }
    let bytes_delta = (last_pos.saturating_sub(first_pos)) as u64;
    if bytes_delta == 0 {
        return None;
    }
    let observed_bps = bytes_delta.saturating_mul(8 * 1_000_000) / pcr_us;
    if (100_000..=10_000_000_000).contains(&observed_bps) {
        Some(observed_bps)
    } else {
        None
    }
}

/// Parse the 27 MHz PCR out of the adaptation field, if this packet
/// carries one. PCR layout per ISO/IEC 13818-1:
/// `byte 3 bit 0x20`        → adaptation_field_control bit
/// `byte 4`                 → adaptation_field_length (0 means no AF body)
/// `byte 5 bit 0x10`        → PCR_flag
/// `bytes 6..12`            → 33 bit base @ 90 kHz | 6 bits reserved | 9 bit ext @ 27 MHz
fn extract_pcr_27mhz(pkt: &[u8; TS_PACKET]) -> Option<u64> {
    let af_ctrl = (pkt[3] >> 4) & 0b11;
    if af_ctrl != 0b10 && af_ctrl != 0b11 {
        return None;
    }
    let af_len = pkt[4] as usize;
    if af_len == 0 || 5 + af_len > TS_PACKET {
        return None;
    }
    let flags = pkt[5];
    if flags & 0x10 == 0 {
        return None;
    }
    if af_len < 7 {
        return None;
    }
    let p = &pkt[6..12];
    let base = ((p[0] as u64) << 25)
        | ((p[1] as u64) << 17)
        | ((p[2] as u64) << 9)
        | ((p[3] as u64) << 1)
        | ((p[4] as u64) >> 7);
    let ext = (((p[4] as u64) & 0x01) << 8) | (p[5] as u64);
    Some(base * 300 + ext)
}

// ── In-place TS-packet rewriters (smooth-splice helpers) ────────────────
//
// All rewriters operate on a `&mut [u8; TS_PACKET]` and never allocate.
// The only allocator activity introduced by the smooth-splice path is the
// first-seen-PID insert into `SpliceContinuity::last_cc`, which is bounded
// to the handful of PIDs the playlist will ever touch (PAT, PMT, video,
// audio, optional PCR, null) — see `feedback_performance_data_path` for
// the no-allocation-on-data-path constraint this protects.

/// Rewrite a TS packet's continuity counter so the on-wire CC sequence
/// stays continuous across loops and playlist transitions. Returns the
/// CC value written, or `None` if the packet was skipped (null PID).
///
/// Per ISO/IEC 13818-1: payload-bearing packets advance CC by 1, AF-only
/// packets (afc=10, no payload) keep CC == previous payload's CC, and
/// the null PID 0x1FFF is fully exempt.
///
/// The earlier "skip AF-only packets" shortcut left the source's
/// original CC in place on AF-only frames — but the source CC and the
/// rewritten payload CC sequence drift apart over time. Receivers then
/// see e.g. payload CC=3, AF-only CC=5 (from source), payload CC=4 and
/// report "missing packets" on the video PID — visible as periodic
/// pixelation on PID 0x0203 (Sky Witness HD). The fix below pins every
/// AF-only packet's CC to the most-recent payload CC on the same PID.
#[inline]
pub(super) fn rewrite_cc(
    pkt: &mut [u8; TS_PACKET],
    cont: &mut SpliceContinuity,
) -> Option<u8> {
    if pkt[0] != SYNC_BYTE {
        return None;
    }
    let pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
    if pid == 0x1FFF {
        return None;
    }
    let afc = (pkt[3] >> 4) & 0b11;
    let has_payload = afc == 0b01 || afc == 0b11;
    if !has_payload {
        // AF-only packet: CC must equal the previous payload's CC on
        // this PID. If we've never seen a payload on this PID yet,
        // leave the source CC alone (cold-start fallback — the next
        // payload-bearing packet will anchor the sequence).
        if let Some(prev) = cont.last_cc.get(&pid).copied() {
            pkt[3] = (pkt[3] & 0xF0) | prev;
            return Some(prev);
        }
        return None;
    }
    let next = match cont.last_cc.get(&pid).copied() {
        Some(prev) => (prev + 1) & 0x0F,
        None => 0,
    };
    cont.last_cc.insert(pid, next);
    pkt[3] = (pkt[3] & 0xF0) | next;
    Some(next)
}

/// Add a signed 27 MHz offset to the PCR field of a TS packet, modulo the
/// full PCR field range (`2^33 × 300`). Returns true if the packet had a
/// PCR and was rewritten; false otherwise.
///
/// Bit layout per ISO/IEC 13818-1 §2.4.3.5:
/// `byte 4` upper bit = base[0]; middle 6 bits = reserved (`'1'`); low bit
/// = ext[8]. `byte 5` = ext[7..0].
#[inline]
pub(super) fn rewrite_pcr_in_place(
    pkt: &mut [u8; TS_PACKET],
    offset_27m: i64,
) -> bool {
    let af_ctrl = (pkt[3] >> 4) & 0b11;
    if af_ctrl != 0b10 && af_ctrl != 0b11 {
        return false;
    }
    let af_len = pkt[4] as usize;
    if af_len == 0 || 5 + af_len > TS_PACKET {
        return false;
    }
    let flags = pkt[5];
    if flags & 0x10 == 0 || af_len < 7 {
        return false;
    }
    let p = 6;
    let base = ((pkt[p] as u64) << 25)
        | ((pkt[p + 1] as u64) << 17)
        | ((pkt[p + 2] as u64) << 9)
        | ((pkt[p + 3] as u64) << 1)
        | ((pkt[p + 4] as u64) >> 7);
    let ext = (((pkt[p + 4] as u64) & 0x01) << 8) | (pkt[p + 5] as u64);
    let pcr_27m = base * 300 + ext;

    const PCR_RANGE: i128 = (1i128 << 33) * 300;
    let adjusted = (pcr_27m as i128 + offset_27m as i128).rem_euclid(PCR_RANGE) as u64;
    let new_base = adjusted / 300;
    let new_ext = adjusted % 300;

    pkt[p] = (new_base >> 25) as u8;
    pkt[p + 1] = (new_base >> 17) as u8;
    pkt[p + 2] = (new_base >> 9) as u8;
    pkt[p + 3] = (new_base >> 1) as u8;
    // base[0] | reserved (6 bits, all '1' = 0x7E) | ext[8]
    pkt[p + 4] = (((new_base & 0x01) as u8) << 7)
        | 0x7E
        | ((new_ext >> 8) as u8 & 0x01);
    pkt[p + 5] = (new_ext & 0xFF) as u8;
    true
}

/// Add a signed 90 kHz offset to the PTS (and DTS, if present) inside any
/// PES header that begins inside this TS packet's payload. Returns the
/// adjusted PTS if rewriting occurred, or `None` if the packet doesn't
/// carry a PES start (`PUSI=0`, non-PTS-bearing stream_id, etc.).
///
/// Mirrors the bit interleave from `engine::rtmp::ts_mux::write_timestamp`
/// — get the layout wrong and every player chokes. Round-tripped via
/// unit test on a synthetic 33-bit-wraparound input below.
#[inline]
pub(super) fn rewrite_pes_timestamps_in_place(
    pkt: &mut [u8; TS_PACKET],
    offset_90k: i64,
) -> Option<u64> {
    if pkt[0] != SYNC_BYTE {
        return None;
    }
    let pusi = (pkt[1] & 0x40) != 0;
    if !pusi {
        return None;
    }
    let afc = (pkt[3] >> 4) & 0b11;
    let payload_start = match afc {
        0b01 => 4,
        0b11 => 4usize + 1 + pkt[4] as usize,
        _ => return None,
    };
    // Need at least the 9 bytes of fixed PES header that precede the PTS.
    if payload_start + 14 > TS_PACKET {
        return None;
    }
    if pkt[payload_start] != 0x00
        || pkt[payload_start + 1] != 0x00
        || pkt[payload_start + 2] != 0x01
    {
        return None;
    }
    let stream_id = pkt[payload_start + 3];
    // PTS-bearing stream_ids: 0xC0..=0xDF audio, 0xE0..=0xEF video,
    // 0xBD private_stream_1, 0xFD extended-stream-id. Padding (0xBE),
    // program_stream_map (0xBC), private_stream_2 (0xBF), and the
    // 0xF0..=0xF8 / 0xFF reserved IDs never carry PTS.
    let pts_eligible = matches!(stream_id, 0xBD | 0xC0..=0xEF | 0xFD);
    if !pts_eligible {
        return None;
    }
    let flags2 = pkt[payload_start + 7];
    let pts_dts_flags = (flags2 >> 6) & 0b11;
    if pts_dts_flags != 0b10 && pts_dts_flags != 0b11 {
        return None;
    }
    let pts_off = payload_start + 9;
    if pts_off + 5 > TS_PACKET {
        return None;
    }
    let pts = read_pes_ts(&pkt[pts_off..pts_off + 5]);
    let new_pts = ((pts as i128 + offset_90k as i128).rem_euclid(1i128 << 33)) as u64;
    write_pes_ts_in_place(&mut pkt[pts_off..pts_off + 5], new_pts);

    if pts_dts_flags == 0b11 {
        let dts_off = pts_off + 5;
        if dts_off + 5 <= TS_PACKET {
            let dts = read_pes_ts(&pkt[dts_off..dts_off + 5]);
            let new_dts = ((dts as i128 + offset_90k as i128).rem_euclid(1i128 << 33)) as u64;
            write_pes_ts_in_place(&mut pkt[dts_off..dts_off + 5], new_dts);
        }
    }
    Some(new_pts)
}

#[inline]
fn read_pes_ts(b: &[u8]) -> u64 {
    let b0 = b[0] as u64;
    let b1 = b[1] as u64;
    let b2 = b[2] as u64;
    let b3 = b[3] as u64;
    let b4 = b[4] as u64;
    (((b0 >> 1) & 0x07) << 30)
        | (b1 << 22)
        | (((b2 >> 1) & 0x7F) << 15)
        | (b3 << 7)
        | ((b4 >> 1) & 0x7F)
}

#[inline]
fn write_pes_ts_in_place(out: &mut [u8], ts: u64) {
    let ts = ts & 0x1_FFFF_FFFF;
    // Preserve the original 4-bit marker prefix (0x02 PTS-only, 0x03 PTS
    // when DTS is present, 0x01 DTS).
    let marker_bits = out[0] >> 4;
    out[0] = (marker_bits << 4) | (((ts >> 29) & 0x0E) as u8) | 0x01;
    out[1] = (ts >> 22) as u8;
    out[2] = (((ts >> 14) & 0xFE) as u8) | 0x01;
    out[3] = (ts >> 7) as u8;
    out[4] = (((ts << 1) & 0xFE) as u8) | 0x01;
}

/// Set `discontinuity_indicator=1` in the adaptation-field flags byte.
/// Returns true if the flag landed (the packet must already have an AF
/// with at least one flags byte). Packets without an AF are skipped —
/// we cannot grow the packet without losing payload data, and the
/// discontinuity flag is a hint for receivers, not a hard requirement
/// once CC + PTS are continuous on the wire.
#[inline]
pub(super) fn try_set_discontinuity_indicator(pkt: &mut [u8; TS_PACKET]) -> bool {
    let af_ctrl = (pkt[3] >> 4) & 0b11;
    if af_ctrl != 0b10 && af_ctrl != 0b11 {
        return false;
    }
    let af_len = pkt[4] as usize;
    if af_len == 0 || 5 + af_len > TS_PACKET {
        return false;
    }
    pkt[5] |= 0x80;
    true
}

// ── MP4 + Image dispatch (Phase 3 / 4) ──────────────────────────────────

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
async fn play_mp4(
    path: &Path,
    paced_bitrate_bps: Option<u64>,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    mp4_demux::play_mp4_file(path, paced_bitrate_bps, session).await
}

#[cfg(not(all(feature = "media-codecs", feature = "fdk-aac")))]
async fn play_mp4(
    _path: &Path,
    _paced_bitrate_bps: Option<u64>,
    _session: &mut PlayerSession<'_>,
) -> Result<()> {
    Err(anyhow!(
        "media-player MP4 source requires the 'media-codecs' and 'fdk-aac' features — rebuild the edge with those enabled, or use a pre-encoded .ts file instead"
    ))
}

#[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
async fn play_image(
    path: &Path,
    fps: u8,
    bitrate_kbps: u32,
    audio_silence: bool,
    duration_secs: Option<u32>,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    session.media_stats.reader_mode.store(
        crate::stats::collector::media_player_reader_mode::IMAGE,
        Ordering::Relaxed,
    );
    // A still has a fixed duration only when the operator sets one; otherwise
    // it renders indefinitely (slate/fallback), so the progress head has no
    // total — leave it unknown (indeterminate bar).
    if let Some(d) = duration_secs {
        session
            .media_stats
            .current_source_duration_ms
            .store((d as u64).saturating_mul(1000), Ordering::Relaxed);
    }
    image_slate::play_image_file(path, fps, bitrate_kbps, audio_silence, duration_secs, session).await
}

#[cfg(not(all(feature = "media-codecs", feature = "fdk-aac")))]
async fn play_image(
    _path: &Path,
    _fps: u8,
    _bitrate_kbps: u32,
    _audio_silence: bool,
    _duration_secs: Option<u32>,
    _session: &mut PlayerSession<'_>,
) -> Result<()> {
    Err(anyhow!(
        "media-player image source requires the 'media-codecs' and 'fdk-aac' features — rebuild the edge with those enabled, or upload the slate as a pre-encoded .ts file"
    ))
}

// ── Bundle helpers ───────────────────────────────────────────────────────

/// Drain `bundle` (one or more 188 B TS packets concatenated) and publish
/// it onto the per-input broadcast as a single `RtpPacket { is_raw_ts: true }`.
/// Mirrors the bundling pattern in `input_test_pattern.rs` — publishing
/// each 188 B packet individually saturates the 2048-slot broadcast and
/// starves the analyser/thumbnail subscribers.
pub(super) fn emit_bundle(
    bundle: &mut BytesMut,
    session: &mut PlayerSession<'_>,
    rtp_ts: u32,
) {
    if bundle.is_empty() {
        return;
    }
    let total_len = bundle.len();
    let data: Bytes = std::mem::replace(bundle, BytesMut::with_capacity(session.bundle_size)).freeze();
    let pkt = RtpPacket {
        data,
        sequence_number: *session.seq_num,
        rtp_timestamp: rtp_ts,
        recv_time_us: crate::util::time::now_us(),
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
    };
    *session.seq_num = session.seq_num.wrapping_add(1);
    session
        .stats
        .input_packets
        .fetch_add(1, Ordering::Relaxed);
    session
        .stats
        .input_bytes
        .fetch_add(total_len as u64, Ordering::Relaxed);
    // Route through the optional ingress transcoder. None ⇒ passthrough
    // (single broadcast send). Some ⇒ block_in_place re-encode then send.
    crate::engine::input_transcode::publish_input_packet_with_post(
        session.transcoder,
        session.post,
        session.per_input_tx,
        pkt,
    );
}

/// Fabricate an RTP timestamp from wall-clock 90 kHz. The TS file's actual
/// PTS / PCR is preserved inside the 188 B packets; the outer RTP timestamp
/// is informational for downstream stats only.
fn fake_rtp_ts(_session: &PlayerSession<'_>) -> u32 {
    let now_us = crate::util::time::now_us();
    ((now_us / 1_000) * 90).rem_euclid(0x1_0000_0000) as u32
}

/// Wrap a ready TS `data` bundle into an `RtpPacket`, run the optional input
/// transcoder + post-processor under `block_in_place`, and push the result to
/// the OS-thread pacer for absolute-deadline emission. Returns `true` on
/// success and `false` if the OS thread has shut down (channel
/// disconnected) — the async loop should bail when that happens.
///
/// Channel backpressure is exposed via `try_send`: when the pacer
/// queue is full the producer yields back to tokio (so the worker can
/// run other tasks) and retries. Producer-side blocking is forbidden by
/// the data-path performance rules — no `block_in_place` around a
/// `send()` that could stall a worker for tens of ms when a slow
/// downstream broadcast subscriber back-pressures the OS thread.
/// Absolute emission deadline for one TS bundle, anchored on the source's
/// own PCR timeline.
///
/// `base` is the wall-clock instant the most recent PCR maps to (computed
/// in [`play_ts_file`] from the `pcr_epoch_27mhz` / `pacer_epoch_ns` pair);
/// `bytes_since_pcr` is the content emitted since that PCR. The estimated
/// rate therefore only interpolates *within* one PCR interval, so a rate
/// overestimate can run ahead by at most one interval instead of
/// compounding across the file and across loops.
///
/// Returns `None` before the first PCR — and for TS that carries no PCR at
/// all — which leaves the pacer in its legacy byte-rate mode.
///
/// Step between consecutive PCRs beyond which the source's clock is taken
/// to have jumped rather than advanced. 500 ms, mirroring
/// [`crate::engine::wire_emit`]'s `PCR_DISCONTINUITY_27MHZ`; MPEG-TS
/// mandates a PCR at least every 100 ms, so a legitimate step is an order
/// of magnitude below this.
const PCR_STEP_DISCONTINUITY_27MHZ: i64 = 13_500_000;

/// Whether TS playout paces on PCR-anchored deadlines (default) or on the
/// legacy byte-rate estimate.
///
/// Rollback lever, same shape as `BILBYCAST_MEDIA_PLAYER_CONTROLLER` and
/// `BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4`. Set
/// `BILBYCAST_MEDIA_PLAYER_PCR_DEADLINES` to `0`/`false`/`off` to fall back
/// to byte-rate pacing on a single node without rolling back the release.
fn pcr_deadline_pacing_enabled() -> bool {
    match std::env::var("BILBYCAST_MEDIA_PLAYER_PCR_DEADLINES") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

fn ts_bundle_deadline(
    base: Option<u64>,
    bytes_since_pcr: u64,
    bitrate_bps: u64,
) -> Option<u64> {
    let base = base?;
    // Clamp the divisor to the estimator's own lower sanity bound so a
    // zero/absurd rate can never produce a division trap or a deadline
    // decades away.
    let rate = bitrate_bps.max(100_000) as u128;
    let ns = (bytes_since_pcr as u128).saturating_mul(8 * 1_000_000_000u128) / rate;
    Some(base.saturating_add(ns.min(u64::MAX as u128) as u64))
}

async fn emit_to_pacer(
    data: Bytes,
    session: &mut PlayerSession<'_>,
    pacer_tx: &std::sync::mpsc::SyncSender<PacerMsg>,
    bitrate_bps: u64,
    // Source bytes consumed by transcoder calls that buffered (returned
    // nothing). Carried across calls so the next message that DOES emit is
    // credited with the full source-content it represents — see `PacerMsg`.
    pending_src_bytes: &mut u64,
    // Explicit absolute emission deadline (MP4 sample-anchored path). `None`
    // ⇒ byte-rate mode (TS passthrough). See [`PacerMsg::target_ns`].
    target_ns: Option<u64>,
) -> bool {
    if data.is_empty() {
        return true;
    }
    let total_len = data.len();
    // `recv_time_us` is filled in by the OS-thread pacer right before
    // the broadcast send so the downstream `output_latency` metric
    // measures only wire_emit's queue + pacer time, not the extra
    // queueing introduced by the OS-thread channel. Matches the
    // semantics of the previous in-task `emit_bundle` path.
    let pkt = RtpPacket {
        data,
        sequence_number: *session.seq_num,
        rtp_timestamp: fake_rtp_ts(session),
        recv_time_us: 0,
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
    };
    *session.seq_num = session.seq_num.wrapping_add(1);
    session.stats.input_packets.fetch_add(1, Ordering::Relaxed);
    session.stats.input_bytes.fetch_add(total_len as u64, Ordering::Relaxed);

    // Run the optional transcoder + post-processor on the async side
    // (they need a tokio runtime for `block_in_place`). `None` means
    // the stage buffered without producing output — silently skip.
    let processed = match crate::engine::input_transcode::process_input_packet_with_post(
        session.transcoder,
        session.post,
        pkt,
    ) {
        Some(p) => p,
        None => {
            // Transcoder buffered this bundle without emitting. Remember its
            // source bytes so the next emitted message is paced for the full
            // content it carries; emitting nothing now keeps the pacer's
            // content clock conserved rather than dropping this slice's time.
            *pending_src_bytes = pending_src_bytes.saturating_add(total_len as u64);
            return true;
        }
    };

    // This message carries its own source bundle plus any buffered ones.
    let content_src_bytes = pending_src_bytes.saturating_add(total_len as u64);
    *pending_src_bytes = 0;

    // Hand off to the OS-thread pacer. On `Full`, park briefly and retry
    // — the pacer drains at file-bitrate (~2.1 ms per bundle at 5 Mbps),
    // so the producer typically waits at most one bundle period.
    let mut to_send = PacerMsg {
        pkt: processed,
        target_ns,
        bitrate_bps,
        content_src_bytes,
    };
    loop {
        if session.cancel.is_cancelled() {
            return false;
        }
        match pacer_tx.try_send(to_send) {
            Ok(()) => {
                session.media_stats.pacer_queue_depth.fetch_add(1, Ordering::Relaxed);
                // First successful hand-off means playout is genuinely
                // under way. Without this the state only ever reached
                // PLAYING via the lateness-latch RECOVERY branch, so a
                // healthy input that never lagged reported "starting"
                // for its entire life — making the documented state
                // machine wrong and any manager UI keyed on "playing"
                // permanently dark. Compare-exchange keeps this a
                // one-shot: it must not stomp a later STALLED.
                let _ = session.media_stats.state.compare_exchange(
                    media_player_state::STARTING,
                    media_player_state::PLAYING,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                poll_pacer_lateness_latch(session);
                return true;
            }
            Err(std::sync::mpsc::TrySendError::Full(msg)) => {
                to_send = msg;
                // Park, don't spin. `tokio::task::yield_now()` here
                // reschedules immediately rather than sleeping, so the
                // producer burned a full tokio worker at ~100 % of a
                // core for the ENTIRE duration of every MP4 played —
                // the queue sits at capacity in steady state (the
                // producer demuxes far faster than realtime), so this
                // branch is the common path, not an edge case. On a
                // 4-core SBC that is a quarter of the box spent
                // spinning, stealing cycles from codec and wire-emit
                // work. Constraint 1: never block the data path — and
                // never starve it either.
                //
                // A short sleep is correct here rather than a precise
                // one: the pacer thread owns emission timing, this is
                // only backpressure. Selecting on the cancel token at
                // the same time also restores prompt shutdown — the
                // spin previously only noticed cancellation between
                // retries.
                tokio::select! {
                    biased;
                    _ = session.cancel.cancelled() => return false,
                    _ = tokio::time::sleep(PACER_FULL_BACKOFF) => {}
                }
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// How long the producer parks when the pacer queue is full before
/// retrying the hand-off.
///
/// Sized at roughly one bundle period for a high-bitrate file (1316 B at
/// 50 Mbps ≈ 210 µs) so the retry lands about when a slot frees. The
/// queue is 16 deep, so even a several-fold overshoot only drains slack
/// rather than starving the pacer — and the pacer, not this producer,
/// owns emission timing, so this value has no bearing on output pacing
/// accuracy. It exists purely to stop the producer spinning.
const PACER_FULL_BACKOFF: std::time::Duration = std::time::Duration::from_micros(200);

/// Minimum wall-clock spacing the MP4 deadline pacer will place between
/// two consecutive bundles when it has fallen behind schedule.
///
/// Only ever applies to a backlog — when the pacer is meeting its
/// deadlines the computed target is already beyond this floor and the
/// clamp is a no-op. It bounds the drain rate of a backlog to roughly
/// 1316 B / 25 µs ≈ 420 Mbps, far above any real file bitrate but far
/// below the memcpy-rate flood an unclamped past-deadline run produces.
/// The point is to keep a recovery bounded, not to pace normally.
const MIN_BUNDLE_SPACING_NS: u64 = 25_000;

/// How far behind this thread's clock an explicit deadline schedule may
/// fall before it is re-based onto the present rather than drained at
/// [`MIN_BUNDLE_SPACING_NS`].
///
/// Legitimate lateness is bounded by the producer's lead, itself bounded by
/// [`PACER_QUEUE_CAP`] (16) messages of content — tens of milliseconds at
/// any real rate. Half a second is therefore unambiguous evidence of a
/// clock step or a producer stall, not of ordinary scheduling jitter.
const MAX_DEADLINE_LATENESS_NS: u64 = 500_000_000;

/// The mirror bound: how far into the future an explicit deadline may sit
/// before it is treated as a source discontinuity rather than a schedule.
///
/// Without it the pacer sleeps out the whole jump and playout freezes for
/// its duration, invisibly — `late_ms` is a saturating subtract, so running
/// early reports 0 and `media_player_pacer_lagging` never latches.
/// Generous relative to the ~16-message producer lead because
/// `play_ts_file` now re-epochs on a PCR step of 500 ms, which is the
/// precise guard; this is the backstop for everything that misses.
const MAX_DEADLINE_LOOKAHEAD_NS: u64 = 1_000_000_000;

/// Apply [`run_paced_emitter`]'s accumulated re-base offset to one deadline.
///
/// Saturating in both directions and floored at 0 — the offset is a signed
/// nanosecond correction against a `CLOCK_TAI` absolute, and a large
/// negative shift on an early deadline must not wrap.
fn shift_deadline(deadline_ns: u64, shift_ns: i64) -> u64 {
    (deadline_ns as i64).saturating_add(shift_ns).max(0) as u64
}

/// Pacer falls behind its own computed wall-clock schedule by at least this
/// much before we latch `media_player_pacer_lagging`. `run_paced_emitter`
/// (SCHED_FIFO / CLOCK_TAI) normally tracks its target within microseconds —
/// hundreds of ms of accumulated lateness means the producer is handing the
/// pacer content faster than the paced bitrate can drain it, i.e. a burst.
const PACER_LATE_WARN_MS: u64 = 250;
/// Hysteresis: lateness must fall back to at most this before the latch
/// clears and `media_player_pacer_recovered` fires. Prevents flapping while
/// lateness oscillates around the warn threshold.
const PACER_LATE_CLEAR_MS: u64 = 100;

/// Check the pacer-lateness latch and fire `media_player_pacer_lagging` /
/// `media_player_pacer_recovered` on a transition. Cheap (a few atomic
/// loads) — safe to call from the hot emit path; the event send itself is
/// edge-triggered so it only does real work on a state change.
fn poll_pacer_lateness_latch(session: &PlayerSession<'_>) {
    let late_ms = session.media_stats.pacer_lateness_current_ms.load(Ordering::Relaxed);
    let was_lagging = session.media_stats.pacer_lagging.load(Ordering::Relaxed);
    if !was_lagging && late_ms >= PACER_LATE_WARN_MS {
        session.media_stats.pacer_lagging.store(true, Ordering::Relaxed);
        session
            .media_stats
            .state
            .store(media_player_state::STALLED, Ordering::Relaxed);
        session.events.emit_flow_with_details(
            EventSeverity::Warning,
            category::FLOW,
            format!(
                "Media-player input '{}': output pacer falling behind schedule ({late_ms} ms late) — \
                 likely a large/bursty compressed sample outpacing the paced bitrate",
                session.input_id
            ),
            session.flow_id,
            serde_json::json!({
                "error_code": "media_player_pacer_lagging",
                "flow_id": session.flow_id,
                "input_id": session.input_id,
                "pacer_lateness_ms": late_ms,
                "pacer_queue_depth": session.media_stats.pacer_queue_depth.load(Ordering::Relaxed),
                "largest_video_sample_bytes": session
                    .media_stats
                    .largest_video_sample_bytes
                    .load(Ordering::Relaxed),
            }),
        );
    } else if was_lagging && late_ms <= PACER_LATE_CLEAR_MS {
        session.media_stats.pacer_lagging.store(false, Ordering::Relaxed);
        session
            .media_stats
            .state
            .store(media_player_state::PLAYING, Ordering::Relaxed);
        session.events.emit_flow_with_details(
            EventSeverity::Info,
            category::FLOW,
            format!(
                "Media-player input '{}': output pacer caught back up to schedule",
                session.input_id
            ),
            session.flow_id,
            serde_json::json!({
                "error_code": "media_player_pacer_recovered",
                "flow_id": session.flow_id,
                "input_id": session.input_id,
                "pacer_lateness_ms": late_ms,
            }),
        );
    }
}

/// OS-thread pacer body. Sleeps on absolute CLOCK_TAI deadlines via
/// `clock_nanosleep(TIMER_ABSTIME)` and emits each bundle onto
/// `broadcast_tx`. Single anchor for the file's lifetime — drift is
/// determined by `bitrate_bps` accuracy, not by per-sleep slip, because
/// each target is computed independently from `iter_start_wall +
/// bytes_emitted * 8 / bitrate_bps`. Bitrate updates re-anchor
/// `iter_start_wall` against the current wallclock so the next bundle's
/// target lands sensibly close to "now" rather than bursting forward or
/// backward.
fn run_paced_emitter(
    thread_name: String,
    rx: std::sync::mpsc::Receiver<PacerMsg>,
    broadcast_tx: tokio::sync::broadcast::Sender<RtpPacket>,
    initial_bitrate_bps: u64,
    cancel: tokio_util::sync::CancellationToken,
    stats: Arc<MediaPlayerStats>,
) {
    let sched_fifo = crate::util::runtime_diag::apply_sched_fifo(&thread_name, 50);
    let mut bitrate_bps: u64 = initial_bitrate_bps.max(1);
    let mut bytes_emitted: u64 = 0;
    // Incremental target tracking — each bundle's target is the previous
    // target plus `bundle_size * 8 / bitrate_bps`. No global `iter_start`
    // anchor; bitrate changes affect *future* inter-bundle spacing
    // without retroactively shifting the entire schedule. That avoids
    // two failure modes a global-anchor design suffers from:
    //   - re-anchoring on every PCR-noise step propagates `clock_gettime`
    //     slip into each anchor → sustained -1700 ppm drift on SCHED_OTHER
    //     (the user's 2026-05-11 testbed)
    //   - re-anchoring only on > 5 % changes leaves smaller (1-4 %)
    //     refinements to shift targets retroactively, producing a
    //     mini-burst on every refinement as queued bundles' targets
    //     land slightly in the past
    let initial_now = crate::engine::wire_emit::monotonic_now_ns();
    // Preroll 50 ms so the first bundle has a brief warm-up before
    // emission — matches `wire_emit::PREROLL_NS`. Mostly cosmetic for
    // a media-player flow but keeps the first-bundle wallclock in the
    // future at startup so `clock_nanosleep` waits properly.
    let mut next_target_wall: u64 = initial_now.saturating_add(50_000_000);
    // Last deadline actually emitted at, in MP4 sample-anchored mode.
    // Floors each subsequent deadline so a clock step or a producer
    // stall cannot collapse a backlog into a zero-spacing burst — see
    // the clamp in the deadline branch below. Seeded so the very first
    // bundle is never held back.
    let mut last_emitted_ns: u64 = initial_now;
    // Signed correction applied to every explicit deadline, so a schedule
    // that has drifted grossly from this thread's clock is re-based **once**
    // and keeps its relative spacing thereafter. Zero in the common case.
    let mut explicit_shift_ns: i64 = 0;
    tracing::info!(
        "media-pacer '{}': starting (bitrate_initial={} bps, sched_fifo={})",
        thread_name,
        bitrate_bps,
        sched_fifo
    );

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let msg = match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(m) => m,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // Mirror the producer-side increment in `emit_to_pacer` — this
        // message just left the hand-off queue.
        stats.pacer_queue_depth.fetch_sub(1, Ordering::Relaxed);

        // Two timing modes, selected per-message (a given pacer thread only
        // ever sees one mode, since play_ts_file and play_demuxed each spawn
        // their own thread — but branching per message keeps the two paths
        // sharing one emitter body):
        let target_ns = if let Some(explicit) = msg.target_ns {
            // ── MP4 sample-anchored mode ──────────────────────────────────
            // The producer computed this deadline from the sample
            // presentation timeline against a fixed `epoch_ns`.
            //
            // Clamp it forward against the last emission, for the same
            // reason the byte-rate branch below clamps against `now`:
            // without this, ANY event that puts a run of deadlines in
            // the past makes the pacer emit them back-to-back with zero
            // sleep — dumping content at memcpy rate. Two realistic
            // triggers:
            //
            //   * `monotonic_now_ns()` reads CLOCK_TAI, which despite
            //     the name is steppable (chrony `makestep` after a DHCP
            //     settle, or phc2sys stepping the system clock — both
            //     documented on these edges). A forward step of X
            //     instantly retires every deadline up to `epoch + X`.
            //   * producer stalls — a slow transcode or post-process in
            //     `emit_to_pacer`, or a preroll too short for the first
            //     IDR's mux — leave a backlog of past deadlines that all
            //     fire at once when the producer catches up.
            //
            // Bursting the remainder of a file at memcpy rate is exactly
            // the overflow this pacer exists to prevent (issue #67), so
            // an unguarded deadline branch reintroduces the bug it fixes.
            //
            // The clamp preserves the schedule whenever it is being met
            // (the common case: `explicit` is already >= the floor, so
            // this is a no-op) and degrades to nominal spacing when it
            // is not, instead of collapsing to zero spacing.
            //
            // ── The floor alone does NOT deliver what the paragraph above
            // claims. `MIN_BUNDLE_SPACING_NS` bounds the spacing between
            // consecutive bundles, not the schedule's lateness against the
            // clock, and 25 µs per 1316-byte bundle is ~421 Mbps — two
            // orders of magnitude above any content rate. So a backlog
            // still drains as a burst, just a floor-rated one: a forward
            // CLOCK_TAI step of X retires X worth of deadlines and dumps
            // them in X/42 000 of the time. On a PTP edge that is not even
            // a fault — the kernel's `tai_offset` starts at 0 and jumps
            // ~37 s the moment chrony or ptp4l sets it, which would dump
            // ~47 MB in under a second into the very decoder queue whose
            // overrun this pacer exists to prevent.
            //
            // The byte-rate branch below never had this problem because it
            // re-anchors on `now` every message (`t.max(now)`). The
            // deadline branch has no `now` term at all, so add the missing
            // one: when the schedule has drifted grossly in either
            // direction, re-base it onto the present and carry that shift
            // forward, so relative spacing is preserved and the drift is
            // paid once rather than per bundle. Within tolerance the shift
            // stays 0 and the schedule is exactly as the producer computed
            // it.
            let now = crate::engine::wire_emit::monotonic_now_ns();
            let shifted = shift_deadline(explicit, explicit_shift_ns);
            let rebased = if now.saturating_sub(shifted) > MAX_DEADLINE_LATENESS_NS
                || shifted.saturating_sub(now) > MAX_DEADLINE_LOOKAHEAD_NS
            {
                explicit_shift_ns = (now as i64).saturating_sub(explicit as i64);
                tracing::warn!(
                    "media-player pacer: deadline schedule re-based onto now \
                     (was {} ms {}) — clock step, producer stall or source discontinuity",
                    now.abs_diff(shifted) / 1_000_000,
                    if shifted < now { "late" } else { "ahead" },
                );
                now
            } else {
                shifted
            };
            let floor = last_emitted_ns.saturating_add(MIN_BUNDLE_SPACING_NS);
            let clamped = rebased.max(floor);
            last_emitted_ns = clamped;
            clamped
        } else {
            // ── Byte-rate mode (TS passthrough) ───────────────────────────
            // Bitrate refinement piggybacks on every bundle so the OS thread
            // tracks the producer's view of the file rate even when the
            // bundle channel is steady-state full. Updates affect FUTURE
            // inter-bundle spacing only; the next target is the previous
            // target plus the new bundle period. No retroactive shift of
            // queued targets, so a rate refinement never produces a burst.
            if msg.bitrate_bps != 0 {
                bitrate_bps = msg.bitrate_bps;
            }
            // Pace by the SOURCE content this message represents, not a fixed
            // `BUNDLE_SIZE`. In passthrough `content_src_bytes == BUNDLE_SIZE`
            // (1 message per source bundle) so behaviour is unchanged; with an
            // input transcoder the count carries any buffered-then-merged
            // source bundles, so the file plays at true 1.0x instead of ~2 %
            // fast. Fall back to BUNDLE_SIZE if a producer ever sends 0.
            let content_bytes = if msg.content_src_bytes != 0 {
                msg.content_src_bytes
            } else {
                BUNDLE_SIZE as u64
            };
            bytes_emitted = bytes_emitted.saturating_add(content_bytes);
            let bundle_period_ns =
                content_bytes.saturating_mul(8_000_000_000) / bitrate_bps.max(1);
            let t = next_target_wall;
            // Advance the running target for the next bundle. If the actual
            // fire wallclock catches up to (or passes) `next_target_wall`
            // — e.g. the channel has been silent for a while and the OS
            // thread had nothing to send — anchor forward from "now" so the
            // pacer doesn't burst through a backlog of past targets when a
            // bundle eventually lands.
            let now = crate::engine::wire_emit::monotonic_now_ns();
            next_target_wall = t.max(now).saturating_add(bundle_period_ns);
            t
        };
        // How far the pacer has already fallen behind this bundle's own
        // deadline, in ms. `0` when on schedule or running ahead. The
        // closest signal the media player has to "am I keeping up with the
        // file's own timeline" — see `MediaPlayerStats` doc comment.
        let now_check = crate::engine::wire_emit::monotonic_now_ns();
        let late_ms = now_check.saturating_sub(target_ns) / 1_000_000;
        stats.record_pacer_lateness_ms(late_ms);
        crate::engine::wire_emit::sleep_until_monotonic_ns(target_ns);
        // `broadcast.send` is non-blocking — slow subscribers count
        // against their own `packets_dropped`, never the producer.
        // Failure means no subscribers; not fatal, continue draining
        // the queue so backpressure stays released for the async
        // producer.
        //
        // Stamp `recv_time_us` here (not in the async producer) so the
        // downstream `output_latency_us` metric measures the wire_emit-
        // side wait only — i.e. the same thing it measured before the
        // OS-thread pacer landed. Without this, the OS-thread queue
        // depth shows up as a ~20 ms latency increase even though
        // wire-side timing is unchanged.
        let mut out_pkt = msg.pkt;
        out_pkt.recv_time_us = crate::util::time::now_us();
        let _ = broadcast_tx.send(out_pkt);
    }
    // Zero the hand-off gauge on the way out. The loop can break on
    // cancellation with messages still queued, and each of those was
    // counted by the producer's `fetch_add` but never reaches the
    // matching `fetch_sub` above. `MediaPlayerStats` outlives any single
    // pacer thread (it spans the whole input, while a thread is spawned
    // per source), so a playlist cycling sources under repeated
    // cancellation would otherwise accumulate phantom depth until the
    // reported value stopped meaning anything.
    stats.pacer_queue_depth.store(0, Ordering::Relaxed);
    tracing::info!("media-pacer '{}': exited", thread_name);
}

/// Re-permute the play order when a commit wraps the playlist back to its
/// first slot.
///
/// The legacy sequential loop rebuilds and re-shuffles `order` on every pass,
/// which is what `MediaPlayerInputConfig::shuffle` documents ("randomise the
/// playback order each time the playlist starts") and what the manager's own
/// help text and AI knowledge base promise. The controller path drew its
/// permutation once before its loop, so a shuffled playlist repeated one fixed
/// rotation for the life of the flow — and because `operator_control` defaults
/// on, every existing `shuffle: true` config inherited that on upgrade with no
/// config change and no event.
///
/// `new_index == 0` identifies the wrap unambiguously: from any other slot the
/// natural successor and an operator `Next` both move forward, so index 0 is
/// only ever reached by wrapping off the end.
fn reshuffle_on_wrap(
    config: &MediaPlayerInputConfig,
    order: &mut [usize],
    cr: &transition::CommitResult,
) {
    if config.shuffle && order.len() > 1 && cr.new_index == 0 {
        shuffle_indices(order);
    }
}

// ── Order-shuffle helper ────────────────────────────────────────────────

fn shuffle_indices(order: &mut [usize]) {
    use rand::seq::SliceRandom;
    let mut rng = rand::rng();
    order.shuffle(&mut rng);
}

#[cfg(test)]
mod ts_pacing_tests {
    //! The PCR-anchored TS pacing scheme reduces to three pure decisions —
    //! what deadline a bundle gets, whether a PCR step is the source's own
    //! clock or a discontinuity, and how a drifted schedule is re-based.
    //! All three decide when bytes hit the wire, so they are pinned here
    //! rather than left to a hardware soak to notice.
    use super::{
        shift_deadline, ts_bundle_deadline, MAX_DEADLINE_LATENESS_NS,
        MAX_DEADLINE_LOOKAHEAD_NS, PCR_STEP_DISCONTINUITY_27MHZ,
    };

    /// No PCR seen yet, or a TS that carries none at all: the pacer must
    /// stay in byte-rate mode rather than be handed a bogus absolute.
    #[test]
    fn no_anchor_means_no_deadline() {
        assert_eq!(ts_bundle_deadline(None, 0, 4_000_000), None);
        assert_eq!(ts_bundle_deadline(None, 999_999, 4_000_000), None);
    }

    /// Within one PCR interval the deadline is linear in bytes emitted.
    /// 1316 B at 4 Mbps is 2.632 ms of content.
    #[test]
    fn deadline_interpolates_linearly_within_the_interval() {
        let base = 1_000_000_000;
        assert_eq!(ts_bundle_deadline(Some(base), 0, 4_000_000), Some(base));
        assert_eq!(
            ts_bundle_deadline(Some(base), 1316, 4_000_000),
            Some(base + 2_632_000),
        );
        assert_eq!(
            ts_bundle_deadline(Some(base), 2632, 4_000_000),
            Some(base + 5_264_000),
        );
    }

    /// A zero or absurd rate must not divide by zero or throw the deadline
    /// decades out; the clamp is the estimator's own lower sanity bound.
    #[test]
    fn an_absurd_rate_is_clamped_not_trusted() {
        let base = 1_000_000_000;
        assert_eq!(
            ts_bundle_deadline(Some(base), 1316, 0),
            ts_bundle_deadline(Some(base), 1316, 100_000),
            "a zero rate must clamp to the same floor as the minimum sane rate",
        );
        // 1316 B at the 100 kbps floor = 105.28 ms — large, but bounded.
        assert_eq!(
            ts_bundle_deadline(Some(base), 1316, 1),
            Some(base + 105_280_000),
        );
    }

    /// The re-base offset is applied saturating and floored at zero — it is
    /// a signed correction against a CLOCK_TAI absolute and must never wrap
    /// a small deadline into the far future.
    #[test]
    fn shifting_a_deadline_saturates_instead_of_wrapping() {
        assert_eq!(shift_deadline(1_000, 500), 1_500);
        assert_eq!(shift_deadline(1_000, -500), 500);
        assert_eq!(shift_deadline(1_000, -5_000), 0, "must floor, not wrap");
        // A real CLOCK_TAI absolute is ~1.78e18 ns, with ample i64 headroom.
        // The largest realistic shift is the kernel's `tai_offset` settling
        // from 0 to ~37 s, in either direction.
        let tai_now = 1_780_000_000_000_000_000u64;
        assert_eq!(
            shift_deadline(tai_now, 37_000_000_000),
            tai_now + 37_000_000_000,
        );
        assert_eq!(
            shift_deadline(tai_now, -37_000_000_000),
            tai_now - 37_000_000_000,
        );
    }

    /// The bounds must sit above any legitimate producer lead
    /// (`PACER_QUEUE_CAP` = 16 messages, tens of ms) and far below the
    /// multi-second jumps they exist to catch — otherwise they either trip
    /// constantly or never.
    #[test]
    fn rebase_bounds_bracket_the_legitimate_producer_lead() {
        let generous_lead_ns = 16 * 10_000_000u64; // 16 bundles @ 10 ms
        assert!(MAX_DEADLINE_LATENESS_NS > generous_lead_ns);
        assert!(MAX_DEADLINE_LOOKAHEAD_NS > generous_lead_ns);
        assert!(MAX_DEADLINE_LOOKAHEAD_NS < 2_000_000_000);
    }

    /// A PCR step is judged against the previous PCR, not the epoch. This
    /// mirrors the predicate in `play_ts_file`; the boundary is what
    /// separates "the source's clock advanced" from "the source spliced".
    fn continuous(prev: u64, pcr: u64) -> bool {
        let step = pcr.wrapping_sub(prev) as i64;
        (0..=PCR_STEP_DISCONTINUITY_27MHZ).contains(&step)
    }

    #[test]
    fn an_ordinary_pcr_advance_is_continuous() {
        // MPEG-TS mandates a PCR at least every 100 ms; 40 ms is typical.
        assert!(continuous(27_000_000, 27_000_000 + 27_000 * 40));
        assert!(continuous(27_000_000, 27_000_000 + 27_000 * 100));
        // Exactly at the bound is still accepted.
        assert!(continuous(
            27_000_000,
            27_000_000 + PCR_STEP_DISCONTINUITY_27MHZ as u64
        ));
    }

    /// The case that made a forward splice freeze playout for its whole
    /// duration: a jump must re-epoch, never become a deadline.
    #[test]
    fn a_forward_splice_is_a_discontinuity() {
        assert!(!continuous(27_000_000, 27_000_000 + 27_000_000 * 2));
        assert!(!continuous(27_000_000, 27_000_000 + 27_000_000 * 1200));
    }

    #[test]
    fn a_backward_step_and_a_wrap_are_discontinuities() {
        assert!(!continuous(27_000_000 * 10, 27_000_000));
        // 42-bit PCR wrap: tiny value after a near-maximal one.
        assert!(!continuous((1u64 << 42) - 27_000_000, 1_000));
    }

    /// A long asset must NOT be called discontinuous merely for having
    /// played a while — the defect in judging span-from-epoch instead of
    /// step-from-previous.
    #[test]
    fn a_multi_hour_asset_stays_continuous() {
        let two_hours = 27_000_000u64 * 3600 * 2;
        assert!(continuous(two_hours, two_hours + 27_000 * 40));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `PlayerSession` for exercising free functions that only
    /// touch `media_stats` / `events` / `flow_id` / `input_id` — doesn't
    /// need a real file, transcoder, or broadcast receiver.
    type SessionHarnessParts = (
        broadcast::Sender<RtpPacket>,
        Arc<FlowStatsAccumulator>,
        CancellationToken,
        SpliceContinuity,
        Option<crate::engine::input_transcode::InputTranscoder>,
        EventSender,
        tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        Arc<MediaPlayerStats>,
    );

    fn test_session_harness() -> SessionHarnessParts {
        let (tx, _rx) = broadcast::channel::<RtpPacket>(16);
        let stats = Arc::new(FlowStatsAccumulator::new(
            "test-flow".into(),
            "test-flow-name".into(),
            "media_player".into(),
        ));
        let cancel = CancellationToken::new();
        let cont = SpliceContinuity::default();
        let transcoder = None;
        let (events, events_rx) = crate::manager::events::event_channel();
        let media_stats = Arc::new(MediaPlayerStats::default());
        (tx, stats, cancel, cont, transcoder, events, events_rx, media_stats)
    }

    #[test]
    fn pacer_lateness_latch_fires_lagging_then_recovered() {
        let (tx, stats, cancel, mut cont, mut transcoder, events, mut events_rx, media_stats) =
            test_session_harness();
        let mut seq_num: u16 = 0;
        let mut post = None;
        let mut demux_cache = DemuxCacheField::default();
        let session = PlayerSession {
            seq_num: &mut seq_num,
            per_input_tx: &tx,
            stats: &stats,
            cancel: &cancel,
            cont: &mut cont,
            transcoder: &mut transcoder,
            pid_overrides: None,
            post: &mut post,
            splice_gap_signal: None,
            bundle_size: BUNDLE_SIZE,
            media_stats: &media_stats,
            events: &events,
            flow_id: "test-flow",
            input_id: "test-input",
            demux_cache: &mut demux_cache,
        };

        // Below the warn threshold: no transition, no event.
        media_stats.pacer_lateness_current_ms.store(50, Ordering::Relaxed);
        poll_pacer_lateness_latch(&session);
        assert!(!media_stats.pacer_lagging.load(Ordering::Relaxed));
        assert!(events_rx.try_recv().is_err(), "must not fire below the warn threshold");

        // Crosses the warn threshold: latches lagging, fires Warning with
        // the stable error_code.
        media_stats.pacer_lateness_current_ms.store(300, Ordering::Relaxed);
        poll_pacer_lateness_latch(&session);
        assert!(media_stats.pacer_lagging.load(Ordering::Relaxed));
        assert_eq!(
            media_stats.state.load(Ordering::Relaxed),
            media_player_state::STALLED
        );
        let ev = events_rx.try_recv().expect("must fire on warn-threshold crossing");
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(
            ev.details.as_ref().and_then(|d| d["error_code"].as_str()),
            Some("media_player_pacer_lagging")
        );

        // Still lagging (between clear and warn thresholds): latch holds,
        // no duplicate event.
        media_stats.pacer_lateness_current_ms.store(150, Ordering::Relaxed);
        poll_pacer_lateness_latch(&session);
        assert!(media_stats.pacer_lagging.load(Ordering::Relaxed));
        assert!(events_rx.try_recv().is_err(), "latch must not re-fire while still lagging");

        // Drops at/below the clear threshold: latch clears, fires Info
        // recovered event.
        media_stats.pacer_lateness_current_ms.store(0, Ordering::Relaxed);
        poll_pacer_lateness_latch(&session);
        assert!(!media_stats.pacer_lagging.load(Ordering::Relaxed));
        assert_eq!(
            media_stats.state.load(Ordering::Relaxed),
            media_player_state::PLAYING
        );
        let ev = events_rx.try_recv().expect("must fire on recovery");
        assert_eq!(ev.severity, EventSeverity::Info);
        assert_eq!(
            ev.details.as_ref().and_then(|d| d["error_code"].as_str()),
            Some("media_player_pacer_recovered")
        );
    }

    #[test]
    fn pcr_parser_decodes_known_packet() {
        // Adaptation field with PCR base = 1, ext = 0 → 27 MHz value 300.
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[3] = 0b00100000; // adaptation_field_control = 10
        pkt[4] = 7; // af_len
        pkt[5] = 0x10; // PCR flag
        // base = 1 → byte 9 = 0x80 (low bit of base shifts into bit 7)
        pkt[6] = 0;
        pkt[7] = 0;
        pkt[8] = 0;
        pkt[9] = 0;
        pkt[10] = 0x80;
        pkt[11] = 0;
        let pcr = extract_pcr_27mhz(&pkt).expect("must parse PCR");
        assert_eq!(pcr, 300, "base=1, ext=0 → 1*300+0 = 300");
    }

    #[test]
    fn pcr_parser_skips_packets_without_pcr() {
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[3] = 0b00010000; // payload only
        assert!(extract_pcr_27mhz(&pkt).is_none());
    }

    #[test]
    fn shuffle_preserves_indices() {
        let mut a = vec![0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        shuffle_indices(&mut a);
        a.sort();
        assert_eq!(a, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn resolve_media_path_rejects_traversal() {
        assert!(resolve_media_path("../etc/passwd").is_err());
        assert!(resolve_media_path("foo/bar").is_err());
        assert!(resolve_media_path("foo\\bar").is_err());
        assert!(resolve_media_path("good-name.ts").is_ok());
    }

    #[test]
    fn classify_io_not_found_is_missing() {
        let io = std::io::Error::from(std::io::ErrorKind::NotFound);
        let err = anyhow::Error::from(io).context("open /tmp/nope.ts");
        assert_eq!(classify_playback_error(&err), "media_player_source_missing");
    }

    #[test]
    fn classify_io_permission_denied_is_missing() {
        let io = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let err = anyhow::Error::from(io).context("open /root/locked.ts");
        assert_eq!(classify_playback_error(&err), "media_player_source_missing");
    }

    #[test]
    fn classify_io_invalid_data_is_unsupported() {
        let io = std::io::Error::from(std::io::ErrorKind::InvalidData);
        let err = anyhow::Error::from(io).context("read foo.mp4");
        assert_eq!(classify_playback_error(&err), "media_player_source_unsupported");
    }

    #[test]
    fn classify_other_io_is_render_failed() {
        let io = std::io::Error::from(std::io::ErrorKind::TimedOut);
        let err = anyhow::Error::from(io).context("read foo.ts");
        assert_eq!(classify_playback_error(&err), "media_player_source_render_failed");
    }

    #[test]
    fn classify_no_sync_byte_is_unsupported() {
        let err = anyhow!(
            "TS file has no sync byte in 1 MiB of slop — file is not MPEG-TS or is severely corrupt"
        );
        assert_eq!(classify_playback_error(&err), "media_player_source_unsupported");
    }

    #[test]
    fn classify_fragmented_mp4_is_unsupported() {
        let err = anyhow!(
            "fragmented MP4 (fMP4 / moof) is not supported by the media player: /media/clip.mp4 \
             stores its coded frames in movie-fragment boxes the demuxer cannot address"
        );
        assert_eq!(classify_playback_error(&err), "media_player_source_unsupported");
    }

    #[test]
    fn classify_feature_gate_is_codec_unsupported() {
        let err = anyhow!(
            "media-player MP4 source requires the 'media-codecs' and 'fdk-aac' features — rebuild the edge with those enabled, or use a pre-encoded .ts file instead"
        );
        assert_eq!(
            classify_playback_error(&err),
            "media_player_source_codec_unsupported"
        );
    }

    #[test]
    fn classify_unrecognised_falls_back_to_generic() {
        let err = anyhow!("something we did not anticipate");
        assert_eq!(classify_playback_error(&err), "media_player_source_failed");
    }

    // ── Smooth-splice helpers ───────────────────────────────────────────

    /// Build a minimal 188-byte TS packet with PID + payload-flag set so
    /// `rewrite_cc` can be tested in isolation.
    fn ts_payload_pkt(pid: u16) -> [u8; TS_PACKET] {
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[1] = ((pid >> 8) as u8) & 0x1F;
        pkt[2] = pid as u8;
        // afc = 01 (payload only), CC starts at 0xF (so wrap is observable)
        pkt[3] = 0x10 | 0x0F;
        pkt
    }

    #[test]
    fn rewrite_cc_advances_per_pid_independently() {
        let mut cont = SpliceContinuity::default();
        // Two different PIDs: each gets its own running CC.
        let mut a = ts_payload_pkt(0x100);
        let mut b = ts_payload_pkt(0x101);
        let mut a2 = ts_payload_pkt(0x100);

        rewrite_cc(&mut a, &mut cont).expect("video pid first emit");
        rewrite_cc(&mut b, &mut cont).expect("audio pid first emit");
        rewrite_cc(&mut a2, &mut cont).expect("video pid second emit");

        assert_eq!(a[3] & 0x0F, 0x0, "first emit on a new PID is CC=0");
        assert_eq!(b[3] & 0x0F, 0x0, "first emit on a new PID is CC=0");
        assert_eq!(a2[3] & 0x0F, 0x1, "second emit on a known PID is CC=prev+1");
    }

    #[test]
    fn rewrite_cc_skips_null_pid_and_payloadless_packets() {
        let mut cont = SpliceContinuity::default();

        // Null PID: skipped regardless of payload flag.
        let mut null_pkt = ts_payload_pkt(0x1FFF);
        assert!(rewrite_cc(&mut null_pkt, &mut cont).is_none());

        // afc = 10 (adaptation only, no payload): CC must NOT advance.
        let mut af_only = ts_payload_pkt(0x100);
        af_only[3] = 0x20 | 0x0F; // afc=10, original CC=15
        assert!(rewrite_cc(&mut af_only, &mut cont).is_none());
        assert_eq!(af_only[3] & 0x0F, 0x0F, "low nibble untouched");
    }

    #[test]
    fn rewrite_cc_continues_across_pseudo_file_boundary() {
        // Simulate two consecutive `play_*_file` calls sharing the same
        // continuity state — the wire CC must be continuous across the
        // implicit boundary, even though each "file" emits packets
        // starting from the same PID with arbitrary internal CCs.
        let mut cont = SpliceContinuity::default();

        for _ in 0..18 {
            let mut p = ts_payload_pkt(0x100);
            rewrite_cc(&mut p, &mut cont);
        }
        // After 18 emits, last CC on the wire must be (18 - 1) mod 16 = 1.
        assert_eq!(*cont.last_cc.get(&0x100).unwrap(), 1);

        // "New file" — the source's internal CC happens to be 8, but
        // we keep going from 2 on the wire.
        let mut next = ts_payload_pkt(0x100);
        next[3] = 0x10 | 0x08;
        let cc = rewrite_cc(&mut next, &mut cont).unwrap();
        assert_eq!(cc, 2);
        assert_eq!(next[3] & 0x0F, 2);
    }

    #[test]
    fn rewrite_pcr_round_trip_with_zero_offset_is_a_noop() {
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[3] = 0b00100000; // afc=10 (adaptation only)
        pkt[4] = 7;
        pkt[5] = 0x10; // PCR flag
        // base = 12_345_678, ext = 234
        let base: u64 = 12_345_678;
        let ext: u64 = 234;
        pkt[6] = (base >> 25) as u8;
        pkt[7] = (base >> 17) as u8;
        pkt[8] = (base >> 9) as u8;
        pkt[9] = (base >> 1) as u8;
        pkt[10] = (((base & 1) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 1);
        pkt[11] = ext as u8;

        let pcr_before = extract_pcr_27mhz(&pkt).unwrap();
        let touched = rewrite_pcr_in_place(&mut pkt, 0);
        let pcr_after = extract_pcr_27mhz(&pkt).unwrap();

        assert!(touched);
        assert_eq!(pcr_before, pcr_after);
        assert_eq!(pcr_after, base * 300 + ext);
    }

    #[test]
    fn rewrite_pcr_applies_signed_offset_with_wrap() {
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[3] = 0b00110000; // afc=11 (adaptation + payload)
        pkt[4] = 7;
        pkt[5] = 0x10;
        // start at PCR_27m = 100
        pkt[6] = 0;
        pkt[7] = 0;
        pkt[8] = 0;
        pkt[9] = 0;
        pkt[10] = 0x7E; // base bit 0 = 0, ext bit 8 = 0
        pkt[11] = 100;
        // forward by 500 → 600
        rewrite_pcr_in_place(&mut pkt, 500);
        assert_eq!(extract_pcr_27mhz(&pkt).unwrap(), 600);
        // backward by 700 → wraps to PCR_RANGE - 100 (still positive)
        rewrite_pcr_in_place(&mut pkt, -700);
        const PCR_RANGE: u64 = (1u64 << 33) * 300;
        assert_eq!(extract_pcr_27mhz(&pkt).unwrap(), PCR_RANGE - 100);
    }

    #[test]
    fn rewrite_pes_timestamps_round_trips_signed_offset_through_wrap() {
        // Build a PUSI=1 video TS packet with a synthetic PES header.
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID=0
        pkt[2] = 0x00;
        pkt[3] = 0x10; // afc=01 (payload only)
        // PES header at byte 4
        pkt[4] = 0x00;
        pkt[5] = 0x00;
        pkt[6] = 0x01;
        pkt[7] = 0xE0; // stream_id = video
        pkt[8] = 0x00;
        pkt[9] = 0x10; // PES_packet_length (low byte; high stays 0)
        pkt[10] = 0x80; // flags1
        pkt[11] = 0xC0; // flags2: PTS+DTS present
        pkt[12] = 0x0A; // PES_header_data_length
        let pts: u64 = 90_000;
        let dts: u64 = 89_100;
        write_pes_ts_in_place(&mut pkt[13..18], pts);
        // Cheat: marker_bits not yet set on writeback; force them.
        pkt[13] = (0x03 << 4) | (pkt[13] & 0x0F);
        write_pes_ts_in_place(&mut pkt[18..23], dts);
        pkt[18] = (0x01 << 4) | (pkt[18] & 0x0F);

        // Apply +5_000_000 offset.
        let new_pts = rewrite_pes_timestamps_in_place(&mut pkt, 5_000_000).unwrap();
        assert_eq!(new_pts, pts + 5_000_000);
        assert_eq!(read_pes_ts(&pkt[13..18]), pts + 5_000_000);
        assert_eq!(read_pes_ts(&pkt[18..23]), dts + 5_000_000);

        // Wrap test: PTS near the top of the 33-bit space, plus a forward
        // offset, must wrap back into the field.
        let big_pts: u64 = (1 << 33) - 100;
        write_pes_ts_in_place(&mut pkt[13..18], big_pts);
        pkt[13] = (0x03 << 4) | (pkt[13] & 0x0F);
        write_pes_ts_in_place(&mut pkt[18..23], big_pts);
        pkt[18] = (0x01 << 4) | (pkt[18] & 0x0F);
        rewrite_pes_timestamps_in_place(&mut pkt, 200);
        // big_pts + 200 mod 2^33 = 100
        assert_eq!(read_pes_ts(&pkt[13..18]), 100);
        assert_eq!(read_pes_ts(&pkt[18..23]), 100);
    }

    #[test]
    fn rewrite_pes_timestamps_skips_non_pusi_and_padding_streams() {
        let mut cont = SpliceContinuity::default();
        let _ = &mut cont; // silence unused warning if test is reordered

        // PUSI=0: must skip.
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[1] = 0x01; // no PUSI, PID high
        pkt[3] = 0x10;
        assert!(rewrite_pes_timestamps_in_place(&mut pkt, 99).is_none());

        // PUSI=1 but stream_id=0xBE (padding): must skip.
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[1] = 0x40;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x00;
        pkt[6] = 0x01;
        pkt[7] = 0xBE; // padding stream
        assert!(rewrite_pes_timestamps_in_place(&mut pkt, 99).is_none());
    }

    #[test]
    fn try_set_discontinuity_indicator_lights_existing_af_flag() {
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[3] = 0b00110000; // afc=11 (adaptation + payload)
        pkt[4] = 7;
        pkt[5] = 0x10; // PCR flag set, disc-indicator clear
        assert!(try_set_discontinuity_indicator(&mut pkt));
        assert_eq!(pkt[5] & 0x80, 0x80, "disc-indicator now set");
        assert_eq!(pkt[5] & 0x10, 0x10, "PCR flag preserved");
    }

    #[test]
    fn try_set_discontinuity_indicator_skips_payload_only_packets() {
        // afc=01 packets carry no AF — flag has nowhere to go. Must
        // return false (the caller picks a later packet).
        let mut pkt = [0u8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[3] = 0x10;
        assert!(!try_set_discontinuity_indicator(&mut pkt));
    }

    /// `detect_ts_packet_size_in_buf` recognises 188-byte canonical TS,
    /// 192-byte M2TS (4-byte timestamp prefix), and 204-byte DVB
    /// (16-byte Reed-Solomon parity suffix). Anything inconclusive
    /// falls through to 188 so the resync loop handles malformed input
    /// the same way as before.
    #[test]
    fn detect_stride_188() {
        let mut buf = vec![0xAAu8; 188 * 8];
        for i in 0..8 {
            buf[i * 188] = SYNC_BYTE;
        }
        assert_eq!(detect_ts_packet_size_in_buf(&buf), 188);
    }

    #[test]
    fn detect_stride_192_m2ts() {
        let mut buf = vec![0xAAu8; 192 * 8];
        for i in 0..8 {
            // M2TS layout: 4-byte timestamp, then sync, then 187 body bytes.
            buf[i * 192 + 4] = SYNC_BYTE;
        }
        assert_eq!(detect_ts_packet_size_in_buf(&buf), 192);
    }

    #[test]
    fn detect_stride_204_dvb() {
        let mut buf = vec![0xAAu8; 204 * 8];
        for i in 0..8 {
            // DVB layout: sync + 187 body, then 16 parity bytes (suffix).
            buf[i * 204] = SYNC_BYTE;
        }
        assert_eq!(detect_ts_packet_size_in_buf(&buf), 204);
    }

    #[test]
    fn detect_stride_falls_back_to_188_on_garbage() {
        let buf = vec![0xAAu8; 1024];
        assert_eq!(detect_ts_packet_size_in_buf(&buf), 188);
    }

    #[test]
    fn detect_stride_rejects_lone_sync_byte() {
        // A single 0x47 in random data must not be misclassified as 188.
        let mut buf = vec![0xAAu8; 1024];
        buf[10] = SYNC_BYTE;
        assert_eq!(detect_ts_packet_size_in_buf(&buf), 188);
    }

    /// End-to-end test for 204-byte (DVB) TS playback. Builds a fixture
    /// that mirrors a professional DVB capture — every 188-byte TS packet
    /// is followed by 16 bytes of (synthetic) Reed-Solomon parity — and
    /// asserts the player drains it without losing packets and CC stays
    /// continuous (i.e. the trailer skip is correctly aligned, not just
    /// "every other packet" via the resync fallback).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn play_ts_file_handles_204_byte_dvb_packets() {
        use crate::engine::rtmp::ts_mux::TsMuxer;
        use std::collections::HashMap;
        use std::io::Write;

        // Build a 188-byte TS source first.
        let mut mux = TsMuxer::new();
        mux.set_has_video(true);
        mux.set_video_stream_type(0x1B);
        let mut ts188: Vec<u8> = Vec::new();
        for i in 0..30u64 {
            let pts = (i + 1) * 3_000;
            let dts = pts;
            let is_keyframe = i == 0;
            let nal_type: u8 = if is_keyframe { 0x65 } else { 0x41 };
            let annex_b = vec![0x00, 0x00, 0x00, 0x01, nal_type, 0xAB, 0xCD];
            for pkt in mux.mux_video(&annex_b, pts, dts, is_keyframe) {
                ts188.extend_from_slice(&pkt);
            }
        }
        assert!(ts188.len() % TS_PACKET == 0);

        // Wrap into 204-byte packets: TS188 + 16 bytes of synthetic parity.
        let mut ts204: Vec<u8> = Vec::with_capacity(ts188.len() / 188 * 204);
        for chunk in ts188.chunks(TS_PACKET) {
            ts204.extend_from_slice(chunk);
            ts204.extend_from_slice(&[0xDEu8; 16]); // any non-0x47 parity
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dvb-fixture.ts");
        std::fs::File::create(&path).unwrap().write_all(&ts204).unwrap();

        let (tx, mut rx) = broadcast::channel::<RtpPacket>(2048);
        let stats = std::sync::Arc::new(FlowStatsAccumulator::new(
            "test-flow".into(),
            "test-flow-name".into(),
            "media_player".into(),
        ));
        let cancel = CancellationToken::new();
        let mut seq_num: u16 = 0;
        let mut cont = SpliceContinuity::default();
        let (events, _events_rx) = crate::manager::events::event_channel();
        let media_stats = std::sync::Arc::new(MediaPlayerStats::default());

        cont.open_file("dvb-fixture.ts");
        let mut transcoder: Option<crate::engine::input_transcode::InputTranscoder> = None;
        let mut demux_cache = DemuxCacheField::default();
        {
            let mut session = PlayerSession {
                seq_num: &mut seq_num,
                per_input_tx: &tx,
                stats: &stats,
                cancel: &cancel,
                cont: &mut cont,
                transcoder: &mut transcoder,
                pid_overrides: None,
                post: &mut None,
                splice_gap_signal: None,
                bundle_size: BUNDLE_SIZE,
                media_stats: &media_stats,
                events: &events,
                flow_id: "test-flow",
                input_id: "test-input",
                demux_cache: &mut demux_cache,
            };
            play_ts_file(&path, None, None, &mut session).await.unwrap();
        }

        drop(tx);
        let mut bundles: Vec<RtpPacket> = Vec::new();
        while let Ok(p) = rx.try_recv() {
            bundles.push(p);
        }
        assert!(!bundles.is_empty(), "must emit at least one bundle");

        // Walk the wire and assert CC continuity on every PID. If the
        // 204→188 stride handling were broken (e.g. losing every other
        // packet via the resync fallback), CC would skip every other
        // value and trip the assertion.
        let mut last_cc: HashMap<u16, u8> = HashMap::new();
        let mut total_packets = 0usize;
        for bundle in &bundles {
            for ts_pkt in bundle.data.chunks(TS_PACKET) {
                if ts_pkt.len() != TS_PACKET || ts_pkt[0] != SYNC_BYTE {
                    continue;
                }
                total_packets += 1;
                let pid = (((ts_pkt[1] & 0x1F) as u16) << 8) | ts_pkt[2] as u16;
                let afc = (ts_pkt[3] >> 4) & 0b11;
                let has_payload = afc == 0b01 || afc == 0b11;
                if has_payload && pid != 0x1FFF {
                    let cc = ts_pkt[3] & 0x0F;
                    if let Some(&prev) = last_cc.get(&pid) {
                        let expected = (prev + 1) & 0x0F;
                        assert_eq!(
                            cc, expected,
                            "CC discontinuity on PID 0x{pid:04X} — \
                             204→188 stride handling lost a packet"
                        );
                    }
                    last_cc.insert(pid, cc);
                }
            }
        }
        // Every TS packet from the source should reach the wire. Some
        // bundles may include a partial flush so we expect ≥ source.
        let source_packet_count = ts188.len() / TS_PACKET;
        assert!(
            total_packets >= source_packet_count,
            "expected ≥ {source_packet_count} packets through the wire, got {total_packets} \
             — 204-byte stride handling is dropping packets"
        );
    }

    #[test]
    fn open_close_round_trip_advances_target_by_guard() {
        let mut cont = SpliceContinuity::default();
        cont.open_file("a.ts");
        // Cold start: no flag — there's no preceding stream to be
        // discontinuous from.
        assert!(!cont.pending_discontinuity);

        cont.close_file(100_000);
        assert_eq!(cont.last_emitted_output_pts_90k, 100_000);
        assert_eq!(
            cont.next_target_output_pts_90k,
            100_000 + SPLICE_GUARD_TICKS_90K
        );
        assert!(cont.has_played_at_least_one_file);

        // Self-loop on the same source: no flag (CC + PTS rewriting is
        // sufficient — the max-PTS anchor keeps the wire-PCR timeline
        // monotonic across loops without needing DI=1).
        cont.open_file("a.ts");
        assert!(!cont.pending_discontinuity);

        cont.close_file(200_000);

        // Real transition: flag must arm.
        cont.open_file("b.mp4");
        assert!(cont.pending_discontinuity);
    }

    /// Regression for issue #68: a playlist transition to a different
    /// source must not inherit the previous source's converged pacer
    /// bitrate. `SpliceContinuity` persists for the whole playlist, so a
    /// bare (unscoped) read of `last_converged_bitrate_bps` would leak
    /// `a.mp4`'s rate into `b.mp4`'s pacer startup, starting the new file
    /// at the wrong rate right at the boundary.
    #[test]
    fn carried_bitrate_is_scoped_to_the_source_that_produced_it() {
        let mut cont = SpliceContinuity::default();

        // No source has ever converged — nothing carries.
        assert_eq!(cont.carried_bitrate_for("a.mp4"), None);

        cont.set_converged_bitrate("a.mp4", 4_800_000);
        // Same source, e.g. a loop: carries.
        assert_eq!(cont.carried_bitrate_for("a.mp4"), Some(4_800_000));
        // Different source, e.g. a playlist transition: must NOT carry,
        // even though `last_converged_bitrate_bps` is still populated.
        assert_eq!(cont.carried_bitrate_for("b.mp4"), None);
        assert_eq!(cont.last_converged_bitrate_bps, Some(4_800_000));

        // After b.mp4 converges, a.mp4 no longer carries either — the
        // scope always tracks the single most recent source, not a set.
        cont.set_converged_bitrate("b.mp4", 7_200_000);
        assert_eq!(cont.carried_bitrate_for("b.mp4"), Some(7_200_000));
        assert_eq!(cont.carried_bitrate_for("a.mp4"), None);
    }

    #[test]
    fn update_layout_bumps_pmt_version_only_on_real_change() {
        let mut cont = SpliceContinuity::default();
        let l1 = StreamLayout {
            video_pid: Some(0x100),
            video_stream_type: Some(0x1B),
            audio_pid: Some(0x101),
            audio_stream_type: Some(0x0F),
        };
        assert!(cont.update_layout(l1.clone()), "first call is always 'changed'");
        assert_eq!(cont.pmt_version, 1);

        // No change: version stays at 1.
        assert!(!cont.update_layout(l1.clone()));
        assert_eq!(cont.pmt_version, 1);

        // Real change (audio drop): version bumps.
        let l2 = StreamLayout {
            audio_pid: None,
            audio_stream_type: None,
            ..l1
        };
        assert!(cont.update_layout(l2));
        assert_eq!(cont.pmt_version, 2);
    }

    #[test]
    fn update_layout_wraps_pmt_version_at_5_bits() {
        let mut cont = SpliceContinuity::default();
        cont.pmt_version = 31;
        let l1 = StreamLayout {
            video_pid: Some(0x100),
            video_stream_type: Some(0x1B),
            audio_pid: None,
            audio_stream_type: None,
        };
        cont.update_layout(l1);
        assert_eq!(cont.pmt_version, 0, "wrap mod 32");
    }

    /// End-to-end smoke for the smooth-splice path: build a synthetic
    /// MPEG-TS stream via the project's own `TsMuxer`, write it to a
    /// tempfile, then drive it through `play_ts_file` *twice* with a
    /// shared `SpliceContinuity` (the way `run()` does on a loop).
    /// Capture every emitted bundle and assert that:
    ///
    ///   - On-wire continuity counters advance by exactly 1 (mod 16) per
    ///     PID, with **no reset** at the loop boundary — this is the
    ///     load-bearing guarantee the user asked us to deliver.
    ///   - On-wire video PTS values are **strictly monotonic** across
    ///     the loop boundary — no jump backward.
    ///
    /// This catches regressions where `rewrite_cc` / `rewrite_pcr` /
    /// `rewrite_pes_timestamps_in_place` get unwired, or where
    /// `SpliceContinuity::close_file` / `open_file` lose state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn play_ts_file_loop_keeps_cc_and_pts_continuous() {
        use crate::engine::rtmp::ts_mux::TsMuxer;
        use std::collections::HashMap;
        use std::io::Write;

        // ── Build a tiny TS source: 30 video frames, 1 IDR every 30 ───
        // Tiny 7-byte H.264 NAL — bytes don't have to be valid, the
        // muxer doesn't decode, just packetises them. The TS structure
        // (PAT / PMT / PCR / PES + PTS) is what we care about.
        let mut mux = TsMuxer::new();
        mux.set_has_video(true);
        mux.set_video_stream_type(0x1B);
        let mut bytes: Vec<u8> = Vec::new();
        for i in 0..30u64 {
            let pts = (i + 1) * 3_000;
            let dts = pts;
            let is_keyframe = i == 0;
            let nal_type: u8 = if is_keyframe { 0x65 } else { 0x41 };
            let annex_b = vec![0x00, 0x00, 0x00, 0x01, nal_type, 0xAB, 0xCD];
            for pkt in mux.mux_video(&annex_b, pts, dts, is_keyframe) {
                bytes.extend_from_slice(&pkt);
            }
        }
        assert!(bytes.len() % TS_PACKET == 0, "fixture must be 188-aligned");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loop-fixture.ts");
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();

        // ── Set up minimal player session ─────────────────────────────
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(2048);
        let stats = std::sync::Arc::new(FlowStatsAccumulator::new(
            "test-flow".into(),
            "test-flow-name".into(),
            "media_player".into(),
        ));
        let cancel = CancellationToken::new();
        let mut seq_num: u16 = 0;
        let mut cont = SpliceContinuity::default();
        let mut transcoder: Option<crate::engine::input_transcode::InputTranscoder> = None;
        let (events, _events_rx) = crate::manager::events::event_channel();
        let media_stats = std::sync::Arc::new(MediaPlayerStats::default());
        let mut demux_cache = DemuxCacheField::default();

        // Iteration 1 — cold start.
        cont.open_file("loop-fixture.ts");
        {
            let mut session = PlayerSession {
                seq_num: &mut seq_num,
                per_input_tx: &tx,
                stats: &stats,
                cancel: &cancel,
                cont: &mut cont,
                transcoder: &mut transcoder,
                pid_overrides: None,
                post: &mut None,
                splice_gap_signal: None,
                bundle_size: BUNDLE_SIZE,
                media_stats: &media_stats,
                events: &events,
                flow_id: "test-flow",
                input_id: "test-input",
                demux_cache: &mut demux_cache,
            };
            play_ts_file(&path, None, None, &mut session).await.unwrap();
        }

        // Iteration 2 — same file, simulating a loop.
        cont.open_file("loop-fixture.ts");
        {
            let mut session = PlayerSession {
                seq_num: &mut seq_num,
                per_input_tx: &tx,
                stats: &stats,
                cancel: &cancel,
                cont: &mut cont,
                transcoder: &mut transcoder,
                pid_overrides: None,
                post: &mut None,
                splice_gap_signal: None,
                bundle_size: BUNDLE_SIZE,
                media_stats: &media_stats,
                events: &events,
                flow_id: "test-flow",
                input_id: "test-input",
                demux_cache: &mut demux_cache,
            };
            play_ts_file(&path, None, None, &mut session).await.unwrap();
        }

        // ── Drain bundles and validate on-wire structure ──────────────
        drop(tx); // close so try_recv eventually returns Closed
        let mut bundles: Vec<RtpPacket> = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(pkt) => bundles.push(pkt),
                Err(_) => break,
            }
        }
        assert!(
            !bundles.is_empty(),
            "the player must have emitted at least one bundle"
        );

        let mut last_cc: HashMap<u16, u8> = HashMap::new();
        let mut last_video_pts: Option<u64> = None;
        let mut total_ts_packets: usize = 0;

        for bundle in &bundles {
            for ts_pkt in bundle.data.chunks(TS_PACKET) {
                if ts_pkt.len() != TS_PACKET || ts_pkt[0] != SYNC_BYTE {
                    continue;
                }
                total_ts_packets += 1;
                let pid =
                    (((ts_pkt[1] & 0x1F) as u16) << 8) | ts_pkt[2] as u16;
                let afc = (ts_pkt[3] >> 4) & 0b11;
                let has_payload = afc == 0b01 || afc == 0b11;

                // CC continuity check (skipping null PID and AF-only packets).
                if has_payload && pid != 0x1FFF {
                    let cc = ts_pkt[3] & 0x0F;
                    if let Some(&prev) = last_cc.get(&pid) {
                        let expected = (prev + 1) & 0x0F;
                        assert_eq!(
                            cc, expected,
                            "CC discontinuity across loop on PID 0x{pid:04X}: prev={prev} cur={cc}"
                        );
                    }
                    last_cc.insert(pid, cc);
                }

                // PTS monotonicity check on the video PID. Looking for
                // PUSI=1 packets that start a PES with PTS_DTS_flags set.
                let pusi = (ts_pkt[1] & 0x40) != 0;
                if !pusi || pid != 0x0100 {
                    continue;
                }
                let payload_start = match afc {
                    0b01 => 4,
                    0b11 => 4 + 1 + ts_pkt[4] as usize,
                    _ => continue,
                };
                if payload_start + 14 > TS_PACKET {
                    continue;
                }
                if ts_pkt[payload_start] != 0
                    || ts_pkt[payload_start + 1] != 0
                    || ts_pkt[payload_start + 2] != 1
                {
                    continue;
                }
                let flags2 = ts_pkt[payload_start + 7];
                if (flags2 >> 6) & 0b11 == 0 {
                    continue;
                }
                let pts_off = payload_start + 9;
                let pts = read_pes_ts(&ts_pkt[pts_off..pts_off + 5]);
                if let Some(prev) = last_video_pts {
                    assert!(
                        pts > prev,
                        "video PTS regression across loop: prev={prev} cur={pts}"
                    );
                }
                last_video_pts = Some(pts);
            }
        }

        assert!(
            total_ts_packets > 0,
            "must have walked at least one TS packet"
        );
        assert!(
            last_video_pts.is_some(),
            "must have observed at least one video PES with PTS"
        );
        // The continuity state should have observed both files cleanly.
        assert!(cont.has_played_at_least_one_file);
    }

    /// Synthesise a single-packet PMT carrying one video + one audio ES and
    /// confirm that the audio PID is added to the set.
    fn synth_pmt(pmt_pid: u16, program_no: u16, entries: &[(u8, u16, &[u8])]) -> [u8; TS_PACKET] {
        let mut pkt = [0xFFu8; TS_PACKET];
        pkt[0] = SYNC_BYTE;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F); // PUSI=1
        pkt[2] = pmt_pid as u8;
        pkt[3] = 0x10; // afc=01, CC=0
        pkt[4] = 0; // pointer_field
        let body_start = 5;
        pkt[body_start] = 0x02; // table_id
        let mut es_loop_bytes = Vec::new();
        for (st, pid, desc) in entries {
            es_loop_bytes.push(*st);
            es_loop_bytes.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
            es_loop_bytes.push(*pid as u8);
            let dl = desc.len();
            es_loop_bytes.push(0xF0 | ((dl >> 8) as u8 & 0x0F));
            es_loop_bytes.push(dl as u8);
            es_loop_bytes.extend_from_slice(desc);
        }
        let section_len = 9 + es_loop_bytes.len() + 4; // header(9) + es loop + CRC(4)
        pkt[body_start + 1] = 0xB0 | ((section_len >> 8) as u8 & 0x0F);
        pkt[body_start + 2] = section_len as u8;
        pkt[body_start + 3] = (program_no >> 8) as u8;
        pkt[body_start + 4] = program_no as u8;
        pkt[body_start + 5] = 0xC1; // version=0, current_next=1
        pkt[body_start + 6] = 0; // section_number
        pkt[body_start + 7] = 0; // last_section_number
        pkt[body_start + 8] = 0xE0; // PCR_PID high (we use video PID)
        pkt[body_start + 9] = 0x00;
        pkt[body_start + 10] = 0xF0; // program_info_length = 0
        pkt[body_start + 11] = 0x00;
        let es_start = body_start + 12;
        pkt[es_start..es_start + es_loop_bytes.len()].copy_from_slice(&es_loop_bytes);
        // Don't bother with real CRC — refresh_audio_pids_from_pmt doesn't
        // verify it. Fill the 4 CRC bytes with zero.
        let crc_start = es_start + es_loop_bytes.len();
        for i in 0..4 {
            pkt[crc_start + i] = 0;
        }
        pkt
    }

    #[test]
    fn pmt_audio_pid_discovery_picks_up_mp2_and_ac3() {
        // BSkyB Sky Witness HD shape: MP2 (0x04) + AC-3 carried as private
        // (0x06) with AC-3 descriptor 0x6A. Video PID 0x0203 (H.264 0x1B).
        let entries: &[(u8, u16, &[u8])] = &[
            (0x1B, 0x0203, &[]),           // H.264 video — not audio
            (0x04, 0x0283, &[]),           // MPEG-2 audio
            (0x06, 0x0297, &[0x6A, 0x01, 0x00]), // AC-3 via DVB descriptor
            (0x06, 0x0241, &[0x56, 0x01, 0x00]), // teletext via DVB 0x56 — not audio
        ];
        let pkt = synth_pmt(0x0106, 3928, entries);
        let mut out: HashSet<u16> = HashSet::new();
        refresh_audio_pids_from_pmt(&pkt, &mut out);
        assert!(out.contains(&0x0283), "MP2 PID should be detected as audio");
        assert!(out.contains(&0x0297), "AC-3-via-0x06+0x6A should be detected as audio");
        assert!(!out.contains(&0x0203), "video PID must not be flagged as audio");
        assert!(!out.contains(&0x0241), "teletext PID must not be flagged as audio");
    }

    #[test]
    fn pmt_audio_pid_discovery_handles_aac_and_eac3() {
        // AAC-LATM (0x11) + E-AC-3 via DVB (0x06 + 0x7A descriptor).
        let entries: &[(u8, u16, &[u8])] = &[
            (0x11, 0x0100, &[]),
            (0x06, 0x0101, &[0x7A, 0x01, 0x00]),
        ];
        let pkt = synth_pmt(0x1000, 1, entries);
        let mut out: HashSet<u16> = HashSet::new();
        refresh_audio_pids_from_pmt(&pkt, &mut out);
        assert!(out.contains(&0x0100));
        assert!(out.contains(&0x0101));
    }

    #[test]
    fn pmt_audio_pid_discovery_clears_stale_pids_across_calls() {
        // A second PMT carrying only one of the audio PIDs replaces the
        // set wholesale — stale audio PIDs from prior PMT versions don't
        // accumulate.
        let mut out: HashSet<u16> = HashSet::new();

        let pkt1 = synth_pmt(0x1000, 1, &[(0x04, 0x0100, &[]), (0x81, 0x0101, &[])]);
        refresh_audio_pids_from_pmt(&pkt1, &mut out);
        assert!(out.contains(&0x0100));
        assert!(out.contains(&0x0101));

        let pkt2 = synth_pmt(0x1000, 1, &[(0x04, 0x0100, &[])]);
        refresh_audio_pids_from_pmt(&pkt2, &mut out);
        assert!(out.contains(&0x0100));
        assert!(!out.contains(&0x0101), "removed audio PID must not linger");
    }

}
