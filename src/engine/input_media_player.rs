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

        if let Some(pcr) = extract_pcr_27mhz(&packet) {
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
}
