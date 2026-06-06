# Events and Alarms

bilbycast-edge generates operational events and forwards them to bilbycast-manager via WebSocket. Events provide real-time visibility into connection state changes, failures, and significant operational conditions that go beyond periodic stats and health messages.

## Event Protocol

Events are sent as WebSocket messages with type `"event"`:

```json
{
  "type": "event",
  "timestamp": "2026-04-02T12:00:00Z",
  "payload": {
    "severity": "warning",
    "category": "srt",
    "message": "SRT input disconnected, reconnecting",
    "flow_id": "flow-1",
    "details": { ... }
  }
}
```

### Severity Levels

| Severity | Meaning | Action |
|----------|---------|--------|
| `critical` | Service-impacting failure | Operator should investigate immediately |
| `warning` | Degradation or potential issue | Operator should investigate when possible |
| `info` | Notable state change | No action required, operational awareness |

### Event Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `severity` | string | yes | `"info"`, `"warning"`, or `"critical"` |
| `category` | string | yes | Event category (see tables below) |
| `message` | string | yes | Human-readable description |
| `flow_id` | string | no | Associated flow or tunnel ID |
| `details` | object | no | Structured context (error codes, peer addresses, etc.) |

### Deduplication

Events are emitted on state transitions, not periodically. For example, an SRT disconnect event fires once when the connection drops, not repeatedly while disconnected. The corresponding reconnection event fires when the connection is restored.

For protocols with automatic reconnection loops (RTSP, SRT caller), disconnect events fire **once per connection cycle** — if the remote is unreachable and retries continue, subsequent retry failures are silent until a connection succeeds and then drops again.

### Buffering

Events are queued in an unbounded in-memory channel. When the edge is not connected to the manager (e.g., during reconnection), events accumulate and are delivered once the connection is re-established.

---

## Event Reference

### Flow Lifecycle (`flow`)

| Severity | Message | Trigger |
|----------|---------|---------|
| info | Flow '{id}' started | Flow successfully created and running |
| info | Flow '{id}' stopped | Flow stopped by command or config change |
| critical | Flow '{id}' failed to start: {error} | Flow startup error (input bind, output bind, validation) |
| critical | Flow input lost: {error} | Input task exited unexpectedly |
| info | Output '{id}' added to flow '{flow_id}' | Output assigned and started on a running flow |
| info | Output '{id}' removed from flow '{flow_id}' | Output unassigned and stopped on a running flow |
| warning | Output '{id}' failed to start on flow '{flow_id}': {error} | Output startup failure within a running flow |
| info | Input '{id}' created | Independent input created via CRUD command. `input_id` set on event |
| info | Input '{id}' updated | Independent input updated via CRUD command. `input_id` set on event |
| info | Input '{id}' deleted | Independent input deleted via CRUD command. `input_id` set on event |
| info | Output '{id}' created | Independent output created via CRUD command. `output_id` set on event |
| info | Output '{id}' updated | Independent output updated via CRUD command. `output_id` set on event |
| info | Output '{id}' deleted | Independent output deleted via CRUD command. `output_id` set on event |
| info | Flow '{flow_id}': active input switched to '{input_id}' | Input switched via API or manager command. `input_id` set on event. Details: `{ previous_input_id }`. The TS continuity fixer automatically resets CC state (creating a clean break), injects the new input's cached PAT/PMT (version-bumped to force re-parse), forwards all packets immediately, and — if the new input has ingress `video_encode` — asks the re-encoder to emit an IDR on its first post-switch frame. Visible switch latency at the receiver is one to two frames for both passthrough and ingress-transcoded inputs. Fully format-agnostic — inputs can use different codecs, containers, and transports (e.g., H.264, H.265, JPEG XS, uncompressed ST 2110, any audio). For non-TS transports the fixer is transparent. |
| info | Flow '{flow_id}': output '{output_id}' set active/passive | Output toggled via API or manager command. `output_id` set on event. Details: `{ active: bool }` |
| info | Input '{id}' added to flow '{flow_id}' | Input hot-attached to a running flow via the `add_input` WS command (or the `update_flow` / `update_config` diff). New input joins as warm-passive — `activate_input` flips it active seamlessly via the existing switch path. Sibling inputs + every output keep running. |
| info | Input '{id}' removed from flow '{flow_id}' | Input hot-detached from a running flow via the `remove_input` WS command (or the `update_flow` / `update_config` diff). The fixer's per-input PSI cache is evicted so a later re-add under the same id doesn't carry stale PAT/PMT. Cross-flow subscribers see `RecvError::Closed` cleanly. |
| warning | Input '{id}' failed to add to flow '{flow_id}': {error} | Hot-add refusal. Carries structured `error_code` on `command_ack`: `active_input_in_use`, `pid_bus_input_in_use`, `hitless_leg_change_requires_restart`, `input_already_member`, `input_not_member`, `input_resource_critical`, or `port_conflict` / `bind_failed` (caught by the 600 ms bind-failure window on listener inputs). |
| warning | Master clock source input '{id}' is being removed from flow '{flow_id}' | **R5**. The input being removed is the operator-declared master-clock source (`flow.master_clock`). The PCR PLL coasts on its last-known frequency; the master clock's auto-resolver picks the next active input and re-locks. Output PCR may briefly drift until lock. Details: `{ error_code: "master_clock_input_removed", input_id, flow_id }`. |

| warning | Edge-added A/V skew {n} ms on flow '{id}' exceeds the EBU R37 ±40 ms limit … | The exact lip-sync error introduced by this edge's PTS-touching stages (`FlowStats.av_skew`) exceeded ±40 ms for two consecutive 5 s polls. Details: `{ error_code: "av_skew_exceeded", skew_ms, worst_abs_ms, lipsync_trim_ms, threshold_ms }`. The configured lipsync trim counts toward the skew (a ±100 ms deliberate trim WILL alarm — by design, the operator asked for an offset that large). |
| info | Edge-added A/V skew on flow '{id}' recovered to {n} ms | Skew back under 30 ms (hysteresis). Details: `{ error_code: "av_skew_recovered", skew_ms }`. |
| warning | A/V mux interleave p95 {n} ms on flow '{id}' — not a lip-sync fault, but receivers buffering less than this … | Windowed p95 of `OutputStats.av_interleave` exceeded 1200 ms for two consecutive polls. NOT a lip-sync defect — guidance that consumer players with ~1 s default caching (VLC) will starve their audio queue on this stream. Details: `{ error_code: "av_interleave_deep", window_p95_abs_ms, threshold_ms }`. |
| info | A/V mux interleave on flow '{id}' recovered to p95 {n} ms | Windowed p95 back under 900 ms. Details: `{ error_code: "av_interleave_recovered", window_p95_abs_ms }`. |

**Source**: `src/engine/manager.rs`, `src/manager/client.rs`. Hot-swap engine paths: `FlowRuntime::{add_input, remove_input}` (`src/engine/flow.rs`). A/V quality watcher: `src/engine/av_quality_watch.rs`.

> Note: the R5 `master_clock_input_removed` event above rides the **`flow`**
> category. The PLL fallback / recovery events below use a separate
> **`master_clock`** category.

---

### Master Clock (`master_clock`)

PLL lock-state transitions for flows whose master clock runs the source-PCR
PLL (`master_clock.kind = source_pcr_pll` / `contribution`, or the `auto`
cascade's PLL rung). Emitted by the per-flow fallback watcher in
`src/engine/master_clock.rs`.

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| warning | PCR PLL did not lock within {n}s on flow '{id}' (reason: {reason}); falling back to wallclock | The PLL failed to lock within the grace window (`pll_lock_timeout_s`, default 30 s). The master clock drops to the wallclock rung; output PCR is bounded but no longer tracks the source. | `{ error_code: "master_clock_pll_fallback", input_id, samples_received, samples_needed: 100, p99_jitter_us, lock_threshold_us: 100, fallback_reason, waited_s }`. `fallback_reason` is `"no_pcr_observed"` (no PCR samples), `"insufficient_samples"` (< 100 samples), or `"jitter_too_high"` (samples seen but p99 jitter above the lock threshold). |
| info | PCR PLL re-acquired lock on flow '{id}' (input '{id}'); leaving wallclock fallback | The PLL converged again after a prior fallback. The master clock self-heals back to the PLL rung; the grace window is reset. | `{ error_code: "master_clock_pll_recovered", input_id, samples_received, p99_jitter_us }`. |

**Source**: `src/engine/master_clock.rs` (`fire_fallback`, the recovery branch). Telemetry counterpart on `FlowStats.master_clock` (`fallback_active` / `fallback_reason`) — see [`metrics.md`](metrics.md#master-clock-telemetry-flowstatsmaster_clock) and [`clocking.md`](clocking.md#telemetry).

> The egress / ingress de-jitter residence-cap shed surfaces as **stats**
> (`OutputStats.egress_shed`, `InputStats.ingress_dejitter_shed`), **not** as
> dedicated events — see [`metrics.md`](metrics.md#wire-pacing-and-egress-de-jitter-telemetry-outputstats).

---

### Bandwidth (`bandwidth`)

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| warning | Flow '{id}' bandwidth exceeded limit ({current} Mbps > {limit} Mbps) | Input bitrate exceeds configured `bandwidth_limit.max_bitrate_mbps` for the grace period (alarm action) | `{ current_mbps, limit_mbps, action: "alarm" }` |
| critical | Flow '{id}' blocked: bandwidth exceeded limit ({current} Mbps > {limit} Mbps) | Input bitrate exceeds configured limit for the grace period (block action) — packets dropped until bandwidth normalizes | `{ current_mbps, limit_mbps, action: "block" }` |
| info | Flow '{id}' bandwidth returned to normal, unblocked | Bitrate returned within limits after being blocked — flow resumes | `{ current_mbps, limit_mbps }` |
| info | Flow '{id}' bandwidth returned to normal ({current} Mbps <= {limit} Mbps) | Bitrate returned within limits after alarm | `{ current_mbps, limit_mbps }` |

**Source**: `src/engine/bandwidth_monitor.rs`

---

### SRT (`srt`)

#### SRT Input

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | SRT input connected (mode=listener) | Listener accepted a caller connection | `{ mode, local_addr, stream_id }` |
| info | SRT input connected (mode={mode}) | Caller connected to remote | `{ mode, local_addr, remote_addr, stream_id }` |
| info | SRT input connected (mode=listener, redundant leg 1) | Redundant leg 1 accepted | `{ mode, leg, local_addr }` |
| warning | SRT input disconnected, reconnecting | Peer disconnected, reconnection in progress | `{ mode, local_addr }` or `{ mode, remote_addr }` |
| critical | SRT input connection failed: {error} | Listener accept or caller connect failed | `{ mode, local_addr/remote_addr, stream_id, error }` |

#### SRT Output

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | SRT output '{id}' connected | Peer connected (listener) or caller connected. `output_id` set on event | `{ mode, local_addr, remote_addr, stream_id }` |
| warning | SRT output '{id}' disconnected | Peer disconnected or connection lost. `output_id` set on event | `{ mode, local_addr/remote_addr }` |
| warning | SRT output '{id}' stale connection detected | No ACK received after timeout, re-accepting. `output_id` set on event | |
| critical | SRT output '{id}' connection failed: {error} | Caller can't reach remote, or bind fails. `output_id` set on event | `{ mode, remote_addr, stream_id, error }` |

**Source**: `src/engine/input_srt.rs`, `src/engine/output_srt.rs`

---

### Bonding (`bond`)

Emitted by bonded inputs and outputs (`src/engine/input_bonded.rs`,
`src/engine/output_bonded.rs`) when the underlying
`bonding_transport::BondSocket` signals a path lifecycle transition
or a change in the aggregate bond health. Stats-level per-path
metrics (RTT, loss, throughput, alive/dead) continue to flow every
stats snapshot regardless — these events are transition-only alarms
meant to complement the live stats pane.

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | bonded {input\|output} path '{name}' (#{id}) alive (M/N paths up) | A previously-dead path saw a keepalive ack (sender) or an inbound datagram (receiver) | `{ path_id, path_name, scope, alive_count, total }` |
| warning | bonded {input\|output} path '{name}' (#{id}) dead: {reason} (M/N paths up) | No keepalive ack within `keepalive_miss_threshold × keepalive_interval` (sender) or no inbound packet within the same window (receiver) | `{ path_id, path_name, scope, reason, alive_count, total }` |
| warning | bonded {input\|output} degraded — 1/N paths up (redundancy lost) | Bond dropped from ≥ 2 alive paths to exactly one | `{ path_id, path_name, scope, alive_count, total }` |
| critical | bonded {input\|output} down — 0/N paths up (media plane offline) | Every path went dead | `{ path_id, path_name, scope, alive_count: 0, total }` |
| info | bonded {input\|output} recovered — M/N paths up | Bond returned to ≥ 2 alive paths after a Degraded or Down state | `{ path_id, path_name, scope, alive_count, total }` |

**`reason`** is one of `keepalive_timeout`, `receive_timeout`,
`transport_error`. Per-path events are **flap-deduped with a 2 s
grace window** — an alive↔dead transition arriving within 2 s of the
opposite transition for the same path is suppressed so a flapping
link doesn't flood the events feed (stats still reflect reality via
`PathStats.dead`). Bond-aggregate events (`degraded` / `down` /
`recovered`) always emit and are not flap-deduped.

**Source**: `src/engine/input_bonded.rs`,
`src/engine/output_bonded.rs`, forwarder helper in
`src/manager/events.rs::run_bond_event_forwarder`. Liveness detection
and event generation live inside `bilbycast-bonding` at
`bonding-transport/src/health.rs` + `sender.rs` + `receiver.rs`.

---

### SMPTE 2022-7 Redundancy (`redundancy`)

| Severity | Message | Trigger |
|----------|---------|---------|
| warning | Redundant leg 1 lost | SRT redundant leg 1 stopped receiving |
| warning | Redundant leg 2 lost | SRT redundant leg 2 stopped receiving |
| critical | Both redundant legs lost | No data from either leg, will reconnect |

**Source**: `src/engine/input_srt.rs`

---

### RTMP (`rtmp`)

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | RTMP publisher connected | Client connected and started publishing | |
| warning | RTMP publisher disconnected | Publisher disconnected | |
| critical | RTMP server error: {error} | Server bind or accept failure | `{ error }` |

**Source**: `src/engine/input_rtmp.rs`

---

### RTSP (`rtsp`)

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | RTSP connected to {url} | RTSP DESCRIBE/SETUP/PLAY succeeded | `{ url }` |
| warning | RTSP input disconnected: {error}. Reconnecting in {n}s | Stream lost after a successful connection | `{ url, reconnect_delay_secs, error }` |

The disconnect event fires **once per connection cycle** — if the RTSP server is unreachable and the edge retries repeatedly, only the first failure emits an event. A new "connected" event followed by a new "disconnected" event will fire when the connection is established and then lost again.

**Source**: `src/engine/input_rtsp.rs`

---

### HLS (`hls`)

| Severity | Message | Trigger |
|----------|---------|---------|
| warning | HLS segment upload failed: {error} | HTTP PUT fails for a segment |
| critical | HLS output failed: {error} | Output task exited with error |

**Source**: `src/engine/output_hls.rs`

---

### WebRTC (`webrtc`)

| Severity | Message | Trigger |
|----------|---------|---------|
| info | WHIP publisher connected | ICE+DTLS complete on WHIP server input |
| info | WHIP publisher disconnected | Publisher left |
| info | WHEP connected | WHEP client input connected to remote |
| info | WHEP disconnected | WHEP client input disconnected |
| info | WHEP viewer connected | New WHEP viewer joined an output |
| info | WHEP viewer disconnected | WHEP viewer left an output |
| warning | WebRTC session failed: {error} | ICE failure, DTLS error, or session creation error |
| warning | WebRTC session creation failed: {error} | Output session could not be created |

**Source**: `src/engine/input_webrtc.rs`, `src/engine/output_webrtc.rs`

---

### Audio Encoder (`audio_encode`)

ffmpeg-sidecar audio encoder lifecycle for the Phase B compressed-audio
egress on RTMP, HLS, and WebRTC outputs. Emitted by the per-output
build helpers (`output_rtmp::build_encoder_state`,
`output_hls::hls_output_loop` startup gate,
`output_webrtc::build_webrtc_encoder_state`) and the long-running
encoder supervisor (`engine::audio_encode::supervisor_loop`).

| Severity | Message | Trigger |
|----------|---------|---------|
| info | audio encoder started: output '{id}' codec={codec} {N} kbps | First successful ffmpeg spawn for an RTMP/WebRTC output, or HLS startup with `audio_encode` set. Details payload: `{ output_id, codec, bitrate_kbps, sample_rate, channels }` |
| warning | audio encoder restarted: output '{id}' restart {N}/{max} | Supervisor restarted ffmpeg after a crash. Details payload: `{ output_id, restart_count, max_restarts }` |
| warning | HLS output '{id}': segment {n} remux failed: {error} | Per-segment ffmpeg fork failed on a single HLS segment. The next segment may succeed |
| critical | audio encoder failed: output '{id}' exhausted {N} restarts in {S} s | Supervisor gave up after MAX_RESTARTS in RESTART_WINDOW |
| critical | RTMP/HLS/WebRTC output '{id}': audio_encode requires ffmpeg in PATH but it is not installed | ffmpeg missing at lazy-build time |
| critical | output '{id}': audio_encode requires AAC-LC input ... got profile={p} | Phase A `AacDecoder` rejected the source AAC profile (HE-AAC, AAC-Main, multichannel, etc.) |
| critical | output '{id}': audio_encode is set but the flow input cannot carry TS audio (PCM-only source) | `compressed_audio_input` is false (e.g. ST 2110-30, `rtp_audio` input) |
| critical | output '{id}': audio_encode encoder spawn failed: {error} | `AudioEncoder::spawn` failed for any other reason (codec rejected by ffmpeg, etc.) |
| info | TS audio encoder started: output '{id}' | In-process TsAudioReplacer created for SRT/RIST/RTP/UDP outputs. `output_id` set on event | `{ codec }` |
| critical | TS audio encoder failed: output '{id}': {error} | TsAudioReplacer construction rejected. `output_id` set on event | `{ error }` |

**Source**: `src/engine/audio_encode.rs`, `src/engine/output_rtmp.rs`,
`src/engine/output_hls.rs`, `src/engine/output_webrtc.rs`,
`src/engine/output_srt.rs`, `src/engine/output_rist.rs`,
`src/engine/output_rtp.rs`, `src/engine/output_udp.rs`.

---

### PID bus / Flow Assembly (`flow`)

Critical events emitted by `build_assembly_plan()` at flow bring-up and by `FlowRuntime::replace_assembly()` on runtime hot-swap when an `assembly` block fails to resolve. Every event carries a structured `details` block — at minimum `{ "error_code": "..." }`, usually plus `input_id`, `input_type`, `program_number`, etc. The manager UI matches on `error_code` to highlight the offending form field without parsing the error string.

| `error_code` | When | Typical `details` | Notes |
|---|---|---|---|
| `pid_bus_spts_input_needs_audio_encode` | Flow bring-up: a referenced input could produce TS via input-level `audio_encode` but isn't configured. | `{ input_id, input_type }` | Set `audio_encode.codec = "aac_lc"` (or HE-AAC / s302m) on the input. ST 2110-31 must use `s302m`. |
| `pid_bus_audio_encode_codec_not_supported_on_input` | Flow bring-up: `audio_encode.codec` validates but has no Phase 6.5 runtime path (today: `mp2`, `ac3`). | `{ input_id, input_type }` | First-light codecs: `aac_lc`, `he_aac_v1`, `he_aac_v2`, `s302m`. `mp2` / `ac3` deferred. |
| `pid_bus_spts_non_ts_input` | Flow bring-up: referenced input has no current path to TS (ST 2110-40 ANC, or a non-TS input without an `audio_encode` escape hatch). | `{ input_id, input_type }` | ST 2110-40 ANC-to-TS wrapping is deferred. |
| `pid_bus_no_program` | Flow bring-up: `assembly.kind = spts/mpts` but `programs` is empty. | `{}` | Should not normally reach runtime — config validation catches this earlier. |
| `pid_bus_essence_kind_not_implemented` | Flow bring-up: `SlotSource::Essence` with a `kind` the resolver can't yet satisfy. | `{ input_id, kind }` | First-light supports `video` and `audio`; `subtitle` / `data` under development. |
| `pid_bus_essence_no_catalogue` | Flow bring-up: Essence slot but the named input has no PSI catalogue yet (non-TS input or ingress not warm). | `{ input_id }` | Switch to a `SlotSource::Pid` slot, or wait for PSI; re-try with `UpdateFlowAssembly`. |
| `pid_bus_essence_no_match` | Flow bring-up: Essence slot of kind X, but no matching ES found in the input's PMT. | `{ input_id, kind }` | Check the input's live PSI catalogue in the manager UI. |
| `pid_bus_spts_stream_type_mismatch` | Warning logged when a slot's configured `stream_type` doesn't match the source PMT's declared `stream_type`. | `{ input_id, source_pid, configured, observed }` | Non-fatal — the slot still forwards bytes. Fix the `stream_type` on the slot to match the upstream PMT. |
| `pid_bus_hitless_leg_not_pid` | Flow bring-up: a `SlotSource::Hitless` leg is neither `Pid` nor `Essence`. | `{ program_number, leg: "primary"/"backup" }` | Nested Hitless is rejected at config-save time; this fires only if a follow-up variant slips past validation. |
| `pid_bus_mpts_pcr_source_required` | Flow bring-up: MPTS program has no effective PCR (neither program-level `pcr_source` nor flow-level fallback). | `{ program_number }` | Config validation also catches this — runtime check is a belt-and-braces guard. |
| `pid_bus_pcr_source_unresolved` | Flow bring-up: configured `pcr_source` `(input_id, pid)` doesn't hit any slot in its program (or in an Essence-slot's input). | `{ input_id, pid, program_number }` | Make sure the PCR PID is one of the PIDs you're carrying into the program. |
| `pid_bus_switch_empty_legs` | Config-save: a `SlotSource::Switch` has zero legs. | `{}` | A switch slot needs at least one leg. Add the input(s) to switch between. |
| `pid_bus_switch_too_many_legs` | Config-save: a `SlotSource::Switch` has more than 64 legs. | `{}` | Cap is 64 — split into multiple switch slots if you genuinely need more. |
| `pid_bus_switch_duplicate_leg` | Config-save: two legs in the same Switch slot share the same identity (`(input_id, source_pid)` for `pid` legs, `(input_id, kind)` for `essence` legs). | `{}` | Each leg must be unique within a slot. |
| `pid_bus_switch_initial_leg_unknown` | Config-save: `initial_input_id` doesn't match any leg's `input_id`. | `{}` | Pick "Initially active" on one of the leg rows in the Advanced editor. |
| `pid_bus_switch_legs_kind_mismatch` | Config-save: every leg of a Switch slot is `essence`-typed, but they don't all agree on `kind`. | `{}` | All Essence legs in the same Switch slot must declare the same essence kind. |
| `pid_bus_switch_nested_in_hitless` | Config-save: a `SlotSource::Switch` appears as a leg of a `SlotSource::Hitless`. | `{}` | Switch-in-Hitless is rejected by design. Use either the Hitless variant (auto-failover) or a Switch slot (operator-driven), not both nested. Switch-in-Switch is type-system impossible. |
| `pid_bus_switch_codec_mismatch` | Reserved — runtime-only check for a future follow-up that cross-checks the active leg's resolved `stream_type` against the slot's declared `stream_type` on every flip. **Not emitted today.** | `{ slot_path, expected_stream_type, actual_stream_type, leg_input_id }` | Today: rely on config-save `pid_bus_spts_stream_type_mismatch` for legs whose source is known up front. Future: the Take ack will lift this code so the manager UI can flash the offending preset. |
| `pid_bus_switch_no_match` / `pid_bus_switch_no_catalogue` | Reserved — runtime-only Essence-leg resolution failure on a Switch slot, mirroring the existing `pid_bus_essence_*` codes. **Not emitted today** (the existing `pid_bus_essence_*` codes fire instead during initial resolution at flow bring-up). | `{ input_id, kind }` | Reserved for a future per-flip runtime resolver. Today, an Essence leg that fails to resolve fires `pid_bus_essence_no_match` / `pid_bus_essence_no_catalogue` at flow bring-up. |
| `pid_bus_switch_splice_budget_out_of_range` | Config-save: `SlotSource::Switch.splice_budget_ms` is outside the accepted `20..=5000` ms range. | `{}` | Pick a budget between 20 ms and 5 s; omit the field to use the default (200 ms for audio). |
| `pes_splice_completed` | Info, **PES Switch Phase 4**. PES-aligned splice committed. Audio (`kind: "audio"`, default since edge 0.64.0) commits on B's first PUSI=1 PES with `pts ≥ last_a_pts + audio_frame_duration`; video (`kind: "video"`, edge 0.66.0) additionally requires an IDR NAL in the same PES (H.264 type 5, HEVC IRAP 16..=21). Active leg has been flipped, PMT bumped (v+1 mod 32), DI=1 armed on next PCR, fresh PSI pushed. Receiver should see a glitchless cut. | `{ kind?, program_number, out_pid, to_input_id, first_b_pts }` (the `kind` field is only present on video splices for backwards compatibility with manager UIs that pre-date the video MVP) | Operator-visible confirmation that the PES-aligned path actually fired. Without this event the operator can't tell whether the splice mode is having any effect. |
| `pes_splice_timeout` | Warning, **PES Switch Phase 4**. PES-aligned splice budget expired without B producing an aligned PES — runtime fell back to today's `pmt_bump` path (immediate flip + PMT v+1 + DI=1). Audio (`kind: "audio"`) and video (`kind: "video"`) both surface here; video timeouts usually indicate B's GoP exceeds the configured `splice_budget_ms` (default 2000 ms — longer than the audio default because the next IDR may be 1–2 s away). | `{ kind, program_number, out_pid, to_input_id }` | Usually means B's stream rate is much slower than expected, B isn't producing PUSI=1 in time, or (video only) B's next IDR didn't land inside the budget. Bump `splice_budget_ms` if expected; otherwise inspect B's ingress for buffer/jitter / GoP-length issues. |
| `pes_splice_codec_param_mismatch` | Warning, **PES Switch Phase 4**. PES-aligned splice arrived at the right boundary but B's codec parameters differ from A's; the runtime refused the PES-aligned commit and fell back to today's `pmt_bump` path (flip + PMT v+1 + DI=1) so receivers re-initialise the decoder cleanly on the new params. The `kind` field tells the two families apart — `"audio"` for AAC `AudioSpecificConfig` differences (profile / sample_rate / channel_config; coverage extended to LATM in edge 0.67.0); `"video"` for SPS-derived differences on H.264 / HEVC slots (profile_idc / level_idc / chroma_format / bit-depth / width / height). | Audio: `{ kind: "audio", program_number, out_pid, to_input_id, a_aac_params: {profile, sample_rate_idx, sample_rate_hz, channel_config}, b_aac_params: {...} }` (kind field is the audio-MVP shape and is omitted in pre-0.67.0 events for backwards compat). Video: `{ kind: "video", program_number, out_pid, to_input_id, a_video_params: {profile_idc, level_idc, chroma_format_idc, bit_depth_luma, bit_depth_chroma, width, height}, b_video_params: {...} }`. | Operator-actionable: receivers will still recover via the PMT bump, but the cut isn't glitch-free. Re-encode the legs to a common config (same SPS for video, same AudioSpecificConfig for audio) or set the slot's `splice_mode` to `pmt_bump` deliberately to silence the event. |
| `pid_bus_slot_source_closed` | Warning. Runtime: an assembled flow's slot fan-in lost its upstream ES bus channel — the `(input_id, source_pid)` publisher dropped while the parent assembler is still running. Most commonly this means the **owning flow** of `source_input_id` was stopped while sibling flows still reference it via assembly slots (the "input-host flow" pattern) — see `bilbycast-edge/docs/architecture.md` "Input-host flows". The slot's output goes silent until the owning flow restarts (the bus channel re-arms automatically on owner restart and the next packet wakes the assembler). Suppressed during normal teardown — the assembler's `cancel.cancelled()` arm wins over the channel-close arm when the parent flow is being stopped or hot-swapped. | `{ error_code: "pid_bus_slot_source_closed", program_number, out_pid, source_input_id, source_pid }` | Restart the owning flow, OR remove the cross-flow reference from this flow's assembly. |

**Source:** `src/engine/flow.rs` (`build_assembly_plan`, `non_ts_spts_error_code`, `resolve_essence_slots`), `src/engine/ts_assembler.rs` (`run_assembler` — splice commit / timeout, `slot_fanin` — `pid_bus_slot_source_closed`), `src/engine/pes_splice.rs` (audio + video splice state machines, AAC sentinel, IDR-aware video boundary detector), `src/config/validation.rs` (`validate_slot_source`). The edge manager-WS client also lifts these codes onto `command_ack.error_code` for `UpdateFlowAssembly` so the manager UI can highlight the offending field without needing the event stream.

---

### Video Encoder (`video_encode`)

In-process video transcoding lifecycle for TS outputs (SRT, RIST, RTP, UDP).
Emitted at `TsVideoReplacer::new()` call sites in each output module.

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | Video encoder started: output '{id}' | TsVideoReplacer created successfully. `output_id` set on event | `{ codec }` |
| critical | Video encoder failed: output '{id}': {error} | TsVideoReplacer construction rejected (missing feature, unsupported codec). `output_id` set on event | `{ error }` |

**Source**: `src/engine/output_srt.rs`, `src/engine/output_rist.rs`,
`src/engine/output_rtp.rs`, `src/engine/output_udp.rs`.

### Encoder runtime diagnostics

Surfaced as structured `tracing::warn!` lines (not WS events) — they ride
the edge log and the manager picks them up via the standard log-stream
ingest. Each carries `error_code` so dashboards / alerting rules can match
without parsing the human-readable message.

| `error_code` | Severity | Trigger | Fields | What it tells the operator |
|---|---|---|---|---|
| `encoder_chroma_not_supported` | warn | ST 2110-20 ingress encoder open path detects that the operator-pinned HW backend (`hevc_vaapi`, `hevc_qsv`, `hevc_nvenc`, `h264_vaapi`, `h264_qsv`, `h264_nvenc`) doesn't support the requested `(chroma, bit_depth)` on this host's probe matrix. The flow keeps running on a SW fallback (`x265` for HEVC, `x264` for H.264). | `requested`, `backend`, `chroma`, `bit_depth`, `fallback` | This host's iGPU/driver lacks the matching VAAPI/QSV/NVENC entrypoint (e.g. Arrow Lake iHD has no `VAProfileHEVCMain422_10`). Use `hevc_auto` / `h264_auto` to let the resolver pick the cheapest supported backend, or pin to the SW backend explicitly to suppress the warn. Same error code is used by the manager-side preflight in `device-edge/src/validation.rs`. |
| `video_encode_fps_mismatch` | warn | `engine::ts_video_replace::TsVideoReplacer` measures the source frame rate from DTS deltas and finds it disagrees with `video_encode.fps_num` / `fps_den` by more than 0.1 % (one-shot per encoder run). | `measured_fps`, `pinned_fps_num`, `pinned_fps_den`, `pinned_fps`, `drift_pct` | The encoder is running at the operator's pinned time_base, but receiving frames at the source rate — A/V sync will drift by the rate-ratio bias. Common case: NTSC source (29.97 fps = 30000/1001) into a `25/1` pinned encoder. Either remove the pin (the encoder auto-locks to the source rate when unpinned) or set it to the source rate explicitly. |

**Source**: `src/engine/st2110_video_io.rs` (chroma resolver), `src/engine/ts_video_replace.rs` (fps mismatch).

---

### RTP Input (`rtp`)

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | RTP input listening on {addr} | Socket successfully bound | `{ bind_addr }` |
| critical | RTP input bind failed: {error} | UDP socket bind failed (port in use, permission denied) | `{ bind_addr, error }` |

For redundant RTP inputs, each leg emits its own bind event with a `leg` field in details.

**Source**: `src/engine/input_rtp.rs`

---

### UDP Input (`udp`)

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | UDP input listening on {addr} | Socket successfully bound | `{ bind_addr }` |
| critical | UDP input bind failed: {error} | UDP socket bind failed (port in use, permission denied) | `{ bind_addr, error }` |

**Source**: `src/engine/input_udp.rs`

---

### Per-NIC Interface Binding

Per-input/per-output `interface_binding` field (loose source-IP bind by default; strict `SO_BINDTODEVICE` when `strict: true`). Surface details + capability gating: [`docs/configuration-guide.md`](configuration-guide.md#per-nic-interface-binding).

| Severity | Error code | Trigger | Details |
|----------|-----------|---------|---------|
| critical | `interface_not_found` | `name` doesn't match any interface enumerated on the host at bind time. The error message lists available NIC names. | `{ name, available }` |
| critical | `interface_binding_strict_denied` | `setsockopt(SO_BINDTODEVICE)` returned `EPERM`. Edge process lacks `CAP_NET_RAW` — install `packaging/strict-binding.conf` and restart. | `{ name }` |
| warning | `interface_binding_legacy_addr_ignored` | Both `interface_binding` and a legacy `interface_addr` are set on the same struct. New field wins; legacy is ignored. Operator should pick one and remove the other. | `{ field }` |
| critical | `srt_strict_binding_unsupported` | `strict: true` requested on an SRT/RIST surface or SRT bonding endpoint. Phase 1 limitation — pending `SRTO_BINDTODEVICE` plumbing in `bilbycast-libsrt-rs` / librist. Use `strict: false` (loose source-IP binding) or pin via UDP-based protocols. | `{ where }` |

**Source**: `src/util/socket.rs` (`resolve_interface_binding`, `apply_strict_binding`, `srt_local_addr_from_binding`), `src/config/validation.rs` (`validate_interface_binding`).

---

### Media Player Input (`flow`)

The media-player input emits its lifecycle events on the shared `flow`
category (rather than its own category) so they thread into the same
flow-level event stream as start/stop/fail. Each event carries
`flow_id` and `input_id` in `details`.

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | Media player input started | Input task came up; reports source count and `loop_playback` / `shuffle` flags | `{ flow_id, input_id, source_count, loop_playback, shuffle }` |
| critical | Media player source failed: {error} | A source failed to open, parse, or render (missing file, corrupt container, unsupported codec). Engine sleeps 2 s and advances to the next source. | `{ error_code, flow_id, input_id, source_index, source_kind, source_name, error }` |
| info | Media player playlist exhausted | Final source finished and `loop_playback = false`. The input task exits cleanly — restart the flow to replay. | `{ flow_id, input_id }` |

The Critical "source failed" event carries a stable `error_code` in
`details` so the manager UI can attribute and highlight the offending file
in the library picker without parsing the free-form message:

| `error_code` | Trigger |
|---|---|
| `media_player_source_missing` | File not found on disk (`io::Error::NotFound`) or unreadable (`PermissionDenied`). |
| `media_player_source_unsupported` | Container parse failed — no MPEG-TS sync byte in the first 1 MiB, or `io::Error::InvalidData` from a downstream demuxer. |
| `media_player_source_codec_unsupported` | The active feature set cannot decode this source kind — e.g. an MP4 / image source in a build that lacks `media-codecs` + `fdk-aac`. |
| `media_player_source_render_failed` | Read or render error after the source opened — transient I/O, decoder crash, or other runtime fault. |
| `media_player_source_failed` | Generic fallback when no specific cause was identified. |

**Source**: `src/engine/input_media_player.rs`

The bind-failure unification (`port_conflict` / `bind_failed`) does not
apply — the media player is file-backed and binds no sockets.

---

### Tunnel (`tunnel`)

| Severity | Message | Trigger | Details |
|----------|---------|---------|---------|
| info | Tunnel '{name}' started | Tunnel created and connecting | `{ tunnel_name }` |
| info | Tunnel '{name}' stopped | Tunnel stopped by command or config change | `{ tunnel_name }` |
| info | Tunnel connected to relay | QUIC connection established and TunnelReady received | `{ relay_addr }` |
| warning | Tunnel disconnected from relay: {reason} | QUIC connection lost or forwarder exited | `{ reason }` |
| warning | Tunnel peer disconnected: {reason} | Relay reported peer unbound (TunnelDown) | `{ reason }` |
| warning | Tunnel connection to relay failed: {error} | QUIC connect or TLS error | `{ relay_addr }` |
| warning | Tunnel '{name}' attempt {N} failed: {error} | Direct-mode retry failed | `{ tunnel_name, attempt }` |
| critical | Tunnel '{name}' failed: {error} | Tunnel task exited with fatal error | `{ tunnel_name }` |
| critical | Tunnel bind rejected by relay: {reason} | HMAC bind token verification failed | `{ relay_addr }` |

**Source**: `src/tunnel/manager.rs`, `src/tunnel/relay_client.rs`

---

### Manager Connection (`manager`)

| Severity | Message | Trigger |
|----------|---------|---------|
| info | Connected to manager | WebSocket auth succeeded |
| warning | Manager connection lost, reconnecting | WebSocket closed or errored |
| critical | Manager authentication failed: {reason} | Auth rejected by manager |

**Source**: `src/manager/client.rs`

---

### NMOS Registry Client (`nmos_registry`)

Surfaced by the IS-04 registration client (`src/api/nmos_registration.rs`) when
the edge is configured to push to an external NMOS registry — see
[`docs/nmos.md`](nmos.md) ("Registration Client"). All four events carry
`details.error_code` so the manager UI can colour-code the rows and operators
can grep on a stable string.

| Severity | Message | Trigger | `details.error_code` |
|----------|---------|---------|----------------------|
| info | `Registered with NMOS registry <url>` | First successful POST of the node resource on (re-)registration. | `nmos_registered` |
| warning | `NMOS heartbeat to <url> returned HTTP <status> — re-registering` | Heartbeat returned non-2xx (typically `404` because the registry expired the node). The client falls back to the registration phase on the next tick. | `nmos_heartbeat_lost` |
| critical | `NMOS registration of <type> at <url> failed: HTTP <status> <error>` | A registration POST returned 4xx/5xx. The client retries with exponential backoff. | `nmos_registration_failed` |
| warning | `NMOS registry <url> unreachable: <error>` | Network / DNS / TLS error reaching the registry. The client retries with exponential backoff. | `nmos_registry_unreachable` |

### Configuration (`config`)

| Severity | Message | Trigger |
|----------|---------|---------|
| info | Configuration updated | Config applied via manager command |
| warning | Failed to persist configuration: {error} | Config write to disk failed after update |

**Source**: `src/manager/client.rs`

### System Resources (`system_resources`)

| Severity | Message | Trigger |
|----------|---------|---------|
| warning | CPU usage {value}% exceeds warning threshold {threshold}% | CPU stays above warning threshold for grace period |
| critical | CPU usage {value}% exceeds critical threshold {threshold}% | CPU stays above critical threshold for grace period |
| warning | RAM usage {value}% exceeds warning threshold {threshold}% | RAM stays above warning threshold for grace period |
| critical | RAM usage {value}% exceeds critical threshold {threshold}% | RAM stays above critical threshold for grace period |
| warning | {metric} usage recovered below critical, still above warning | Metric drops below critical but remains above warning |
| info | {metric} usage returned to normal | Metric drops below warning threshold |

**Details**: `{ "metric": "cpu"|"ram", "current_percent": float, "warning_threshold": float, "critical_threshold": float }`

**Source**: `src/engine/resource_monitor.rs`

| warning | Flow '{id}' creation blocked: system resources critical | Flow creation rejected because `critical_action` is `"gate_flows"` and a metric is in critical state | `{ flow_id }` |

**Source**: `src/engine/resource_monitor.rs`, `src/engine/manager.rs`

Requires `resource_limits` config block. When `critical_action` is `"gate_flows"`, new flow creation is rejected while any metric is in the critical state. Events fire on state transitions with a configurable grace period (default 10 seconds) to avoid flapping.

#### HW encoder oversubscription

| Severity | Message | Trigger |
|----------|---------|---------|
| warning | Flow '{id}' caused {family} encoder oversubscription: {in_use} sessions in use, {max} probed at startup. Reduce HW transcodes or restart on a host with more capacity. | A new flow's HW `video_encode` outputs push the per-family in-use count above the startup-probed `max_sessions`. Soft warning only — the flow still starts. |

**Details**: `{ error_code: "hw_encoder_oversubscribed", family: "nvenc"|"qsv"|"amf", role: "encoder", in_use: u32, max_sessions: u32, flow_id }`

**Source**: `src/engine/manager.rs::create_flow` → `emit_hw_oversubscribe_warnings`. The probed-max-sessions number comes from `engine::hardware_probe::probe_encoder_session_limits` at startup (capped at 8; see [`docs/configuration-guide.md`](configuration-guide.md#capacity--resource-budget) and the `BILBYCAST_PROBE_SESSION_LIMITS=0` opt-out). Soft warning matches the existing modal `updateResourceImpact` 80/100 % units pattern — the alarm tells the operator to fix it without blocking flow creation.

### Remote upgrade (`upgrade`)

Surface for the `upgrade_binary` lifecycle. Manager UI gates the per-node "Upgrade" button on the `"upgrade"` capability bit; older edges that predate this surface never see the controls.

| Severity | Error code | Trigger |
|----------|------------|---------|
| info | `upgrade_started` | Manager command accepted, staging begins. Carries `from_version`, `to_version`, `channel`, `arch`, `variant`. |
| info | `upgrade_downloaded` | Manifest verified + tarball downloaded + SHA-256 matched. |
| info | `upgrade_staged` | Tarball extracted, symlink swapped. Edge is about to drain flows + exit for systemd respawn. |
| info | `upgrade_completed` | New binary booted + authenticated to manager + healthy for `boot_health_window_secs`. Status flipped to `stable`. |
| info | `upgrade_staged_manual` | `manual_only = true` — tarball is on disk under `versions/<v>/` but the symlink swap is deferred until a SIGUSR1. |
| critical | `upgrade_rolled_back` | Boot watchdog reverted the symlink to `previous` after `max_boot_attempts` failed boots, **or** the new binary failed to authenticate within `boot_health_window_secs`. Carries `from_version`, `to_version`. |
| critical | `upgrade_signature_invalid` | Sigstore bundle signature did not verify against the manifest bytes. |
| critical | `upgrade_identity_not_allowed` | Bundle was signed but the cert's identity claims (issuer / repo / workflow / ref) do not match the compiled-in `ALLOWED_SIGNERS` allowlist. The actual identity claims are logged for forensic review. |
| critical | `upgrade_rekor_invalid` | Rekor inclusion proof missing or malformed. |
| warning | `upgrade_disabled` | Edge received `upgrade_binary` while `upgrades.enabled = false`. The command is rejected; this event is purely audit. |
| warning | `upgrade_channel_not_allowed` | Edge received `upgrade_binary` for a channel not in `upgrades.allowed_channels`. |
| warning | `upgrade_version_too_old` | Requested version is below `min_version` or further back than `rollback_grace`. |
| warning | `upgrade_sequence_too_old` | Manifest's `sequence` is `≤` the last installed sequence — replay defence. |
| warning | `upgrade_in_progress` | A second `upgrade_binary` arrived while the first was still staging. |
| warning | `upgrade_url_invalid` | Manifest tarball URL host is not in the upgrade host whitelist (`github.com` / `objects.githubusercontent.com`). |
| warning | `upgrade_checksum_mismatch` | Downloaded tarball SHA-256 did not match the value in the verified manifest. |
| warning | `upgrade_extract_failed` | Tarball extracted but the binary couldn't be located, hoisted, or made executable. |
| warning | `upgrade_disk_full` | `ENOSPC` while extracting. |
| warning | `upgrade_network_error` | Manifest, bundle, or tarball fetch failed after retries. |
| warning | `upgrade_arch_mismatch` | Manifest carries no artefact for the host's `(arch, variant)` tuple. |

**Details**: `{ error_code, from_version, to_version, channel, arch, variant, … }` (fields populated according to the lifecycle stage).

**Source**: `src/upgrade/mod.rs::error_codes`, emitted by `src/upgrade/{mod,watchdog,verify,download,apply}.rs`. The full operator-facing trust model and runbook live in [`docs/upgrade.md`](upgrade.md).

### MXL (Media eXchange Layer) (`flow`)

Lifecycle + format events for MXL inputs / outputs. Gated by the `mxl` Cargo feature (default off). The manager UI surfaces these events only when an edge advertises one of `mxl-video` / `mxl-audio` / `mxl-anc` on `HealthPayload.capabilities`. See [`docs/mxl-integration-plan.md`](mxl-integration-plan.md) for the integration plan and [`bilbycast-mxl-rs/CLAUDE.md`](../../bilbycast-mxl-rs/CLAUDE.md) for the build prereq footprint.

| Severity | Error code | Trigger |
|----------|------------|---------|
| critical | `mxl_domain_unavailable` | Boot-time `MxlDomainManager::probe()` failed to locate or dlopen libmxl.so. Capability bits are not advertised; existing MXL flow configs refuse to start. |
| critical | `mxl_ptp_required` | Flow validation: `master_clock = wallclock` was configured on a flow that references any MXL input or output. MXL is PTP-anchored at v1.0; wallclock would silently drift relative to upstream / downstream MXL pods. |
| critical | `mxl_input_not_wired` | M2-pending stub fires when a flow with an MXL input starts on a build whose engine modules haven't been wired yet. Will be removed when M2 engine modules land. |
| critical | `mxl_output_not_wired` | Same as `mxl_input_not_wired` but on the output side. M2/M3 engine modules land per essence. |
| warning | `mxl_domain_not_tmpfs` | The configured `domain_path` is not on `tmpfs` or `ramfs` per `statfs(2)`. libmxl's perf model degrades sharply off tmpfs; recommend operator mount the path on tmpfs (the default for `/dev/shm`). |
| warning | `mxl_format_unsupported` | Operator config specifies a format not yet supported by upstream MXL v1.0 (non-V210 video, non-Float32 audio, non-48 kHz sample rate). |
| warning | `mxl_grain_drop` | An MXL output dropped grains because the broadcast subscriber lagged. Increments `OutputStats.packets_dropped`. |

**Details**: `{ error_code, domain_path, flow_name, kind ("video" | "audio" | "anc"), … }`.

**Source**: M1 wired `mxl_domain_unavailable` from the boot probe path (`src/main.rs`); the per-flow events land alongside the M2/M3 engine modules in `src/engine/mxl_io.rs` and `src/engine/mxl_video_io.rs`. Until then, `mxl_input_not_wired` / `mxl_output_not_wired` are emitted from the flow dispatch stub in `src/engine/flow.rs::spawn_single_input` / `start_output`.

---

## Manager-Generated Events

In addition to events sent by the edge, the manager itself generates these events when an edge connects or disconnects:

| Severity | Category | Message | Trigger |
|----------|----------|---------|---------|
| info | connection | Node connected to manager | Edge successfully authenticates |
| warning | compatibility | Node WS protocol version differs | Protocol version mismatch during auth |
| critical | connection | Node disconnected from manager | Edge WebSocket closes |

These are generated server-side in `bilbycast-manager/crates/manager-server/src/ws/node_hub.rs`.

---

## Event Categories Summary

| Category | Count | Description |
|----------|-------|-------------|
| `flow` | 16 | Flow lifecycle (start/stop/fail, output add/remove, input/output CRUD including update, media-player start/source-failed/playlist-exhausted) |
| `bandwidth` | 4 | Per-flow bandwidth monitoring (alarm, block, recovery) |
| `srt` | 9 | SRT input and output connection state (now with structured details) |
| `redundancy` | 3 | SMPTE 2022-7 dual-leg status |
| `rtmp` | 3 | RTMP publisher connections |
| `rtsp` | 2 | RTSP input state (now with structured details) |
| `hls` | 2 | HLS output failures |
| `webrtc` | 8 | WHIP/WHEP session lifecycle |
| `audio_encode` | 9 | Audio encoder lifecycle — ffmpeg sidecar (RTMP/HLS/WebRTC) + in-process TsAudioReplacer (SRT/RIST/RTP/UDP) |
| `video_encode` | 2 | Video transcoder lifecycle — in-process TsVideoReplacer (SRT/RIST/RTP/UDP) |
| `tunnel` | 9 | Tunnel connection state (now with structured details) |
| `manager` | 3 | Manager WebSocket connection |
| `config` | 2 | Configuration changes |
| `master_clock` | 2 | Source-PCR PLL lock-state transitions (fallback to wallclock, recovery) |
| `system_resources` | 7 | CPU/RAM threshold monitoring + flow creation gating |
| `rtp` | 2 | RTP input bind and lifecycle |
| `udp` | 2 | UDP input bind and lifecycle |
| `rist` | 4 | RIST Simple Profile connection lifecycle |
| `bond` | 5 | Bonded input/output — per-path alive/dead + bond-aggregate degraded/down/recovered |
| `ptp` | — | SMPTE ST 2110 PTP slave clock state changes (Phase 1) |
| `network_leg` | — | SMPTE 2022-7 Red/Blue per-leg loss / recovery (Phase 1) |
| `nmos` | — | NMOS IS-04 / IS-05 / IS-08 controller activity (Phase 1) |
| `nmos_registry` | 4 | IS-04 registration client lifecycle (registered, heartbeat lost, registration failed, registry unreachable) |
| `scte104` | — | SCTE-104 splice events parsed from ST 2110-40 ANC (Phase 1) |
| **Total** | **89** | |

### Phase 1 ST 2110 categories

The four `ptp` / `network_leg` / `nmos` / `scte104` categories are
defined in `src/manager/events.rs` as part of the SMPTE ST 2110 work.
Producers wire them in step 9 of the Phase 1 plan; the categories are
declared up-front so the manager UI's category icons and filter
dropdown render them as soon as the first event arrives. Severity
mapping:

| Category | Typical severity | Triggers |
|----------|------------------|----------|
| `ptp` | info / warning / critical | `ptp_lock_acquired`, `ptp_lock_lost`, `ptp_holdover`, `ptp_unavailable` |
| `network_leg` | warning / critical | `red_leg_lost`, `blue_leg_lost`, `leg_recovered`, `both_legs_lost` |
| `nmos` | info | NMOS controller IS-05 activations, IS-08 channel-map changes |
| `scte104` | info | Cue-out / cue-in / cancel splice messages parsed from ANC |

### By Severity

| Severity | Count | Description |
|----------|-------|-------------|
| critical | 23 | Service-impacting: flow/tunnel failures, auth rejection, both legs lost, bandwidth block, audio/video encoder failures, bind failures (RTP/UDP/RIST), media-player source failed |
| warning | 23 | Degradation: disconnects, stale connections, upload failures, reconnects, bandwidth exceeded, audio_encode restart, resource gating, tunnel retry, master-clock PLL fallback |
| info | 43 | State changes: connections established, flows started, config updated, bandwidth recovery, encoder started, input/output CRUD, bind success (RTP/UDP), media-player started/playlist-exhausted, master-clock PLL recovery |

## Unified bind-failure events (`port_conflict` / `bind_failed`)

Every runtime bind site (SRT listener, RTP/UDP/RIST/RTMP inputs,
standby UDP/TCP listeners, tunnel egress) emits a Critical event under
one of two reserved categories:

| Category | When | `details.error_code` |
|----------|------|----------------------|
| `port_conflict` | OS reports `EADDRINUSE` or the config-load validator detected an internal collision between two configured entities | `port_conflict` |
| `bind_failed` | Any other bind error (permission denied, no such device, multicast group rejected, etc.) | `bind_failed` |

Standard `details` shape:

```json
{
  "error_code": "port_conflict",
  "component": "SRT input listener leg 2",
  "addr": "0.0.0.0:9527",
  "protocol": "UDP",
  "error": "<original error message>"
}
```

The manager UI keys off `error_code` to highlight the offending field
in the input/output modal, surfaces the event on the per-node banner
when an unresolved one occurred in the last hour, and exposes
quick-filter chips for both categories on `/events`. The same shape
appears on `command_ack` payloads via the new optional `error_code`
field on `CommandAckPayload`, so manager-initiated Create/Update
commands can rely on `error_code === "port_conflict"` without parsing
the `error` string.

Producers should use `EventSender::emit_port_conflict()` and
`emit_bind_failed()` (defined in `src/manager/events.rs`) rather than
emitting raw `Event` instances — the helpers populate `details` with
the canonical shape and the recent-event tracker that lets the
WS command handler return runtime bind failures synchronously on the
`command_ack` for `create_flow` / `update_flow`.

## Content Analysis events

In-depth content-analysis tiers (Lite, Audio Full, Video Full —
gated by `FlowConfig.content_analysis`, module:
[`src/engine/content_analysis/`](../src/engine/content_analysis/))
emit flow-scoped events when their heuristics cross a threshold.
Every event sets `details.error_code` to the category name so the
manager UI can route / filter without string parsing.

| Category | Severity | When it fires | Tier |
|---|---|---|---|
| `content_analysis_scte35_pid` | Info | A new PID carrying `stream_type = 0x86` (SCTE-35) appears in the PMT | Lite |
| `content_analysis_scte35_cue` | Info | A `splice_info_section` is decoded on any SCTE-35 PID (debounced to one event per unique cue) | Lite |
| `content_analysis_caption_lost` | Warning | Captions (SEI `user_data_registered_itu_t_t35` with ATSC `GA94`) were present, then disappear for ≥ 5 s. 30 s event-ratelimit. | Lite |
| `content_analysis_mdi_above_threshold` | Warning | RFC 4445 Delay Factor exceeds 50 ms in a 1 s window. 30 s event-ratelimit. | Lite |
| `content_analysis_audio_silent` | Warning | Decoded AAC audio's EBU R128 momentary loudness drops to ≤ −60 LUFS for ≥ 2 s (on codecs we can't yet decode — MP2 / AC-3 / E-AC-3 — the analyser falls back to the PES-bitrate-below-1 kbps proxy). 30 s event-ratelimit. | Audio Full |
| `content_analysis_video_freeze` | Warning | YUV sum-of-absolute-differences between two consecutive decoded video frames drops below 0.75 on a 0–255 mean-absolute-difference scale for ≥ 3 s. Runs on decoded Y planes from the in-process FFmpeg decoder (`video_engine::VideoDecoder`). 30 s event-ratelimit. | Video Full |

All tiers are opt-in via the `content_analysis` block on the flow
(Lite defaults ON for TS-carrying inputs; Audio/Video Full default
OFF). The tiers attach as independent broadcast subscribers — they
cannot add jitter or block the data path. Lag on the broadcast
channel increments a `lite_drops` / `audio_full_drops` /
`video_full_drops` counter on `ContentAnalysisAccumulator`.

## Configuration-guide pointer

Content-analysis configuration schema and examples live in
[`docs/configuration-guide.md`](configuration-guide.md) under
"Content Analysis (in-depth)".


## Replay-server events

The replay server (Phase 1, gated by the `replay` Cargo feature)
emits events under category `replay` for both recording-side and
playback-side state changes. Every event sets
`details.replay_event` to a stable string identifier; failure events
also set `details.error_code` for `command_ack` correlation.

| `replay_event` | Severity | When it fires | Stable `error_code` |
|---|---|---|---|
| `recording_started` | Info | A flow with `recording.enabled = true` brought up its writer | — |
| `recording_stopped` | Info | A `stop_recording` command was acked | — |
| `recording_start_failed` | Critical | The writer task failed to start (storage unavailable, permission denied) | `replay_disk_full` |
| `clip_created` | Info | A `mark_out` materialised a new clip into `clips.json` (with fsync). `details.clip` carries the full `ClipInfo`. | — |
| `clip_deleted` | Info | A `delete_clip` succeeded | — |
| `recording_deleted` | Info | A `delete_recording` succeeded — the recording's directory under `<replay_root>/<recording_id>/` was unlinked. `details.bytes_freed` carries the byte total reclaimed. | — |
| `playback_started` | Info | A replay input transitioned to playing | — |
| `playback_stopped` | Info | A replay input was stopped by command | — |
| `playback_eof` | Info | A replay input reached the end of its range and `loop_playback = false` | — |
| `writer_lagged` | Critical | The recording writer's bounded internal channel filled — packets dropped to keep the broadcast channel non-blocking. Rate-limited to one event per 5 s under sustained lag. | `replay_writer_lagged` |
| `disk_pressure` | Warning | Recording disk usage crossed 80 % of the configured `max_bytes` cap (or of the replay-root filesystem when `max_bytes = 0`). Sticky until usage falls back below 70 % so the events feed isn't spammed. `details.pct` carries the snapshot percentage. Emit early so operators can free disk before the recorder hits ENOSPC. | `replay_disk_pressure` |
| `disk_full` | Critical | The writer hit a disk-write error (typically EOSPC). Recording stops; the flow remains up. | `replay_disk_full` |
| `recovery_alert` | Warning | Edge restarted after a crash; `.tmp/` orphan segments were unlinked and / or `recording.json` was corrupt. Resume continues from the largest segment id on disk + 1. `details.tmp_orphans_removed`, `details.meta_corrupt`, `details.next_segment_id`. | `replay_recovery_alert` |
| `metadata_stale` | Warning | `recording.json` write failed on segment roll. Recovery scan on next start derives the resume id from the directory listing. | `replay_metadata_stale` |
| `max_bytes_below_segment` | Warning | Retention can't satisfy `max_bytes` without unlinking the live edge — the operator's cap is smaller than one segment. | `replay_max_bytes_below_segment` |

Stable `command_ack.error_code` values surfaced by the replay-server WS
actions (`start_recording`, `mark_in`, `mark_out`, `cue_clip`,
`play_clip`, `scrub_playback`, `delete_clip`, `get_clip`, …):

| `error_code` | Meaning |
|---|---|
| `replay_recording_not_active` | Flow has no `recording` block, or `mark_in/mark_out` came before the recorder was armed |
| `replay_no_playback_input` | `cue_clip` / `play_clip` / `stop_playback` / `scrub_playback` was sent to a flow whose active input is not a `replay` variant |
| `replay_clip_not_found` | The requested `clip_id` does not exist on this edge |
| `replay_writer_lagged` | The recording writer dropped packets — see the matching Critical event |
| `replay_disk_pressure` | Recording disk usage at 80 %+ — see the matching Warning event. Operators should free disk before the recorder trips ENOSPC |
| `replay_disk_full` | The recording writer could not write a segment — typically EOSPC |
| `replay_index_corrupt` | `index.bin` failed CRC / size validation on startup; the writer surfaces this as a Warning + rebuild |
| `replay_invalid_segment_seconds` / `replay_invalid_recording_id` / `replay_storage_id_invalid` | Validation rejection at config save / `update_flow` time |
| `replay_invalid_field` | `mark_out` / `rename_clip` / `update_clip` `name` exceeded 256 chars / contained control characters, or `description` exceeded 4096 chars |
| `replay_invalid_range` | `play_clip` / `scrub_playback` was given a `to_pts_90khz` < `from_pts_90khz` (or below the clip's `in_pts`); `update_clip` was given a prospective `in_pts_90khz` / `out_pts_90khz` that would invert the clip range |
| `replay_invalid_tag` | Phase 2 / 1.5 — `update_clip` (or any tag-bearing path) carried a tag that failed `[A-Z0-9_-]{1,32}`, or > 16 tags per clip |
| `replay_clip_update_failed` | `update_clip` produced no result (clip vanished mid-update or persistence failed for an unclassified reason) |
| `replay_recovery_alert` | Crash-recovery scan ran on writer init — see matching Warning event |
| `replay_metadata_stale` | `recording.json` write failed; resume id is derived from disk on restart |
| `replay_max_bytes_below_segment` | `max_bytes` smaller than one segment — retention can't keep usage under the cap without deleting the live edge |
| `replay_recording_active` | `delete_recording` was sent for a recording that the writer is currently appending to — operator must `stop_recording` first |
| `replay_export_format_unsupported` | `export_clip` / `export_recording` was given a `format` other than `"ts"` (Phase 1 ships TS only) |
| `replay_export_too_large` | `export_recording` resolved to > 4 GiB of bytes — operator should mark a clip first to bound the pull |
| `replay_export_failed` | `export_clip` / `export_recording` hit an unclassified read error mid-pull (transient I/O, segment removed by retention between calls). Manager retries from the same `byte_offset`. |
| `replay_no_segments` | `export_recording` ran against a recording with no segments on disk yet |
| `replay_no_index` | `export_clip` / `export_recording` ran against a recording with an empty `index.bin` (no IDR captured yet) |

Producers should use `EventSender::emit_with_details(EventSeverity::*,
category::REPLAY, message, flow_id, details)` (defined in
`src/manager/events.rs`) — `category::REPLAY` is the canonical
constant.

## Display-output events (`display`)

The local-display output (Linux-only, gated on the `display` Cargo
feature) emits events under category `display`. Every failure event
sets `details.error_code` for `command_ack` correlation, plus
`details.output_id` so the manager UI can attribute the failure to
the offending output row on a multi-output flow.

| Event | Severity | Trigger | Notes |
|---|---|---|---|
| `display_started` | Info | Modeset succeeded, ALSA opened (or muted), first frame queued | `details = { error_code: "ok", output_id, … }` |
| `display_stopped` | Info | Cancellation token fired; CRTC + framebuffers + ALSA released | Includes lifetime `frames_displayed` / `late_drops` / `audio_underruns` |
| `display_output_waiting` | Info | Another flow's display output already holds the targeted `(device, audio_device)` pair. The output has registered as a FCFS waiter on the per-edge `DisplayClaimRegistry` and parks here until the holder releases or the per-output cancel token fires. `KmsDisplay::open` has not been attempted yet. | `details = { error_code: "display_output_waiting", output_id, device, audio_device, holder_flow_id, holder_output_id, queue_position }` |
| `display_output_acquired` | Info | The per-edge `DisplayClaimRegistry` promoted this output to be the new holder of the `(device, audio_device)` pair. Always follows a `display_output_waiting` for the same `(flow_id, output_id)`; pairs with the existing `display_started` event a moment later (after the KMS modeset). | `details = { error_code: "display_output_acquired", output_id, device, audio_device, previous_holder_flow_id, previous_holder_output_id }` |
| `display_auto_matched` | Info | `scaling_mode: match_source` modeset to the source-covering mode succeeded | Fires on first decoded frame and again on every source-shape change |
| `display_auto_match_failed` | Warning | `scaling_mode: match_source` modeset rejected by KMS — output keeps running at the startup mode | Lifts whatever `display_*` error string KMS returned |
| `display_monitor_native_set` | Info | `scaling_mode: monitor_native` modeset to the connector-preferred (panel-native) mode succeeded | Fires once per output start; usually a no-op since KMS opened at preferred |
| `display_monitor_native_set_failed` | Warning | `scaling_mode: monitor_native` modeset rejected by KMS — output keeps running at the startup mode | Same shape as `display_auto_match_failed` |
| `display_device_unavailable` | Critical | KMS connector vanished mid-flow (cable unplug observed via udev or `drmModeGetConnector` `connection != connected`) | Surfaces `error_code: display_device_unavailable` |
| `display_mode_set_failed` | Critical | `drmModeSetCrtc` returned `EINVAL` / `ENOSPC` for the chosen resolution / refresh | `error_code: display_resolution_unsupported` or `display_mode_set_failed` |
| `display_audio_open_failed` | Critical | `snd_pcm_open` returned non-zero, or ALSA `writei` returned `ENODEV` mid-stream | `error_code: display_audio_device_invalid` / `display_audio_open_failed` |
| `display_decoder_overload` | Warning | `frames_dropped_late` > 5 % over a 5-s rolling window | Indicates the SW video decoder can't keep up — recommend HW decode (v2) or a smaller resolution |
| `display_av_drift` | Warning | `|av_sync_offset_ms|` > 100 ms sustained ≥ 3 s | Audio-vs-video drift exceeded the dup/drop window |
| `display_subscriber_lagged` | Warning | broadcast `Lagged(n)`; rate-limited to one event / second | The video + audio decoders are flushed and resync on the next IDR. The continuous-rate `subscriber_lag_events` counter on `DisplayStats` tracks every Lagged event for the manager UI |
| `display_unsupported_pixfmt` | Warning | Decoder emitted a frame in a pixel format the display path can't deinterleave / scale (e.g. 4:4:4 semi-planar, custom HW formats); first occurrence + re-emitted every 60 s while the rate persists | `details = { pixel_format (i32 AVPixelFormat), decoder_kind ("cpu"/"nvdec"/"qsv"/"vaapi"), width, height, output_id }`. Today the dispatch covers planar YUV 4:2:0 / 4:2:2 / 4:4:4 (8/10/12-bit) and semi-planar `NV12` / `NV16` / `P010LE` / `P016LE` / `P210LE` / `P216LE` — every other format drops frames silently between events. The continuous-rate `frames_dropped_unsupported_pixfmt` counter on `DisplayStats` tracks every dropped frame |
| `display_hw_decode_unavailable_falling_back` | Warning | Operator forced a HW backend (`hw_decode = nvdec` / `qsv` / `vaapi`) the host can't satisfy at probe time. The display task falls back to CPU and continues — broadcast invariant is "picture stays on screen". | `details = { error_code, output_id, preference, reason ("feature_disabled"/"driver_missing"/"probe_unavailable"/"no_readable_pixfmt"), fell_back_to: "cpu" }`. Manager UI flags the dropdown as "wrong choice" while the picture still renders via CPU |
| `display_hw_decode_runtime_failed` | Warning | Either (a) sustained `send_packet` error run on the HW decoder (≥ 30 consecutive errors ≈ 1 s on 25–30 fps), or (b) decoder accepted packets but produced no frame within 2.5 s of first send — both paths converge on `force_cpu_fallback`, which emits this once per HW→CPU demotion | `details = { error_code, output_id, requested_backend, fell_back_to: "cpu", trigger ("send_packet_errors"/"watchdog_no_frames"), last_error }`. The continuous-rate counters `send_packet_errors`, `decoder_demotions`, `frames_received_since_open` on `DisplayStats` complement this single event |
| `display_hw_decode_no_frames` | Warning | Watchdog: HW decoder opened cleanly + accepted at least one packet, but produced zero frames in 2.5 s. Always followed in the same iteration by `display_hw_decode_runtime_failed` (with `trigger: "watchdog_no_frames"`) — the two events together describe the symptom and the action | `details = { error_code, output_id, backend, ms_since_first_send }` |
| `display_input_switch_acquiring` | Info | `TsDemuxer` surfaced `DemuxedFrame::Discontinuity` because the upstream PMT `version_number` advanced — the canonical signal `TsContinuityFixer::on_switch` injects on every operator switch (including dead-input switches). The display loop flushes every persistent decoder so the next AU from the new stream becomes the fresh anchor | `details = { error_code, output_id }`. Always paired with a follow-up `display_input_switch_acquired` (success) or `display_input_switch_slow_gop` (warning) — the operator UI uses both to render an "acquiring..." chip with elapsed-ms |
| `display_input_switch_acquired` | Info | First decoded video frame from the new stream landed; the pre-flush counter (`frames_received_since_open`) advanced past 0 for the current open-window | `details = { error_code, output_id, elapsed_ms }`. Healthy short-GOP sources finish well under one second; long-GOP DVB-style sources finish near `2 × source_GOP_ms` |
| `display_input_switch_slow_gop` | Warning | Acquisition has been in flight for 5 s without producing a decoded frame. One-shot per acquiring window (re-armed on the next `_acquiring`) — usually a long source GOP or a genuinely broken pipeline | `details = { error_code, output_id, elapsed_ms }`. Distinguishable from `display_hw_decode_no_frames` by category (`flow` vs `system_resources`) and by whether the watchdog also fired in the same window |

Operator-driven `command_ack.error_code` values (lifted from
`add_output` / `update_config` failures):

| `error_code` | Meaning |
|---|---|
| `display_device_invalid` | `device` regex failed at config-load OR connector not present in `enumerate_displays()` at runtime OR build was compiled without the `display` Cargo feature / for a non-Linux target |
| `display_audio_device_invalid` | `audio_device` regex failed OR ALSA refused to open it |
| `display_resolution_unsupported` | Configured `resolution` / `refresh_hz` does not match any mode the connector advertises |
| `display_program_not_found` | After 5 s, the demuxer hasn't seen the configured `program_number` in the PAT |
| `display_audio_track_not_found` | Configured `audio_track_index` exceeds the PMT's audio-stream count |
| `display_device_busy` | Reserved for future use. The previous "static-config rejection on duplicate `(device, audio_device)`" semantics no longer apply: the validator now logs a `warn!` and allows duplicates so the runtime `DisplayClaimRegistry` can serialise them via take-over. Runtime contention emits `display_output_waiting` / `display_output_acquired` instead |
| `display_decoder_overload_predicted` | Validation-time warning when 4K60 is requested without HW decode — does NOT block save; surfaced as a hint in the manager UI |

Configuration schema and operator-visible knobs live in
[`docs/configuration-guide.md`](configuration-guide.md) under
"Display Output (HDMI / DisplayPort + ALSA)". Producers should use
`EventSender::emit_with_details(EventSeverity::*, "display", message,
flow_id, details)` (defined in `src/manager/events.rs`).
