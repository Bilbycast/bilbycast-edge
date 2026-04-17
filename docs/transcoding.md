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
| **RTMP**   | ✅              | ✅ (requires `audio_encode`) | ✅ | H.264 target rides classic FLV; HEVC target rides [Enhanced RTMP v2](https://veovera.org/docs/enhanced/enhanced-rtmp-v2) with FourCC `hvc1`. Transcode disables the same-codec AAC passthrough fast-path. HEVC passthrough (no `video_encode` set) also emits E-RTMP tags. |
| **HLS**    | ✅              | ✅ (in-process remux only) | ⏳ | `video-thumbnail` feature required for transcode; subprocess fallback ignores it with a warning. |
| **WebRTC** | ✅              | ✅ (`transcode.channels` overrides Opus channel count; unset keeps source) | ✅ | H.264 target only (browsers do not decode HEVC); SPS/PPS emitted in-band on every IDR via `global_header = false`. HEVC sources are decoded and re-encoded to H.264 automatically. No scaling / no force-IDR on PLI yet (encoder GOP cadence drives keyframes). |
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
| `x264`        | `video-encoder-x264`      | `apt install libx264-dev`    | **GPL v2+** — binary becomes AGPL-3.0-or-later combined work (see `NOTICE.full`). |
| `x265`        | `video-encoder-x265`      | `apt install libx265-dev`    | **GPL v2+** — same implications as x264. |
| `h264_nvenc`  | `video-encoder-nvenc`     | `nv-codec-headers` + NVIDIA driver (runtime) | Royalty-free; API-layer LGPL-compatible. No GPL bundle. |
| `hevc_nvenc`  | `video-encoder-nvenc`     | same                         | same                      |

Default release build has no software video encoders (AGPL-only
binary). The composite `video-encoders-full` feature bundles all
three encoders and is used by the GitHub Actions release workflow
to produce the `*-linux-full` variant — see
[`docs/installation.md`](installation.md) for the two-channel
release model. Runtime error `video encoder disabled: rebuild with
…` surfaces when a config targets a codec whose feature flag was
not enabled at build.

**Commercial licensing + GPL**: bilbycast-edge source is dual-licensed
(AGPL-3.0-or-later / commercial from Softside Tech). The Softside
commercial licence covers bilbycast source only — it cannot
relicense libx264 or libx265, which remain GPL-2.0-or-later inside
any binary built with those features. If you distribute under a
commercial licence and need to avoid GPL copyleft on the encoder
portions, either (a) ship the default variant without software video
encoders, or (b) build with `video-encoder-nvenc` only (LGPL API
layer; NVIDIA handles H.264/H.265 patent coverage at the hardware
layer). See [`LICENSE.commercial`](../LICENSE.commercial) and the
bundled `NOTICE.full` for the full scope statement.

```bash
# Linux — default build (no video encoders, matches *-linux release):
cargo build --release

# Linux — full build (bundles x264 + x265 + NVENC, matches *-linux-full release):
sudo apt install libx264-dev libx265-dev nv-codec-headers
cargo build --release --features video-encoders-full

# Linux — individual opt-ins (à la carte):
cargo build --release --features video-encoder-x264
cargo build --release --features video-encoder-x265
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
6. **No extradata injection on reconnect.** The TS video replacer
   emits SPS/PPS inline (`global_header: false`). If a downstream
   client connects mid-GOP, it must wait for the next IDR. Good
   enough for MPEG-TS contribution and for WebRTC RTP (the RFC 6184
   packetizer carries inline SPS/PPS per IDR natively); RTMP
   `VideoEncoderState` already uses `global_header: true` for the
   FLV sequence header.
7. **PTS anchoring is simplistic.** The output stream uses the first
   source PES PTS as the anchor, then advances by `90_000 / fps` per
   emitted frame. A/V drift relative to the (still-passthrough) audio
   stream is therefore bounded by encoder buffering; sustained drift
   would need explicit PES PTS re-sync from the source.

### Phase 4d — container / RTP output video_encode

Each of these outputs owns its own demux + re-mux pipeline; video_encode
plugs in via those paths rather than the TS-stream replacer.

- **RTMP: done** — see `engine::output_rtmp::VideoEncoderState`. H.264
  targets use classic FLV `VideoData`; HEVC targets use the Enhanced
  RTMP v2 extended VideoTagHeader (FourCC `hvc1`, PacketType
  `CodedFramesX`). The encoder is opened with `global_header = true`
  so the FLV sequence header is built once from `VideoEncoder::extradata()`.
- **WebRTC: done** — see `engine::output_webrtc::WebrtcVideoEncoderState`.
  H.264-only target (WebRTC browsers do not decode HEVC; validation
  rejects `x265` / `hevc_nvenc`). The encoder is opened with
  `global_header = false` so SPS / PPS travel in-band on every IDR;
  `engine::webrtc::rtp_h264::H264Packetizer` forwards them as ordinary
  NAL units. HEVC source streams are decoded and re-encoded to H.264
  automatically. MVP limitations: no output scaling (requested
  width/height are logged and the source resolution is used), and
  PLI / FIR from the receiver is still logged-and-ignored — the
  encoder's configured GOP (default 2× fps) drives keyframe cadence.
  Force-IDR on PLI is tracked under a follow-up.

### Deferred items (still to implement)

- **Phase 4d — HLS video_encode.** Slot `VideoEncoder` into the HLS
  segment remuxer (`engine/output_hls.rs`), align IDRs to segment
  boundaries. HLS is the only remaining output type without
  `video_encode`.
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
- **`extradata` out-of-band for HLS.** HLS (still deferred) needs
  `global_header: true` and access to `VideoEncoder::extradata()`
  for the init segment / fMP4 moov. RTMP already uses this mode;
  WebRTC uses `global_header: false` by design (SPS/PPS in-band per
  IDR is the standard RFC 6184 approach and handled natively by
  `H264Packetizer`).
- **Feature forwarding to bilbycast-manager UI.** The operator UI
  currently exposes `audio_encode` but not `video_encode`. Manager
  schema update + form rendering needed before non-CLI operators can
  configure it.

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
  as output `video_encode` (default build without `video-encoders-full`
  or an individual `video-encoder-*` opt-in cannot drive -20 inputs at
  runtime).
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
  built with `--features video-encoder-x264` (or the composite
  `--features video-encoders-full`); a default build logs an error
  and falls back to passthrough.

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
