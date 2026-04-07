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
- **SMPTE 302M bit packing matches ffmpeg.** `S302mPacketizer` and
  `S302mDepacketizer` follow ffmpeg's `libavcodec/s302menc.c` /
  `s302mdec.c` byte-for-byte; round trips are sample-exact at 16-bit
  and within ±1 LSB at 24-bit (verified by 11 unit tests in
  `engine::audio_302m::tests`).

For the operator-facing description, configuration syntax, presets,
and worked use cases, see the [Audio Gateway Guide](audio-gateway.md).

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
