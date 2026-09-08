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
                  drop-on-Lagged ──► bounded mpsc ──► writer task
                                                       │
                                                       ▼
                                              segment N.ts on disk
                                              index.bin (timecode → offset)
                                              clips.json (named ranges)
```

- The recorder is **another broadcast subscriber**, exactly like the
  TR-101290 / content-analysis tiers. `RecvError::Lagged(n)` increments
  `packets_dropped` and emits a Critical `replay_writer_lagged` event;
  the input task is never blocked.
- Disk I/O lives behind a bounded `tokio::sync::mpsc` (default capacity
  1024 packets) feeding a dedicated writer task. The subscriber drops
  rather than awaiting `write_all`.
- Playback is a **new input type** (`type: "replay"`). The replayer
  reads segments back, paces them by PCR via
  `replay::paced_replayer::PacedReplayer`, and publishes onto the
  flow's broadcast channel just like any other input.

### Module map

| Path | Role |
|---|---|
| `src/replay/mod.rs` | Public types (`RecordingHandle`, `ReplayCommand`, `ClipInfo`), storage-root resolution |
| `src/replay/writer.rs` | Subscriber + writer task; segment roll, retention prune, fsync, stats |
| `src/replay/reader.rs` | Segment-by-segment streaming reads with seek across segment boundaries |
| `src/replay/index.rs` | 24 B append-only `index.bin` entries (timecode → segment + offset), in-memory load + binary search |
| `src/replay/clips.rs` | `clips.json` persistence (atomic `tmp` + rename + fsync) |
| `src/replay/paced_replayer.rs` | PCR-paced bundle yield for the replay input |
| `src/engine/input_replay.rs` | Replay input task; per-input command channel; lifecycle events |
| `src/engine/flow.rs` | `FlowRuntime.recording_handle` lifecycle; spawn/teardown |
| `src/manager/client.rs` (`start_recording` arm at line 2400, `scrub_playback` arm at line 2910) | WS command dispatch — all 14 replay arms `#[cfg(feature = "replay")]`-gated |

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
    .tmp/                ← in-flight writes; atomic rename onto the final path on segment roll
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
| `1 << 1` | `PCR_DISCONTINUITY` | Set on the first IDR after a > 5 min PCR step (stream-source change) **and on the first IDR after a writer restart**; the reader skips wallclock-pacing across it, and clip export refuses to cut across it |
| `1 << 2` | `SMPTE_TC_VALID` | `smpte_tc` holds a decoded SMPTE timecode for this IDR |

Entries are append-only; rebuild on corruption is a Phase 2 item — the
current behaviour emits a `replay_index_corrupt` Warning and continues
to append (a non-fatal recovery path).

### The wall-clock anchor

`recording.json` carries a pair — `anchor_wall_us` (microseconds since the Unix
epoch) and `anchor_pts_90khz` — that turns a wall-clock instant into a PTS:

```
pts  = anchor_pts_90khz + (wall_us - anchor_wall_us) * 90_000 / 1_000_000
wall = anchor_wall_us   + (pts - anchor_pts_90khz)   * 1_000_000 / 90_000
```

It is taken once, on the first index entry appended, and preserved across
restarts. Both fields are absent on recordings made before it existed; readers
fall back to `created_at_unix` and inherit its coarseness.

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

A restart resumes an existing recording: it continues the segment numbering,
appends to the same `index.bin`, and keeps the anchor. The counter therefore has
to be resumed too. It picks up at **the last indexed tick plus the wall-clock
time the writer was down** — the gap is added, not skipped, because nothing was
recorded during it but the anchor maps wall clock to ticks linearly, and a
counter that ignored the outage would place everything after it earlier by
exactly the downtime.

Starting the counter again at zero writes a second, overlapping timeline into
one file. Every wall-clock lookup after the restart then lands in a hole, and
any that resolves can match a frame from before it.

**The media is still discontinuous.** The index's own timeline is continuous
across the join, but the PCR in the TS begins again with the process, so the two
sides are two timelines however neatly the index numbers them. The first entry
after a resume therefore carries `PCR_DISCONTINUITY`, and anything muxing a
range has to honour it — see [Clip export](#clip-export) below.

## Lifecycle

### Recording arm → roll → prune

1. Operator sends `start_recording { flow_id }` (or armed at flow
   start when `RecordingConfig.enabled = true`). Edge spawns the
   subscriber + writer tasks via
   `FlowRuntime::start_recording_for_flow`.
2. `recording_started` Info event fires. `RecordingStats.armed = true`.
3. The writer appends 188 B-aligned packets to a staging file under
   `.tmp/`. PCR is tracked from the data; SMPTE timecode is extracted
   via `engine::content_analysis::timecode::TimecodeTracker`.
4. Every `segment_seconds` (default 10 s, validated `[2, 60]`):
   - `seg.file.sync_all()` on the current segment.
   - Atomic rename `.tmp/<n>.ts` → `<n>.ts`.
   - Append index entries for the IDR boundaries inside that segment;
     `index_writer.write().sync_all()`.
   - Run retention prune — oldest-first by mtime, capped by both
     `retention_seconds` and `max_bytes`. Either being `0` disables
     that axis (still bounded by free disk on the size axis).
5. On stop (`stop_recording`) or fatal I/O error: subscriber
   detaches, current segment closes + fsyncs, `RecordingStats.armed
   = false`, `recording_stopped` Info event.
6. Retention will **never** unlink the just-finalized segment id
   (`meta.current_segment_id`) — losing the live edge would tear out
   clips that touch the most recent few seconds. A `max_bytes` cap
   smaller than one segment fires the Warning
   `replay_max_bytes_below_segment` instead.

### Crash recovery (writer init scan)

Whenever `spawn_writer` runs (cold start, edge restart after SIGKILL,
operator-driven re-arm), it scans the recording directory before
opening a new segment:

1. **`.tmp/` orphan cleanup.** Any `<NNNNNN>.ts` left in `.tmp/` is a
   partial segment the writer never atomically renamed onto the
   recording — unlinked unconditionally.
2. **Resume id derivation.** The next segment id is
   `max(<NNNNNN>.ts on disk) + 1`, never just `recording.json`'s
   `current_segment_id`. The meta file is best-effort on the roll
   path (and may be corrupt after a SIGKILL); trusting it would
   cause segment-id reuse and overwrite finalized data.
3. **`index.bin` alignment.** If the file length isn't a 24-byte
   multiple (a SIGKILL between `append` and `flush_and_sync` can
   leave a partial entry), `IndexWriter::open` aligns down to the
   last valid boundary in place. `InMemoryIndex::load` applies the
   same rule on the reader side. The trailing partial IDR is
   discarded; the next IDR re-establishes the index head.
4. **Recovery alert.** If any of the above fired (orphans removed,
   meta corrupt, or disk-derived id outranked the meta), the writer
   emits the Warning event `replay_recovery_alert` with structured
   `details.tmp_orphans_removed`, `details.meta_corrupt`,
   `details.next_segment_id`. Recovery is non-fatal; the recording
   continues from the next id without operator intervention.

### Clip create

1. Operator presses **I** (or `mark_in`). `replay_command_channel`
   sends `MarkIn { pts? }`. If `pts` is omitted, the writer's
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
   to the broadcast channel.
4. **K** key or `stop_playback` halts playback (`playback_stopped`
   Info). On reaching `clip.out_pts_90khz` with `loop_playback = false`
   the replayer fires `playback_eof` and idles on the last frame with
   NULL-PID padding.

## Error matrix

Every error path emits a structured event under category `replay`
with `details.error_code`. The same `error_code` rides on
`command_ack.error_code` so the manager UI can highlight the offending
field on a Create/Update modal without parsing strings.

| `error_code` | Severity | Trigger | Operator action |
|---|---|---|---|
| `replay_recording_not_active` | Error | `mark_in`/`mark_out` while flow has no recording armed | Send `start_recording` first |
| `replay_no_playback_input` | Error | `play_clip`/`scrub_playback` on a flow with no `replay` input | Add a `replay` input to the flow |
| `replay_clip_not_found` | Error | `play_clip`/`delete_clip` with an unknown `clip_id` | Refresh the clip list (it may have been pruned by retention) |
| `replay_writer_lagged` | Critical | Writer mpsc full; recorder dropped packets | Check disk throughput; reduce concurrent recording flows; investigate fs latency |
| `replay_disk_pressure` | Warning | Recording usage ≥ 80 % of `max_bytes` (or of replay-root filesystem when `max_bytes = 0`); sticky until back below 70 % | Free disk before ENOSPC; raise `max_bytes` if appropriate; reduce retention |
| `replay_disk_full` | Critical | Segment write hit ENOSPC | Free disk, then `stop_recording` + `start_recording` to re-arm |
| `replay_index_corrupt` | Warning | `index.bin` failed validation on open | Phase 1 keeps going (appends new entries); Phase 2 will do a full rebuild from segments |
| `replay_invalid_segment_seconds` | Error | `RecordingConfig.segment_seconds` outside `[2, 60]` | Use a value in range |
| `replay_invalid_recording_id` | Error | `start_recording` references a flow with no `recording` config | Add `RecordingConfig` to the flow first |
| `replay_storage_id_invalid` | Error | `RecordingConfig.storage_id` fails the alphanumeric + `._-` ≤ 64 char rule | Use a valid id |
| `replay_invalid_field` | Error | `mark_out` / `rename_clip` / `update_clip` `name` > 256 chars or contains control chars; `description` > 4096 chars | Trim to limits |
| `replay_invalid_range` | Error | `play_clip` / `scrub_playback` with `to_pts_90khz < from_pts_90khz` (or below the clip's `in_pts`); `update_clip` with the prospective `in_pts_90khz / out_pts_90khz` inverted | Pass a forward range |
| `replay_invalid_tag` | Error | `update_clip` (or any tag-bearing path) with a tag that fails `[A-Z0-9_-]{1,32}`, more than 16 tags per clip | Use the v1 fixed set (`GOAL`/`FOUL`/`OFFSIDE`/`SAVE`/`YELLOW`/`VAR-CHECK`) or shorten / re-case |
| `replay_max_bytes_below_segment` | Warning | Retention can't satisfy `max_bytes` without deleting the live edge — operator's cap is smaller than one segment | Raise `max_bytes` to at least `segment_seconds × bitrate × 2` |
| `replay_metadata_stale` | Warning | `recording.json` write failed on segment roll; recovery scan will derive next segment id from the directory listing on restart | Investigate the disk (typically ENOSPC on the replay volume) |
| `replay_recovery_alert` | Warning | Edge restarted after a crash; orphan `.tmp/` segments cleaned and / or `recording.json` was corrupt | Informational — verify `details.tmp_orphans_removed` and `details.next_segment_id` match expectations |

### Orphan-recovery list_clips

`list_clips` accepts either `flow_id` (the normal manager UI path —
resolves the flow's recording via `FlowRuntime`) or `recording_id`
(direct lookup against the on-disk recording, even if no flow
references it any more). The latter is the recovery path when a
flow has been deleted but its segments + clips persisted on disk
under the same `<recording_id>` — the operator points a fresh flow
at it via a `replay` input and uses `recording_id` to enumerate the
clips. When both fields are present, `flow_id` wins.

Disk-pressure monitoring runs alongside the reactive ENOSPC handling.
On every segment roll the writer computes a usage percentage —
`bytes_written / max_bytes` when the operator set a per-recording
cap, or filesystem `(total - free) / total` when `max_bytes = 0`.
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
| `mode` | Phase 2 / 1.5 — wire-string mirror of [`WriterMode`]: `"armed"` when a session is live, `"pre_buffer"` when the writer is rolling pre-roll TS but the operator hasn't pressed Start, `"idle"` when the writer is stopped (post-Stop with no pre-buffer, or post-cancel). Drives the `/replay` page's tri-state `Recording / Pre-roll / Idle` badge and the flow-card `● PRE-ROLL` chip. Older edges omit the field; the manager falls back to `armed`-derived state. |
| `segments_written` | Completed, rolled, fsynced segments |
| `bytes_written` | Total bytes appended (across all segments, including pruned) |
| `segments_pruned` | Segments evicted by retention (mtime / size) |
| `packets_dropped` | Packets dropped at the broadcast subscriber (writer mpsc full) |
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

## Phase 2 / 1.5 — clip tags + `update_clip`

Clips carry an optional `tags: Vec<String>` for sports / VAR
workflows. Bounds:

- Each tag matches `^[A-Z0-9_-]{1,32}$` (operator-friendly enum
  shorthand — `GOAL`, `FOUL`, `VAR-CHECK`, etc.).
- ≤ 16 tags per clip. Server-side dedup'd in input order.
- Hard-coded set in the manager UI's quick-tag bar for v1 (`GOAL`,
  `FOUL`, `OFFSIDE`, `SAVE`, `YELLOW`, `VAR-CHECK`); the edge stores
  whatever the manager sends so per-group customisation later doesn't
  need an edge release.

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

**SMPTE TC handling on trim.** The IDR index doesn't carry SMPTE
strings (just PTS / segment / offset / flags), so the edge can't
cheaply re-derive a fresh `HH:MM:SS:FF` for a new in/out PTS. When
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
- **No index rebuild on corruption.** `replay_index_corrupt` is a
  Warning today; the writer keeps appending. Phase 2 will rebuild
  from segments at open time.
- **Clip IDs are edge-side generated.** Cross-edge collision
  handling isn't in scope — clip IDs are scoped to a recording.
- **SMPTE TC cleared on trim.** When `update_clip` changes
  `in_pts_90khz` / `out_pts_90khz`, the corresponding `smpte_in` /
  `smpte_out` is cleared (the IDR index doesn't carry SMPTE strings,
  so the edge can't cheaply re-derive a fresh `HH:MM:SS:FF`). The
  manager UI shows `—` until the operator re-marks. A Phase 3 index
  schema bump could carry SMPTE alongside PTS to remove this gap.

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
aligned, so no per-session bookkeeping is needed.

```jsonc
// Request
{
  "type": "export_clip",
  "clip_id": "clp_…",
  "format": "ts",            // optional; "ts" (default) or "mp4"
  "byte_offset": 0,          // optional; default 0
  "chunk_bytes": 1048576     // optional; default 1 MiB, hard cap 3 MiB
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
  "eof": false
}
```

`export_recording` takes the same shape with `recording_id` instead
of `clip_id`, plus optional `from_pts_90khz` / `to_pts_90khz` to
bound the export to a PTS sub-range. Whole-recording exports are
capped at 4 GiB total — over-cap requests fail with
`replay_export_too_large` and the operator should mark a clip first.

The exported bytes are **packet-aligned MPEG-TS** — the manager can
concatenate chunks in order and serve the result as
`Content-Type: application/mp2t` without resyncing the first byte.

**MP4 export has shipped.** Passing `format: "mp4"` runs the
`src/replay/export_mp4.rs` TS→fragmented-MP4 remuxer (reuses the CMAF
fMP4 box writer): video H.264 (`avc1`) / HEVC (`hvc1`), audio AAC
(`mp4a`) / AC-3 / E-AC-3 / MP2. The remuxer assumes PTS == DTS (DTS
recovery via PES parsing is a follow-up), builds the file one-shot into
a 5-minute-TTL in-memory cache, and caps exports at 256 MiB —
over-cap clips fail with `replay_export_too_large` (download TS
instead). Unsupported essence (MPEG-2 video, Opus audio) surfaces
`replay_export_format_unsupported`. The edge advertises the
`replay_export_mp4` capability so the manager UI lights up the ⬇ MP4
button alongside ⬇ TS.

## Clip export

**Not the same thing as a replay clip.** A *replay clip* is a mark-in/mark-out
range an operator creates in the manager UI, stored in `clips.json` and played
back or exported through `export_clip`. A *clip export* starts in the browser
DVR player: a viewer marks a moment, asks for so many seconds either side, and
gets an MP4 on their portal sign-in page. The two share this recording and
nothing else.

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
   the anchor, and `export_recording_mp4_chunk` remuxes that range to fMP4.
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
every clip came out longer than it was asked for.

**Expect up to a GOP more than you asked for.** The range ends at the first
index entry *past* the requested out-point, so the clip covers the window
rather than stopping short of it: a 30 s request yields 30–32 s.

### When the recording cannot serve the moment

The recorder's retention and the relay's origin window are configured
independently, so a moment can age out of one while the other still holds it.
The index also keeps naming a segment for a little while after retention has
pruned the file.

Any failure from the exact path therefore hands the clip to the segment
fallback rather than failing the export — the common one being
`stat segment …: No such file`. The clip then lands on segment boundaries
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
