// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! File-backed media-player input.
//!
//! Plays one or more local assets — pre-encoded MPEG-TS, MP4 / MOV / MKV
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
//! | `image` | encoder frame rate      | re-encode to H.264 + silent AAC | `video-thumbnail` + `fdk-aac` |
//!
//! The TS path is the lowest-CPU, lowest-latency option — recommended for
//! pre-baked broadcast slates. MP4 and image paths exist for operator
//! convenience (most slate assets live as MP4 in production media stores
//! and as PNG / JPEG marketing artwork).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::config::models::{MediaPlayerInputConfig, MediaPlayerSource};
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
mod image_slate;
#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
mod mp4_demux;

/// 188-byte MPEG-TS packet size.
const TS_PACKET: usize = 188;
/// MPEG-TS sync byte (every TS packet starts with this).
const SYNC_BYTE: u8 = 0x47;
/// Bundle 7 × 188 = 1316 bytes per `RtpPacket` — the standard MPEG-TS-over-RTP
/// payload framing the rest of the engine already assumes.
pub(super) const PACKETS_PER_BUNDLE: usize = 7;
pub(super) const BUNDLE_SIZE: usize = TS_PACKET * PACKETS_PER_BUNDLE;
/// Fallback when the file has no PCR and no `paced_bitrate_bps` override.
const DEFAULT_FALLBACK_BITRATE_BPS: u64 = 4_000_000;

/// Public entry point. Matches the signature shape of `input_test_pattern`
/// and `input_bonded` so the flow runtime calls it through the same
/// per-input forwarder pipeline.
pub fn spawn_media_player_input(
    config: MediaPlayerInputConfig,
    per_input_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: EventSender,
    flow_id: String,
    input_id: String,
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

async fn run(
    config: MediaPlayerInputConfig,
    per_input_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: EventSender,
    flow_id: String,
    input_id: String,
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

    let mut seq_num: u16 = 0;
    // One continuity state for the whole playlist's lifetime — carried
    // across loops and file transitions so the on-wire CC + PTS + PCR
    // sequence is continuous regardless of how many times we wrap around
    // or move between sources.
    let mut cont = SpliceContinuity::default();

    loop {
        let mut order: Vec<usize> = (0..config.sources.len()).collect();
        if config.shuffle && order.len() > 1 {
            shuffle_indices(&mut order);
        }

        for idx in order {
            if cancel.is_cancelled() {
                return;
            }
            let source = &config.sources[idx];
            let mut session = PlayerSession {
                seq_num: &mut seq_num,
                per_input_tx: &per_input_tx,
                stats: &stats,
                cancel: &cancel,
                cont: &mut cont,
            };
            let result = play_source(source, &config, &mut session).await;
            if let Err(e) = result {
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
}

/// 30 ms gap inserted between the previous file's last emitted PTS and the
/// next file's first emitted PTS. ≥ 1 video frame at every common rate
/// (24 / 25 / 30 / 50 / 60 fps), enough to keep some hardware decoders
/// from treating a same-tick splice as a stuck timestamp.
const SPLICE_GUARD_TICKS_90K: u64 = 2_700;

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
        if self.has_played_at_least_one_file && is_transition {
            self.pending_discontinuity = true;
        }
        self.last_source_id = Some(source_id.to_string());
    }

    /// Called by each per-format player at the end of a file to update
    /// the target PTS for the *next* file's first emit. `last_pts_90k`
    /// is the maximum 90 kHz PTS value emitted on the wire across both
    /// audio and video PIDs during this file.
    pub(super) fn close_file(&mut self, last_pts_90k: u64) {
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
    {
        return "media_player_source_unsupported";
    }
    if msg_lc.contains("requires the 'video-thumbnail'") {
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
        MediaPlayerSource::Ts { name } => name,
        MediaPlayerSource::Mp4 { name } => name,
        MediaPlayerSource::Image { name, .. } => name,
    }
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
        MediaPlayerSource::Ts { name } => {
            let path = resolve_media_path(name)?;
            play_ts_file(&path, cfg.paced_bitrate_bps, session).await
        }
        MediaPlayerSource::Mp4 { name } => {
            let path = resolve_media_path(name)?;
            play_mp4(&path, session).await
        }
        MediaPlayerSource::Image {
            name,
            fps,
            bitrate_kbps,
            audio_silence,
        } => {
            let path = resolve_media_path(name)?;
            play_image(&path, *fps, *bitrate_kbps, *audio_silence, session).await
        }
    }
}

// ── TS file player ───────────────────────────────────────────────────────

/// Play one MPEG-TS file end-to-end. Pacing prefers embedded PCR; if the
/// file has no PCR-bearing packets in the first second of reading, falls
/// back to `paced_bitrate_bps` (or [`DEFAULT_FALLBACK_BITRATE_BPS`] when
/// the operator left that unset).
async fn play_ts_file(
    path: &Path,
    paced_bitrate_bps: Option<u64>,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut bundle = BytesMut::with_capacity(BUNDLE_SIZE);
    let mut packet = [0u8; TS_PACKET];
    let mut first_pcr_27mhz: Option<u64> = None;
    let mut first_wall: Option<Instant> = None;
    let mut bundle_emit_count: u64 = 0;
    let bundle_start_wall = Instant::now();

    // ── Smooth-splice state ─────────────────────────────────────────────
    // The first PCR we encounter anchors the per-file splice offset:
    //   offset_27m = (target_first_output_pts × 300) − first_input_pcr
    // which is then applied to every PCR, PTS, and DTS we emit so the
    // wire timeline continues smoothly from the previous file. Pacing
    // math still uses the *raw* PCR deltas (wall-clock relative to the
    // file's own PCR baseline) — adding a constant offset preserves
    // those deltas, so pacing and rewriting are independent.
    let target_pts_90k = session.cont.next_target_output_pts_90k;
    let mut splice_offset_27m: Option<i64> = None;
    let mut max_emitted_pts_90k: u64 = target_pts_90k;

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

        // Read the raw PCR (pre-rewrite) for pacing and offset anchoring.
        let raw_pcr = extract_pcr_27mhz(&packet);

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
            match (first_pcr_27mhz, first_wall) {
                (None, _) => {
                    first_pcr_27mhz = Some(pcr);
                    first_wall = Some(Instant::now());
                }
                (Some(p0), Some(w0)) => {
                    // Discard implausible jumps so PCR resets / wraps don't
                    // park us in the future or stall.
                    let pcr_delta = pcr.wrapping_sub(p0);
                    let max_plausible_us: u64 = 60 * 60 * 6 * 1_000_000; // 6 hours
                    let target_offset_us = pcr_delta / 27;
                    if target_offset_us < max_plausible_us {
                        let target = w0 + Duration::from_micros(target_offset_us);
                        tokio::select! {
                            _ = session.cancel.cancelled() => break,
                            _ = tokio::time::sleep_until(target) => {}
                        }
                    }
                }
                _ => {}
            }
        }

        // ── Smooth-splice rewriters (in place, no allocation) ──────────
        rewrite_cc(&mut packet, session.cont);
        if let Some(off_27m) = splice_offset_27m {
            rewrite_pcr_in_place(&mut packet, off_27m);
            let off_90k = off_27m / 300;
            if let Some(new_pts) =
                rewrite_pes_timestamps_in_place(&mut packet, off_90k)
            {
                if new_pts > max_emitted_pts_90k {
                    max_emitted_pts_90k = new_pts;
                }
            }
        }
        if session.cont.pending_discontinuity
            && try_set_discontinuity_indicator(&mut packet)
        {
            session.cont.pending_discontinuity = false;
        }

        bundle.extend_from_slice(&packet);
        if bundle.len() >= BUNDLE_SIZE {
            // Pure-bitrate fallback when no PCR has been seen (rare —
            // even simple ffmpeg files include PCRs).
            if first_pcr_27mhz.is_none() {
                let bitrate = paced_bitrate_bps.unwrap_or(DEFAULT_FALLBACK_BITRATE_BPS);
                let interval_us = (BUNDLE_SIZE as u64 * 8 * 1_000_000) / bitrate.max(1);
                bundle_emit_count += 1;
                let target =
                    bundle_start_wall + Duration::from_micros(interval_us * bundle_emit_count);
                tokio::select! {
                    _ = session.cancel.cancelled() => break,
                    _ = tokio::time::sleep_until(target) => {}
                }
            }
            emit_bundle(&mut bundle, session, fake_rtp_ts(session));
        }
    }

    // Flush the trailing partial bundle so we don't lose the file's tail.
    if !bundle.is_empty() {
        emit_bundle(&mut bundle, session, fake_rtp_ts(session));
    }

    // Hand off the high-water-mark PTS to SpliceContinuity so the next
    // file's first emitted PTS picks up immediately after.
    session.cont.close_file(max_emitted_pts_90k);
    Ok(())
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
/// CC value written, or `None` if the packet doesn't advance CC (per
/// ISO/IEC 13818-1: only payload-bearing packets advance, and the
/// null-PID 0x1FFF is exempt).
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

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
async fn play_mp4(path: &Path, session: &mut PlayerSession<'_>) -> Result<()> {
    mp4_demux::play_mp4_file(path, session).await
}

#[cfg(not(all(feature = "video-thumbnail", feature = "fdk-aac")))]
async fn play_mp4(_path: &Path, _session: &mut PlayerSession<'_>) -> Result<()> {
    Err(anyhow!(
        "media-player MP4 source requires the 'video-thumbnail' and 'fdk-aac' features — rebuild the edge with those enabled, or use a pre-encoded .ts file instead"
    ))
}

#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
async fn play_image(
    path: &Path,
    fps: u8,
    bitrate_kbps: u32,
    audio_silence: bool,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    image_slate::play_image_file(path, fps, bitrate_kbps, audio_silence, session).await
}

#[cfg(not(all(feature = "video-thumbnail", feature = "fdk-aac")))]
async fn play_image(
    _path: &Path,
    _fps: u8,
    _bitrate_kbps: u32,
    _audio_silence: bool,
    _session: &mut PlayerSession<'_>,
) -> Result<()> {
    Err(anyhow!(
        "media-player image source requires the 'video-thumbnail' and 'fdk-aac' features — rebuild the edge with those enabled, or upload the slate as a pre-encoded .ts file"
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
    let data: Bytes = std::mem::replace(bundle, BytesMut::with_capacity(BUNDLE_SIZE)).freeze();
    let pkt = RtpPacket {
        data,
        sequence_number: *session.seq_num,
        rtp_timestamp: rtp_ts,
        recv_time_us: crate::util::time::now_us(),
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
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
    let _ = session.per_input_tx.send(pkt);
}

/// Fabricate an RTP timestamp from wall-clock 90 kHz. The TS file's actual
/// PTS / PCR is preserved inside the 188 B packets; the outer RTP timestamp
/// is informational for downstream stats only.
fn fake_rtp_ts(_session: &PlayerSession<'_>) -> u32 {
    let now_us = crate::util::time::now_us();
    ((now_us / 1_000) * 90).rem_euclid(0x1_0000_0000) as u32
}

// ── Order-shuffle helper ────────────────────────────────────────────────

fn shuffle_indices(order: &mut [usize]) {
    use rand::seq::SliceRandom;
    let mut rng = rand::rng();
    order.shuffle(&mut rng);
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn classify_feature_gate_is_codec_unsupported() {
        let err = anyhow!(
            "media-player MP4 source requires the 'video-thumbnail' and 'fdk-aac' features — rebuild the edge with those enabled, or use a pre-encoded .ts file instead"
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

    #[test]
    fn open_close_round_trip_advances_target_by_guard() {
        let mut cont = SpliceContinuity::default();
        cont.open_file("a.ts");
        // Cold start: no flag.
        assert!(!cont.pending_discontinuity);

        cont.close_file(100_000);
        assert_eq!(cont.last_emitted_output_pts_90k, 100_000);
        assert_eq!(
            cont.next_target_output_pts_90k,
            100_000 + SPLICE_GUARD_TICKS_90K
        );
        assert!(cont.has_played_at_least_one_file);

        // Self-loop on the same source: no flag (CC + PTS rewriting is
        // sufficient).
        cont.open_file("a.ts");
        assert!(!cont.pending_discontinuity);

        cont.close_file(200_000);

        // Real transition: flag must arm.
        cont.open_file("b.mp4");
        assert!(cont.pending_discontinuity);
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

        // Iteration 1 — cold start.
        cont.open_file("loop-fixture.ts");
        {
            let mut session = PlayerSession {
                seq_num: &mut seq_num,
                per_input_tx: &tx,
                stats: &stats,
                cancel: &cancel,
                cont: &mut cont,
            };
            play_ts_file(&path, None, &mut session).await.unwrap();
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
            };
            play_ts_file(&path, None, &mut session).await.unwrap();
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
}
