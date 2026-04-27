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

**Source**: `src/engine/manager.rs`, `src/manager/client.rs`

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

**Source:** `src/engine/flow.rs` (`build_assembly_plan`, `non_ts_spts_error_code`, `resolve_essence_slots`), `src/engine/ts_assembler.rs`. The edge manager-WS client also lifts these codes onto `command_ack.error_code` for `UpdateFlowAssembly` so the manager UI can highlight the offending field without needing the event stream.

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
| `media_player_source_codec_unsupported` | The active feature set cannot decode this source kind — e.g. an MP4 / image source in a build that lacks `video-thumbnail` + `fdk-aac`. |
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
| `system_resources` | 7 | CPU/RAM threshold monitoring + flow creation gating |
| `rtp` | 2 | RTP input bind and lifecycle |
| `udp` | 2 | UDP input bind and lifecycle |
| `rist` | 4 | RIST Simple Profile connection lifecycle |
| `bond` | 5 | Bonded input/output — per-path alive/dead + bond-aggregate degraded/down/recovered |
| `ptp` | — | SMPTE ST 2110 PTP slave clock state changes (Phase 1) |
| `network_leg` | — | SMPTE 2022-7 Red/Blue per-leg loss / recovery (Phase 1) |
| `nmos` | — | NMOS IS-04 / IS-05 / IS-08 controller activity (Phase 1) |
| `scte104` | — | SCTE-104 splice events parsed from ST 2110-40 ANC (Phase 1) |
| **Total** | **87** | |

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
| warning | 22 | Degradation: disconnects, stale connections, upload failures, reconnects, bandwidth exceeded, audio_encode restart, resource gating, tunnel retry |
| info | 42 | State changes: connections established, flows started, config updated, bandwidth recovery, encoder started, input/output CRUD, bind success (RTP/UDP), media-player started/playlist-exhausted |

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

Producers should use `EventSender::emit_with_details(EventSeverity::*,
category::REPLAY, message, flow_id, details)` (defined in
`src/manager/events.rs`) — `category::REPLAY` is the canonical
constant.
