# Changelog

All notable changes to `bilbycast-edge` are recorded here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.33.0] - unreleased

### Added

- **Replay server (Phase 1)** — pure-Rust continuous flow recording to disk
  + clip playback as a fresh input. Gated by the `replay` Cargo feature,
  on by default; pure-Rust, no new C deps.
  - New flow attribute `recording: { enabled, storage_id, segment_seconds
    (2–60, default 10), retention_seconds (default 86400), max_bytes
    (default 50 GiB) }`. Writer rolls 188 B-aligned MPEG-TS segments into
    `<replay_root>/<recording_id>/` (resolved from `BILBYCAST_REPLAY_DIR`
    → `$XDG_DATA_HOME/bilbycast/replay/` → `$HOME/.bilbycast/replay/` →
    `./replay/`).
  - New input type `type: "replay"` with `recording_id`, optional
    `clip_id`, `start_paused`, `loop_playback`. Forward 1.0× playback
    only; seeks snap to the nearest IDR ≤ target via binary search on the
    in-memory index.
  - New WS commands (all `#[cfg(feature = "replay")]`-gated, capability
    advertised on `HealthPayload.capabilities` so missing-feature edges
    return `unknown_action` gracefully): `start_recording`,
    `stop_recording`, `mark_in`, `mark_out`, `list_clips`, `get_clip`,
    `delete_clip`, `rename_clip`, `recording_status`, `cue_clip`,
    `play_clip`, `scrub_playback`, `stop_playback`.
  - New events on category `replay`: `recording_started/stopped`,
    `recording_start_failed`, `clip_created/deleted`,
    `playback_started/stopped/eof`, `writer_lagged` (Critical),
    `disk_full` (Critical), `index_corrupt` (Warning).
  - New stable `error_code` values on `command_ack`:
    `replay_recording_not_active`, `replay_no_playback_input`,
    `replay_clip_not_found`, `replay_writer_lagged`,
    `replay_disk_full`, `replay_index_corrupt`,
    `replay_invalid_segment_seconds`, `replay_invalid_recording_id`,
    `replay_storage_id_invalid`.
  - New per-recording stats on `FlowStats.recording`: `armed`,
    `segments_written`, `bytes_written`, `segments_pruned`,
    `packets_dropped`, `index_entries`, `current_pts_90khz`. All
    lock-free `AtomicU64`, sampled at 1 Hz on the WS stats path.
  - **Disk-pressure signal** — new `replay_disk_pressure` Warning event
    fires once when recording usage crosses 80 % of the configured
    `max_bytes` cap (or of the replay-root filesystem when
    `max_bytes = 0`). Sticky until usage falls back below 70 %
    (hysteresis on the same bit) so the events feed isn't spammed.
    Operators get a chance to free disk before ENOSPC. The
    `recording_status` response now also surfaces `max_bytes`,
    `replay_root_free_bytes`, and `replay_root_total_bytes` so the
    manager UI can render a coloured disk meter.
  - **`BILBYCAST_REPLAY_DIR`** environment variable for storage-root
    override.
  - Reference: `docs/replay.md`, `docs/configuration-guide.md`
    ("Replay (recording + playback)"), `docs/events-and-alarms.md`
    ("Replay-server events"), `docs/metrics.md`
    ("Replay-server metrics").
  - End-to-end test plan + working config: `testbed/REPLAY_TEST.md`,
    `testbed/configs/replay-edge.json`.

### Phase 2 / deferred

- Reverse playback, slow-motion, variable-speed.
- Frame-accurate scrubbing (currently snaps to the nearest IDR ≤ target).
- Index rebuild on corruption (`replay_index_corrupt` is a Warning today;
  the writer keeps appending).
- Per-clip thumbnail at the in-point IDR (the existing `video-thumbnail`
  JPEG path is the natural place to wire this).
