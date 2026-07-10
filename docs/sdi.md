# Native SDI I/O (Blackmagic DeckLink)

bilbycast-edge can capture SDI directly from Blackmagic DeckLink cards —
video plus embedded audio, encoded in-process and published as a standard
A+V MPEG-TS flow — with no external SDI→IP converter. Gated behind the
`sdi-decklink` Cargo feature (default **off**), backed by the sibling
[`bilbycast-decklink-rs`](https://github.com/AJ-Github-Account/bilbycast-decklink-rs)
crate. Upstream tracking issue:
[Bilbycast/bilbycast-edge#19](https://github.com/Bilbycast/bilbycast-edge/issues/19).

| Capability | Status |
|------------|--------|
| SDI input, video (UYVY422 8-bit, any raster the card detects) | **Verified on hardware** (1080i50, DeckLink Quad) |
| SDI input, embedded audio (2 ch → AAC) | **Verified on hardware** (8/16 ch untested) |
| Signal-loss / raster-change / device-loss resilience | **Verified by physically pulling the cable** |
| Per-port hardware status on `HealthPayload` | **Verified** (all 8 ports, live during capture) |
| Encoder backends | All of them — x264 / x265 / NVENC / QSV / VAAPI / `h264_auto` / `hevc_auto` (x264 + NVENC hardware-verified; QSV/VAAPI compile-verified) |
| SDI output (playout) | Crate-level **verified by physical loopback** (bars out one port, captured on another, photographed); edge `OutputConfig::Sdi` integration pending |
| SDI output, audio | Pending (video-only playout first) |
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

**Per-host** — `HealthPayload.sdi_devices[]`, one entry per port, refreshed by
a 10 s background poller (`IDeckLinkStatus` needs no open handle, so this
covers **all** ports — idle ones and ports held by other processes — without
disturbing live flows): signal / reference (genlock) / ancillary lock, busy,
detected raster + colorimetry + field dominance, SDI link config, PCIe link
speed and width (an undersized PCIe slot is a classic invisible cause of
capture drops). Every field is optional on the wire; **absent means "the card
did not say", never "no"**.

**Capability** — `sdi-decklink` on `HealthPayload.capabilities`, gated on the
boot probe reaching the SDK. A card-less host with Desktop Video still
advertises (the capability means "this edge can do SDI"; `sdi_devices` says
which ports exist).

## SDI output (playout)

`decklink-rs::DecklinkPlayout` implements scheduled video playout: frames are
copied into card memory and scheduled against the **card's clock** — a
3-frame pre-roll, then an 8-frame in-flight window whose completion callbacks
pace the writer (no userspace timers). Late/dropped completions are counted.
Video-only today; `write_audio` returns `Unsupported` until the video path
has soaked.

Verified by physical loopback on a DeckLink Quad: colour bars
(`examples/playout_bars.rs`) scheduled on one sub-device, emerging on its
pair-partner connector, through a BNC loop into another port, captured
through the full edge pipeline and photographed — correct colours, moving
sweep element, `late=0 dropped=0` over 3000 frames.

**Edge integration (`OutputConfig::Sdi`) is not yet written.** The planned
shape mirrors `output_display.rs`: broadcast subscriber → `TsDemuxer` →
`VideoDecoder` (all HW/SW backends) → planar→UYVY repack →
`DecklinkPlayout`.

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

## Known limitations / roadmap

* 10-bit (`v210`) capture unpack — unlocks the 10-bit HEVC hardware paths.
* SDI output edge integration, then playout audio.
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
