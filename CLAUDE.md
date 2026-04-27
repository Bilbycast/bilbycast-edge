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

See the root `CLAUDE.md` feature flag table for the canonical list, defaults, and build prerequisites. Default `cargo build` produces an AGPL-only binary with no software video encoders; `video-encoder-x264` / `-x265` / `-nvenc` / `-qsv` are opt-in (GPL or vendor-specific) and bundled by the `video-encoders-full` composite. The GitHub Actions release workflow ships two binary variants per architecture — a default (AGPL-only) and a full (AGPL + GPL bundle) — and the `*-x86_64-linux-full` variant additionally bundles QSV (Intel oneVPL); the `*-aarch64-linux-full` variant omits QSV because Intel iGPU is x86_64-only. See `docs/installation.md` for the release channel reference and `docs/transcoding.md` for the full transcoding reference.

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

At runtime, each flow has one or more inputs and N outputs connected via a `tokio::broadcast::channel(2048)`. The `FlowManager` (`src/engine/manager.rs`) holds all active flows in a `DashMap<FlowId, Arc<FlowRuntime>>`. Hot-add/remove of outputs, backpressure rules, and fan-out behavior are unchanged.

**Two modes of runtime operation**, selected per-flow by `FlowConfig.assembly`:

- **Passthrough** (default; `assembly = null` or `kind = passthrough`): at most one input is active at a time, publishing its bytes onto `broadcast_tx`. Input switching is seamless via `TsContinuityFixer`. This is the legacy behaviour every flow had before PID-bus support landed.
- **Assembled** (`kind = spts | mpts`): every referenced input runs concurrently and publishes its ES onto a per-flow `FlowEsBus` (keyed by `(input_id, source_pid)`) via `TsEsDemuxer`. An assembler task subscribes to the selected slots, synthesises a fresh PAT + per-program PMT (versions bump mod 32), and publishes the assembled TS onto the same `broadcast_tx`. Every output type consumes it identically — no output-type gate. Hot-swap via `UpdateFlowAssembly`. See `docs/architecture.md` ("PID bus").

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
        ├─ Replay Recorder (independent subscriber, drop-on-Lagged → bounded mpsc → writer task; flow-level `recording` attribute, `replay` Cargo feature)
        ├─ CancellationToken (hierarchical: parent → children)
        └─ StatsAccumulator (AtomicU64 counters, lock-free)
```

**Backpressure rule**: Slow outputs receive `RecvError::Lagged(n)` and increment `packets_dropped`. Input is **never** blocked. No cascading backpressure across outputs.

### The RtpPacket Abstraction (`src/engine/packet.rs`)

All data flows through a single type: `RtpPacket { data: Bytes, sequence_number: u16, rtp_timestamp: u32, recv_time_us: u64, is_raw_ts: bool }`. Inputs produce these, outputs consume them. The `is_raw_ts` flag distinguishes RTP-wrapped TS from raw TS (e.g., from OBS/srt-live-transmit).

### Module Responsibilities

| Module | Key Files | Purpose |
|--------|-----------|---------|
| `engine/` | `manager.rs`, `flow.rs`, `packet.rs` | Flow lifecycle, FlowRuntime bring-up/teardown. `create_flow()` and `FlowRuntime::start()` accept `ResolvedFlow` (not raw config references). `FlowManager::start_flow_group` / `stop_flow_group` provide all-or-nothing parallel group lifecycle — rolls back started members on partial failure |
| `engine/` | `input_rtp.rs`, `input_udp.rs`, `input_srt.rs`, `input_rist.rs`, `input_rtmp.rs` | Protocol-specific input tasks. `input_rist.rs` wraps `rist-transport::RistSocket::receiver` and publishes the delivered RTP payload as `RtpPacket { is_raw_ts: true }`. Optional SMPTE 2022-7 dual-leg merge via the shared `HitlessMerger`. |
| `engine/` | `delay_buffer.rs` | Per-output delay buffer for stream synchronization. `DelayBuffer` queues packets and releases them after a configurable delay. `resolve_output_delay()` is the entry point that resolves an `OutputDelay` config into a `DelayBuffer`, handling the async frame-rate wait for `TargetFrames` mode. Non-blocking, zero-copy (moves only the `RtpPacket` struct, not the `Bytes` data). Used to align parallel outputs with different processing latencies (e.g., clean feed vs. commentary-processed feed). |
| `engine/` | `output_rtp.rs`, `output_udp.rs`, `output_srt.rs`, `output_rist.rs`, `output_rtmp.rs`, `output_hls.rs`, `output_webrtc.rs`, `cmaf/` (see row below) | Protocol-specific output tasks. Every variant accepts optional `program_number: u16` — TS-native outputs (UDP/RTP/SRT/HLS) run `TsProgramFilter` inline (FEC/2022-7 operate on filtered bytes); re-muxing outputs (RTMP/WebRTC) use `TsDemuxer::new(target_program)` (lowest `program_number` in the PAT is the deterministic default when unset). SRT/RIST/UDP/RTP support optional `delay: OutputDelay` (fixed ms, target end-to-end latency ms, or target video frames). A dedicated `rtp_audio` output carries LPCM over RTP; SRT/UDP/`rtp_audio` accept `transport_mode: "audio_302m"` for SMPTE 302M-in-TS (interoperable with `ffmpeg -c:a s302m`, `srt-live-transmit`, broadcast hardware). **Audio transcoding (Phase B):** RTMP/HLS/WebRTC/SRT/RIST/UDP/RTP accept optional `audio_encode` (`aac_lc`/`he_aac_v1`/`he_aac_v2`/`opus`/`mp2`/`ac3` per codec×output matrix) + companion `transcode` block (channel shuffle / SRC / bit-depth via `engine::audio_transcode::PlanarAudioTranscoder`). TS outputs use `engine::ts_audio_replace::TsAudioReplacer` to rewrite the audio ES + PMT stream_type in place; RTMP/HLS/WebRTC replace audio in the output path. `transcode` wins over `audio_encode` on conflicting fields. ST 2110-31 (AES3) is excluded. **Video transcoding (Phase 4):** SRT/RIST/UDP/RTP/RTMP/WebRTC accept optional `video_encode` (libx264/libx265/NVENC, Cargo-feature-gated). TS-carrying outputs use `engine::ts_video_replace::TsVideoReplacer`; RTMP drives `engine::output_rtmp::VideoEncoderState`; WebRTC drives `engine::output_webrtc::WebrtcVideoEncoderState`. **Key constraints**: WebRTC is H.264-only (validation rejects `x265`/`hevc_nvenc`); HLS `video_encode` still deferred; CMAF supports `video_encode` natively (segmenter forces GoP alignment). **Default** (`video-thumbnail` + `fdk-aac`): in-process AAC (fdk-aac) + Opus/MP2/AC-3 (libavcodec); no external ffmpeg. **Fallback** (features off): ffmpeg subprocess for audio; `video_encode` refuses to start. Outputs without `audio_encode`/`video_encode` need no codec libraries. Full transcoding reference: [`docs/transcoding.md`](docs/transcoding.md). Audio gateway + `rtp_audio` / `audio_302m` reference: [`docs/audio-gateway.md`](docs/audio-gateway.md). |
| `engine/rtmp/` | `server.rs`, `amf0.rs`, `chunk.rs`, `ts_mux.rs` | RTMP protocol internals, FLV→MPEG-TS. `ts_mux.rs` is the shared TS muxer (PAT/PMT/PES) used by RTMP, RTSP, and WebRTC inputs. PAT/PMT `section_length` must include the 4-byte CRC32 per MPEG-TS spec. Inputs using TsMuxer must bundle TS packets per-frame (not per-188-byte packet) to avoid broadcast channel pressure |
| `engine/` | `tr101290.rs` | Transport stream quality analysis (sync, CC, PAT/PMT, PCR) |
| `engine/` | `media_analysis.rs` | Media content detection (codec, resolution, frame rate, audio format, per-PID bitrate) |
| `engine/content_analysis/` | `mod.rs`, `lite.rs`, `audio_full.rs`, `video_full.rs`, `bitreader.rs`, plus per-check helpers (`gop.rs`, `signalling.rs`, `timecode.rs`, `captions.rs`, `scte35.rs`, `mdi.rs`) | **In-depth content analysis.** Three per-flow opt-in tiers gated on `FlowConfig.content_analysis` (`lite`, `audio_full`, `video_full`). Each tier is a **dedicated broadcast subscriber** — drop-on-`Lagged`, no feedback into the data path. **Lite** (default on, <1 % CPU): GOP IDR cadence from NAL type 5 (H.264) / 16–21 (H.265) / GOP-start (MPEG-2); full H.264 + H.265 SPS/VUI decode for aspect ratio (SAR→DAR math, 16:9 / 4:3 / 21:9 snapping), colour primaries / transfer / matrix / range, HDR family, MaxFALL / MaxCLL from SEI 144, and AFD from ATSC A/53 user-data; SMPTE timecode from H.264/H.265 `pic_timing` SEI (payload type 1); CEA-608/708 caption presence via SEI `user_data_registered_itu_t_t35` + ATSC `GA94`; SCTE-35 cue decode (splice_insert + time_signal PTS); Media Delivery Index (RFC 4445 — NDF from peak-IAT spread in the current 1 s window, MLR from TS CC discontinuities). Shared Exp-Golomb bit reader + RBSP emulation-prevention stripper in `bitreader.rs`. **Audio Full** (opt-in): three ingress paths share the same R128 + true-peak + mute + clip pipeline. `ts` — per-PID ADTS AAC framing in-task → `engine::audio_decode::AacDecoder` (fdk-aac) → planar f32 PCM → `ebur128` crate (I/M/S + LRA + TRUE_PEAK). `pcm` — ST 2110-30 PM/AM (L16/L24) and generic RtpAudio depacketize directly off the broadcast channel; no decoder. `aes3` — ST 2110-31 32-bit AES3 subframe extraction (24-bit audio bits 27..4, V/U/C/P stripped). Hard-mute (2000 consecutive zero samples), clipping (samples ≥ 0.9975), silence (LUFS-M ≤ -60 for ≥ 2 s). MP2 / AC-3 / E-AC-3 *inside MPEG-TS* are tracked with `codec_decoded: false` + structured `decode_note` — R128 on those codecs waits on the libavcodec audio-decode bridge. **Video Full** (opt-in, `video_full_hz` cadence override, default 1 Hz, clamped to `(0.0, 30.0]`): one decoded frame per sample tick via `video_engine::VideoDecoder` under `tokio::task::block_in_place` (H.264 / H.265). Pure-Rust pixel metrics on the decoded Y plane — YUV-SAD freeze against previous frame, 3×3 Laplacian-variance blur, 8×8 boundary-gradient blockiness, letterbox / pillarbox row+column black-edge detection, SMPTE-bars column-uniformity heuristic, slate (freeze + mid-brightness). All events carry structured `details.error_code` matching the category — see [`docs/events-and-alarms.md`](docs/events-and-alarms.md#content-analysis-events). Wire shape: [`docs/metrics.md`](docs/metrics.md#content-analysis-metrics-phase-1-3). Configuration schema: [`docs/configuration-guide.md`](docs/configuration-guide.md#content-analysis-in-depth). |
| `engine/` | `thumbnail.rs` | Optional per-flow thumbnail generation. **Default (`video-thumbnail` feature):** in-process via `video-engine` (FFmpeg libavcodec/libswscale) — extracts video NAL units from buffered TS, decodes one frame, scales to 320x180, encodes JPEG, computes luminance for black-screen detection, all without spawning a subprocess. **Fallback (feature disabled):** ffmpeg subprocess. Buffers ~3s of TS packets, generates every 10s. Gated by `FlowConfig.thumbnail` + availability. Thumbnails sent to manager as `thumbnail` WS messages |
| `engine/` | `ts_parse.rs` | Shared MPEG-TS packet parsing helpers (used by tr101290, media_analysis, and thumbnail). Includes `parse_pat_programs` which returns `(program_number, pmt_pid)` pairs, reused by the program filter and the demuxer. |
| `engine/` | `ts_continuity_fixer.rs` | **TS continuity fixer for seamless input switching.** `TsContinuityFixer` sits in the input forwarder (shared `Arc<Mutex>` per flow across all per-input forwarders). On switch: (1) clears output-side CC state so receivers resync via natural packet-loss recovery; (2) injects the new input's cached PAT/PMT with `version_number` drawn from a **per-fixer monotonic counter** (CRC32 recalculated) — counter advances on every switch including to dead inputs, so consecutive switches always emit a strictly-different version stamp (critical — naive `cached_version+1` produces phantom `version=1` collisions and locks receiver audio decoders on the wrong format after an `A→B→A` round-trip); (3) forwards all packets immediately. Per-input PSI cache; passive inputs pre-warm via `observe_passive()`. Forwarder emits 250 ms-interval NULL-PID (0x1FFF) TS padding while the active input is silent (keeps downstream UDP sockets and decoder state alive). Fully format-agnostic — any video/audio/data codec, any container, any transport. Non-TS transports (raw ST 2110 RTP) bypass the fixer. Zero-cost before first switch. Unit-tested for CC passthrough, monotonic PSI stamping (including dead-input counter advance + 32-value wrap), PAT/PMT injection, RTP-wrapped TS. Full rationale: [`docs/architecture.md`](docs/architecture.md). |
| `engine/` | `ts_program_filter.rs` | **MPTS → SPTS filter.** `TsProgramFilter` consumes raw TS and emits only the packets belonging to a target program, rewriting the PAT into a single-program form (CRC via `mpeg2_crc32`). Used by UDP/RTP/SRT/HLS outputs and the thumbnail generator when `program_number` (or `thumbnail_program_number`) is set. Unit-tested for MPTS down-selection, synthetic PAT validity, and PAT version-bump handling |
| `engine/` | `ts_audio_replace.rs` | **Streaming TS audio ES replacer.** Used by SRT/UDP/RTP outputs when `audio_encode` is set. Learns PAT → PMT → audio PID, buffers audio PES across TS packets, decodes AAC-LC via fdk-aac, optionally runs `engine::audio_transcode::PlanarAudioTranscoder` when `transcode` is set, re-encodes to the target codec, repacketises into fresh TS, rewrites the PMT stream_type (CRC recomputed) on codec-family change. Video / PAT / null / other PIDs pass through. `tokio::task::block_in_place`. Unit-tested. Full reference: [`docs/transcoding.md`](docs/transcoding.md). |
| `engine/` | `ts_es_bus.rs`, `ts_es_hitless.rs`, `ts_assembler.rs`, `ts_es_analysis.rs`, `input_pcm_encode.rs` | **PID bus / Flow Assembly — SPTS/MPTS synthesis from N inputs.** `FlowEsBus` is a per-flow `DashMap<(input_id, source_pid), broadcast::Sender<EsPacket>>` that every referenced TS-carrying input publishes onto via `TsEsDemuxer`. `ts_assembler::spawn_spts_assembler` subscribes to the bus per slot, rewrites each 188-byte packet's PID to the configured `out_pid` (with per-PID monotonic CC), bundles 7 × 188 B into 1316 B RTP packets, synthesises fresh PAT + per-program PMT on a 100 ms cadence (versions mod 32, monotonic), and publishes onto the flow's existing `broadcast_tx` — every output type (UDP, RTP, SRT, RIST, HLS, CMAF, RTMP, WebRTC) consumes the assembled TS identically to a passthrough flow. Hot-swap via `PlanCommand::ReplacePlan` (`UpdateFlowAssembly` WS command / REST mirror). `ts_es_hitless::spawn_hitless_es_merger` implements pre-bus Hitless failover (primary-preference, 200 ms stall timer — **not** 2022-7 seq-aware dedup, `EsPacket` carries no upstream RTP seq today). `ts_es_analysis::spawn_per_es_analyzer` is a lightweight per-bus-key stats task producing `FlowStats.per_es[]` (packets/bytes/bitrate/CC/PCR-disc, annotated with current `out_pid`). `input_pcm_encode` is the decoded-ES cache that turns PCM/AES3 inputs (ST 2110-30, ST 2110-31, `rtp_audio`) into TS carriers via input-level `audio_encode` (`aac_lc` / `he_aac_v1` / `he_aac_v2` / `s302m`). Error codes: see `docs/events-and-alarms.md` ("PID bus / Flow Assembly"). Full reference: [`docs/architecture.md`](docs/architecture.md) ("PID bus"), [`docs/configuration-guide.md`](docs/configuration-guide.md) ("Flow Assembly (PID bus)"). |
| `stats/` | `pcr_trust.rs` | **Per-output PCR accuracy sampler.** Fixed-size rotating reservoir (4096 samples) on every TS-bearing output's send path. Records `|ΔPCR_µs − Δwall_µs|` on successful `socket.send_to` of PCR-bearing TS packets. Sample-skip rule: Δ > 500 ms in either direction discards state (filters startup jitter, keyframe gaps, stream restarts, 33-bit PCR wrap). Exposes exact p50/p95/p99/max on snapshot; `FlowStats.pcr_trust_flow` aggregates across outputs. Wired into `output_udp.rs` (MPTS UDP) and `output_rtp.rs` (raw-TS + RTP-wrapped TS). |
| `engine/` | `ts_video_replace.rs` | **Streaming TS video ES replacer (Phase 4 MVP).** Used by SRT/UDP/RTP outputs when `video_encode` is set. Same shape as `TsAudioReplacer` but on the video PID: `VideoDecoder` → `VideoEncoder` (x264/x265/NVENC, feature-gated) → rebuilt video PES. Runtime error if the selected backend wasn't compiled in. Limitations + roadmap in [`docs/transcoding.md`](docs/transcoding.md). |
| `engine/cmaf/` | `mod.rs`, `box_writer.rs`, `codecs.rs`, `nalu.rs`, `fmp4.rs`, `manifest.rs`, `segmenter.rs`, `upload.rs`, `encode.rs`, `cenc.rs`, `cenc_boxes.rs` | **CMAF / CMAF-LL HTTP-push output.** Fragmented-MP4 sibling to `output_hls.rs`. Hand-rolled ISO-BMFF box writer (no MP4 crate). H.264 + HEVC passthrough or re-encode via `video_encode` (segmenter forces GoP alignment), AAC passthrough or re-encode via `audio_encode`. HLS m3u8 + DASH MPD dual manifests, LL-CMAF chunked-transfer PUT (`reqwest::Body::wrap_stream` + `mpsc(8)` drop-on-full → never blocks broadcast subscriber). ClearKey CENC: AES-CTR `cenc` + AES-CBC-pattern 1:9 `cbcs` with `tenc`/`senc`/`saio`/`saiz`/`pssh` boxes; operator-supplied Widevine/PlayReady/FairPlay PSSH passthrough. All codec work in `tokio::task::block_in_place`. Full reference: [`docs/cmaf.md`](docs/cmaf.md). |
| `engine/` | `input_rtsp.rs` | RTSP input: pull H.264/H.265 + AAC from IP cameras and media servers via retina crate |
| `engine/` | `input_webrtc.rs` | WebRTC input: WHIP server (receive contributions) and WHEP client (pull from server) |
| `engine/` | `input_media_player.rs` | **File-backed media player input.** Replays a playlist of local assets (TS / MP4 / MOV / MKV / still image) as a paced fresh MPEG-TS on the flow's broadcast channel. Sources resolve against the edge's media library (`media::media_dir()`, governed by `BILBYCAST_MEDIA_DIR` → `$XDG_DATA_HOME/bilbycast/media/` → `$HOME/.bilbycast/media/` → `./media/`). `loop_playback`, `shuffle`, optional `paced_bitrate_bps` (TS-only override, 100 kbps – 200 Mbps). Image sources render at `fps` (1–60, default 5) at `bitrate_kbps` (50–50 000, default 250) with optional silent stereo AAC pairing. Lifecycle events ride the `flow` category — `info` start, `critical` source-failed (engine sleeps 2 s and advances), `info` playlist-exhausted on non-loop tail. Designed to feed a PID-bus Hitless leg as an automatic fallback to a live primary. |
| `media/` | `mod.rs` | **Media library on disk.** Backs `input_media_player`. Resolves the library directory (`media_dir()`), enforces filename rules (alphanumeric + `._- `, 1–255 chars, no leading dot, no path separators), holds the upload limits (`MAX_FILE_BYTES = 4 GiB`, `MAX_TOTAL_BYTES = 16 GiB`, `STAGING_TTL = 1 h`), and implements the chunked-upload state machine — staging files live at `<media_dir>/.tmp/<name>.<session_id>` and are atomically renamed onto `<media_dir>/<name>` on the final chunk (which also `fsync_all`s). WS commands wired in `manager/client.rs`: `list_media`, `upload_media_chunk`, `delete_media`. |
| `replay/` | `mod.rs`, `writer.rs`, `reader.rs`, `index.rs`, `clips.rs`, `paced_replayer.rs` | **Replay server (Phase 1 + 1.5).** Gated by the `replay` Cargo feature (default on, pure-Rust, no new C deps). Flow-level `RecordingConfig { enabled, storage_id, segment_seconds (2–60, default 10), retention_seconds (default 86400), max_bytes (default 50 GiB), pre_buffer_seconds (Option<u32>, [1, 300] when set — auto-arms in `PreBuffer` mode so a later operator Start picks up the last N seconds of pre-roll; `RecordingStats.armed` stays `false` while in pre-buffer) }` rolls 188 B-aligned MPEG-TS segments to `<replay_root>/<recording_id>/{NNNNNN.ts, recording.json, index.bin (24 B / IDR), clips.json, .tmp/}`. Storage root: `BILBYCAST_REPLAY_DIR` → XDG → `$HOME/.bilbycast/replay/` → `./replay/`. Writer is a sibling broadcast subscriber (drop-on-`Lagged`, bounded mpsc → writer task — never blocks the data path); atomic segment rename + `sync_all` on roll; oldest-first prune by mtime + total bytes (the just-finalized segment id is **never** pruned — a too-tight `max_bytes` fires the Warning `replay_max_bytes_below_segment` instead of corrupting the live edge). `index.bin` is append-only (24 B / IDR: `pts_90khz`, `segment_id`, `byte_offset`, `flags` — `IS_IDR` / `PCR_DISCONTINUITY` / `SMPTE_TC_VALID`; the writer flags `PCR_DISCONTINUITY` on the next IDR after a > 5 min PCR step so the reader skips wallclock-pacing across a stream-source change); reader uses binary search + `paced_replayer::PacedReplayer` for forward 1.0× IDR-snapped playback. **Crash recovery** runs on writer init: scan `<recording_id>/.tmp/` and unlink any `<NNNNNN>.ts` orphans, derive the resume segment id from `max(<NNNNNN>.ts on disk) + 1` (so a stale or corrupt `recording.json` never causes id reuse), align `index.bin` down to the last 24-byte boundary if a SIGKILL truncated a partial entry. The `replay_recovery_alert` Warning event surfaces `tmp_orphans_removed`, `meta_corrupt`, `next_segment_id` so the operator sees what was repaired. **Disk-full** path closes the partial segment (best-effort fsync + rename out of `.tmp/`, falling back to `unlink` on ENOSPC) before nulling the segment slot, so the writer task never holds an orphaned file handle. **Meta-write** failures emit `replay_metadata_stale` instead of swallowing the error. Companion: `engine/input_replay.rs` (replay input task, per-input command channel, `playback_started/stopped/eof` events; `play_clip` / `scrub_playback` reject inverted ranges with `replay_invalid_range`). WS handlers in `manager/client.rs` (`start_recording` arm at line 2400, `scrub_playback` arm at line 2910), all `#[cfg(feature = "replay")]`-gated. **All 14 commands**: `start_recording`, `stop_recording`, `mark_in`, `mark_out`, `list_clips` (accepts `flow_id` or `recording_id` for orphan recovery), `get_clip`, `delete_clip`, `rename_clip` (legacy — name + description), **`update_clip`** (Phase 2 / 1.5 — superset: name + description + `tags: Vec<String>` + `in_pts_90khz` / `out_pts_90khz` trim, all optional, at least one required; SMPTE TC cleared on PTS trim because the IDR index doesn't carry SMPTE strings), `recording_status` (returns `armed`, **`mode`** ("armed" / "pre_buffer" / "idle" — Phase 2 / 1.5 tri-state discriminator that drives the manager's `Pre-roll` badge), `recording_id`, `current_pts_90khz`, `segments_written`, `bytes_written`, `segments_pruned`, `packets_dropped`, `index_entries`, `max_bytes`, `replay_root_free_bytes`, `replay_root_total_bytes`), `cue_clip`, `play_clip`, `stop_playback`, `scrub_playback`. Clip name (≤ 256, no control chars) and description (≤ 4096) length-validated at the dispatcher with `replay_invalid_field`. **Tag bounds** (Phase 2 / 1.5): each tag matches `^[A-Z0-9_-]{1,32}$`, ≤ 16 tags per clip, dedup'd server-side; violation lifts `replay_invalid_tag` onto `command_ack.error_code`. Edge stores whatever the manager sends (no v1 enum membership check) so per-group customisation later doesn't need an edge release. Capability advertised on `HealthPayload.capabilities` so missing-feature edges return `unknown_action` gracefully. Full reference: [`docs/replay.md`](docs/replay.md). |
| `engine/` | `input_replay.rs` | Replay input task. Subscribes to a per-input `tokio::mpsc<ReplayCommand>` (Cue/Play/Stop/Scrub) and reads off a `replay::reader::Reader` paced by `paced_replayer::PacedReplayer`. Publishes onto the flow's broadcast channel like any other input. Idle state pads NULL-PID 0x1FFF every 250 ms to keep downstream sockets and decoders warm. |
| `engine/webrtc/` | `session.rs`, `ts_demux.rs`, `rtp_h264.rs`, `rtp_h264_depack.rs`, `signaling.rs` | WebRTC core: str0m session wrapper, TS demuxer, RFC 6184 H.264 packetizer/depacketizer, WHIP/WHEP HTTP signaling |
| `engine/` | `input_st2110_30.rs`, `input_st2110_31.rs`, `input_st2110_40.rs`, `output_st2110_*.rs`, `st2110_io.rs` | **SMPTE ST 2110 Phase 1 — audio + data.** Six thin spawn modules over shared `st2110_io.rs`. ST 2110-30 (L16/L24 PCM), -31 (AES3 transparent), -40 (RFC 8331 ANC). Inputs validate via `PcmDepacketizer` / `unpack_ancillary` and republish whole `RtpPacket`s — paired in/out achieves byte-identical loopback by passthrough. 2022-7 Red/Blue via `engine::st2110::redblue::RedBluePair`, optional source-IP allow-list (single-leg only). Publishes `PtpStateHandle` + `RedBlueStats` onto `FlowStatsAccumulator`. Full reference: [`docs/st2110.md`](docs/st2110.md). |
| `engine/` | `input_st2110_20.rs`, `input_st2110_23.rs`, `output_st2110_20.rs`, `output_st2110_23.rs`, `st2110_video_io.rs` | **SMPTE ST 2110 Phase 2 — uncompressed video** (-20 single-stream, -23 multi-stream with 2SI + sample-row partition). Ingress: RFC 4175 depacketize → bounded mpsc → `spawn_blocking` worker (`video-engine::VideoEncoder`, x264/x265/NVENC, feature-gated) → `TsMuxer` → `RtpPacket { is_raw_ts: true }`. Egress: broadcast → `TsDemuxer` → `spawn_blocking` worker (`VideoDecoder` + `VideoScaler` at 4:2:2 8/10-bit destination) → `Rfc4175Packetizer` → UDP Red (+ optional Blue). Pixel formats: YCbCr-4:2:2 8-bit (pgroup=4) and 10-bit LE (pgroup=5). Input **requires** `video_encode`; output does not. Tokio reactor is never blocked. ST 2110-22 (JPEG XS) deferred pending libjxs wrapper. |
| `engine/st2110/` | `audio.rs`, `ancillary.rs`, `sdp.rs`, `ptp.rs`, `redblue.rs`, `hwts.rs`, `scte104.rs`, `timecode.rs`, `captions.rs`, `video.rs` | ST 2110 essence helpers: PCM pack/depack, RFC 8331 ANC, RFC 4566 SDP gen+parse (-30/-31/-40), PTP state via Unix datagram to `/var/run/ptp4l`, Red/Blue around `redundancy::merger::HitlessMerger`, Linux SO_TIMESTAMPING (no `nix` dep, macOS → Unsupported), SCTE-104 parser, SMPTE 12M-2 ATC timecode, CEA-608/708 caption detection. `video.rs`: RFC 4175 pack/depack, pgroup ↔ planar YUV, ST 2110-23 partition/reassembler. Pure Rust, unit-tested. |
| `api/` | `nmos.rs`, `nmos_is05.rs`, `nmos_is08.rs`, `nmos_mdns.rs` | NMOS surface: IS-04 Node API (multi-essence audio/data/video + BCP-004 receiver caps), IS-05 Connection Management (ST 2110-23 inputs report primary sub-stream leg), IS-08 audio channel mapping (disk-persisted active map), best-effort `_nmos-node._tcp` mDNS-SD via `mdns-sd`. IS-04 `/self` advertises a PTP clock entry when any flow declares `clock_domain`. Full reference: [`docs/nmos.md`](docs/nmos.md). |
| `api/` | `webrtc.rs` | WHIP/WHEP HTTP endpoint handlers and session registry. **Important:** When a WebRTC/WHIP flow is created, its `whip_session_tx` (stored in `FlowRuntime`) must be registered with `WebrtcSessionRegistry` via `register_whip_input()` — this wiring happens in `main.rs`, `api/flows.rs`, and `manager/client.rs` after every `create_flow` call |
| `fec/` | `encoder.rs`, `decoder.rs`, `matrix.rs` | SMPTE 2022-1 FEC (XOR column×row) |
| `redundancy/` | `merger.rs` | SMPTE 2022-7 hitless merge (seq dedup from dual RTP or SRT legs) |
| `api/` | `inputs.rs`, `outputs.rs` | CRUD endpoints for independent top-level inputs and outputs |
| `api/` | `flows.rs` | Flow CRUD. Resolves `input_ids`/`output_ids` references via `AppConfig::resolve_flow()` before engine calls. `add_output` takes an `output_id` (not inline config) and assigns it; `remove_output` unassigns an output from the flow |
| `api/` | `server.rs`, `auth.rs`, `stats.rs`, `tunnels.rs`, `ws.rs`, `nmos.rs`, `nmos_is05.rs` | Axum REST API, OAuth2/JWT, WebSocket stats, NMOS IS-04 Node API, NMOS IS-05 Connection Management |
| `config/` | `models.rs`, `validation.rs`, `persistence.rs`, `secrets.rs` | JSON config (version 2), enum-tagged types, atomic save, secrets split (`config.json` + `secrets.json`). `models.rs` defines `InputDefinition`, `OutputConfig` (top-level), `FlowConfig` (with `input_ids`/`output_ids` references), `ResolvedFlow`, and the `AppConfig::resolve_flow()` method |
| `stats/` | `collector.rs`, `models.rs`, `throughput.rs` | Lock-free stats registry, bitrate estimation |
| `tunnel/` | `manager.rs`, `relay_client.rs`, `udp_forwarder.rs`, `tcp_forwarder.rs`, `crypto.rs`, `auth.rs` | QUIC-based IP tunnels (relay/direct), end-to-end encryption, HMAC-SHA256 bind authentication. `protocol.rs` defines `TUNNEL_PROTOCOL_VERSION`, `ParsedMessage<T>` + `read_message_resilient()` for graceful handling of unknown message types, and `Hello`/`HelloAck` handshake for version detection |
| `manager/` | `client.rs`, `config.rs`, `events.rs` | WebSocket client to bilbycast-manager. Sends stats (1s), health (15s), thumbnails (10s), operational events. Auth payload carries `protocol_version` + `software_version`. Handles get_config, flow CRUD + start/stop, input/output CRUD, add/remove output, tunnel CRUD, `update_flow` / `update_config` (both diff-based — `PartialEq` old vs new, flows only restart when input/metadata changes; output-only changes apply via hot-add/remove without disrupting other outputs or SRT connections). **Key behaviours:** `GetConfig` runs `strip_secrets()` so the manager never receives `node_secret`, tunnel keys, or SRT passphrases; `UpdateConfig` merges the incoming (secret-less) config with existing local secrets before applying. Event reference: [`docs/events-and-alarms.md`](docs/events-and-alarms.md). |
| `monitor/` | `server.rs`, `dashboard.rs` | Embedded HTML/JS dashboard on separate port |
| `setup/` | `handlers.rs`, `wizard.rs` | Browser-based setup wizard for initial provisioning (inline HTML, gated by `setup_enabled` config flag). `setup_enabled` auto-flips to `false` (persisted via `persist_credentials` in `manager/client.rs`) on the first successful manager registration so `/setup` stops accepting reconfiguration post-provisioning. **Auth gate:** `POST /setup` from a non-loopback address requires `Authorization: Bearer <setup_token>`. The token is auto-generated on first boot (256-bit OS RNG, hex-encoded), persisted encrypted in `secrets.json`, printed once to stdout, and cleared alongside `setup_enabled` on first manager registration. Loopback callers (`127.0.0.0/8`, `::1`) bypass the check (operator on the box has authenticated by other means). Re-print via `bilbycast-edge --print-setup-token` |
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

- Public: `/health`, `/oauth/token`, `/metrics`, `/setup` (gated by `setup_enabled` + a one-shot `setup_token` bearer check on non-loopback callers)
- Read-only (JWT or auth-disabled): `GET /api/v1/*`
- Admin (requires `admin` role): `POST/PUT/DELETE /api/v1/*`
- **Inputs**: `GET /api/v1/inputs` (list), `POST /api/v1/inputs` (create), `GET /api/v1/inputs/{id}`, `PUT /api/v1/inputs/{id}`, `DELETE /api/v1/inputs/{id}`
- **Outputs**: `GET /api/v1/outputs` (list), `POST /api/v1/outputs` (create), `GET /api/v1/outputs/{id}`, `PUT /api/v1/outputs/{id}`, `DELETE /api/v1/outputs/{id}`
- **Flow assembly**: `PUT /api/v1/flows/{flow_id}/assembly` — body is a `FlowAssembly` JSON (same shape as `FlowConfig.assembly`). Mirrors the manager's `update_flow_assembly` WS command. No-op if the incoming assembly deserialises byte-equal to the current; otherwise calls `FlowRuntime::replace_assembly` which validates + resolves Essence + re-spawns Hitless mergers + sends `PlanCommand::ReplacePlan` to the assembler and persists to `config.json` after the swap succeeds. Transitions across the passthrough boundary are rejected.
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
