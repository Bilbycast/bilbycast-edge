# bilbycast-edge Architecture

## System Context

```
                          ┌─────────────────────────────┐
                          │     bilbycast-manager        │
                          │  (centralized monitoring)    │
                          └──────────┬──────────────────┘
                                     │ WebSocket
                                     │ (registration, commands)
                                     │
  ┌──────────────┐          ┌────────▼─────────────────────────────────────────────┐
  │   Operators  │──REST──▶ │                  bilbycast-edge                      │
  │  (API/Web)   │◀─WS────│                                                       │
  └──────────────┘          │  ┌─────────────────────────────────────────────────┐  │
                            │  │              CONTROL PLANE                      │  │
                            │  │                                                 │  │
                            │  │  ┌──────────┐  ┌────────────┐  ┌────────────┐  │  │
                            │  │  │ REST API │  │  Auth/JWT  │  │  Config    │  │  │
                            │  │  │ (axum)   │──│  (OAuth2)  │  │ (JSON +   │  │  │
                            │  │  │          │  │  RBAC      │  │  secrets)  │  │  │
                            │  │  └────┬─────┘  └────────────┘  └─────┬──────┘  │  │
                            │  │       │                              │         │  │
                            │  └───────┼──────────────────────────────┼─────────┘  │
                            │          │                              │            │
                            │  ┌───────▼──────────────────────────────▼─────────┐  │
                            │  │              DATA PLANE                        │  │
                            │  │                                                │  │
                            │  │  ┌──────────────────────────────────────────┐  │  │
                            │  │  │           FlowManager (DashMap)          │  │  │
                            │  │  │                                          │  │  │
                            │  │  │   ┌─────────── Flow N ──────────────┐   │  │  │
                            │  │  │   │                                 │   │  │  │
                            │  │  │   │  ┌─────────┐   broadcast(2048) │   │  │  │
                            │  │  │   │  │  Input  │──────┬──────────┐ │   │  │  │
                            │  │  │   │  │  Task   │      │          │ │   │  │  │
                            │  │  │   │  └─────────┘      ▼          ▼ │   │  │  │
                            │  │  │   │                ┌────────┐┌────────┐│  │  │
                            │  │  │   │                │Output-1││Output-N││  │  │
                            │  │  │   │                │  Task  ││  Task  ││  │  │
                            │  │  │   │                └────────┘└────────┘│  │  │
                            │  │  │   │                                 │   │  │  │
                            │  │  │   │  CancellationToken (parent)    │   │  │  │
                            │  │  │   │  StatsAccumulator (AtomicU64)  │   │  │  │
                            │  │  │   │  TR-101290 Analyzer            │   │  │  │
                            │  │  │   │  Media Analyzer (toggleable)   │   │  │  │
                            │  │  │   └─────────────────────────────────┘   │  │  │
                            │  │  │                                          │  │  │
                            │  │  └──────────────────────────────────────────┘  │  │
                            │  │                                                │  │
                            │  │  ┌────────────────┐    ┌───────────────────┐   │  │
                            │  │  │ StatsCollector │    │  TunnelManager   │   │  │
                            │  │  │ (lock-free     │    │  (QUIC relay/    │   │  │
                            │  │  │  AtomicU64)    │    │   direct)        │   │  │
                            │  │  └────────────────┘    └───────────────────┘   │  │
                            │  │                                                │  │
                            │  └────────────────────────────────────────────────┘  │
                            │                                                      │
                            │  ┌────────────────────────────────────────────────┐  │
                            │  │              MONITOR PLANE                     │  │
                            │  │  ┌──────────┐  ┌──────────────┐  ┌─────────┐  │  │
                            │  │  │Dashboard │  │ WS Stats     │  │Promethe-│  │  │
                            │  │  │(embedded │  │ (1/sec       │  │us /metr-│  │  │
                            │  │  │ HTML/JS) │  │  broadcast)  │  │ics     │  │  │
                            │  │  └──────────┘  └──────────────┘  └─────────┘  │  │
                            │  └────────────────────────────────────────────────┘  │
                            └──────────────────────────────────────────────────────┘

  ┌─────────────┐                        │                      ┌─────────────┐
  │ SRT Sources │─── SRT (AES) ──────────┤                      │ SRT Dest    │
  │ RTP Sources │─── RTP/UDP ────────────┤     bilbycast-edge   ├── SRT ──────│
  │ RTMP (OBS)  │─── RTMP ──────────────►│     (data plane)     │── RTP/UDP ──│
  │ IP Cameras  │─── RTSP ─────────────►│                      │── RTMP(S) ──│
  │ WHIP (OBS)  │─── WebRTC ───────────►│                      │── HLS ──────│
  └─────────────┘                        │                      │── WebRTC ───│
                                         │                      └─────────────┘
```

## Data Plane: Packet Flow

Inputs and outputs are independent top-level config entities referenced by
flows via `input_ids` and `output_ids`. A flow may have multiple inputs but at
most one is active at a time. At startup or on create/update,
`AppConfig::resolve_flow()` dereferences these IDs into a `ResolvedFlow`
(containing `Vec<InputDefinition>` + `Vec<OutputConfig>`), which is what
the engine's `FlowRuntime` receives. The engine never sees raw ID references.

```
  ┌─────────────────────────────────────────────────────────────────────────────┐
  │                              FlowRuntime                                   │
  │                                                                            │
  │  INGRESS                      FAN-OUT                      EGRESS          │
  │                                                                            │
  │  ┌──────────────┐                                                          │
  │  │  RTP Input   │  ┌────────────────┐                                      │
  │  │  ┌─────────┐ │  │  RP 2129       │                                      │
  │  │  │ UDP Recv │─┼──▶  Ingress      │                                      │
  │  │  └─────────┘ │  │  Filters       │     ┌───────────────────┐            │
  │  │  ┌─────────┐ │  │  ┌───────────┐ │     │  broadcast::      │            │
  │  │  │ FEC     │◀┼──┤  │ C5: Src IP│ │     │  channel(2048)    │            │
  │  │  │ Decode  │ │  │  │ U4: PT    │ ├────▶│                   │            │
  │  │  │ (2022-1)│─┼──▶  │ C7: Rate  │ │     │  Sender ────┐    │            │
  │  │  └─────────┘ │  │  └───────────┘ │     │             │    │            │
  │  └──────────────┘  └────────────────┘     │             ▼    │            │
  │                                           │  ┌──────────────┐│  ┌────────┐│
  │  ┌──────────────┐                         │  │ subscribe()  ├┼─▶│RTP Out ││
  │  │  SRT Input   │                         │  └──────────────┘│  │+FEC Enc││
  │  │  ┌─────────┐ │  ┌─────────────────┐   │  ┌──────────────┐│  │+DSCP   ││
  │  │  │ Leg A   │─┼──▶  Hitless Merge  │   │  │ subscribe()  ├┼─▶└────────┘│
  │  │  ├─────────┤ │  │  (2022-7)       ├──▶│  └──────────────┘│  ┌────────┐│
  │  │  │ Leg B   │─┼──▶  Seq dedup      │   │  ┌──────────────┐│  │SRT Out ││
  │  │  └─────────┘ │  └─────────────────┘   │  │ subscribe()  ├┼─▶│+Redund.││
  │  │  AES decrypt │                         │  └──────────────┘│  └────────┘│
  │  │  Auto-reconnect                        │  ┌──────────────┐│  ┌────────┐│
  │  └──────────────┘                         │  │ subscribe()  ├┼─▶│RTMP Out││
  │                                           │  └──────────────┘│  └────────┘│
  │  ┌──────────────┐                         │  ┌──────────────┐│  ┌────────┐│
  │  │  RTMP Input  │                         │  │ subscribe()  ├┼─▶│HLS Out ││
  │  │  ┌─────────┐ │                         │  └──────────────┘│  └────────┘│
  │  │  │ FLV→TS  │─┼───────────────────────▶│  ┌──────────────┐│  ┌────────┐│
  │  │  │ Muxer   │ │                         │  │ subscribe()  ├┼─▶│WebRTC  ││
  │  │  └─────────┘ │                         │  └──────────────┘│  └────────┘│
  │  │  H.264+AAC   │                         │                   │            │
  │  └──────────────┘                         └───────────────────┘            │
  │                                                                            │
  │  ┌──────────────┐                                                          │
  │  │  RTSP Input  │─── retina client ── H.264+AAC ── TsMuxer ──────────────▶│
  │  │  (IP camera) │    auto-reconnect                                        │
  │  └──────────────┘                                                          │
  │                                                                            │
  │  ┌──────────────┐                                                          │
  │  │  TR-101290   │◀── subscribe() ── (independent quality analyzer)         │
  │  │  Analyzer    │                                                          │
  │  └──────────────┘                                                          │
  │  ┌──────────────┐                                                          │
  │  │  Media       │◀── subscribe() ── (codec/resolution/fps detection)       │
  │  │  Analyzer    │    toggleable per-flow via media_analysis config          │
  │  └──────────────┘                                                          │
  └─────────────────────────────────────────────────────────────────────────────┘
```

## Input Switching & TS Continuity

When a flow has multiple inputs (one active, others standby), all inputs run
simultaneously ("warm passive"). Switching the active input is a single
`watch::send()` — no task restart, no reconnect gap. The per-input forwarder
tasks gate packets onto the main broadcast channel based on the current active
input ID.

A shared **TS continuity fixer** (`engine/ts_continuity_fixer.rs`) sits in the
forwarder path to ensure external receivers (hardware decoders, ffplay, VLC,
broadcast IRDs) do not lose lock during a switch:

```
  Input A (active)  ──▶  per-input broadcast  ──▶  Forwarder A ──┐
                                                                   │
                                                   ┌───────────┐  │
                                                   │ TS Contin- │◀─┘  (shared, Arc<Mutex>)
                                                   │ uity Fixer │◀─┐
                                                   └─────┬─────┘  │
                                                         │        │
  Input B (passive) ──▶  per-input broadcast  ──▶  Forwarder B ──┘
                                                   (observe PSI
                                                    for cache)
                                                         │
                                                         ▼
                                                  broadcast::channel
                                                    (to outputs)
```

**What it does on switch:**

1. **CC state reset** — Output-side continuity counter tracking is cleared on
   switch. The new input's original CC values pass through, creating a natural
   CC jump on all PIDs. Receivers detect this as "packet loss," flush their PES
   buffers, and resync on the next PES start (PUSI=1). This is more reliable
   than trying to maintain CC continuity — it gives receivers a clean break
   signal that works regardless of codec or stream structure.

2. **Per-input PSI caching** — Each input has its own PAT/PMT cache (not
   shared). Passive inputs observe PSI tables while running warm. On switch,
   the *new* input's cached PAT and PMTs are immediately injected before the
   first data packet, so receivers can re-acquire the stream structure without
   waiting for the next natural PAT/PMT cycle (~200 ms).

3. **PSI version bump — monotonic, per-fixer.** Injected PAT/PMT packets have
   their `version_number` rewritten in place (with CRC32 recalculated) to a
   value drawn from a **per-fixer monotonic counter** advanced on every
   switch, not from `cached_version + 1`. This is the critical detail: every
   independently-generated stream (ffmpeg, srt-live-transmit, most camera
   SDKs) carries natural `version_number = 0`, so a naive `+1` produced two
   phantoms with identical `version = 1` for every input in a flow. After
   `A → B → A` the second phantom looked identical to the first and ffplay
   treated it as "already seen, don't re-parse" — keeping its audio decoder
   pointed at B's format and silently dropping every audio PES from the
   returning stream. The monotonic counter guarantees consecutive switches
   always produce a strictly-different version stamp, forcing re-parse every
   time. It also advances unconditionally on a switch to a dead input (one
   with no cached PSI and no phantom emitted), so the next real switch still
   gets a fresh stamp. Wraps at 32; each consecutive switch is still
   different from the previous.

4. **Immediate forwarding** — All packets (video, audio, data) are forwarded
   immediately after a switch. The CC jump signals receivers to flush and
   resync on the next PES start. **Fully format-agnostic**: inputs can use
   any codec, container, or transport — H.264, H.265/HEVC, JPEG XS,
   JPEG 2000, uncompressed video, SMPTE ST 2110-30/-31/-40, AAC, HE-AAC,
   Opus, MP2, AC-3, LPCM, SMPTE 302M, or any future format. Inputs do not
   need to share the same codec, resolution, frame rate, sample rate, channel
   count, or stream structure. For non-TS transports (e.g., raw ST 2110 RTP),
   the fixer is transparent — the switch mechanism (watch channel) works
   identically.

5. **Force IDR on the newly-active ingress re-encoder** — When the incoming
   input has an `video_encode` stage (ingress transcoding), the forwarder
   asks its encoder to emit an IDR on the first post-switch frame via a
   one-shot `AVFrame.pict_type = AV_PICTURE_TYPE_I` hint honoured by
   libx264 / libx265 / NVENC. Without this, downstream decoders would have
   to wait up to a full GOP (default 2 s at 30 fps) for the next natural
   keyframe before they could display anything. Passthrough inputs (no
   `video_encode`) have no encoder to signal and are unaffected — their
   IDR cadence is whatever the upstream source chose.

6. **NULL-PID keepalive on the active forwarder.** When the active input
   has no packets to forward for 250 ms (typical when the operator has
   switched to an input whose source isn't currently transmitting — an
   RTP bind with nothing on the wire, an SRT caller to an unreachable
   host), the forwarder emits a single 1316-byte UDP datagram of seven
   NULL-PID (0x1FFF) TS packets. Sized to exactly one
   `TS_PACKETS_PER_DATAGRAM` batch so the UDP output's 7-packet aligner
   flushes it immediately rather than letting it sit in the buffer.
   Receivers are required by spec to drop NULL packets; the keepalive
   exists purely to keep sockets and decoder state alive during dead-
   input periods. Without it, a 3 s+ silence on the output triggers
   time-out / EOF behaviour in several downstream receivers that cannot
   be recovered by later data resuming.

With mechanisms 1–6 combined, the visible switch latency at the receiver is
one to two frames regardless of whether the target input is passthrough or
ingress-transcoded. The pre-fix behaviour (switching *to* an ingress-
transcoded input used to stall for up to one full GOP) is gone.

**Zero-cost when not switching:** Before the first switch occurs, the fixer
passes packets through unchanged — a single `bool` check per packet, no
allocations, no copies. CC state is tracked passively so it is accurate if a
switch occurs later.

**Downstream safety:** The fixer operates strictly at the TS packet level
(188-byte boundaries). RTP headers are preserved unchanged. No output, FEC
encoder, delay buffer, program filter, or analyzer is affected — they all
treat TS payloads as opaque bytes or maintain independent state.

## Concurrency & Shutdown Model

```
  main() shutdown signal (Ctrl+C)
  │
  ├─▶ FlowManager.stop_all()
  │     │
  │     ├─▶ Flow-1 cancel_token.cancel()
  │     │     ├─▶ input_task (child token) ──▶ exits select! loop
  │     │     ├─▶ tr101290_task (child)    ──▶ exits select! loop
  │     │     ├─▶ media_analysis (child)  ──▶ exits select! loop (if enabled)
  │     │     ├─▶ output-A (child token)   ──▶ exits select! loop
  │     │     └─▶ output-B (child token)   ──▶ exits select! loop
  │     │
  │     └─▶ Flow-N cancel_token.cancel()
  │           └─▶ (same hierarchy)
  │
  ├─▶ TunnelManager.stop_all()
  │
  └─▶ API server graceful shutdown

  Hot-add/remove (runtime, no restart):
  ├─ add_output()    ──▶ new child token + subscribe to broadcast
  └─ remove_output() ──▶ cancel child token only, others unaffected
```

## Security Layers

```
  External Request
  │
  ▼
  ┌────────────────────────────────────────┐
  │  Layer 1: TLS (default on)              │
  │  rustls + ring crypto                  │
  └────────────┬───────────────────────────┘
               ▼
  ┌────────────────────────────────────────┐
  │  Layer 2: OAuth 2.0 + JWT (HS256)     │
  │  /oauth/token → client_credentials    │
  │  Bearer token → HMAC-SHA256 verify    │
  │  Role-based: admin | monitor          │
  └────────────┬───────────────────────────┘
               ▼
  ┌────────────────────────────────────────┐
  │  Layer 3: Route-level RBAC            │
  │  Public:    /health, /oauth/token,   │
  │             /setup (gated by config) │
  │  Read-only: GET /api/v1/* (any role)  │
  │  Admin:     POST/PUT/DELETE (admin)   │
  └────────────┬───────────────────────────┘
               ▼
  ┌────────────────────────────────────────┐
  │  Layer 4: Data plane ingress filters  │
  │  (RP 2129 / SMPTE trust boundaries)  │
  │  C5: Source IP allow-list (HashSet)   │
  │  U4: Payload type filter             │
  │  C7: Rate limiter (token bucket)     │
  └────────────────────────────────────────┘

  Tunnel Security:
  ┌────────────────────────────────────────┐
  │  QUIC + TLS 1.3 (quinn/rustls)        │
  │  E2E: ChaCha20-Poly1305 (AEAD)       │
  │  32-byte shared key per tunnel        │
  │  Manager generates + distributes keys │
  │  Relay is stateless (no auth/ACL)     │
  │  28 bytes overhead (12 nonce+16 tag)  │
  │  Per-tunnel PSK (direct mode)         │
  └────────────────────────────────────────┘

  SRT Security:
  ┌────────────────────────────────────────┐
  │  AES-128/192/256 encryption           │
  │  Passphrase auth (10-79 chars)        │
  └────────────────────────────────────────┘
```

## Module Dependency Graph

```
                    ┌──────────┐
                    │  main.rs │
                    └────┬─────┘
           ┌─────────┬──┴──┬─────────┬──────────┐
           ▼         ▼     ▼         ▼          ▼
       ┌───────┐ ┌──────┐┌──────┐┌────────┐┌────────┐
       │  api  │ │engine││config││ tunnel │ │monitor │ │setup │
       └──┬────┘ └──┬───┘└──────┘└───┬────┘ └────────┘ └──────┘
          │         │                │
          ├────────▶│◀───────────────┘
          │         │
          │    ┌────┼────────┐
          │    ▼    ▼        ▼
          │ ┌─────┐┌───┐┌──────────┐
          │ │stats││fec││redundancy│
          │ └─────┘└───┘└──────────┘
          │    ▲
          └────┘
                ┌────┐  ┌─────┐
                │util│  │ srt │
                └────┘  └─────┘
                   ▲       ▲
                   └───┬───┘
                       │
                    (engine, tunnel)
```

## Audio gateway pipeline (`engine::audio_transcode`, `engine::audio_302m`)

Two engine modules implement Bilbycast's audio gateway feature set,
inserted between the broadcast channel and the per-output send loop:

```
broadcast::channel<RtpPacket> (per flow)
        │
        │  per-output subscribe
        ▼
┌─────────────────────────────────────────────────────────┐
│ engine::audio_transcode::TranscodeStage (per output)    │
│   PcmDepacketizer → decode_pcm_be (BE PCM → planar f32) │
│     → apply_channel_matrix (IS-08 watch::Receiver       │
│       sampled per packet, single atomic load)           │
│     → rubato SRC (SincFixedIn or FastFixedIn)           │
│     → encode_pcm_be (planar f32 → BE PCM with TPDF)     │
│     → PcmPacketizer (RTP framing at target packet time) │
│   stats: TranscodeStats { input_packets, output_packets │
│          dropped, format_resets, last_latency_us }      │
└────────┬────────────────────────────────────────────────┘
         │
         ▼
  ┌─────────────────────────┐
  │ output backend          │
  │  • st2110_30 / -31 RTP  │  → byte-identical RTP send (Red + Blue)
  │  • rtp_audio RTP        │
  │  • engine::audio_302m   │  → S302mPacketizer (302M bit packing)
  │       S302mOutputPipe   │     → TsMuxer (BSSD reg descriptor in PMT)
  │                         │       → 7×188 byte chunk bundling
  │                         │         → SRT / UDP / RTP-MP2T (PT 33)
  └─────────────────────────┘
```

Key invariants:

- **Lock-free hot path.** No `Mutex` is taken on per-packet processing
  inside `TranscodeStage` or `S302mOutputPipeline`. All counters are
  `AtomicU64`. The IS-08 `watch::Receiver::has_changed()` check is a
  single atomic load; the per-output channel matrix is cached and only
  recomputed when an IS-08 activation arrives (typically operator
  action, never per packet).
- **No back-pressure on the input task.** When the transcoder can't
  keep up, it drops packets via `TranscodeStats::dropped` rather than
  signaling backpressure to upstream. Same rule as every other output
  in the codebase.
- **Passthrough is free.** When a flow's outputs don't set a `transcode`
  block, no `TranscodeStage` is constructed and the existing
  byte-identical passthrough path runs unchanged. The stage is purely
  opt-in per output.
- **Compressed-audio outputs share the same transcoder.** On RTMP, HLS,
  WebRTC, and the TS-carrying SRT / UDP / RTP / RIST outputs, a slim
  planar-f32-only variant — `engine::audio_transcode::PlanarAudioTranscoder`
  — sits between the AAC decoder and the target encoder inside the
  `audio_encode` pipeline (see `ts_audio_replace.rs`, `output_rtmp.rs`,
  `output_hls.rs`, `output_webrtc.rs`). It reuses `ChannelMatrix`,
  `apply_channel_matrix`, `expand_preset`, and rubato SRC, but skips the
  RTP PCM wire encoding (encoders accept f32 directly) and skips
  bit-depth / dither (irrelevant for compressed codecs). Same fast-paths
  — empty block is a full passthrough, identity matrix skips the
  mul, rate match skips rubato.
- **SMPTE 302M bit packing matches ffmpeg.** `S302mPacketizer` and
  `S302mDepacketizer` follow ffmpeg's `libavcodec/s302menc.c` /
  `s302mdec.c` byte-for-byte; round trips are sample-exact at 16-bit
  and within ±1 LSB at 24-bit (verified by 11 unit tests in
  `engine::audio_302m::tests`).

For the operator-facing description, configuration syntax, presets,
and worked use cases, see the [Audio Gateway Guide](audio-gateway.md).

## Configuration Model: Independent Inputs/Outputs

Config version 2 separates inputs, outputs, and flows into three independent
top-level arrays. Inputs and outputs are standalone entities with stable IDs;
flows connect one or more inputs (one active at a time) to N outputs by reference (`input_ids` + `output_ids`).

```
  config.json (version 2)
  ┌──────────────────────────────────────────────���───────┐
  │  "inputs": [                                         │
  │    { "id": "srt-in", "type": "srt", ... }            │
  │    { "id": "rtp-in", "type": "rtp", ... }            │
  │  ]                                                   │
  │                                                      │
  │  "outputs": [                                        │
  │    { "id": "rtp-out", "type": "rtp", ... }           │
  │    { "id": "srt-out", "type": "srt", ... }           │
  │    { "id": "rtmp-out", "type": "rtmp", ... }         │
  │  ]                                                   │
  │                                                      │
  │  "flows": [                                          │
  │    { "id": "flow-1",                                 │
  │      "input_ids": ["srt-in"],      ◄── reference      │
  │      "output_ids": ["rtp-out", "rtmp-out"] }         │
  │  ]                                                   │
  └──────────────────────────────────────────────────────┘
```

**Resolution boundary:** `AppConfig::resolve_flow()` dereferences the IDs
into a `ResolvedFlow` containing the full `InputDefinition` and
`Vec<OutputConfig>`. The engine only ever sees `ResolvedFlow` — the
reference-resolution boundary lives in the API/config layer, not the engine.

**Assignment constraints:**
- An input can only be assigned to one flow at a time.
- An output can only be assigned to one flow at a time.
- Unassigned inputs/outputs are configured but not running.

**REST API surface:**
- `GET/POST /api/v1/inputs`, `GET/PUT/DELETE /api/v1/inputs/{id}`
- `GET/POST /api/v1/outputs`, `GET/PUT/DELETE /api/v1/outputs/{id}`
- Flows reference inputs/outputs by ID; `add_output` and `remove_output`
  assign/unassign existing outputs rather than creating inline definitions.

## Adding New Input/Output Types

Current pattern requires changes in these locations:

| Step | File | Change |
|------|------|--------|
| 1 | `src/config/models.rs` | Add variant to `InputConfig` or `OutputConfig` enum |
| 2 | `src/config/validation.rs` | Add validation rules for the new variant |
| 3 | `src/config/secrets.rs` | Add secret fields to `InputSecrets`/`OutputSecrets`, update `extract_from`/`merge_into`/`strip_secrets`/`has_secrets` |
| 4 | `src/engine/input_xxx.rs` or `output_xxx.rs` | Create the new task module |
| 5 | `src/engine/mod.rs` | Declare `pub mod` |
| 6 | `src/engine/flow.rs` | Add `match` arm in `start()` or `start_output()` |
| 7 | `src/engine/flow.rs` | Add config metadata extraction |

The spawn function signature convention:
```rust
pub fn spawn_xxx_output(
    config: XxxOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()>
```

## PID bus (Flow Assembly — SPTS / MPTS synthesis)

When a flow's `FlowConfig.assembly` selects `spts` or `mpts`, the flow no longer forwards the active input's bytes. Instead, a parallel ES-level data plane builds a fresh MPEG-TS from elementary streams pulled off **any of the flow's inputs** and publishes it onto the same broadcast channel every existing output subscribes to — so UDP, RTP (with / without 2022-1 FEC, 2022-7), SRT (incl. bonded / 2022-7), RIST (incl. ARQ), RTMP, HLS, CMAF, WebRTC all consume the assembled TS unchanged.

```
  Input A (TS)                                  FlowEsBus (keyed by (input_id, source_pid))
    ├── forwarder → broadcast_tx                 ┌──────────────────────────────────────────┐
    └── TsEsDemuxer ──────────publishes──────────►  (A, 0x100) → broadcast<EsPacket>          │
                                                │  (A, 0x101) → broadcast<EsPacket>          │
                                                │  ...                                       │
  Input B (TS)                                  │                                            │
    ├── forwarder → broadcast_tx ─► SUPPRESSED  │                                            │
    └── TsEsDemuxer ──────────publishes──────────►  (B, 0x100) → broadcast<EsPacket>          │
                                                │  (B, 0x101) → broadcast<EsPacket>          │
                                                └──────────────────────────────────────────┘
  Input C (ST 2110-30 + audio_encode=aac_lc)                │
    └── input_pcm_encode (decoded-ES cache) ────────────────┘
                                                            │
                                                            ▼
                                          ┌──────────────────────────────────┐
                                          │ ts_assembler (spawn_spts_assembler)
                                          │ ─────────────────────────────────
                                          │ fan-in per slot via broadcast::Receiver
                                          │ rewrite PID → out_pid (+ CC stamp)
                                          │ bundle 7 × 188 B = 1316 B RTP packet
                                          │ synthesise PAT + PMT(s) @ 100 ms
                                          │ per-program PCR byte-for-byte
                                          │ PAT/PMT version_number bumps (mod 32)
                                          └──────────────────────────────────┘
                                                            │
                                                            ▼
                                                 broadcast::Sender<RtpPacket>
                                                            │
                            ┌───────────────────────────────┼───────────────────────────────┐
                            ▼                               ▼                               ▼
                         UDP output                      SRT output                      RTMP output
                      (passthrough TS)                 (passthrough TS)            (re-mux via TsDemuxer)
```

The runtime never forwards the inputs' original TS bytes directly onto `broadcast_tx` when the flow is assembled — it publishes each input's ES onto the bus and lets the assembler drive the flow broadcast channel.

### Per-ES bus primitives (`engine/ts_es_bus.rs`)

- **`EsPacket`** — one 188-byte TS packet (source bytes untouched) + `source_pid`, PMT `stream_type`, PUSI flag, `has_pcr`, extracted 27 MHz PCR value (when present), and the upstream `recv_time_us`. The source CC stays in-band for the assembler to rewrite.
- **`FlowEsBus`** — per-flow `DashMap<(input_id, source_pid), broadcast::Sender<EsPacket>>`. Lazily creates the channel on first observation of a PID; channel capacity is 2048 TS packets per PID. Slow consumers see `RecvError::Lagged(n)` and drop — no cascade backpressure. PAT / PMT / NULL-PID packets are not published.
- **`TsEsDemuxer`** — the per-input bridge. Parses the input's TS, maintains a per-input PSI catalogue via `ts_psi_catalog` (Phase 2), and publishes every ES packet onto its bus key. One demuxer per input; active whenever the flow is assembled.

### Decoded-ES cache for non-TS audio (`engine/input_pcm_encode.rs`)

PCM / AES3-transparent inputs (ST 2110-30, ST 2110-31, `rtp_audio`) become TS carriers when the operator sets `audio_encode` on the input. The input task runs AAC-LC / HE-AAC v1/v2 (fdk-aac) or SMPTE 302M wrapping over the PCM samples, wraps the encoded audio into a synthetic audio-only TS via the shared `TsMuxer`, and publishes onto the bus like any native-TS input. First-light runtime codecs: `aac_lc`, `he_aac_v1`, `he_aac_v2`, `s302m`. ST 2110-31 is valid only with `s302m` — its 337M sub-frames ride bit-for-bit.

### Pre-bus Hitless merger (`engine/ts_es_hitless.rs`)

A `SlotSource::Hitless { primary, backup }` spawns a merger task that subscribes to both legs on the bus and republishes onto a synthetic `hitless:<uid>` bus key the assembler's slot then points at. Strategy: **primary-preference with a 200 ms stall timer** (not sequence-aware 2022-7 dedup — `EsPacket` carries no upstream RTP sequence number today). Primary packets forward verbatim; no primary for 200 ms → flip to backup; primary traffic resumes → switch back after a short hold-off. Either leg must itself be `Pid` or `Essence`; nested Hitless is rejected at config-save time.

### Assembler (`engine/ts_assembler.rs`)

- **Subscribe**: one fan-in task per slot, each draining a `broadcast::Receiver<EsPacket>` and forwarding into a single `mpsc::Sender<(slot_idx, EsPacket)>`. Slow fan-in loses packets at the bus edge, not in mpsc backpressure — keeps the no-cascade invariant.
- **Egress**: one `select!` loop rewrites each 188-byte packet's PID to the configured `out_pid`, stamps a per-out-PID monotonic CC, and batches seven packets into a 1316-byte RTP bundle that gets published to `broadcast_tx`. One `BytesMut::with_capacity(1316)` allocation per bundle; zero per-TS-packet allocations.
- **PSI synthesis**: PAT + one PMT per program on a 100 ms cadence, with `mpeg2_crc32` for the CRC32 trailer. A 10 ms safety-net flush keeps partially-filled bundles shipping during sparse audio-only / keyframe-gap periods.
- **Versioning**: `PAT.version_number` bumps (mod 32, monotonic) when the program set changes; each program's `PMT.version_number` bumps when its slot composition or `pcr_source` changes. Same monotonic-counter pattern as `TsContinuityFixer` — avoids the phantom-version collision the passthrough switcher already had to solve.
- **Hot-swap**: `PlanCommand::ReplacePlan { plan }` via an mpsc channel. The handler diffs old vs new, re-spawns fan-ins for added / changed slots, cancels fan-ins for removed slots, bumps PMT versions accordingly, and emits a fresh PSI burst immediately so receivers never see ES bytes on an unknown PID.

### Per-ES analyser (`engine/ts_es_analysis.rs`)

One lightweight task per bus key maintains a `PerEsAccumulator` on `FlowStatsAccumulator.per_es_stats`. Tracks packets, bytes, rolling 1 Hz bitrate, CC errors, PCR discontinuities (100 ms threshold matching flow-level TR-101290), and last-seen `stream_type`. Snapshot path annotates each entry with its current `out_pid` from `FlowStatsAccumulator.pid_routing` (refreshed on every plan change) so operators can pivot off the egress PID. Shipped on `FlowStats.per_es[]`.

### Output-side PCR accuracy sampler (`stats/pcr_trust.rs`)

Fixed-size rotating reservoir (4096 samples) on every TS-bearing output's send path. On each successful `socket.send_to` of a datagram containing a PCR-bearing TS packet, the sampler records `|ΔPCR_µs − Δwall_µs|` — then exposes exact percentiles (p50 / p95 / p99 / max) on snapshot. Sample-skip rule: Δ > 500 ms on either side discards and resets state (filters startup jitter, keyframe gaps, restarts, 33-bit wrap). Wired from `engine/output_udp.rs` (MPTS UDP) and `engine/output_rtp.rs` (raw-TS + RTP-wrapped TS). Flow-level rollup on `FlowStats.pcr_trust_flow` aggregates across outputs.

### Runtime plumbing (`engine/flow.rs`)

- `FlowRuntime::start` builds the initial `AssemblyPlan` via `build_assembly_plan()` — enforces input-side TS-eligibility, resolves `Essence` slots against each input's PSI catalogue, wires Hitless mergers, cross-checks PCR sources against program slots, and fails the flow bring-up with a specific `pid_bus_*` error code (shipped as a Critical event with structured `details`) when anything is unresolvable.
- `FlowRuntime::replace_assembly` handles the `UpdateFlowAssembly` WS command (and the `PUT /api/v1/flows/{id}/assembly` REST mirror). Re-runs the validator + essence resolver on the incoming plan, re-spawns Hitless mergers for any new synthetic keys, sends `PlanCommand::ReplacePlan` to the assembler, and persists the new assembly to `config.json` only after the swap succeeds. Transitions across the passthrough boundary (passthrough ↔ assembled) are rejected — those need a full `UpdateFlow` because the plumbing on the flow changes (bus + assembler spawn vs. direct broadcast).

**Production-safety invariant.** Flows without an `assembly` block — or with `assembly.kind = passthrough` — run exactly as before. Assembled flows fail loudly with a Critical `pid_bus_*` event on any unresolvable state; no silent misbehaviour.

## MPTS → SPTS program filtering

When an input carries an MPTS (Multi-Program Transport Stream) and an output wants only a single program, a per-output **program filter** runs between the broadcast receiver and the output's send path. Two implementations share the heavy lifting:

```
  MPTS input ──▶ broadcast channel ──▶ subscribe()
                                            │
                                            ▼
                                  ┌──────────────────┐
                                  │ program_filter   │    (only when program_number is set)
                                  │ ───────────────  │
                                  │ TS-native output │    TsProgramFilter: rewrites PAT,
                                  │ (UDP/RTP/SRT/HLS)│    drops non-target PMT/ES/PCR PIDs,
                                  │                  │    preserves RTP header on wrapped
                                  │                  │    packets. Output becomes SPTS.
                                  │                  │
                                  │ Re-muxing output │    TsDemuxer(target_program):
                                  │ (RTMP/WebRTC)    │    picks video/audio PIDs from the
                                  │                  │    selected program's PMT only.
                                  │                  │    Default = lowest program_number.
                                  └──────────────────┘
                                            │
                                            ▼
                                  protocol-specific send
```

- **TS-native path** (`engine/ts_program_filter.rs`): `TsProgramFilter` consumes raw 188-byte TS packets and emits only those belonging to the selected program. It reads the PAT to find the target's PMT PID, parses that PMT to derive the allowed-PID set (PMT + each ES PID + PCR PID), generates a fresh single-program PAT (using the existing `mpeg2_crc32` for the CRC32 trailer), and drops everything else. State persists across calls so PAT version bumps and programs coming/going mid-stream are handled.
- **Re-muxing path** (`engine/ts_demux.rs`): `TsDemuxer::new(target_program: Option<u16>)` honours the same selector. When `None`, it reads the PAT, sorts programs by `program_number`, and locks onto the lowest — a deterministic default that replaces the previous "first PMT seen" race. When `Some(N)`, non-target PMTs are ignored entirely.
- **Thumbnail generator** (`engine/thumbnail.rs`): accepts an optional `thumbnail_program_number` on `FlowConfig`. When set, the buffered TS goes through `TsProgramFilter` before being piped to ffmpeg so the manager UI preview matches the chosen program.

FEC (2022-1) and hitless redundancy (2022-7) run **after** the filter, so the receiver's recovery layer protects the filtered SPTS bytes — not the original MPTS.

`program_number` is per-output, so one flow can fan an MPTS out to multiple outputs, each locked onto a different program, alongside a sibling output that still passes the full MPTS unchanged.

## Backpressure & QoS

```
  Input ──▶ broadcast::channel(2048) ──▶ Output subscribers

  Slow output?
  ├─ recv() returns RecvError::Lagged(n)
  ├─ Output increments packets_dropped (AtomicU64)
  ├─ Input is NEVER blocked (other outputs unaffected)
  └─ No cascading backpressure

  SRT output inner buffer:
  ├─ mpsc::channel(256) between broadcast task and SRT send
  ├─ try_send() (non-blocking) — drops if full
  └─ Separate from broadcast backpressure

  RTP output:
  └─ Direct send from broadcast receiver (no intermediate buffer)
```
