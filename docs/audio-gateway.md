# Bilbycast Audio Gateway Guide

This guide covers Bilbycast as an **IP audio gateway**: bridging
PCM audio between studios, carrying radio contribution feeds over the
public internet, distributing talkback over encrypted SRT links, and
interoperating with the standard broadcast tool stack (`ffmpeg`,
`srt-live-transmit`, hardware encoders/decoders that speak SMPTE 302M
LPCM-in-MPEG-TS).

If you only want byte-identical SMPTE ST 2110-30/-31 passthrough on a
local broadcast plant, see [`st2110.md`](st2110.md) instead — that
remains the lowest-latency, zero-overhead path. This document is for
everything that ST 2110 *can't* do on its own: format conversion, no-PTP
operation, and WAN transport.

> **Why this exists.** Bilbycast originally shipped ST 2110 audio as
> strict byte-identical RTP passthrough — input and output format had to
> match exactly, and there was no signal processing on the audio path.
> That made it useless for the cases this guide covers. The audio
> gateway feature set turns that Phase 1 plumbing into a real audio
> gateway: per-output PCM transcoding, a generic `rtp_audio` variant
> with no PTP requirement, and SMPTE 302M LPCM-in-MPEG-TS over SRT, UDP,
> and RTP/MP2T.

---

## Table of Contents

- [What you can do with it](#what-you-can-do-with-it)
- [Audio inputs and outputs at a glance](#audio-inputs-and-outputs-at-a-glance)
- [The `transcode` block — per-output PCM conversion](#the-transcode-block--per-output-pcm-conversion)
  - [Allowed values](#allowed-values)
  - [Channel routing presets](#channel-routing-presets)
  - [SRC quality and dither](#src-quality-and-dither)
  - [IS-08 channel-map hot reload](#is-08-channel-map-hot-reload)
  - [Stats](#stats)
- [The `rtp_audio` input/output — generic PCM/RTP without ST 2110 baggage](#the-rtp_audio-inputoutput--generic-pcmrtp-without-st-2110-baggage)
- [SMPTE 302M LPCM in MPEG-TS — `transport_mode: "audio_302m"`](#smpte-302m-lpcm-in-mpeg-ts--transport_mode-audio_302m)
  - [Where it works](#where-it-works)
  - [Why 48 kHz only?](#why-48-khz-only)
  - [Mutually exclusive with FEC, program filtering, and 2022-7](#mutually-exclusive-with-fec-program-filtering-and-2022-7)
  - [Interop tests](#interop-tests)
- [Worked use cases](#worked-use-cases)
  - [1. Local AES67 → AES67 monitoring downmix (5.1 → stereo)](#1-local-aes67--aes67-monitoring-downmix-51--stereo)
  - [2. AES67 contribution Sydney → Perth via Bilbycast tunnel](#2-aes67-contribution-sydney--perth-via-bilbycast-tunnel)
  - [3. Bidirectional SRT talkback over the public internet](#3-bidirectional-srt-talkback-over-the-public-internet)
  - [4. Bilbycast → third-party SRT decoder (302M)](#4-bilbycast--third-party-srt-decoder-302m)
  - [5. Third-party SRT encoder → Bilbycast → AES67](#5-third-party-srt-encoder--bilbycast--aes67)
- [Flow groups](#flow-groups)
- [Validation rules quick reference](#validation-rules-quick-reference)
- [Limitations and deferred items](#limitations-and-deferred-items)

---

## What you can do with it

| Task | Tool inside Bilbycast |
|---|---|
| Bridge AES67 stereo to a 44.1 kHz / 16-bit ST 2110-30 monitor | `transcode` block on the output |
| Sum a 5.1 surround source to stereo for monitoring | `transcode.channel_map_preset: "5_1_to_stereo_bs775"` |
| Carry a radio feed over the public internet to a third-party decoder | SRT output with `transport_mode: "audio_302m"` |
| Receive a sine wave from `ffmpeg -c:a s302m -f mpegts srt://...` | SRT input with `transport_mode: "audio_302m"` *(deferred — see [Limitations](#limitations-and-deferred-items))* |
| Bridge audio between two studios that don't share a PTP fabric | `rtp_audio` input/output (no PTP requirement) |
| Hot-swap channel routing on a live monitor mix | IS-08 active channel map (`/x-nmos/channelmapping/v1.0/map/activate`) |
| Atomically start a multi-essence broadcast group (audio + ANC) | `start_flow_group` manager command |
| Send 24-bit LPCM to a hardware decoder that expects MPEG-TS over UDP | UDP output with `transport_mode: "audio_302m"` |
| Send 24-bit LPCM to a hardware decoder that expects RTP/MP2T (RFC 2250) | `rtp_audio` output with `transport_mode: "audio_302m"` |

---

## Audio inputs and outputs at a glance

| `type` | Direction | Wire format | PTP | Use case |
|---|---|---|---|---|
| `st2110_30` | Input + Output | RFC 3551 RTP, L16/L24 PCM, big-endian | Required for proper ST 2110-30 PM/AM profile | Local broadcast plant with shared PTP |
| `st2110_31` | Input + Output | RFC 3551 RTP, AES3 sub-frames in 24-bit | Required | Local plant transparent AES3 transport (preserves Dolby E) |
| `st2110_40` | Input + Output | RFC 8331 ANC | Required | SCTE-104 ad markers, SMPTE 12M timecode, captions |
| `rtp_audio` | Input + Output | RFC 3551 RTP, L16/L24 PCM | **Not required** | WAN contribution, talkback, ffmpeg/OBS interop |
| `srt` (with `transport_mode: audio_302m`) | Input *(deferred)* / Output | SMPTE 302M LPCM in MPEG-TS, over SRT | N/A | WAN transport with ARQ, ffmpeg/srt-live-transmit interop |
| `udp` (with `transport_mode: audio_302m`) | Output | SMPTE 302M LPCM in MPEG-TS, over UDP | N/A | Legacy hardware that expects raw TS over UDP |
| `rtp_audio` (with `transport_mode: audio_302m`) | Output | SMPTE 302M LPCM in MPEG-TS, wrapped in RFC 2250 RTP/MP2T (PT 33) | N/A | Hardware that expects MPEG-TS over RTP |

The `rtp_audio` input/output is wire-identical to ST 2110-30 (same RTP
header, same big-endian L16/L24 payload) but with relaxed validation —
sample rates 32 / 44.1 / 48 / 88.2 / 96 kHz, no `clock_domain`
requirement, no NMOS PTP advertising, no RFC 7273 timing reference.
Internally it shares the ST 2110-30 runtime with `clock_domain` forced
off so the same transcode stage, IS-08 hot reload, SMPTE 2022-7
redundancy, and per-output stats wiring all work for free.

---

## The `transcode` block — per-output PCM conversion

Every audio output (`st2110_30`, `st2110_31`, `rtp_audio`) accepts an
optional `transcode` field. When present, Bilbycast inserts a per-output
PCM conversion stage between the broadcast channel subscriber and the
RTP send loop:

```text
flow input (RFC 3551 RTP, any rate/depth/channels)
        │
        ▼
broadcast::channel<RtpPacket>
        │  subscribe
        ▼
┌──────────────────────────────────────┐
│  TranscodeStage (per output)         │
│  ┌────────────────────────────────┐  │
│  │ PcmDepacketizer                │  │
│  │   ↓                            │  │
│  │ decode big-endian PCM → f32    │  │
│  │   ↓                            │  │
│  │ apply channel matrix           │  │
│  │   (IS-08 active or static)    │  │
│  │   ↓                            │  │
│  │ rubato SRC (sinc or fast)      │  │
│  │   ↓                            │  │
│  │ encode f32 → big-endian PCM    │  │
│  │   (TPDF dither on down-cvt)    │  │
│  │   ↓                            │  │
│  │ PcmPacketizer                  │  │
│  └────────────────────────────────┘  │
└──────────────┬───────────────────────┘
               │
               ▼
       RTP socket send (Red + Blue legs)
```

When `transcode` is **absent**, the output runs the existing
byte-identical passthrough path — no allocation, no signal processing,
zero overhead. The transcoder only ever runs when the operator opts in
on a specific output.

### Allowed values

```json
{
  "transcode": {
    "sample_rate": 44100,
    "bit_depth": 16,
    "channels": 2,
    "channel_map": [[0], [1]],
    "channel_map_preset": null,
    "packet_time_us": 4000,
    "payload_type": 96,
    "src_quality": "high",
    "dither": "tpdf"
  }
}
```

| Field | Allowed values | Default | Notes |
|---|---|---|---|
| `sample_rate` | `32000`, `44100`, `48000`, `88200`, `96000` | input sample rate | Pass-through if equal to input |
| `bit_depth` | `16`, `20`, `24` | input bit depth | L20 is carried as L24 with the bottom 4 bits zero per RFC 3190 §4.5 |
| `channels` | `1`..=`16` | input channel count | Must agree with `channel_map` length |
| `channel_map` | `[[in_ch, ...], ...]` | identity (or auto-promote) | One row per output channel; each row lists input channel indices to sum (unity gain) |
| `channel_map_preset` | one of the named presets | none | Mutually exclusive with `channel_map` |
| `packet_time_us` | `125`, `250`, `333`, `500`, `1000`, `4000` | `1000` | 4 ms is a sensible default for talkback / WAN |
| `payload_type` | `96`..=`127` | `97` | RTP dynamic payload type |
| `src_quality` | `"high"`, `"fast"` | `"high"` | High = `rubato::SincFixedIn` (sinc, broadcast quality). Fast = `rubato::FastFixedIn` (polynomial, lower latency). |
| `dither` | `"tpdf"`, `"none"` | `"tpdf"` | TPDF triangular dither on bit-depth downconversion. `"none"` truncates. |

`channel_map` and `channel_map_preset` are mutually exclusive. If both
are absent and the input/output channel counts agree, the matrix
defaults to identity. If they differ by exactly mono→stereo (1→2) or
stereo→mono (2→1), a sensible default preset is auto-applied. Any other
shape mismatch must be specified explicitly via `channel_map` or
`channel_map_preset` — the validator rejects ambiguous configs at config
load time.

### Channel routing presets

| Preset | Input ch | Output ch | Math |
|---|---|---|---|
| `mono_to_stereo` | 1 | 2 | L = R = ch0 |
| `stereo_to_mono_3db` | 2 | 1 | mono = (L + R) × 0.7071 (~ −3 dB) |
| `stereo_to_mono_6db` | 2 | 1 | mono = (L + R) × 0.5 (~ −6 dB) |
| `5_1_to_stereo_bs775` | 6 | 2 | ITU-R BS.775: Lt = L + (−3 dB)·C + (−3 dB)·Ls; Rt = R + (−3 dB)·C + (−3 dB)·Rs (LFE dropped). 5.1 channel order: L, R, C, LFE, Ls, Rs |
| `7_1_to_stereo_bs775` | 8 | 2 | ITU-R BS.775 extended: Lt = L + (−3 dB)·C + (−3 dB)·Lss + (−3 dB)·Lrs; Rt mirrors. 7.1 channel order: L, R, C, LFE, Lss, Rss, Lrs, Rrs |
| `4ch_to_stereo_lt_rt` | 4 | 2 | Lt = L + (−3 dB)·Ls; Rt = R + (−3 dB)·Rs. Quad order: L, R, Ls, Rs |

Need a custom routing matrix? Use `channel_map` directly. For example,
to route a 4-channel input where you want output ch0 = (in0 + in2) and
output ch1 = (in1 + in3):

```json
"channel_map": [[0, 2], [1, 3]]
```

The current schema treats every routed input as unity gain. If you need
non-unity gains in a custom matrix today, use one of the named presets
(which apply −3 dB / −6 dB internally) — first-class JSON support for
per-entry gains is on the roadmap.

### SRC quality and dither

Bilbycast uses [`rubato`](https://docs.rs/rubato) for sample rate
conversion. Two profiles:

- **`high`** — `SincFixedIn` with a 256-tap windowed sinc filter and
  256× oversampling. Broadcast monitoring quality. ~3-5 ms of conversion
  delay. CPU cost is several × the `fast` profile. **Default.**
- **`fast`** — `FastFixedIn` with linear polynomial interpolation and
  64-tap sinc. Lower CPU, ~1 ms latency, audibly worse on critical
  content. Use for talkback / IFB paths where pristine fidelity is not
  required.

Bit-depth down-conversion (e.g., L24 → L16) applies **TPDF**
(triangular probability density function) dither by default to break
quantization correlation. Set `dither: "none"` to truncate instead —
faster, slightly worse quality, only do this when the downstream is
going to dither anyway.

### IS-08 channel-map hot reload

When the operator activates a new IS-08 channel map via the NMOS REST
API (`POST /x-nmos/channelmapping/v1.0/map/activate`), every running
audio output's transcoder picks up the new routing on the next packet —
no flow restart, no audio glitch, no listener disconnect.

How it works under the hood:

1. `Is08State` holds a `tokio::sync::watch::Sender<Arc<ChannelMap>>`.
2. On activation, the new map is pushed through the watch channel.
3. Each per-output `TranscodeStage` holds a
   `MatrixSource::Is08Tracked` value with a `watch::Receiver` and the
   output's IS-08 id (e.g., `st2110_30:my-flow:my-output`).
4. On every packet, the transcoder calls `rx.has_changed()` (a single
   atomic load). If `false`, the cached per-output matrix is reused
   (an `Arc` clone). If `true`, the receiver snapshots the new map and
   recomputes the matrix once.

Steady-state cost on the hot path: one atomic load + one `Arc::clone`.
No locks, no allocations.

The per-output **static** `channel_map` from the operator's `transcode`
config is the **fallback** — if the IS-08 map has no entry for this
output, or its entry has zero channels, the static matrix is used.
Cross-input routing in IS-08 (where one output channel references a
different upstream input) is treated as muted on this output's
transcoder — single-flow audio routing only, by design.

### Stats

When a transcoder is active on an output, the per-output stats snapshot
gains a `transcode_stats` block:

```json
{
  "output_id": "monitor-stereo",
  "packets_sent": 12000,
  "bytes_sent": 3456000,
  "transcode_stats": {
    "input_packets": 12000,
    "output_packets": 11999,
    "dropped": 0,
    "format_resets": 0,
    "last_latency_us": 1820
  }
}
```

| Field | Meaning |
|---|---|
| `input_packets` | RTP packets the transcoder pulled from the broadcast channel |
| `output_packets` | RTP packets the transcoder emitted (may differ from input due to packet-time conversion) |
| `dropped` | Packets dropped inside the transcoder (decode error, malformed RTP, resampler failure). Distinct from broadcast-channel lag drops |
| `format_resets` | Times the transcoder rebuilt internal state due to a detected upstream format change |
| `last_latency_us` | Most recent end-to-end transcoder latency, microseconds (input recv timestamp → emit timestamp) |

When `transcode` is absent, `transcode_stats` is omitted from the
snapshot — the wire format stays clean for passthrough flows.

---

## The `rtp_audio` input/output — generic PCM/RTP without ST 2110 baggage

`rtp_audio` is the no-PTP variant of ST 2110-30. Same wire format
(RFC 3551 RTP + big-endian L16/L24 PCM payload), relaxed constraints:

- Sample rate: any of **32000, 44100, 48000, 88200, 96000 Hz**
- Bit depth: **16 or 24**
- Channels: **1..=16**
- Packet time: 125 / 250 / 333 / 500 / 1000 / 4000 / 20000 µs
- Dynamic payload type: 96..=127
- **No PTP requirement**, no RFC 7273 timing reference, no NMOS
  `clock_domain` advertising

Use it for radio contribution feeds over the public internet, talkback
between studios that don't share a PTP fabric, ffmpeg / OBS / GStreamer
interop, and any general PCM-over-RTP source where ST 2110-30's PTP
assumption is overkill.

### Example: `rtp_audio` input

```json
{
  "id": "perth-contribution-rx",
  "name": "Perth contribution receiver",
  "input": {
    "type": "rtp_audio",
    "bind_addr": "0.0.0.0:5004",
    "sample_rate": 48000,
    "bit_depth": 24,
    "channels": 2,
    "packet_time_us": 1000,
    "payload_type": 97
  },
  "outputs": []
}
```

### Example: `rtp_audio` output with transcoded downmix

```json
{
  "type": "rtp_audio",
  "id": "monitor-mix",
  "name": "Monitor stereo to control room",
  "dest_addr": "239.10.20.1:5004",
  "sample_rate": 48000,
  "bit_depth": 24,
  "channels": 2,
  "packet_time_us": 1000,
  "payload_type": 97,
  "transcode": {
    "sample_rate": 44100,
    "bit_depth": 16,
    "channels": 2,
    "channel_map_preset": "5_1_to_stereo_bs775"
  }
}
```

`rtp_audio` outputs share the `transcode` block exactly with ST 2110-30
outputs and reuse the same shared runtime — the only difference is that
the synthesized internal config has `clock_domain: None`, so the PTP
reporter and any NMOS `clock_domain` advertising are skipped.

---

## SMPTE 302M LPCM in MPEG-TS — `transport_mode: "audio_302m"`

SMPTE 302M is the broadcast industry standard for carrying lossless PCM
audio inside MPEG-2 transport streams. It avoids any audio codec
dependency (no AAC, no AC-3, no MP2 — none of which have
production-grade pure-Rust encoders today), and it's exactly what
hardware encoders, hardware decoders, and the standard ffmpeg /
srt-live-transmit tool stack expect for lossless audio over MPEG-TS
contribution links.

Bilbycast packs 48 kHz LPCM (16 / 20 / 24 bit, 2 / 4 / 6 / 8 channels)
as 302M private PES inside a single-program MPEG-TS, with the PMT
carrying the `BSSD` (`0x42535344`) registration descriptor that
identifies the elementary stream as 302M. The bit packing matches
ffmpeg's `libavcodec/s302menc.c` byte-for-byte; round trips through
Bilbycast's internal `S302mPacketizer` ↔ `S302mDepacketizer` are
sample-exact at 16-bit and within ±1 LSB at 24-bit (verified by unit
tests).

### Where it works

| Output | `transport_mode` | What it sends |
|---|---|---|
| `srt` | `"audio_302m"` | 7 × 188 byte (1316-byte) MPEG-TS chunks over an SRT live socket |
| `udp` | `"audio_302m"` | 7 × 188 byte MPEG-TS chunks as plain UDP datagrams |
| `rtp_audio` | `"audio_302m"` | 7 × 188 byte MPEG-TS chunks wrapped in an RFC 2250 RTP/MP2T packet (payload type 33) |

In all three modes, the runtime instantiates an internal pipeline:

```text
Transcode (force 48 kHz, even channels, 16/20/24-bit)
  → S302mPacketizer (4-byte AES3 header + bit-packed samples per pair)
  → TsMuxer.mux_private_audio (BSSD PMT + private PES on AUDIO_PID)
  → 1316-byte chunk bundling
  → SRT/UDP/RTP socket
```

When the upstream input is **not** 48 kHz, Bilbycast's transcode stage
(see above) automatically resamples it to 48 kHz before 302M
packetization — that's the explicit design choice, since 302M is
48-kHz-only per the spec. Mono inputs are auto-promoted to stereo;
odd-channel inputs are rounded up to the next even count (capped at 8
to stay within 302M's channel limits).

### Why 48 kHz only?

Per SMPTE 302M-2007, the audio sampling frequency is **48 kHz, full
stop**. Combined with Bilbycast's transcode stage, the design pattern
is: a 44.1 kHz radio feed gets resampled up to 48 kHz before 302M
packetization, transported over SRT, and the receiving end resamples
back down to 44.1 kHz if it cares. This is exactly what professional
broadcast contribution links already do, and it interoperates cleanly
with every 302M-aware decoder in the wild.

### Mutually exclusive with FEC, program filtering, and 2022-7

The `audio_302m` mode is **mutually exclusive** with:

- `packet_filter` (SRT FEC) — Bilbycast's SRT FEC is XOR over TS payload
  bytes and doesn't usefully protect audio elementary stream content.
- `program_number` — 302M emits a single-program TS, by definition.
- SMPTE 2022-7 redundancy on the SRT output — the 302M output path
  doesn't yet duplicate to a second leg.

Validation rejects all three combinations at config load time. If you
need wire-level protection on a 302M-over-SRT link, rely on SRT's own
ARQ; if you need redundancy, run two parallel flows on independent
links.

### Interop tests

Four runnable shell scripts in
`testbed/audio-tests/302m-interop/` exercise the wire boundary against
the standard tool stack:

| Script | Direction | Status |
|---|---|---|
| `01-ffmpeg-to-bilbycast-srt.sh` | ffmpeg `-c:a s302m -f mpegts srt://...` → Bilbycast SRT input | **SKIPPED** until SRT 302M input demux lands (see [Limitations](#limitations-and-deferred-items)) |
| `02-bilbycast-to-ffmpeg.sh` | Bilbycast SRT 302M output → ffmpeg consumer → WAV verify | **READY** |
| `03-bilbycast-via-slt.sh` | Bilbycast → `srt-live-transmit` → Bilbycast | **SKIPPED** (depends on input demux) |
| `04-bilbycast-to-tee-ffprobe.sh` | Bilbycast → `srt-live-transmit` → `.ts` file → `ffprobe` codec verify | **READY** |

Each script auto-detects whether `ffmpeg` and `srt-live-transmit` are
installed; if either is missing, the script prints `SKIPPED:` and exits
0 so it can be run safely inside CI without forcing every CI worker to
have the broadcast tool stack installed.

To run all four tests on a host with ffmpeg and srt-live-transmit:

```bash
cd testbed/audio-tests/302m-interop/
for s in *.sh; do bash "$s"; done
```

The tests rely on `testbed/configs/audio/bilbycast-302m-out.json`,
which defines a flow that ingests an `rtp_audio` input on `0.0.0.0:5004`
and emits SRT 302M on `127.0.0.1:9000`.

---

## Worked use cases

### 1. Local AES67 → AES67 monitoring downmix (5.1 → stereo)

A 5.1 surround source is multicast on the studio's AES67 fabric at
L24/48k/6 ch. The monitoring desk needs L24/48k/2 ch stereo. Single
edge node, single flow, downmix happens in the output.

```json
{
  "id": "monitoring-downmix",
  "name": "5.1 → Monitoring stereo",
  "enabled": true,
  "clock_domain": 0,
  "input": {
    "type": "st2110_30",
    "bind_addr": "239.0.0.10:5004",
    "interface_addr": "10.0.0.5",
    "sample_rate": 48000,
    "bit_depth": 24,
    "channels": 6,
    "packet_time_us": 1000,
    "payload_type": 97
  },
  "outputs": [
    {
      "type": "st2110_30",
      "id": "monitor-stereo",
      "name": "Monitoring desk stereo",
      "dest_addr": "239.0.0.20:5004",
      "interface_addr": "10.0.0.5",
      "sample_rate": 48000,
      "bit_depth": 24,
      "channels": 2,
      "packet_time_us": 1000,
      "payload_type": 97,
      "dscp": 46,
      "transcode": {
        "channels": 2,
        "channel_map_preset": "5_1_to_stereo_bs775"
      }
    }
  ]
}
```

Notes:
- `transcode` only overrides what differs — sample_rate and bit_depth
  pass through unchanged.
- The transcoder runs inside the output spawn module, so the input task
  is unaffected. Other outputs on the same flow can be a full 5.1
  passthrough at zero overhead.
- Latency through the transcoder is reported in
  `output_stats.transcode_stats.last_latency_us`.

### 2. AES67 contribution Sydney → Perth via Bilbycast tunnel

The source studio in Sydney has an AES67 stereo feed at L24/48k. The
destination radio station in Perth wants L16/44.1k/2ch. The two edges
are connected by a Bilbycast QUIC tunnel through a relay.

**Sydney edge** — emits a `rtp_audio` flow into the tunnel:

```json
{
  "id": "sydney-to-perth",
  "name": "Sydney → Perth contribution",
  "enabled": true,
  "input": {
    "type": "st2110_30",
    "bind_addr": "239.0.0.10:5004",
    "interface_addr": "10.0.0.5",
    "sample_rate": 48000,
    "bit_depth": 24,
    "channels": 2,
    "packet_time_us": 1000,
    "payload_type": 97,
    "clock_domain": 0
  },
  "outputs": [
    {
      "type": "rtp_audio",
      "id": "to-tunnel",
      "name": "Forward to Perth via tunnel",
      "dest_addr": "127.0.0.1:7000",
      "sample_rate": 48000,
      "bit_depth": 24,
      "channels": 2,
      "packet_time_us": 4000,
      "payload_type": 97
    }
  ]
}
```

The tunnel forwards `127.0.0.1:7000` to the Perth edge as a UDP stream.

**Perth edge** — receives `rtp_audio` from the tunnel and re-emits as
ST 2110-30 with the transcode block doing the format change:

```json
{
  "id": "perth-from-sydney",
  "name": "Perth ← Sydney contribution",
  "enabled": true,
  "input": {
    "type": "rtp_audio",
    "bind_addr": "127.0.0.1:7000",
    "sample_rate": 48000,
    "bit_depth": 24,
    "channels": 2,
    "packet_time_us": 4000,
    "payload_type": 97
  },
  "outputs": [
    {
      "type": "st2110_30",
      "id": "perth-monitor",
      "name": "Perth monitoring",
      "dest_addr": "239.20.0.10:5004",
      "interface_addr": "10.20.0.5",
      "sample_rate": 44100,
      "bit_depth": 16,
      "channels": 2,
      "packet_time_us": 1000,
      "payload_type": 97,
      "dscp": 46,
      "transcode": {
        "sample_rate": 44100,
        "bit_depth": 16,
        "channels": 2
      }
    }
  ]
}
```

Notes:
- The `transcode` block on the Perth edge does the SRC + bit-depth
  conversion. With `src_quality: "high"` (default) the transcoder uses
  windowed sinc resampling — broadcast quality.
- A 4 ms packet time on the tunnel reduces per-packet header overhead;
  the Perth edge's transcoder takes care of repacketizing back to 1 ms
  for the local monitor multicast.
- See `bilbycast-edge/docs/configuration-guide.md` for the tunnel
  configuration syntax.

### 3. Bidirectional SRT talkback over the public internet

Two production sites need to talk back and forth over the public
internet. Each site has a Bilbycast edge plus an AES67 mic and a monitor
return. Each site runs **two** flows — one outbound (mic → SRT 302M),
one inbound (SRT 302M → AES67 monitor).

**Site A — outbound (mic to remote site)**

```json
{
  "id": "site-a-talkback-out",
  "name": "Site A mic → Site B talkback",
  "input": {
    "type": "st2110_30",
    "bind_addr": "239.0.0.30:5004",
    "interface_addr": "10.0.0.5",
    "sample_rate": 48000,
    "bit_depth": 24,
    "channels": 2,
    "packet_time_us": 1000,
    "payload_type": 97,
    "clock_domain": 0
  },
  "outputs": [
    {
      "type": "srt",
      "id": "talkback-out",
      "name": "SRT 302M to Site B",
      "mode": "caller",
      "local_addr": "0.0.0.0:0",
      "remote_addr": "site-b.example.com:9000",
      "latency_ms": 80,
      "transport_mode": "audio_302m"
    }
  ]
}
```

**Site A — inbound (Site B's mic appearing as a local AES67 multicast)**

```json
{
  "id": "site-a-talkback-in",
  "name": "Site B talkback → Site A monitor",
  "input": {
    "type": "srt",
    "mode": "listener",
    "local_addr": "0.0.0.0:9001",
    "latency_ms": 80,
    "transport_mode": "audio_302m"
  },
  "outputs": [
    {
      "type": "st2110_30",
      "id": "talkback-monitor",
      "name": "Talkback monitor to control room",
      "dest_addr": "239.0.0.40:5004",
      "interface_addr": "10.0.0.5",
      "sample_rate": 48000,
      "bit_depth": 24,
      "channels": 2,
      "packet_time_us": 1000,
      "payload_type": 97,
      "dscp": 46
    }
  ]
}
```

> **Note:** The inbound flow above (`type: srt` with
> `transport_mode: audio_302m`) needs the SRT 302M *input demux*, which
> is a deferred follow-up from the Audio Gateway phase. Until that
> lands, you can still run the OUTBOUND half of bidirectional talkback
> end-to-end. Track the deferral in [Limitations](#limitations-and-deferred-items).

Site B mirrors Site A with destinations swapped. Each direction is an
independent SRT connection with its own latency budget and ARQ. Aim for
80 ms one-way latency on a 30 ms RTT link.

### 4. Bilbycast → third-party SRT decoder (302M)

A radio broadcaster's existing playout chain expects MPEG-TS over SRT
with SMPTE 302M LPCM. Bilbycast ingests an AES67 feed from the studio
and emits SRT 302M directly to the decoder.

```json
{
  "id": "playout-feed",
  "name": "Studio → playout",
  "enabled": true,
  "input": {
    "type": "st2110_30",
    "bind_addr": "239.0.0.10:5004",
    "interface_addr": "10.0.0.5",
    "sample_rate": 48000,
    "bit_depth": 24,
    "channels": 2,
    "packet_time_us": 1000,
    "payload_type": 97,
    "clock_domain": 0
  },
  "outputs": [
    {
      "type": "srt",
      "id": "to-playout",
      "name": "Playout decoder",
      "mode": "caller",
      "local_addr": "0.0.0.0:0",
      "remote_addr": "playout-decoder.example.com:9000",
      "latency_ms": 200,
      "transport_mode": "audio_302m"
    }
  ]
}
```

You can verify the receiving end with `ffprobe`:

```bash
srt-live-transmit srt://playout-decoder.example.com:9000 file://capture.ts &
sleep 5; kill %1
ffprobe -v error -select_streams a -show_entries \
    stream=codec_name,sample_rate,channels,bits_per_raw_sample \
    -of default=nw=1 capture.ts
```

Expected output:

```
codec_name=s302m
sample_rate=48000
channels=2
bits_per_raw_sample=24
```

This is exactly what `testbed/audio-tests/302m-interop/04-bilbycast-to-tee-ffprobe.sh`
automates.

### 5. Third-party SRT encoder → Bilbycast → AES67

A field encoder (or `ffmpeg -c:a s302m`) sends L24/48k/2ch via SRT 302M
to Bilbycast. Bilbycast receives, optionally transcodes, and emits
AES67 onto the local studio plant.

> **Note:** This use case depends on the SRT 302M *input demux*, which
> is a deferred follow-up from the Audio Gateway phase. The config
> shape below is what it will look like once the demux lands; the
> validator already accepts it today.

```json
{
  "id": "field-feed",
  "name": "Field encoder → studio AES67",
  "enabled": true,
  "input": {
    "type": "srt",
    "mode": "listener",
    "local_addr": "0.0.0.0:9000",
    "latency_ms": 250,
    "transport_mode": "audio_302m"
  },
  "outputs": [
    {
      "type": "st2110_30",
      "id": "field-monitor",
      "name": "Field feed on AES67",
      "dest_addr": "239.0.0.50:5004",
      "interface_addr": "10.0.0.5",
      "sample_rate": 48000,
      "bit_depth": 24,
      "channels": 2,
      "packet_time_us": 1000,
      "payload_type": 97,
      "dscp": 46
    }
  ]
}
```

---

## Flow groups

A flow group binds multiple per-essence flows on the same edge into a
single logical unit. The classic use case is "audio + ANC + (future)
video share a PTP clock domain and must activate together".

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flow_groups": [
    {
      "id": "studio-a-program",
      "name": "Studio A program",
      "clock_domain": 0,
      "flow_ids": ["studio-a-stereo", "studio-a-anc"]
    }
  ],
  "flows": [
    { "id": "studio-a-stereo", "...": "ST 2110-30 audio flow" },
    { "id": "studio-a-anc",   "...": "ST 2110-40 ANC flow" }
  ]
}
```

The audio gateway phase added two new manager protocol commands that
operate on flow groups:

| Command | Edge action |
|---|---|
| `start_flow_group { flow_group_id }` | Spawns every member flow's `FlowRuntime::start` in **parallel**. If any member fails, every member that did start is rolled back via `destroy_flow` and the command returns an error (all-or-nothing). |
| `stop_flow_group { flow_group_id }` | Best-effort parallel `destroy_flow` for every member. Individual member failures are logged but do not abort the rest. |

These commands are dispatched the same way as `add_flow_group` /
`remove_flow_group` (which only persist to disk) — the new pair triggers
the runtime. Older edges that don't recognize them fall through to the
existing `Unknown command` arm, so the protocol version is not bumped.

> **Caveat — IS-05 group barrier.** Atomicity here means
> "FlowManager-perspective atomic" (every member's `FlowRuntime::start`
> future completes within roughly one tokio scheduler tick). The IS-05
> grouped *activation* barrier — where multiple receivers in a group
> wait on a shared `tokio::sync::Notify` so they apply staged transport
> params at the same `activation_time` — is a deferred follow-up. See
> [Limitations](#limitations-and-deferred-items).

---

## Validation rules quick reference

The validator runs at config load time and on every `update_config`
manager command. Failed validation rejects the change without touching
the running flows.

| Field | Allowed values |
|---|---|
| `transcode.sample_rate` | 32000, 44100, 48000, 88200, 96000 |
| `transcode.bit_depth` | 16, 20, 24 |
| `transcode.channels` | 1..=16 |
| `transcode.channel_map.length` | must equal `transcode.channels` |
| `transcode.channel_map[i][j]` | must be `< input.channels` |
| `transcode.channel_map_preset` | `mono_to_stereo`, `stereo_to_mono_3db`, `stereo_to_mono_6db`, `5_1_to_stereo_bs775`, `7_1_to_stereo_bs775`, `4ch_to_stereo_lt_rt` |
| `transcode.channel_map` + `transcode.channel_map_preset` | mutually exclusive |
| `transcode.packet_time_us` | 125, 250, 333, 500, 1000, 4000 |
| `transcode.payload_type` | 96..=127 |
| `transcode.src_quality` | `"high"`, `"fast"` |
| `transcode.dither` | `"tpdf"`, `"none"` |
| `rtp_audio.sample_rate` | 32000, 44100, 48000, 88200, 96000 |
| `rtp_audio.bit_depth` | 16, 24 |
| `rtp_audio.channels` | 1..=16 |
| `rtp_audio.packet_time_us` | 125, 250, 333, 500, 1000, 4000, 20000 |
| SRT/UDP/`rtp_audio` `transport_mode` | `"ts"` (default — UDP/SRT) or `"rtp"` (default — `rtp_audio`) or `"audio_302m"` |
| SRT `transport_mode == "audio_302m"` | rejects `packet_filter`, `program_number`, `redundancy` |
| UDP `transport_mode == "audio_302m"` | rejects `program_number` |
| SMPTE 302M channel count (when 302M mode active) | 2, 4, 6, 8 (Bilbycast auto-promotes mono → stereo) |

---

## Limitations and deferred items

These are **known gaps** in the current Audio Gateway phase. They're
documented so you can plan around them and so a follow-up commit can
land them without surprise. None of them block the use cases in this
guide that are marked READY.

1. **SRT 302M input demux.** The SRT input does not yet recognize
   incoming SMPTE 302M-in-MPEG-TS streams and demux them back to RTP
   audio. The plumbing exists (`SrtInputConfig.transport_mode`,
   `engine::audio_302m::S302mDepacketizer`, the `ts_parse` PMT helpers),
   but the wiring inside `input_srt.rs` to walk the PMT, locate the
   BSSD elementary stream, reassemble PES, depacketize the LPCM, and
   republish as RTP audio is not yet written. Until then, interop tests
   01 and 03 in `testbed/audio-tests/302m-interop/` print `SKIPPED`,
   and use cases 3 (inbound half) and 5 above are documented but
   inactive at runtime. Use case 4 (output direction) and tests 02 and
   04 fully work today.

2. **IS-05 grouped activation barrier.** `FlowManager::start_flow_group`
   provides "all-or-nothing from the FlowManager perspective" — every
   member's `FlowRuntime::start` future completes within roughly one
   tokio scheduler tick. But the IS-05 staging path does not yet gate
   immediate activations on a per-group `tokio::sync::Notify` barrier:
   each receiver in a group still applies its staged transport params
   independently. For most operational use this is fine (the spread is
   tens of milliseconds at worst), but for strict NMOS controllers that
   expect a single shared `activation_time` across grouped receivers,
   the barrier extension is on the roadmap.

3. **SMPTE 2022-7 redundancy on the SRT 302M output.** The standard ST
   2110-30 / -31 output supports 2022-7 dual-leg duplication (the
   transcoder runs once and feeds both Red and Blue legs from the
   post-transcode buffer). The SRT 302M output **does not** — its
   pipeline is single-leg. If you need wire-level redundancy on a 302M
   contribution link, run two parallel flows on independent SRT
   connections. The validator rejects `transport_mode == "audio_302m"`
   combined with `redundancy` to surface this constraint at config
   load time.

4. **Manager UI: edit-modal populator for the transcode block.** The
   create-flow path in the manager UI fully supports the new
   `transcode` block and the SRT `transport_mode` selector. The
   edit-flow path round-trips raw JSON correctly (so the persisted
   config is preserved across edits) but the form fields stay empty
   when re-opening an existing flow with a `transcode` block — you'll
   need to re-enter the values or edit raw JSON in the YAML view. The
   create-flow path is the primary surface and is fully wired.

5. **Manager UI: dedicated `rtp_audio` option in the output-type
   dropdown.** The runtime supports `OutputConfig::RtpAudio` end-to-end,
   but the manager UI dropdown still hides it behind raw JSON. Edit the
   flow config directly to use `"type": "rtp_audio"` for now.

6. **Custom channel-map gains in JSON.** The `transcode.channel_map`
   field currently treats every routed input as unity gain. For
   non-unity routing today, use one of the named presets (which apply
   −3 dB / −6 dB internally). First-class JSON support for per-entry
   gains (`[[in_ch, gain], ...]`) is on the roadmap.

7. **L20 wire format.** L20 is accepted by the validator and the
   transcoder; it's serialized on the wire as L24 with the bottom 4
   bits zeroed per RFC 3190 §4.5. If you specifically need L20-aware
   receivers to advertise L20 in their SDP, raise this with the
   maintainer — the on-the-wire bytes are correct but the NMOS
   advertisement may need a follow-up to surface L20 explicitly.

8. **Latency benchmark numbers per use case.** The plan called for
   measured `transcode_latency_us` and glass-to-glass numbers per use
   case to be reported here. Those need a real test bench (or `tc qdisc`
   network simulation) and will be added once the bench is set up. The
   `transcode_stats.last_latency_us` field is plumbed end-to-end and
   you can read it today via `GET /api/v1/stats`.

For the implementation history and the chunk-by-chunk breakdown of how
the audio gateway feature set was built, see
[`AUDIO_GATEWAY_REPORT.md`](../../AUDIO_GATEWAY_REPORT.md) at the
monorepo root.
