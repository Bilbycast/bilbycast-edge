// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Recording writer: drains a flow's broadcast channel, writes
//! 188-byte-aligned MPEG-TS segments to disk, and maintains the
//! side-car timecode → byte-offset index.
//!
//! # Architecture
//!
//! Three tasks per recording, in series:
//!
//! 1. **Subscriber task** (`subscriber_loop`) — `broadcast::Receiver`,
//!    drops on `RecvError::Lagged`, never blocks the data path. Queues
//!    each message for the writer within a budget counted in bytes
//!    ([`WRITER_QUEUE_BYTES`], and past its first [`WRITER_QUEUE_FLOOR_BYTES`]
//!    within [`NODE_RECORDER_QUEUE_BYTES`] shared by every recorder on the
//!    node); past it, drops the packet, bumps a
//!    counter, and emits a (rate-limited) Critical event under category
//!    `replay` with `error_code: replay_writer_lagged`.
//!
//! 2. **Writer task** (`writer_loop`) — drains that queue, parses TS
//!    packets out of each `RtpPacket`, appends them through a userspace
//!    buffer to the current segment file under `<recording_dir>/.tmp/`,
//!    tracks PCR for the index, runs SMPTE TC extraction via
//!    [`TimecodeTracker`], and rolls segments on wall-time. Everything it
//!    does per packet is bookkeeping in memory and buffered writes; a roll
//!    hands the finished segment to the closer and opens the next.
//!
//! 3. **Closer task** (`run_closer`) — finishes segments one at a time, in
//!    the order they closed: flush and fsync the segment, rename it out of
//!    `.tmp/`, fsync the index, write `recording.json`, run the retention
//!    prune. Every fsync and directory scan on the roll path is here, off
//!    the receive path, and a disk that cannot keep up makes the writer
//!    wait for room ([`MAX_PENDING_CLOSES`]) rather than pile up staging
//!    files. Two fsyncs stay with the writer: a mark-out persists
//!    `clips.json` (the file and its directory) before it answers, and the
//!    writer's exit fsyncs the index once the closer has finished.
//!
//! Mirrors the slow-consumer pattern in `engine::output_udp` — slow disk
//! never propagates back to the broadcast channel.
//!
//! # Segment ids
//!
//! An id is handed out once in a recording's life. A segment's file, its
//! `index.bin` entries and the clips cut through them all name it by id, so
//! an id that came round again would point every one of them at media
//! recorded later. Three rules keep it so:
//!
//! - **Every close moves the id on.** `recording.json`'s
//!   `current_segment_id` is the lowest id not yet finished — the open
//!   segment's while one is open, the next to open once it has closed — and a
//!   roll, a Stop, a write error and the writer exiting all move it past the
//!   segment they close. A Start after a Stop therefore opens a new id, never
//!   the stopped one.
//! - **A writer resumes past every id anything names** ([`open_writer`]): the
//!   largest of that field, one past the largest segment on disk, and one past
//!   the largest id `index.bin` names. After a clean stop — a Stop, a flow
//!   restart, a node restart — the three agree, because the writer exits only
//!   once the closer has written the field past its last segment, so the
//!   numbering carries straight on. After a crash the field trails by every
//!   segment the closer had not finished, while the writer may already have
//!   indexed them; the index term resumes past those, so their entries name a
//!   file that is missing rather than media recorded after the restart. An id
//!   nothing names — a segment lost before its first entry — is used again.
//! - **A gap in the ids is not the end of the recording.** A crash leaves one
//!   where the segments it lost were, and so does a close that fails.
//!   Playback steps over it to the next segment on disk
//!   (`Reader::advance_segment`), and lands after it when cued or scrubbed
//!   into it; an export that needs a segment `index.bin` names there is
//!   refused with `replay_segment_missing` rather than cut short
//!   (`export::plan_pts_range`). Closing such a gap instead would mean
//!   handing the lost ids out again, and so first cutting their entries out
//!   of `index.bin` on the strength of a directory scan — and a failed close
//!   leaves a gap whatever the numbering does.
//! - **One writer per recording at a time** ([`RecordingLease`]). A writer
//!   holds its recording's directory from before its crash-recovery sweep
//!   until its closer has finished, and a writer started for the same
//!   recording meanwhile — a flow restarted while the old closer is still
//!   fsyncing on a slow disk — waits for it before it sweeps `.tmp/` or picks
//!   a resume id. Without that, the sweep unlinked the segments the old closer
//!   was still finishing, and a segment it had open with no index entry yet
//!   shared its id with the new writer's first.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex, OwnedMutexGuard, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RecordingConfig;
use crate::engine::content_analysis::timecode::TimecodeTracker;
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::util::byte_budget::{self, Charge, NodeBudget};

use super::clips::ClipStore;
use super::filmstrip::{FilmstripStats, spawn_filmstrip_writer};
use super::clock;
use super::index::{flag, InMemoryIndex, IndexEntry, IndexWriter};
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
/// What the queue between the subscriber and the writer may hold before the
/// subscriber drops: 32 MiB per recorder, and only while the writer is behind.
/// Sized so a disk that pauses for seconds costs latency rather than packets,
/// while a disk that has stopped still surfaces `replay_writer_lagged` long
/// before memory is in question.
///
/// Counted in bytes, because a message is not a fixed size. An SDI flow
/// broadcasts one 188-byte TS packet per message, about 6,900 a second at
/// 10 Mbit/s; an RTMP or RTSP flow broadcasts a whole frame per message, tens
/// of kilobytes. A cap in messages — 1024 until bilby-voyager's five recorders
/// dropped ~2.5 % of their packets at every load (2026-10-22), then 16,384 —
/// was ~150 ms and then ~2.4 s of the first, but let the second hold hundreds
/// of megabytes on a stalled disk. Each message is charged its payload plus
/// [`MESSAGE_OVERHEAD`], so in time the budget is ~17 s of that SDI feed,
/// ~25 s of 1316-byte datagrams at 10 Mbit/s and ~27 s of a 10 Mbit/s
/// frame-per-message feed.
const WRITER_QUEUE_BYTES: usize = 32 * 1024 * 1024;
/// What every recorder's queue on the node may hold between them: eight
/// recorders' full budgets.
///
/// Every recorder writes under the one replay root, so a disk that stalls
/// stalls them all, and the per-recorder budget alone let a node hold 32 MiB
/// times its recording flows at exactly that moment. Through such a stall
/// bilby-voyager's five recorders keep their whole budgets (160 MiB); a node
/// recording more flows than eight shares this between them and drops sooner.
/// Not shared with the CMAF intakes' node budget: a disk and an origin fail
/// independently, and one filling a shared budget would make the other drop
/// on a stall it would have ridden out.
const NODE_RECORDER_QUEUE_BYTES: usize = 256 * 1024 * 1024;
/// The budget every recorder's queue on the node shares.
static NODE_RECORDER_QUEUES: NodeBudget = NodeBudget::new(NODE_RECORDER_QUEUE_BYTES);
/// What of each recorder's queue is its own, charged to it and not to
/// [`NODE_RECORDER_QUEUES`]: an eighth, a second or two of a 10 Mbit/s feed.
///
/// A recorder that keeps up never holds this much, so it never asks the node
/// for anything and never drops for what other recorders hold. On the one
/// disk they usually share that costs nothing — a disk that stalls stalls them
/// all — but a recording's directory can be on a volume of its own, and a
/// writer can fall behind for reasons of its own; a node budget every byte had
/// to pass through made those recorders drop with the stalled ones. The node's
/// recorders can therefore hold [`NODE_RECORDER_QUEUE_BYTES`] plus this much
/// each.
const WRITER_QUEUE_FLOOR_BYTES: usize = WRITER_QUEUE_BYTES / 8;
/// What one queued message costs beyond its payload: its slot in the queue,
/// the shared header the broadcast's clone gave its `Bytes`, and the
/// allocator's own overhead on the payload. Negligible beside a frame, and
/// half again a 188-byte SDI packet, so counting the payload alone
/// understated what an SDI recorder's queue really held by a third.
const MESSAGE_OVERHEAD: usize = 96;
/// The queue's capacity in messages — a backstop only. It is what the byte
/// budget admits at the smallest message that records anything, one TS
/// packet, so the budget always binds first.
const WRITER_QUEUE_CAP: usize = WRITER_QUEUE_BYTES / (TS_PACKET + MESSAGE_OVERHEAD);
/// Userspace buffer in front of each segment file.
///
/// Unbuffered, every broadcast packet was its own `write_all` — for an SDI
/// flow ~6,900 round trips a second through tokio's blocking pool per
/// recorder, each awaited before the next packet left the queue. 256 KiB is
/// ~0.2 s of a 10 Mbit/s feed per write. Nothing reads a segment before it is
/// closed and renamed, and the close flushes this buffer first.
const SEGMENT_WRITE_BUF: usize = 256 * 1024;
/// Finished segments that may wait for the closer — besides the one it is
/// working on — before a roll makes the writer wait for room. The close is
/// off the receive path; this bounds how far behind it may fall, in staging
/// files, while the queue above absorbs the wait.
const MAX_PENDING_CLOSES: usize = 4;
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
    /// The lowest segment id not yet finished: the open segment's while one
    /// is open, the next NNNNNN.ts to open once it has closed. Resumed as-is
    /// — see "Segment ids" in the module docs.
    current_segment_id: u32,
    /// Wall clock at `anchor_pts_90khz`, in **microseconds** since the Unix
    /// epoch — the pair that lets a wall-clock instant be turned into a PTS.
    ///
    /// `created_at_unix` cannot do this job. It is whole seconds, and it marks
    /// when the writer opened rather than when the first frame landed:
    /// measured against the CMAF published dates over 124s of media it sat a
    /// stable +0.47s out, so a mark converted through it lands about half a
    /// second from the frame the operator chose. That is no better than
    /// cutting on segment boundaries, which is the thing exact cutting exists
    /// to avoid.
    ///
    /// Written once, when the first index entry is appended, and preserved
    /// across a writer restart. Absent on recordings made before this existed;
    /// the clip exporter then has no way to date the timeline and falls back
    /// to whole segments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor_wall_us: Option<i64>,
    /// The PTS `anchor_wall_us` describes. Stored explicitly rather than
    /// derived from the first index entry, so the pair is self-describing and
    /// a reader needs only `recording.json` to date the timeline:
    /// `wall(pts) = anchor_wall_us + (pts - anchor_pts_90khz) / 90_000`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor_pts_90khz: Option<u64>,
    /// A second, much later `(wall, pts)` sample, refreshed while the writer
    /// runs. With the anchor it gives two points on the line rather than one.
    ///
    /// One point forces an assumption the media does not honour: that the
    /// counter advances at exactly 90 000 ticks per wall-clock second. It does
    /// not — `accumulated_pts` advances on the *source's* PCR, and a source
    /// free-running against this host's clock is the normal case. The CMAF
    /// half of this same feature measured its own source at ~450 ppm slow and
    /// added a slewing epoch for exactly this reason, recording that an epoch
    /// pinned once and held drifts 4.1 s across a 2h30m session.
    ///
    /// A mark comes from wall-clock-true dates, so a single-point mapping puts
    /// the cut out by (offset × the age of the *recording*, not of the clip):
    /// 1.6 s after an hour at that rate, and unbounded on a 24/7 DVR flow —
    /// silently, because the exporter clamps rather than erroring and hands
    /// back a playable clip of the wrong moment. Two points give the measured
    /// rate instead of an assumed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recent_wall_us: Option<i64>,
    /// The PTS `recent_wall_us` describes. See [`Self::recent_wall_us`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recent_pts_90khz: Option<u64>,
}

/// How often the second `(wall, pts)` sample is re-taken.
///
/// Long enough that the rate it implies is not dominated by the arrival jitter
/// of one frame — at 60 s, a 5 ms sampling error is 83 ppm, the same order as
/// the offset being measured, and it averages out over the growing span
/// because the *anchor* end of the pair never moves. Short enough that the
/// mapping never extrapolates far past the last measurement.
const REANCHOR_INTERVAL_US: i64 = 60_000_000;

/// Wall clock as microseconds since the Unix epoch, saturating at 0.
fn now_unix_us() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

/// A gap in the recording whose length is not known until the media comes back.
///
/// Three things produce one and they are the same problem: a writer restart, an
/// operator's Stop/Start pair, and a PCR step large enough that the delta is not
/// believable as elapsed time. In all three the counter must not simply carry on
/// from where it stopped, because wall-clock time passed and the anchor maps
/// wall clock to ticks linearly — a mark placed after the gap would resolve to
/// media from before it.
///
/// Held until a PCR actually arrives, because that is the first instant at
/// which the gap's true length is known.
#[derive(Debug, Clone, Copy)]
struct PendingResume {
    /// The tick the counter must stay strictly above.
    last_indexed: u64,
    anchor_wall_us: Option<i64>,
    anchor_pts_90khz: Option<u64>,
    /// The rolling second sample, so the gap is filled at the rate the
    /// recording has actually run at — see [`super::clock`].
    recent: Option<(i64, u64)>,
}

/// The largest outage `resume_pts` will believe, in microseconds — 90 days.
///
/// Past this the anchor is not evidence, it is arithmetic about a clock that
/// was wrong: a host with no RTC boots at the Unix epoch, takes an anchor, and
/// is stepped to the real date by NTP a few seconds later. The gap that
/// implies is decades, and honouring it would place every later frame on a
/// timeline nothing can look up.
const MAX_RESUME_GAP_US: i128 = 90 * 24 * 60 * 60 * 1_000_000;

/// Where the pseudo-PTS counter must pick up when a recording is resumed.
///
/// `None` only for a fresh recording — an empty index — because nothing
/// precedes one.
///
/// **Monotonicity is unconditional and needs no anchor.** A restart appends to
/// the same `index.bin`, so a counter that restarted at zero wrote a second,
/// overlapping timeline into one file: `find_floor` binary-searches unsorted
/// data after that, and every wall-clock lookup either lands in a hole or
/// matches a frame from the previous run. That used to happen to every
/// recording made before the anchor existed, and to any whose `recording.json`
/// could not be read — the two cases where the resume is *most* needed, and the
/// two it refused. It no longer depends on the anchor: with no way to date the
/// join, the counter still advances by one tick, which is enough to keep the
/// index sorted and enough for the reader to see a discontinuity.
///
/// **The wall-clock gap is added when, and only when, there is an anchor to
/// measure it with.** Nothing was recorded during the outage, but the anchor
/// maps wall clock to ticks linearly, so a counter that resumed without the gap
/// would place everything after the restart earlier by exactly the downtime,
/// and a clip asked for by wall clock would come back showing the wrong moment.
///
/// Computed in i128 throughout. In u64 the multiply wrapped once the gap passed
/// about 6.5 years — reachable on an RTC-less board that boots at the epoch,
/// indexes a frame, and is then stepped to the real date — and a release build
/// has no overflow checks, so it produced arithmetic garbage rather than an
/// error, on a timeline that is then poisoned for the life of the recording.
/// An implausible gap is clamped away rather than believed; see
/// [`MAX_RESUME_GAP_US`].
///
/// **Filled at the recording's own rate**, through [`clock::advance_to_wall`],
/// with the same measured line the clip exporter reads. Dating the last tick
/// on the nominal 90 kHz line instead — which is what this did — re-based
/// every resume by the whole drift the source had accumulated: after a day at
/// 450 ppm a 10 s Stop/Start moved the counter 49 s, the rolling sample then
/// read that kink as the source speeding up, and a mark placed minutes before
/// the Stop resolved 39 s late.
fn resume_pts(
    last: u64,
    anchor_wall_us: Option<i64>,
    anchor_pts: Option<u64>,
    recent: Option<(i64, u64)>,
    now_us: i64,
) -> u64 {
    // No anchor: monotonic, undated. One tick is the whole claim.
    let (Some(aw), Some(ap)) = (anchor_wall_us, anchor_pts) else {
        return last.saturating_add(1);
    };
    clock::advance_to_wall(last, aw, ap, recent, now_us, MAX_RESUME_GAP_US)
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
    /// Filmstrip writer counters when the flow opted into
    /// `recording.filmstrip_seconds`. `None` when filmstrip is
    /// disabled — the manager UI gracefully skips the strip render.
    #[allow(dead_code)]
    pub filmstrip_stats: Option<Arc<FilmstripStats>>,
    pub command_tx: mpsc::Sender<RecordingCommand>,
    /// The writer task, and the filmstrip writer's when there is one.
    /// Shutdown is driven by the flow's cancel token, which the recorder's
    /// tasks run under, never by aborting these: a writer finishes the
    /// segments it has closed before it exits, and [`Self::finished`] is how
    /// a stopping flow waits for it.
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl RecordingHandle {
    /// Wait for the recorder's tasks to exit after a cancel, at most `within`
    /// for all of them. `false` if one did not.
    ///
    /// A cancelled writer is not done yet: it hands its open segment to the
    /// closer and waits for every close still pending — flush, fsync, rename
    /// — before it returns. A flow that restarted without waiting started a
    /// new writer on the same directory meanwhile, whose first act is to
    /// unlink every staging file in `.tmp/` as a crash's leftovers: the old
    /// writer's last segments, still being finished, went with them. And a
    /// process that exited without waiting abandoned them.
    ///
    /// One deadline, not one each: the writer and the filmstrip are cancelled
    /// together and write to the same volume, so on the disk that holds one
    /// up the other is held up too, and waiting for each in turn doubled the
    /// wait a stopping flow — or a node, one flow after another — spent there.
    pub async fn finished(&self, within: Duration) -> bool {
        // Taken out under the lock and awaited outside it: the lock guards a
        // hand-over, not the wait.
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        let deadline = tokio::time::Instant::now() + within;
        let mut all = true;
        for task in tasks {
            all &= tokio::time::timeout_at(deadline, task).await.is_ok();
        }
        all
    }
}

/// Spawn the subscriber + writer task pair for a recording. Returns
/// the handle the FlowRuntime stores. Errors from setup (directory
/// creation, index open, recording.json write) are surfaced
/// synchronously — the caller is expected to lift them onto a
/// `command_ack.error_code` if it's a manager-driven create — except while a
/// previous writer of the same recording is still finishing: the open then
/// waits for it in the writer task, and an error from it is raised there as
/// the same `recording_start_failed` event (see [`RecordingLease`]).
pub async fn spawn_writer(
    flow_id: String,
    config: RecordingConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    events: EventSender,
    parent_cancel: CancellationToken,
) -> Result<RecordingHandle> {
    let recording_id = config.storage_id.clone().unwrap_or_else(|| flow_id.clone());
    let dir = super::recording_dir(&recording_id);
    spawn_writer_at(flow_id, recording_id, dir, config, broadcast_tx, events, parent_cancel).await
}

/// [`spawn_writer`] into a named directory rather than the replay root's, so
/// a test can record into a directory of its own.
async fn spawn_writer_at(
    flow_id: String,
    recording_id: String,
    dir: PathBuf,
    config: RecordingConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    events: EventSender,
    parent_cancel: CancellationToken,
) -> Result<RecordingHandle> {
    // Subscribed first, so what the flow publishes while the recording is
    // opened waits in the broadcast ring rather than going unrecorded.
    let mut rx = broadcast_tx.subscribe();
    let setup = WriterSetup {
        flow_id,
        recording_id: recording_id.clone(),
        dir: dir.clone(),
        config: config.clone(),
        events: events.clone(),
        stats: Arc::default(),
        budget: Arc::default(),
    };
    let stats = setup.stats.clone();
    let budget = setup.budget.clone();
    // Opened here when nothing else holds the recording, so a failure is the
    // caller's to report. When a previous writer of it is still finishing —
    // a restart on a disk its closer is behind on — the open waits for that
    // writer in the writer task instead: the flow's start does not wait on
    // somebody else's fsyncs, and what the flow publishes meanwhile queues
    // for the writer within its budget, as it would behind a slow disk.
    let opening = match RecordingLease::try_take(&dir) {
        Ok(lease) => Opening::Ready(Box::new(open_writer(setup, lease).await?)),
        Err(held) => Opening::AfterPrevious(held, setup),
    };

    let cancel = parent_cancel.child_token();
    // Each message with the instant it came off the broadcast — see
    // `WriterState::arrived`.
    let (writer_tx, writer_rx) = mpsc::channel::<(Bytes, Instant, Charge)>(WRITER_QUEUE_CAP);
    let (cmd_tx, cmd_rx) = mpsc::channel::<RecordingCommand>(32);

    // Subscriber task: forwards Bytes onto the writer queue within its byte
    // budget, with drop-past-it + Critical event semantics.
    {
        let stats_sub = stats.clone();
        let events_sub = events.clone();
        let cancel_sub = cancel.clone();
        let recording_id_sub = recording_id.clone();
        tokio::spawn(async move {
            subscriber_loop(
                &mut rx, writer_tx, budget, stats_sub, events_sub, cancel_sub, recording_id_sub,
            ).await;
        });
    }

    // Filmstrip subscriber — opt-in via `recording.filmstrip_seconds`.
    // Sibling broadcast subscriber, drop-on-Lagged, never blocks the
    // data path. Stats are surfaced on the FlowStats recording snapshot
    // so the manager UI can render a "frames written" counter alongside
    // the segment counters.
    let (filmstrip_stats, filmstrip_join) = if let Some(secs) = config.filmstrip_seconds {
        let fstats = Arc::new(FilmstripStats::default());
        let join = spawn_filmstrip_writer(
            recording_id.clone(),
            dir,
            &broadcast_tx,
            secs,
            fstats.clone(),
            events.clone(),
            cancel.clone(),
        );
        (Some(fstats), Some(join))
    } else {
        (None, None)
    };

    let join = tokio::spawn(async move {
        let state = match opening {
            Opening::Ready(state) => *state,
            Opening::AfterPrevious(held, setup) => {
                match open_after_previous(held, setup, &cancel).await {
                    Ok(Some(state)) => state,
                    Ok(None) => return,
                    Err(failed) => {
                        // Nothing will write what the subscriber queues: it
                        // stops when it finds the queue closed.
                        drop(writer_rx);
                        refuse_commands(cmd_rx, &cancel, &failed).await;
                        return;
                    }
                }
            }
        };
        writer_loop(state, writer_rx, cmd_rx, cancel.clone()).await;
    });

    Ok(RecordingHandle {
        recording_id,
        stats,
        filmstrip_stats,
        command_tx: cmd_tx,
        tasks: std::sync::Mutex::new(std::iter::once(join).chain(filmstrip_join).collect()),
    })
}

/// What opening a recording needs, besides the [`RecordingLease`] on its
/// directory.
struct WriterSetup {
    /// The flow it records — what a failure to open it is reported against.
    flow_id: String,
    recording_id: String,
    dir: PathBuf,
    config: RecordingConfig,
    events: EventSender,
    /// Made before the open, because the flow publishes them, and the
    /// subscriber charges the queue, from the moment the recorder is spawned.
    stats: Arc<RecordingStats>,
    budget: Arc<QueueBudget>,
}

/// A recorder's open: done, or waiting on the writer before it.
enum Opening {
    Ready(Box<WriterState>),
    AfterPrevious(Arc<Mutex<()>>, WriterSetup),
}

/// Open the recording once the writer before this one has let it go — see
/// [`RecordingLease`]. `None` if the flow stops first. A failed open is
/// reported as a failure at flow start is — the same Critical
/// `recording_start_failed` against the flow — and returned, for the writer
/// to answer the flow's recording commands with.
async fn open_after_previous(
    held: Arc<Mutex<()>>,
    setup: WriterSetup,
    cancel: &CancellationToken,
) -> Result<Option<WriterState>> {
    let flow_id = setup.flow_id.clone();
    let recording_id = setup.recording_id.clone();
    let events = setup.events.clone();
    tracing::info!(
        recording_id = %recording_id,
        "replay: the recording's previous writer is still finishing its segments; \
         opening it once that writer is done"
    );
    let waiting = Instant::now();
    let guard = tokio::select! {
        _ = cancel.cancelled() => return Ok(None),
        guard = held.lock_owned() => guard,
    };
    let lease = RecordingLease::held(setup.dir.clone(), guard);
    match open_writer(setup, lease).await {
        Ok(state) => {
            tracing::info!(
                recording_id = %recording_id, waited_ms = waiting.elapsed().as_millis() as u64,
                "replay: opened the recording after its previous writer finished"
            );
            Ok(Some(state))
        }
        Err(e) => {
            events.emit_with_details(
                EventSeverity::Critical,
                category::REPLAY,
                format!("Recording failed to start for flow '{flow_id}': {e}"),
                Some(&flow_id),
                serde_json::json!({
                    "replay_event": "recording_start_failed",
                    "error_code": "replay_disk_full",
                    "recording_id": recording_id,
                    "error": e.to_string(),
                }),
            );
            Err(e)
        }
    }
}

/// Answer the flow's recording commands, until it stops, for a recorder whose
/// open failed after the flow had started: as a flow with no recorder is
/// answered, `replay_recording_not_active`.
///
/// The flow has already published this recorder and reported it started — the
/// open waited in the writer task — and keeps its handle. A writer task that
/// simply ended left that handle's commands failing with "recording writer
/// task is gone", which carries no code at all.
async fn refuse_commands(
    mut cmd_rx: mpsc::Receiver<RecordingCommand>,
    cancel: &CancellationToken,
    failed: &anyhow::Error,
) {
    let refusal = || anyhow!("replay_recording_not_active: the recording failed to start: {failed}");
    loop {
        let cmd = tokio::select! {
            _ = cancel.cancelled() => return,
            cmd = cmd_rx.recv() => cmd,
        };
        // A reply nobody waits for any more is nothing to report.
        match cmd {
            None => return,
            Some(RecordingCommand::Start { reply }) => drop(reply.send(Err(refusal()))),
            Some(RecordingCommand::Stop { reply }) => drop(reply.send(Err(refusal()))),
            Some(RecordingCommand::MarkIn { reply, .. }) => drop(reply.send(Err(refusal()))),
            Some(RecordingCommand::MarkOut { reply, .. }) => drop(reply.send(Err(refusal()))),
        }
    }
}

/// The recording directories a writer is working in, each with the lock its
/// writer holds — see [`RecordingLease`].
fn recordings_in_use() -> &'static DashMap<PathBuf, Arc<Mutex<()>>> {
    static IN_USE: OnceLock<DashMap<PathBuf, Arc<Mutex<()>>>> = OnceLock::new();
    IN_USE.get_or_init(DashMap::new)
}

/// Whether a writer is working in the recording in `dir` now: holding it, or
/// waiting on the writer before it that is.
///
/// What tells a staging file on its way from a crash's leftover — see
/// [`super::segment_state`]. A writer holds the directory from before its
/// sweep of `.tmp/` until its closer has renamed its last segment, so while it
/// does, every staging file there is one of its segments still to come.
pub(crate) fn is_being_written(dir: &Path) -> bool {
    recordings_in_use().contains_key(dir)
}

/// A hold on the recording in `dir` as its writer takes one, for the tests of
/// what a live writer changes for its readers.
#[cfg(test)]
pub(crate) fn hold_as_writer(dir: &Path) -> impl Drop + use<> {
    RecordingLease::try_take(dir).expect("another writer holds the recording")
}

/// One writer's hold on its recording directory, from before its
/// crash-recovery sweep until its closer has finished every segment and the
/// index is synced: the writer state and the closer each keep a share, so
/// the hold outlives whichever of them ends last, a panic included.
///
/// Two writers in one directory reach for the same ids and the same staging
/// files. That happens on any restart — a flow edit, a DVR session re-arming
/// the recorder — whose previous writer is still finishing on a disk its
/// closer is behind on. `FlowRuntime::stop` waits up to 10 s for it, which a
/// slow enough disk outruns, and then the new writer's sweep of `.tmp/` as a
/// crash's leftovers unlinked the segments the old closer had yet to rename,
/// and a segment the old writer had open with no index entry yet — an id the
/// resume does not count, since nothing names it — shared its id with the new
/// writer's first segment, the later rename landing on the earlier file.
///
/// A lock, not a wait with a deadline: the old closer is bounded
/// ([`MAX_PENDING_CLOSES`] segments behind at most), and giving up on it
/// would bring back exactly the race.
struct RecordingLease {
    dir: PathBuf,
    guard: Option<OwnedMutexGuard<()>>,
}

impl RecordingLease {
    /// Take `dir` if no writer holds it; otherwise the lock to wait on.
    fn try_take(dir: &Path) -> std::result::Result<Self, Arc<Mutex<()>>> {
        let lock = recordings_in_use().entry(dir.to_path_buf()).or_default().clone();
        match lock.clone().try_lock_owned() {
            Ok(guard) => Ok(Self::held(dir.to_path_buf(), guard)),
            Err(_) => Err(lock),
        }
    }

    fn held(dir: PathBuf, guard: OwnedMutexGuard<()>) -> Self {
        Self { dir, guard: Some(guard) }
    }
}

impl Drop for RecordingLease {
    fn drop(&mut self) {
        drop(self.guard.take());
        // Forgotten once nothing holds or waits on it, so the registry is as
        // long as the recordings in use, not every recording the node has
        // ever opened. Anything holding or waiting on the lock holds its
        // `Arc`, and the check runs under the map's shard lock, which a
        // writer taking the entry needs too.
        recordings_in_use().remove_if(&self.dir, |_, lock| Arc::strong_count(lock) == 1);
    }
}

/// Open — or resume — the recording in `dir`, and start its closer: the
/// writer's state, ready for packets, before any task feeds it.
async fn open_writer(setup: WriterSetup, lease: RecordingLease) -> Result<WriterState> {
    let WriterSetup { flow_id: _, recording_id, dir, config, events, stats, budget } = setup;
    let lease = Arc::new(lease);
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
    // Scan the directory once: unlink `.tmp/*.ts` orphans and find the
    // largest finalized segment id, one of the three bounds on the resume
    // id below. This is also the recovery path when `recording.json`
    // deserialise fails.
    let recovered_orphans = recover_tmp_orphans(&staging).await;
    let max_finalized_id = scan_max_segment_id(&dir).await;
    // The index, loaded before the resume id is chosen, because it names ids
    // the directory and the meta can both miss — see below. `load` aligns a
    // partial trailing entry down by itself, so it reads what
    // `IndexWriter::open` will keep. It is also the lookup mirror: post-crash
    // mark-in / mark-out for a PTS in an existing segment still resolves its
    // SMPTE TC, and new IDR appends in `write_chunk` push into it.
    let index_path = dir.join("index.bin");
    let index_mirror = InMemoryIndex::load(&index_path).await.unwrap_or_default();
    let max_indexed_id = index_mirror.entries.iter().map(|e| e.segment_id).max();

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
    // The resume id: past every id anything on disk names — see "Segment
    // ids" in the module docs.
    //
    // `current_segment_id` is the lowest id not yet finished, so it is taken
    // as it stands, never plus one: after a clean stop it is already one past
    // the last segment, and adding one more left a hole in the ids that a
    // reader took for the end of the recording. The directory and the index
    // each bound it from below. The directory, because the meta is written
    // after the rename it follows, and can be missing or corrupt. The index,
    // because the closer writes the meta only once it has finished a segment,
    // while the writer is already indexing the next — and, on a disk the
    // closer is behind on, the several after that. A crash in that window
    // leaves entries for ids neither of the others accounts for, and opening
    // one of those again would point them into media recorded after the
    // restart. A fresh recording, with none of the three, starts at 0.
    let meta_seg = existing_meta.as_ref().map(|m| m.current_segment_id);
    // `Option::max`: any id beats none.
    let max_used_id = max_finalized_id.max(max_indexed_id);
    let starting_segment = max_used_id.map(|id| id.saturating_add(1)).max(meta_seg).unwrap_or(0);
    // Something to report: a crash's leftovers swept up, a meta that could not
    // be read, or ids in use past the one the meta had reached — the segments
    // a crash lost while the closer was behind, or a meta write that failed.
    // Not an id equal to it: that is the segment a crash left open, whose
    // staging file is the orphan already counted, or one renamed into place
    // just before the meta caught up, which lost nothing. Nor a meta written
    // by a build from before every close moved the id on, which held the id
    // of the last segment rather than the next.
    if recovered_orphans > 0 || meta_corrupt
        || max_used_id.zip(meta_seg).is_some_and(|(used, meta)| used > meta)
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
        created_at_unix: existing_meta.as_ref().map(|m| m.created_at_unix).unwrap_or(now_unix),
        segment_seconds: config.segment_seconds,
        current_segment_id: starting_segment,
        // Carried across a restart: the anchor describes the media already on
        // disk, and re-taking it against a later PTS would silently shift
        // every clip cut from what came before.
        anchor_wall_us: existing_meta.as_ref().and_then(|m| m.anchor_wall_us),
        anchor_pts_90khz: existing_meta.as_ref().and_then(|m| m.anchor_pts_90khz),
        // Carried too. The rate it implies was measured over the media on
        // disk, which is the media a mark from before the restart resolves
        // against; dropping it would send every such lookup back to the
        // assumed 90 000 ticks/s the pair exists to replace.
        recent_wall_us: existing_meta.as_ref().and_then(|m| m.recent_wall_us),
        recent_pts_90khz: existing_meta.as_ref().and_then(|m| m.recent_pts_90khz),
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
    stats.armed.store(matches!(initial_mode, WriterMode::Armed), Ordering::Relaxed);
    stats.mode.store(
        match initial_mode {
            WriterMode::PreBuffer => MODE_PRE_BUFFER,
            WriterMode::Armed => MODE_ARMED,
            // Unreachable — `initial_mode` is selected above as PreBuffer
            // (when `pre_buffer_seconds` is set) or Armed (otherwise).
            // Kept exhaustive to satisfy the compiler.
            WriterMode::Idle => MODE_IDLE,
        },
        Ordering::Relaxed,
    );

    let clips = ClipStore::open(&recording_id, &dir).await?;
    let index_writer = IndexWriter::open(&index_path).await?;
    let (closer_tx, closer_rx) = mpsc::channel::<CloseJob>(MAX_PENDING_CLOSES);
    let closer = Closer {
        recording_id: recording_id.clone(),
        dir: dir.clone(),
        meta_path: meta_path.clone(),
        index: index_writer.sync_handle().await?,
        config: config.clone(),
        stats: stats.clone(),
        events: events.clone(),
        disk_pressure_emitted: false,
        _lease: lease.clone(),
    };

    // Resume the pseudo-PTS timeline; do not restart it at zero.
    //
    // `accumulated_pts` is not the stream's PTS — it is a 64-bit counter this
    // writer accumulates from PCR deltas precisely so the index stays
    // monotonic across a 33-bit wrap. A restart appends to the same index and
    // continues the segment numbering, so starting the counter again at zero
    // writes a second, overlapping timeline into one file: every wall-clock
    // lookup after a restart lands in a hole, and any that resolves can match
    // a frame from before it. That is what `replay_no_video_frames` was.
    //
    // The gap is added, not skipped. The edge was down for it, so nothing was
    // recorded — but the anchor maps wall-clock to ticks linearly, and a
    // resumed counter that ignored the outage would shift everything after it
    // earlier by exactly the downtime.
    // The gap is measured at the first frame back, not here.
    //
    // Sampling `now()` at writer start dates every later index entry early by
    // the spawn→first-frame interval — an SRT caller's reconnect, routinely a
    // few hundred milliseconds and unbounded when the source is down — and the
    // anchor is deliberately never re-taken, so that offset rides every clip
    // cut from the rest of the recording. It is the same error the anchor
    // itself is taken on the first indexed frame to avoid.
    //
    // So the floor is applied now, which keeps the index sorted from the first
    // byte, and the wall-clock gap is resolved when a frame actually arrives.
    let last_indexed = index_mirror.span().map(|(_, last)| last);
    let resumed_pts = last_indexed.map(|last| last.saturating_add(1));
    let pending_resume = last_indexed.map(|last| PendingResume {
        last_indexed: last,
        anchor_wall_us: meta.anchor_wall_us,
        anchor_pts_90khz: meta.anchor_pts_90khz,
        recent: meta.recent_wall_us.zip(meta.recent_pts_90khz),
    });
    if let Some(last) = last_indexed {
        tracing::info!(
            recording_id = %recording_id, last_indexed = last,
            dated = meta.anchor_wall_us.is_some(),
            "replay: resuming the recording's PTS timeline across a restart"
        );
    }

    Ok(WriterState {
        recording_id,
        dir,
        meta,
        config,
        stats,
        events,
        clips,
        index_writer,
        current_segment: None,
        closer: closer_tx,
        closer_task: tokio::spawn(run_closer(closer, closer_rx)),
        _lease: lease,
        budget,
        arrived: Instant::now(),
        current_segment_started: None,
        accumulated_pts: resumed_pts.unwrap_or(0),
        last_pcr: None,
        pending_resume,
        // A resume IS a discontinuity in the media, even though the index's
        // own timeline is now continuous across it. The PCR in the TS restarts
        // with the process, so the first frame back gets flagged and a reader
        // can see that the two sides cannot be presented as one timeline.
        //
        // Keyed on there being an index to resume INTO, not on the resume being
        // datable. Keying it on the anchor left the join unflagged in exactly
        // the cases where the counter could not be dated either — a pre-anchor
        // recording, or an unreadable `recording.json` — so `spans_discontinuity`
        // reported clean across a real join and the clip exporter's refusal,
        // which exists for that join, never fired.
        pending_pcr_discontinuity: resumed_pts.is_some(),
        timecode: TimecodeTracker::new(),
        index_mirror,
        pending_mark_in: Arc::new(Mutex::new(None)),
        mode: initial_mode,
    })
}

/// The writer queue's budget in bytes — [`WRITER_QUEUE_BYTES`] of its own,
/// past its first [`WRITER_QUEUE_FLOOR_BYTES`] within
/// [`NODE_RECORDER_QUEUE_BYTES`] shared by every recorder on the node. The
/// subscriber charges each message as it queues it, and the writer releases
/// it as it takes it off; what is still queued when both have gone goes back
/// to the node then.
#[derive(Debug)]
struct QueueBudget(byte_budget::QueueBudget);

impl Default for QueueBudget {
    fn default() -> Self {
        Self::within(&NODE_RECORDER_QUEUES)
    }
}

impl QueueBudget {
    fn within(node: &'static NodeBudget) -> Self {
        Self(byte_budget::QueueBudget::new(WRITER_QUEUE_BYTES, WRITER_QUEUE_FLOOR_BYTES, node))
    }

    fn cost(payload: usize) -> usize {
        payload + MESSAGE_OVERHEAD
    }

    /// Charge a message of `payload` bytes, or refuse it — charging nothing —
    /// if it would take the queue, or the node's recorders between them, past
    /// the budget.
    fn try_charge(&self, payload: usize) -> Option<Charge> {
        self.0.try_charge(Self::cost(payload))
    }

    fn release(&self, charge: Charge) {
        self.0.release(charge);
    }
}

async fn subscriber_loop(
    rx: &mut broadcast::Receiver<RtpPacket>,
    writer_tx: mpsc::Sender<(Bytes, Instant, Charge)>,
    budget: Arc<QueueBudget>,
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
                    // Charged before it is queued, so the writer can never
                    // release a message the budget has not counted yet.
                    let queued = match budget.try_charge(pkt.data.len()) {
                        None => false,
                        Some(charge) => match writer_tx.try_send((pkt.data, Instant::now(), charge)) {
                            Ok(()) => true,
                            Err(TrySendError::Full(_)) => {
                                budget.release(charge);
                                false
                            }
                            // The writer has gone — it could not open the
                            // recording, or the flow is stopping — and nothing
                            // will take another packet. Counting each one as
                            // a lag raised a Critical `replay_writer_lagged`
                            // every five seconds for the rest of the flow.
                            Err(TrySendError::Closed(_)) => {
                                budget.release(charge);
                                break;
                            }
                        },
                    };
                    if !queued {
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

/// Writer arming state.
///
/// * `Idle` — operator pressed Stop. The writer task is still alive
///   (so subsequent commands ack cleanly) but `write_chunk` returns
///   early without touching disk and no new segments roll. Re-enter
///   `Armed` via `Start`.
/// * `PreBuffer` — pre-buffer is configured (`recording.pre_buffer_seconds.is_some()`)
///   and the operator hasn't pressed Start *yet*. Initial state at
///   flow startup only — Stop now goes to `Idle`, not back to
///   `PreBuffer`. Segments roll to disk with retention pinned at
///   `pre_buffer_seconds`. `RecordingStats.armed` stays false so the
///   manager UI distinguishes pre-roll from a recording session, and
///   the manager's stall detector doesn't trigger (it gates on `armed`).
/// * `Armed` — recording session active. Retention bumps to the full
///   `recording.retention_seconds`. Existing pre-buffer segments
///   become the head of the recording. `armed` flips true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterMode {
    Idle,
    PreBuffer,
    Armed,
}

/// State captured by `MarkIn` and consumed by `MarkOut`. Carries both the
/// in-PTS and the SMPTE timecode that was live at mark-in time, so the
/// resulting clip's `smpte_in` reflects the moment the operator marked
/// rather than the moment they marked the out-point.
#[derive(Debug, Clone)]
struct PendingMarkIn {
    pts_90khz: u64,
    smpte_tc: Option<String>,
}

struct WriterState {
    recording_id: String,
    dir: PathBuf,
    meta: RecordingMeta,
    config: RecordingConfig,
    stats: Arc<RecordingStats>,
    events: EventSender,
    clips: ClipStore,
    index_writer: IndexWriter,
    current_segment: Option<OpenSegment>,
    /// Where finished segments go to be closed, in order — see [`Closer`].
    /// Bounded at [`MAX_PENDING_CLOSES`]: a roll that finds it full waits.
    closer: mpsc::Sender<CloseJob>,
    /// The closer itself, awaited when the writer exits so that a stopped
    /// writer has finished every segment it closed.
    closer_task: JoinHandle<()>,
    /// This writer's share of its hold on the recording's directory; the
    /// closer keeps the other.
    _lease: Arc<RecordingLease>,
    /// The queue's byte budget, released here as each message is taken off.
    budget: Arc<QueueBudget>,
    /// When the message being written came off the broadcast.
    ///
    /// The writer can be seconds behind the feed — its queue holds up to
    /// [`WRITER_QUEUE_BYTES`] — so whatever it dates or times by the clock,
    /// it dates by this rather than by when it got round to the message: the
    /// wall-clock anchor and its rolling sample, a gap's length, a segment's
    /// age. Dated on processing instead, a writer catching up on a stalled
    /// disk would stamp the anchor's sample seconds late, and every clip cut
    /// through it would come out that far from the moment marked.
    arrived: Instant,
    /// When the open segment's first message arrived — see [`Self::arrived`].
    current_segment_started: Option<Instant>,
    accumulated_pts: u64,
    last_pcr: Option<u64>,
    /// A resume whose wall-clock gap has not been measured yet, or an
    /// in-process gap that has just ended. Consumed by the first PCR seen
    /// afterwards; see [`PendingResume`].
    pending_resume: Option<PendingResume>,
    /// Sticky bit set when an unusually large PCR step is observed.
    /// Cleared by the next IDR after stamping `flag::PCR_DISCONTINUITY`
    /// onto its index entry — readers use this to avoid wallclock-pacing
    /// across a stream-source change.
    pending_pcr_discontinuity: bool,
    timecode: TimecodeTracker,
    /// In-memory mirror of the on-disk `index.bin`. Loaded once at writer
    /// startup, then appended in lock-step with `index_writer`. Lets
    /// `mark_in` / `mark_out` resolve the SMPTE TC at a target PTS via
    /// `find_floor` instead of using the live snapshot, so a clip's
    /// `smpte_in` and `smpte_out` reflect the actual frames the operator
    /// marked rather than whatever TC happened to be live at mark-out
    /// time.
    index_mirror: InMemoryIndex,
    pending_mark_in: Arc<Mutex<Option<PendingMarkIn>>>,
    /// Current arming state. Drives retention selection in
    /// [`WriterState::effective_retention_seconds`] and the meaning of
    /// the Start / Stop commands. See [`WriterMode`].
    mode: WriterMode,
}

struct OpenSegment {
    id: u32,
    file: BufWriter<File>,
    bytes_written: u64,
    staging_path: PathBuf,
    final_path: PathBuf,
    saw_first_idr: bool,
}

async fn writer_loop(
    mut state: WriterState,
    mut rx: mpsc::Receiver<(Bytes, Instant, Charge)>,
    mut cmd_rx: mpsc::Receiver<RecordingCommand>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            cmd = cmd_rx.recv() => match cmd {
                Some(c) => state.handle_command(c).await,
                None => break,
            },
            data = rx.recv() => match data {
                Some((buf, arrived, charge)) => {
                    state.budget.release(charge);
                    if let Err(e) = state.write_chunk(&buf, arrived).await {
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
                        // rather than a generic "send to closed channel" —
                        // packets are dropped until the next roll boundary
                        // opens a fresh segment. The partial segment goes to
                        // the closer like any other, so it lands in order:
                        // renamed into place if it can be finished, its
                        // staging file unlinked if not (on ENOSPC the close
                        // will likely fail too). Marked as reported, since
                        // this event already says what happened to it.
                        state.close_current(true).await;
                    }
                }
                None => break,
            }
        }
    }
    let stats = state.stats.clone();
    state.shut_down().await;
    stats.armed.store(false, Ordering::Relaxed);
    stats.mode.store(MODE_IDLE, Ordering::Relaxed);
}

impl WriterState {
    /// Write one chunk of broadcast bytes to the current segment.
    /// Each chunk is the `data` field of one `RtpPacket` — for our
    /// usage this is either raw MPEG-TS (`is_raw_ts: true`) or
    /// RTP-wrapped TS. We assume raw TS in the simple Phase 1 path
    /// (assembled and passthrough flows produce raw TS); the RTP-wrap
    /// case is handled by skipping the 12-byte RTP header.
    ///
    /// `arrived` is when the chunk came off the broadcast — see
    /// [`Self::arrived`].
    async fn write_chunk(&mut self, buf: &Bytes, arrived: Instant) -> Result<()> {
        // Idle: operator pressed Stop. Drain the broadcast subscriber
        // (so it doesn't lag) but no bytes hit disk and no segments
        // roll. A later Start flips back to Armed and writes resume.
        if self.mode == WriterMode::Idle {
            return Ok(());
        }
        self.arrived = arrived;
        let ts = strip_rtp_if_present(buf);
        if ts.is_empty() || !ts.len().is_multiple_of(TS_PACKET) {
            // Mis-sized or non-TS payload — don't poison the segment.
            // Audio-only PCM flows would land here; recording is a no-op
            // for them in Phase 1.
            return Ok(());
        }
        // Roll segment if wall time exceeded — the feed's, not the writer's,
        // so a segment holds `segment_seconds` of media even when the writer
        // is catching up on a backlog.
        let need_roll = match self.current_segment_started {
            None => true,
            Some(started) => {
                arrived.saturating_duration_since(started).as_secs() >= self.config.segment_seconds as u64
            }
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
                // Mirror in memory so mark-in / mark-out can resolve
                // SMPTE TC by PTS without re-reading index.bin.
                self.index_mirror.entries.push(entry);

                // Take the wall-clock anchor on the first indexed frame.
                //
                // Here rather than at writer start, because the gap between
                // opening the writer and the first frame arriving is exactly
                // the error that made `created_at_unix` useless for this. This
                // still carries the pipeline delay — the frame is timestamped
                // when it reaches us, not when it was shot — but that delay is
                // the same one the CMAF epoch is founded on, so a mark placed
                // through the published dates and a cut made through this
                // anchor agree with each other, which is what matters.
                if self.meta.anchor_wall_us.is_none() {
                    let us = self.arrival_wall_us();
                    self.meta.anchor_wall_us = Some(us);
                    self.meta.anchor_pts_90khz = Some(entry.pts_90khz);
                    tracing::info!(
                        recording_id = %self.recording_id,
                        anchor_pts = entry.pts_90khz, anchor_wall_us = us,
                        "replay: wall-clock anchor taken on the first indexed frame"
                    );
                    // Persisted at once, by the closer: an anchor only in
                    // memory is lost to the first restart, and every clip cut
                    // afterwards would fall back to whole segments.
                    self.persist_meta(true);
                } else {
                    // Re-take the *second* point of the mapping on a slow tick.
                    //
                    // The anchor is deliberately fixed — moving it would shift
                    // every clip cut from media already on disk — but one point
                    // only gives a line if the rate is assumed, and the assumed
                    // 90 000 ticks per wall-clock second is not what the counter
                    // does: it advances on the source's PCR. This pair is what
                    // lets the exporter use the rate the recording has actually
                    // run at instead.
                    //
                    // Never on a counter a gap has not yet dated: an index entry
                    // can land before the first PCR back (an RAI on a packet
                    // with no PCR field), and pairing *now* with a tick from
                    // before the gap would teach the mapping a rate that is
                    // wrong by the whole outage for the next sixty seconds.
                    let us = self.arrival_wall_us();
                    let due = self.pending_resume.is_none()
                        && self
                            .meta
                            .recent_wall_us
                            .is_none_or(|prev| us.saturating_sub(prev) >= REANCHOR_INTERVAL_US);
                    if due {
                        self.meta.recent_wall_us = Some(us);
                        self.meta.recent_pts_90khz = Some(entry.pts_90khz);
                        self.persist_meta(false);
                    }
                }
            }
        }
        Ok(())
    }

    /// Catch the counter up to wall clock after an in-process gap.
    ///
    /// Only meaningful with an anchor, because without one there is no mapping
    /// to catch up *to* — `false` then, and the caller's one tick stands.
    /// Never moves the counter backwards: `resume_pts` floors at one tick past
    /// where it was, so an untrustworthy anchor cannot un-sort the index.
    fn resync_after_gap(&mut self) -> bool {
        if self.meta.anchor_wall_us.is_none() {
            return false;
        }
        let dated = resume_pts(
            self.accumulated_pts,
            self.meta.anchor_wall_us,
            self.meta.anchor_pts_90khz,
            self.meta.recent_wall_us.zip(self.meta.recent_pts_90khz),
            self.arrival_wall_us(),
        );
        tracing::debug!(
            recording_id = %self.recording_id,
            from = self.accumulated_pts, to = dated,
            "replay: catching the PTS counter up to wall clock across a gap in the feed"
        );
        self.accumulated_pts = dated;
        true
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
                    if pcr_flag && af.len() > 6 {
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
            // A gap that ended: this is the first media back, so this is the
            // instant its length can finally be measured.
            //
            // Measured two ways, and the PCR wins when it can. Across an
            // operator's Stop/Start the source's clock kept running, so a
            // step still inside the discontinuity threshold is the exact
            // length of the gap in the source's own ticks — better than any
            // estimate made from this host's clock. Only a restart (no PCR to
            // compare against) or a stop longer than the threshold falls back
            // to the wall clock.
            if let Some(pending) = self.pending_resume.take() {
                if self.last_pcr.is_some() && delta <= PCR_DISCONTINUITY_THRESHOLD_90KHZ {
                    self.accumulated_pts = self
                        .accumulated_pts
                        .wrapping_add(delta)
                        .max(pending.last_indexed.saturating_add(1));
                    tracing::info!(
                        recording_id = %self.recording_id, resumed_pts = self.accumulated_pts,
                        last_indexed = pending.last_indexed, pcr_ticks = delta,
                        "replay: dated the gap by the source's own PCR"
                    );
                } else {
                    let resumed = resume_pts(
                        pending.last_indexed,
                        pending.anchor_wall_us,
                        pending.anchor_pts_90khz,
                        pending.recent,
                        self.arrival_wall_us(),
                    );
                    self.accumulated_pts = resumed.max(self.accumulated_pts);
                    tracing::info!(
                        recording_id = %self.recording_id, resumed_pts = self.accumulated_pts,
                        last_indexed = pending.last_indexed,
                        "replay: dated the gap at the first frame back"
                    );
                }
                self.last_pcr = Some(new_pcr);
                self.stats.current_pts_90khz.store(self.accumulated_pts, Ordering::Relaxed);
            } else if delta > PCR_DISCONTINUITY_THRESHOLD_90KHZ {
                self.pending_pcr_discontinuity = true;
                // A step this large is either a genuine gap in the feed or a
                // PCR base reset. A single tick is right for the second and
                // wrong for the first: wall-clock time passed, so the counter
                // has to catch up or every mark placed afterwards resolves to
                // media from `gap` earlier — and `spans_discontinuity` cannot
                // catch it, because the flagged entry sits *before* the
                // shifted window rather than inside it.
                //
                // Re-dating along the recording's own line serves both: no
                // wall time passes in a base reset, so the counter resolves to
                // within a tick of where it already is, and the floor keeps the
                // index sorted if the anchor is untrustworthy. With no anchor
                // there is nothing to date against, and one tick is the claim.
                if !self.resync_after_gap() {
                    self.accumulated_pts = self.accumulated_pts.wrapping_add(1);
                }
                self.last_pcr = Some(new_pcr);
                self.stats.current_pts_90khz.store(self.accumulated_pts, Ordering::Relaxed);
            } else {
                self.accumulated_pts = self.accumulated_pts.wrapping_add(delta);
                self.last_pcr = Some(new_pcr);
                self.stats.current_pts_90khz.store(self.accumulated_pts, Ordering::Relaxed);
            }
        }

        // Feed the timecode tracker (filters internally; harmless on non-video).
        if payload_start < TS_PACKET && (adaptation_field_control == 0x1 || adaptation_field_control == 0x3) {
            self.timecode.observe_ts(pusi, &ts[payload_start..]);
        }

        // Index every random-access point that lands in a segment.
        //
        // Not one that lands in none. After a write error the writer closes
        // the segment and drops what arrives until the next roll opens one,
        // and an entry made meanwhile named segment 0 — `unwrap_or(0)` — at an
        // offset into the packets being dropped: a scrub or an export resolving
        // it read whatever segment 0 holds, the start of the recording or a
        // file retention removed long ago. The PCR above is still followed, so
        // the counter keeps time across the gap.
        let seg = self.current_segment.as_mut()?;
        if random_access && !seg.saw_first_idr {
            // First random-access of segment — promote to the canonical
            // index entry for this segment.
            seg.saw_first_idr = true;
        }
        if random_access {
            let segment_id = seg.id;
            let snap = self.timecode.snapshot();
            let tc_packed = smpte_tc_packed(snap.as_ref());
            let pcr_disc = std::mem::replace(&mut self.pending_pcr_discontinuity, false);
            let entry = IndexEntry {
                pts_90khz: self.accumulated_pts,
                smpte_tc: tc_packed.unwrap_or(0xFFFFFFFF),
                segment_id,
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
        self.close_current(false).await;
        // Open the new segment.
        let id = self.meta.current_segment_id;
        let staging_path = self.dir.join(".tmp").join(format!("{:06}.ts", id));
        let final_path = self.dir.join(format!("{:06}.ts", id));
        let file = OpenOptions::new()
            .create(true).truncate(true).write(true)
            .open(&staging_path).await
            .with_context(|| format!("open staging {}", staging_path.display()))?;
        let file = BufWriter::with_capacity(SEGMENT_WRITE_BUF, file);
        self.current_segment = Some(OpenSegment {
            id, file, bytes_written: 0, staging_path, final_path, saw_first_idr: false,
        });
        self.current_segment_started = Some(self.arrived);
        Ok(())
    }

    /// Hand the open segment, if there is one, to the closer, and move the
    /// segment id on past it.
    ///
    /// Every path that ends a segment comes through here — a roll, a Stop, a
    /// write error, the writer exiting — because each needs the same two
    /// things. The id moves on whatever closed the segment: only the roll
    /// used to advance it, so after a Stop the next Start opened the stopped
    /// segment's id again, and the roll after that renamed the new file over
    /// the last segment before the Stop, with that segment's index entries
    /// still pointing into it. A write error did the same. And the segment
    /// goes to the one closer, so renames land in the order segments closed.
    ///
    /// `reported` is for the write-error path, whose event already says what
    /// became of the segment: a close that then fails too is cleaned up
    /// without a second one.
    ///
    /// Waits only when [`MAX_PENDING_CLOSES`] segments are already waiting —
    /// a disk that cannot keep up — and the queue in front absorbs that.
    async fn close_current(&mut self, reported: bool) {
        let Some(seg) = self.current_segment.take() else {
            return;
        };
        self.meta.current_segment_id = seg.id.wrapping_add(1);
        // The index entries for this segment's packets go to the kernel
        // before the segment goes to the closer, so the fsync it gives the
        // index after the rename covers them. A buffered write, not an fsync.
        let _ = self.index_writer.flush().await;
        let job = CloseJob::Segment {
            seg,
            meta: self.meta.clone(),
            retention_seconds: self.effective_retention_seconds(),
            reported,
        };
        if self.closer.send(job).await.is_err() {
            // The closer exits only once the writer has, so this is a closer
            // that panicked: nothing will finish this segment, and a restart
            // will find its staging file as a crash's leftover.
            tracing::error!(
                target: "replay", recording_id = %self.recording_id,
                "segment closer is gone; a finished segment was not closed"
            );
        }
    }

    /// Wait until the closer has finished everything handed to it so far.
    async fn drain_closer(&self) {
        let (done, finished) = oneshot::channel();
        if self.closer.send(CloseJob::Drain(done)).await.is_ok() {
            let _ = finished.await;
        }
    }

    /// Have the closer write `recording.json` as it stands — the clock
    /// anchor (`anchor`) or its rolling second sample.
    ///
    /// Never waits: a closer with no room is behind on segments, and rather
    /// than hold the writer for a metadata write, the update rides with the
    /// next segment closed, whose close writes `recording.json` whole.
    fn persist_meta(&self, anchor: bool) {
        let _ = self.closer.try_send(CloseJob::Meta { meta: self.meta.clone(), anchor });
    }

    /// The wall clock at [`Self::arrived`]: now, less the time the message
    /// spent queued.
    fn arrival_wall_us(&self) -> i64 {
        now_unix_us().saturating_sub(i64::try_from(self.arrived.elapsed().as_micros()).unwrap_or(i64::MAX))
    }

    /// The writer exiting — a cancel, or the flow's broadcast or command
    /// channel closing: close the open segment and wait for the closer to
    /// finish every segment it holds, so a stopped writer leaves each one
    /// renamed into place, or gone.
    async fn shut_down(mut self) {
        self.close_current(false).await;
        let WriterState { closer, closer_task, mut index_writer, .. } = self;
        // Its last sender: the closer runs out of work and returns.
        drop(closer);
        let _ = closer_task.await;
        // An entry is appended only while a segment is open, and handed to the
        // kernel when that segment closes, so the closer's fsync after the
        // segment's rename covers it. This covers a closer that did not get
        // that far — one that panicked.
        let _ = index_writer.flush_and_sync().await;
    }

    /// Retention window currently in force. In `Armed` mode this is the
    /// configured `recording.retention_seconds`; in `PreBuffer` mode it
    /// snaps to `recording.pre_buffer_seconds` so the on-disk window
    /// stays bounded to "the last N seconds" while the operator hasn't
    /// pressed Start. In `Idle` mode the value is irrelevant — no new
    /// data is being written, and the only segment closed in `Idle` is the
    /// one a Stop closes, whose close takes the full retention.
    /// `0` always means unlimited (subject to `max_bytes` and free disk).
    fn effective_retention_seconds(&self) -> u64 {
        match self.mode {
            WriterMode::Armed => self.config.retention_seconds,
            WriterMode::PreBuffer => self
                .config
                .pre_buffer_seconds
                .map(|s| s as u64)
                .unwrap_or(self.config.retention_seconds),
            WriterMode::Idle => self.config.retention_seconds,
        }
    }

    /// Resolve the SMPTE TC at a given recording PTS via the in-memory
    /// index mirror. Returns the TC stamped on the IDR at-or-before
    /// `target_pts` (i.e. the SMPTE TC the source video carried for the
    /// frame the operator marked). Returns `None` when the index is
    /// empty (mark before the first IDR), when the closest IDR carried
    /// no decodable SMPTE (`SMPTE_TC_VALID = 0`), or when SMPTE was
    /// rejected by the timecode tracker's validator.
    fn lookup_smpte_at_pts(&self, target_pts: u64) -> Option<String> {
        lookup_smpte_in_index(&self.index_mirror, target_pts)
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
                // Coming back from Idle is a gap like any other.
                //
                // `write_chunk` returns before `observe_packet` while Idle, so
                // the counter is frozen and `last_pcr` is stale for however
                // long the operator left it stopped. Resuming from the frozen
                // value places every later frame earlier than it happened by
                // exactly that interval — and a gap over five minutes was
                // worth one tick, so a ten-minute Stop shifted the rest of the
                // recording by ten minutes against the anchor, silently. The
                // length is measured at the first frame back, the same way a
                // restart's is — by the source's own PCR when the stop was
                // short enough for the step to be believable, which is why
                // `last_pcr` is kept, and by the wall clock otherwise.
                if self.mode == WriterMode::Idle {
                    self.pending_resume = Some(PendingResume {
                        last_indexed: self.accumulated_pts,
                        anchor_wall_us: self.meta.anchor_wall_us,
                        anchor_pts_90khz: self.meta.anchor_pts_90khz,
                        recent: self.meta.recent_wall_us.zip(self.meta.recent_pts_90khz),
                    });
                    self.pending_pcr_discontinuity = true;
                }
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
                // Transition to Idle and fully halt disk writes. The
                // writer task stays alive (so subsequent commands ack
                // cleanly) but `write_chunk` returns early in Idle, no
                // new segments roll. Reset `current_segment_started`
                // so a later Start doesn't see a stale elapsed timer
                // and roll a phantom segment on the first chunk.
                self.mode = WriterMode::Idle;
                self.stats.armed.store(false, Ordering::Relaxed);
                self.stats.mode.store(MODE_IDLE, Ordering::Relaxed);
                // Acked only once the closer has finished every segment this
                // recording wrote, the one just closed last: a stopped
                // recording is complete on disk, every segment renamed into
                // place, in order.
                self.close_current(false).await;
                self.drain_closer().await;
                self.current_segment_started = None;
                let _ = reply.send(Ok(()));
            }
            RecordingCommand::MarkIn { explicit_pts, reply } => {
                let pts = explicit_pts.unwrap_or(self.accumulated_pts);
                if pts == 0 && self.last_pcr.is_none() {
                    let _ = reply.send(Err(anyhow!("replay_recording_not_active")));
                    return;
                }
                // Resolve TC at the marked PTS via the index, not from the
                // live snapshot — for `explicit_pts` in the past that's
                // the only source-of-truth, and for "now" it's still
                // correct (last IDR ≈ now).
                let smpte_tc = self.lookup_smpte_at_pts(pts);
                *self.pending_mark_in.lock().await = Some(PendingMarkIn {
                    pts_90khz: pts,
                    smpte_tc: smpte_tc.clone(),
                });
                let _ = reply.send(Ok(MarkInAck { pts_90khz: pts, smpte_tc }));
            }
            RecordingCommand::MarkOut { name, description, explicit_out_pts, created_by, reply } => {
                let pending = self.pending_mark_in.lock().await.clone();
                let pending = match pending {
                    Some(p) => p,
                    None => {
                        let _ = reply.send(Err(anyhow!("replay_recording_not_active")));
                        return;
                    }
                };
                let in_pts = pending.pts_90khz;
                let out_pts = explicit_out_pts.unwrap_or(self.accumulated_pts);
                if out_pts <= in_pts {
                    let _ = reply.send(Err(anyhow!(
                        "out_pts ({out_pts}) must be greater than in_pts ({in_pts})"
                    )));
                    return;
                }
                let smpte_in = pending.smpte_tc;
                // Independent index lookup at out_pts — distinct from the
                // mark-in lookup above, so `smpte_out` reflects the frame
                // at the out-point rather than cloning `smpte_in`.
                let smpte_out = self.lookup_smpte_at_pts(out_pts);
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
                    *self.pending_mark_in.lock().await = None;
                }
                let _ = reply.send(res);
            }
        }
    }
}

/// Work for the closer, in the order the writer hands it over.
enum CloseJob {
    /// A finished segment.
    Segment {
        seg: OpenSegment,
        /// `recording.json` as of the close, the next segment id included.
        meta: RecordingMeta,
        /// The retention in force when it closed — see
        /// [`WriterState::effective_retention_seconds`].
        retention_seconds: u64,
        /// Already reported by the writer, whose own write failed: a close
        /// that fails too is cleaned up without a second event.
        reported: bool,
    },
    /// `recording.json` alone, between segments: the clock anchor
    /// (`anchor`), or its rolling second sample.
    Meta { meta: RecordingMeta, anchor: bool },
    /// Answered once everything handed over before it is done.
    Drain(oneshot::Sender<()>),
}

/// The recorder's third task: finishes each segment the writer closes, one
/// at a time, in the order they closed.
///
/// Before it, the roll did all of this in line — and #192 then moved only
/// the segment's own flush, fsync and rename into concurrent background
/// tasks. That left the index fsync, the retention scan and the metadata
/// fsync on the receive path, let renames land out of order (a slow close of
/// one segment finishing after its successor's, so a reader or a clip export
/// met a missing *middle* segment and skipped it silently), made the index
/// durable before the segment it names, and ran retention while the segment
/// it protects was still in `.tmp/` — invisible to the guard and to the
/// `replay_max_bytes_below_segment` warning, which then fired only when the
/// close happened to win the race. One ordered task fixes all four: per
/// segment it flushes and fsyncs, renames, then makes the index durable,
/// writes `recording.json`, and runs retention with that segment in place.
struct Closer {
    recording_id: String,
    dir: PathBuf,
    meta_path: PathBuf,
    /// A second handle on `index.bin`, fsynced after each rename.
    index: File,
    config: RecordingConfig,
    stats: Arc<RecordingStats>,
    events: EventSender,
    /// Sticky bit so the 80 % disk-pressure Warning fires once per
    /// pressure episode rather than every segment roll. Cleared via
    /// hysteresis at 70 % so a recovered system can re-warn later.
    disk_pressure_emitted: bool,
    /// The closer's share of the writer's hold on the directory: a writer
    /// that panicked leaves its closer finishing segments, and the next
    /// writer must wait for that too.
    _lease: Arc<RecordingLease>,
}

async fn run_closer(mut closer: Closer, mut jobs: mpsc::Receiver<CloseJob>) {
    // Ends when the writer, its only sender, has gone.
    while let Some(job) = jobs.recv().await {
        match job {
            CloseJob::Segment { seg, meta, retention_seconds, reported } => {
                closer.finish(seg, &meta, retention_seconds, reported).await;
            }
            CloseJob::Meta { meta, anchor } => {
                if let Err(e) = write_meta_atomic(&closer.meta_path, &meta).await {
                    if anchor {
                        tracing::warn!(
                            recording_id = %closer.recording_id, error = %e,
                            "replay: could not persist the wall-clock anchor; it goes \
                             with the next segment's metadata, and until then clip cuts \
                             after a restart fall back to whole segments"
                        );
                    } else {
                        tracing::debug!(
                            recording_id = %closer.recording_id, error = %e,
                            "replay: could not persist the rolling clock sample"
                        );
                    }
                }
            }
            CloseJob::Drain(done) => {
                let _ = done.send(());
            }
        }
    }
}

impl Closer {
    async fn finish(&mut self, seg: OpenSegment, meta: &RecordingMeta, retention: u64, reported: bool) {
        let id = seg.id;
        let staging = seg.staging_path.clone();
        match close_segment(seg).await {
            Ok(()) => {
                self.stats.segments_written.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                // Never left in `.tmp/`, outside both retention and the
                // recordings listing, for the next start to sweep up.
                let _ = tokio::fs::remove_file(&staging).await;
                if !reported {
                    self.report_close_failure(id, &e);
                }
            }
        }
        // The index after the rename, never before, so the fsync issued for
        // a segment never makes its entries durable while it is still in
        // `.tmp/`. That is all the ordering buys. The writer is already
        // appending entries for the open segment, and for any queued behind
        // this one, and an fsync covers the whole file — as can the kernel's
        // own writeback — so a power cut can still leave entries naming a
        // segment that never left `.tmp/`. A restart resumes past every id
        // the index names, so such an entry names a missing file, never one
        // written later. Best-effort, as the index is rebuildable from the
        // segments.
        let _ = self.index.sync_all().await;
        // Surface meta-write failures to the operator. On restart
        // `open_writer` resumes past the directory listing and the ids in
        // `index.bin` as well as the meta, so a stale meta doesn't cause
        // segment-id reuse — but the operator should still see the warning
        // so they can investigate (typically ENOSPC on the same volume that
        // holds the segments).
        if let Err(e) = write_meta_atomic(&self.meta_path, meta).await {
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
                    "current_segment_id": meta.current_segment_id,
                }),
            );
        }
        self.run_retention(id, retention).await;
    }

    /// A segment that could not be finished is a gap in the recording, so it
    /// is Critical whatever the cause; the code says which kind of gap. The
    /// disk itself failing — full, over quota, read-only, I/O errors — is
    /// `replay_disk_full`, the code the writer's own write failures carry
    /// and the one the manager and the docs know: a close that fails there
    /// is the same fault surfacing a little later. Anything else — the
    /// staging file taken away, a rename refused — is
    /// `replay_segment_close_failed`.
    fn report_close_failure(&self, id: u32, e: &anyhow::Error) {
        tracing::error!(
            target: "replay", recording_id = %self.recording_id, segment = id,
            "segment close failed: {e:#}"
        );
        let (error_code, replay_event) = if is_disk_failure(e) {
            ("replay_disk_full", "disk_full")
        } else {
            ("replay_segment_close_failed", "segment_close_failed")
        };
        self.events.emit_with_details(
            EventSeverity::Critical,
            category::REPLAY,
            format!(
                "Recording '{}' could not finish segment {id}; it is lost: {e}",
                self.recording_id
            ),
            None,
            serde_json::json!({
                "error_code": error_code,
                "replay_event": replay_event,
                "recording_id": self.recording_id,
                "segment_id": id,
            }),
        );
    }

    /// Prune by age and size, oldest first — never `protected_id`, the
    /// segment just finished.
    async fn run_retention(&mut self, protected_id: u32, retention: u64) {
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
        // The just-finished segment is never pruned — losing the most
        // recent segment would tear out clips that touch the live edge. It
        // is renamed into place before this runs, so it is in the listing
        // and in `total`: the guard and the warning below both see it.
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
}

/// Whether a failure is the disk itself: full, over quota, remounted
/// read-only, or failing I/O.
fn is_disk_failure(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| matches!(io.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT | libc::EIO | libc::EROFS)))
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
/// directory, or `None` on a fresh recording. One of the three bounds on
/// the resume id — see [`open_writer`] — because `recording.json` is
/// written after the rename it follows, and can be missing or corrupt.
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

/// Resolve the SMPTE TC at `target_pts` against an in-memory index,
/// returning `None` when the index has no entry at-or-before
/// `target_pts` or the closest IDR carried no decodable SMPTE. Pulled
/// out of `WriterState::lookup_smpte_at_pts` so unit tests can exercise
/// the lookup-and-unpack logic without spinning up a writer task.
fn lookup_smpte_in_index(index: &InMemoryIndex, target_pts: u64) -> Option<String> {
    let entry = index.find_floor(target_pts)?;
    if !entry.smpte_tc_valid() {
        return None;
    }
    Some(unpack_smpte_tc(entry.smpte_tc))
}

/// Inverse of [`smpte_tc_packed`] — turns the 32-bit-packed `IndexEntry`
/// representation back into the human-readable `HH:MM:SS:FF` string the
/// clip metadata stores. Drop-frame is not distinguished (matches the
/// pack side).
fn unpack_smpte_tc(packed: u32) -> String {
    let h = (packed >> 24) & 0xFF;
    let m = (packed >> 16) & 0xFF;
    let s = (packed >> 8) & 0xFF;
    let f = packed & 0xFF;
    format!("{:02}:{:02}:{:02}:{:02}", h, m, s, f)
}

/// Pack a SMPTE TimecodeStats snapshot into a u32 bit-packed
/// HH:MM:SS:FF representation. Returns `None` if no timecode is known.
/// Format: `(HH << 24) | (MM << 16) | (SS << 8) | FF` — drop frame is
/// not flagged in Phase 1.
fn smpte_tc_packed(snap: Option<&crate::stats::models::TimecodeStats>) -> Option<u32> {
    let s = snap?.last.as_ref()?;
    let parts: Vec<&str> = s.split([':', ';']).collect();
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
    // Drains the userspace buffer into the file, then the file to the kernel.
    seg.file.flush().await?;
    seg.file.get_mut().sync_all().await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A restart must continue the recording's timeline, not start a new one.
    ///
    /// `accumulated_pts` is a pseudo-PTS the writer builds from PCR deltas so
    /// the index stays monotonic across a 33-bit wrap — and `find_floor`
    /// binary-searches it, so monotonic is not a nicety. A restart appends to
    /// the same index and continues the segment numbering, so a counter that
    /// began again at zero wrote a second overlapping timeline into one file.
    /// Observed on the rig: 556 entries running 86,400 .. 74,606,400, then
    /// dropping to 144,000 at the restart. Every wall-clock lookup after that
    /// point missed, which is what `replay_no_video_frames` was.
    #[test]
    fn a_resumed_recording_continues_its_pts_timeline() {
        // Anchor: wall 1_000.000_000s ↔ pts 86_400.
        let anchor_wall_us = 1_000_000_000i64;
        let anchor_pts = 86_400u64;
        // Last indexed frame sits 100s after the anchor.
        let last = anchor_pts + 100 * 90_000;
        // The writer comes back 30s later.
        let now_us = anchor_wall_us + 130 * 1_000_000;

        let resumed = resume_pts(last, Some(anchor_wall_us), Some(anchor_pts), None, now_us);

        assert!(resumed > last, "the timeline went backwards, and the index is no longer sorted");
        // 130s after the anchor: the 100s recorded plus the 30s outage.
        assert_eq!(
            resumed,
            anchor_pts + 130 * 90_000,
            "the outage was not accounted for, so every later clip is shifted by it"
        );

        // A clock that has gone backwards must not rewind the timeline either.
        let backwards = resume_pts(last, Some(anchor_wall_us), Some(anchor_pts), None, anchor_wall_us);
        assert!(backwards > last, "a backwards clock unsorted the index");
    }

    /// A recording with no anchor still resumes — monotonically.
    ///
    /// This is the case the resume most needed and refused: every recording
    /// written before the anchor existed, plus any whose `recording.json` could
    /// not be read. Returning `None` there set the counter back to zero into a
    /// non-empty `index.bin`, so the file held two overlapping timelines,
    /// `find_floor` binary-searched unsorted data, and the very bug the resume
    /// was written to fix reappeared on exactly those recordings.
    ///
    /// Dating the join needs an anchor; keeping the index sorted does not.
    #[test]
    fn a_recording_with_no_anchor_still_resumes_monotonically() {
        let last = 74_606_400u64;
        let now_us = 1_788_827_202_551_745i64;

        let resumed = resume_pts(last, None, None, None, now_us);
        assert!(
            resumed > last,
            "the counter restarted below the index and un-sorted it"
        );
        assert_eq!(resumed, last + 1, "un-dated means the minimum honest step");

        // Half an anchor is no anchor.
        assert_eq!(resume_pts(last, Some(1), None, None, now_us), last + 1);
        assert_eq!(resume_pts(last, None, Some(1), None, now_us), last + 1);
    }

    /// A clock that was wrong when the anchor was taken must not poison the
    /// timeline.
    ///
    /// An RTC-less board — the shipped Rockchip artefact is one — boots at the
    /// Unix epoch, indexes its first frame, and is stepped to the real date by
    /// NTP moments later. The gap that implies is decades. In u64 the multiply
    /// wrapped and a release build, which has no overflow checks, resumed on a
    /// timeline about 4.7 years away from anything a lookup could reach; a
    /// debug build panicked inside `spawn_writer`, which `FlowRuntime::start`
    /// awaits inline, taking the whole flow start with it.
    #[test]
    fn an_absurd_gap_is_clamped_rather_than_wrapped() {
        // Anchor taken at ten seconds past the epoch; now is 2026.
        let anchor_wall_us = 10_000_000i64;
        let anchor_pts = 0u64;
        let last = 60 * 90_000u64; // 60s of media indexed
        let now_us = 1_789_000_000_000_000i64;

        let resumed = resume_pts(last, Some(anchor_wall_us), Some(anchor_pts), None, now_us);
        assert!(resumed > last, "the timeline went backwards");
        let max_plausible = last + (MAX_RESUME_GAP_US as u64) * 90_000 / 1_000_000;
        assert!(
            resumed <= max_plausible,
            "resumed to {resumed}, past the {max_plausible} the clamp allows — \
             an arithmetic result, not a measurement"
        );
    }

    fn entry(pts: u64, packed: u32, valid: bool) -> IndexEntry {
        IndexEntry {
            pts_90khz: pts,
            smpte_tc: packed,
            segment_id: 0,
            byte_offset: 0,
            flags: flag::IS_IDR | if valid { flag::SMPTE_TC_VALID } else { 0 },
        }
    }

    #[test]
    fn unpack_roundtrips_pack() {
        // 01:23:45:12 → packed → unpacked
        let packed: u32 = (1 << 24) | (23 << 16) | (45 << 8) | 12;
        assert_eq!(unpack_smpte_tc(packed), "01:23:45:12");
    }

    #[test]
    fn lookup_returns_floor_entry_smpte() {
        // Two IDRs: PTS=100 → 00:00:01:00, PTS=200 → 00:00:02:00
        let mut idx = InMemoryIndex::default();
        idx.entries.push(entry(100, (0 << 24) | (0 << 16) | (1 << 8) | 0, true));
        idx.entries.push(entry(200, (0 << 24) | (0 << 16) | (2 << 8) | 0, true));

        // PTS in [100, 200) → first entry; PTS ≥ 200 → second entry.
        assert_eq!(lookup_smpte_in_index(&idx, 100).as_deref(), Some("00:00:01:00"));
        assert_eq!(lookup_smpte_in_index(&idx, 150).as_deref(), Some("00:00:01:00"));
        assert_eq!(lookup_smpte_in_index(&idx, 200).as_deref(), Some("00:00:02:00"));
        assert_eq!(lookup_smpte_in_index(&idx, 999).as_deref(), Some("00:00:02:00"));
    }

    #[test]
    fn lookup_returns_distinct_smpte_for_distinct_pts() {
        // The Bug A guarantee: an in-PTS and an out-PTS that fall on
        // different IDRs must resolve to different SMPTE strings.
        let mut idx = InMemoryIndex::default();
        idx.entries.push(entry(1_000_000, (5 << 24) | (40 << 16) | (22 << 8) | 0, true));
        idx.entries.push(entry(2_000_000, (5 << 24) | (40 << 16) | (27 << 8) | 0, true));

        let smpte_in = lookup_smpte_in_index(&idx, 1_500_000);
        let smpte_out = lookup_smpte_in_index(&idx, 2_500_000);
        assert_eq!(smpte_in.as_deref(), Some("05:40:22:00"));
        assert_eq!(smpte_out.as_deref(), Some("05:40:27:00"));
        assert_ne!(smpte_in, smpte_out);
    }

    #[test]
    fn lookup_returns_none_when_index_empty() {
        // Mark before any IDR has been observed.
        let idx = InMemoryIndex::default();
        assert!(lookup_smpte_in_index(&idx, 12345).is_none());
    }

    #[test]
    fn lookup_returns_none_when_floor_entry_has_invalid_smpte() {
        // IDR was indexed but the source had no SMPTE (or it was
        // rejected by the validator) — entry stored with SMPTE_TC_VALID=0.
        let mut idx = InMemoryIndex::default();
        idx.entries.push(entry(100, 0xFFFFFFFF, false));
        assert!(lookup_smpte_in_index(&idx, 150).is_none());
    }

    /// One 188-byte TS packet on PID 0x100, its payload stamped with `n` so a
    /// segment overwritten by a later one reads differently. `pcr_90k` puts a
    /// PCR in an adaptation field, and `rap` sets its random-access flag —
    /// which is what the writer indexes.
    fn ts_packet(n: u64, pcr_90k: Option<u64>, rap: bool) -> [u8; TS_PACKET] {
        let mut p = [0u8; TS_PACKET];
        p[0] = SYNC_BYTE;
        p[1] = 0x41;
        p[2] = 0x00;
        let cc = (n & 0x0F) as u8;
        let mut payload_at = 4;
        if pcr_90k.is_some() || rap {
            p[3] = 0x30 | cc;
            let mut flags = 0u8;
            if rap {
                flags |= 0x40;
            }
            let af_len = if let Some(base) = pcr_90k {
                flags |= 0x10;
                let base = base & ((1u64 << 33) - 1);
                p[6] = (base >> 25) as u8;
                p[7] = (base >> 17) as u8;
                p[8] = (base >> 9) as u8;
                p[9] = (base >> 1) as u8;
                p[10] = (((base & 1) as u8) << 7) | 0x7E;
                p[11] = 0;
                7
            } else {
                1
            };
            p[4] = af_len as u8;
            p[5] = flags;
            payload_at = 5 + af_len;
        } else {
            p[3] = 0x10 | cc;
        }
        for (i, b) in p[payload_at..].iter_mut().enumerate() {
            *b = (n >> ((i % 8) * 8)) as u8;
        }
        p
    }

    fn raw_ts(data: Vec<u8>) -> RtpPacket {
        RtpPacket {
            data: Bytes::from(data),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        }
    }

    /// A writer recording into `parent/rec`, with the receiver for its events.
    async fn writer_in(
        parent: &Path,
        config: RecordingConfig,
    ) -> (WriterState, mpsc::Receiver<crate::manager::events::Event>) {
        let (events, events_rx) = crate::manager::events::event_channel();
        let dir = parent.join("rec");
        let lease = RecordingLease::try_take(&dir).expect("another writer holds the recording");
        let setup = WriterSetup {
            flow_id: "flow".into(),
            recording_id: "rec".into(),
            dir,
            config,
            events,
            stats: Arc::default(),
            budget: Arc::default(),
        };
        let state = open_writer(setup, lease).await.unwrap();
        (state, events_rx)
    }

    /// Feed packets `from..to`, one per message, arriving 100 ms apart from
    /// `t0` — so a 2 s segment rolls every 20 — with a PCR on each and a
    /// random-access point on every fifth.
    async fn feed(state: &mut WriterState, t0: Instant, from: u64, to: u64) {
        for n in from..to {
            let data = Bytes::from(ts_packet(n, Some(n * 9_000), n % 5 == 0).to_vec());
            state.write_chunk(&data, t0 + Duration::from_millis(n * 100)).await.unwrap();
        }
    }

    async fn command(state: &mut WriterState, stop: bool) {
        if stop {
            let (reply, ack) = oneshot::channel();
            state.handle_command(RecordingCommand::Stop { reply }).await;
            ack.await.unwrap().unwrap();
        } else {
            let (reply, ack) = oneshot::channel();
            state.handle_command(RecordingCommand::Start { reply }).await;
            ack.await.unwrap().unwrap();
        }
    }

    /// The `.ts` files in `dir`, sorted.
    fn segment_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".ts"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn drain_events(rx: &mut mpsc::Receiver<crate::manager::events::Event>) -> Vec<crate::manager::events::Event> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn error_code(e: &crate::manager::events::Event) -> Option<&str> {
        e.details.as_ref()?.get("error_code")?.as_str()
    }

    /// Two rolls, then a Stop: every segment the recording wrote is renamed
    /// into place by the time the Stop is answered, none is left in `.tmp/`,
    /// `segments_written` counts exactly the files, and every index entry
    /// lands on the random-access packet it names.
    #[tokio::test]
    async fn a_stopped_recording_is_complete_on_disk_and_its_index_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let (mut state, _events) = writer_in(tmp.path(), config).await;
        let dir = tmp.path().join("rec");
        feed(&mut state, Instant::now(), 0, 50).await;
        command(&mut state, true).await;

        assert_eq!(segment_files(&dir), ["000000.ts", "000001.ts", "000002.ts"]);
        assert!(segment_files(&dir.join(".tmp")).is_empty(), "a staging file outlived the Stop");
        assert_eq!(state.stats.segments_written.load(Ordering::Relaxed), 3);
        let on_disk: u64 = segment_files(&dir).iter().map(|f| std::fs::metadata(dir.join(f)).unwrap().len()).sum();
        assert_eq!(on_disk, 50 * TS_PACKET as u64);

        let index = InMemoryIndex::load(&dir.join("index.bin")).await.unwrap();
        assert_eq!(index.entries.len(), 10);
        for e in &index.entries {
            let seg = std::fs::read(dir.join(format!("{:06}.ts", e.segment_id))).unwrap();
            let p = &seg[e.byte_offset as usize..e.byte_offset as usize + TS_PACKET];
            assert_eq!(p[0], SYNC_BYTE);
            assert!(p[3] & 0x20 != 0 && p[5] & 0x40 != 0, "entry {e:?} is not on a random-access packet");
        }
        // The id the next segment takes is past every one on disk.
        let meta: RecordingMeta =
            serde_json::from_slice(&std::fs::read(dir.join("recording.json")).unwrap()).unwrap();
        assert_eq!(meta.current_segment_id, 3);
    }

    /// The `n` a [`ts_packet`] was stamped with.
    fn stamp(p: &[u8]) -> u64 {
        let at = if p[3] & 0x20 != 0 { 5 + p[4] as usize } else { 4 };
        u64::from_le_bytes(p[at..at + 8].try_into().unwrap())
    }

    /// Between a write error and the next roll no segment is open, and the
    /// packets that arrive are dropped. A random-access point among them is
    /// not indexed: an entry made then named segment 0, at an offset into
    /// packets nobody wrote, so a scrub or an export resolving it read the
    /// start of the recording instead.
    #[tokio::test]
    async fn no_entry_is_indexed_while_no_segment_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let (mut state, _events) = writer_in(tmp.path(), config).await;
        let dir = tmp.path().join("rec");
        let t0 = Instant::now();
        feed(&mut state, t0, 0, 10).await;
        // What the writer does on a write error: close the segment and drop
        // what arrives until the roll, two seconds after it opened.
        state.close_current(true).await;
        feed(&mut state, t0, 10, 30).await;
        command(&mut state, true).await;

        let index = InMemoryIndex::load(&dir.join("index.bin")).await.unwrap();
        let indexed: Vec<u64> = index.entries.iter().map(|e| e.pts_90khz / 9_000).collect();
        assert_eq!(indexed, [0, 5, 20, 25], "a random-access point no segment holds was indexed");
        for e in &index.entries {
            let seg = std::fs::read(dir.join(format!("{:06}.ts", e.segment_id))).unwrap();
            let p = &seg[e.byte_offset as usize..e.byte_offset as usize + TS_PACKET];
            assert_eq!(stamp(p), e.pts_90khz / 9_000, "entry {e:?} names another packet");
        }
    }

    /// Stop then Start must not reuse the stopped segment's id.
    ///
    /// Only a roll used to move the id on. A Stop closed the open segment
    /// and left the id where it was, so the Start opened it again and the
    /// next roll renamed the new file over the last segment before the
    /// Stop — whose index entries still named byte offsets in it.
    #[tokio::test]
    async fn stop_then_start_never_overwrites_the_last_segment_before_the_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let (mut state, _events) = writer_in(tmp.path(), config).await;
        let dir = tmp.path().join("rec");
        let t0 = Instant::now();
        feed(&mut state, t0, 0, 30).await;
        command(&mut state, true).await;
        let last_before_stop = std::fs::read(dir.join("000001.ts")).unwrap();
        let index_before = InMemoryIndex::load(&dir.join("index.bin")).await.unwrap().entries;

        command(&mut state, false).await;
        feed(&mut state, t0, 100, 130).await;
        command(&mut state, true).await;

        assert_eq!(
            std::fs::read(dir.join("000001.ts")).unwrap(),
            last_before_stop,
            "the segment closed by the Stop was overwritten after the Start"
        );
        assert_eq!(segment_files(&dir), ["000000.ts", "000001.ts", "000002.ts", "000003.ts"]);
        // The entries written before the Stop still land on what they named.
        for e in index_before.iter().filter(|e| e.segment_id == 1) {
            let o = e.byte_offset as usize;
            assert_eq!(&last_before_stop[o..o + TS_PACKET], &std::fs::read(dir.join("000001.ts")).unwrap()[o..o + TS_PACKET]);
        }
    }

    /// Every byte a replay of the whole recording in `dir` plays.
    async fn play_whole(dir: &Path) -> Vec<u8> {
        let mut reader = crate::replay::reader::Reader::open_recording_at("rec", dir.to_path_buf())
            .await
            .unwrap();
        let mut played = Vec::new();
        while let Some(bundle) = reader.read_bundle(7).await.unwrap() {
            played.extend_from_slice(&bundle);
        }
        played
    }

    /// Every segment in `dir`, in id order, end to end.
    fn recorded(dir: &Path) -> Vec<u8> {
        segment_files(dir).iter().flat_map(|f| std::fs::read(dir.join(f)).unwrap()).collect()
    }

    /// What a crash leaves behind: whatever the kernel had been handed — the
    /// segments renamed into place, the index entries appended, the staging
    /// files — and nothing the process still held: the open segment's
    /// buffer, the closes it had not done. The closer is let finish what it
    /// was already given, so drain it first, or stall it, to say where in
    /// its work the crash fell.
    async fn crash(mut state: WriterState) {
        state.index_writer.flush().await.unwrap();
        let WriterState { closer, closer_task, .. } = state;
        drop(closer);
        closer_task.await.unwrap();
    }

    fn recovery_alert(rx: &mut mpsc::Receiver<crate::manager::events::Event>) -> Option<serde_json::Value> {
        drain_events(rx)
            .into_iter()
            .find(|e| error_code(e) == Some("replay_recovery_alert"))
            .and_then(|e| e.details)
    }

    /// A clean stop and restart — a flow restart, a node restart, a DVR
    /// session re-arming the recorder — carries the numbering straight on,
    /// with or without a Stop first, and playback runs across the join.
    ///
    /// The meta's id is the next to open once a segment has closed. Resumed
    /// plus one, as when it held the last segment's, it left a hole one id
    /// wide at every restart, and a replay ended there.
    #[tokio::test]
    async fn a_restarted_recording_carries_its_numbering_on_with_no_gap() {
        for stop_first in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
            let dir = tmp.path().join("rec");
            let (mut state, _events) = writer_in(tmp.path(), config.clone()).await;
            feed(&mut state, Instant::now(), 0, 50).await;
            if stop_first {
                command(&mut state, true).await;
            }
            state.shut_down().await;

            let (mut state, mut events) = writer_in(tmp.path(), config).await;
            assert_eq!(state.meta.current_segment_id, 3, "stop first: {stop_first}");
            assert_eq!(recovery_alert(&mut events), None, "a clean restart was reported as a recovery");
            feed(&mut state, Instant::now(), 0, 25).await;
            state.shut_down().await;

            assert_eq!(
                segment_files(&dir),
                ["000000.ts", "000001.ts", "000002.ts", "000003.ts", "000004.ts"],
                "stop first: {stop_first}"
            );
            assert_eq!(play_whole(&dir).await, recorded(&dir), "stop first: {stop_first}");
        }
    }

    /// A recording last closed by a build whose meta held the last segment's
    /// id rather than the next resumes one past it, and is not reported as a
    /// recovery: nothing was lost.
    #[tokio::test]
    async fn a_meta_naming_the_last_segment_resumes_past_it_unreported() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let dir = tmp.path().join("rec");
        let (mut state, _events) = writer_in(tmp.path(), config.clone()).await;
        feed(&mut state, Instant::now(), 0, 50).await;
        state.shut_down().await;
        let meta_path = dir.join("recording.json");
        let mut meta: RecordingMeta = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta.current_segment_id = 2;
        write_meta_atomic(&meta_path, &meta).await.unwrap();

        let (state, mut events) = writer_in(tmp.path(), config).await;
        assert_eq!(state.meta.current_segment_id, 3);
        assert_eq!(recovery_alert(&mut events), None);
    }

    /// A crash with a segment open that the index already names: the restart
    /// resumes past it, so the entries the crash left for it name a missing
    /// file rather than media recorded after the restart — and a replay
    /// steps over the gap.
    #[tokio::test]
    async fn a_crash_never_hands_out_again_an_id_the_index_names() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let dir = tmp.path().join("rec");
        let (mut state, _events) = writer_in(tmp.path(), config.clone()).await;
        // Segments 0 and 1, then 2 open with two random-access points in it.
        feed(&mut state, Instant::now(), 0, 50).await;
        state.drain_closer().await;
        crash(state).await;
        let left = InMemoryIndex::load(&dir.join("index.bin")).await.unwrap().entries;
        assert!(left.iter().any(|e| e.segment_id == 2));

        let (mut state, mut events) = writer_in(tmp.path(), config).await;
        assert_eq!(state.meta.current_segment_id, 3, "an id the index names was handed out again");
        let alert = recovery_alert(&mut events).expect("the crash's leftovers are reported");
        assert_eq!(alert["tmp_orphans_removed"], 1);
        assert_eq!(alert["next_segment_id"], 3);
        feed(&mut state, Instant::now(), 0, 45).await;
        state.shut_down().await;

        assert_eq!(segment_files(&dir), ["000000.ts", "000001.ts", "000003.ts", "000004.ts", "000005.ts"]);
        assert_eq!(play_whole(&dir).await, recorded(&dir), "a replay stopped at the gap");
    }

    /// An id nothing names yet is used again: a crash before the open
    /// segment's first random-access point leaves no entry naming it, so the
    /// restart carries on with no gap.
    #[tokio::test]
    async fn a_crash_before_the_open_segment_is_indexed_leaves_no_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let dir = tmp.path().join("rec");
        let (mut state, _events) = writer_in(tmp.path(), config.clone()).await;
        let t0 = Instant::now();
        feed(&mut state, t0, 0, 40).await;
        // Rolls to segment 2 on a packet that is not a random-access point.
        let data = Bytes::from(ts_packet(41, Some(41 * 9_000), false).to_vec());
        state.write_chunk(&data, t0 + Duration::from_millis(4_100)).await.unwrap();
        state.drain_closer().await;
        crash(state).await;

        let (mut state, _events) = writer_in(tmp.path(), config).await;
        assert_eq!(state.meta.current_segment_id, 2);
        feed(&mut state, Instant::now(), 0, 25).await;
        state.shut_down().await;
        assert_eq!(segment_files(&dir), ["000000.ts", "000001.ts", "000002.ts", "000003.ts"]);
    }

    /// A crash while the closer is behind — a disk stalling on fsyncs —
    /// loses every segment it had not finished, and the meta it last wrote
    /// trails all of them. The restart resumes past every one the index
    /// names.
    #[tokio::test]
    async fn a_crash_with_the_closer_behind_resumes_past_every_segment_it_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let dir = tmp.path().join("rec");
        let (mut state, _events) = writer_in(tmp.path(), config.clone()).await;
        let t0 = Instant::now();
        feed(&mut state, t0, 0, 50).await;
        state.drain_closer().await;
        // From here nothing is taken off the closer's queue, as when it is
        // stuck on a disk that has stopped answering.
        let (stalled, queued) = mpsc::channel::<CloseJob>(16);
        drop(std::mem::replace(&mut state.closer, stalled));
        // Closes 2, 3 and 4, and opens 5.
        feed(&mut state, t0, 50, 110).await;
        crash(state).await;
        drop(queued);
        assert_eq!(segment_files(&dir), ["000000.ts", "000001.ts"]);
        let meta: RecordingMeta =
            serde_json::from_slice(&std::fs::read(dir.join("recording.json")).unwrap()).unwrap();
        assert_eq!(meta.current_segment_id, 2);

        let (state, mut events) = writer_in(tmp.path(), config).await;
        assert_eq!(state.meta.current_segment_id, 6, "an id the index names was handed out again");
        let alert = recovery_alert(&mut events).expect("the lost segments are reported");
        assert_eq!(alert["tmp_orphans_removed"], 4);
        assert_eq!(alert["next_segment_id"], 6);
    }

    /// A close that fails leaves no staging file behind, raises a Critical
    /// event under a documented code, and is not counted as written.
    #[tokio::test]
    async fn a_close_that_fails_is_reported_and_its_staging_file_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let (mut state, mut events) = writer_in(tmp.path(), config).await;
        let dir = tmp.path().join("rec");
        // A directory where segment 0 is to land: its rename is refused
        // (EISDIR) after the flush and fsync have succeeded — whoever runs
        // the test, root included.
        std::fs::create_dir(dir.join("000000.ts")).unwrap();
        feed(&mut state, Instant::now(), 0, 30).await;
        command(&mut state, true).await;

        assert!(!dir.join(".tmp/000000.ts").exists(), "the failed segment's staging file was left behind");
        assert!(dir.join("000001.ts").is_file());
        assert_eq!(state.stats.segments_written.load(Ordering::Relaxed), 1, "a failed close was counted");
        let failed: Vec<_> = drain_events(&mut events)
            .into_iter()
            .filter(|e| error_code(e) == Some("replay_segment_close_failed"))
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].severity, EventSeverity::Critical);
        assert_eq!(failed[0].details.as_ref().unwrap()["segment_id"], 0);
    }

    /// The disk failing under a close is the fault the writer's own writes
    /// report, under the code the manager and the docs already know.
    #[test]
    fn a_close_failing_on_the_disk_itself_is_replay_disk_full() {
        let io = |errno| anyhow::Error::from(std::io::Error::from_raw_os_error(errno));
        for errno in [libc::ENOSPC, libc::EDQUOT, libc::EIO, libc::EROFS] {
            assert!(is_disk_failure(&io(errno)), "errno {errno}");
            assert!(is_disk_failure(&io(errno).context("rename a -> b")), "errno {errno} under context");
        }
        for errno in [libc::EISDIR, libc::ENOENT, libc::EACCES] {
            assert!(!is_disk_failure(&io(errno)), "errno {errno}");
        }
        assert!(!is_disk_failure(&anyhow!("not an I/O error")));
    }

    /// Retention runs once the segment it protects is in place, so a cap
    /// below one segment is reported every time rather than when the close
    /// happened to win a race — and the protected segment survives it.
    #[tokio::test]
    async fn a_cap_below_one_segment_is_reported_and_the_newest_segment_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig {
            segment_seconds: 2,
            max_bytes: 5 * TS_PACKET as u64,
            ..RecordingConfig::default()
        };
        let (mut state, mut events) = writer_in(tmp.path(), config).await;
        let dir = tmp.path().join("rec");
        feed(&mut state, Instant::now(), 0, 30).await;
        command(&mut state, true).await;

        let below: usize = drain_events(&mut events)
            .iter()
            .filter(|e| error_code(e) == Some("replay_max_bytes_below_segment"))
            .count();
        // Once for segment 0 at the roll, once for segment 1 at the Stop.
        assert_eq!(below, 2);
        assert_eq!(segment_files(&dir), ["000001.ts"]);
    }

    /// A writer behind its feed dates the clock anchor by when the frame came
    /// off the broadcast, not by when it got round to writing it — or every
    /// clip cut through the anchor is off by the backlog.
    #[tokio::test]
    async fn the_anchor_is_dated_by_arrival_not_by_the_writers_backlog() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut state, _events) = writer_in(tmp.path(), RecordingConfig::default()).await;
        let backlog = Duration::from_secs(5);
        let arrived = Instant::now().checked_sub(backlog).unwrap();
        let data = Bytes::from(ts_packet(0, Some(0), true).to_vec());
        state.write_chunk(&data, arrived).await.unwrap();
        let anchor = state.meta.anchor_wall_us.expect("the first random-access point takes the anchor");
        let behind_us = now_unix_us() - anchor;
        assert!(
            (4_500_000..6_000_000).contains(&behind_us),
            "anchored {behind_us} us before now; it arrived {backlog:?} ago"
        );
    }

    /// The queue is bounded by what it holds, not by how many messages: a
    /// feed that broadcasts whole frames gets the same 32 MiB as one that
    /// broadcasts single TS packets — and for the smallest message, the
    /// message cap never binds before the bytes do.
    #[test]
    fn the_writer_queue_is_bounded_in_bytes() {
        static NODE: NodeBudget = NodeBudget::new(usize::MAX);
        let admitted = |payload: usize| {
            let budget = QueueBudget::within(&NODE);
            let charges: Vec<Charge> = std::iter::from_fn(|| budget.try_charge(payload)).collect();
            (charges, budget)
        };
        // RTMP-sized frames: 16,384 of them used to be ~800 MB.
        let (frames, budget) = admitted(50_000);
        assert!(frames.len() * 50_000 <= WRITER_QUEUE_BYTES);
        assert!((frames.len() + 1) * QueueBudget::cost(50_000) > WRITER_QUEUE_BYTES);
        budget.release(frames[0]);
        assert!(budget.try_charge(50_000).is_some(), "a released message's room was not given back");
        // SDI-sized messages, overhead and all.
        let (packets, _) = admitted(TS_PACKET);
        assert!(packets.len() <= WRITER_QUEUE_CAP, "the message cap would bind before the budget");
        assert!(packets.len() * TS_PACKET < WRITER_QUEUE_BYTES, "the per-message overhead went uncounted");
    }

    /// The node's recorders cannot queue more between them than the node's
    /// budget past their floors, whatever each may on its own — a disk that
    /// stalls under the one replay root stalls them all at once — and a
    /// recorder with nothing of the node's left still has its floor.
    #[test]
    fn recorders_share_the_nodes_queue_budget() {
        const NODE_LIMIT: usize = WRITER_QUEUE_BYTES + WRITER_QUEUE_BYTES / 2;
        const FLOOR: usize = WRITER_QUEUE_FLOOR_BYTES;
        static NODE: NodeBudget = NodeBudget::new(NODE_LIMIT);
        let cost = QueueBudget::cost(50_000);
        let fill = |budget: &QueueBudget| std::iter::from_fn(|| budget.try_charge(50_000)).count();
        let first = QueueBudget::within(&NODE);
        let second = QueueBudget::within(&NODE);
        let (a, b) = (fill(&first), fill(&second));
        assert_eq!(a, WRITER_QUEUE_BYTES / cost, "the first did not get its own whole budget");
        assert_eq!(
            b,
            (NODE_LIMIT - (a * cost - FLOOR) + FLOOR) / cost,
            "the second was not held to its floor and what the node had left"
        );
        let third = QueueBudget::within(&NODE);
        assert!(fill(&third) >= FLOOR / cost, "a recorder that keeps up dropped for what the others hold");
        drop(third);
        // A recorder stopped with a backlog gives it back.
        drop(first);
        assert_eq!(NODE.queued(), b * cost - FLOOR);
    }

    /// A cancelled recorder is not finished until it has closed its open
    /// segment and every close behind it — what `FlowRuntime::stop` waits on
    /// before a restarted recorder may sweep `.tmp/`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_recorder_is_finished_only_once_its_segments_are_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let (events, _events_rx) = crate::manager::events::event_channel();
        let (tx, _) = broadcast::channel::<RtpPacket>(1024);
        let cancel = CancellationToken::new();
        let dir = tmp.path().join("rec");
        let handle = spawn_writer_at(
            "flow".into(), "rec".into(), dir.clone(), RecordingConfig::default(), tx.clone(), events, cancel.clone(),
        )
        .await
        .unwrap();
        for n in 0..20 {
            tx.send(raw_ts(ts_packet(n, Some(n * 9_000), n % 5 == 0).to_vec())).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.stats.bytes_written.load(Ordering::Relaxed) < 20 * TS_PACKET as u64 {
            assert!(Instant::now() < deadline, "the writer never took the packets");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        assert!(handle.finished(Duration::from_secs(10)).await);
        assert_eq!(segment_files(&dir), ["000000.ts"]);
        assert!(segment_files(&dir.join(".tmp")).is_empty());
        assert_eq!(std::fs::metadata(dir.join("000000.ts")).unwrap().len(), 20 * TS_PACKET as u64);
        assert_eq!(handle.stats.segments_written.load(Ordering::Relaxed), 1);
    }

    /// Publish packets `from..to` as a flow would, one per message, with a
    /// PCR on each and a random-access point on every fifth — whether or not
    /// anything is still subscribed, as a flow's input does.
    fn publish(tx: &broadcast::Sender<RtpPacket>, from: u64, to: u64) {
        for n in from..to {
            let _ = tx.send(raw_ts(ts_packet(n, Some(n * 9_000), n % 5 == 0).to_vec()));
        }
    }

    /// A writer started for a recording whose previous writer is still
    /// finishing — a flow restarted while the old closer fsyncs on a slow
    /// disk — waits for it before it sweeps `.tmp/` or picks a resume id.
    /// Without that the sweep unlinked the segment the old closer had yet to
    /// rename, and that segment, with no index entry naming it, shared its id
    /// with the new writer's first. Nor does the wait hold up the flow:
    /// spawning returns at once, and what the flow publishes meanwhile is
    /// queued and recorded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restarted_writer_waits_for_the_previous_one_to_finish() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
        let dir = tmp.path().join("rec");
        // Segments 0 and 1, closed cleanly: `recording.json` names 2.
        let (mut state, _events) = writer_in(tmp.path(), config.clone()).await;
        feed(&mut state, Instant::now(), 0, 40).await;
        state.shut_down().await;
        // The previous writer, still finishing segment 2: it holds the
        // recording, and the segment waits in `.tmp/` for its rename, with
        // no index entry naming it.
        let previous = RecordingLease::try_take(&dir).unwrap();
        let in_flight: Vec<u8> = (0..20u64).flat_map(|n| ts_packet(500 + n, None, false)).collect();
        std::fs::write(dir.join(".tmp/000002.ts"), &in_flight).unwrap();

        let (events, mut events_rx) = crate::manager::events::event_channel();
        let (tx, _) = broadcast::channel::<RtpPacket>(1024);
        let cancel = CancellationToken::new();
        let handle = spawn_writer_at("flow".into(), "rec".into(), dir.clone(), config, tx.clone(), events, cancel.clone())
            .await
            .unwrap();
        publish(&tx, 0, 30);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(dir.join(".tmp/000002.ts").is_file(), "the new writer swept the old one's segment");
        assert_eq!(handle.stats.bytes_written.load(Ordering::Relaxed), 0);

        // The previous writer's closer finishes: renames its segment, writes
        // the next id, lets the recording go.
        std::fs::rename(dir.join(".tmp/000002.ts"), dir.join("000002.ts")).unwrap();
        let meta_path = dir.join("recording.json");
        let mut meta: RecordingMeta = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta.current_segment_id = 3;
        write_meta_atomic(&meta_path, &meta).await.unwrap();
        drop(previous);

        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.stats.bytes_written.load(Ordering::Relaxed) < 30 * TS_PACKET as u64 {
            assert!(Instant::now() < deadline, "the writer never opened the recording");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        assert!(handle.finished(Duration::from_secs(10)).await);

        assert_eq!(std::fs::read(dir.join("000002.ts")).unwrap(), in_flight, "the previous writer's segment was overwritten");
        // Published at once, so one segment's worth of arrival time.
        assert_eq!(segment_files(&dir), ["000000.ts", "000001.ts", "000002.ts", "000003.ts"]);
        assert_eq!(std::fs::metadata(dir.join("000003.ts")).unwrap().len(), 30 * TS_PACKET as u64);
        assert_eq!(handle.stats.packets_dropped.load(Ordering::Relaxed), 0, "packets published during the wait were lost");
        assert_eq!(recovery_alert(&mut events_rx), None, "a clean restart was swept as a crash's leftovers");
    }

    /// A flow stopped while its writer waits for the previous one ends the
    /// wait: nothing opened, nothing left running.
    #[tokio::test]
    async fn a_writer_waiting_on_the_previous_one_stops_with_its_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("rec");
        let _previous = RecordingLease::try_take(&dir).unwrap();
        let (events, _events_rx) = crate::manager::events::event_channel();
        let (tx, _) = broadcast::channel::<RtpPacket>(16);
        let cancel = CancellationToken::new();
        let handle = spawn_writer_at("flow".into(), "rec".into(), dir.clone(), RecordingConfig::default(), tx, events, cancel.clone())
            .await
            .unwrap();
        cancel.cancel();
        assert!(handle.finished(Duration::from_secs(5)).await);
        assert!(!dir.exists(), "the recording was opened");
    }

    /// An open that waited and then failed is reported once, as a failed
    /// start of the flow's recording, and the subscriber stops with the
    /// writer instead of calling every packet after it a lag. The flow keeps
    /// its handle, and the recorder answers its commands as a flow with no
    /// recorder is answered, not with "recording writer task is gone".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waited_open_that_fails_is_reported_as_a_failed_start() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the recording's directory should be.
        let dir = tmp.path().join("rec");
        std::fs::write(&dir, b"not a directory").unwrap();
        let previous = RecordingLease::try_take(&dir).unwrap();
        let (events, mut events_rx) = crate::manager::events::event_channel();
        let (tx, _) = broadcast::channel::<RtpPacket>(1024);
        let cancel = CancellationToken::new();
        let handle = spawn_writer_at("flow".into(), "rec".into(), dir, RecordingConfig::default(), tx.clone(), events, cancel.clone())
            .await
            .unwrap();
        drop(previous);
        let failed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = events_rx.recv().await.expect("the events channel closed");
                if event.details.as_ref().and_then(|d| d["replay_event"].as_str()) == Some("recording_start_failed") {
                    return event;
                }
            }
        })
        .await
        .expect("the writer never gave up");
        assert_eq!(failed.flow_id.as_deref(), Some("flow"), "the failure was not reported against the flow");
        assert_eq!(failed.details.as_ref().unwrap()["recording_id"], "rec");

        publish(&tx, 0, 20);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(tx.receiver_count(), 0, "the subscriber outlived the writer");
        let (reply, ack) = oneshot::channel();
        handle.command_tx.send(RecordingCommand::Start { reply }).await.expect("the recorder no longer answers");
        let err = ack.await.unwrap().expect_err("a recording that failed to start started");
        assert!(err.to_string().starts_with("replay_recording_not_active"), "{err}");
        let events = drain_events(&mut events_rx);
        assert!(
            !events.iter().any(|e| e.details.as_ref().and_then(|d| d["replay_event"].as_str()) == Some("recording_start_failed")),
            "the failure was reported twice"
        );
        assert!(
            !events.iter().any(|e| error_code(e) == Some("replay_writer_lagged")),
            "packets nothing would take were reported as a lag"
        );
        cancel.cancel();
        assert!(handle.finished(Duration::from_secs(5)).await);
    }

    /// `finished` waits once for all of a recorder's tasks, not once each:
    /// the writer and the filmstrip stall on the same disk together, and a
    /// node stopping waits on every recording flow's.
    #[tokio::test]
    async fn finished_waits_once_for_all_of_a_recorders_tasks() {
        let stuck = || tokio::spawn(tokio::time::sleep(Duration::from_secs(60)));
        let (command_tx, _commands) = mpsc::channel(1);
        let handle = RecordingHandle {
            recording_id: "rec".into(),
            stats: Arc::default(),
            filmstrip_stats: None,
            command_tx,
            tasks: std::sync::Mutex::new(vec![stuck(), stuck()]),
        };
        let started = Instant::now();
        assert!(!handle.finished(Duration::from_secs(1)).await);
        let waited = started.elapsed();
        assert!(waited < Duration::from_millis(1_900), "waited {waited:?} on two tasks given 1 s");
    }

    /// The budget is given back as the writer takes each message off: a
    /// recorder that keeps up carries more than [`WRITER_QUEUE_BYTES`]
    /// through its queue without dropping a packet. Without the release,
    /// every recorder went silent once 32 MiB had passed through it — about
    /// 17 s of an SDI feed — and stayed silent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_writer_that_keeps_up_carries_more_than_its_budget_without_dropping() {
        // ~64 KiB a message, at most ~2 MiB in flight at once.
        const PER_MSG: u64 = 348;
        const BATCH: u64 = 32;
        let tmp = tempfile::tempdir().unwrap();
        let (events, _events_rx) = crate::manager::events::event_channel();
        let (tx, _) = broadcast::channel::<RtpPacket>(1024);
        let cancel = CancellationToken::new();
        let handle = spawn_writer_at(
            "flow".into(), "rec".into(), tmp.path().join("rec"), RecordingConfig::default(), tx.clone(), events, cancel.clone(),
        )
        .await
        .unwrap();
        let msg_bytes = PER_MSG * TS_PACKET as u64;
        let total = WRITER_QUEUE_BYTES as u64 / msg_bytes + 16;
        let (mut n, mut sent) = (0u64, 0u64);
        while n < total {
            for _ in 0..BATCH.min(total - n) {
                let mut data = Vec::with_capacity(msg_bytes as usize);
                for k in 0..PER_MSG {
                    let pcr = (k == 0).then_some(n * 9_000);
                    data.extend_from_slice(&ts_packet(n * PER_MSG + k, pcr, k == 0 && n % 8 == 0));
                }
                tx.send(raw_ts(data)).unwrap();
                n += 1;
                sent += msg_bytes;
            }
            // Let the writer catch up before the next batch.
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let written = handle.stats.bytes_written.load(Ordering::Relaxed);
                let dropped = handle.stats.packets_dropped.load(Ordering::Relaxed) * msg_bytes;
                if written + dropped >= sent {
                    break;
                }
                assert!(Instant::now() < deadline, "the writer stalled at {written} of {sent} bytes");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        assert!(sent > WRITER_QUEUE_BYTES as u64);
        assert_eq!(handle.stats.packets_dropped.load(Ordering::Relaxed), 0, "the budget was never given back");
        assert_eq!(handle.stats.bytes_written.load(Ordering::Relaxed), sent);
        cancel.cancel();
        assert!(handle.finished(Duration::from_secs(10)).await);
    }

    /// The recorder against an SDI flow's message rate, measured.
    ///
    /// An SDI flow broadcasts one 188-byte TS packet per message, about 6,900
    /// a second at 10 Mbit/s — the shape that made every recorder on
    /// bilby-voyager drop ~2.5 % of its packets (#190), five sessions to one
    /// volume. This runs five recorders at that rate for 25 s with 2 s
    /// segments, so each rolls a dozen times, and reports what they dropped
    /// and how far the worst of them fell behind its feed. A measurement
    /// rather than a pass/fail test, because the answer depends on the disk:
    /// run it on the volume a recorder would use, under whatever else that
    /// volume carries, e.g.
    ///
    /// `TMPDIR=/srv/replay cargo test --bin bilbycast-edge bench_sdi_rate -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "a 25 s measurement of five recorders against an SDI message rate"]
    async fn bench_sdi_rate_through_the_recorder() {
        const RATE: u64 = 6_900;
        const SECS: u64 = 25;
        const RECORDERS: usize = 5;
        let tmp = tempfile::tempdir().unwrap();
        let (events, _events_rx) = crate::manager::events::event_channel();
        let cancel = CancellationToken::new();
        let mut txs = Vec::new();
        let mut handles = Vec::new();
        for r in 0..RECORDERS {
            let (tx, _) = broadcast::channel::<RtpPacket>(16_384);
            let config = RecordingConfig { segment_seconds: 2, ..RecordingConfig::default() };
            let id = format!("bench-{r}");
            let dir = tmp.path().join(&id);
            handles.push(
                spawn_writer_at(id.clone(), id, dir, config, tx.clone(), events.clone(), cancel.clone())
                    .await
                    .unwrap(),
            );
            txs.push(tx);
        }

        let published = Arc::new(AtomicU64::new(0));
        let publisher = {
            let published = published.clone();
            // Clones: a flow holds its sender for as long as it runs, and a
            // recorder stops when the last one goes.
            let txs = txs.clone();
            std::thread::spawn(move || {
                // 69 packets every 10 ms, against absolute deadlines so the
                // rate holds however long a send takes.
                let start = Instant::now();
                let per_tick = RATE / 100;
                for tick in 0..SECS * 100 {
                    let due = start + Duration::from_millis(tick * 10);
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    for k in 0..per_tick {
                        let n = tick * per_tick + k;
                        let pcr = n.is_multiple_of(276).then_some(n * 90_000 / RATE);
                        let rap = n.is_multiple_of(RATE);
                        let pkt = raw_ts(ts_packet(n, pcr, rap).to_vec());
                        for tx in &txs {
                            let _ = tx.send(pkt.clone());
                        }
                        published.fetch_add(1, Ordering::Relaxed);
                    }
                }
                start.elapsed()
            })
        };

        // How far each writer is behind its feed, sampled every 10 ms: what
        // was published and neither written nor dropped is still queued. And
        // the longest any writer went without writing while the feed ran.
        let mut lags_ms: Vec<f64> = Vec::new();
        let mut longest_stall = Duration::ZERO;
        let mut progress = [(0u64, Instant::now()); RECORDERS];
        let byte_rate = (RATE * TS_PACKET as u64) as f64;
        while !publisher.is_finished() {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let sent = published.load(Ordering::Relaxed) * TS_PACKET as u64;
            for (h, last) in handles.iter().zip(progress.iter_mut()) {
                let written = h.stats.bytes_written.load(Ordering::Relaxed);
                let dropped = h.stats.packets_dropped.load(Ordering::Relaxed) * TS_PACKET as u64;
                lags_ms.push(sent.saturating_sub(written + dropped) as f64 / byte_rate * 1000.0);
                if written != last.0 {
                    *last = (written, Instant::now());
                }
                longest_stall = longest_stall.max(last.1.elapsed());
            }
        }
        let elapsed = publisher.join().unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        for h in &handles {
            let (reply, ack) = tokio::sync::oneshot::channel();
            h.command_tx.send(RecordingCommand::Stop { reply }).await.unwrap();
            ack.await.unwrap().unwrap();
        }

        lags_ms.sort_by(f64::total_cmp);
        let pct = |p: f64| lags_ms[((lags_ms.len() - 1) as f64 * p) as usize];
        let mut on_disk = 0u64;
        let mut files = 0u64;
        for r in 0..RECORDERS {
            let mut rd = tokio::fs::read_dir(tmp.path().join(format!("bench-{r}"))).await.unwrap();
            while let Some(e) = rd.next_entry().await.unwrap() {
                if e.file_name().to_string_lossy().ends_with(".ts") {
                    files += 1;
                    on_disk += e.metadata().await.unwrap().len();
                }
            }
        }
        let sent = published.load(Ordering::Relaxed) * RECORDERS as u64;
        let dropped: u64 = handles.iter().map(|h| h.stats.packets_dropped.load(Ordering::Relaxed)).sum();
        let segments: u64 = handles.iter().map(|h| h.stats.segments_written.load(Ordering::Relaxed)).sum();
        eprintln!(
            "recorder bench: {RECORDERS} recorders, {sent} packets in {:.2} s ({:.0}/s each); \
             dropped {dropped} ({:.3} %); writer lag p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms; \
             longest stall {} ms; segments_written {segments}, {files} files, \
             {on_disk} bytes on disk of {} published",
            elapsed.as_secs_f64(),
            sent as f64 / RECORDERS as f64 / elapsed.as_secs_f64(),
            dropped as f64 * 100.0 / sent as f64,
            pct(0.50),
            pct(0.99),
            pct(1.0),
            longest_stall.as_millis(),
            sent * TS_PACKET as u64,
        );
        cancel.cancel();
    }
}
