# Replay Server

The replay server captures a flow's broadcast channel to disk and replays
named clips back onto a flow's broadcast channel. It is implemented
purely in Rust, with no new C dependencies, and is enabled by default
via the `replay` Cargo feature.

Several v2 surfaces have shipped on top of the original Phase 1 recorder
— the edge advertises `replay`, `replay-v2`, `replay-filmstrip`, and
`replay_export_mp4` capabilities (pre-buffer recording, filmstrip
thumbnails, clip tags + `update_clip` trim, and TS→MP4 export). The
manager UI gates each surface on the matching capability bit, so the
"Phase 1 / Phase 2" labels below track which release a feature landed
in rather than what is or isn't implemented.

The configuration schema (storage root, `RecordingConfig`,
`ReplayInputConfig`) lives in
[`configuration-guide.md`](configuration-guide.md#replay-recording--playback)
— this document covers architecture, lifecycle, and the error /
operator-action matrix.

## When to use it

- **In-broadcast replay** — clip the play that just happened, send it to
  the keyer, return to live. The JKL UI in the manager (`/replay`)
  matches Avid muscle memory: J/K/L scrub/pause/play, I/O mark, Space
  toggle, comma/period frame-step.
- **Compliance recording** — continuous capture of an outgoing feed
  with a 24 h retention default. The recorder is a sibling subscriber
  on the broadcast channel and never feeds back into the data path,
  so enabling recording cannot affect live egress.
- **Time-shift workflows** — record the rehearsal and play it out as a
  fresh input on a different flow, paced by PCR.

It is **not** a video editing surface. There is no reverse playback,
slow-motion, or multi-track timeline. (Clip / recording **export** does
ship — TS download plus the TS→MP4 remuxer, see the Recordings library
section.)

## Architecture

```
                 ┌────────── flow's broadcast channel ──────────┐
                 │                                              │
       Inputs ───┘    every existing subscriber             ┌── Outputs (UDP/SRT/HLS/…)
                       (TR-101290, content-analysis,        │
                        thumbnail, …)                       │
                                                            │
                                       ┌── replay::writer ──┘
                                       │     (Phase-1 recorder)
                                       │
                                       ▼
                  drop-on-Lagged ──► 32 MiB queue ──► writer task
                                     (256 MiB node-wide)
                                                         │ finished segments,
                                                         ▼ in order (≤ 4 waiting)
                                                     closer task
                                                         │
                                                         ▼
                                              segment N.ts on disk
                                              index.bin (timecode → offset)
                                              recording.json, retention
                                              clips.json (named ranges)
```

- The recorder is **another broadcast subscriber**, exactly like the
  TR-101290 / content-analysis tiers. `RecvError::Lagged(n)` adds `n` to
  `packets_dropped` silently; the Critical `replay_writer_lagged` event
  fires when the writer's queue is past its budget and a packet is dropped
  there (rate-limited to one event per 5 s); the input task is never
  blocked.
- **The queue to the writer is bounded in bytes: 32 MiB per recorder**,
  held only while the writer is behind, each message charged its payload
  plus a fixed 96 bytes for its queue slot, its shared `Bytes` header and
  the allocator. Bytes rather than messages because a message is not a
  fixed size: an SDI flow broadcast one 188-byte TS packet per message
  until edge#166, about 6,900 a second at 10 Mbit/s (it now publishes up to
  seven to a message, 1316 B), while an RTMP or RTSP flow broadcasts a whole
  frame per message. A cap of 1,024 messages was ~150 ms of the SDI
  feed — five recorders on bilby-voyager dropped ~2.5 % of their packets at
  every load (#190) — and the 16,384 that replaced it was 2.4 s of that feed
  but hundreds of megabytes of frames on a stalled disk. 32 MiB is about
  17 s of a 10 Mbit/s feed of 188-byte messages, 25 s of 1316-byte
  datagrams (SDI's shape now) and 27 s of a frame-per-message feed at the
  same rate.
- **And every recorder's queue on the node shares 256 MiB**
  (`NODE_RECORDER_QUEUE_BYTES`), because every recorder writes under the one
  replay root: a disk that stalls stalls them all, and the per-recorder
  budget alone let a node hold 32 MiB times its recording flows at exactly
  that moment. Eight recorders' full budgets — bilby-voyager's five keep
  theirs whole (160 MiB) — so a node recording more flows than eight shares
  it and drops sooner, with the same `replay_writer_lagged`. Separate from
  the CMAF intakes' node budget (`docs/cmaf.md`): a disk and an origin fail
  independently, and one stall filling a shared budget would make the other
  consumers drop on a stall they would have ridden out. What a stopped
  recorder still had queued goes back to the node as it goes. The first
  4 MiB of each recorder's queue (`WRITER_QUEUE_FLOOR_BYTES`, an eighth of
  it) are its own and charge the node nothing, so a recorder that keeps up
  never drops for what the others hold: on the one disk they usually share
  that costs nothing, but a recording directory can sit on a volume of its
  own, and a writer can fall behind for reasons of its own. Beyond each
  flow's broadcast ring, the node's queues together hold at most 512 MiB
  plus those floors — see the worst case in `docs/cmaf.md`.
- **The writer task** does in-memory bookkeeping and buffered writes only:
  it parses the TS, tracks PCR and SMPTE timecode, appends index entries,
  and writes each segment through a 256 KiB userspace buffer (unbuffered, an
  SDI flow was ~6,900 blocking-pool round trips a second). It can be
  seconds behind its feed, so it dates nothing by when it got round to a
  message: each message carries the instant it came off the broadcast, and
  the wall-clock anchor, its rolling sample, a gap's length and a segment's
  age are all taken from that. A writer catching up on a stalled disk
  would otherwise stamp the anchor's sample late by the backlog, and every
  clip cut through it would come out that far from the moment marked.
- **The closer task** finishes each segment the writer closes, one at a
  time, in the order they closed — see [Recording arm → roll →
  prune](#recording-arm--roll--prune). Every fsync and directory scan on
  the roll path is there. Up to four finished segments may wait for it;
  past that the writer waits for room, and the queue in front absorbs it.
  Two fsyncs stay on the writer: a mark-out persists `clips.json` (the file
  and its directory) before it answers, and the writer's exit fsyncs the
  index once the closer has finished.
- Playback is a **new input type** (`type: "replay"`). The replayer
  reads segments back, paces them by PCR from an inline pump in
  `engine::input_replay` (`pump_one_bundle` + `PacingState`), and
  publishes onto the flow's broadcast channel just like any other input.

### Module map

| Path | Role |
|---|---|
| `src/replay/mod.rs` | Public types (`RecordingCommand`, `ReplayCommand`, `ClipInfo`), storage-root resolution + `statvfs` disk usage |
| `src/replay/writer.rs` | Subscriber, writer and closer tasks; the byte-bounded queue, segment roll, the ordered close (fsync, rename, index fsync, `recording.json`), retention prune, stats |
| `src/replay/reader.rs` | Segment-by-segment streaming reads with seek across segment boundaries |
| `src/replay/index.rs` | 24 B append-only `index.bin` entries (timecode → segment + offset), in-memory load + binary search |
| `src/replay/clips.rs` | `clips.json` persistence (atomic `tmp` + rename + fsync) |
| `src/replay/paced_replayer.rs` | Standalone PCR-paced `pump` (cancel-token only — no per-input command channel, so cue / scrub / stop cannot interrupt it mid-playback); **dead code today** (`#![allow(dead_code)]`, no callers) — the replay input paces inline in `engine::input_replay::pump_one_bundle` |
| `src/replay/filmstrip.rs` | Opt-in JPEG thumbnail subscriber; `thumbs/` writer + `.tmp/` orphan scan; `list_frames` / `read_frame` |
| `src/replay/recordings.rs` | On-disk recording enumeration for the Recordings library (including orphans with no live flow) |
| `src/replay/export.rs` | MPEG-TS clip / recording export |
| `src/replay/export_mp4.rs` | TS→progressive MP4 export, all-intra re-encode (`replay_export_mp4` capability) |
| `src/replay/clock.rs` | The one wall-clock ↔ PTS mapping the writer and the clip exporter share |
| `src/engine/input_replay.rs` | Replay input task; per-input command channel; lifecycle events |
| `src/engine/flow.rs` | `FlowRuntime.recording_handle` lifecycle; spawn/teardown |
| `src/manager/client.rs` (replay dispatch arms, `configure_recording` through the `cue_clip \| play_clip \| stop_playback \| scrub_playback \| set_speed \| step_frame` arm) | WS command dispatch — 18 arms covering 23 command names, all `#[cfg(feature = "replay")]`-gated |

## Storage layout

```
<replay_root>/
  <recording_id>/
    000000.ts            ← rolled, fsynced 188-byte-aligned MPEG-TS segment
    000001.ts
    ...
    NNNNNN.ts
    recording.json       ← schema_version, recording_id, created_at_unix, segment_seconds, current_segment_id, anchor_wall_us, anchor_pts_90khz
    index.bin            ← 24 B / IDR; binary search resolves PTS → (segment_id, byte_offset)
    clips.json           ← Vec<ClipInfo>; atomic write-to-tmp + rename + fsync
    .tmp/                ← in-flight writes; renamed onto the final path, in id order, once the closer has fsynced the segment
    thumbs/              ← filmstrip JPEGs, one per capture, named <pts_90khz>.jpg (only when filmstrip_seconds is set)
      .tmp/              ← in-flight filmstrip encodes; fsync + rename onto thumbs/; orphans unlinked on writer init
```

The same root is shared across recordings. Per-recording subdirectories
are named after `RecordingConfig.storage_id` (or the flow id when
unset). Resolution order for the root:

1. `BILBYCAST_REPLAY_DIR`
2. `$XDG_DATA_HOME/bilbycast/replay/`
3. `$HOME/.bilbycast/replay/`
4. `./replay/`

### `index.bin` entry format

Each entry is exactly 24 bytes, packed `u64 + u32 + u32 + u32 + u32`
(little-endian):

| Bytes | Field | Notes |
|---|---|---|
| 0..8 | `pts_90khz` | Recorded PCR-derived PTS at the IDR boundary |
| 8..12 | `smpte_tc` | Packed SMPTE timecode at the IDR (`0xFFFFFFFF` if unknown) |
| 12..16 | `segment_id` | The `NNNNNN` of the file the IDR lives in |
| 16..20 | `byte_offset` | Offset within the segment file |
| 20..24 | `flags` | Bit flags (see below) |

`flags` is a bitfield (`src/replay/index.rs::flag`); three bits are
defined and set by the writer:

| Bit | Constant | Meaning |
|---|---|---|
| `1 << 0` | `IS_IDR` | Entry marks an IDR / GOP boundary (set on every entry) |
| `1 << 1` | `PCR_DISCONTINUITY` | Set on the first IDR after a > 5 min PCR step (stream-source change) **and on the first IDR after a writer restart**; MP4 clip export refuses to cut across it (`InMemoryIndex::spans_discontinuity`, checked by both `export_mp4` and the DVR clip cutter in `engine::cmaf::clips`); the raw-TS export (`replay::export`) does not check it, and the playback pacer does not consult the flag |
| `1 << 2` | `SMPTE_TC_VALID` | `smpte_tc` holds a decoded SMPTE timecode for this IDR |

Entries are append-only; rebuild on corruption is a Phase 2 item — the
only corruption handled today is a truncated tail: `IndexWriter::open`
silently `set_len`s an `index.bin` whose length is not a multiple of 24
down to the last whole entry and continues to append, and every reader
(`InMemoryIndex::load`) ignores the trailing partial entry the same way.
Nothing else is validated (no magic, no CRC), and no event or log line
is emitted for a corrupt or truncated index — `replay_recovery_alert`
reads `index.bin` only for the segment ids it names (see Crash recovery
item 4), never for its integrity.

### The wall-clock anchor

`recording.json` carries a pair — `anchor_wall_us` (microseconds since the Unix
epoch) and `anchor_pts_90khz` — that turns a wall-clock instant into a PTS:

```
pts  = anchor_pts_90khz + (wall_us - anchor_wall_us) * 90_000 / 1_000_000
wall = anchor_wall_us   + (pts - anchor_pts_90khz)   * 1_000_000 / 90_000
```

It is taken once, on the first index entry appended, and preserved across
restarts. It is dated by when that frame came off the flow's broadcast, not by
when the writer got round to it — the writer's queue can hold seconds of the
feed, and the rolling sample below is dated the same way. Both fields are absent on recordings made before it existed; the clip
exporter refuses the exact cut on those and falls back to whole segments, which
is the honest answer — there is no mapping to cut through.

**And a second point, `recent_wall_us` / `recent_pts_90khz`,** re-taken every
60 s while the writer runs and carried across a restart with the anchor. One
point only gives a line if the rate is assumed, and the assumed 90 000 ticks per
wall-clock second is not what the counter does: `pts_90khz` advances on the
*source's* PCR, while a mark is placed from a wall-clock-true date. The CMAF half
of this same feature measured its own source at ~450 ppm slow and added a
slewing epoch because of it, recording that an epoch pinned once and held drifts
4.1 s across a 2h30m session — and the recorder's anchor is exactly such a
pinned epoch. Left at the nominal rate the cut drifts by (offset × the age of the
**recording**, not of the clip): 1.6 s after an hour at that figure, past the 2 s
segment-boundary fallback the exact cut exists to beat inside ninety minutes,
and unbounded on a 24/7 DVR flow — silently, because `find_floor` clamps rather
than erroring and hands back a playable clip of the wrong moment.

The exporter therefore takes its rate from `(recent - anchor)` when that span is
at least ten minutes and the result is within ±5 % of nominal, and from the
nominal 90 kHz otherwise. A shorter span is dominated by the arrival jitter of
the two sampled frames; a result outside the band is not a clock offset but a
corrupt or mis-paired sample, and extrapolating on it would be worse than
assuming nominal. The anchor end of the pair never moves, so the measurement
gets better as the recording ages rather than worse.

**Why not `created_at_unix`.** It is whole seconds, and it marks when the writer
opened rather than when the first frame landed. Measured against the CMAF
published dates over 124 s of media it sat a stable +0.47 s out, and on another
run −18.8 s. A mark converted through it lands about half a second from the
frame the operator chose — no better than cutting on segment boundaries, which
is the thing exact cutting exists to avoid. The anchor measured −36 ms on the
same rig, inside one frame at 25 fps.

### The PTS timeline across a restart

`pts_90khz` in the index is **not the stream's PTS**. It is a 64-bit counter the
writer accumulates from PCR deltas, so the index stays monotonic across the
33-bit PCR wrap — and `find_floor` binary-searches it, so monotonic is a
requirement, not a nicety.

A restart resumes an existing recording: it continues the segment numbering
(past any ids a crash lost — see [Segment ids](#segment-ids)), appends to the
same `index.bin`, and keeps the anchor. The counter therefore has
to be resumed too. It picks up at **the last indexed tick plus the wall-clock
time the writer was down** — the gap is added, not skipped, because nothing was
recorded during it but the anchor maps wall clock to ticks linearly, and a
counter that ignored the outage would place everything after it earlier by
exactly the downtime.

Starting the counter again at zero writes a second, overlapping timeline into
one file. Every wall-clock lookup after the restart then lands in a hole, and
any that resolves can match a frame from before it.

**Monotonicity does not depend on the anchor.** Dating the join does; keeping
the index sorted does not. A recording with no anchor — one written before the
pair existed, or one whose `recording.json` could not be read — resumes at the
last indexed tick plus one. Refusing to resume those was the sharper form of the
same bug, because they are precisely the recordings that have already restarted
at least once.

**The gap is measured when the media comes back, not when the writer opens.**
Sampling the clock at writer start dates every later entry early by the
spawn→first-frame interval — an SRT caller's reconnect, routinely a few hundred
milliseconds and unbounded when the source is down — and since the anchor is
never re-taken, that offset rides every clip cut from the rest of the recording.
It is the same error the anchor itself is taken on the first indexed frame to
avoid.

**Three kinds of gap, one rule.** A writer restart, an operator's Stop/Start
pair, and a PCR step too large to believe as elapsed time are all gaps in which
wall-clock time passed and no media was written. All three now date the gap at
the first frame back — by the source's own PCR step when there is a previous PCR
and the step is inside the 5-minute discontinuity threshold (a short Stop/Start,
during which the source's clock kept running), and through the anchor otherwise
(a restart, which has no previous PCR to compare against; a stop longer than the
threshold; and the over-threshold PCR step itself, which by definition cannot be
believed as elapsed time). A Stop/Start used to freeze the
counter outright and resume from the frozen value, so a ten-minute stop shifted
the rest of the recording ten minutes against the anchor — and
`spans_discontinuity` could not catch it, because the flagged entry sits *before*
the shifted window rather than inside it.

**The media is still discontinuous.** The index's own timeline is continuous
across the join, but the PCR in the TS begins again with the process, so the two
sides are two timelines however neatly the index numbers them. The first entry
after a resume therefore carries `PCR_DISCONTINUITY`, and anything muxing a
range has to honour it — see [Clip export](#clip-export) below.

## Lifecycle

### Recording arm → roll → prune

1. The subscriber, writer and closer tasks are spawned once, at flow start, by
   `FlowRuntime::start` → `replay::writer::spawn_writer` when
   `RecordingConfig.enabled = true`. With no `pre_buffer_seconds` the
   writer starts already `Armed`; with one it starts in `PreBuffer`.
   `start_recording { flow_id }` does not spawn anything — it sends
   `RecordingCommand::Start` to that writer (arming it from
   `PreBuffer`, or from `Idle` after a Stop) and fails with
   `replay_recording_not_active` on a flow with no recording configured
   (or `enabled = false`, which spawns no writer either).
2. `RecordingStats.armed = true`, and the `recording_started` Info
   event fires — at flow start when no `pre_buffer_seconds` is
   configured, or on the operator's Start while the writer is still in
   pre-buffer mode (that event's `details.pre_buffered_seconds` is the
   configured `pre_buffer_seconds`). With `pre_buffer_seconds` set,
   flow start emits `recording_pre_buffer_started` instead and `armed`
   stays `false` until that Start. A Start after a Stop (or while
   already armed) flips `armed` but emits no event.
3. The writer appends 188 B-aligned packets to a staging file under
   `.tmp/`. PCR is tracked from the data; SMPTE timecode is extracted
   via `engine::content_analysis::timecode::TimecodeTracker`.
4. Every `segment_seconds` (default 10 s, validated `[2, 60]`) of the
   feed's arrival time, the writer opens the next segment and hands the
   finished one to the **closer**, moving the segment id on past it. The
   IDR entries were already appended one by one in `write_chunk` as each
   random-access point was observed; the writer hands them to the kernel
   (no fsync) before the hand-over. The closer then, for each segment in
   the order they closed:
   - flushes the userspace buffer and `sync_all`s the segment;
   - renames `.tmp/<n>.ts` → `<n>.ts`. One closer, so renames land in id
     order: a reader or a clip export finds only the newest segments not
     yet in place, never a later one in place ahead of an earlier one still
     being finished. A segment can still be missing from the middle — a
     close that failed, or the segments a crash lost — and playback steps
     over it while an export across it is refused (see
     [Segment ids](#segment-ids));
   - fsyncs `index.bin` — **after** the rename, so the fsync issued for a
     segment never makes its entries durable while the segment is still in
     `.tmp/`. That is all the ordering buys: the writer is already
     appending entries for the open segment, and for any queued behind this
     one, and an fsync covers the whole file (as can the kernel's own
     writeback), so a power cut can still leave entries naming a segment
     that never left `.tmp/`. A restart resumes past every id the index
     names, so such an entry names a missing file, never media recorded
     after the restart;
   - writes `recording.json` (the next segment id, the clock anchor and
     its rolling sample);
   - runs the retention prune — oldest-first by mtime, capped by both
     `retention_seconds` and `max_bytes`. Either being `0` disables that
     axis (still bounded by free disk on the size axis).
   `segments_written` counts a segment once its close has succeeded.
5. On stop (`stop_recording`): the writer goes Idle
   (`RecordingCommand::Stop`) and hands the open segment to the closer,
   and the command is acknowledged only once the closer has finished
   every segment handed to it — a stopped recording is complete on disk,
   every segment renamed into place, in order. `RecordingStats.armed =
   false`. The writer task and its broadcast subscriber stay alive —
   `write_chunk` returns early while Idle, so the queue keeps draining
   but nothing reaches disk — and no event is emitted (there is no
   `recording_stopped` event). The segment id has moved on past the
   stopped segment, so a later Start opens a new one; until this was
   fixed it reopened the stopped segment's id, and the next roll renamed
   the new file over the last segment before the Stop, whose index
   entries still pointed into it. A flow that stops or restarts waits up
   to 10 s for its recorder to do the same (`RecordingHandle::finished`,
   one deadline for the writer and its filmstrip together), so a process
   that exits leaves every segment in place. A restarted writer does not
   rely on that wait: it opens the recording only once the previous writer
   and its closer have let it go (see [Crash recovery](#crash-recovery-writer-init-scan)),
   because a disk slow enough outran the 10 s and the new writer's `.tmp/`
   sweep deleted the segments the old one was still finishing. A node shutting down stops all its flows
   at once and gives them 10 s in all (`SHUTDOWN_FLOW_STOP_BUDGET`), so a
   stalled replay volume costs that wait once rather than once per
   recording flow, and the tunnels and bond routes torn down after the
   flows still go before systemd's `TimeoutStopSec=20`. On a fatal write error:
   Critical `replay_disk_full` (`replay_event: disk_full`); the partial
   segment goes to the closer, which renames it into place or, when that
   close fails too, unlinks its `.tmp/` staging file (without a second
   event); the writer stays armed (`armed` is not cleared) and the next
   roll boundary opens a fresh segment. What arrives meanwhile is dropped:
   its PCR is still followed, so the timeline keeps time across the gap,
   but no random-access point in it is indexed — an entry made then used
   to name segment 0, at an offset into packets nobody wrote, so a scrub
   or an export resolving it read the start of the recording instead.
6. A close that fails — flush, fsync or rename — unlinks the segment's
   staging file and raises a Critical event: `replay_disk_full` when the
   disk itself failed (ENOSPC, EDQUOT, EIO, EROFS), the fault the writer's
   own writes report, and `replay_segment_close_failed` for anything else
   (the staging file taken away, a rename refused). The segment is lost;
   its index entries remain, naming a file that does not exist; playback
   steps over the gap and an export across it is refused with
   `replay_segment_missing`. Writing carries on with the next segment.
7. Retention will **never** unlink the segment just finished — losing the
   live edge would tear out clips that touch the most recent few seconds.
   It runs after that segment's rename, so the segment is in the listing
   and in the total. A `max_bytes` cap smaller than one segment fires the
   Warning `replay_max_bytes_below_segment` at every close instead.

### Crash recovery (writer init scan)

Whenever `spawn_writer` runs (cold start, edge restart after SIGKILL,
operator-driven re-arm), it scans the recording directory before
opening a new segment — and first makes sure it is the only writer
there:

0. **One writer at a time.** A writer holds its recording's directory
   (`RecordingLease`, a per-directory lock in a node-wide registry) from
   before this scan until its closer has finished every segment and the
   index is synced; the writer state and the closer each keep a share, so
   the hold outlives whichever ends last, a panic included. A writer
   started while the previous one still holds it — a flow restarted while
   the old closer fsyncs on a slow disk — opens in its own task once the
   previous one lets go, so the flow's start never waits on it: what the
   flow publishes meanwhile queues for the writer within its 32 MiB budget
   (past it, `replay_writer_lagged`, as behind any slow disk), and a flow
   stopped meanwhile ends the wait. `FlowRuntime::stop`'s 10 s wait used to
   be all that kept the two apart, and a disk slower than that let the new
   writer's sweep below unlink the segments the old closer had yet to
   rename — and a segment the old writer had open with no index entry yet,
   an id nothing names, share its id with the new writer's first.

   An open that waited and then fails raises the same Critical
   `recording_start_failed` event a failure at flow start does, against the
   flow. The flow has already reported that recording started by then and
   keeps its handle, so the recorder goes on answering its commands until
   the flow stops — `start_recording`, `mark_in` and `mark_out` with
   `replay_recording_not_active`, as for a flow with no recorder
   (`stop_recording`'s handler codes nothing, so it carries that code at
   the head of its message) — where a writer task that simply ended left
   them failing with the uncoded "recording writer task is gone".

   A writer of another flow — two flows naming one recording through
   `storage_id` — waits the same way while that flow is stopping. One
   config push that moves a `storage_id` from one flow to another restarts
   the two in no set order, so the flow taking it can start before the one
   giving it up has begun to stop, and a deleted flow's closer can still be
   finishing when another flow takes its recording. Another flow still
   running is given 30 s (`HANDOVER_GRACE`) to begin stopping — long enough
   for the push to reach it — and if it has not, the waiting writer is
   refused with `replay_recording_in_use`, naming that flow, through the
   same `recording_start_failed` event and with the same answers to its
   commands. Refused at once, a handover was refused for good whenever the
   push restarted the taking flow first; waited for as long as the other
   flow runs, the writer would record nothing while its queue filled and
   raised `replay_writer_lagged` against the disk.
1. **`.tmp/` orphan cleanup.** Any `<NNNNNN>.ts` left in `.tmp/` is a
   partial segment the writer never atomically renamed onto the
   recording — unlinked unconditionally.
2. **Resume id derivation.** The next segment id is the largest of
   `recording.json`'s `current_segment_id`, the largest `<NNNNNN>.ts` on
   disk + 1, and the largest `segment_id` in `index.bin` + 1 (a
   brand-new recording, with none of them, starts at 0). The field is the
   lowest id not yet finished, so it is taken as it stands, never plus
   one. Neither of the other two is redundant: the meta is written by
   the closer after the rename it follows, so it can be missing, corrupt
   or behind the directory; and it trails the index by every segment the
   closer had not finished when the edge died, whose entries may already
   be in `index.bin`. Opening one of those ids again would point its
   entries at media recorded after the restart. See
   [Segment ids](#segment-ids).
3. **`index.bin` alignment.** If the file length isn't a 24-byte
   multiple (a SIGKILL between `append` and `flush_and_sync` can
   leave a partial entry), `IndexWriter::open` aligns down to the
   last valid boundary in place. `InMemoryIndex::load` applies the
   same rule on the reader side. The trailing partial IDR is
   discarded; the next IDR re-establishes the index head.
4. **Recovery alert.** If any of the above fired (orphans removed,
   meta corrupt, or the directory or the index held an id past the one
   the meta had reached — segments a crash lost while the closer was
   behind, or a meta write that failed), the writer
   emits the Warning event `replay_recovery_alert` with structured
   `details.tmp_orphans_removed`, `details.meta_corrupt`,
   `details.next_segment_id`. Recovery is non-fatal; the recording
   continues from the next id without operator intervention.

### Segment ids

A segment id is handed out once in a recording's life. A segment's file,
its `index.bin` entries and the clips cut through them all name it by id,
so an id that came round again would point every one of them at media
recorded later. The writer (`replay::writer`, whose module docs state the
same rules) keeps it so:

- **Every close moves the id on** — a roll, a Stop, a write error, the
  writer exiting. A Start after a Stop therefore opens a new id, never the
  stopped one.
- **A writer resumes past every id anything names**: the largest of the
  meta's `current_segment_id`, the largest segment on disk + 1 and the
  largest id in `index.bin` + 1 (see [Crash recovery](#crash-recovery-writer-init-scan)).
  After a clean stop — a Stop, a flow restart, a node restart — the three
  agree and the numbering carries straight on. After a crash the index
  term resumes past the segments the crash lost, so their entries name a
  file that is missing rather than media recorded after the restart; an
  id nothing names yet (a segment lost before its first random-access
  point) is used again.
- **One writer per recording at a time.** A writer resumes past what the
  directory, the meta and the index say only if no other writer is still
  adding to them; a restarted writer waits for the previous one (step 0 of
  [Crash recovery](#crash-recovery-writer-init-scan)).
- **A gap in the ids is not the end of the recording.** A crash leaves one
  where the segments it lost were, and so does a close that fails.
  Playback (`Reader::advance_segment`) steps over it to the next segment
  on disk; it used to stop at the first missing id, ending a replay at the
  gap. The gap is not closed instead, because that would mean handing the
  lost ids out again and first cutting their entries out of `index.bin` on
  the strength of a directory scan — and a failed close leaves a gap
  whatever the numbering does.
- **An entry can name a segment the disk does not hold**, and everything
  that turns an entry into a read checks first. Retention prunes segment
  files and never index entries, and a crash or a failed close leaves
  entries naming a file that was never finished. So:
  - a replay of the whole recording, and the first chunk of an export with
    no `from_pts_90khz` for a manager that follows the export cursor, start
    at the first entry whose segment is still on disk
    (`reader::recording_start`) — the first entry outright named a pruned
    file on any recording old enough, and the replay failed at its first
    read. The rest of that export's chunks are read from the cursor each
    chunk hands out (see [`export_clip` /
    `export_recording`](#export_clip--export_recording));
  - a cue, scrub or play into a lost window lands on the first random-access
    point after it still on disk (`Reader::playable_from`), and the
    `scrub_playback` ack names where it landed. That is where playback from
    before the hole arrives anyway, by stepping over it; the last point
    before the hole could be minutes of media nobody asked for. A clip, or a
    played range, with nothing left before its end is refused with
    `replay_segment_missing`, and so is a lost tail with nothing after it.
    A `play_clip` with a `from_pts_90khz` lands within the range it asks
    for — bounded by its own `to_pts_90khz`, else by the clip it names,
    else by the range already in play — where it was bounded by the range
    played before it, so a Play past an earlier range's end was refused
    though its own range had media after the hole. A Play refused for any
    reason — its range, its clip, its landing, its `speed`, its
    `start_at_unix_ms` — leaves playback as it was: the reader, its range
    and the pacer, its speed included. Every check runs before anything
    moves, and the clip a Play names is opened beside the reader in play
    and takes its place only once the Play has landed on it. The clip used
    to take its place first, so a refused Play of a clip put that clip on
    air from its in-point while the operator was told it had failed, and a
    refused speed or start time came after the reader had moved and the
    pacer had been reset;
  - an export (TS or MP4, clip or range, and the DVR clip cutter's exact
    cut) that needs a segment `index.bin` names but the disk lacks is
    refused with `replay_segment_missing`. It skipped the segment and
    returned a file short of the range with nothing to say so — a clip
    spanning the hole played straight across it. The DVR cutter takes the
    refusal like any other exact-cut failure and cuts from whole segments
    on the origin instead (see [When the recording cannot serve the
    moment](#when-the-recording-cannot-serve-the-moment)). A gap no entry
    names — a segment with no random-access point whose close failed, or
    the hole an older build left in the ids at every restart, which lost
    nothing — is stepped over, as playback steps over it. An export's
    `total_bytes` is the sum of exactly the segment windows it reads; it
    was worked out separately, and a missing last segment was counted
    there and not read. The start's own segment is checked whatever the
    end: an export from a lost tail with no `to_pts_90khz` is refused,
    where it came back as an empty file and a success;
  - an entry can also name a segment that is **not on disk yet**: the
    writer indexes the open segment's random-access points as it sees them,
    and the segments waiting on the closer until each is renamed into
    place. A cue, scrub, play or export that reaches one — a mark-out at
    "now" exported at once, a range ending in the newest few seconds — is
    refused with `replay_segment_not_ready`, which is transient: the same
    request succeeds once the segment has closed, within `segment_seconds`
    plus the closer's lag. What tells the two apart is the live writer's
    hold on the directory (`RecordingLease`, `writer::is_being_written`):
    while a writer holds it, every staging file in `.tmp/` is one of its
    segments still to come; with nobody writing, a staging file is a
    crash's leftover and the segment it names is lost. The finished path is
    looked at again after the staging one, since the closer may rename
    between the two (`replay::segment_state`).

### Clip create

1. Operator presses **I** (or `mark_in`). The WS arm sends
   `RecordingCommand::MarkIn { explicit_pts }` down the writer's
   `RecordingHandle.command_tx` (`explicit_pts` is the command's
   optional `pts_90khz`). If `pts_90khz` is omitted, the writer's
   current PTS (most recent PCR-derived) is used. Reply carries the
   resolved `pts_90khz` and (best-effort) the SMPTE timecode at that
   PTS.
2. Operator presses **O** (or `mark_out`). The writer materialises a
   `ClipInfo { id, name, in_pts_90khz, out_pts_90khz, created_at_unix,
   created_by, description }`, appends it to `clips.json` via the
   atomic write-tmp + rename + fsync path, and emits a `clip_created`
   Info event.
3. The "Save last 10 / 20 / 30 / 60 s" quick-clip buttons in the UI do
   the two steps in one go — `mark_in { pts: now − N s }` then
   `mark_out { name }`. This is the bread-and-butter sports workflow.

### Playback

The `replay` input config carries `start_paused: bool` (default `true`)
— when true, the input idles on flow start with NULL-PID padding until
an explicit `play_clip` / `cue_clip` arrives. This is the safe default
for live workflows where a flow start should not immediately push
recorded content to downstream outputs. Set `start_paused: false` for
auto-play scenarios (e.g., a routine that brings up a flow already
pointed at a known clip).

1. Operator selects a clip in the UI sidebar. `cue_clip { clip_id }`
   pre-loads the clip without rolling.
2. **L** key (or `play_clip`) starts playback. `input_replay`
   transitions to playing; `playback_started` Info event.
3. The replayer reads the segment containing
   `clip.in_pts_90khz` (binary-search on the in-memory index → IDR
   ≤ target), seeks to the IDR's byte offset, and yields paced bundles
   to the broadcast channel. When that segment is no longer on disk it
   starts at the first IDR after it that is, and refuses a clip with
   nothing left before its out-point (`replay_segment_missing`, or
   `replay_segment_not_ready` while what comes next is still being
   recorded; see [Segment ids](#segment-ids)).
4. **K** key or `stop_playback` halts playback (`playback_stopped`
   Info). On reaching the end of the recording on disk (no next
   `<NNNNNN>.ts` segment) with `loop_playback = false` the replayer
   fires `playback_eof` and idles with NULL-PID padding; with
   `loop_playback = true` it wraps to the range's `from_pts_90khz` at
   that same point — and when that start can no longer be played
   (retention removed it, a crash lost it, the recording was deleted under
   the replay) it stops instead, with a Warning `playback_loop_failed`
   naming the refusal (`details.error_code`: `replay_segment_missing`, or
   `replay_segment_not_ready`). It used to ignore the refusal and spin at
   the end of the range with playback on, listing the recording directory
   on every turn of the input's loop. The clip's `out_pts_90khz` (and a `play_clip`
   `to_pts_90khz`) is recorded on the reader's range and reported in
   the `playback_started` event, but is not enforced by the inline
   pump today — a cued clip plays through to the end of the
   recording. The out-point check exists only in the unused
   `replay::paced_replayer::pump`.

## Error matrix

Two disjoint channels carry a replay `error_code`, and no code rides on
both. Recorder-side failures — the writer (`replay::writer`), the
flow-start arming path (`engine::flow`, which raises `replay_disk_full`
when the recorder fails to start) and the filmstrip subscriber
(`replay::filmstrip`) — happen asynchronously with no command in
flight, so they emit a structured event under category `replay` with
`details.error_code` and surface on the manager's Events page. Command
rejections (`mark_in` / `mark_out` / `play_clip` / `update_clip` /
`delete_recording` / the export verbs, …) carry the code on
`command_ack.error_code` **only** — no event is raised — so the manager
UI can highlight the offending field on a Create/Update modal without
parsing strings.

| `error_code` | Severity | Trigger | Operator action |
|---|---|---|---|
| `replay_recording_not_active` | Error | `start_recording` / `stop_recording` / `mark_in` / `mark_out` on a flow whose `recording` block is absent or has `enabled: false` (no writer is spawned at flow start — `engine/flow.rs` filters on `enabled`); `start_recording` on an unknown `flow_id` (the other three return a bare error with no code for that); `mark_in` without an explicit `pts_90khz` on a fresh recording before the writer has seen its first PCR (the counter is still 0), or `mark_out` with no pending `mark_in` (or with `pts_90khz` at or before the pending in-point — the arm lifts every writer-side error under this code); any of them on a flow whose recorder failed to open after waiting for its previous writer (see `recording_start_failed`) | Add a `RecordingConfig` with `enabled: true` via `configure_recording` (the edge reports `restart_required`; the recorder binds at the next flow start) or `update_flow` (`recording` is a restart-triggering field). For the mark cases, wait for the source to deliver, or send `mark_in` first |
| `replay_no_playback_input` | Error | `play_clip`/`scrub_playback` on a flow with no `replay` input | Add a `replay` input to the flow |
| `replay_clip_not_found` | Error | `play_clip`/`delete_clip` with an unknown `clip_id` | Refresh the clip list (it may have been pruned by retention) |
| `replay_writer_lagged` | Critical | The writer's queue is past its 32 MiB budget, or past its own first 4 MiB with the node's recorders past the 256 MiB they share — the writer has been seconds behind its feed — and the recorder dropped packets | Check disk throughput; reduce concurrent recording flows; investigate fs latency |
| `replay_disk_pressure` | Warning | Recording usage ≥ 80 % of `max_bytes` (or of replay-root filesystem when `max_bytes = 0`); sticky until back below 70 % | Free disk before ENOSPC; raise `max_bytes` if appropriate; reduce retention |
| `replay_recording_in_use` | Critical (`recording_start_failed`) | The flow's recorder was not started: another flow's writer was still working in the same recording directory 30 s after it was asked to start, and that flow was not stopping — both flows' recordings resolve to one `storage_id`, or one flow's `storage_id` names another flow's id. Nothing in validation refuses the pair, and the two writers used to corrupt the directory between them. A recording moved from one flow to another is handed over instead, whatever order the push restarts the two in | Give one of the flows its own `recording.storage_id`, then restart it |
| `replay_disk_full` | Critical | A segment write hit a disk error (typically ENOSPC), or a finished segment's close (flush, fsync, rename) failed on the disk itself — ENOSPC, EDQUOT, EIO or EROFS; the latter carries `details.segment_id`, and that segment is lost | Free disk, then `stop_recording` + `start_recording` to re-arm |
| `replay_segment_close_failed` | Critical | A finished segment's close failed for a reason other than the disk — its staging file removed from under the writer, a rename refused (permissions, something else at the final path). The staging file is unlinked and the segment lost; `details.segment_id`. Recording carries on with the next segment | Check the recording directory's ownership and permissions, and that nothing else writes into it |
| `validation_error` | Error (`command_ack.error_code` only — no event) | `configure_recording`, `create_flow`, `update_flow` or `update_config` carrying a `recording` block whose `segment_seconds` is outside `[2, 60]`, `pre_buffer_seconds` outside `[1, 300]` or above a non-zero `retention_seconds`, `filmstrip_seconds` outside `[1, 30]`, `storage_id` not 1–64 ASCII alphanumerics + `_-`, or no `storage_id` on a flow whose id is not a single path component (`.`, `..`, or containing `/` `\`). The same check refuses `config.json` at node start, where there is no ack at all | Use a value in range, or set a valid `storage_id` |
| `replay_storage_id_invalid` | Error (`command_ack.error_code` only — no event) | `delete_recording` whose `recording_id` is empty, > 64 chars, or contains a character outside ASCII alphanumerics + `_-`, or whose directory could not be removed (`remove_dir_all` failed; a directory that does not exist is acked with `bytes_freed: 0`, not refused). A bad `RecordingConfig.storage_id` is never reported under this code — it is refused as `validation_error` (row above), and `.` is not permitted in the charset | Use a valid id / check the replay volume |
| `replay_invalid_field` | Error | `mark_out` / `rename_clip` / `update_clip` `name` > 256 chars or contains control chars; `description` > 4096 chars; an export chunk's `resume` this edge would not have handed out | Trim to limits; export again from the start |
| `replay_invalid_range` | Error | `update_clip` whose prospective `out_pts_90khz <= in_pts_90khz` (empty or inverted); `export_recording` (TS or MP4) with both bounds supplied and `to_pts_90khz <= from_pts_90khz`; `export_clip` (TS or MP4) on a clip whose stored `out_pts_90khz <= in_pts_90khz`. `play_clip` with `to_pts_90khz < from_pts_90khz` (or below the clip's `in_pts` when `from` is omitted) is refused too, but its ack carries `replay_clip_not_found` with `replay_invalid_range` as the message; `scrub_playback` takes a single `pts_90khz` and cannot raise it | Pass a forward range |
| `replay_invalid_tag` | Error | `update_clip` (or any tag-bearing path) with a tag that fails `[A-Z0-9_-]{1,32}`, more than 16 tags per clip | Use the v1 fixed set (`GOAL`/`FOUL`/`OFFSIDE`/`SAVE`/`YELLOW`/`VAR-CHECK`) or shorten / re-case |
| `replay_segment_missing` | Error (in the error text) | An export (`export_clip` / `export_recording`, TS or MP4) needs a segment `index.bin` names that is not on disk — lost to a crash or a failed close, or pruned by retention; or a `cue_clip` / `play_clip` / `scrub_playback` lands in a lost window with no recorded media after it before the end of the range; or a whole-recording replay or export finds none of the index's segments on disk; or a later chunk of a TS export still owes bytes from a segment that has gone since the download began; or a manager that follows no export cursor asks for a recording whose first indexed segment is gone. Settled, not transient — told from a segment still being recorded, which is `replay_segment_not_ready`. `dispatch_replay_input_command` re-codes cue / play / scrub errors onto `replay_clip_not_found`, and the export verbs' `error_code` mapping in `src/manager/client.rs` does not name it yet, so an export ack carries `replay_export_failed` with this code at the head of the message | Export each side of the gap separately, or move the clip clear of it; a moment the recorder lost may still be on the relay's origin (the DVR cutter falls back to it by itself) |
| `replay_segment_not_ready` | Error (in the error text) | A cue, play, scrub or export (TS or MP4) reaches a segment `index.bin` already names that the live writer has yet to put in place — the open segment, or one waiting on its closer: a mark-out at "now" exported at once, a range ending in the newest few seconds; or a whole-recording replay or export of a recording whose first segment is still open. Transient. Carried in the error text the same way as `replay_segment_missing` (cue / play / scrub re-coded onto `replay_clip_not_found`, an export ack carrying `replay_export_failed`) | Retry once the segment has closed — within `segment_seconds` plus the closer's lag |
| `replay_max_bytes_below_segment` | Warning | Retention can't satisfy `max_bytes` without deleting the live edge — operator's cap is smaller than one segment | Raise `max_bytes` to at least `segment_seconds × bitrate × 2` |
| `replay_metadata_stale` | Warning | `recording.json` write failed after a segment's close; on restart the resume id is bounded by the directory listing and `index.bin` as well, so no id is reused | Investigate the disk (typically ENOSPC on the replay volume) |
| `replay_recovery_alert` | Warning | Edge restarted after a crash; orphan `.tmp/` segments cleaned, `recording.json` was corrupt, and / or ids were in use past the one `recording.json` had reached | Informational — verify `details.tmp_orphans_removed` and `details.next_segment_id` match expectations |
| `replay_filmstrip_setup_failed` | Warning | Filmstrip writer could not create `<recording_dir>/thumbs/.tmp/`; the subscriber returns and no filmstrip is written (the recording itself keeps rolling) | Check permissions / free space on the replay root, then re-arm the recording |
| `replay_filmstrip_decode_failed` | Warning | Filmstrip decode failed on a capture tick; rate-limited to one event per 60 s per recording | Informational unless sustained — check the source has decodable video (`details.detail` carries the decoder error) |
| `replay_filmstrip_frame_not_found` | Error (`command_ack.error_code` only — no event) | `get_filmstrip_frame` for a `pts_90khz` with no `thumbs/<pts>.jpg` | Re-run `list_filmstrip` — the frame may have been pruned with its recording, or the PTS was not a capture tick |

### Orphan-recovery list_clips

`list_clips` accepts either `flow_id` (the normal manager UI path —
resolves the flow's recording via `FlowRuntime`) or `recording_id`
(direct lookup against the on-disk recording, even if no flow
references it any more). The latter is the recovery path when a
flow has been deleted but its segments + clips persisted on disk
under the same `<recording_id>` — the operator points a fresh flow
at it via a `replay` input and uses `recording_id` to enumerate the
clips. When both fields are present, `recording_id` wins and `flow_id`
is not consulted.

Disk-pressure monitoring runs alongside the reactive ENOSPC handling.
After every segment close the closer computes a usage percentage —
`(on-disk segment bytes after prune) / max_bytes` when the operator
set a per-recording cap, or filesystem `(total - free) / total` when
`max_bytes = 0`.
Crossing 80 % emits a sticky `replay_disk_pressure` Warning;
recovery is signalled when usage falls back below 70 % (hysteresis
on the same sticky bit), so a continuously-pressured recorder
doesn't spam the events feed. The same numbers ride on
`recording_status` (`max_bytes`, `replay_root_free_bytes`,
`replay_root_total_bytes`) so the manager `/replay` page can
render a coloured disk meter as soon as the recorder is armed.

## Metrics

`RecordingStats` is sampled at 1 Hz onto the WS stats path under
`FlowStats.recording`. All counters are lock-free `AtomicU64`.

| Field | Meaning |
|---|---|
| `armed` | `true` while a recording session is active. Pre-buffer mode keeps `armed = false` so the manager UI can distinguish pre-roll from a recording session and the stall detector doesn't fire on pre-buffered flows. |
| `mode` | Phase 2 / 1.5 — wire-string mirror of [`WriterMode`]: `"armed"` when a session is live, `"pre_buffer"` when the writer is rolling pre-roll TS but the operator hasn't pressed Start, `"idle"` when the writer is stopped — after any Stop (pre-buffer configured or not; a Stop never returns to `pre_buffer`), or after cancel. Drives the `/replay` page's tri-state `Recording / Pre-roll / Idle` badge and the flow-card `● PRE-ROLL` chip. Older edges omit the field; the manager falls back to `armed`-derived state. |
| `segments_written` | Segments closed successfully — fsynced and renamed into place. A segment whose close failed is not counted (see `replay_disk_full` / `replay_segment_close_failed`), and one still waiting for the closer is not counted yet |
| `bytes_written` | Total bytes appended (across all segments, including pruned) |
| `segments_pruned` | Segments evicted by retention (mtime / size) |
| `packets_dropped` | Packets dropped at the broadcast subscriber — the broadcast lagged it, or the writer's queue was past its 32 MiB budget (or past its own first 4 MiB with the node's recorders past their shared 256 MiB) |
| `index_entries` | Entries in `index.bin` (one per IDR) |
| `current_pts_90khz` | Most recent PCR-derived PTS; `0` until the first PCR is seen |

Wire shape: see [`metrics.md`](metrics.md#replay-server-metrics). Manager
side: the `/replay` page polls `recording_status` every 1 s for live
display of these counters.

## Capability gate

The edge advertises `"replay"` in `HealthPayload.capabilities` only
when compiled with the feature. The manager UI reads this list and
hides every record/replay surface when the capability is absent —
flow-form recording fields, the dedicated `/replay` page link, the
"Open Replay" badge on flow cards. Manager → edge replay commands sent
to a non-replay edge fall through to the generic `unknown_action` ack
path, so old edges don't trip on new commands.

`"clip-export"` is a separate bit and is advertised **unconditionally**,
because `engine::cmaf::clips` is compiled in unconditionally. It answers a
different question: *will a mark a viewer places actually get cut?* An edge
that predates clip export runs no poller, so every mark stays pending on the
relay for the life of the session while the node streams perfectly — a
degradation indistinguishable from success on every surface an operator looks
at, which is why the manager refuses to provision a DVR session against a node
that does not advertise it. The bit promises a clip, not a frame-exact one:
cutting to the frame additionally needs the `replay` recorder armed on the
source flow (see [Clip export](#clip-export)), and without it the exporter
falls back to whole segments.

### Arming the recorder: `configure_recording`

A DVR session points at whatever flow an operator nominates, including one
hand-written in this node's own config that the manager has no row for.
`update_flow` could arm it, but only by taking a **whole** `FlowConfig` the
caller would have to reconstruct — and every field it did not model would be
silently lost. `configure_recording { flow_id, recording }` sets or clears one
flow's `RecordingConfig` and persists it, touching nothing else;
`"recording": null` clears it, which is what a session teardown wants.

Three properties worth knowing:

* **It validates.** The block goes through `validate_recording_config` before
  it reaches `config.json`. Skipping that was a latent node-down bug: the
  flow-id charset is wider than the replay-id charset, so arming a session on a
  legally-named flow such as `stadium.cam1` persisted a `storage_id` the
  boot-time validator refuses — the node ran on until its next restart and then
  would not come back, with no manager socket left to repair it through.
* **It merges, it does not replace.** A caller that models five keys does not
  delete the two it does not (`pre_buffer_seconds`, `filmstrip_seconds`). An
  explicit `null` still clears a field; an absent key means "not modelled".
* **`restart_required` answers "did the value change", not "is the flow up".**
  The recorder binds at flow spawn, so a genuine change does need a restart —
  but a byte-identical re-arm (a second DVR session on a flow already recording
  at an equal-or-wider window) must not take a live feed off air for a write
  that changed nothing.

The field has two owners as a result, so a whole-config push that does not
model `recording` holds the value the node already has rather than clearing it
— the same rule, and the same reason, as `hold_active_inputs`.

## Filmstrip

Opt-in JPEG thumbnails written alongside the segments so the manager's
`/replay` scrubber can render a strip without decoding the recording.
Off unless the flow sets `RecordingConfig.filmstrip_seconds` (validated
`1..=30`; `None` = no filmstrip, and no extra decode cost on a
recording-only flow). Capability bit: `replay-filmstrip`.

It is a **sibling** of the segment writer, not a stage in it — the
filmstrip task takes its own `broadcast_tx.subscribe()` on the flow's
channel and drops on `Lagged` rather than awaiting, so a slow JPEG
encode can never stall recording or the data path. On each cadence tick
it decodes one frame out of the buffered TS and writes
`<recording_dir>/thumbs/<pts_90khz>.jpg` at 160x90, via
`thumbs/.tmp/<pts>.jpg` + `sync_all` + rename. A tick whose PTS already
has a file (a stalled source that hasn't advanced its clock) is a no-op
rather than an overwrite. Writer init unlinks any `.tmp/*` orphans left
by a SIGKILL mid-encode — filmstrip frames are PTS-keyed, so unlike the
segment writer there is no "next id" to derive.

Counters (`frames_written`, `bytes_written`, `decode_drops`) accumulate
on `FilmstripStats`, hung off the recording handle. They are not on the
`FlowStats.recording` wire shape yet.

### `list_filmstrip`

Frame **metadata** for a window — deliberately no inline JPEG, because
200 frames × ~5 KB is ~1 MB in one WS round-trip.

```jsonc
// Request — provide recording_id or flow_id. `recording_id` is
// checked first and wins; flow_id resolves through the flow's live
// recording handle. Same order as list_clips.
{
  "type": "list_filmstrip",
  "flow_id": "flow-1",
  "from_pts_90khz": 0,        // optional window start
  "to_pts_90khz": 5400000,    // optional window end
  "max_count": 60             // optional, default 60, clamped to 1..=200
}

// Response
{
  "recording_id": "show-a",
  "frames": [ { "pts_90khz": 91000, "size_bytes": 4812 }, … ],
  "total_count": 1873          // before downsampling
}
```

When the window holds more than `max_count` frames the edge returns an
evenly-spaced subset that always includes the first and last entry, so
the strip's edges line up with the requested window.

### `get_filmstrip_frame`

One JPEG by exact PTS — the companion fetch behind each strip cell.

```jsonc
// Request
{ "type": "get_filmstrip_frame", "recording_id": "show-a", "pts_90khz": 91000 }

// Response
{ "recording_id": "show-a", "pts_90khz": 91000, "data": "<base64 JPEG>", "width": 160, "height": 90 }
```

`<pts>.jpg` is content-addressed and never rewritten once flushed, so
the manager proxies it with a long `Cache-Control` and a strong ETag.
An unknown PTS returns `replay_filmstrip_frame_not_found`.

## Phase 2 / 1.5 — clip tags + `update_clip`

Clips carry an optional `tags: Vec<String>` for sports / VAR
workflows. Bounds:

- Each tag matches `^[A-Z0-9_-]{1,32}$` (operator-friendly enum
  shorthand — `GOAL`, `FOUL`, `VAR-CHECK`, etc.).
- ≤ 16 tags per clip. Server-side dedup'd in input order.
- The manager's quick-tag bar is configured per group rather than
  hard-coded: Admin → Groups → Replay quick-tags (`/admin/replay-tags`)
  stores up to 20 named profiles on `groups.replay_tag_profiles`, each
  of at most 9 tags (one per hotkey 1–9), seeded from built-in sport
  presets in `shared/replay_tag_presets.js`; each operator picks their
  active profile per group (`ui_preferences.replay_tag_profile_id`). A
  group with no profile config falls back to the built-in Soccer preset
  (`GOAL`, `FOUL`, `OFFSIDE`, `SAVE`, `YELLOW`, `VAR-CHECK`); a group
  that saved an empty list hides the bar. The edge
  stores whatever the manager sends, validating shape only
  (`replay::clips`), which is why that customisation needed no edge
  release.

### `update_clip` (the unified clip-mutation command)

```jsonc
{
  "type": "update_clip",
  "clip_id": "clp_…",
  "name": "Goal — Smith 24'",        // optional
  "description": "Header into top corner", // optional
  "tags": ["GOAL"],                   // optional, replaces the existing list
  "in_pts_90khz":  91000,             // optional (Phase 2 / 1.5 trim)
  "out_pts_90khz": 360000             // optional (Phase 2 / 1.5 trim)
}
```

At least one of `name` / `description` / `tags` / `in_pts_90khz` /
`out_pts_90khz` must be set. Returns the updated `ClipInfo`.

**SMPTE TC handling on trim.** Every `index.bin` entry carries a packed
SMPTE timecode (`smpte_tc: u32`, bytes 8..12, flagged
`SMPTE_TC_VALID`), and the writer already resolves `HH:MM:SS:FF` for an
arbitrary PTS through it (`lookup_smpte_in_index` + `unpack_smpte_tc`,
used by `mark_in` / `mark_out`). `update_clip`, however, runs through
`ClipStore` alone, which holds no index handle, so it does not
re-derive the timecode for a new in/out PTS. When
`in_pts_90khz` is set, `smpte_in` is cleared on the clip (and likewise
for the out side); the manager UI renders `—` until the operator
re-marks. Persisting a stale SMPTE that no longer matches the PTS
would mislead operators worse than the blank.

`rename_clip` continues to work unchanged — the manager's PATCH proxy
auto-routes to `rename_clip` when only `name` / `description` are
present, and to `update_clip` when any tag / PTS field is set, so old
edges keep accepting the legacy shape.

## Current limitations

- **Forward playback only, 0.1×–1.0×.** Variable-speed / slow-motion
  forward playback has shipped (the `play_clip` / `set_speed` `speed`
  param rewrites PCR + PES PTS/DTS in `input_replay.rs`, Phase 2.4).
  **No reverse playback** — the reverse-scrub mode and the
  audio-on-scrub toggle remain follow-ups.
- **Seeks snap to the nearest IDR ≤ target.** Frame-accurate
  scrubbing is still a follow-up.
- **No index rebuild on corruption.** The only check `index.bin` gets
  on open is length alignment: a file whose length is not a multiple
  of 24 bytes (a SIGKILL between `append` and `flush_and_sync`) is
  silently truncated down to the last whole entry by
  `IndexWriter::open`, the reader (`InMemoryIndex::load`) ignores the
  same partial tail, and the writer keeps appending. There is no CRC
  or content validation, no `replay_index_corrupt` event, and nothing
  is surfaced to the operator (the start-up `replay_recovery_alert`
  reads `index.bin` only for the segment ids it names, not for
  corruption). Phase 2
  will rebuild from segments at open time.
- **Clip IDs are edge-side generated.** Cross-edge collision
  handling isn't in scope — clip IDs are scoped to a recording.
- **SMPTE TC cleared on trim.** When `update_clip` changes
  `in_pts_90khz` / `out_pts_90khz`, the corresponding `smpte_in` /
  `smpte_out` is cleared and the manager UI shows `—` until the
  operator re-marks. This is not an index-schema gap: every
  `index.bin` entry already carries a packed SMPTE TC (bytes 8..12,
  `SMPTE_TC_VALID` flag), and `mark_in` / `mark_out` already resolve
  `HH:MM:SS:FF` from it by floor-IDR lookup (`lookup_smpte_in_index`
  in `src/replay/writer.rs`). `ClipStore::update` simply never
  consults the index — the `update_clip` handler opens the store from
  `clips.json` alone. Closing the gap means loading the recording's
  `index.bin` (`InMemoryIndex::load`, as the reader does) in that
  handler and running the same floor lookup for the new PTS (the
  lookup helper is private to the writer today and would need
  exposing); no schema change is needed.

## Recordings library (browse / export / delete after recording stops)

When a flow's `recording.enabled` flips off — or the flow is deleted
entirely — the on-disk recording under `<replay_root>/<recording_id>/`
keeps its segments + index + clips. They remain playable via the
existing `list_clips { recording_id }` orphan-recovery path; the
**Recordings library** surface exposes them so an operator can
browse, export, or delete them without re-arming the writer.

### `list_recordings`

Enumerate every recording directory under the replay root.

```jsonc
// Request
{ "type": "list_recordings" }

// Response
{
  "recordings": [
    {
      "recording_id": "show-a",
      "flow_id": "flow-1",          // null when no flow currently has it armed
      "armed": true,                // true while the writer is rolling
      "segment_count": 187,
      "total_bytes": 1_843_200_000,
      "first_pts_90khz": 0,
      "last_pts_90khz": 168_300_000,
      "created_at_unix": 1714000000,
      "last_modified_unix": 1714001872,
      "clip_count": 4
    }
  ],
  "replay_root_free_bytes": 53_500_000_000,
  "replay_root_total_bytes": 250_000_000_000
}
```

`flow_id` is `null` when the recording is an **orphan** — its source
flow has either disabled recording or been deleted. The manager UI
renders these with an `(orphan)` chip; they're still playable
through the JKL surface keyed off `recording_id`.

### `delete_recording`

```jsonc
// Request
{ "type": "delete_recording", "recording_id": "show-a" }

// Response
{ "recording_id": "show-a", "bytes_freed": 1_843_200_000 }
```

Refuses with `error_code: replay_recording_active` if the writer is
currently armed against the recording — operator must
`stop_recording` first. The directory unlink is recursive and
irreversible; the edge emits a `recording_deleted` Info event with
`details.bytes_freed` so the action is auditable.

### `export_clip` / `export_recording`

Pull-based chunked TS export. The manager makes repeat calls with
increasing `byte_offset` until the response carries `eof: true`. The
edge re-opens the on-disk reader stateless-ly on each call —
`InMemoryIndex::load` is cheap and the segment files are byte
aligned, so no per-session bookkeeping is needed: where a download
stands travels with it, in `resume`.

```jsonc
// Request
{
  "type": "export_clip",
  "clip_id": "clp_…",
  "format": "ts",            // optional; "ts" (default) or "mp4"
  "byte_offset": 0,          // optional; default 0
  "chunk_bytes": 1048576,    // optional; default 1 MiB, hard cap 3 MiB
  "pin_plan_start": true,    // optional; this manager follows `resume` — see below
  "resume": { … }            // optional; the previous chunk's `resume`, verbatim
}

// Response
{
  "clip_id": "clp_…",
  "recording_id": "show-a",
  "format": "ts",
  "byte_offset": 0,
  "total_bytes": 12_345_678,
  "chunk_bytes": 1048576,
  "data": "<base64>",
  "eof": false,
  "resume": {                // TS, every chunk but the last; see below
    "segment": 360, "offset": 1048576,
    "end_segment": 372, "end_offset": 1203840,
    "remaining": 11297102
  }
}
```

**Every chunk of one download is read from one plan.** Being stateless,
each call used to plan the export afresh and read its `byte_offset` against
that plan, and a start the edge resolves for itself moves: an
`export_recording` with no `from_pts_90khz` starts at the first segment
retention has kept, and a recording at its retention limit loses that
segment every time one closes — so a download spanning a prune had every
later chunk read a segment further on, a file of the right length with that
much cut out of the middle and nothing to say so. An `export_clip` had the
same fault through `update_clip`, whose trim moved the in-point under the
download.

So every TS chunk but the last answers with `resume`: the segment and the
byte in it where the next chunk starts, the segment and byte the plan ends
at, and how many bytes lie between. It is this edge's to define; the
manager does not read it, and sends it back unchanged with the next request,
beside the `byte_offset` it goes on counting. A chunk that carries it is
read from that position to that end, with no start resolved again:

- segments behind the cursor are never looked at. They are delivered, and
  they are what retention removes first, so a long download of a recording
  at its retention limit outlives every prune behind it — planned from its
  first chunk's start on every chunk instead, it failed at the first prune
  with every byte it still owed on disk;
- a segment still owed that has gone is refused, `replay_segment_missing` —
  by name, or, for a segment no entry names (one with no random-access
  point in it, which a plan steps over when it is missing), by the bytes
  left no longer adding up. The manager ends the download short of its
  `Content-Length` and logs the edge's text. One still owed that is open or
  closing is `replay_segment_not_ready`;
- an export with no `to_pts_90khz` stops at the end its first chunk
  announced, however far the recording has grown since;
- a `resume` this edge would not have handed out — an unknown field, a
  value out of range, a position at or past the end it names or past what
  the segments hold, fewer bytes than lie between — is refused
  `replay_invalid_field`. One claiming more bytes than lie between cannot be
  told from a segment no entry names that has gone, and is refused
  `replay_segment_missing`.

A manager that follows `resume` says so with `pin_plan_start: true` on every
request, the first included. Its first chunk of a whole-recording export (no
`from_pts_90khz`) starts at the first entry whose segment is still on disk,
so a recording retention has pruned downloads whole. MP4 chunks carry no
`resume`: the file is built whole and cached.

**A manager from before the cursor** (v0.89.0 and older) sends neither
field, and the first chunk's request again with only the offset moved. Every
chunk it asks for with no `from_pts_90khz`, the first included, is planned
from the first entry `index.bin` names, which never moves. On a recording
retention has pruned, the first chunk is refused `replay_segment_missing`,
with a message saying why and asking for a clip, a from/to range or a
manager upgrade — before that manager has committed to a response, so the
Replay page shows the error at once rather than a download that stops after
its first megabyte. A prune during the download fails the next chunk rather
than splicing the file. A request carrying `pin_plan_start` but no `resume`
past the first chunk is read the same way. A clip, or a range with
`from_pts_90khz`, is planned from its own start on every chunk as before —
so with such a manager a trim during a clip's download still shifts the
rest of it. An edge from before the cursor ignores both fields and acks no
`resume`, and a manager that follows the cursor then sends the request it
always sent. There is no protocol version bump on either side.

`export_recording` takes the same shape with `recording_id` instead
of `clip_id`, plus optional `from_pts_90khz` / `to_pts_90khz` to
bound the export to a PTS sub-range. Whole-recording exports are
capped at 4 GiB total — over-cap requests fail with
`replay_export_too_large` and the operator should mark a clip first.
A TS clip export has no such cap, and its `resume` carries whatever it
has left, however far past 4 GiB.

The exported bytes are **packet-aligned MPEG-TS** — the manager can
concatenate chunks in order and serve the result as
`Content-Type: application/mp2t` without resyncing the first byte.

**MP4 export has shipped.** Passing `format: "mp4"` runs
`src/replay/export_mp4.rs`, which builds a **progressive** MP4 — one `moov`
carrying real sample tables (`stts` / `stss` / `stsc` / `stsz` / `stco`) ahead
of one `mdat`, not the fragmented shape the CMAF path publishes. Audio is AAC
(`mp4a`) / AC-3 / E-AC-3 / MP2, remuxed and never re-encoded. **Video is not a
remux**: on any build carrying an x264 encoder — which is all three release
artefacts — the range is decoded and re-encoded all-intra (`gop_size = 1`,
CRF 20, no B-frames, H.264 out whatever went in), because that is what makes a
clip step; a build with no encoder falls back to the source's own GOP structure
with a log line. See [The shape of the file](#the-shape-of-the-file) for why.
The re-encode is fed in the order the transport carried the frames — decode
order, the only order a decoder accepts — and each output frame is labelled with
its own picture's PTS, which the decoder carries through its reorder queue, so a
source with B-frames comes out right. It used to label the k-th picture out with
the k-th smallest PTS in, which assumed the decoder emits every picture it is
given: a clip opening on an open-GOP random access point (an HEVC CRA, whose
leading pictures reference the GOP before the cut) has those pictures dropped by
the decoder, so every later frame took the label of the one ahead of it and the
whole clip — and the audio aligned against it — shifted by the pictures dropped. Only the no-encoder fallback
assumes PTS == DTS (DTS recovery via PES parsing is a follow-up). The audio and
video tracks are aligned **by PTS**: a transport stream muxes video ahead of
its PTS by the VBV delay and audio by much less, so the first audio frame in a
range is earlier than the first picture by hundreds of milliseconds, and it is
dropped rather than played ahead of a picture it does not belong to. The
result is built one-shot into a bounded in-memory cache (5-minute TTL, byte and
entry ceilings, LRU) and refused past 256 MiB on **either** the source range or
the muxed essence — the built file itself is never measured — with
`replay_export_too_large` (download TS instead). A range that crosses a
recorder restart is refused with `replay_export_spans_restart`, which is
settled rather than transient; with no `to_pts_90khz` that guard covers the
whole recording, so a `format: "mp4"` export of a recording that has ever
restarted is refused this way. Unsupported essence (MPEG-2 video, Opus audio)
surfaces `replay_export_format_unsupported`. The edge advertises the
`replay_export_mp4` capability so the manager UI lights up the ⬇ MP4
button alongside ⬇ TS.

## Clip export

**Not the same thing as a replay clip.** A *replay clip* is a mark-in/mark-out
range an operator creates in the manager UI, stored in `clips.json` and played
back or exported through `export_clip`. A *clip export* starts in the browser
DVR player: a viewer marks a moment, asks for so many seconds either side, and
gets an MP4 on their portal sign-in page. The two share this recording, the
progressive muxer and the all-intra re-encode — the difference is who asks and
where the answer lands.

The recorder is what makes the export exact, which is why a DVR session arms one
unconditionally — see the relay's [distribution.md](../../bilbycast-relay/docs/distribution.md)
for the request/serve half and the manager for the provisioning half.

### How the edge cuts one

`src/engine/cmaf/clips.rs` runs one poller per **passthrough** CMAF output (the
proxy rendition is skipped: an operator exporting a moment wants the
full-resolution picture, and the player asks against the main stream). Every 5 s
it asks the origin for that stream's pending clips and cuts each in turn.

The poller authenticates with the CMAF output's **ingest** token — the same one
it PUTs segments with. The origin's read gate therefore has to admit an ingest
token as well as a viewer's, for both the clip list and the objects behind it.

Two paths, in order:

1. **Exact, from this recording.** The mark's wall clock maps to a PTS through
   the anchor, and the exporter builds that range into the progressive,
   all-intra MP4 described below.
2. **Whole segments, from the relay's origin.** Used when there is no recording
   for the flow, or the moment predates it. The clip is assembled from the init
   segment plus every segment overlapping the window, so it lands on segment
   boundaries — up to one segment early at the in-point and one late at the out.

### The shape of the file

A clip is **all-intra H.264 in a progressive MP4** — every frame an IDR, one
`moov` with real sample tables, one `mdat`. Both halves are deliberate, and
both were arrived at the hard way.

**All-intra, because a clip is for reviewing.** The source is long-GOP: one IDR
every ~49 frames (~2 s), the rest differences from what came before. That is
right for transport and wrong for stepping. Only 1 frame in 49 stands alone, so
stepping backward means decoding from the keyframe every time — which VLC does
badly enough that it reads as the picture breaking up. **No container fixes
this**; it is a property of the encode. So the cut is decoded and re-encoded
with `gop_size = 1`, x264, CRF 20, no B-frames. Measured on the rig: ~25 Mbps
and ~93 MB for 30 s, against ~8 Mbps for the passthrough it replaces, and about
14 s of encode for a 30 s clip.

A file that size is uploaded under its own deadline, a minute plus the body at
2 Mbit/s (capped at fifteen minutes), not the segment client's 30 s — which
covers the whole exchange, body included, and would have failed every clip
larger than thirty seconds of uplink, then re-encoded and re-sent it twice more
before calling it failed.

The recording is read from wherever the recorder filed it: `storage_id` when
one is set, the flow id otherwise. The exporter takes the id the writer
published on the flow's stats rather than assuming the default, because the
manager keeps an operator's own `storage_id` when it arms a DVR session, and
looking under the flow id then found no recording and cut every clip from
whole segments while the session read healthy.

`x264` specifically, not `h264_auto`: a hardware encoder is tuned for streaming
and several will not honour a one-frame GOP at all, which would quietly hand
back the long-GOP clip this exists to avoid.

If the re-encode fails the export **falls back to the source's own GOP
structure** rather than failing. A clip that steps poorly is worth more than no
clip, and the reason goes to the log.

**Progressive, because it is a file.** A fragmented MP4 is right for streaming,
where the player follows a manifest, and wrong for something downloaded and
opened. Two earlier shapes failed:

* **One fragment holding the whole clip.** No `sidx`, no `mfra`, an empty
  `moov` — nothing to seek by at all.
* **One fragment per GOP.** Every fragment opened on a keyframe, and it still
  would not scrub in VLC, because the index a player builds a seek from is
  `stss` and a fragmented file has none.

So the tables are written out: `stts`, `stss`, `stsc`, `stsz`, `stco`, and a
real `mvhd` duration so a player can draw a scrub bar. `moov` precedes `mdat`
so they are readable without fetching the whole file, and chunks interleave
video and audio per GOP so a player reading forward keeps both fed.

**Audio is never re-encoded.** AAC frames are already independently decodable,
so there is nothing to gain and a generation of quality to lose.

**Two trims worth knowing about.** The byte range opens on an IDR, but the
demux ahead of the muxer still yields frames before the first it marks as sync
— the tail of the previous GOP, reassembled from a PES that began before the
cut. They are not decodable: measured, a 30-second clip carried 240 samples of
which the leading 41 decoded to nothing, and the decoder rejects them outright
when they are fed to the re-encoder. The cut therefore starts at the first real
keyframe. Frames keep the PTS they were demuxed with rather than being
re-stamped on an even step — the source drops frames when the link is lossy,
and an even step spread 749 frames across the 31.4 s they really covered, so
every clip came out longer than it was asked for. That holds for the frames the
decoder and encoder were still holding at the end too: they come back on their
own pictures' PTS (they used to be stamped on the median step after the last).
A frame whose PTS the decoder lost continues the median display-order step.

**Expect up to a GOP more than you asked for.** The range ends at the first
index entry *past* the requested out-point, so the clip covers the window
rather than stopping short of it: a 30 s request yields 30–32 s.

### Memory: a spike, not a leak

Cutting a clip is a burst, not a working set. For a thirty-second all-intra
export the process transiently holds the source frames, the re-encoded frames,
the interleaved payload and the finished file — around 300 MB, freed the moment
the clip is uploaded.

Freed to *the process*. glibc gives busy threads their own arenas — the edge
runs about a hundred threads on a twelve-core box — and media work allocates in
bursts of wildly different shapes, so those arenas fragment and the free runs
are held rather than returned. Nothing is lost and the memory is reused, but
the resident size only ever climbs.

Measured on the demo rig: an edge sitting at **3399 MB dropped to 1786 MB on a
single `malloc_trim`**, so 1.6 GB of it had been free all along. Eight clips in
a row took it from 2.4 GB to 4.5 GB and it was still going.

Two things address it, and neither is a leak fix because there is no leak:

* the clip exporter trims when each cut finishes, so a burst is given back
  immediately rather than waiting;
* `main` runs a trimmer every minute for everything else, on a blocking
  thread rather than a runtime worker. Every minute rather than every three
  because the live path churns about 330 MB/min of transient allocation, so at
  three the resident size oscillated between 1.6 GB and 2.6 GB and at one it
  stays within a few hundred megabytes of the floor. It is still deliberately
  slow: it takes each arena's lock in turn and this process has real-time work
  on those threads.

Worth recognising, because it looks exactly like a leak from the outside: RSS
climbing steadily on a process that is doing nothing unusual, with no single
allocation to blame.

### When the recording cannot serve the moment

The recorder's retention and the relay's origin window are configured
independently, so a moment can age out of one while the other still holds it.
The index also keeps naming a segment for a little while after retention has
pruned the file.

Any failure from the exact path therefore hands the clip to the segment
fallback rather than failing the export — the common one being
`replay_segment_missing`, the exporter's refusal of a range that needs a
segment no longer on disk (retention pruned it, or a crash or a failed close
lost it) rather than a clip that plays straight across the hole. A window that
reaches the segment still being recorded is refused `replay_segment_not_ready`
and falls back too: the cutter does not wait for that segment to close. The clip then lands on segment boundaries
instead of the frame, which is the documented trade, and the reason is logged.
It used to be a permanent failure after three attempts, with media sitting on
the relay that would have served it.

### What it will not do

**A moment spanning a writer restart is refused**, permanently and on the first
attempt. The media either side of the join is two timelines (see [The PTS
timeline across a restart](#the-pts-timeline-across-a-restart)), and the segment
fallback cannot rescue it either — the relay's CMAF renditions restart with the
same process, so their timeline resets at the same instant. Measured: muxing
across the join produced a playable 30-second clip declaring a duration of
70,567 seconds. A file that plays but lies about its own length is discovered in
an edit suite, not here, so the operator is told to move the mark instead.

**Cuts are GOP-aligned, not frame-exact.** The exporter bounds a range with
`find_floor` at both ends. At the in-point that rounds outward and the marked
moment is safe; at the out-point it rounds inward, so a range naming the exact
end loses everything back to the previous random-access point — 26.35 s of a
30 s request on the rig. The exporter is therefore asked for the first index
entry *past* the end, which makes that floor land on it and the clip cover what
was asked for (31.6 s for 30 s). Trimming to the frame needs the leading GOP
re-encoded, which is separate work.


## Cross-references

- Configuration schema:
  [`configuration-guide.md`](configuration-guide.md#replay-recording--playback)
- Events + error_code wire shapes:
  [`events-and-alarms.md`](events-and-alarms.md#replay-server-events)
- Metrics:
  [`metrics.md`](metrics.md#replay-server-metrics)
- Manager UI + REST surface: see `bilbycast-manager/CLAUDE.md`
  (Replay section)
- Testbed: [`../../testbed/REPLAY_TEST.md`](../../testbed/REPLAY_TEST.md),
  [`../../testbed/configs/replay-edge.json`](../../testbed/configs/replay-edge.json)
