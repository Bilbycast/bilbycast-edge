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
| info | Output '{id}' added to flow '{flow_id}' | Hot-add output succeeded |
| info | Output '{id}' removed from flow '{flow_id}' | Hot-remove output succeeded |
| warning | Output '{id}' failed to start on flow '{flow_id}': {error} | Output startup failure within a running flow |

**Source**: `src/engine/manager.rs`, `src/engine/input_srt.rs`

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

| Severity | Message | Trigger |
|----------|---------|---------|
| info | SRT input connected (mode=listener) | Listener accepted a caller connection |
| info | SRT input connected (mode={mode}) | Caller connected to remote |
| info | SRT input connected (mode=listener, redundant leg 1) | Redundant leg 1 accepted |
| warning | SRT input disconnected, reconnecting | Peer disconnected, reconnection in progress |
| critical | SRT input connection failed: {error} | Listener accept or caller connect failed |

#### SRT Output

| Severity | Message | Trigger |
|----------|---------|---------|
| info | SRT output '{id}' connected | Peer connected (listener) or caller connected |
| warning | SRT output '{id}' disconnected | Peer disconnected or connection lost |
| warning | SRT output '{id}' stale connection detected | No ACK received after timeout, re-accepting |
| critical | SRT output '{id}' connection failed: {error} | Caller can't reach remote, or bind fails |

**Source**: `src/engine/input_srt.rs`, `src/engine/output_srt.rs`

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

| Severity | Message | Trigger |
|----------|---------|---------|
| info | RTMP publisher connected | Client connected and started publishing |
| warning | RTMP publisher disconnected | Publisher disconnected |
| critical | RTMP server error: {error} | Server bind or accept failure |

**Source**: `src/engine/input_rtmp.rs`

---

### RTSP (`rtsp`)

| Severity | Message | Trigger |
|----------|---------|---------|
| info | RTSP connected to {url} | RTSP DESCRIBE/SETUP/PLAY succeeded |
| warning | RTSP input disconnected: {error}. Reconnecting in {n}s | Stream lost after a successful connection |

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

**Source**: `src/engine/audio_encode.rs`, `src/engine/output_rtmp.rs`,
`src/engine/output_hls.rs`, `src/engine/output_webrtc.rs`.

---

### Tunnel (`tunnel`)

| Severity | Message | Trigger |
|----------|---------|---------|
| info | Tunnel '{name}' started | Tunnel created and connecting |
| info | Tunnel '{name}' stopped | Tunnel stopped by command or config change |
| info | Tunnel connected to relay | QUIC connection established and TunnelReady received |
| warning | Tunnel disconnected from relay: {reason} | QUIC connection lost or forwarder exited |
| warning | Tunnel peer disconnected: {reason} | Relay reported peer unbound (TunnelDown) |
| warning | Tunnel connection to relay failed: {error} | QUIC connect or TLS error |
| critical | Tunnel '{name}' failed: {error} | Tunnel task exited with fatal error |
| critical | Tunnel bind rejected by relay: {reason} | HMAC bind token verification failed |

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
| `flow` | 7 | Flow lifecycle (start/stop/fail, output add/remove) |
| `bandwidth` | 4 | Per-flow bandwidth monitoring (alarm, block, recovery) |
| `srt` | 9 | SRT input and output connection state |
| `redundancy` | 3 | SMPTE 2022-7 dual-leg status |
| `rtmp` | 3 | RTMP publisher connections |
| `rtsp` | 2 | RTSP input state |
| `hls` | 2 | HLS output failures |
| `webrtc` | 8 | WHIP/WHEP session lifecycle |
| `audio_encode` | 7 | ffmpeg-sidecar audio encoder lifecycle (Phase B) |
| `tunnel` | 8 | Tunnel connection state |
| `manager` | 3 | Manager WebSocket connection |
| `config` | 2 | Configuration changes |
| `ptp` | — | SMPTE ST 2110 PTP slave clock state changes (Phase 1) |
| `network_leg` | — | SMPTE 2022-7 Red/Blue per-leg loss / recovery (Phase 1) |
| `nmos` | — | NMOS IS-04 / IS-05 / IS-08 controller activity (Phase 1) |
| `scte104` | — | SCTE-104 splice events parsed from ST 2110-40 ANC (Phase 1) |
| **Total** | **58** | |

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
| critical | 16 | Service-impacting: flow/tunnel failures, auth rejection, both legs lost, bandwidth block, audio_encode build/restart-cap failures |
| warning | 20 | Degradation: disconnects, stale connections, upload failures, reconnects, bandwidth exceeded, audio_encode restart / per-segment HLS remux failure |
| info | 23 | State changes: connections established, flows started, config updated, bandwidth recovery, audio_encode started |
