# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

bilbycast-edge is a Rust media transport gateway for professional broadcast workflows. It bridges multiple protocols (SRT, RIST Simple Profile, RTP, UDP, RTMP, RTSP, HLS, WebRTC WHIP/WHEP, CMAF / CMAF-LL with optional ClearKey CENC encryption) with SMPTE 2022-1 FEC and SMPTE 2022-7 hitless redundancy. SRT additionally supports **native libsrt socket-group bonding** (`bonding: { mode: broadcast | backup, endpoints[] }` on SRT inputs/outputs) — wire-compatible with `srt-live-transmit grp://`, Haivision socket groups, and every other libsrt-based bonding peer — with per-member stats surfaced through the WS + Prometheus telemetry. Bonding is mutually exclusive with the app-layer `redundancy` block and requires the libsrt backend. WebRTC support includes all four WHIP/WHEP modes (input and output, client and server) via the pure-Rust `str0m` WebRTC stack, interoperable with OBS, browsers, and standard WHIP/WHEP implementations. Designed for low-latency, uninterrupted media flow with broadcast-grade QoS.

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

See the root `CLAUDE.md` for the full backend comparison and switching instructions. Edge-specific notes:

- **SRT**: two swappable sibling crate sets expose identical `srt-transport` + `srt-protocol` APIs (`../bilbycast-srt/` pure Rust, or `../bilbycast-libsrt-rs/` wrapper). Switch via the `── SRT backend ──` block in `Cargo.toml`.
- **RIST**: always compiled in, no feature flag, zero C deps. `rist-transport::RistSocket::receiver` / `::sender` is consumed directly by `engine::input_rist` / `engine::output_rist`. Wire-verified against librist 0.2.11.

## Feature Flags

See the root `CLAUDE.md` feature flag table for the canonical list, defaults, and build prerequisites. Default `cargo build` produces an AGPL-only binary with no software video encoders; `video-encoder-x264` / `-x265` / `-nvenc` are opt-in (GPL or vendor-specific) and bundled by the `video-encoders-full` composite. The GitHub Actions release workflow ships two binary variants per architecture — a default (AGPL-only) and a full (AGPL + GPL bundle) — see `docs/installation.md` for the release channel reference and `docs/transcoding.md` for the full transcoding reference.

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
        ├─ Thumbnail Generator (independent subscriber, in-process via libavcodec)
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
| `engine/` | `input_rtp.rs`, `input_udp.rs`, `input_srt.rs`, `input_rist.rs`, `input_rtmp.rs` | Protocol-specific input tasks. `input_rist.rs` wraps `rist-transport::RistSocket::receiver` and publishes the delivered RTP payload as `RtpPacket { is_raw_ts: true }`. Optional SMPTE 2022-7 dual-leg merge via the shared `HitlessMerger`. |
| `engine/` | `delay_buffer.rs` | Per-output delay buffer for stream synchronization. `DelayBuffer` queues packets and releases them after a configurable delay. `resolve_output_delay()` is the entry point that resolves an `OutputDelay` config into a `DelayBuffer`, handling the async frame-rate wait for `TargetFrames` mode. Non-blocking, zero-copy (moves only the `RtpPacket` struct, not the `Bytes` data). Used to align parallel outputs with different processing latencies (e.g., clean feed vs. commentary-processed feed). |
| `engine/` | `output_rtp.rs`, `output_udp.rs`, `output_srt.rs`, `output_rist.rs`, `output_rtmp.rs`, `output_hls.rs`, `output_webrtc.rs`, `cmaf/` (CMAF / CMAF-LL with optional CENC ClearKey encryption — see the dedicated row below) | Protocol-specific output tasks. Every variant accepts an optional `program_number: u16` in its config. SRT, RIST, RTP, and UDP outputs also accept an optional `delay: OutputDelay` for stream synchronization — supports fixed delay (ms), target end-to-end latency (ms), or target latency in video frames (auto-detected fps). TS-native outputs (UDP/RTP/SRT/HLS) run a `TsProgramFilter` inline when set (MPTS→SPTS rewrite with FEC/2022-7 operating on the filtered bytes); re-muxing outputs (RTMP/WebRTC) pass the selector into `TsDemuxer::new` which either picks a specific program's PMT or — when unset — locks onto the lowest `program_number` in the PAT (deterministic default). **Audio encode (Phase B):** RTMP, HLS, WebRTC — and now **SRT, RIST, UDP, RTP** — accept an optional `audio_encode` block. When set, the output decodes the input AAC-LC via `engine::audio_decode::AacDecoder` (Phase A), runs the planar f32 PCM through an `engine::audio_encode::AudioEncoder` that emits the configured codec (`aac_lc` / `he_aac_v1` / `he_aac_v2` / `opus` / `mp2` / `ac3` per the codec×output matrix), and re-injects the encoded frames into the egress path. On TS outputs (SRT / UDP / RTP) the streaming `engine::ts_audio_replace::TsAudioReplacer` owns the audio-ES replacement in place — rewriting the PMT stream_type with a recomputed CRC and leaving video + other PIDs untouched. Every output that accepts `audio_encode` also accepts an optional companion `transcode` block (channel shuffle / sample-rate / bit-depth conversion via `engine::audio_transcode::PlanarAudioTranscoder`). It sits between the AAC decoder and the target encoder; unset fields pass through that stage so an empty block is a no-op. Resolution rule: `transcode` wins over `audio_encode` on conflicting fields; any `audio_encode.sample_rate`/`channels` override is used as a fallback for whatever `transcode` leaves unset. ST 2110-31 (AES3) is excluded — its channel labels live inside the SMPTE 337M payload and are opaque to the pipeline. **Video encode (Phase 4):** **SRT, RIST, UDP, RTP, RTMP, and WebRTC** outputs accept an optional `video_encode` block. TS-carrying outputs run the streaming `engine::ts_video_replace::TsVideoReplacer` (source H.264/HEVC → `video-engine::VideoDecoder` → `video-engine::VideoEncoder` (libx264 / libx265 / NVENC, selected by Cargo feature) → re-muxed back into the TS with a fresh PMT when the codec family changes). RTMP drives the same decode+encode pair via `engine::output_rtmp::VideoEncoderState` (FLV sequence header built once from `VideoEncoder::extradata()` with `global_header = true`; H.264 rides classic FLV, HEVC rides Enhanced RTMP v2 `hvc1`). **WebRTC** drives it via `engine::output_webrtc::WebrtcVideoEncoderState` — H.264-only target (browsers do not decode HEVC; `x265` / `hevc_nvenc` rejected at validation), `global_header = false` so SPS/PPS travel in-band on every IDR and are forwarded by `engine::webrtc::rtp_h264::H264Packetizer` as ordinary RFC 6184 NAL units; HEVC source streams are decoded and re-encoded to H.264 automatically. HLS `video_encode` is the only remaining deferred transport — tracked in `docs/transcoding.md`. **Default (with `video-thumbnail` + `fdk-aac` features):** audio codecs are encoded in-process — AAC via fdk-aac, Opus/MP2/AC-3 via libavcodec (from `bilbycast-ffmpeg-video-rs`). Video encoders require an opt-in feature. No external ffmpeg binary required. **Fallback (features disabled):** uses ffmpeg subprocess for audio; video_encode refuses to start. HLS uses in-process TS audio remuxing (decode → re-encode → video passthrough) when `video-thumbnail` is enabled. Outputs without `audio_encode` / `video_encode` set need no codec libraries at all. Full reference: [`docs/transcoding.md`](docs/transcoding.md) |
| `engine/rtmp/` | `server.rs`, `amf0.rs`, `chunk.rs`, `ts_mux.rs` | RTMP protocol internals, FLV→MPEG-TS. `ts_mux.rs` is the shared TS muxer (PAT/PMT/PES) used by RTMP, RTSP, and WebRTC inputs. PAT/PMT `section_length` must include the 4-byte CRC32 per MPEG-TS spec. Inputs using TsMuxer must bundle TS packets per-frame (not per-188-byte packet) to avoid broadcast channel pressure |
| `engine/` | `tr101290.rs` | Transport stream quality analysis (sync, CC, PAT/PMT, PCR) |
| `engine/` | `media_analysis.rs` | Media content detection (codec, resolution, frame rate, audio format, per-PID bitrate) |
| `engine/` | `thumbnail.rs` | Optional per-flow thumbnail generation. **Default (`video-thumbnail` feature):** in-process via `video-engine` (FFmpeg libavcodec/libswscale) — extracts video NAL units from buffered TS, decodes one frame, scales to 320x180, encodes JPEG, computes luminance for black-screen detection, all without spawning a subprocess. **Fallback (feature disabled):** ffmpeg subprocess. Buffers ~3s of TS packets, generates every 10s. Gated by `FlowConfig.thumbnail` + availability. Thumbnails sent to manager as `thumbnail` WS messages |
| `engine/` | `ts_parse.rs` | Shared MPEG-TS packet parsing helpers (used by tr101290, media_analysis, and thumbnail). Includes `parse_pat_programs` which returns `(program_number, pmt_pid)` pairs, reused by the program filter and the demuxer. |
| `engine/` | `ts_continuity_fixer.rs` | **TS continuity fixer for input switching.** `TsContinuityFixer` sits in the input forwarder path (shared via `Arc<Mutex>` across all per-input forwarders within a flow) and ensures external receivers handle input switches cleanly. On switch: (1) clears output-side CC state so the new input's CC passes through, creating a natural CC jump that receivers detect and handle via packet-loss recovery (PES flush + resync on PUSI); (2) injects the new input's cached PAT/PMT with a bumped `version_number` (CRC32 recalculated) to force receivers to re-parse even when both inputs share the same PSI version; (3) forwards all packets immediately. Per-input PSI caching ensures each input's PAT/PMT is tracked independently. **Fully format-agnostic — inputs can use different codecs, containers, and transports** (e.g., H.264 on one input, H.265 on another, JPEG XS on a third, uncompressed ST 2110 on a fourth). Works for any video, audio, or data format. For non-TS transports (raw ST 2110 RTP), the fixer is transparent. Zero-cost before first switch (single `bool` check). Passive inputs pre-warm the PSI cache via `observe_passive()`. Unit-tested for CC passthrough, PSI version bumping, PAT/PMT injection, and RTP-wrapped TS |
| `engine/` | `ts_program_filter.rs` | **MPTS → SPTS filter.** `TsProgramFilter` consumes raw TS and emits only the packets belonging to a target program, rewriting the PAT into a single-program form (CRC via `mpeg2_crc32`). Used by UDP/RTP/SRT/HLS outputs and the thumbnail generator when `program_number` (or `thumbnail_program_number`) is set. Unit-tested for MPTS down-selection, synthetic PAT validity, and PAT version-bump handling |
| `engine/` | `ts_audio_replace.rs` | **Streaming TS audio ES replacer.** `TsAudioReplacer` runs inside SRT/UDP/RTP outputs when `audio_encode` is set. Learns PAT → PMT → audio PID, buffers audio PES across TS packets, decodes AAC-LC via `bilbycast-fdk-aac-rs`, runs optional `engine::audio_transcode::PlanarAudioTranscoder` (shuffle / SRC / bit-depth) when `transcode` is set, re-encodes to the target codec (`aac_lc` / `he_aac_v1` / `he_aac_v2` / `mp2` / `ac3`), repacketises into fresh TS, and rewrites the PMT stream_type (with recomputed CRC) when the codec family differs. Video / PAT / null / other PIDs pass through verbatim. Synchronous — driven from `tokio::task::block_in_place`. Unit-tested |
| `engine/` | `ts_video_replace.rs` | **Streaming TS video ES replacer (Phase 4 MVP).** `TsVideoReplacer` runs inside SRT/UDP/RTP outputs when `video_encode` is set. Same shape as `TsAudioReplacer` but operates on the video PID: `video-engine::VideoDecoder` → plane extract → `video-engine::VideoEncoder` (x264 / x265 / NVENC, feature-gated) → rebuilt video PES. Feature-gated at compile time; runtime error when the selected backend wasn't compiled in. See `docs/transcoding.md` for limitations (no scaling / fps auto-detect yet, no RTMP/HLS/WebRTC wiring yet) |
| `engine/cmaf/` | `mod.rs`, `box_writer.rs`, `codecs.rs`, `nalu.rs`, `fmp4.rs`, `manifest.rs`, `segmenter.rs`, `upload.rs`, `encode.rs`, `cenc.rs`, `cenc_boxes.rs` | **CMAF / CMAF-LL HTTP-push output.** Sibling to `output_hls.rs` but with fragmented MP4 segments. Hand-rolled ISO-BMFF box writer (no MP4 crate dependency); supports H.264 + HEVC video passthrough or re-encode, AAC audio passthrough or re-encode (`audio_encode`), HLS m3u8 + DASH MPD dual manifests, LL-CMAF chunked-transfer streaming PUT (`reqwest::Body::wrap_stream` + `mpsc(8)` with drop-on-full → never blocks broadcast subscriber), and ClearKey CENC (AES-CTR `cenc` + AES-CBC-pattern 1:9 `cbcs`) with `tenc`/`senc`/`saio`/`saiz`/`pssh` boxes plus operator-supplied Widevine/PlayReady/FairPlay PSSH passthrough. All codec work runs in `tokio::task::block_in_place`. Full reference in [`docs/cmaf.md`](docs/cmaf.md) |
| `engine/` | `input_rtsp.rs` | RTSP input: pull H.264/H.265 + AAC from IP cameras and media servers via retina crate |
| `engine/` | `input_webrtc.rs` | WebRTC input: WHIP server (receive contributions) and WHEP client (pull from server) |
| `engine/webrtc/` | `session.rs`, `ts_demux.rs`, `rtp_h264.rs`, `rtp_h264_depack.rs`, `signaling.rs` | WebRTC core: str0m session wrapper, TS demuxer, RFC 6184 H.264 packetizer/depacketizer, WHIP/WHEP HTTP signaling |
| `engine/` | `input_st2110_30.rs`, `input_st2110_31.rs`, `input_st2110_40.rs`, `output_st2110_*.rs`, `st2110_io.rs` | **SMPTE ST 2110 (Phase 1) — audio + data.** Six thin spawn modules backed by `st2110_io.rs` shared runtime helpers. ST 2110-30 (L16/L24 PCM), ST 2110-31 (AES3 transparent), ST 2110-40 (RFC 8331 ANC). Inputs validate via `engine::st2110::audio::PcmDepacketizer` / `engine::st2110::ancillary::unpack_ancillary` and republish whole `RtpPacket`s through the broadcast channel — paired in/out achieves byte-identical loopback by passthrough. SMPTE 2022-7 Red/Blue dual-network bind via `engine::st2110::redblue::RedBluePair`, optional source-IP allow-list (single-leg only). Stats publishes `PtpStateHandle` and `RedBlueStats` onto `FlowStatsAccumulator` so `snapshot()` populates `ptp_state` / `network_legs`. |
| `engine/` | `input_st2110_20.rs`, `input_st2110_23.rs`, `output_st2110_20.rs`, `output_st2110_23.rs`, `st2110_video_io.rs` | **SMPTE ST 2110 (Phase 2) — uncompressed video.** ST 2110-20 (single-stream) and ST 2110-23 (multi-stream single essence, 2SI + sample-row partitioning). Ingress: RFC 4175 depacketize → bounded mpsc → `spawn_blocking` worker running `video-engine::VideoEncoder` (x264 / x265 / NVENC, Cargo-feature-gated) → `TsMuxer` → `RtpPacket { is_raw_ts: true }`. Egress: broadcast → `TsDemuxer` → bounded mpsc → `spawn_blocking` worker running `video-engine::VideoDecoder` + `VideoScaler` (new 4:2:2 8/10-bit destination formats) → pack to pgroup bytes → `Rfc4175Packetizer` → UDP Red (+ optional Blue). Pixel formats: YCbCr-4:2:2 at 8-bit (pgroup=4 bytes) and 10-bit LE (pgroup=5 bytes). Input config **requires** a `video_encode` block; output does not. The tokio reactor is never blocked — all codec work runs on blocking workers with drop-on-lag mpsc. ST 2110-22 (JPEG XS) remains deferred pending a libjxs wrapper. |
| `engine/st2110/` | `audio.rs`, `ancillary.rs`, `sdp.rs`, `ptp.rs`, `redblue.rs`, `hwts.rs`, `scte104.rs`, `timecode.rs`, `captions.rs`, `video.rs` | ST 2110 essence helpers: PCM packetize/depacketize, RFC 8331 ANC pack/unpack with hand-rolled bit reader/writer, RFC 4566 SDP generator+parser scoped to -30/-31/-40, PTP state reporter that polls `/var/run/ptp4l` management socket via Unix datagram (best-effort, falls back to `Unavailable`), Red/Blue UDP bind around `redundancy::merger::HitlessMerger`, Linux SO_TIMESTAMPING via libc directly (no `nix` dep, macOS returns Unsupported), single-op SCTE-104 parser, SMPTE 12M-2 ATC timecode, CEA-608/708 caption detection. `video.rs` adds RFC 4175 packetizer/depacketizer, pgroup ↔ planar YUV conversion, and the ST 2110-23 partition / reassembler (2SI + sample-row). All pure Rust, all unit-tested. |
| `api/` | `nmos.rs`, `nmos_is05.rs`, `nmos_is08.rs`, `nmos_mdns.rs` | NMOS surface: IS-04 Node API with multi-essence audio / data / video formats and BCP-004 receiver caps (audio: `audio/L16`, `audio/L24`, sample_rate/channel_count/sample_depth; data: `video/smpte291`; video (ST 2110-20/-23): `video/raw` with `color_sampling=YCbCr-4:2:2`, `component_depth` ∈ {8, 10}, `frame_width`, `frame_height`, `grain_rate`); IS-05 Connection Management (ST 2110-23 inputs report the primary sub-stream leg in transport params); IS-08 audio channel mapping (`/io`, `/map/active`, `/map/staged`, `/map/activate`) with disk-persisted active map; best-effort `_nmos-node._tcp` mDNS-SD registration via the pure-Rust `mdns-sd` crate. The IS-04 `/self` resource advertises a PTP clock entry whenever any flow declares `clock_domain`. |
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
| `setup/` | `handlers.rs`, `wizard.rs` | Browser-based setup wizard for initial provisioning (inline HTML, gated by `setup_enabled` config flag). `setup_enabled` auto-flips to `false` (persisted via `persist_credentials` in `manager/client.rs`) on the first successful manager registration so `/setup` stops accepting reconfiguration post-provisioning |
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
2. **OAuth 2.0 + JWT**: client credentials grant, HS256 signing, role-based (admin vs monitor), per-IP rate limiting on `/oauth/token` (default 10 req/min, configurable via `token_rate_limit_per_minute`)
3. **Route-level RBAC**: public routes (`/health`, `/oauth/token`), read-only (any JWT), admin-only (mutations). NMOS endpoints (`nmos_require_auth`, `Option<bool>`) are **secure-by-default**: when `auth.enabled: true` and the field is unset, NMOS IS-04/IS-05/IS-08 require JWT Bearer auth. Operators opt out explicitly with `nmos_require_auth: false` (a loud `SECURITY:` warning is logged at startup). When `auth.enabled: false` NMOS stays public regardless. Read via `AuthConfig::nmos_require_auth_effective()`, never the raw field
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
- NMOS IS-04 (public by default, optionally auth-protected): `/x-nmos/node/v1.3/` — Node, Device, Source, Flow, Sender, Receiver resources. ST 2110-30/-31 inputs report `urn:x-nmos:format:audio`; ST 2110-40 inputs report `urn:x-nmos:format:data`. Receiver caps include BCP-004 constraint sets for ST 2110 audio (sample_rate / channel_count / sample_depth)
- NMOS IS-05 (public by default, optionally auth-protected): `/x-nmos/connection/v1.1/` — Staged/active transport parameter management for senders and receivers
- NMOS IS-08 (public by default, optionally auth-protected): `/x-nmos/channelmapping/v1.0/` — Audio channel mapping (`io`, `map/active`, `map/staged`, `map/activate`). Active map persists to `<config_dir>/nmos_channel_map.json`
- NMOS endpoints require JWT Bearer auth by default whenever `auth.enabled: true`. Set `nmos_require_auth: false` to explicitly opt out (logs a `SECURITY:` warning). Both `admin` and `monitor` roles have access when auth is active
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

See the root `CLAUDE.md` "Key Design Constraints (Apply Everywhere)" section for the full list. Edge-specific restatements:

- **Never block the input task** — output lag must not propagate upstream
- **Stopping a flow cancels all children; stopping one output leaves others running** (hierarchical `CancellationToken`)
