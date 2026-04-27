# Replay Server (Phase 1)

The replay server captures a flow's broadcast channel to disk and replays
named clips back onto a flow's broadcast channel. It is implemented
purely in Rust, with no new C dependencies, and is enabled by default
via the `replay` Cargo feature.

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
slow-motion, multi-track timeline, or render-to-file export.

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
| `src/manager/client.rs` (lines 2391–2750) | WS command dispatch (all `#[cfg(feature = "replay")]`-gated) |

## Storage layout

```
<replay_root>/
  <recording_id>/
    000000.ts            ← rolled, fsynced 188-byte-aligned MPEG-TS segment
    000001.ts
    ...
    NNNNNN.ts
    recording.json       ← schema_version, recording_id, created_at_unix, segment_seconds, current_segment_id
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

Each entry is exactly 24 bytes, packed `u64 + u32 + u64 + u32`
(little-endian):

| Bytes | Field | Notes |
|---|---|---|
| 0..8 | `pts_90khz` | Recorded PCR-derived PTS at the IDR boundary |
| 8..12 | `segment_id` | The `NNNNNN` of the file the IDR lives in |
| 12..20 | `byte_offset` | Offset within the segment file |
| 20..24 | `flags` | Reserved; currently always `0` |

Entries are append-only; rebuild on corruption is a Phase 2 item — the
current behaviour emits a `replay_index_corrupt` Warning and continues
to append (a non-fatal recovery path).

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
| `armed` | `true` while a writer task is running for the flow |
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

## Phase 1 limitations

- **Forward 1.0× playback only.** No reverse, slow-motion, or
  variable-speed.
- **Seeks snap to the nearest IDR ≤ target.** Frame-accurate scrubbing
  is a Phase 2 item.
- **No index rebuild on corruption.** `replay_index_corrupt` is a
  Warning today; the writer keeps appending. Phase 2 will rebuild from
  segments at open time.
- **Clip IDs are edge-side generated.** Cross-edge collision handling
  isn't in scope — clip IDs are scoped to a recording.

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
