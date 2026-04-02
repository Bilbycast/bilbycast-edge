# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

bilbycast-edge is a Rust media transport gateway for professional broadcast workflows. It bridges multiple protocols (SRT, RTP, UDP, RTMP, RTSP, HLS, WebRTC WHIP/WHEP) with SMPTE 2022-1 FEC and SMPTE 2022-7 hitless redundancy. WebRTC support includes all four WHIP/WHEP modes (input and output, client and server) via the pure-Rust `str0m` WebRTC stack, interoperable with OBS, browsers, and standard WHIP/WHEP implementations. Designed for low-latency, uninterrupted media flow with broadcast-grade QoS.

## Build Commands

```bash
cargo check                    # Quick type/borrow check
cargo build                    # Debug build (all features: TLS + WebRTC)
cargo build --release          # Optimized release build (all features)
cargo test                     # Run all tests
cargo test <test_name>         # Run a single test
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

Both features are enabled by default. A plain `cargo build` includes everything.

- `tls` — HTTPS and RTMPS support (tokio-rustls + axum-server) — **default on**
- `webrtc` — WebRTC WHIP/WHEP input and output via str0m (pure Rust, sans-I/O) — **default on**

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
        │    ├─ Optional: Hitless merge (2022-7, RTP or SRT dual-leg)
        │    └─ Publishes RtpPacket into broadcast channel
        │
        ├─ broadcast::Sender ──► Output-1 (subscriber)
        │                      ├─► Output-2
        │                      └─► Output-N
        │
        ├─ TR-101290 Analyzer (independent subscriber)
        ├─ Media Analyzer (independent subscriber, toggleable per-flow)
        ├─ Thumbnail Generator (independent subscriber, requires ffmpeg)
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
| `engine/` | `input_rtp.rs`, `input_udp.rs`, `input_srt.rs`, `input_rtmp.rs` | Protocol-specific input tasks |
| `engine/` | `output_rtp.rs`, `output_udp.rs`, `output_srt.rs`, `output_rtmp.rs`, `output_hls.rs`, `output_webrtc.rs` | Protocol-specific output tasks |
| `engine/rtmp/` | `server.rs`, `amf0.rs`, `chunk.rs`, `ts_mux.rs` | RTMP protocol internals, FLV→MPEG-TS. `ts_mux.rs` is the shared TS muxer (PAT/PMT/PES) used by RTMP, RTSP, and WebRTC inputs. PAT/PMT `section_length` must include the 4-byte CRC32 per MPEG-TS spec. Inputs using TsMuxer must bundle TS packets per-frame (not per-188-byte packet) to avoid broadcast channel pressure |
| `engine/` | `tr101290.rs` | Transport stream quality analysis (sync, CC, PAT/PMT, PCR) |
| `engine/` | `media_analysis.rs` | Media content detection (codec, resolution, frame rate, audio format, per-PID bitrate) |
| `engine/` | `thumbnail.rs` | Optional per-flow thumbnail generation via ffmpeg subprocess. Buffers ~3s of TS packets, pipes to ffmpeg every 10s for a 320x180 JPEG. Gated by `FlowConfig.thumbnail` + ffmpeg availability (detected at startup). Thumbnails sent to manager as `thumbnail` WS messages |
| `engine/` | `ts_parse.rs` | Shared MPEG-TS packet parsing helpers (used by tr101290, media_analysis, and thumbnail) |
| `engine/` | `input_rtsp.rs` | RTSP input: pull H.264/H.265 + AAC from IP cameras and media servers via retina crate |
| `engine/` | `input_webrtc.rs` | WebRTC input: WHIP server (receive contributions) and WHEP client (pull from server) |
| `engine/webrtc/` | `session.rs`, `ts_demux.rs`, `rtp_h264.rs`, `rtp_h264_depack.rs`, `signaling.rs` | WebRTC core: str0m session wrapper, TS demuxer, RFC 6184 H.264 packetizer/depacketizer, WHIP/WHEP HTTP signaling |
| `api/` | `webrtc.rs` | WHIP/WHEP HTTP endpoint handlers and session registry. **Important:** When a WebRTC/WHIP flow is created, its `whip_session_tx` (stored in `FlowRuntime`) must be registered with `WebrtcSessionRegistry` via `register_whip_input()` — this wiring happens in `main.rs`, `api/flows.rs`, and `manager/client.rs` after every `create_flow` call |
| `fec/` | `encoder.rs`, `decoder.rs`, `matrix.rs` | SMPTE 2022-1 FEC (XOR column×row) |
| `redundancy/` | `merger.rs` | SMPTE 2022-7 hitless merge (seq dedup from dual RTP or SRT legs) |
| `api/` | `server.rs`, `auth.rs`, `flows.rs`, `stats.rs`, `tunnels.rs`, `ws.rs`, `nmos.rs`, `nmos_is05.rs` | Axum REST API, OAuth2/JWT, WebSocket stats, NMOS IS-04 Node API, NMOS IS-05 Connection Management |
| `config/` | `models.rs`, `validation.rs`, `persistence.rs`, `secrets.rs` | JSON config, enum-tagged types, atomic save, secrets split (`config.json` + `secrets.json`) |
| `stats/` | `collector.rs`, `models.rs`, `throughput.rs` | Lock-free stats registry, bitrate estimation |
| `tunnel/` | `manager.rs`, `relay_client.rs`, `udp_forwarder.rs`, `tcp_forwarder.rs`, `crypto.rs`, `auth.rs` | QUIC-based IP tunnels (relay/direct), end-to-end encryption, HMAC-SHA256 bind authentication. `protocol.rs` defines `TUNNEL_PROTOCOL_VERSION`, `ParsedMessage<T>` + `read_message_resilient()` for graceful handling of unknown message types, and `Hello`/`HelloAck` handshake for version detection |
| `manager/` | `client.rs`, `config.rs`, `events.rs` | WebSocket client to bilbycast-manager. Sends stats (1s), health (15s), thumbnails (10s), and operational events. `events.rs` defines `EventSender`/`EventSeverity`/`Event` types and the event channel. See `docs/events-and-alarms.md` for the full event reference. Auth payload includes `protocol_version` and `software_version` for compatibility detection. Handles commands: get_config, create/delete/start/stop flow, update_flow (diff-based), update_config (diff-based), add/remove output, create/delete tunnel. **GetConfig strips secrets** — `strip_secrets()` removes all sensitive fields before serializing the response, so the manager never receives `node_secret`, tunnel keys, SRT passphrases, etc. **UpdateConfig merges secrets** — incoming config from manager (which lacks secrets) is merged with existing local secrets before applying. **Config updates use diff logic** — `update_flow` and `update_config` compare old vs new using `PartialEq` and only restart flows when input/metadata changes; output-only changes are applied surgically via hot-add/remove without disrupting other outputs or SRT connections |
| `monitor/` | `server.rs`, `dashboard.rs` | Embedded HTML/JS dashboard on separate port |
| `setup/` | `handlers.rs`, `wizard.rs` | Browser-based setup wizard for initial provisioning (inline HTML, gated by `setup_enabled` config flag) |
| `srt/` | `connection.rs` | SRT socket config via `SrtConnectionParams` struct, stats polling (45+ fields including FEC, ACK/NAK, flow control, buffer state), `convert_srt_stats()` |
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

SRT uses AES-128/192/256 encryption + passphrase auth with selectable cipher mode (`crypto_mode`: `"aes-ctr"` default or `"aes-gcm"` for authenticated encryption; GCM requires libsrt >= 1.5.2 peer, AES-128/256 only). SRT Stream ID access control: callers send `stream_id` (max 512 chars) during handshake; listeners with `stream_id` set reject connections that don't match. Supports plain strings and structured `#!::r=resource,m=publish,u=user` format per the SRT Access Control specification. SRT FEC (Forward Error Correction): optional `packet_filter` enables XOR-based FEC with configurable row/column groups (`"fec,cols:10,rows:5,layout:staircase,arq:onreq"`), negotiated during handshake via `SRT_CMD_FILTER`, wire-compatible with libsrt v1.5.5. Tunnels use QUIC/TLS 1.3 for transport + ChaCha20-Poly1305 (AEAD) end-to-end encryption between edge nodes. The `tunnel_encryption_key` (32-byte hex-encoded) is generated by the manager and shared with both edges. The relay cannot read tunnel traffic. Optional relay bind authentication: the manager distributes a `tunnel_bind_secret` (32-byte hex) to both edges; edges compute HMAC-SHA256 `bind_token` and include it in `TunnelBind` messages. Tunnel IDs must be valid UUIDs (v5 deterministic fallback removed for security).

**Input validation** (`src/config/validation.rs`): All configs are validated at load time and when received via manager WebSocket commands. Validation includes:
- String length limits: IDs max 64 chars, names max 256 chars, URLs max 2048 chars, tokens max 4096 chars
- Socket address parsing, address family consistency, DSCP 0-63, FEC params bounded
- SRT passphrase 10-79 chars, AES key length 16/24/32, crypto_mode must be "aes-ctr" or "aes-gcm" (GCM rejects AES-192), mode-specific field requirements, max_rexmit_bw must be >= -1, stream_id max 512 chars (per SRT spec), SRT packet_filter max 512 chars with FEC config validation (cols/rows 1-256), max_bw >= 0, overhead_bw 5-100, flight_flag_size/send_buffer_size/recv_buffer_size >= 32, ip_tos 0-255, retransmit_algo "default"/"reduced", payload_size 188-1456, km_refresh_rate/km_pre_announce > 0
- RTP bitrate limit max 10 Gbps, HLS segments 0.5-10s, RTMP URL scheme validation
- Tunnel config: tunnel ID must be valid UUID, mode-specific required fields, address validation, PSK length limits, `tunnel_encryption_key` must be exactly 64 hex chars (32 bytes) when present (required for relay mode, optional for direct mode), `tunnel_bind_secret` must be exactly 64 hex chars if present
- **Manager commands are validated before execution** — `create_flow`, `update_flow`, `add_output`, `update_config`, and `create_tunnel` all call their respective validation functions after deserialization
- Duplicate flow ID detection on create

**Manager connection**: The WebSocket client to bilbycast-manager enforces `wss://` (TLS). Plaintext `ws://` URLs are rejected at connection time. Self-signed cert mode (`accept_self_signed_cert: true`) requires `BILBYCAST_ALLOW_INSECURE=1` env var as a safety guard. Optional certificate pinning (`cert_fingerprint`) validates the server's SHA-256 certificate fingerprint against a configured value, protecting against compromised CAs. Secret rotation (`rotate_secret` command) allows the manager to replace the node's authentication secret over the active WebSocket connection.

### API Structure (`src/api/server.rs`)

- Public: `/health`, `/oauth/token`, `/metrics`, `/setup` (gated by `setup_enabled`)
- Read-only (JWT or auth-disabled): `GET /api/v1/*`
- Admin (requires `admin` role): `POST/PUT/DELETE /api/v1/*`
- WebSocket: `/api/v1/ws/stats` (1/sec stats broadcast)
- NMOS IS-04 (public, no auth): `/x-nmos/node/v1.3/` — Node, Device, Source, Flow, Sender, Receiver resources
- NMOS IS-05 (public, no auth): `/x-nmos/connection/v1.1/` — Staged/active transport parameter management for senders and receivers

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

Full reference in `docs/CONFIGURATION.md`. Config is JSON with enum-tagged input/output types (e.g., `"type": "srt"`, `"mode": "caller"`). Validation runs at load time and on every manager command — not per-packet. When adding new config fields, always add validation in `src/config/validation.rs`.

### Config/Secrets Split

Configuration is persisted across two files:

- **`config.json`** — Operational config (addresses, ports, protocols, flow definitions without secrets). Safe to send to the manager, inspect, and version-control.
- **`secrets.json`** — All secrets (node credentials, tunnel encryption keys, SRT passphrases, RTMP stream keys, auth config, TLS paths). Never leaves the node. **Encrypted at rest** using AES-256-GCM with a machine-specific key derived from `/etc/machine-id` (Linux) or a generated `.secrets_key` file (fallback for macOS/containers). Written with `0600` permissions on Unix. Existing unencrypted files are auto-migrated on first load.

At runtime, both files merge into a single `AppConfig` in memory — all existing code works unchanged. The split is purely a persistence and serialization boundary.

**Secrets inventory** (fields stored in `secrets.json`):
- Node-level: `manager.node_secret`, `manager.registration_token`, `server.tls`, `server.auth` (jwt_secret, client credentials)
- Per-tunnel (keyed by ID): `tunnel_encryption_key`, `tunnel_bind_secret`, `tunnel_psk`, `tls_cert_pem`, `tls_key_pem`

**Flow parameters in `config.json`** (NOT in secrets.json):
- SRT `passphrase`, RTMP `stream_key`, RTSP `username`/`password`, WebRTC `bearer_token`, HLS `auth_token`
- These stay in config.json so they are visible in the manager UI and survive round-trip config updates

**Key implementation files:**
- `src/config/secrets.rs` — `SecretsConfig` struct, `extract_from()`, `merge_into()`, `has_secrets()`, and `AppConfig::strip_secrets()`
- `src/config/crypto.rs` — AES-256-GCM encryption/decryption for `secrets.json`, machine seed derivation (`/etc/machine-id` or `.secrets_key` fallback), HKDF-SHA256 key derivation
- `src/config/persistence.rs` — `load_config_split()` (with auto-migration from legacy single-file), `save_config_split()`, `save_secrets()`

**Migration**: On first startup after upgrade, if `config.json` contains secrets and `secrets.json` does not exist, the system automatically splits them. Users don't need to take any action.

**When adding new infrastructure secret fields**: Add the field to the appropriate secrets struct (`TunnelSecrets` or `SecretsConfig`), update `extract_from`/`merge_into`/`strip_secrets`, and update `has_secrets` for migration detection. Flow-level user parameters (passphrases, credentials, keys) should stay in `config.json`, not `secrets.json`.

## Key Design Constraints

- **Never block the input task** — output lag must not propagate upstream
- **Lock-free on hot path** — AtomicU64 for stats, DashMap for registries, no Mutex in packet flow
- **Graceful degradation** — slow outputs drop packets rather than buffering unboundedly
- **Hierarchical cancellation** — stopping a flow cancels all children; stopping one output leaves others running
