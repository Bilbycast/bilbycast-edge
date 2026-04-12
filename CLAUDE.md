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

SRT support depends on one of two swappable sibling crate sets (identical public API):
- **Pure Rust** (default): `srt-transport` + `srt-protocol` from `../bilbycast-srt/`
- **libsrt wrapper**: `srt-transport` + `srt-protocol` from `../bilbycast-libsrt-rs/`

Switch by commenting/uncommenting two path lines in `Cargo.toml` — look for the `── SRT backend ──` block. Default is libsrt. No other code changes needed. The libsrt wrapper adds SRT bonding and guarantees FEC/encryption interop with all SRT devices; the pure-Rust version has zero C dependencies.

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

**Inputs** and **outputs** are independent top-level entities in the config (`inputs` and `outputs` arrays). Each has a stable ID and name. **Flows** connect one or more inputs to N outputs by reference (`input_ids` + `output_ids`). A flow may have multiple inputs but at most one is active (publishing to the broadcast channel) at a time. At startup (or on create/update), `AppConfig::resolve_flow()` dereferences the IDs into a `ResolvedFlow` containing `Vec<InputDefinition>` and `Vec<OutputConfig>`. The engine only ever sees `ResolvedFlow` — the reference-resolution boundary lives in the API/config layer, not the engine.

At runtime, each flow has one or more inputs (one active at a time) and N outputs connected via a `tokio::broadcast::channel(2048)`. The `FlowManager` (`src/engine/manager.rs`) holds all active flows in a `DashMap<FlowId, Arc<FlowRuntime>>`. Hot-add/remove of outputs, backpressure rules, and fan-out behavior are unchanged.

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
| `engine/` | `manager.rs`, `flow.rs`, `packet.rs` | Flow lifecycle, FlowRuntime bring-up/teardown. `create_flow()` and `FlowRuntime::start()` accept `ResolvedFlow` (not raw config references) |
| `engine/` | `input_rtp.rs`, `input_udp.rs`, `input_srt.rs`, `input_rtmp.rs` | Protocol-specific input tasks |
| `engine/` | `delay_buffer.rs` | Per-output delay buffer for stream synchronization. `DelayBuffer` queues packets and releases them after a configurable delay. `resolve_output_delay()` is the entry point that resolves an `OutputDelay` config into a `DelayBuffer`, handling the async frame-rate wait for `TargetFrames` mode. Non-blocking, zero-copy (moves only the `RtpPacket` struct, not the `Bytes` data). Used to align parallel outputs with different processing latencies (e.g., clean feed vs. commentary-processed feed). |
| `engine/` | `output_rtp.rs`, `output_udp.rs`, `output_srt.rs`, `output_rtmp.rs`, `output_hls.rs`, `output_webrtc.rs` | Protocol-specific output tasks. Every variant accepts an optional `program_number: u16` in its config. SRT, RTP, and UDP outputs also accept an optional `delay: OutputDelay` for stream synchronization — supports fixed delay (ms), target end-to-end latency (ms), or target latency in video frames (auto-detected fps). TS-native outputs (UDP/RTP/SRT/HLS) run a `TsProgramFilter` inline when set (MPTS→SPTS rewrite with FEC/2022-7 operating on the filtered bytes); re-muxing outputs (RTMP/WebRTC) pass the selector into `TsDemuxer::new` which either picks a specific program's PMT or — when unset — locks onto the lowest `program_number` in the PAT (deterministic default). **Audio encode (Phase B):** RTMP, HLS, and WebRTC outputs accept an optional `audio_encode` block. When set, the output decodes the input AAC-LC via `engine::audio_decode::AacDecoder` (Phase A), runs the planar f32 PCM through an `engine::audio_encode::AudioEncoder` ffmpeg subprocess that emits the configured codec (`aac_lc` / `he_aac_v1` / `he_aac_v2` / `opus` / `mp2` / `ac3` per the codec×output matrix), and re-injects the encoded frames into the egress path. RTMP and WebRTC use one persistent ffmpeg per output; HLS forks ffmpeg per segment as a TS-in/TS-out remuxer (`-c:v copy -c:a {codec}`). The pure-Rust binary is unchanged — ffmpeg is invoked at runtime via subprocess, never linked. Outputs without `audio_encode` set keep working without ffmpeg installed |
| `engine/rtmp/` | `server.rs`, `amf0.rs`, `chunk.rs`, `ts_mux.rs` | RTMP protocol internals, FLV→MPEG-TS. `ts_mux.rs` is the shared TS muxer (PAT/PMT/PES) used by RTMP, RTSP, and WebRTC inputs. PAT/PMT `section_length` must include the 4-byte CRC32 per MPEG-TS spec. Inputs using TsMuxer must bundle TS packets per-frame (not per-188-byte packet) to avoid broadcast channel pressure |
| `engine/` | `tr101290.rs` | Transport stream quality analysis (sync, CC, PAT/PMT, PCR) |
| `engine/` | `media_analysis.rs` | Media content detection (codec, resolution, frame rate, audio format, per-PID bitrate) |
| `engine/` | `thumbnail.rs` | Optional per-flow thumbnail generation via ffmpeg subprocess. Buffers ~3s of TS packets, pipes to ffmpeg every 10s for a 320x180 JPEG. Gated by `FlowConfig.thumbnail` + ffmpeg availability (detected at startup). Thumbnails sent to manager as `thumbnail` WS messages |
| `engine/` | `ts_parse.rs` | Shared MPEG-TS packet parsing helpers (used by tr101290, media_analysis, and thumbnail). Includes `parse_pat_programs` which returns `(program_number, pmt_pid)` pairs, reused by the program filter and the demuxer. |
| `engine/` | `ts_program_filter.rs` | **MPTS → SPTS filter.** `TsProgramFilter` consumes raw TS and emits only the packets belonging to a target program, rewriting the PAT into a single-program form (CRC via `mpeg2_crc32`). Used by UDP/RTP/SRT/HLS outputs and the thumbnail generator when `program_number` (or `thumbnail_program_number`) is set. Unit-tested for MPTS down-selection, synthetic PAT validity, and PAT version-bump handling |
| `engine/` | `input_rtsp.rs` | RTSP input: pull H.264/H.265 + AAC from IP cameras and media servers via retina crate |
| `engine/` | `input_webrtc.rs` | WebRTC input: WHIP server (receive contributions) and WHEP client (pull from server) |
| `engine/webrtc/` | `session.rs`, `ts_demux.rs`, `rtp_h264.rs`, `rtp_h264_depack.rs`, `signaling.rs` | WebRTC core: str0m session wrapper, TS demuxer, RFC 6184 H.264 packetizer/depacketizer, WHIP/WHEP HTTP signaling |
| `engine/` | `input_st2110_30.rs`, `input_st2110_31.rs`, `input_st2110_40.rs`, `output_st2110_*.rs`, `st2110_io.rs` | **SMPTE ST 2110 (Phase 1).** Six thin spawn modules backed by `st2110_io.rs` shared runtime helpers. ST 2110-30 (L16/L24 PCM), ST 2110-31 (AES3 transparent), ST 2110-40 (RFC 8331 ANC). Inputs validate via `engine::st2110::audio::PcmDepacketizer` / `engine::st2110::ancillary::unpack_ancillary` and republish whole `RtpPacket`s through the broadcast channel — paired in/out achieves byte-identical loopback by passthrough. SMPTE 2022-7 Red/Blue dual-network bind via `engine::st2110::redblue::RedBluePair`, optional source-IP allow-list (single-leg only). Stats publishes `PtpStateHandle` and `RedBlueStats` onto `FlowStatsAccumulator` so `snapshot()` populates `ptp_state` / `network_legs`. |
| `engine/st2110/` | `audio.rs`, `ancillary.rs`, `sdp.rs`, `ptp.rs`, `redblue.rs`, `hwts.rs`, `scte104.rs`, `timecode.rs`, `captions.rs` | ST 2110 essence helpers: PCM packetize/depacketize, RFC 8331 ANC pack/unpack with hand-rolled bit reader/writer, RFC 4566 SDP generator+parser scoped to -30/-31/-40, PTP state reporter that polls `/var/run/ptp4l` management socket via Unix datagram (best-effort, falls back to `Unavailable`), Red/Blue UDP bind around `redundancy::merger::HitlessMerger`, Linux SO_TIMESTAMPING via libc directly (no `nix` dep, macOS returns Unsupported), single-op SCTE-104 parser, SMPTE 12M-2 ATC timecode, CEA-608/708 caption detection. All pure Rust, all unit-tested. |
| `api/` | `nmos.rs`, `nmos_is05.rs`, `nmos_is08.rs`, `nmos_mdns.rs` | NMOS Phase 1 surface: IS-04 Node API with multi-essence audio/data formats and BCP-004 receiver caps; IS-05 Connection Management; IS-08 audio channel mapping (`/io`, `/map/active`, `/map/staged`, `/map/activate`) with disk-persisted active map; best-effort `_nmos-node._tcp` mDNS-SD registration via the pure-Rust `mdns-sd` crate. The IS-04 `/self` resource advertises a PTP clock entry whenever any flow declares `clock_domain`. |
| `api/` | `webrtc.rs` | WHIP/WHEP HTTP endpoint handlers and session registry. **Important:** When a WebRTC/WHIP flow is created, its `whip_session_tx` (stored in `FlowRuntime`) must be registered with `WebrtcSessionRegistry` via `register_whip_input()` — this wiring happens in `main.rs`, `api/flows.rs`, and `manager/client.rs` after every `create_flow` call |
| `fec/` | `encoder.rs`, `decoder.rs`, `matrix.rs` | SMPTE 2022-1 FEC (XOR column×row) |
| `redundancy/` | `merger.rs` | SMPTE 2022-7 hitless merge (seq dedup from dual RTP or SRT legs) |
| `api/` | `inputs.rs`, `outputs.rs` | CRUD endpoints for independent top-level inputs and outputs |
| `api/` | `flows.rs` | Flow CRUD. Resolves `input_ids`/`output_ids` references via `AppConfig::resolve_flow()` before engine calls. `add_output` takes an `output_id` (not inline config) and assigns it; `remove_output` unassigns an output from the flow |
| `api/` | `server.rs`, `auth.rs`, `stats.rs`, `tunnels.rs`, `ws.rs`, `nmos.rs`, `nmos_is05.rs` | Axum REST API, OAuth2/JWT, WebSocket stats, NMOS IS-04 Node API, NMOS IS-05 Connection Management |
| `config/` | `models.rs`, `validation.rs`, `persistence.rs`, `secrets.rs` | JSON config (version 2), enum-tagged types, atomic save, secrets split (`config.json` + `secrets.json`). `models.rs` defines `InputDefinition`, `OutputConfig` (top-level), `FlowConfig` (with `input_ids`/`output_ids` references), `ResolvedFlow`, and the `AppConfig::resolve_flow()` method |
| `stats/` | `collector.rs`, `models.rs`, `throughput.rs` | Lock-free stats registry, bitrate estimation |
| `tunnel/` | `manager.rs`, `relay_client.rs`, `udp_forwarder.rs`, `tcp_forwarder.rs`, `crypto.rs`, `auth.rs` | QUIC-based IP tunnels (relay/direct), end-to-end encryption, HMAC-SHA256 bind authentication. `protocol.rs` defines `TUNNEL_PROTOCOL_VERSION`, `ParsedMessage<T>` + `read_message_resilient()` for graceful handling of unknown message types, and `Hello`/`HelloAck` handshake for version detection |
| `manager/` | `client.rs`, `config.rs`, `events.rs` | WebSocket client to bilbycast-manager. Sends stats (1s), health (15s), thumbnails (10s), and operational events. `events.rs` defines `EventSender`/`EventSeverity`/`Event` types and the event channel. See `docs/events-and-alarms.md` for the full event reference. Auth payload includes `protocol_version` and `software_version` for compatibility detection. Handles commands: get_config, create/delete/start/stop flow, update_flow (diff-based), update_config (diff-based), add/remove output, create/delete tunnel, **plus 6 new input/output CRUD command handlers** (create/update/delete input, create/update/delete output). **GetConfig strips secrets** — `strip_secrets()` removes all sensitive fields before serializing the response, so the manager never receives `node_secret`, tunnel keys, SRT passphrases, etc. **UpdateConfig merges secrets** — incoming config from manager (which lacks secrets) is merged with existing local secrets before applying. **Config updates use diff logic** — `update_flow` and `update_config` compare old vs new using `PartialEq` and only restart flows when input/metadata changes; output-only changes are applied surgically via hot-add/remove without disrupting other outputs or SRT connections |
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
- `program_number` (on every output) and `thumbnail_program_number` (on flow): must be `> 0` if set — program number 0 is reserved for the NIT in the MPEG-TS spec and never identifies a real program
- Tunnel config: tunnel ID must be valid UUID, mode-specific required fields, address validation, PSK length limits, `tunnel_encryption_key` must be exactly 64 hex chars (32 bytes) when present (required for relay mode, optional for direct mode), `tunnel_bind_secret` must be exactly 64 hex chars if present
- **Manager commands are validated before execution** — `create_flow`, `update_flow`, `add_output`, `update_config`, `create_tunnel`, and the new input/output CRUD commands all call their respective validation functions after deserialization
- **Inputs and outputs are validated independently** — each is validated when created or updated, regardless of whether it is assigned to a flow
- **Assignment uniqueness is enforced** — an input or output cannot be assigned to multiple flows simultaneously. Duplicate input/output/flow ID detection on create

**Manager connection**: The WebSocket client to bilbycast-manager enforces `wss://` (TLS). Plaintext `ws://` URLs are rejected at connection time. Self-signed cert mode (`accept_self_signed_cert: true`) requires `BILBYCAST_ALLOW_INSECURE=1` env var as a safety guard. Optional certificate pinning (`cert_fingerprint`) validates the server's SHA-256 certificate fingerprint against a configured value, protecting against compromised CAs. Secret rotation (`rotate_secret` command) allows the manager to replace the node's authentication secret over the active WebSocket connection.

### API Structure (`src/api/server.rs`)

- Public: `/health`, `/oauth/token`, `/metrics`, `/setup` (gated by `setup_enabled`)
- Read-only (JWT or auth-disabled): `GET /api/v1/*`
- Admin (requires `admin` role): `POST/PUT/DELETE /api/v1/*`
- **Inputs**: `GET /api/v1/inputs` (list), `POST /api/v1/inputs` (create), `GET /api/v1/inputs/{id}`, `PUT /api/v1/inputs/{id}`, `DELETE /api/v1/inputs/{id}`
- **Outputs**: `GET /api/v1/outputs` (list), `POST /api/v1/outputs` (create), `GET /api/v1/outputs/{id}`, `PUT /api/v1/outputs/{id}`, `DELETE /api/v1/outputs/{id}`
- WebSocket: `/api/v1/ws/stats` (1/sec stats broadcast)
- NMOS IS-04 (public, no auth): `/x-nmos/node/v1.3/` — Node, Device, Source, Flow, Sender, Receiver resources. ST 2110-30/-31 inputs report `urn:x-nmos:format:audio`; ST 2110-40 inputs report `urn:x-nmos:format:data`. Receiver caps include BCP-004 constraint sets for ST 2110 audio (sample_rate / channel_count / sample_depth)
- NMOS IS-05 (public, no auth): `/x-nmos/connection/v1.1/` — Staged/active transport parameter management for senders and receivers
- NMOS IS-08 (public, no auth): `/x-nmos/channelmapping/v1.0/` — Audio channel mapping (`io`, `map/active`, `map/staged`, `map/activate`). Active map persists to `<config_dir>/nmos_channel_map.json`
- mDNS-SD: `_nmos-node._tcp` registered on startup via `api::nmos_mdns` (best-effort, never blocks startup)

## Adding New Input/Output Types

Inputs and outputs are top-level entities in the config. Flows reference them by ID. Follow this pattern when extending protocol support:

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

Full reference in `docs/CONFIGURATION.md`. Config version is **2**. The JSON structure has three top-level arrays:

- **`inputs`** — Array of `InputDefinition` objects. Each has `id`, `name`, and flattened `InputConfig` fields (enum-tagged by `"type"`, e.g., `"type": "srt"`, `"mode": "listener"`).
- **`outputs`** — Array of `OutputConfig` objects. Each has `id`, `name`, and protocol-specific fields (enum-tagged by `"type"`).
- **`flows`** — Array of `FlowConfig` objects. Each has `id`, `name`, `input_ids` (array of input IDs, one active at a time), and `output_ids` (array of output IDs). The engine resolves these references via `AppConfig::resolve_flow()` into `ResolvedFlow` before starting.

An input or output can exist without being assigned to any flow. Assignment uniqueness is enforced: an input can only be used by one flow at a time, and each output can only be assigned to one flow at a time.

Validation runs at load time and on every manager command — not per-packet. When adding new config fields, always add validation in `src/config/validation.rs`.

### Config/Secrets Split

Configuration is persisted across two files:

- **`config.json`** — Operational config (addresses, ports, protocols, flow definitions without secrets). Safe to send to the manager, inspect, and version-control.
- **`secrets.json`** — All secrets (node credentials, tunnel encryption keys, SRT passphrases, RTMP stream keys, auth config, TLS paths). Never leaves the node. **Encrypted at rest** using AES-256-GCM with a machine-specific key derived from `/etc/machine-id` (Linux) or a generated `.secrets_key` file (fallback for macOS/containers). Written with `0600` permissions on Unix. Existing unencrypted files are auto-migrated on first load.

At runtime, both files merge into a single `AppConfig` in memory — all existing code works unchanged. The split is purely a persistence and serialization boundary.

**Secrets inventory** (fields stored in `secrets.json`):
- Node-level: `manager.node_secret`, `manager.registration_token`, `server.tls`, `server.auth` (jwt_secret, client credentials)
- Per-tunnel (keyed by ID): `tunnel_encryption_key`, `tunnel_bind_secret`, `tunnel_psk`, `tls_cert_pem`, `tls_key_pem`

**Input/output parameters in `config.json`** (NOT in secrets.json):
- SRT `passphrase`, RTMP `stream_key`, RTSP `username`/`password`, WebRTC `bearer_token`, HLS `auth_token`
- These live in the top-level `inputs` and `outputs` arrays in config.json so they are visible in the manager UI and survive round-trip config updates

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
