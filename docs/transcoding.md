# Transcoding reference — audio_encode + transcode + video_encode

This document is the canonical reference for the `audio_encode`,
`transcode`, and `video_encode` output blocks in `bilbycast-edge`, plus
a running record of the known limitations and work deferred for later
phases. When planning follow-up work, start here.

---

## Output × block support matrix

Legend: ✅ = wired and tested, ⏳ = planned (tracked below), ❌ = not
applicable / by design.

| Output     | `audio_encode` | `transcode` (channel shuffle / SRC) | `video_encode` | Notes |
|------------|:--------------:|:-----------------------------------:|:--------------:|-------|
| **SRT**    | ✅              | ✅ (requires `audio_encode`) | ✅ | Both transforms stack; forces raw-TS egress when either is set. |
| **UDP**    | ✅              | ✅ (requires `audio_encode`) | ✅ | Same as SRT. |
| **RTP**    | ✅              | ✅ (requires `audio_encode`) | ✅ | Strips source RTP framing, rewraps with fresh RFC 2250 headers. |
| **RIST**   | ✅              | ✅ (requires `audio_encode`) | ✅ | TS-carrying; same plumbing as SRT/UDP/RTP. |
| **RTMP**   | ✅              | ✅ (requires `audio_encode`) | ⏳ | Transcode disables the same-codec AAC passthrough fast-path. Video re-encode is Phase 4d. |
| **HLS**    | ✅              | ✅ (in-process remux only) | ⏳ | `video-thumbnail` feature required for transcode; subprocess fallback ignores it with a warning. |
| **WebRTC** | ✅              | ✅ (`transcode.channels` overrides Opus channel count; unset keeps source) | ⏳ | H.264 re-encode wiring is Phase 4d. |
| **ST 2110-30 / `rtp_audio`** | ✅ (auto via compressed-audio bridge) | ✅ (native PCM transcode, bit-depth + SRC + shuffle) | ❌ | Uncompressed PCM outputs; transcode is first-class here. |
| **ST 2110-31** | ✅ | ❌ (AES3 opaque — channel labels inside SMPTE 337M payload, not addressable from the pipeline) | ❌ | |
| **ST 2110-40** | ❌ | ❌ | ❌ | Ancillary data — no codec concept. |

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

## `transcode` — channel shuffle / sample-rate conversion

Sits between the AAC decoder and the target encoder in the
`audio_encode` pipeline. Every output that supports `audio_encode` also
accepts an optional `transcode` block (except ST 2110-31, where AES3
framing is opaque to the pipeline). Unset fields pass through that
stage — so an empty `transcode: {}` is a no-op and setting only
`channel_map_preset` runs no sample-rate conversion. This is the
"option 3" design: one block, three logically independent sub-stages
(channel matrix, sample-rate conversion, bit-depth quantization), each
of which is skipped whenever the corresponding fields are unset.

`transcode` has no effect without `audio_encode` — validation rejects
the combination at load time.

### Schema

```jsonc
"transcode": {
  "channels":              2,     // optional; defaults to source
  "sample_rate":           48000, // optional; defaults to source
  "channel_map_preset":    "stereo_to_mono_3db", // OR channel_map / channel_map_with_gain
  "channel_map":           [[0], [1]],           // per-output-channel unity-gain source list
  "channel_map_with_gain": [[[0, 1.0]], [[1, 0.7071]]], // per-output-channel [[src_ch, gain], ...]
  "src_quality":           "high",  // "high" (default) | "fast"
  "dither":                "tpdf"   // "tpdf" (default) | "none"; only applied to PCM outputs
}
```

The three channel-routing forms (`channel_map`, `channel_map_with_gain`,
`channel_map_preset`) are mutually exclusive. Presets:
`mono_to_stereo`, `stereo_to_mono_3db`, `stereo_to_mono_6db`,
`5_1_to_stereo_bs775`, `7_1_to_stereo_bs775`, `4ch_to_stereo_lt_rt`.

### Resolution rule

When both blocks set the same field, `transcode` wins. `audio_encode`'s
`sample_rate` / `channels` are used as fallbacks for fields that
`transcode` leaves unset. Example:

- `audio_encode.channels = 2` + `transcode.channels = 1` → encoder sees
  mono PCM (transcode wins).
- `audio_encode.sample_rate = 44100` + `transcode.sample_rate` unset →
  encoder ingests at 44100 Hz (fallback).
- Neither set → encoder follows the source.

On WebRTC: Opus is always 48 kHz on the wire. `transcode.sample_rate`
only chooses the PCM rate the encoder ingests; Opus resamples
internally to 48 kHz. `transcode.channels` overrides the Opus
channel count; if unset, the Opus encoder follows the source.

### Engine internals

- Core stage: `src/engine/audio_transcode.rs::PlanarAudioTranscoder`
  (planar f32 in, planar f32 out). Uses the same `ChannelMatrix`,
  `rubato`-based SRC, and preset expander as the PCM path for
  ST 2110-30.
- Fast paths: full passthrough when rate+channels+matrix are identity,
  matrix-only when rates match, matrix+rubato otherwise.
- Wiring:
  - TS outputs (SRT / UDP / RTP / RIST): inserted inside
    `ts_audio_replace::TsAudioReplacer` between decoder and encoder.
  - RTMP: `EncoderState::Active.transcoder`; disables the same-codec
    fast path because PCM must be decoded to apply the shuffle.
  - HLS: constructed fresh per segment inside `remux_ts_audio_inprocess`.
  - WebRTC: `WebrtcEncoderState::Active.transcoder`, on both the WHEP
    viewer loop and the WHIP client loop.

### Example — downmix a 5.1 AAC source to stereo on an SRT TS output

```jsonc
{
  "type": "srt",
  "id": "srt-stereo-feed",
  "audio_encode": { "codec": "aac_lc", "bitrate_kbps": 128 },
  "transcode":    { "channels": 2, "channel_map_preset": "5_1_to_stereo_bs775" }
}
```

### Example — swap L/R on an RTMP publish, no SRC

```jsonc
{
  "type": "rtmp",
  "audio_encode": { "codec": "aac_lc" },
  "transcode":    { "channels": 2, "channel_map": [[1], [0]] }
}
```

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
- **~~`VideoScaler` output in plain YUV420P.~~** Done as part of the
  ST 2110-20 work: `VideoScaler::new_with_dst_format()` now supports
  `ScalerDstFormat::{Yuvj420p, Yuv422p8, Yuv422p10le}`. The existing
  `VideoScaler::new()` constructor defaults to `Yuvj420p` and is
  behaviour-compatible.
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

### ST 2110-20 / -23 uncompressed video (Phase 2)

ST 2110-20 and -23 reuse the same `VideoEncoder` / `VideoDecoder` /
`VideoScaler` infrastructure as `video_encode`, but plug in at the
input and output edges rather than inside a TS replacer:

- **Ingress (`st2110_20`, `st2110_23` inputs)** — RFC 4175 depacketize
  → raw-frame mpsc → `spawn_blocking` worker running `VideoEncoder`
  (x264/x265/NVENC) → `TsMuxer` → `RtpPacket { is_raw_ts: true }` onto
  the flow's broadcast channel. Configured via a **mandatory**
  `video_encode` block on the input. Validation rejects inputs with
  no encoder block; encoder backends obey the same Cargo-feature gate
  as output `video_encode` (default LGPL-clean build cannot drive -20
  inputs at runtime).
- **Egress (`st2110_20`, `st2110_23` outputs)** — subscribe to the
  broadcast, `TsDemuxer` → NALU mpsc → `spawn_blocking` worker running
  `VideoDecoder` + `VideoScaler::new_with_dst_format()` → pack planar
  YUV into RFC 4175 pgroups → `Rfc4175Packetizer` → `UdpSocket::send_to`
  (Red + optional Blue). No `video_encode` block is accepted; the
  decode step is implicit.
- **Pixel formats** — Phase 2 supports 4:2:2 at 8-bit (`pgroup=4`) and
  10-bit LE (`pgroup=5`). 4:2:0 / 4:4:4 / 12-bit / RGB are validated-
  and-rejected. Bit-depth reduction before the encoder is a simple
  `>> 2` (no dithering); adequate for contribution but a follow-up
  item for mastering-grade workflows.
- **ST 2110-23** — partition modes `two_sample_interleave` (2SI) and
  `sample_row` are supported; `sample_column` is validated-and-rejected.
  The reassembler at ingress is timestamp-keyed with `max_in_flight=4`
  to bound memory.
- **Non-blocking** — all codec work runs on `spawn_blocking`; between
  reactor tasks and blocking workers we use bounded mpsc channels
  with drop-on-lag (same policy as broadcast channel lag). The tokio
  reactor is never blocked.

Testbed config: `testbed/configs/st2110-video-loopback-edge.json`
exercises an `st2110_20` input encoding to H.264 into an SRT listener.

**Deferred (still to land)**:
- ST 2110-22 (JPEG XS) — pending a libjxs wrapper crate.
- `sample_column` partition mode for ST 2110-23.
- 4:2:0 / 4:4:4 / 12-bit / RGB pgroup formats.
- PTP-derived RTP timestamps on the egress packetizer (currently uses
  a monotonic counter derived from upstream DTS).
- Dithered 10→8 bit conversion feeding the H.264/HEVC encoder.

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

A follow-up will add an `st2110-video` capability flag so the manager
UI can offer the ST 2110-20 / -23 pixel-format / partition-mode
pickers only when the edge's decoder (`video-thumbnail` feature) and
at least one encoder (`video-encoder-*` feature) are both compiled in.

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
