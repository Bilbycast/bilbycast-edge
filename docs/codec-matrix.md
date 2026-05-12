# Codec / Encoder / Decoder / Transcoder Reference

Canonical support matrix for every video and audio codec backend in
`bilbycast-edge`. Cross-linked from
[`docs/transcoding.md`](transcoding.md) and from `CLAUDE.md`'s feature-flag
table. The host runtime probe (`engine::hardware_probe`) layers
per-driver / per-host gates on top of the matrix below; this document
captures the **static** support shape and the **resolver** behaviour for
the `*_auto` codec strings.

## Decision tree — which codec do I pick?

For most operators the answer is **`h264_auto`** (or **`hevc_auto`**) on
the `video_encode.codec` field. The edge resolves Auto on flow start
against the host's probed capabilities and the requested
chroma + bit-depth, picking the cheapest backend that handles the
combination. The resolver chain is:

| Family + chroma + bit-depth | Resolver chain (head → tail) |
|---|---|
| H.264 + 4:2:0 + 8-bit (the dominant distribution path) | NVENC ≻ QSV ≻ VAAPI ≻ libx264 |
| H.264 + anything else | libx264 (no HW H.264 supports 4:2:2 / 10-bit / 4:4:4) |
| HEVC + 4:2:0 + 8-bit | NVENC ≻ QSV ≻ VAAPI ≻ libx265 |
| HEVC + 4:2:0 + 10-bit | NVENC ≻ VAAPI ≻ QSV ≻ libx265 |
| HEVC + 4:2:2 + 8-bit | VAAPI (Intel iHD) ≻ libx265 |
| HEVC + 4:2:2 + 10-bit | VAAPI (Intel iHD) ≻ libx265 |
| HEVC + 4:4:4 + any | libx265 (HW backends reject NV24 today) |

Operators who need wire-level reproducibility (e.g. for compliance
deployments) pick an explicit backend and the manager UI cross-validates
the chroma + bit-depth combo before flow start.

## Video encode — pixel-format / bit-depth matrix

| Backend | 4:2:0 / 8 | 4:2:2 / 8 | 4:2:0 / 10 | 4:2:2 / 10 | 4:4:4 |
|---|:-:|:-:|:-:|:-:|:-:|
| **libx264** (CPU, `video-encoder-x264`) | ✓ | ✓ | ✓ | ✓ | ✓ |
| **libx265** (CPU, `video-encoder-x265`) | ✓ | ✓ | ✓ | ✓ | ✓ |
| **h264_nvenc** (NVIDIA, `video-encoder-nvenc`) | ✓ | ✗ | ✗ | ✗ | ✗ |
| **hevc_nvenc** (NVIDIA, `video-encoder-nvenc`) | ✓ | ✗ | ✓ | ✗ | ✗ |
| **h264_qsv** (Intel, `video-encoder-qsv`) | ✓ | ✗ | ✗ | ✗ | ✗ |
| **hevc_qsv** (Intel, `video-encoder-qsv`) | ✓ | ✗ | ✓ | ✗ | ✗ |
| **h264_vaapi** (Linux, `video-encoder-vaapi`) | ✓ | ✗ | ✗ | ✗ | ✗ |
| **hevc_vaapi** (Intel iHD Tiger Lake+) | ✓ | ✓ | ✓ | ✓ | ✗ (NV24 deferred) |
| **hevc_vaapi** (AMD radeonsi) | ✓ | usually ✗ | ✓ | usually ✗ | ✗ |

**Implication for broadcast 4:2:2 contribution:** on NVIDIA-only and
AMD-only hosts there is no GPU path for 4:2:2 10-bit; libx265 is the
fallback. Intel Tiger Lake+ iGPUs have the full broadcast HEVC matrix
through VAAPI.

Static rejection happens at config validation (`config/validation.rs`)
**and** at encoder open (`video_engine/src/video_encoder.rs`) so an
operator who bypasses the manager UI still gets a precise error before
any frame is encoded.

## Video decode — coverage by path

| Backend | Display output (KMS) | TS → TS transcode (input decode) | Notes |
|---|:-:|:-:|---|
| **libavcodec** (CPU) | ✓ (fallback) | ✓ (the universal path) | Every format, every bit-depth |
| **NVDEC** (`video-decoder-nvdec`) | ✓ zero-copy scanout | ✓ (Auto resolves it on NVIDIA hosts) | x86_64; 4:2:0 only |
| **QSV-decode** (`video-decoder-qsv`) | ✓ | ✓ (Auto resolves it on Intel iGPU hosts) | x86_64; 4:2:0 only |
| **VAAPI-decode** (`video-decoder-vaapi`) | ✓ zero-copy DMA-BUF | ✓ (Auto preferred on Intel iHD / AMD radeonsi) | Linux; Intel iHD does 4:2:2 + Main10 |

**Auto resolution priority** (display + transcode share the same
priority via [`resolve_display_decoder`] and [`resolve_transcode_decoder`]):

```
VAAPI ≻ NVDEC ≻ QSV ≻ CPU
```

The display path also relies on KMS atomic-commit zero-copy (VAAPI →
DRM PRIME); the transcode path doesn't get the same fast path but
benefits from offloading decode to dedicated silicon.

## Audio codec matrix

| Codec | Encode backend | Decode backend | Sample rates exposed in UI | Channels | Notes |
|---|---|---|---|---|---|
| **AAC-LC** | FDK-AAC in-process (`fdk-aac`) | FDK-AAC | 8 k–48 k | 1–8 (mono–7.1) | Production AAC; lowest latency |
| **HE-AAC v1** | FDK-AAC in-process | FDK-AAC | 8 k–48 k | 1–2 | SBR adds chroma-bandwidth dependency |
| **HE-AAC v2** | FDK-AAC in-process | FDK-AAC | 8 k–48 k | 2 (stereo only) | Parametric Stereo — manager UI hard-filters mono |
| **Opus** | libopus via libavcodec | libopus via libavcodec | 48 k | 1–2 | Decode wired (transcode-from-Opus-on-TS works) |
| **MP2** | libavcodec | libavcodec | 32 k / 48 k | 1–2 | DVB-T / SD broadcast |
| **AC-3** | libavcodec | libavcodec | 32 k / 44.1 k / 48 k | 1–6 (5.1) | ATSC / Blu-ray |
| **E-AC-3** | (passthrough only) | libavcodec | — | 1–7.1 | UHD ATSC 3.0 |
| **PCM (ST 2110-30 / -31)** | passthrough | passthrough | 48 k / 96 k | 1–16 | Hardcoded sample-rate in config; auto-detect deferred |
| **SMPTE 302M** | inline mux | inline demux | matches input | matches input | `transport_mode: audio_302m` on SRT / UDP / `rtp_audio` |

## Cross-architecture build matrix

| Host class | What `video-encoders-full` ships | What runtime probe activates |
|---|---|---|
| x86_64 NVIDIA Linux | x264 + x265 + NVENC + QSV + VAAPI + (NVDEC + QSV-dec + VAAPI-dec) | NVENC + NVDEC + (x264 / x265) |
| x86_64 AMD Linux | same | VAAPI encode + decode + (x264 / x265) |
| x86_64 Intel iGPU Linux | same | QSV + VAAPI (encode + decode) + (x264 / x265) |
| aarch64 NVIDIA (Jetson) | x264 + x265 + NVENC + NVDEC + VAAPI + VAAPI-dec (no QSV — Intel iGPU is x86_64-only) | NVENC + NVDEC + (x264 / x265) |
| aarch64 AMD APU SBC | same | VAAPI + (x264 / x265) |
| Apple Silicon (`aarch64-apple-darwin`) | VideoToolbox compiles cleanly via `cfg(target_os = "macos")` but is not in the release matrix today | (n/a) |

The aarch64-no-QSV decision is correct (Intel iGPU is x86-only).
Apple Silicon CI is a planned artefact channel.

## Capability strings on `HealthPayload.capabilities`

Edges advertise the following strings when the matching feature is
compiled in AND the runtime probe finds the underlying driver / GPU
usable. The manager UI keys per-output codec dropdown options off
these strings — anything that's missing is disabled in the UI with a
tooltip.

| Capability | Source | Meaning |
|---|---|---|
| `video-encode` | `video-encoder-{x264,x265,nvenc,qsv,vaapi}` (any) | Edge can re-encode video |
| `video-encoder-x264` | `video-encoder-x264` | libx264 available |
| `video-encoder-x265` | `video-encoder-x265` | libx265 available |
| `video-encoder-nvenc` | `video-encoder-nvenc` | h264_nvenc / hevc_nvenc available |
| `video-encoder-qsv` | `video-encoder-qsv` | h264_qsv / hevc_qsv available |
| `video-encoder-vaapi` | `video-encoder-vaapi` | h264_vaapi / hevc_vaapi available |
| `video-decoder-nvdec` | `video-decoder-nvdec` + runtime probe | NVDEC decode available (transcode + display) |
| `video-decoder-qsv` | `video-decoder-qsv` + runtime probe | QSV decode available |
| `video-decoder-vaapi` | `video-decoder-vaapi` + runtime probe | VAAPI decode available |
| `display-nvdec` (legacy) | `display-nvdec` | Older managers fall back on this for the display dropdown |
| `display-qsv` (legacy) | `display-qsv` | Same for QSV |
| `display-vaapi` (legacy) | `display-vaapi` | Same for VAAPI |
| `display` | `display` + ≥ 1 KMS connector enumerated | Local-display output usable |
| `fdk-aac` | `fdk-aac` | In-process AAC family |
| `media-codecs` | `media-codecs` (default on) | libavcodec for video decode + Opus / MP2 / AC-3 audio decode |
| `webrtc` | `webrtc` (default on) | WHIP / WHEP supported |
| `tls` | `tls` (default on) | HTTPS + RTMPS |
| `replay` | `replay` (default on) | Recording + clip playback |

The `resource_budget.hw_encoder_chroma` block on the same payload
carries the per-(codec, chroma, bit-depth) matrix with one boolean per
cell — that is the source of truth the manager UI reads when graying
out 4:2:2 chroma against NVENC, etc.

## Cost model — pixel-rate awareness

Per-flow cost units are computed by
[`engine::hardware_probe::compute_flow_cost_units`] off a
`FlowCostPlan` precomputed by [`engine::flow::derive_cost_plan`]. The
**video encode** units scale with pixel rate:

```
units = base × (width × height × fps) / (1920 × 1080 × 30)
        × 1.5    if bit_depth == 10
        × 1.33   if chroma == yuv422p
        × 2.0    if chroma == yuv444p
```

`base` is **100** for HW backends and **500** for SW. Floored at the
per-output baseline so a 240p test pattern stays at the per-output
weight; ceilinged at 100 000 to keep a misconfigured 16K120 flow from
overflowing the running total.

| Profile | Approx units (cost helper) |
|---|---|
| 720p30 H.264 4:2:0 8-bit on NVENC | ≈ 50 (clamped to 100 baseline) |
| 1080p30 H.264 4:2:0 8-bit on NVENC | 100 |
| 1080p30 HEVC 4:2:0 10-bit on NVENC | 150 |
| 4K30 H.264 4:2:0 8-bit on NVENC | 400 |
| 4K60 HEVC 4:2:2 10-bit on libx265 (broadcast contribution) | ~7 980 |
| 1080p30 H.264 4:2:0 8-bit on libx264 | 500 |
| 4K60 HEVC 4:2:0 8-bit on libx265 | 4000 |

The total per-host budget is `1000 + 200 × physical_cores`, so a 4-core
edge gets 1800 units (one 4K30 NVENC flow + headroom), a 32-core EPYC
gets 7400 (which fits the 4K60 4:2:2 contribution profile but only with
no other transcoding work).

## Verification commands per host class

End-to-end sanity check on each host class:

1. **Pixel-format probe truth** — start the edge and curl
   `/api/v1/stats/health`. Confirm:
   * Tiger Lake+ Intel iHD: `hw_encoder_chroma.hevc_vaapi_yuv422_10bit = true`
   * AMD radeonsi: `hw_encoder_chroma.hevc_vaapi_yuv422_10bit = false`
   * Anywhere with NVENC: every `hevc_nvenc_yuv422_*` cell is `false`.

2. **Auto resolution lands on the right backend.** Create a flow with
   `video_encode.codec = "h264_auto"`, chroma `yuv420p`, 8-bit. Watch
   the edge logs: the line
   `video_encode auto-resolved 'h264_auto' → <backend>` confirms the
   resolver picked the expected one for the host (NVENC on NVIDIA, QSV
   on Intel without VAAPI preference, VAAPI on AMD).

3. **4:2:2 10-bit transcode rejects on AMD, accepts on Intel iHD.**
   Create a flow with `video_encode.codec = "h264_nvenc"`, chroma
   `yuv422p`. Manager preflight returns HTTP 422 +
   `error_code: encoder_chroma_not_supported` before the WS round-trip.
   Switch the codec to `hevc_auto` — flow starts with `hevc_vaapi` on
   Intel iHD, `libx265` on AMD.

4. **HW transcode decode activates** when `video_encode.hw_decode =
   "auto"` (or unset — Auto is the default). Edge logs
   `ts_video_replace: opened HW decoder (backend=Vaapi)` (or NVDEC /
   QSV). Run `perf top` against the edge process and confirm
   `av_hwframe_transfer_data` is in the hot path (sysmem download) but
   `libavcodec_decode` is not (the source decode runs on the GPU).

5. **HDR-on-SDR-panel warning** — connect a non-HDR HDMI panel, route
   an HDR10 source through a `display` output. The manager Events tab
   should surface the
   `category: display, error_code: display_hdr_tonemap_active`
   warning exactly once per flow start.

6. **Resource impact tile reflects pixel rate.** Start one 720p30 H.264
   4:2:0 8-bit on NVENC and one 4K60 HEVC 4:2:2 10-bit on libx265 on
   the same host. The Resources card should show roughly a 100 : 8000
   unit ratio between the two flows.

7. **Cost-budget oversubscription warning fires** when the second
   instance of `h264_nvenc` exceeds the probed
   `nvenc_max_sessions`. Manager event:
   `category: system_resources, error_code: hw_encoder_oversubscribed`.

## Build channels

The release workflow ships two binary variants per architecture per tag:

* **`*-linux`** (`cargo build --release`) — AGPL-3.0-or-later, no
  software video encoders. For deployments that don't transcode video.
* **`*-linux-full`** (`cargo build --release --features
  video-encoders-full` on x86_64; the explicit feature list on
  aarch64) — bundles every video codec backend the edge knows about,
  including HW decoders for both display and transcode use. Combined
  work is AGPL with GPL terms on the GPL portions (see shipped
  `NOTICE.full`).

The `aarch64-linux-full` build omits QSV (Intel iGPU is x86_64-only).
Apple Silicon (`aarch64-apple-darwin`) is a planned artefact channel.
