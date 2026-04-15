# Transcoding reference — audio_encode + video_encode

This document is the canonical reference for the `audio_encode` and
`video_encode` output blocks in `bilbycast-edge`, plus a running record
of the known limitations and work deferred for later phases. When
planning follow-up work, start here.

---

## Output × block support matrix

Legend: ✅ = wired and tested, ⏳ = planned (tracked below), ❌ = not
applicable / by design.

| Output     | `audio_encode` | `video_encode` | Notes |
|------------|:--------------:|:--------------:|-------|
| **SRT**    | ✅              | ✅              | Both transforms stack; forces raw-TS egress when either is set. |
| **UDP**    | ✅              | ✅              | Same as SRT. |
| **RTP**    | ✅              | ✅              | Strips source RTP framing, rewraps with fresh RFC 2250 headers. |
| **RTMP**   | ✅              | ⏳              | Audio only today (Phase B). Video re-encode wiring is Phase 4d. |
| **HLS**    | ✅              | ⏳              | Audio only today (per-segment remux). Video re-encode wiring is Phase 4d. |
| **WebRTC** | ✅              | ⏳              | Audio (Opus) only today. H.264 re-encode wiring is Phase 4d. |
| **ST 2110-30 / -31 / `rtp_audio`** | ✅ (auto via compressed-audio bridge) | ❌ | Uncompressed audio transports; video is not carried here. |
| **ST 2110-40** | ❌ | ❌ | Ancillary data — no codec concept. |

---

## `audio_encode` — compressed-audio re-encoding

Decodes the source audio ES (AAC-LC ADTS in MPEG-TS), rescales sample
rate / channel count via the PCM pipeline, and re-encodes into the
target codec. The source AAC stream is replaced in the output TS;
video and other PIDs pass through unchanged.

### Schema

```jsonc
"audio_encode": {
  "codec": "aac_lc" | "he_aac_v1" | "he_aac_v2" | "opus" | "mp2" | "ac3",
  "bitrate_kbps": 128,   // optional; per-codec default
  "sample_rate":  48000, // optional; defaults to source
  "channels":     2      // optional; defaults to source
}
```

### Allowed codecs per output

| Output       | Allowed codecs                                     |
|--------------|----------------------------------------------------|
| RTMP         | `aac_lc`, `he_aac_v1`, `he_aac_v2`                 |
| HLS          | `aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`   |
| WebRTC       | `opus`                                             |
| **SRT / UDP / RTP (new)** | `aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3` — `opus` is rejected here because MPEG-TS has no standard Opus mapping. |

### Rejected combinations (validation bails at load time)

- `audio_encode` + `transport_mode: "audio_302m"` — the 302M path owns the TS stream already.
- `audio_encode` + SMPTE 2022-7 redundancy (RTP / SRT).
- `audio_encode` + SMPTE 2022-1 FEC encode (RTP).
- `audio_encode` + SRT FEC (`packet_filter`).

### Engine internals

- Core stage: `src/engine/ts_audio_replace.rs` (streaming `TsAudioReplacer`).
- Wiring: `output_udp.rs`, `output_rtp.rs`, `output_srt.rs` insert a
  `block_in_place` call between the program filter and the egress buffer.
- Decoder: `bilbycast-fdk-aac-rs::AacDecoder::open_adts` (Fraunhofer FDK AAC).
- Encoder: `bilbycast-fdk-aac-rs::AacEncoder` for AAC family;
  `video-engine::AudioEncoder` (libavcodec) for MP2 / AC-3. Opus uses
  libopus in the same crate; unused on TS outputs.

---

## `video_encode` — H.264 / HEVC re-encoding

**Status:** Phase 4 MVP. Active on SRT / UDP / RTP outputs in the TS
pipeline. Everything else is deferred (see below).

Decodes the source video ES (H.264 or HEVC) in-process via
`video-engine::VideoDecoder`, re-encodes via a feature-gated backend,
and muxes the new bitstream back into the output TS. When the target
codec family differs from the source (H.264 ↔ HEVC), the PMT is
rewritten in place with a recomputed CRC32; same-family transcodes
leave the PMT untouched.

### Schema

```jsonc
"video_encode": {
  "codec":       "x264" | "x265" | "h264_nvenc" | "hevc_nvenc",
  "width":       1920,       // optional — see "Limitations"
  "height":      1080,       // optional — see "Limitations"
  "fps_num":     30,         // recommended — operator-supplied, no auto-detect yet
  "fps_den":     1,
  "bitrate_kbps": 4000,      // optional, default 4000; range 100–100000
  "gop_size":    60,         // optional, default 2 × fps_num
  "preset":      "medium",   // optional, default medium; `ultrafast`..`veryslow`
  "profile":     "high"      // optional, auto if unset; `baseline` / `main` / `high`
}
```

### Backend availability

Backends are compile-time-gated via Cargo features on `bilbycast-edge`.
See the licensing notes in the main `bilbycast-edge/CLAUDE.md`:

| Backend       | Feature flag              | Library needed (Linux)       | License impact            |
|---------------|---------------------------|------------------------------|---------------------------|
| `x264`        | `video-encoder-x264`      | `apt install libx264-dev`    | **GPL v2+** — infects the built binary. |
| `x265`        | `video-encoder-x265`      | `apt install libx265-dev`    | **GPL v2+** — infects the built binary. |
| `h264_nvenc`  | `video-encoder-nvenc`     | `nv-codec-headers` + NVIDIA driver (runtime) | Royalty-free; LGPL-clean at the API layer. |
| `hevc_nvenc`  | `video-encoder-nvenc`     | same                         | same                      |

Default release build is LGPL-clean (no video encoder backends). Runtime
error `video encoder disabled: rebuild with …` surfaces when a config
targets a codec whose feature flag was not enabled at build.

```bash
# Linux — x264 opt-in build
sudo apt install libx264-dev
cargo build --release --features video-encoder-x264

# Linux — x265 opt-in build
sudo apt install libx265-dev
cargo build --release --features video-encoder-x265

# Linux — NVENC opt-in build (requires NVIDIA GPU at runtime)
sudo apt install nv-codec-headers
cargo build --release --features video-encoder-nvenc
```

### Rejected combinations

Same set as `audio_encode`:

- `video_encode` + `transport_mode: "audio_302m"`.
- `video_encode` + SMPTE 2022-7 redundancy.
- `video_encode` + SMPTE 2022-1 FEC encode (RTP).
- `video_encode` + SRT FEC (`packet_filter`).

### Engine internals

- Core stage: `src/engine/ts_video_replace.rs` (streaming `TsVideoReplacer`).
- Pipeline per decoded frame: `VideoDecoder` → `DecodedFrame::yuv_planes()` → `VideoEncoder::encode_frame(y, u, v, pts)` → fresh video PES → 188-byte TS packets.
- Wiring: `output_udp.rs`, `output_rtp.rs`, `output_srt.rs` chain
  `program_filter → audio_replacer → video_replacer → egress`.

---

## Known limitations (revisit these)

Keep this list up to date. When something is addressed, move it to a
commit message or release note and delete the bullet.

### MVP-era limits for `video_encode`

1. **No resolution scaling.** If `video_encode.width` or `.height` is
   set and differs from the source, the replacer logs a warning and
   uses the source resolution anyway. Plumbing `VideoScaler` into
   `TsVideoReplacer` (and extending it to output YUV420P, not only
   YUVJ420P) is pending.
2. **No frame-rate auto-detection.** The encoder must know fps at
   `open` time; we default to 30/1 when the config omits it. The right
   answer is to read the source SPS / VPS and pass the detected fps
   in. Until then, operators should set `fps_num` / `fps_den`
   explicitly.
3. **No rate-control tuning knobs.** We pass `bitrate_kbps` + a
   `tune=zerolatency` option and rely on defaults for VBV buffer size,
   CRF, look-ahead, etc. CBR-strict profiles (true constant-bitrate
   muxing) may need extra work for hard-rate contribution paths.
4. **No B-frames.** `max_b_frames = 0` is hard-coded to simplify
   decoder interop. Enabling them later would improve quality at a
   given bitrate.
5. **No keyframe alignment with source.** The encoder emits IDRs on
   its own GOP cadence, ignoring the source PES PTS alignment. This is
   fine for distribution receivers but can trip HLS segment boundaries
   once that path is wired up (Phase 4d).
6. **No extradata injection on reconnect.** The encoder emits SPS/PPS
   inline (`global_header: false`). If a downstream client connects
   mid-GOP, it must wait for the next IDR. Good enough for MPEG-TS
   contribution; not enough for some WebRTC / RTMP flows — Phase 4d
   will switch to global_header for those transports and prepend
   extradata to each PES.
7. **PTS anchoring is simplistic.** The output stream uses the first
   source PES PTS as the anchor, then advances by `90_000 / fps` per
   emitted frame. A/V drift relative to the (still-passthrough) audio
   stream is therefore bounded by encoder buffering; sustained drift
   would need explicit PES PTS re-sync from the source.

### Deferred items (still to implement)

- **Phase 4d — RTMP / HLS / WebRTC video_encode.** Each output owns
  its own demux + re-mux pipeline; video_encode has to plug in via
  those paths rather than the TS-stream replacer:
  - RTMP: feed `EncodedVideoFrame` into `engine/rtmp/ts_mux.rs` or
    directly into the FLV tag writer, passing extradata as the AVC
    config record.
  - HLS: slot `VideoEncoder` into the segment remuxer
    (`engine/output_hls.rs`), align IDRs to segment boundaries.
  - WebRTC: emit Annex-B NALUs into `engine/webrtc/rtp_h264.rs` with
    fresh SPS/PPS packets per IDR.
- **Video transcode + FEC / redundancy.** Current validation rejects
  these combinations. Lifting the restriction means running the
  replacer upstream of the FEC encoder and preserving the RTP
  sequence-number space across re-muxing.
- **`VideoScaler` output in plain YUV420P.** The scaler currently
  hard-codes YUVJ420P (MJPEG full range). Adding a target-format
  parameter is a small change in `video-engine::scaler`.
- **Source-driven frame-rate detection.** Parse the SPS / VPS during
  decode-warm-up and feed detected fps into the encoder before first
  frame. Avoids the default-30fps fallback above.
- **`extradata` out-of-band for container-bound outputs.** When
  Phase 4d lands, RTMP / WebRTC / HLS will need `global_header: true`
  and access to `VideoEncoder::extradata()`.
- **Feature forwarding to bilbycast-manager UI.** The operator UI
  currently exposes `audio_encode` but not `video_encode`. Manager
  schema update + form rendering needed before non-CLI operators can
  configure it.
- **Licensing gate in release pipeline.** Nightly CI currently builds
  LGPL-clean. Once operator demand is validated we'll add a second
  matrix entry for `--features video-encoder-x264` with a
  prominently-labelled GPL artefact.
- **Binary purity gate.** `testbed/check-binary-purity.sh` knows about
  the existing C dependency set; it needs updating to recognise
  libx264 / libx265 / NVENC as expected when the matching feature is
  on.

### Compressed-audio bridge — current coverage

`src/engine/flow.rs` already auto-detects compressed-audio TS inputs
and routes them through `audio_decode` when the egress is ST 2110-30 /
-31 or `rtp_audio`. No explicit "decode_audio" flag is needed. Phase 2
is therefore delivered — just noting here in case a user-facing toggle
is wanted later for parity with `audio_encode`.

---

## Testbed configs

- `testbed/configs/audio-encode-srt-edge.json` — exercises audio_encode
  to MP2 (SRT), AC-3 (UDP), AAC-LC 64 kbps (RTP), and video_encode to
  2 Mbps libx264 (SRT). The video output only works when the edge is
  built with `--features video-encoder-x264`; on an LGPL-clean build
  it logs an error and falls back to passthrough.

## Capability advertisement (WS protocol)

The edge includes its compiled transcoding backends in the
`HealthPayload.capabilities` array so the manager UI can gray out
options the current binary cannot satisfy.

| Flag                        | Emitted when                                                   |
|-----------------------------|----------------------------------------------------------------|
| `audio-encode`              | Always (the AAC/Opus/MP2/AC-3 encoders are unconditional).     |
| `video-encode`              | Any of the three `video-encoder-*` features is enabled.        |
| `video-encoder-x264`        | Built with `--features video-encoder-x264`.                    |
| `video-encoder-x265`        | Built with `--features video-encoder-x265`.                    |
| `video-encoder-nvenc`       | Built with `--features video-encoder-nvenc`.                   |

A manager UI that wants to offer `video_encode` should check
`video-encode` first (to decide whether to render the block at all)
and then enable only the codec options whose backend flag is also
present. `h264_nvenc` and `hevc_nvenc` both gate on
`video-encoder-nvenc`.

See `bilbycast-edge/src/manager/client.rs::edge_capabilities` for the
source of truth.

## References in code

- `src/config/models.rs` — `AudioEncodeConfig`, `VideoEncodeConfig`.
- `src/config/validation.rs` — `validate_audio_encode`, `validate_video_encode`.
- `src/engine/ts_audio_replace.rs` — audio stage.
- `src/engine/ts_video_replace.rs` — video stage.
- `bilbycast-ffmpeg-video-rs/video-engine/src/video_encoder.rs` — low-level wrapper.
- `bilbycast-ffmpeg-video-rs/libffmpeg-video-sys/build.rs` — FFmpeg configure flags per feature.
