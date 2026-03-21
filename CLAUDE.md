# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

bilbycast-edge is a Rust media transport gateway for professional broadcast workflows. It bridges multiple protocols (SRT, RTP/UDP, RTMP, HLS, WebRTC) with SMPTE 2022-1 FEC and SMPTE 2022-7 hitless redundancy. Designed for low-latency, uninterrupted media flow with broadcast-grade QoS.

## Build Commands

```bash
cargo check                    # Quick type/borrow check
cargo build                    # Debug build
cargo build --release          # Optimized release build
cargo test                     # Run all tests
cargo test <test_name>         # Run a single test
cargo build --features tls     # Enable HTTPS/RTMPS support
```

## Running

```bash
./target/release/bilbycast-edge --config config.json
./target/release/bilbycast-edge --config config.json --port 8080 --log-level debug
```

Example configs in `config_examples/`. Interop testing uses `test-edge1.json` through `test-edge4.json` (see `docs/INTEROP_TEST.md`).

## External Crate Dependencies

SRT support depends on sibling crates outside this repo:
- `srt-transport` at `../bilbycast-srt/srt-transport`
- `srt-protocol` at `../bilbycast-srt/srt-protocol`

These must be present for the project to compile.

## Feature Flags

- `tls` — HTTPS and RTMPS support (tokio-rustls + axum-server)
- `webrtc` — WebRTC/WHIP output (currently commented out in Cargo.toml)

## Architecture Overview

Single-crate Rust project on Tokio async runtime. See `docs/architecture.md` for full diagrams.

The system is split into three planes:

| Plane | Modules | Purpose |
|-------|---------|---------|
| **Control** | `api/`, `config/`, `manager/` | REST API, auth, config, remote commands |
| **Data** | `engine/`, `fec/`, `redundancy/`, `srt/`, `tunnel/` | Packet processing, protocol I/O, QoS features |
| **Monitor** | `stats/`, `monitor/`, `api/ws.rs` | Lock-free metrics, dashboard, Prometheus |

### Core Data Flow Model

Each **flow** has exactly one input and N outputs connected via a `tokio::broadcast::channel(2048)`. The `FlowManager` (`src/engine/manager.rs`) holds all active flows in a `DashMap<FlowId, Arc<FlowRuntime>>`.

```
FlowManager (DashMap)
  └─► FlowRuntime
        ├─ Input task (RTP/SRT/RTMP receiver)
        │    ├─ Optional: FEC decode (2022-1), ingress filters (RP 2129)
        │    ├─ Optional: Hitless merge (2022-7, SRT dual-leg)
        │    └─ Publishes RtpPacket into broadcast channel
        │
        ├─ broadcast::Sender ──► Output-1 (subscriber)
        │                      ├─► Output-2
        │                      └─► Output-N
        │
        ├─ TR-101290 Analyzer (independent subscriber)
        ├─ CancellationToken (hierarchical: parent → children)
        └─ StatsAccumulator (AtomicU64 counters, lock-free)
```

**Backpressure rule**: Slow outputs receive `RecvError::Lagged(n)` and increment `packets_dropped`. Input is **never** blocked. No cascading backpressure across outputs.

### The RtpPacket Abstraction (`src/engine/packet.rs`)

All data flows through a single type: `RtpPacket { data: Bytes, sequence_number: u16, rtp_timestamp: u32, recv_time_us: u64, is_raw_ts: bool }`. Inputs produce these, outputs consume them. The `is_raw_ts` flag distinguishes RTP-wrapped TS from raw TS (e.g., from OBS/srt-live-transmit).

### Module Responsibilities

| Module | Key Files | Purpose |
|--------|-----------|---------|
| `engine/` | `manager.rs`, `flow.rs`, `packet.rs` | Flow lifecycle, FlowRuntime bring-up/teardown |
| `engine/` | `input_rtp.rs`, `input_srt.rs`, `input_rtmp.rs` | Protocol-specific input tasks |
| `engine/` | `output_rtp.rs`, `output_srt.rs`, `output_rtmp.rs`, `output_hls.rs`, `output_webrtc.rs` | Protocol-specific output tasks |
| `engine/rtmp/` | `server.rs`, `amf0.rs`, `chunk.rs`, `ts_mux.rs` | RTMP protocol internals, FLV→MPEG-TS |
| `engine/` | `tr101290.rs` | Transport stream quality analysis (sync, CC, PAT/PMT, PCR) |
| `fec/` | `encoder.rs`, `decoder.rs`, `matrix.rs` | SMPTE 2022-1 FEC (XOR column×row) |
| `redundancy/` | `merger.rs` | SMPTE 2022-7 hitless merge (seq dedup from dual SRT legs) |
| `api/` | `server.rs`, `auth.rs`, `flows.rs`, `stats.rs`, `tunnels.rs`, `ws.rs` | Axum REST API, OAuth2/JWT, WebSocket stats |
| `config/` | `models.rs`, `validation.rs`, `persistence.rs` | JSON config, enum-tagged types, atomic save |
| `stats/` | `collector.rs`, `models.rs`, `throughput.rs` | Lock-free stats registry, bitrate estimation |
| `tunnel/` | `manager.rs`, `relay_client.rs`, `udp_forwarder.rs`, `tcp_forwarder.rs` | QUIC-based IP tunnels (relay/direct) |
| `manager/` | `client.rs`, `config.rs` | WebSocket client to bilbycast-manager. Sends stats (1s) and health (15s). Handles commands: get_config (returns full AppConfig), create/delete/start/stop flow, add/remove output, create/delete tunnel |
| `monitor/` | `server.rs`, `dashboard.rs` | Embedded HTML/JS dashboard on separate port |
| `srt/` | `connection.rs` | SRT stats polling and socket config |
| `util/` | `rtp_parse.rs`, `socket.rs`, `time.rs` | RTP header parsing, UDP/multicast, monotonic clock |

### Concurrency Model

- **`DashMap`** — lock-free concurrent registries (FlowManager, StatsCollector, TunnelManager)
- **`AtomicU64`** (Relaxed ordering) — hot-path stats counters, never block packet flow
- **`broadcast::channel(2048)`** — fan-out from input to outputs, bounded, non-blocking
- **`CancellationToken`** — hierarchical shutdown: parent flow token → child output tokens
- **Hot-add/remove** — outputs can be added/removed at runtime without restarting the flow

### Security Architecture

Four security layers, from outermost to innermost:

1. **TLS** (optional `tls` feature): rustls + ring for HTTPS/RTMPS
2. **OAuth 2.0 + JWT**: client credentials grant, HS256 signing, role-based (admin vs monitor)
3. **Route-level RBAC**: public routes (`/health`, `/oauth/token`), read-only (any JWT), admin-only (mutations)
4. **Data plane ingress filters** (RP 2129): source IP allow-list (C5), payload type filter (U4), rate limiter token bucket (C7)

SRT uses AES-128/192/256 encryption + passphrase auth. Tunnels use QUIC/TLS 1.3 + HMAC-SHA256 tokens.

### API Structure (`src/api/server.rs`)

- Public: `/health`, `/oauth/token`, `/metrics`
- Read-only (JWT or auth-disabled): `GET /api/v1/*`
- Admin (requires `admin` role): `POST/PUT/DELETE /api/v1/*`
- WebSocket: `/api/v1/ws/stats` (1/sec stats broadcast)

## Adding New Input/Output Types

Follow this pattern when extending protocol support:

| Step | File | What to do |
|------|------|------------|
| 1 | `src/config/models.rs` | Add variant to `InputConfig` or `OutputConfig` enum |
| 2 | `src/config/validation.rs` | Add validation rules |
| 3 | `src/engine/input_xxx.rs` or `output_xxx.rs` | Implement the task module |
| 4 | `src/engine/mod.rs` | Declare `pub mod` |
| 5 | `src/engine/flow.rs` | Add `match` arm in `start()` / `start_output()` + config metadata |

Every spawn function follows this signature convention:
```rust
pub fn spawn_xxx_output(
    config: XxxOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()>
```

## Configuration

Full reference in `docs/CONFIGURATION.md`. Config is JSON with enum-tagged input/output types (e.g., `"type": "srt"`, `"mode": "caller"`). Validation runs at load time, not per-packet.

## Key Design Constraints

- **Never block the input task** — output lag must not propagate upstream
- **Lock-free on hot path** — AtomicU64 for stats, DashMap for registries, no Mutex in packet flow
- **Graceful degradation** — slow outputs drop packets rather than buffering unboundedly
- **Hierarchical cancellation** — stopping a flow cancels all children; stopping one output leaves others running
