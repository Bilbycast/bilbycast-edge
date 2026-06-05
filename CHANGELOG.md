# Changelog

All notable changes to `bilbycast-edge` are recorded here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.33.0] - unreleased

> **Note:** This changelog trails the shipped behaviour. Release tags are
> well ahead of the latest version recorded here, and several major
> subsystems shipped without changelog entries. Treat the per-project
> `CLAUDE.md` and the `docs/` set as the source of truth until the
> changelog is reconciled. Notable shipped-but-unlogged subsystems include:
> RIST Simple Profile input/output; the per-flow master clock + A/V sync
> (`master_clock`, encoder-style PES PTS regeneration); in-depth content
> analysis (`content_analysis` lite / audio_full / video_full tiers); the
> PID bus / Flow Assembly + PES Switch (SPTS/MPTS synthesis, PES-aligned
> splice); egress and ingress de-jitter buffers; and wallclock-egress wire
> pacing (`engine::wire_emit`, SO_TXTIME / ETF + `clock_nanosleep` tiers).

### Changed

- **Release workflow triggers** — `push.tags: v*` and `workflow_dispatch`
  added alongside the existing nightly cron. The new monorepo-root
  `release-all.sh` orchestrator detects Cargo.toml version bumps and pushes
  matching tags for on-demand releases; the nightly cron stays as a safety
  net for version bumps that were committed but not manually released
  (idempotent — `gh release view` short-circuits the build when the tag
  already exists). Workflow filename is preserved (`nightly-release.yml`)
  because it is hard-coded into `src/upgrade/trust.rs::ALLOWED_SIGNERS` and
  the cosign self-verify regex — renaming would invalidate every
  previously-signed manifest. Manual install URLs
  (`releases/latest/download/...`) and the manager-driven auto-upgrade
  pipeline are unchanged.

### Added

- **Native dual-stack (IPv4 + IPv6) listeners** — the API server
  (REST + NMOS IS-04/05/08 + setup wizard + Prometheus) and the
  optional monitor dashboard now bind v4 and v6 simultaneously by
  default. New optional config fields `ServerConfig.listen_addrs` and
  `MonitorConfig.listen_addrs` (both `Option<Vec<String>>`) carry the
  dual-stack list; default is `["0.0.0.0", "[::]"]` for fresh installs.
  Existing configs without the field keep the legacy single-address
  `listen_addr` behaviour — no migration required. v6 entries get
  `IPV6_V6ONLY=1` so they coexist with v4 listeners on the same port
  without `EADDRINUSE`. CLI override `--bind-addrs <comma-separated>`
  is the multi-addr companion to legacy `--bind`. Setup wizard accepts
  comma-separated input in the Listen Address field. Effective list
  resolvers: `ServerConfig::effective_listen_addrs` /
  `MonitorConfig::effective_listen_addrs`. Listener construction
  (`main.rs::build_std_tcp_listener` / `build_tokio_tcp_listener`,
  `monitor::server::build_monitor_listener`) uses `socket2` for the v6
  socket option. Per-flow media listeners (SRT / RIST / RTP / UDP /
  RTMP / WebRTC / ST 2110) were already family-agnostic via the
  per-input `bind_addr` field; no change there.
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
- Per-clip thumbnail at the in-point IDR (the existing `media-codecs`
  JPEG path is the natural place to wire this).
