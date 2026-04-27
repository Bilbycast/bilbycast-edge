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

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Set to true on first sample, false after `stop_recording`.
    pub armed: AtomicBool,
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

    // Write or update recording.json. Existing recording = resume.
    let meta_path = dir.join("recording.json");
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let existing_meta = if tokio::fs::try_exists(&meta_path).await.unwrap_or(false) {
        match tokio::fs::read(&meta_path).await {
            Ok(b) => serde_json::from_slice::<RecordingMeta>(&b).ok(),
            Err(_) => None,
        }
    } else {
        None
    };
    let starting_segment = existing_meta.as_ref().map(|m| m.current_segment_id).unwrap_or(0);
    let meta = RecordingMeta {
        schema_version: 1,
        recording_id: recording_id.clone(),
        created_at_unix: existing_meta.map(|m| m.created_at_unix).unwrap_or(now_unix),
        segment_seconds: config.segment_seconds,
        current_segment_id: starting_segment,
    };
    write_meta_atomic(&meta_path, &meta).await?;

    let stats = Arc::new(RecordingStats::default());
    stats.armed.store(true, Ordering::Relaxed);

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
        timecode: TimecodeTracker::new(),
        in_point: Arc::new(Mutex::new(None)),
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
    timecode: TimecodeTracker,
    in_point: Arc<Mutex<Option<u64>>>,
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
                return;
            }
            cmd = cmd_rx.recv() => match cmd {
                Some(c) => state.handle_command(c).await,
                None => {
                    state.flush_and_close().await;
                    state.stats.armed.store(false, Ordering::Relaxed);
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
                        // operator stops/restarts.
                        state.current_segment = None;
                    }
                }
                None => {
                    state.flush_and_close().await;
                    state.stats.armed.store(false, Ordering::Relaxed);
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
        // monotonic across the wrap.
        if let Some(new_pcr) = pcr_field {
            let wrap_modulus: u64 = 1u64 << 33;
            let delta = match self.last_pcr {
                None => 0u64,
                Some(prev) => {
                    let raw = new_pcr.wrapping_sub(prev) & (wrap_modulus - 1);
                    raw
                }
            };
            self.accumulated_pts = self.accumulated_pts.wrapping_add(delta);
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
            let entry = IndexEntry {
                pts_90khz: self.accumulated_pts,
                smpte_tc: tc_packed.unwrap_or(0xFFFFFFFF),
                segment_id: self.current_segment.as_ref().map(|s| s.id).unwrap_or(0),
                byte_offset: byte_offset_in_seg,
                flags: flag::IS_IDR
                    | if tc_packed.is_some() { flag::SMPTE_TC_VALID } else { 0 },
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
            let _ = write_meta_atomic(&self.meta_path, &self.meta).await;
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
        for (id, mtime, size) in entries {
            let too_old = self.config.retention_seconds > 0
                && now_unix.saturating_sub(mtime) > self.config.retention_seconds;
            let too_big = self.config.max_bytes > 0 && total > self.config.max_bytes;
            if !too_old && !too_big {
                break;
            }
            let path = self.dir.join(format!("{:06}.ts", id));
            if tokio::fs::remove_file(&path).await.is_ok() {
                total = total.saturating_sub(size);
                self.stats.segments_pruned.fetch_add(1, Ordering::Relaxed);
            }
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

    async fn handle_command(&mut self, cmd: RecordingCommand) {
        match cmd {
            RecordingCommand::Start { reply } => {
                self.stats.armed.store(true, Ordering::Relaxed);
                let _ = reply.send(Ok(self.recording_id.clone()));
            }
            RecordingCommand::Stop { reply } => {
                self.stats.armed.store(false, Ordering::Relaxed);
                self.flush_and_close().await;
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
