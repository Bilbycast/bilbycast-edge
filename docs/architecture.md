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
