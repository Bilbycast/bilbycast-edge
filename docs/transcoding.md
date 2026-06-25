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
| **HLS**    | ✅              | ✅ (in-process remux only) | ⏳ | `media-codecs` feature required for transcode; subprocess fallback ignores it with a warning. |
| **WebRTC** | ✅              | ✅ (`transcode.channels` overrides Opus channel count; unset keeps source) | ✅ | H.264 target only (browsers do not decode HEVC); SPS/PPS emitted in-band on every IDR via `global_header = false`. HEVC sources are decoded and re-encoded to H.264 automatically. No scaling / no force-IDR on PLI yet (encoder GOP cadence drives keyframes). |
| **ST 2110-30 / `rtp_audio`** | ✅ (auto via compressed-audio bridge) | ✅ (native PCM transcode, bit-depth + SRC + shuffle) | ❌ | Uncompressed PCM outputs; transcode is first-class here. |
| **ST 2110-31** | ✅ | ❌ (AES3 opaque — channel labels inside SMPTE 337M payload, not addressable from the pipeline) | ❌ | |
| **ST 2110-40** | ❌ | ❌ | ❌ | Ancillary data — no codec concept. |
| **CMAF / CMAF-LL** | ✅ (AAC family only) | ✅ (requires `audio_encode`) | ✅ | fMP4 / CMAF segments with HLS m3u8 + DASH MPD; segmenter forces `gop_size = segment_duration × fps` when `video_encode` is set so segments always cut on IDR. Codec work runs in `block_in_place`. See [`docs/cmaf.md`](cmaf.md) for the full reference. |

---

## Input × block support matrix

The same three blocks can be set on almost every input, so a flow can
normalise its feed *once* at ingress and amortise the codec cost across
all attached outputs. The blocks carry identical semantics to their
output counterparts (same structs, same validation, same backends), so
anything documented below for outputs applies to inputs verbatim.

| Input      | `audio_encode` | `transcode` | `video_encode` | Notes |
|------------|:--------------:|:-----------:|:--------------:|-------|
| **RTP**    | ✅ | ✅ (requires `audio_encode`) | ✅ | Strips the RTP header before the TS replacer; republishes as raw TS. |
| **UDP**    | ✅ | ✅ | ✅ | Treated as raw TS. |
| **SRT**    | ✅ | ✅ | ✅ | Auto-detects RTP/TS vs raw TS as before, then applies the replacer. |
| **RIST**   | ✅ | ✅ | ✅ | Post-delivery reliable RTP, same plumbing as RTP. |
| **RTMP**   | ✅ | ✅ | ✅ | Applied on the `TsMuxer` output, before broadcast. |
| **RTSP**   | ✅ | ✅ | ✅ | Same as RTMP. |
| **WebRTC (WHIP / WHEP)** | ✅ | ✅ | ✅ | Applied after the `ts_demux` re-mux. |
| **Test pattern** | ✅ | ✅ (requires `audio_encode`) | ✅ | Synthetic SMPTE bars + 1 kHz tone are muxed to TS first, then routed through the standard `InputTranscoder` — useful for codec test rigs that need a known-good signal in a non-default codec / bitrate. |
| **Media player** | ✅ | ✅ (requires `audio_encode`) | ✅ | All three source kinds (raw TS / MP4 / image slate) emit MPEG-TS, then re-encode through the same `InputTranscoder` plumbing as the live inputs. Useful for normalising a mixed-codec playlist to a single output codec before fan-out. |
| **Replay** | ✅ | ✅ (requires `audio_encode`) | ✅ | Recorded TS segments are paced from disk and routed through `InputTranscoder` — useful when the destination needs a different codec from the captured bitstream. Speed-shifted playback (`speed != 1.0`) rewrites PCR/PTS first; the transcoder then sees the rewritten timestamps. |
| **ST 2110-20 / -23** | ❌ | ❌ | ✅ (**required**) | Uncompressed RFC 4175 → H.264/HEVC on ingest — mandatory, was shipped in Phase 2. |
| **ST 2110-30** | ⏳ (see below) | ✅ (native PCM reshape) | ❌ | `transcode` reshapes linear PCM in place. `audio_encode` changes the broadcast-channel shape to TS (rejects PCM-only outputs on the same flow); the AAC family (`aac_lc` / `he_aac_v1` / `he_aac_v2`) + `s302m` are wired, while `mp2` / `ac3` are deferred — picking one returns `PcmInputError::UnsupportedAudioEncodeCodec` and the flow surfaces a Critical `pid_bus_audio_encode_codec_not_supported_on_input` event. |
| **`rtp_audio`** | ⏳ | ✅ | ❌ | Same story as ST 2110-30. |
| **ST 2110-31** | ❌ | ❌ | ❌ | AES3 opaque — validation rejects any transcode/encode; would destroy SMPTE 337M metadata. |
| **ST 2110-40** | ❌ | ❌ | ❌ | Ancillary data. |

### Why use input-side transcoding?

- **Fan-out optimisation.** A single ingress re-encode feeds many outputs
  without per-output codec work.
- **Codec harmonisation across inputs.** When a flow has multiple
  inputs (active/standby), input-side `audio_encode` / `video_encode`
  forces every input to emit the same codec so the broadcast channel
  shape doesn't change on a switch. Validation enforces this at config
  load time — a mixed-shape flow (PCM-passthrough + PCM-encoded-to-TS)
  is rejected.
- **Upgrading older sources.** An RTMP ingest carrying HEVC via enhanced
  RTMP can be decoded and re-encoded to H.264 on ingress so legacy
  outputs still work.

### Flow-level shape compatibility (new)

`validate_config` enforces two rules so the broadcast channel always
carries a consistent shape:

1. If any input in a flow produces MPEG-TS (natively, or via
   `audio_encode` on a PCM input), every other input on the same flow
   must also produce TS.
2. When a flow's inputs produce TS, PCM-only outputs (ST 2110-30,
   ST 2110-31, `rtp_audio`) cannot attach — they expect raw PCM-RTP on
   the broadcast channel and would silently produce noise.

ST 2110-40 inputs are exempt from these checks (they carry ancillary
data and can mix freely with any media flow).

### Implementation

Group A (TS-carrying) inputs all route through
`engine::input_transcode::InputTranscoder`, which composes the existing
`TsAudioReplacer` + `TsVideoReplacer` (audio-first-then-video, same
order the TS outputs already use) and calls them inside
`tokio::task::block_in_place` to match the output-side non-blocking
contract. Each input task adds exactly one helper call at the point
where it publishes to the broadcast channel:

```rust
// before:
let _ = broadcast_tx.send(packet);
// after:
publish_input_packet(&mut transcoder, &broadcast_tx, packet);
```

Group B (PCM-only) inputs use
`engine::input_pcm_encode`, which wraps the existing
`engine::audio_transcode` path for the PCM → PCM reshape. The
runtime audio-encode path for PCM inputs is wired for the AAC family
(`aac_lc` / `he_aac_v1` / `he_aac_v2`) plus `s302m`; `mp2` / `ac3` are
deferred and return `PcmInputError::UnsupportedAudioEncodeCodec`, which
flow bring-up translates into a Critical
`pid_bus_audio_encode_codec_not_supported_on_input` event before the
input task runs.

### Force-IDR on input switch (multi-input flows)

When a multi-input flow switches to an input that has `video_encode`
(ingress transcoding), the switch forwarder sets a one-shot
`Arc<AtomicBool>` that the target input's `TsVideoReplacer` consumes
before its next `VideoEncoder::encode_frame` call. The replacer then
sets `AVFrame.pict_type = AV_PICTURE_TYPE_I`, which libx264 / libx265 /
NVENC all honour by emitting an IDR for that frame.

Without this hook, downstream decoders had to wait for the next natural
keyframe from the ingress re-encoder, which at the default
`gop_size = 2 × fps` cadence is up to 2 s at 30 fps. With the hook,
the first post-switch frame is always an IDR, and visible switch
latency at the receiver is one to two frames — indistinguishable from
a passthrough input.

Inputs with no `video_encode` (passthrough) have no encoder to signal;
their keyframe cadence is whatever the upstream source chose, and the
existing CC-jump + PSI-injection mechanisms handle those switches.
Rapid back-and-forth switching collapses into at most one IDR per
switch event — the flag is consumed on the next encoded frame and
cleared, so repeated sets over a single frame interval don't cause a
bitrate spike.

Wiring:

- `video-engine::VideoEncoder::force_next_keyframe()` — arms the flag
  on the encoder; consumed inside `encode_frame`.
- `engine::ts_video_replace::TsVideoReplacer::new(cfg, force_idr)` —
  optionally accepts an externally-owned `Arc<AtomicBool>` so the
  forwarder and the replacer share the same trigger.
- `engine::flow::spawn_input_forwarder` — sets the flag on the
  passive → active edge in the watch-channel observer loop.

---

## `audio_encode` — compressed-audio re-encoding

Decodes the source audio ES, rescales sample rate / channel count via
the PCM pipeline, and re-encodes into the target codec. The source
audio stream is replaced in the output TS; video and other PIDs pass
through unchanged.

**Source codecs accepted on every output (TS-out + re-mux):** AAC-LC /
HE-AAC ADTS (MPEG-TS `stream_type` 0x0F), MP2 / MPEG-1 / MPEG-2 audio
(0x03 / 0x04), AC-3 (0x80 / 0x81 / 0xC1), and E-AC-3 / Dolby Digital
Plus (0x87 / 0xC2). AAC decodes via the in-process FDK-AAC bridge
(`fdk-aac` feature, default on); MP2 / AC-3 / E-AC-3 decode via the
in-process FFmpeg bridge (`media-codecs` feature, default on). Both
share the same downstream PCM → encoder pipeline, so every target
codec listed in the matrix below works regardless of source codec.

### Schema

```jsonc
"audio_encode": {
  "codec": "aac_lc" | "he_aac_v1" | "he_aac_v2" | "opus" | "mp2" | "ac3",
  "bitrate_kbps": 128,       // optional; per-codec default
  "sample_rate":  48000,     // optional; defaults to source
  "channels":     2,         // optional; defaults to source
  "silent_fallback": false   // optional; RTMP / WebRTC / CMAF only
}
```

### `silent_fallback`

When `true`, the edge injects a zero-filled (silent) PCM track into the
encoder whenever the upstream source has no audio PID, or stops
delivering audio mid-stream (500 ms grace window). Guarantees the
output container always carries a valid, continuous audio track.

Required whenever:

- Pushing a **video-only source** (IP cameras, drones, slide feeds) to
  Twitch / YouTube / Facebook RTMP — their live-preview thumbnailer
  gates on audio presence and will show "LIVE" without ever rendering
  a picture on silent streams.
- Distributing via **WebRTC** (WHIP / WHEP) — browsers emit muted-track
  warnings and may disable the audio decoder when the track fails to
  produce samples.
- Packaging **low-latency CMAF / CMAF-LL** — the segmenter expects
  monotonic audio timestamps per segment; gaps cause player stalls.

Wired for RTMP, WebRTC (both WHIP client and WHEP server paths), and
CMAF (HLS + DASH output) in this release. HLS (standalone HLS output,
not the CMAF-HLS manifest) is tracked for a follow-up because its
per-segment batch remuxer is architecturally different from the
streaming encoder the other three share. On SRT / UDP / RTP / RIST the
field is parsed for round-trip fidelity but has no effect today.

Implementation: `src/engine/audio_silence.rs` + the
`build_encoder_state_eager_for_silent_fallback` helper in each output
(RTMP `src/engine/output_rtmp.rs`, WebRTC `src/engine/output_webrtc.rs`,
CMAF `src/engine/cmaf/encode.rs` via `AudioReencoder`).

### Allowed codecs per output

| Output       | Allowed codecs                                     |
|--------------|----------------------------------------------------|
| RTMP         | `aac_lc`, `he_aac_v1`, `he_aac_v2`                 |
| HLS          | `aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`   |
| WebRTC       | `opus`                                             |
| **SRT / UDP / RTP** | `aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3` — `opus` is rejected here because MPEG-TS has no standard Opus mapping. |
| **CMAF / CMAF-LL** | `aac_lc`, `he_aac_v1`, `he_aac_v2` — fragmented-MP4 audio sample entry is `mp4a`/`enca` (MPEG-4 AAC family); MP2 / AC-3 / Opus are not used. |

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

**Status:** Shipped. Active on SRT / UDP / RTP / RIST outputs (TS
pipeline via `TsVideoReplacer`), RTMP (`output_rtmp::VideoEncoderState`),
WebRTC (H.264 only, `output_webrtc::WebrtcVideoEncoderState`), CMAF /
CMAF-LL (segmenter forces GoP alignment), and ST 2110-20 / -23
(mandatory on those inputs). **HLS is the only remaining output
without `video_encode`** (deferred — see below).

Decodes the source video ES (H.264 or HEVC) in-process via
`video-engine::VideoDecoder`, re-encodes via a feature-gated backend,
and muxes the new bitstream back into the output TS. When the target
codec family differs from the source (H.264 ↔ HEVC), the PMT is
rewritten in place with a recomputed CRC32; same-family transcodes
leave the PMT untouched.

### Schema

```jsonc
"video_encode": {
  "codec":       "x264" | "x265" | "h264_nvenc" | "hevc_nvenc" | "h264_qsv" | "hevc_qsv"
  //             | "h264_vaapi" | "hevc_vaapi" | "h264_auto" | "hevc_auto" | "auto",
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
| `x264`        | `video-encoder-x264`      | `apt install libx264-dev` (dev/à-la-carte). **Portable/release builds static-link x264** — build a static `libx264.a` from source (Debian's `libx264-dev` has none) and put it on `PKG_CONFIG_PATH`; otherwise build.rs falls back to a dynamic link tied to the host SONAME. | **GPL v2+** — binary becomes AGPL-3.0-or-later combined work (see `NOTICE.full`). |
| `x265`        | `video-encoder-x265`      | `apt install libx265-dev` (ships `libx265.a` — static-linked) + `libnuma-dev`. | **GPL v2+** — same implications as x264. |
| `h264_nvenc`  | `video-encoder-nvenc`     | Build: `nv-codec-headers`. Runtime: NVIDIA proprietary driver (provides `libnvidia-encode.so.1` + `libcuda.so.1`). Nouveau is **not** sufficient. | Royalty-free; API-layer LGPL-compatible. No GPL bundle. |
| `hevc_nvenc`  | `video-encoder-nvenc`     | same                         | same                      |
| `h264_qsv`    | `video-encoder-qsv`       | Build: `libvpl-dev` (x86_64). Runtime: **`libvpl2`** + **`libmfx-gen1.2`** (the GPU runtime — most-commonly-missed) + **`intel-media-va-driver-non-free`** (or `intel-media-va-driver`). Intel iGPU (Broadwell / 5th gen+) or Arc dGPU. | Royalty-free; libvpl headers MIT, dispatcher Apache 2.0. No GPL bundle. No `--enable-nonfree` needed. |
| `hevc_qsv`    | `video-encoder-qsv`       | same; HEVC requires Kaby Lake (7th gen) or newer | same |
| `h264_vaapi`  | `video-encoder-vaapi`     | Build: `apt install libva-dev`. Runtime: working VAAPI driver — Mesa **`radeonsi`** for AMD, **`iHD`** for Intel. **4:2:0 8-bit only** on every implementation (no Main10 / 4:2:2 profiles in H.264 VAAPI on any host). | Royalty-free; libva is MIT, FFmpeg's VAAPI wrapper LGPL. No GPL bundle. |
| `hevc_vaapi`  | `video-encoder-vaapi`     | Same build deps. Supports broadcast contribution matrix end-to-end: 4:2:0 8-bit (NV12), 4:2:0 10-bit (P010LE), 4:2:2 8-bit (NV16), 4:2:2 10-bit (P210LE) — see per-vendor notes below. | same |

#### VAAPI HEVC chroma × bit-depth support per vendor

The 4:2:2 / 10-bit broadcast-contribution cells are wired end-to-end in
`hevc_vaapi`, but actual support varies by host driver. The startup
hardware probe (`HwEncoderChromaCapability::hevc_vaapi_*`) tries each
combination via `VideoEncoder::open()` and the manager UI gates the
flow modal's chroma/bit-depth dropdown to whatever the host can
actually open.

| Combination          | Surface | Intel iHD                                  | AMD radeonsi (VCN)                           |
|----------------------|---------|--------------------------------------------|----------------------------------------------|
| HEVC 4:2:0 8-bit     | NV12    | Skylake (6th gen) and newer                | All RDNA / RDNA2 / RDNA3                     |
| HEVC 4:2:0 10-bit    | P010LE  | Kaby Lake (7th gen) and newer (Main 10)    | RDNA2+ (VCN3+); generally works              |
| HEVC 4:2:2 8-bit     | NV16    | Tiger Lake (11th gen) and newer            | Generally **rejected** — fall back to libx265 |
| HEVC 4:2:2 10-bit    | P210LE  | Tiger Lake (11th gen) and newer            | Generally **rejected** — fall back to libx265 |

Sports / live-broadcast contribution that needs 4:2:2 on an AMD-on-Linux
host today should ship the `*-linux-full` artefact and select
`x265` (libx265 supports the full Main 4:2:2 / Main 4:2:2 10 matrix at
the cost of GPL-2.0-or-later combined-work licensing — see the
"Commercial licensing + GPL" note below).

#### Per-vendor backend recommendation

The right choice depends on host vendor:

| Host         | First choice for encode             | Notes                                                         |
|--------------|-------------------------------------|---------------------------------------------------------------|
| **NVIDIA**   | `h264_nvenc` / `hevc_nvenc`         | NVENC is mature and well-tuned; pick VAAPI only if NVENC drivers aren't available. |
| **Intel**    | `h264_qsv` / `hevc_qsv`             | QSV via libvpl exposes more rate-control knobs than VAAPI on iHD. VAAPI works as a fallback when libvpl isn't installed. |
| **AMD**      | `h264_vaapi` / `hevc_vaapi`         | The only royalty-clean HW encode on AMD-on-Linux. AMD has no NVENC equivalent and no QSV. |
| **CPU-only** | `x264` / `x265`                     | Highest quality at low broadcast-contribution bitrates; AGPL-3.0-or-later combined work. |

**Quality caveat for AMD VCN.** At low broadcast-contribution bitrates
(roughly ≤ 6 Mbps 1080p H.264, ≤ 8 Mbps 1080p HEVC), the AMD VCN
encoder produces noticeably lower quality than libx264 / NVENC at the
same bitrate — visible on grass / crowd textures. Operators who can
afford 30–50 % more bitrate offset most of the gap. Above that
bitrate the difference is negligible. Intel iHD VAAPI quality is
roughly on par with QSV; the caveat is AMD-specific. If you need
broadcast-quality H.264 at low bitrate on an AMD-on-Linux host, ship
the `*-linux-full` artefact and select `x264` instead — the binary is
GPL-2.0-or-later combined work either way (libx264 contagion).

**Broadcast contribution workflows (4:2:2 / 10-bit).** Sports and
live-broadcast workflows commonly use 4:2:2 chroma and / or 10-bit
sample depth — for **both** SDR and HDR contribution. The full
chroma × bit-depth matrix maps to broadcast formats as:

- **4:2:0 8-bit** — consumer / streaming / OTT distribution.
- **4:2:0 10-bit** — HDR (PQ / HLG) consumer distribution; also the
  most-common 10-bit SDR cell for streaming.
- **4:2:2 8-bit** — SDI baseband contribution (the broadcast
  "lossless-ish" baseline; still common in sports A/B and remote
  encoders).
- **4:2:2 10-bit** — SDI 10-bit broadcast contribution. Standard for
  high-end sports / live-events workflows whether the show is HDR or
  SDR — operators pick 10-bit for the additional headroom in the
  contribution-encode ladder, not for HDR specifically.

`hevc_vaapi` covers the full matrix on Intel iHD (Tiger Lake / 11th gen
and newer); `h264_vaapi` is locked to 4:2:0 8-bit on every VAAPI
implementation. AMD VCN encoders generally reject 4:2:2 — broadcast
contribution shops on AMD-on-Linux land on libx265.

Default release build has no software video encoders (AGPL-only
binary). The composite `video-encoders-full` feature bundles every
video codec backend the edge knows about — encoders (x264 + x265 +
NVENC + QSV + VAAPI) **and** HW decoders for the local-display +
transcode-input paths (NVDEC + QSV-decode + VAAPI-decode) — and is
used by the GitHub Actions release workflow to produce the
`*-linux-full` variant. See
[`docs/installation.md`](installation.md) for the two-channel
release model. The `*-aarch64-linux-full` artefact intentionally
drops QSV (encode + decode; Intel iGPU is x86_64-only) and lists
the remaining features explicitly: x264 + x265 + NVENC + NVDEC +
VAAPI encode/decode.
Runtime error `video encoder disabled: rebuild with …` surfaces
when a config targets a codec whose feature flag was not enabled at
build; the display output's `hw_decode: "nvdec" / "qsv"` choice
similarly emits `display_hw_decode_unavailable` when the matching
HW decode feature isn't compiled in.

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

# libx264 + libx265 are STATICALLY linked (the binary then has no
# libx264.so/libx265.so runtime dependency and isn't tied to a distro's
# ABI-versioned SONAME). libx265-dev ships libx265.a + libnuma is its dep;
# Debian/Ubuntu's libx264-dev has NO static .a, so build one from source
# and expose it on PKG_CONFIG_PATH (else build.rs warns and falls back to a
# non-portable DYNAMIC libx264 link). nasm is required for the x264 asm.
sudo apt install nasm libx265-dev libnuma-dev
git clone --depth 1 https://code.videolan.org/videolan/x264.git /tmp/x264
( cd /tmp/x264 && ./configure --prefix="$HOME/x264-static" \
    --enable-static --enable-pic --disable-cli && make -j"$(nproc)" && make install )
export PKG_CONFIG_PATH="$HOME/x264-static/lib/pkgconfig:$PKG_CONFIG_PATH"

# Linux x86_64 — full build (bundles x264 + x265 + NVENC + QSV + VAAPI
# encoders, plus NVDEC + QSV-decode + VAAPI-decode for the display +
# transcode-input paths; matches *-x86_64-linux-full release):
sudo apt install nv-codec-headers libvpl-dev libva-dev libdrm-dev
cargo build --release --features video-encoders-full

# Linux aarch64 — full build minus QSV encode + decode (Intel iGPU is
# x86_64-only). Same static-x264/x265 prerequisites as above.
sudo apt install nv-codec-headers libva-dev libdrm-dev
cargo build --release --features "video-encoder-x264 video-encoder-x265 video-encoder-nvenc video-encoder-vaapi video-decoder-nvdec video-decoder-vaapi"

# Linux — individual opt-ins (à la carte):
cargo build --release --features video-encoder-x264
cargo build --release --features video-encoder-x265
cargo build --release --features video-encoder-nvenc
cargo build --release --features video-encoder-qsv      # x86_64 only
```

### QSV (Intel QuickSync) at runtime

Once you've built with `video-encoder-qsv`, the host needs four things
to actually exercise the encoder:

1. **Hardware**: a 5th-gen (Broadwell) or newer Intel Core CPU with an
   integrated GPU, or an Intel Arc / Battlemage discrete GPU. HEVC
   encoding requires 7th-gen (Kaby Lake) or newer.
2. **oneVPL dispatcher** — `libvpl2`. Implements `MFXLoad`. The bilbycast
   binary links to it dynamically.
3. **oneVPL GPU runtime** — `libmfx-gen1.2`. **This is the package most
   commonly missed.** The dispatcher itself contains zero encoding code;
   it `dlopen`s `libmfx-gen.so.1.2` at session create. Without this
   package installed, `MFXLoad` returns `MFX_ERR_NOT_FOUND` (-9) and
   `avcodec_open2` fails with EINVAL — the same symptom an "incorrect
   parameters" message in the FFmpeg log produces.
4. **Intel media VAAPI driver** — `intel-media-va-driver-non-free` (or
   `intel-media-va-driver` for the upstream open-source variant). Provides
   `iHD_drv_video.so` for the VAAPI fallback path that `libmfx-gen` uses
   for some pixel-format conversions and zero-copy frame paths.
5. **Device access**: the running user must be in the `render` group so
   it can open `/dev/dri/renderD*`.

```bash
sudo apt install libvpl2 libmfx-gen1.2 intel-media-va-driver-non-free
sudo usermod -aG render "$USER"
# log out + back in for the group change to take effect
```

Verify all three runtime files are present before starting the edge:

```bash
ls /usr/lib/x86_64-linux-gnu/libvpl.so.2          # dispatcher
ls /usr/lib/x86_64-linux-gnu/libmfx-gen.so.1.2    # GPU runtime — the critical one
ls /usr/lib/x86_64-linux-gnu/dri/iHD_drv_video.so # VAAPI driver
ls /dev/dri/                                      # card* + renderD* device nodes
```

If any of those are missing, `avcodec_find_encoder_by_name("h264_qsv")`
returns null OR `avcodec_open2` returns -22; either way the edge surfaces
a Critical event under category `video_encode` for the affected output,
then passthroughs the source video unchanged. The CPU encoders
(`x264`, `x265`) still work in the same binary as a fallback.

#### Why `libmfx-gen1.2` is mandatory and cannot be statically linked

Intel's oneVPL is intentionally split into a thin **dispatcher**
(`libvpl.so.2`, what we link against at compile time) and an **Intel-shipped
GPU runtime backend** (`libmfx-gen.so.1.2`, what does the actual encoding).
The same model applies to NVENC (`libnvidia-encode.so.1`), to VAAPI
(`iHD_drv_video.so`), and to OpenCL (vendor ICDs). bilbycast cannot
statically link the GPU runtime in — it is a GPU-architecture-specific
binary that Intel distributes as part of their driver stack, the same
way the NVIDIA driver ships NVENC. Every QSV-using application — bare
ffmpeg, OBS, GStreamer, HandBrake — has the same runtime-package
requirement.

**QSV constraints** enforced at config validation time:

- `h264_qsv` is 8-bit only — for 10-bit pick `hevc_qsv` (on Kaby Lake+).
- Neither QSV variant supports `chroma=yuv444p`. Use `yuv420p` or
  `yuv422p` (the latter only on supported codec / hardware combinations).
- `hevc_qsv` is rejected on WebRTC outputs (browsers don't decode HEVC);
  `h264_qsv` is allowed.

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

### PCR_AC at the receiver — observed-rate pacing

PCR jitter (PCR_AC in TR-101290) at the receiver is a function of the
encoder's actual output rate, not its declared bitrate. CRF / capped-VBR
/ "CBR" with VBV all overshoot transiently — the wire pacer
(`engine::wire_emit`) closed-loops on the inter-PCR observed rate and
adapts within ~10 PCRs (~400 ms typical). It does not consult
`video_encode.bitrate_kbps`. Universal across codec, RC mode, and
encoder backend (CPU x264/x265, NVENC, QSV, VAAPI).

The receiver-side PCR_AC envelope you should expect:

| Tier | Expected PCR_AC (p99) | Receiver compliance | When |
|---|---|---|---|
| 1 (SO_TXTIME + ETF + NIC HW) | < 1 µs | Tier-1 broadcast, T-STD ≤ 500 ns met | Opt-in via `BILBYCAST_ENABLE_TXTIME=1` + full PTP / ETF / HW-PTP stack |
| 2 (SO_TXTIME + software ETF) | < 50 µs | Most professional decoders happy | Opt-in via `BILBYCAST_ENABLE_TXTIME=1` + ETF qdisc (no HW-PTP NIC needed) |
| 4 ⭐ (`clock_nanosleep` SCHED_FIFO) | < 3 ms typical, ms-tail under load | Broadcast tier-2 envelope (≤ 30 ms p99); compressed TS through 2 Gbps; VLC / ffplay / OBS / cloud receivers / most professional decoders in standard tolerance mode | **Default** — no setup required |
| 5 (no SCHED_FIFO grant) | < 5 ms | Worst-case fallback; visual decode usually still clean | Non-Linux or Linux without `LimitRTPRIO` |

See [`wire-pacing.md`](wire-pacing.md) for the full architecture, the
decision matrix for when ETF earns its keep, and the per-symptom
diagnostic table. [`installation.md`](installation.md#wire-pacing)
covers the four-step opt-in procedure (qdisc → boot-time systemd unit
→ PTP → env var).

---

## Known limitations (revisit these)

Keep this list up to date. When something is addressed, move it to a
commit message or release note and delete the bullet.

### MVP-era limits for `video_encode`

1. **Resolution scaling (done).** `video_encode.width` / `.height` are
   now honoured on every decode→encode path: SRT / UDP / RTP / RIST
   (via `TsVideoReplacer`), RTMP (`output_rtmp::VideoActive`), WebRTC
   (`output_webrtc::WebrtcVideoActive`), CMAF (`cmaf::VideoReencoder`),
   and ST 2110-20 / -23 input (`st2110_video_io::encode_worker`). All
   five share the `ScaledVideoEncoder` pipeline in
   `video_encode_util.rs`, which lazy-opens a `VideoScaler` (Lanczos)
   between decoder/raw-planes and encoder when the target dimensions
   differ from the source. Supported target chroma / bit-depth for
   scaling: 4:2:0 8-bit, 4:2:2 8-bit, 4:2:2 10-bit. Unsupported
   (4:2:0 10-bit, 4:4:4): the pipeline logs a warning and falls back
   to no-scale encode.
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
  automatically. Remaining MVP limitation: PLI / FIR from the receiver
  is still logged-and-ignored — the encoder's configured GOP
  (default 2× fps) drives keyframe cadence. Force-IDR on PLI is tracked
  under a follow-up.

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
| `video-encode`              | Any `video-encoder-*` feature (x264 / x265 / nvenc / qsv / vaapi) is enabled. |
| `video-encoder-x264`        | Built with `--features video-encoder-x264`.                    |
| `video-encoder-x265`        | Built with `--features video-encoder-x265`.                    |
| `video-encoder-nvenc`       | Built with `--features video-encoder-nvenc`.                   |
| `video-encoder-qsv`         | Built with `--features video-encoder-qsv` (x86_64 only).       |
| `video-encoder-vaapi`       | Built with `--features video-encoder-vaapi` (Linux).          |

A follow-up will add an `st2110-video` capability flag so the manager
UI can offer the ST 2110-20 / -23 pixel-format / partition-mode
pickers only when the edge's decoder (`media-codecs` feature) and
at least one encoder (`video-encoder-*` feature) are both compiled in.

A manager UI that wants to offer `video_encode` should check
`video-encode` first (to decide whether to render the block at all)
and then enable only the codec options whose backend flag is also
present. `h264_nvenc` and `hevc_nvenc` both gate on
`video-encoder-nvenc`; `h264_qsv` and `hevc_qsv` both gate on
`video-encoder-qsv`; `h264_vaapi` and `hevc_vaapi` both gate on
`video-encoder-vaapi`. The `h264_auto` / `hevc_auto` / `auto` strings
are accepted whenever at least one encoder backend is compiled in and
resolve to a concrete backend per-host at flow start (see
[`docs/codec-matrix.md`](codec-matrix.md)).

See `bilbycast-edge/src/manager/client.rs::edge_capabilities` for the
source of truth.

## References in code

- `src/config/models.rs` — `AudioEncodeConfig`, `VideoEncodeConfig`.
- `src/config/validation.rs` — `validate_audio_encode`, `validate_video_encode`.
- `src/engine/ts_audio_replace.rs` — audio stage.
- `src/engine/ts_video_replace.rs` — video stage.
- `bilbycast-ffmpeg-video-rs/video-engine/src/video_encoder.rs` — low-level wrapper.
- `bilbycast-ffmpeg-video-rs/libffmpeg-video-sys/build.rs` — FFmpeg configure flags per feature.
