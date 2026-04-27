// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Recording writer: drains a flow's broadcast channel, writes
//! 188-byte-aligned MPEG-TS segments to disk, and maintains the
//! side-car timecode → byte-offset index.
//!
//! # Architecture
//!
//! Two tasks per recording, in series:
//!
//! 1. **Subscriber task** (`subscriber_loop`) — `broadcast::Receiver`,
//!    drops on `RecvError::Lagged`, never blocks the data path. Try-sends
//!    into a bounded mpsc; if the mpsc is full, drops the packet, bumps
//!    a counter, and emits a (rate-limited) Critical event under
//!    category `replay` with `error_code: replay_writer_lagged`.
//!
//! 2. **Writer task** (`writer_loop`) — drains the mpsc, parses TS
//!    packets out of each `RtpPacket`, appends them to the current
//!    segment file under `<recording_dir>/.tmp/`, tracks PCR for the
//!    index, runs SMPTE TC extraction via [`TimecodeTracker`], and rolls
//!    segments on wall-time. On roll: fsync the segment, atomic-rename
//!    out of `.tmp/`, fsync the index, run retention prune.
//!
//! Mirrors the slow-consumer pattern in `engine::output_udp` — slow disk
//! never propagates back to the broadcast channel.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RecordingConfig;
use crate::engine::content_analysis::timecode::TimecodeTracker;
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};

use super::clips::ClipStore;
use super::index::{IndexEntry, IndexWriter, flag};
use super::{MarkInAck, RecordingCommand};

/// 188-byte MPEG-TS packet size.
const TS_PACKET: usize = 188;
/// `RecordingStats.mode` discriminants. Match the [`WriterMode`] variants
/// plus the post-Stop "no pre-buffer" branch that reports as `MODE_IDLE`.
pub const MODE_IDLE: u8 = 0;
pub const MODE_PRE_BUFFER: u8 = 1;
pub const MODE_ARMED: u8 = 2;

/// Map a [`RecordingStats::mode`] discriminant to the canonical wire
/// string used by `RecordingSnapshot.mode` and the `recording_status` WS
/// response. Unknown discriminants resolve to `"idle"` to keep the
/// wire-shape forward-compatible if a future variant is added.
pub fn mode_to_wire_str(mode: u8) -> &'static str {
    match mode {
        MODE_PRE_BUFFER => "pre_buffer",
        MODE_ARMED => "armed",
        _ => "idle",
    }
}
/// MPEG-TS sync byte.
const SYNC_BYTE: u8 = 0x47;
/// Bounded queue between the subscriber and the writer task. Sized so a
/// brief disk hiccup doesn't immediately drop, but a sustained stall
/// still surfaces the `replay_writer_lagged` event before unbounded
/// memory growth.
const WRITER_QUEUE_CAP: usize = 1024;
/// Minimum gap between `replay_writer_lagged` events when the writer is
/// continuously dropping. Avoids flooding the events feed.
const LAG_EVENT_INTERVAL: Duration = Duration::from_secs(5);

/// Live, atomic counters for one recording. Read by the manager-WS
/// stats poller via the FlowRuntime.
#[derive(Debug, Default)]
pub struct RecordingStats {
    pub segments_written: AtomicU64,
    pub bytes_written: AtomicU64,
    pub segments_pruned: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub index_entries: AtomicU64,
    pub current_pts_90khz: AtomicU64,
    /// Wall-clock Unix milliseconds of the most recent successful TS
    /// append. The manager uses this as a freshness signal — if `armed`
    /// is true but this value stops advancing while the WS connection
    /// is healthy, the recorder has stalled. `0` until the first write.
    pub last_write_unix_ms: AtomicU64,
    /// Set to true on first sample, false after `stop_recording`.
    pub armed: AtomicBool,
    /// Externally-visible writer state. Mirrors the writer's internal
    /// [`WriterMode`] plus the post-Stop "no pre-buffer" branch which
    /// reports as `MODE_IDLE`. Drives the manager UI's tri-state badge
    /// (`Recording` / `Pre-roll` / `Idle`) and the flow-card
    /// `● PRE-ROLL` chip — operators on a pre-buffered flow need to see
    /// that the recorder is rolling pre-roll TS even though `armed` is
    /// `false`. `0` = idle, `1` = pre-buffer, `2` = armed; legacy
    /// readers that don't know the field can ignore it without loss.
    pub mode: AtomicU8,
}

/// `recording.json` shape — small, append-only metadata sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordingMeta {
    /// Schema version, in case future writers extend the format.
    schema_version: u32,
    /// Edge-side recording ID (matches the directory name).
    recording_id: String,
    /// Created-at as Unix seconds.
    created_at_unix: u64,
    /// Configured rolling segment duration.
    segment_seconds: u32,
    /// Current segment ID (next NNNNNN.ts to write).
    current_segment_id: u32,
}

/// Public handle returned by [`spawn_writer`]. Owns the JoinHandle for
/// the writer task; cloning the `command_tx` lets the WS dispatcher send
/// commands to the writer.
pub struct RecordingHandle {
    pub recording_id: String,
    /// Atomic stats counters — exposed on the FlowStats snapshot path
    /// (wired via FlowRuntime). Held for refcount.
    #[allow(dead_code)]
    pub stats: Arc<RecordingStats>,
    pub command_tx: mpsc::Sender<RecordingCommand>,
    /// Per-recording cancel token. Cancelling the parent flow token
    /// cascades to this child, so explicit cancel is rarely needed.
    #[allow(dead_code)]
    pub cancel: CancellationToken,
    /// Writer task handle — held for ownership; shutdown is driven by
    /// [`Self::cancel`] (or its parent), not by aborting the handle.
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
}

/// Spawn the subscriber + writer task pair for a recording. Returns
/// the handle the FlowRuntime stores. Errors from setup (directory
/// creation, index open, recording.json write) are surfaced
/// synchronously — the caller is expected to lift them onto a
/// `command_ack.error_code` if it's a manager-driven create.
pub async fn spawn_writer(
    flow_id: String,
    config: RecordingConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    events: EventSender,
    parent_cancel: CancellationToken,
) -> Result<RecordingHandle> {
    let recording_id = config.storage_id.clone().unwrap_or_else(|| flow_id.clone());
    let dir = super::recording_dir(&recording_id);
    tokio::fs::create_dir_all(&dir).await
        .with_context(|| format!("create recording dir {}", dir.display()))?;
    let staging = dir.join(".tmp");
    tokio::fs::create_dir_all(&staging).await
        .with_context(|| format!("create recording staging {}", staging.display()))?;

    // Crash-recovery scan. If the edge was SIGKILL'd mid-segment, three
    // forms of leftover state can survive into the next start:
    //   * Orphan `.tmp/<NNNNNN>.ts` partial segments — never atomically
    //     renamed onto the main directory, so they're not part of the
    //     recording but still consume disk.
    //   * A `recording.json` whose `current_segment_id` is stale or the
    //     file is truncated/corrupt — would cause segment-id reuse and
    //     overwrite finalized segments on the next roll.
    //   * An `index.bin` truncated mid-entry — handled inside
    //     `IndexWriter::open` / `InMemoryIndex::load`, not here.
    // Scan the directory once: unlink `.tmp/*.ts` orphans and derive the
    // largest finalized segment id so the next roll picks `max + 1`
    // regardless of what the meta file says. This is also the recovery
    // path when `recording.json` deserialise fails.
    let recovered_orphans = recover_tmp_orphans(&staging).await;
    let max_finalized_id = scan_max_segment_id(&dir).await;

    // Write or update recording.json. Existing recording = resume.
    let meta_path = dir.join("recording.json");
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let meta_existed = tokio::fs::try_exists(&meta_path).await.unwrap_or(false);
    let existing_meta: Option<RecordingMeta> = if meta_existed {
        match tokio::fs::read(&meta_path).await {
            Ok(b) => serde_json::from_slice::<RecordingMeta>(&b).ok(),
            Err(_) => None,
        }
    } else {
        None
    };
    let meta_corrupt = meta_existed && existing_meta.is_none();
    // Pick the next segment id by walking the directory: trust whatever
    // is on disk over what `recording.json` claims, because the meta
    // write is best-effort on the roll path. `meta.current_segment_id`
    // is the *just-finalized* id, so the resume id is `max(disk, meta) + 1`
    // — except on a fresh recording where both are 0 and we start at 0.
    let meta_seg = existing_meta.as_ref().map(|m| m.current_segment_id);
    let starting_segment = match (max_finalized_id, meta_seg) {
        (Some(disk), Some(meta)) => disk.max(meta).saturating_add(1),
        (Some(disk), None) => disk.saturating_add(1),
        (None, Some(meta)) if existing_meta.is_some() => meta,
        _ => 0,
    };
    if recovered_orphans > 0 || meta_corrupt
        || max_finalized_id.zip(meta_seg).map(|(d, m)| d > m).unwrap_or(false)
    {
        events.emit_with_details(
            EventSeverity::Warning,
            category::REPLAY,
            format!(
                "Recording '{recording_id}' recovered after restart: {recovered_orphans} \
                 .tmp orphan(s) cleaned, resuming at segment {starting_segment}\
                 {}",
                if meta_corrupt { ", recording.json was corrupt" } else { "" }
            ),
            None,
            serde_json::json!({
                "error_code": "replay_recovery_alert",
                "replay_event": "recovery_alert",
                "recording_id": recording_id,
                "tmp_orphans_removed": recovered_orphans,
                "meta_corrupt": meta_corrupt,
                "next_segment_id": starting_segment,
            }),
        );
    }
    let meta = RecordingMeta {
        schema_version: 1,
        recording_id: recording_id.clone(),
        created_at_unix: existing_meta.map(|m| m.created_at_unix).unwrap_or(now_unix),
        segment_seconds: config.segment_seconds,
        current_segment_id: starting_segment,
    };
    write_meta_atomic(&meta_path, &meta).await?;

    // Phase 2.2 — derive initial mode from the config. With
    // `pre_buffer_seconds` set, the writer auto-arms in `PreBuffer`
    // mode (operator hasn't pressed Start yet); otherwise the writer
    // starts in `Armed` mode (Phase 1 behaviour: the writer being
    // spawned at all means the recording session is live). `armed` on
    // the stats snapshot tracks the recording-session boolean only —
    // pre-buffer is intentionally not "armed" so the manager UI and
    // stall detector can distinguish the two states.
    let initial_mode = if config.pre_buffer_seconds.is_some() {
        WriterMode::PreBuffer
    } else {
        WriterMode::Armed
    };
    let stats = Arc::new(RecordingStats::default());
    stats.armed.store(matches!(initial_mode, WriterMode::Armed), Ordering::Relaxed);
    stats.mode.store(
        match initial_mode {
            WriterMode::PreBuffer => MODE_PRE_BUFFER,
            WriterMode::Armed => MODE_ARMED,
        },
        Ordering::Relaxed,
    );

    let cancel = parent_cancel.child_token();
    let (writer_tx, writer_rx) = mpsc::channel::<Bytes>(WRITER_QUEUE_CAP);
    let (cmd_tx, cmd_rx) = mpsc::channel::<RecordingCommand>(32);

    // Subscriber task: forwards Bytes onto the writer mpsc, with
    // drop-on-full + Critical event semantics.
    {
        let stats_sub = stats.clone();
        let events_sub = events.clone();
        let cancel_sub = cancel.clone();
        let recording_id_sub = recording_id.clone();
        let mut rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            subscriber_loop(
                &mut rx, writer_tx, stats_sub, events_sub, cancel_sub, recording_id_sub,
            ).await;
        });
    }

    let clips = ClipStore::open(&recording_id, &dir).await?;
    let index_path = dir.join("index.bin");
    let index_writer = IndexWriter::open(&index_path).await?;

    let writer_state = WriterState {
        recording_id: recording_id.clone(),
        dir,
        meta_path,
        meta,
        config: config.clone(),
        stats: stats.clone(),
        events: events.clone(),
        clips,
        index_writer,
        current_segment: None,
        current_segment_started: None,
        disk_pressure_emitted: false,
        accumulated_pts: 0,
        last_pcr: None,
        pending_pcr_discontinuity: false,
        timecode: TimecodeTracker::new(),
        in_point: Arc::new(Mutex::new(None)),
        mode: initial_mode,
    };

    let join = tokio::spawn(async move {
        writer_loop(writer_state, writer_rx, cmd_rx, cancel.clone()).await;
    });

    Ok(RecordingHandle {
        recording_id,
        stats,
        command_tx: cmd_tx,
        cancel: parent_cancel.child_token(),
        join,
    })
}

async fn subscriber_loop(
    rx: &mut broadcast::Receiver<RtpPacket>,
    writer_tx: mpsc::Sender<Bytes>,
    stats: Arc<RecordingStats>,
    events: EventSender,
    cancel: CancellationToken,
    recording_id: String,
) {
    let mut last_lag_event: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            recv = rx.recv() => match recv {
                Ok(pkt) => {
                    if writer_tx.try_send(pkt.data.clone()).is_err() {
                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                        let now = Instant::now();
                        let should_emit = last_lag_event.map(|t| now.duration_since(t) > LAG_EVENT_INTERVAL).unwrap_or(true);
                        if should_emit {
                            last_lag_event = Some(now);
                            events.emit_with_details(
                                EventSeverity::Critical,
                                category::REPLAY,
                                format!(
                                    "Recording '{recording_id}' writer queue full — packet dropped"
                                ),
                                None,
                                serde_json::json!({
                                    "error_code": "replay_writer_lagged",
                                    "replay_event": "writer_lagged",
                                    "recording_id": recording_id,
                                }),
                            );
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// Writer arming state. Phase 2.2 split.
///
/// * `Stopped` — no pre-buffer configured and the operator hasn't
///   started a recording session. The writer task isn't even spawned
///   in this mode (the FlowRuntime skips `spawn_writer`).
/// * `PreBuffer` — pre-buffer is configured (`recording.pre_buffer_seconds.is_some()`)
///   and the operator hasn't pressed Start. Segments roll to disk with
///   retention pinned at `pre_buffer_seconds`. `RecordingStats.armed`
///   stays false so the manager UI distinguishes pre-roll from a
///   recording session, and the manager's stall detector doesn't
///   trigger (it gates on `armed`).
/// * `Armed` — recording session active. Retention bumps to the full
///   `recording.retention_seconds`. Existing pre-buffer segments
///   become the head of the recording. `armed` flips true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterMode {
    PreBuffer,
    Armed,
}

struct WriterState {
    recording_id: String,
    dir: PathBuf,
    meta_path: PathBuf,
    meta: RecordingMeta,
    config: RecordingConfig,
    stats: Arc<RecordingStats>,
    events: EventSender,
    clips: ClipStore,
    index_writer: IndexWriter,
    current_segment: Option<OpenSegment>,
    current_segment_started: Option<Instant>,
    /// Sticky bit so the 80 % disk-pressure Warning fires once per
    /// pressure episode rather than every segment roll. Cleared via
    /// hysteresis at 70 % so a recovered system can re-warn later.
    disk_pressure_emitted: bool,
    accumulated_pts: u64,
    last_pcr: Option<u64>,
    /// Sticky bit set when an unusually large PCR step is observed.
    /// Cleared by the next IDR after stamping `flag::PCR_DISCONTINUITY`
    /// onto its index entry — readers use this to avoid wallclock-pacing
    /// across a stream-source change.
    pending_pcr_discontinuity: bool,
    timecode: TimecodeTracker,
    in_point: Arc<Mutex<Option<u64>>>,
    /// Current arming state. Drives retention selection in
    /// [`WriterState::effective_retention_seconds`] and the meaning of
    /// the Start / Stop commands. See [`WriterMode`].
    mode: WriterMode,
}

struct OpenSegment {
    id: u32,
    file: File,
    bytes_written: u64,
    staging_path: PathBuf,
    final_path: PathBuf,
    saw_first_idr: bool,
}

async fn writer_loop(
    mut state: WriterState,
    mut rx: mpsc::Receiver<Bytes>,
    mut cmd_rx: mpsc::Receiver<RecordingCommand>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                state.flush_and_close().await;
                state.stats.armed.store(false, Ordering::Relaxed);
                state.stats.mode.store(MODE_IDLE, Ordering::Relaxed);
                return;
            }
            cmd = cmd_rx.recv() => match cmd {
                Some(c) => state.handle_command(c).await,
                None => {
                    state.flush_and_close().await;
                    state.stats.armed.store(false, Ordering::Relaxed);
                    state.stats.mode.store(MODE_IDLE, Ordering::Relaxed);
                    return;
                }
            },
            data = rx.recv() => match data {
                Some(buf) => {
                    if let Err(e) = state.write_chunk(&buf).await {
                        tracing::error!(target: "replay", "writer error: {e:#}");
                        state.events.emit_with_details(
                            EventSeverity::Critical,
                            category::REPLAY,
                            format!("Recording '{}' write error: {e}", state.recording_id),
                            None,
                            serde_json::json!({
                                "error_code": "replay_disk_full",
                                "replay_event": "disk_full",
                                "recording_id": state.recording_id,
                            }),
                        );
                        // Stop writing on error. The recording stays armed
                        // so subsequent commands surface a meaningful state
                        // rather than a generic "send to closed channel" — but
                        // future packets are silently dropped until the
                        // operator stops/restarts. Best-effort close the
                        // partial segment so we don't leave a half-finished
                        // .tmp/<id>.ts on disk; on ENOSPC the close itself
                        // will likely fail too, so unlink the staging path
                        // as a fallback. Either way the file handle is
                        // released here, not when the writer task exits.
                        if let Some(seg) = state.current_segment.take() {
                            let staging = seg.staging_path.clone();
                            if close_segment(seg).await.is_err() {
                                let _ = tokio::fs::remove_file(&staging).await;
                            }
                        }
                    }
                }
                None => {
                    state.flush_and_close().await;
                    state.stats.armed.store(false, Ordering::Relaxed);
                    state.stats.mode.store(MODE_IDLE, Ordering::Relaxed);
                    return;
                }
            }
        }
    }
}

impl WriterState {
    /// Write one chunk of broadcast bytes to the current segment.
    /// Each chunk is the `data` field of one `RtpPacket` — for our
    /// usage this is either raw MPEG-TS (`is_raw_ts: true`) or
    /// RTP-wrapped TS. We assume raw TS in the simple Phase 1 path
    /// (assembled and passthrough flows produce raw TS); the RTP-wrap
    /// case is handled by skipping the 12-byte RTP header.
    async fn write_chunk(&mut self, buf: &Bytes) -> Result<()> {
        let ts = strip_rtp_if_present(buf);
        if ts.is_empty() || ts.len() % TS_PACKET != 0 {
            // Mis-sized or non-TS payload — don't poison the segment.
            // Audio-only PCM flows would land here; recording is a no-op
            // for them in Phase 1.
            return Ok(());
        }
        // Roll segment if wall time exceeded.
        let need_roll = match self.current_segment_started {
            None => true,
            Some(started) => started.elapsed().as_secs() >= self.config.segment_seconds as u64,
        };
        if need_roll {
            self.roll_segment().await?;
        }
        // Per-TS-packet processing: PCR + timecode + collect index entries.
        let mut offset_in_seg = self.current_segment.as_ref().map(|s| s.bytes_written).unwrap_or(0);
        let mut new_entries: Vec<IndexEntry> = Vec::new();
        for chunk in ts.chunks_exact(TS_PACKET) {
            if let Some(entry) = self.observe_packet(chunk, offset_in_seg as u32) {
                new_entries.push(entry);
            }
            offset_in_seg += TS_PACKET as u64;
        }
        if let Some(seg) = self.current_segment.as_mut() {
            seg.file.write_all(ts).await
                .with_context(|| format!("write to segment {} of {}", seg.id, self.recording_id))?;
            seg.bytes_written += ts.len() as u64;
            self.stats.bytes_written.fetch_add(ts.len() as u64, Ordering::Relaxed);
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            self.stats.last_write_unix_ms.store(now_ms, Ordering::Relaxed);
        }
        // Best-effort index append — failure here is a soft error (the
        // index is rebuildable from segments alone), so we log and
        // continue rather than killing the writer.
        for entry in new_entries {
            if self.index_writer.append(entry).await.is_ok() {
                self.stats.index_entries.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn observe_packet(&mut self, ts: &[u8], byte_offset_in_seg: u32) -> Option<IndexEntry> {
        debug_assert_eq!(ts.len(), TS_PACKET);
        if ts[0] != SYNC_BYTE {
            return None;
        }
        let pusi = (ts[1] & 0x40) != 0;
        let adaptation_field_control = (ts[3] >> 4) & 0x3;
        let mut payload_start = 4;
        let mut random_access = false;
        let mut pcr_field: Option<u64> = None;
        if adaptation_field_control == 0x2 || adaptation_field_control == 0x3 {
            // Adaptation field present.
            let af_len = ts[4] as usize;
            if af_len > 0 && 4 + 1 + af_len <= TS_PACKET {
                let af = &ts[5..5 + af_len];
                if !af.is_empty() {
                    let flags = af[0];
                    random_access = (flags & 0x40) != 0;
                    let pcr_flag = (flags & 0x10) != 0;
                    if pcr_flag && af.len() >= 1 + 6 {
                        // PCR is 33-bit base * 300 + 9-bit extension.
                        let base: u64 = ((af[1] as u64) << 25)
                            | ((af[2] as u64) << 17)
                            | ((af[3] as u64) << 9)
                            | ((af[4] as u64) << 1)
                            | (((af[5] as u64) >> 7) & 0x1);
                        let ext: u64 = (((af[5] as u64) & 0x01) << 8) | (af[6] as u64);
                        // 27 MHz total → 90 kHz (divide by 300).
                        let pcr_27 = base * 300 + ext;
                        pcr_field = Some(pcr_27 / 300);
                    }
                }
            }
            payload_start = 5 + af_len;
        }
        // Update accumulated PTS from PCR. Wraps every ~26 hours at 33-bit
        // base; accumulate into a 64-bit pseudo-PTS so the index stays
        // monotonic across the wrap. If the per-step delta is suspiciously
        // large (> 5 min of 90 kHz ticks) we treat it as a stream
        // discontinuity rather than a real elapsed gap — could be an
        // explicit PCR reset (live → file transition, encoder restart) or
        // a corrupted PCR field. The next IDR's index entry gets the
        // `PCR_DISCONTINUITY` flag so playback can avoid wallclock-pacing
        // across the boundary.
        const PCR_DISCONTINUITY_THRESHOLD_90KHZ: u64 = 90_000 * 60 * 5; // 5 minutes
        if let Some(new_pcr) = pcr_field {
            let wrap_modulus: u64 = 1u64 << 33;
            let delta = match self.last_pcr {
                None => 0u64,
                Some(prev) => new_pcr.wrapping_sub(prev) & (wrap_modulus - 1),
            };
            if delta > PCR_DISCONTINUITY_THRESHOLD_90KHZ {
                self.pending_pcr_discontinuity = true;
                self.accumulated_pts = self.accumulated_pts.wrapping_add(1);
            } else {
                self.accumulated_pts = self.accumulated_pts.wrapping_add(delta);
            }
            self.last_pcr = Some(new_pcr);
            self.stats.current_pts_90khz.store(self.accumulated_pts, Ordering::Relaxed);
        }

        // Feed the timecode tracker (filters internally; harmless on non-video).
        if payload_start < TS_PACKET && (adaptation_field_control == 0x1 || adaptation_field_control == 0x3) {
            self.timecode.observe_ts(pusi, &ts[payload_start..]);
        }

        // Index every random-access point we have a PCR for.
        if random_access && self.current_segment.as_ref().map(|s| !s.saw_first_idr).unwrap_or(false) {
            // First random-access of segment — promote to the canonical
            // index entry for this segment.
            if let Some(seg) = self.current_segment.as_mut() {
                seg.saw_first_idr = true;
            }
        }
        if random_access {
            let snap = self.timecode.snapshot();
            let tc_packed = smpte_tc_packed(snap.as_ref());
            let pcr_disc = std::mem::replace(&mut self.pending_pcr_discontinuity, false);
            let entry = IndexEntry {
                pts_90khz: self.accumulated_pts,
                smpte_tc: tc_packed.unwrap_or(0xFFFFFFFF),
                segment_id: self.current_segment.as_ref().map(|s| s.id).unwrap_or(0),
                byte_offset: byte_offset_in_seg,
                flags: flag::IS_IDR
                    | if tc_packed.is_some() { flag::SMPTE_TC_VALID } else { 0 }
                    | if pcr_disc { flag::PCR_DISCONTINUITY } else { 0 },
            };
            return Some(entry);
        }
        None
    }

    async fn roll_segment(&mut self) -> Result<()> {
        // Close out the current segment first.
        if let Some(seg) = self.current_segment.take() {
            close_segment(seg).await?;
            self.stats.segments_written.fetch_add(1, Ordering::Relaxed);
            // Best-effort sync of the index per-segment.
            let _ = self.index_writer.flush_and_sync().await;
            self.run_retention().await;
            // Update meta with next segment id.
            self.meta.current_segment_id = self.meta.current_segment_id.wrapping_add(1);
            // Surface meta-write failures to the operator. On restart the
            // recovery scan in `WriterState::new` will derive the next
            // segment id from the directory listing, so a stale meta
            // doesn't cause segment-id reuse — but the operator should
            // still see the warning so they can investigate (typically
            // ENOSPC on the same volume that holds the segments).
            if let Err(e) = write_meta_atomic(&self.meta_path, &self.meta).await {
                tracing::warn!(target: "replay", "meta write failed: {e:#}");
                self.events.emit_with_details(
                    EventSeverity::Warning,
                    category::REPLAY,
                    format!(
                        "Recording '{}' metadata write failed: {e} (recovery scan will resume from directory listing)",
                        self.recording_id
                    ),
                    None,
                    serde_json::json!({
                        "error_code": "replay_metadata_stale",
                        "replay_event": "metadata_stale",
                        "recording_id": self.recording_id,
                        "current_segment_id": self.meta.current_segment_id,
                    }),
                );
            }
        }
        // Open the new segment.
        let id = self.meta.current_segment_id;
        let staging_path = self.dir.join(".tmp").join(format!("{:06}.ts", id));
        let final_path = self.dir.join(format!("{:06}.ts", id));
        let file = OpenOptions::new()
            .create(true).truncate(true).write(true)
            .open(&staging_path).await
            .with_context(|| format!("open staging {}", staging_path.display()))?;
        self.current_segment = Some(OpenSegment {
            id, file, bytes_written: 0, staging_path, final_path, saw_first_idr: false,
        });
        self.current_segment_started = Some(Instant::now());
        Ok(())
    }

    async fn run_retention(&mut self) {
        // Snapshot existing segments — exclude .tmp, recording.json,
        // index.bin, clips.json. Compute total size; remove oldest until
        // both retention_seconds and max_bytes are satisfied.
        let mut entries: Vec<(u32, u64, u64)> = Vec::new(); // (id, mtime_unix, size)
        let mut dir = match tokio::fs::read_dir(&self.dir).await {
            Ok(d) => d,
            Err(_) => return,
        };
        while let Ok(Some(e)) = dir.next_entry().await {
            let name = e.file_name();
            let name_str = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            // Match NNNNNN.ts
            let stem = match name_str.strip_suffix(".ts") {
                Some(s) => s,
                None => continue,
            };
            let id: u32 = match stem.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let meta = match e.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = meta
                .modified().ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            entries.push((id, mtime, meta.len()));
        }
        // Oldest first by mtime then id.
        entries.sort_by_key(|(id, m, _)| (*m, *id));
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut total: u64 = entries.iter().map(|(_, _, s)| s).sum();
        let retention = self.effective_retention_seconds();
        // The just-finalized segment id (`run_retention` is called after
        // `current_segment.take()` and *before* the id bump in
        // `roll_segment`, so `meta.current_segment_id` here is the id of
        // the segment we just closed). Never prune it — losing the most
        // recent segment would tear out clips that touch the live edge,
        // and on a fresh-after-stop_recording call the next start would
        // reuse the id and overwrite the very segment we just kept on
        // disk for the retention window.
        let protected_id = self.meta.current_segment_id;
        let mut max_bytes_unmet = false;
        for (id, mtime, size) in entries {
            let too_old = retention > 0
                && now_unix.saturating_sub(mtime) > retention;
            let too_big = self.config.max_bytes > 0 && total > self.config.max_bytes;
            if !too_old && !too_big {
                break;
            }
            if id == protected_id {
                // Can't satisfy `max_bytes` without deleting the live edge —
                // remember it and surface a Warning at the end so the
                // operator sees their cap is smaller than one segment.
                if too_big {
                    max_bytes_unmet = true;
                }
                continue;
            }
            let path = self.dir.join(format!("{:06}.ts", id));
            if tokio::fs::remove_file(&path).await.is_ok() {
                total = total.saturating_sub(size);
                self.stats.segments_pruned.fetch_add(1, Ordering::Relaxed);
            }
        }
        if max_bytes_unmet {
            self.events.emit_with_details(
                EventSeverity::Warning,
                category::REPLAY,
                format!(
                    "Recording '{}' max_bytes ({}) is smaller than one segment — \
                     retention cannot keep usage under the cap without deleting the live edge",
                    self.recording_id, self.config.max_bytes
                ),
                None,
                serde_json::json!({
                    "error_code": "replay_max_bytes_below_segment",
                    "replay_event": "max_bytes_below_segment",
                    "recording_id": self.recording_id,
                    "max_bytes": self.config.max_bytes,
                    "segment_seconds": self.config.segment_seconds,
                }),
            );
        }
        // Disk-pressure signal — emit once at 80 % of either the
        // configured per-recording cap or the filesystem free space.
        // Sticky until usage drops back below 70 % (hysteresis) so a
        // continuously-pressured recorder doesn't spam the events feed.
        let pct = self.compute_pressure_pct(total);
        match (pct, self.disk_pressure_emitted) {
            (Some(p), false) if p >= 80 => {
                self.disk_pressure_emitted = true;
                self.events.emit_with_details(
                    EventSeverity::Warning,
                    category::REPLAY,
                    format!(
                        "Recording '{}' disk usage at {p}% — free disk before recorder hits ENOSPC",
                        self.recording_id
                    ),
                    None,
                    serde_json::json!({
                        "error_code": "replay_disk_pressure",
                        "replay_event": "disk_pressure",
                        "recording_id": self.recording_id,
                        "pct": p,
                    }),
                );
            }
            (Some(p), true) if p < 70 => {
                self.disk_pressure_emitted = false;
            }
            _ => {}
        }
    }

    /// Pick the pressure percentage for the recording. When the
    /// operator set `max_bytes`, treat the cap as authoritative. When
    /// it's 0 (unlimited), fall back to the filesystem-level signal.
    fn compute_pressure_pct(&self, total_bytes_in_dir: u64) -> Option<u8> {
        if self.config.max_bytes > 0 {
            let pct = (total_bytes_in_dir.saturating_mul(100) / self.config.max_bytes).min(100);
            return Some(pct as u8);
        }
        let (free, total) = super::replay_disk_usage()?;
        if total == 0 { return None; }
        let used = total.saturating_sub(free);
        let pct = (used.saturating_mul(100) / total).min(100);
        Some(pct as u8)
    }

    async fn flush_and_close(&mut self) {
        if let Some(seg) = self.current_segment.take() {
            let _ = close_segment(seg).await;
            self.stats.segments_written.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.index_writer.flush_and_sync().await;
    }

    /// Retention window currently in force. In `Armed` mode this is the
    /// configured `recording.retention_seconds`; in `PreBuffer` mode it
    /// snaps to `recording.pre_buffer_seconds` so the on-disk window
    /// stays bounded to "the last N seconds" while the operator hasn't
    /// pressed Start. `0` always means unlimited (subject to
    /// `max_bytes` and free disk).
    fn effective_retention_seconds(&self) -> u64 {
        match self.mode {
            WriterMode::Armed => self.config.retention_seconds,
            WriterMode::PreBuffer => self
                .config
                .pre_buffer_seconds
                .map(|s| s as u64)
                .unwrap_or(self.config.retention_seconds),
        }
    }

    async fn handle_command(&mut self, cmd: RecordingCommand) {
        match cmd {
            RecordingCommand::Start { reply } => {
                let was_pre_buffer = self.mode == WriterMode::PreBuffer;
                let pre_buffered_secs = if was_pre_buffer {
                    self.config.pre_buffer_seconds.unwrap_or(0)
                } else {
                    0
                };
                self.mode = WriterMode::Armed;
                self.stats.armed.store(true, Ordering::Relaxed);
                self.stats.mode.store(MODE_ARMED, Ordering::Relaxed);
                if was_pre_buffer {
                    // Inform the manager that the recording session
                    // started with N seconds of pre-buffered TS already
                    // on disk — the UI uses this to render the
                    // recording's "true zero" earlier than `now()`.
                    self.events.emit_with_details(
                        EventSeverity::Info,
                        category::REPLAY,
                        format!(
                            "Recording '{}' armed (pre-buffer rolled in: {pre_buffered_secs} s)",
                            self.recording_id
                        ),
                        None,
                        serde_json::json!({
                            "replay_event": "recording_started",
                            "recording_id": self.recording_id,
                            "pre_buffered_seconds": pre_buffered_secs,
                        }),
                    );
                }
                let _ = reply.send(Ok(self.recording_id.clone()));
            }
            RecordingCommand::Stop { reply } => {
                let has_pre_buffer = self.config.pre_buffer_seconds.is_some();
                self.stats.armed.store(false, Ordering::Relaxed);
                if has_pre_buffer {
                    // Drop back into pre-buffer mode — keep writing,
                    // retention shrinks to `pre_buffer_seconds`.
                    // Older segments prune naturally on the next roll
                    // via the now-tighter `effective_retention_seconds`.
                    self.mode = WriterMode::PreBuffer;
                    self.stats.mode.store(MODE_PRE_BUFFER, Ordering::Relaxed);
                } else {
                    // No pre-buffer: classic stop, flush and close the
                    // current segment. The writer task stays alive until
                    // the cancel signal so subsequent commands can still
                    // get a meaningful ack.
                    self.flush_and_close().await;
                    self.stats.mode.store(MODE_IDLE, Ordering::Relaxed);
                }
                let _ = reply.send(Ok(()));
            }
            RecordingCommand::MarkIn { explicit_pts, reply } => {
                let pts = explicit_pts.unwrap_or(self.accumulated_pts);
                if pts == 0 && self.last_pcr.is_none() {
                    let _ = reply.send(Err(anyhow!("replay_recording_not_active")));
                    return;
                }
                *self.in_point.lock().await = Some(pts);
                let smpte_tc = self.timecode.snapshot().and_then(|t| t.last);
                let _ = reply.send(Ok(MarkInAck { pts_90khz: pts, smpte_tc }));
            }
            RecordingCommand::MarkOut { name, description, explicit_out_pts, created_by, reply } => {
                let in_pts_opt = *self.in_point.lock().await;
                let in_pts = match in_pts_opt {
                    Some(p) => p,
                    None => {
                        let _ = reply.send(Err(anyhow!("replay_recording_not_active")));
                        return;
                    }
                };
                let out_pts = explicit_out_pts.unwrap_or(self.accumulated_pts);
                if out_pts <= in_pts {
                    let _ = reply.send(Err(anyhow!(
                        "out_pts ({out_pts}) must be greater than in_pts ({in_pts})"
                    )));
                    return;
                }
                let smpte_in = self.timecode.snapshot().and_then(|t| t.last);
                let smpte_out = smpte_in.clone();
                let res = self.clips.create(
                    &self.recording_id, in_pts, out_pts,
                    smpte_in, smpte_out, name, description, created_by,
                    Vec::new(),
                ).await;
                if let Ok(ref clip) = res {
                    self.events.emit_with_details(
                        EventSeverity::Info,
                        category::REPLAY,
                        format!("Clip created: {} ({} ms)", clip.id, clip.duration_ms()),
                        None,
                        serde_json::json!({
                            "replay_event": "clip_created",
                            "recording_id": self.recording_id,
                            "clip": clip,
                        }),
                    );
                    *self.in_point.lock().await = None;
                }
                let _ = reply.send(res);
            }
        }
    }
}

/// Walk `<recording_dir>/.tmp/` and unlink any `<NNNNNN>.ts` files
/// left behind by a crash mid-segment. The atomic rename onto the
/// final segment path is the commit point — anything still in `.tmp/`
/// at startup is partial data the writer never published.
async fn recover_tmp_orphans(staging: &Path) -> u64 {
    let mut count = 0u64;
    let mut dir = match tokio::fs::read_dir(staging).await {
        Ok(d) => d,
        Err(_) => return 0,
    };
    while let Ok(Some(e)) = dir.next_entry().await {
        let name = e.file_name();
        let Some(name_str) = name.to_str() else { continue };
        let Some(stem) = name_str.strip_suffix(".ts") else { continue };
        if stem.parse::<u32>().is_err() {
            continue;
        }
        if tokio::fs::remove_file(e.path()).await.is_ok() {
            count += 1;
        }
    }
    count
}

/// Largest `<NNNNNN>.ts` segment id present in the main recording
/// directory, or `None` on a fresh recording. Used as the source of
/// truth for resume-after-crash because `recording.json` is best-effort
/// on the roll path.
async fn scan_max_segment_id(dir: &Path) -> Option<u32> {
    let mut max_id: Option<u32> = None;
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(d) => d,
        Err(_) => return None,
    };
    while let Ok(Some(e)) = entries.next_entry().await {
        let name = e.file_name();
        let Some(name_str) = name.to_str() else { continue };
        let Some(stem) = name_str.strip_suffix(".ts") else { continue };
        if let Ok(id) = stem.parse::<u32>() {
            max_id = Some(max_id.map_or(id, |cur| cur.max(id)));
        }
    }
    max_id
}

/// Strip the 12-byte RTP header if the buffer looks like RTP-wrapped TS
/// (first byte version = 2). Otherwise return the buffer as-is.
fn strip_rtp_if_present(buf: &Bytes) -> &[u8] {
    if buf.len() < 12 {
        return buf.as_ref();
    }
    let v = (buf[0] >> 6) & 0x03;
    if v == 2 {
        // Heuristic: RTP-over-TS payloads start with sync byte at offset 12.
        if buf.len() > 12 && buf[12] == SYNC_BYTE {
            return &buf[12..];
        }
    }
    buf.as_ref()
}

/// Pack a SMPTE TimecodeStats snapshot into a u32 bit-packed
/// HH:MM:SS:FF representation. Returns `None` if no timecode is known.
/// Format: `(HH << 24) | (MM << 16) | (SS << 8) | FF` — drop frame is
/// not flagged in Phase 1.
fn smpte_tc_packed(snap: Option<&crate::stats::models::TimecodeStats>) -> Option<u32> {
    let s = snap?.last.as_ref()?;
    let parts: Vec<&str> = s.split(|c: char| c == ':' || c == ';').collect();
    if parts.len() != 4 {
        return None;
    }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let sec: u32 = parts[2].parse().ok()?;
    let f: u32 = parts[3].parse().ok()?;
    Some((h << 24) | (m << 16) | (sec << 8) | f)
}

async fn close_segment(mut seg: OpenSegment) -> Result<()> {
    seg.file.flush().await?;
    seg.file.sync_all().await?;
    drop(seg.file);
    tokio::fs::rename(&seg.staging_path, &seg.final_path).await
        .with_context(|| format!("rename {} -> {}", seg.staging_path.display(), seg.final_path.display()))?;
    Ok(())
}

async fn write_meta_atomic(path: &PathBuf, meta: &RecordingMeta) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(meta)?;
    {
        let mut f = OpenOptions::new()
            .create(true).truncate(true).write(true)
            .open(&tmp).await?;
        f.write_all(&bytes).await?;
        f.flush().await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
