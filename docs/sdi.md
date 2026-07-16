# Native SDI I/O (Blackmagic DeckLink)

bilbycast-edge can capture SDI directly from Blackmagic DeckLink cards —
video plus embedded audio, encoded in-process and published as a standard
A+V MPEG-TS flow — with no external SDI→IP converter. Gated behind the
`sdi-decklink` Cargo feature (default **off**), backed by the sibling
[`bilbycast-decklink-rs`](https://github.com/Bilbycast/bilbycast-decklink-rs)
crate. Upstream tracking issue:
[Bilbycast/bilbycast-edge#19](https://github.com/Bilbycast/bilbycast-edge/issues/19).

| Capability | Status |
|------------|--------|
| SDI input, video (UYVY422 8-bit, any raster the card detects) | **Verified on hardware** (1080i50, DeckLink Quad) |
| SDI input, embedded audio (2 ch → AAC) | **Verified on hardware** (8/16 ch untested) |
| SCTE-104 VANC extraction → SCTE-35 PID | Implemented and unit-verified; live VANC source verification pending |
| Signal-loss / raster-change / device-loss resilience | **Verified by physically pulling the cable** |
| Per-port hardware status on `HealthPayload` | **Verified** (all 8 ports, live during capture) |
| Encoder backends | All of them — x264 / x265 / NVENC / QSV / VAAPI / `h264_auto` / `hevc_auto` (x264 + NVENC hardware-verified; QSV/VAAPI compile-verified) |
| SDI output (playout), video | Edge-integrated (`OutputConfig::Sdi`): broadcast → demux → decode → UYVY repack → card-clock-scheduled playout. Crate layer **verified by physical loopback** (bars photographed) |
| SDI output, audio | Edge-integrated: AAC/MP2/AC-3/E-AC-3 decode → 48 kHz interleaved → timestamped, lip-synced to video on the card's clock. **Verified on hardware** (loopback audio measured). Opus/AC-4 not handled (video-only); non-48 kHz dropped |
| SCTE-35 → SCTE-104 VANC injection | Codec/demux unit verified; generic DeckLink VANC attachment **hardware loopback verified** (1080i50). Full edge TS→SDI→edge loop remains pending |
| 10-bit (`v210`) | Rejected at config validation; unpacker not yet written |

## Why the Blackmagic SDK and not FFmpeg's `decklink` avdevice

The first implementation used libavdevice. It worked — and silently hid the
single most important failure mode in an SDI plant: FFmpeg substitutes colour
bars on `bmdFrameHasNoInputSource`, so **a pulled cable is indistinguishable
from a healthy feed**. This was proven on hardware: pulling the SDI cable
produced no error, no event, and a perfectly healthy-looking 10 Mbps stream.

Talking to the SDK directly also removed the `--enable-decklink
--enable-nonfree` FFmpeg build (the edge binary stays redistributable), the
FFmpeg ≥ 8 requirement, a duplicate-`libav*` symbol clash with the vendored
`video-engine` FFmpeg, and an enumeration bug that wedged the card for the
process lifetime. The release binary links **zero** external `libav*`/`libsw*`
libraries; `libDeckLinkAPI.so` is dlopened at runtime by the SDK dispatch.

## Architecture

```
DeckLink card ──(SDK callback, C++ shim)──► decklink-rs::DecklinkCapture
                                                  │  CapturedVideo { signal_present, … }
                                                  │  CapturedAudio { i32 PCM }
                                                  ▼
engine::sdi_io  ── unpack_uyvy422 (straight into the encoder's chroma layout,
                │                  no libswscale pass)
                ├─ ScaledVideoEncoder (x264/x265/NVENC/QSV/VAAPI/auto)
                ├─ embedded PCM → planar f32 → AudioEncoder (AAC, in-process fdk-aac)
                └─ shared TsMuxer → A+V RtpPacket { is_raw_ts: true } → broadcast
```

One device handle carries both essences (unlike MXL, where video and audio
are separate flows). SDI is self-clocked: **no PTP requirement**; SDI flows
resolve to the Wallclock master-clock policy alongside TestPattern and
MediaPlayer.

The capture loop runs under `tokio::task::block_in_place`; the encoder and
muxer state (PCR/PTS continuity) survive device re-opens so receivers never
see a stream reset across a raster change.

### The 90 kHz PTS contract (load-bearing)

`sdi_io` timestamps frames in **90 kHz ticks** — they flow through the
encoder into PES unchanged — and declares that timebase to the encoder via
`ScaledVideoEncoder::set_pts_90k()`. This is not cosmetic: feeding 90 kHz
ticks against the default 1/fps timebase makes libx264's VBV rate control
read frames as minutes apart, overflow internally, and **segfault** roughly
one lookahead-depth of frames after open. NVENC merely tolerates the same
mistake, which is how it originally shipped unnoticed. ST 2110-20/-23 ingest
shares this contract (and previously shared the bug). Details:
`docs/clocking.md` does not cover this; see the module doc in
`engine/video_encode_util.rs` and `VideoEncoderConfig::time_base_num`.

## Build

```bash
# Blackmagic "Desktop Video SDK" (EULA-gated download) → Linux/include
export DECKLINK_SDK_DIR=$HOME/decklink-sdk-include
cargo build --release --features sdi-decklink,video-encoder-x264,video-encoder-nvenc
```

Only the SDK **headers** are needed at build time. At runtime the host needs
Blackmagic **Desktop Video** (kernel driver + `libDeckLinkAPI.so`); a host
without it fails the boot probe gracefully and simply doesn't advertise the
capability.

## Configuration

```json
{
  "id": "sdi1", "name": "SDI 1", "type": "sdi",
  "device": "DeckLink Quad (1)",
  "format": "auto",
  "pixel_format": "uyvy422",
  "audio_channels": 2,
  "scte35_extraction": true,
  "video_encode": {
    "codec": "h264_nvenc", "chroma": "yuv420p",
    "tune": "", "preset": "fast", "rate_control": "cbr",
    "bitrate_kbps": 10000, "gop_size": 50
  }
}
```

| Field | Values | Notes |
|-------|--------|-------|
| `device` | SDK display name, e.g. `"DeckLink Quad (1)"` | As listed by the boot probe and `HealthPayload.sdi_devices[]`. |
| `format` | `"auto"` (default, **strongly preferred**) or a DeckLink mode FourCC (`"Hi50"`, `"Hp25"`, …) | A forced mode that mismatches the source makes the card report **no signal and emit bars**. Use `auto`. |
| `pixel_format` | `"uyvy422"` | `"v210"` (10-bit) is rejected at validation — no unpacker yet. |
| `audio_channels` | 0 / 2 / 8 / 16 | 0 = video-only. AAC family output only; encoder-init failure degrades to video-only with a Warning, never kills the flow. |
| `scte35_extraction` | boolean (default `false`) | Translate SCTE-104 VANC cue-out/cue-in/cancel into SCTE-35 PID `0x01FC` (stream type `0x86`). |
| `video_encode` | mandatory | Same schema as everywhere else. `chroma` must be `yuv420p` or `yuv422p` at 8-bit (capture is 8-bit 4:2:2); every backend including `h264_auto`/`hevc_auto` works. |

Validation rejects — at config load, not mid-show — `v210`, `bit_depth: 10`,
`chroma: yuv444p`, and non-{0,2,8,16} channel counts.

## Failure modes and events

Three failure modes, deliberately handled differently. All events ride the
`flow` category with a structured `error_code`:

| Severity | error_code | Meaning / behaviour |
|----------|------------|---------------------|
| warning | `sdi_signal_lost` | Card reports `bmdFrameHasNoInputSource` (cable pulled, source down, forced-mode mismatch). **The flow keeps encoding** the card's bars/black — holding the TS up is what downstream wants — and alarms. The commonest failure; restarts nothing. |
| info | `sdi_signal_restored` | Signal came back. |
| info | `sdi_capture_opened` | Capture (re)opened; message carries raster + rate + session number. |
| info | `sdi_scte35_emitted` | An SCTE-104 cue was translated and emitted; carries event ID and opcode. |
| warning | `sdi_capture_open_failed` | Device open failed; retried every 500 ms until it returns (one event per session, not per retry). |
| warning | `sdi_raster_changed` | Source format changed; session re-opens with `auto` re-detection. Muxer + PTS state persist, so receivers see a clean continuation. |
| warning | `sdi_capture_lost` | Device delivered an error / vanished; re-open loop begins. |
| critical | `sdi_encode_failed` / `sdi_encode_config_failed` | Encoder could not open — fatal for the flow (re-opening the device would not fix a config problem). |
| critical | `sdi_pixfmt_unsupported` / `sdi_encode_chroma_unsupported` | Defensive runtime guards behind the validation rules above. |

Verified on hardware by physically pulling the cable: `sdi_signal_lost` at
T+0, `sdi_signal_restored` on reconnect, stream sustained at full rate
throughout with no session restart.

## Telemetry

**Per-input** — `InputStats.sdi_stats` (additive; absent on non-SDI inputs):

| Field | Meaning |
|-------|---------|
| `signal_present` | Card is locked to a signal *right now* — queryable state, so a manager that connects after a loss still sees it. The byte stream looks healthy during signal loss (bars are encoded deliberately), so bitrate can never tell you this. |
| `signal_losses` | Cumulative transitions — distinguishes a flapping cable from a clean run. |
| `frames_dropped` | Frames the capture shim dropped because **this edge** fell behind the SDI cadence (encoder saturated, thread starved). Invisible to every transport-side counter. Cumulative across re-opens. |
| `sessions` | Capture sessions opened; > 1 means at least one raster change / device re-open. |
| `scte35_cues_emitted` | SCTE-104 cues successfully translated and emitted on the SCTE-35 PID. |

### SCTE-104 trigger extraction

When enabled, generic VANC packets are copied before the DeckLink SDK frame is
released. The edge filters DID/SDID `0x41/0x07`, decodes supported single-op
SCTE-104 messages, and emits SCTE-35 `splice_insert()` sections directly in
the egress TS. Cue-out, cue-in, cancel, pre-roll, break duration and auto-return
are preserved; zero pre-roll becomes an immediate splice with no scheduled PTS.

Encoding, CRC and TS carriage are round-trip unit-tested. Capture plumbing still
needs verification with a live VANC source, and cue semantics have not yet been
tested with a dedicated SCTE-104 inserter.

**Per-output (playout)** — `OutputStats.sdi_stats` (additive; absent on non-SDI
outputs):

| Field | Meaning |
|-------|---------|
| `frames_sent` | Video frames successfully scheduled onto the card. |
| `frames_late` | Frames the card displayed **late** (behind their slot but still shown) — a scheduling/CPU-pressure signal, **not** lost picture. Deliberately *not* counted as a drop. Cumulative across re-opens. |
| `frames_dropped` | Frames **dropped** — never presented (card fell behind the cadence, or the edge skipped a frame against a wedged card). Also folded into the generic `packets_dropped`. Cumulative across re-opens. |
| `scte35_cues_injected` | SCTE-35 cues translated and attached as SCTE-104 VANC (counted once per cue). |

**Per-host** — `HealthPayload.sdi_devices[]`, one entry per port, refreshed by
a 10 s background poller (`IDeckLinkStatus` needs no open handle, so this
covers **all** ports — idle ones and ports held by other processes — without
disturbing live flows): signal / reference (genlock) / ancillary lock, busy,
detected raster + colorimetry + field dominance, detected dynamic range
(SDR vs HDR), SDI link config, PCIe link speed and width (an undersized PCIe
slot is a classic invisible cause of capture drops). Every field is optional on
the wire; **absent means "the card did not say", never "no"**.

**Capability** — `sdi-decklink` on `HealthPayload.capabilities`, gated on the
boot probe reaching the SDK. A card-less host with Desktop Video still
advertises (the capability means "this edge can do SDI"; `sdi_devices` says
which ports exist).

## SDI output (playout)

`decklink-rs::DecklinkPlayout` implements scheduled video **and audio**
playout: video frames are copied into card memory and scheduled against the
**card's clock** — a 3-frame pre-roll, then an 8-frame in-flight window whose
completion callbacks pace the writer (no userspace timers). Audio is
scheduled via `bmdAudioOutputStreamTimestamped` on the same 90 kHz timeline as
video, which is what gives hardware A/V lock (see "Edge integration" below).
The crate exposes late frames and dropped frames as **separate** counters
(`late_frames()` / `dropped_frames()`) — late means displayed behind its slot
but still shown (soft, scheduling pressure), dropped means never presented
(hard loss); callers should keep them distinct rather than summing them (the
edge integration below does).

Verified by physical loopback on a DeckLink Quad: colour bars
(`examples/playout_bars.rs`) scheduled on one sub-device, emerging on its
pair-partner connector, through a BNC loop into another port, captured
through the full edge pipeline and photographed — correct colours, moving
sweep element, `late=0 dropped=0` over 3000 frames.

### Edge integration (`engine/output_sdi.rs`)

```json
{
  "id": "sdi-out1", "name": "SDI monitor", "type": "sdi",
  "device": "DeckLink Quad (1)",
  "mode": "Hi50",
  "pixel_format": "uyvy422",
  "audio_channels": 2,
  "audio_offset_ms": 0,
  "program_number": null,
  "scte35_injection": true
}
```

| Field | Values | Notes |
|-------|--------|-------|
| `device` | SDK display name | Same namespace as the input's. Mind the connector-pair routing below. |
| `mode` | DeckLink mode FourCC, **required** | Playout has nothing to auto-detect from; `"auto"` is rejected at validation. Must match the decoded video's raster — mismatched frames are dropped with `sdi_playout_raster_mismatch`, never displayed garbled. |
| `pixel_format` | `"uyvy422"` | 10-bit playout not yet implemented. |
| `audio_channels` | 0 / 2 / 8 / 16 | 0 = video-only. The flow's audio is decoded (AAC / MP2 / AC-3 / E-AC-3), interleaved into this channel count, and lip-synced to video via the shared playout clock. Fixed 48 kHz — a non-48 kHz track drops with an alarm. |
| `audio_offset_ms` | `-1000..=1000`, default `0` | Operator A/V-sync trim. **Positive delays audio** (plays later — corrects audio-early); **negative advances it** (plays earlier — corrects audio-late). A constant shift on each scheduled audio block's card time; the drift-free sample counter is untouched, so it never accumulates. Use it to null a residual lip-sync offset once measured on a monitor. |
| `program_number` | optional | MPTS down-select, like every other output. |
| `scte35_injection` | boolean (default `false`) | Decode PMT stream type `0x86` cues and attach SCTE-104 DID/SDID `0x41/0x07` to the next three scheduled frames. |

### SCTE-35 trigger injection

When enabled, the selected program's SCTE-35 PID is section-reassembled by
the shared TS demuxer. CRC-valid `splice_insert()` cues are decoded into the
same `Scte104Message` model used by extraction, encoded as single-operation
SCTE-104, and attached to three successfully scheduled SDI frames for receiver
reliability. Line number `0` asks the DeckLink SDK to auto-place the packet in
VANC. Component splices, encrypted sections and non-`splice_insert` commands
are ignored. Event `sdi_scte104_queued` confirms translation and queuing.

The binary codec, CRC validation, section demux and SDK feature build are
automated-test verified. Generic attachment is hardware-verified: 500 1080i50
frames carrying a known 25-byte SCTE-104 packet scheduled with zero late/drops,
and a looped port captured the exact bytes (the SDK placed requested line `0`
on physical VANC line 9). A full edge TS cue → SDI → edge extraction run remains
the final system-level verification.

Pipeline: broadcast subscriber → `TsDemuxer` → H.264/HEVC/MPEG-2 decode
(`VideoDecoder`) → `pack_uyvy422` (the exact inverse of the input's unpacker —
4:2:0 sources upsample chroma by row duplication) → `DecklinkPlayout`. Only
**8-bit 4:2:0/4:2:2** decoded video can be packed to UYVY422; a 4:4:4 or 10-bit
source drops frames with `sdi_playout_chroma_unsupported` rather than displaying
corrupted colour. The
card's completion callbacks pace the pipeline; a bounded hand-off channel
absorbs jitter and drops (counted on `packets_dropped`) rather than buffering
latency. Card-reported late/dropped completions fold into `packets_dropped`.
Keyframe-gated after every decoder (re)open. Audio (when `audio_channels > 0`) is decoded in the same worker, interleaved to 48 kHz 32-bit, and scheduled timestamped at `pts - first_video_pts` on the shared 90 kHz clock for hardware A/V sync; the schedule position then advances drift-free by sample count, re-anchoring only on a discontinuity. Failure modes mirror
the input: unsupported mode/device is **fatal** (`sdi_playout_mode_unsupported`
— a retry can never fix a config problem); a device that vanishes mid-run
re-opens with backoff (`sdi_playout_lost` / `sdi_playout_open_failed` /
`sdi_playout_opened`). Cost model: 275 units (CPU decode, 1080p-class).

### Hardware gotchas that will cost you a day at a rack

On 8-port Quad cards, **software device numbering interleaves across the
physical connectors** (physical 1–8 = software 1,5,2,6,3,7,4,8), sub-devices
pair as (1,5)(2,6)(3,7)(4,8) sharing adjacent connectors, and a sub-device
playing out while its own connector carries an input **emits on its pair
partner's connector**. Idle outputs emit NTSC black, so a looped input
showing `signal=yes` proves nothing — check `detected_mode`. Full table and
evidence: `bilbycast-decklink-rs/CLAUDE.md`.

## Verification techniques (what actually worked)

* **Signal-loss test**: physically pull the cable. Anything less lies —
  the avdevice implementation passed every software test and failed this one.
* **Picture checks**: add a temporary `udp` output (`127.0.0.1:9999`), grab a
  frame with `ffmpeg -i "udp://127.0.0.1:9999?fifo_size=5000000&overrun_nonfatal=1"
  -frames:v 60 -update 1 /tmp/x.png`, and *look at it*. Chroma bugs (ghosting,
  smear, levels shifts) survive every unit test and die on sight.
  **Kill stray ffmpeg processes first** — an interrupted grab stays bound to
  the UDP port and silently eats the stream.
* **Deterministic no-signal**: force a `format` that mismatches the source
  (e.g. `Hp50` against a 1080i50 feed) — reproduces the whole signal-loss
  path without touching the rack.
* **Automated lip-sync, no monitor required**: generate a test clip with a
  short video flash and a short audio tone-step *of different frequency to
  the bed tone* aligned at the same instants (a plain silence/tone pip is
  fragile — a short transient can fail to survive a decode/re-encode cleanly
  enough for onset detection, which reads as a false dropout). Loop it
  through `media_player` → SDI out → SDI in loopback capture, then detect
  video onsets via `ffmpeg -vf signalstats` (YAVG threshold) and audio onsets
  via zero-crossing rate per short window (a frequency step is far more
  robust to codec artifacts than an amplitude/silence step). Compare onset
  timestamps for offset and drift. This is how the 12 h soak's A/V sync was
  verified without physical access to the rack.

## Production readiness

**Verified on hardware** (bilby-z440, DeckLink Quad, live 1080i50 source):
capture → encode (x264 + NVENC) → A+V TS; signal-loss/reconnect by physically
pulling the cable; per-port `HealthPayload.sdi_devices[]`; SDI→SDI loopback
video (photographed clean) and audio (measured continuous, lip-synced by the
card clock); config validation rejecting bad chroma/bit-depth/mode at load.

**12-hour soak** (full-duplex SDI in+out, 2-channel embedded audio): 12 h 05 m,
716 samples, **0** signal losses, **0** capture/output drops, 1 capture
session (no re-opens), RSS flat at 585 MB (no leak), 100.07 % frame cadence,
**0** segfaults/panics/encode-failures/audio-warnings.

**Automated A/V lip-sync** (media_player → SDI out, loopback capture, a
freq-step test clip — a 200 ms white flash + 200 ms 3 kHz tone-burst aligned
every 1 s on a continuous 1 kHz bed — detected via `signalstats` YAVG for
video and zero-crossing rate for audio): confirmed audio is **not** dropped
(an earlier "pip every 3 s" reading was a measurement artifact of 40 ms
AAC-coded transients, not a pipeline fault — a continuous tone comes back
continuous, 96.7 % active windows, flat RMS) and the A/V offset is **rock
steady with zero drift** across every event. The *absolute* offset is
confounded by the capture/monitor path's own bias and could not be isolated
remotely, so `audio_offset_ms` (below) exists for an operator to null it
against a real reference monitor.

**Robustness properties** built in and worth stating for an operator:

* **Signal loss never drops the transport stream** — the card's bars/black are
  encoded through, an alarm is raised, nothing restarts.
* **No panics on the media path** — malformed/short frames, decode errors,
  unsupported chroma, and a wedged card are all handled (drop + throttled
  alarm, or bounded-wait), never `unwrap`/`panic`. The crate is built for a
  long-running broadcast binary.
* **The reactor never does codec work** — all encode/decode runs under
  `spawn_blocking`; the async feeder only demuxes.
* **Bounded everywhere** — the hand-off channel drops rather than buffers
  latency; the playout write times out rather than hanging on a dead card;
  input switches flush the decoder so they re-anchor on the next keyframe.

**Not yet field-validated** (works in bring-up, needs hardware not on hand):

* Rasters other than 1080i50 (720p / 1080p50 / 2160p).
* 8- and 16-channel embedded audio (2-channel verified).
* QSV / VAAPI encoder backends (compile-verified; no Intel/AMD host to date).
* The *absolute* lip-sync number against a real monitor — presence,
  drift-freedom, and linearity of the `audio_offset_ms` trim are all measured
  (see above); only the "what's the real-world offset on my rack" question
  needs eyes on a physical monitor.

**Operational prerequisites**:

* Blackmagic **Desktop Video** installed (kernel driver + `libDeckLinkAPI.so`).
* Run with `BILBYCAST_PROBE_SESSION_LIMITS=0` if NVENC session probing stalls
  startup on this driver.
* `format: "auto"` on inputs; an explicit `mode` FourCC on outputs.
* For broadcast-grade wire pacing on TS egress, grant `CAP_SYS_NICE` (the
  shipped systemd unit does) — unrelated to SDI but logged loudly at boot.

## Known limitations / roadmap

* 10-bit (`v210`) capture unpack — unlocks the 10-bit HEVC hardware paths.
* HW-decode for the playout path (CPU decode only today); audio resampling (48 kHz-only today); Opus/AC-4 playout audio.
* 8/16-channel embedded audio and non-1080i50 rasters are implemented but
  not yet hardware-verified.
* Genlock: playout free-runs against the card clock today (`reference_locked`
  is surfaced per-port so an operator can see whether house reference is
  patched).
* Device control (input-connection selection, Quad profile switching) is
  deliberately out of scope until an ownership model exists — a profile
  switch reassigns channels under running flows.

## Upstream fixes discovered during this work

Bring-up surfaced seven pre-existing bugs, none SDI-specific. Each is fixed
on its own branch for independent review: NVENC-fatal `tune`/`preset`
defaults (`fix/nvenc-tune-default`), the 4:2:2→4:2:0 scaler-skip chroma
corruption + full-range scaler target (`fix/scaler-chroma-mismatch` +
video-crates `fix/planar-yuv-layout`), the 90 kHz/1-fps timebase segfault
(`fix/encoder-timebase-90k`, plus `set_pts_90k` here and in ST 2110), silent
standalone-edge encoder failures (fixed inline), and a `PKG_CONFIG_PATH`
override in `libffmpeg-video-sys` (`fix/pkg-config-path-inheritance`).
