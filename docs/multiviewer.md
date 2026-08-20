# Multiviewer — the mosaic compositor and stream head

Composite N node-local inputs into one canvas and publish it as a fresh
MPEG-TS feed. The wall is then an ordinary flow source: it restreams over
SRT/RTP/UDP/WebRTC/CMAF, records, nests inside another wall, and produces
thumbnails, all without a line of new output code.

Phase 1 of the distributed multiviewer (edge #107). Design and phasing:
[`../../docs/MULTIVIEWER_PLAN.md`](../../docs/MULTIVIEWER_PLAN.md).

**A wall carries pictures only.** The composited TS has a video PID and nothing
else — `set_has_audio(false)`, so the PMT declares no audio, deliberately: an
announced audio PID that never carries a packet makes a receiver wait for audio
that is never coming and makes every downstream A/V check report a fault that is
not one. Tile ingest drops every non-video access unit for the same reason.
Audio on the wall — carried, mixed or metered into the canvas (#109) — is phase 2.

## Building it

**Off by default as a Cargo feature**, like the encoders it depends on. A wall
is a deployment choice, compositing is the most expensive thing this binary
does, and it is pure cost on a node that will never run one.

That is a statement about `cargo build`, not about what you install: **all three
published release artefacts** — `x86_64-linux-full`, `aarch64-linux-full` and
`aarch64-linux-rockchip` — are built with `multiviewer`, and the release
workflow's *Verify binary* step fails the release on any artefact whose binary
does not report both `feature multiviewer` and `capability mv-compositor`. So a
node running a published binary can compose a wall without rebuilding; only a
self-built edge needs the feature list below.

```bash
# libx264 — GPL v2+, so the binary becomes an AGPL combined work
cargo build --release --features "multiviewer,video-encoder-x264"

# NVENC — LGPL-clean, needs an NVIDIA driver at runtime
cargo build --release --features "multiviewer,video-encoder-nvenc"

# What the published *-full artefacts are built with: video-encoders-full
# already carries every encoder, so the release adds multiviewer and no
# further encoder choice
cargo build --release --features "multiviewer,video-encoders-full"
```

**An encoder is not optional.** The flow bus carries MPEG-TS, so a composite
reaches an output by being **encoded and muxed**, never by being handed over.
A build with `multiviewer` but no encoder compiles, and refuses at flow start
with a message naming the rebuild — verified: a default `cargo build` resolves
every encoder backend to `FeatureDisabled`.

The node advertises `mv-compositor` on `HealthPayload.capabilities` only when
both halves are present, and publishes `HealthPayload.mv_heads` under exactly
the same pair of conditions — so a node that could not run a wall registers no
head in the manager, and there is nothing to point a wall at.

The manager does **not** hide any UI on that bit, which an earlier version of
this page claimed. The Multiviewer Walls page is always reachable — its own
empty state explains what a wall needs, which is more useful than a nav entry
that silently disappears — and `mv-compositor` is read in exactly one place, the
deploy-time check. A wall aimed at a head whose node lacks the compositor is
refused at deploy with HTTP 422 `wall_not_deployable` and the refusal
`node_no_compositor`, rendered on the wall's own card.

**What the node advertises.** Two things, both gated on a video encoder
resolving at build time: the `mv-compositor` capability bit, and a one-entry
`mv_heads` array on the health tick.

```json
{
  "head_id": "stream0",
  "kind": "stream",
  "connector": null,
  "max_canvas_width": 1920,
  "max_canvas_height": 1080,
  "capabilities": { "encoder_backends": ["libx264"] }
}
```

`head_id` is a compile-time constant (`STREAM_HEAD_ID`), not derived from
runtime state, and that is load-bearing: the manager keys its head rows on
`(node_id, head_id)`, so an id minted per boot would create a second row on
every restart and strand the wall pointing at the retired one. `connector` is
`null` because a stream head occupies no physical port. `encoder_backends`
carries the FFmpeg name of the one backend this head **will use** — the first
`select_video_backend()` match, not every backend compiled in — so on every
published release artefact it reads `["libx264"]`. The field names are a wire
contract: the manager's `HeadAdvertisement` deserialises this verbatim.

Phase 1 is one stream head per node. Panel and SDI heads are enumerable — a KMS
connector each, a DeckLink port each — and arrive with their own ids in phase 2
(#110).

## Configuring one

A mosaic is an **input**. Its tiles reference other inputs on the same node by
id — the first input type in the tree that consumes other inputs.

```json
{
  "id": "wall-1",
  "name": "Gallery wall",
  "type": "mosaic",
  "width": 1920,
  "height": 1080,
  "fps": 25,
  "video_bitrate_kbps": 8000,
  "codec": "h264_auto",
  "tiles": [
    { "id": "t1", "source_input_id": "cam-1", "x": 0,   "y": 0,   "width": 960, "height": 540, "label": "CAM 1" },
    { "id": "t2", "source_input_id": "cam-2", "x": 960, "y": 0,   "width": 960, "height": 540, "label": "CAM 2" },
    { "id": "t3", "source_input_id": "cam-3", "x": 0,   "y": 540, "width": 960, "height": 540, "label": "CAM 3" },
    { "id": "t4", "source_input_id": null,    "x": 960, "y": 540, "width": 960, "height": 540, "label": "SPARE" }
  ]
}
```

Put the wall in a flow like any other input and give the flow whatever outputs
you want it seen on.

| Field | Default | Notes |
|---|---|---|
| `width` / `height` | 1920x1080 | Must be even. Capped at 1920x1080 in phase 1 — see *Why UHD is refused*. |
| `fps` | 25 | The **canvas's own** cadence, independent of any source's rate. Range 1–60. |
| `video_bitrate_kbps` | 8000 | 100–200000. |
| `codec` | `h264_auto` | **Accepted but not honoured in phase 1** — see *`codec` selects nothing today* below. Only its length is checked (≤ 64 chars), so `"banana"` is accepted. |
| `tiles` | **required** | 1–64 tiles. |
| `tiles[].id` | **required** | Stable identity, 1–64 chars, unique within the wall. Routing keys on it, so renaming a tile cannot re-point a signal. |
| `tiles[].x` / `.y` | **required** | Top-left of the tile on the canvas, in pixels. |
| `tiles[].width` / `.height` | **required** | Tile size in pixels. Non-zero, and the tile must fit inside the canvas. |
| `tiles[].source_input_id` | `null` | Node-local input id, 1–64 chars when set. `null` renders `UNASSIGNED`. |
| `tiles[].z` | 0 | Paint order, higher on top. Overlap is legal — it is how a PiP is built. |
| `tiles[].label` | `""` | Burned into the tile, max 64 chars. Empty = no label. |

`tiles` and the five tile fields marked **required** carry no serde default, so
a config that omits one fails to *parse*: you get a deserialization error for
the whole document rather than the mosaic-specific refusal
`validate_mosaic_input` produces for every other mistake. Everything else is
validated at save time — a wall with zero tiles ("a wall needs at least one
tile"), a zero-sized tile, a tile that does not fit the canvas, or two tiles
sharing an id is refused with a named reason, by the same validator the
compositor itself uses so the two cannot drift apart.

**The 64-tile cap is not tidiness.** Every tile is an independent decode + scale
task with its own `watch` channel and its own tile-sized buffer, so cost is
linear in tile count and is paid on a node that is usually also carrying the
live feeds being watched. 64 is comfortably above a 7x7 wall.

**`codec` selects nothing today.** The compositor resolves its encoder with
`select_video_backend()` — a function that takes no argument and returns the
first backend compiled into this binary in a fixed order, x264 ≻ x265 ≻ NVENC ≻
QSV, with VAAPI and RKMPP not considered at all — and then *overwrites* the
encode config's codec with that backend's name. The configured string reaches
nothing but the error text emitted when no encoder is compiled in. Every
published release artefact carries `video-encoder-x264`, which wins that order,
so **a wall on a release binary always encodes with libx264 on the CPU**,
whatever this field says. The manager knows this: it emits the constant
`h264_auto` and offers no dropdown. One consequence worth knowing — the
resource-cost model charges the *configured* codec, so a wall written as
`h264_nvenc` is billed against the node's NVENC session budget while actually
running x264.

A tile source may be a full-resolution local SDI feed or a small proxy arriving
over SRT from another site. The compositor cannot tell the difference, which is
what lets proxies be added later without changing it.

## What an operator sees

| State | Rendering |
|---|---|
| **Live** | The source, letterboxed into its tile. |
| **NO SIGNAL** | Amber bar. A source was routed here and has stopped delivering for 2 s. |
| **UNASSIGNED** | Grey bar. No source is routed to this tile at all. |

Two badges, not one, because they send an operator to different places: "the
feed died" and "nobody patched this" are different problems.

**A stale frame is never presented as live.** A source that stops keeps its last
picture on the canvas — going black on a hiccup would be worse — but the badge
goes over the top once the timer expires. That distinction is load-bearing and
is pinned by a test, because the natural implementation gets it wrong: the frame
handoff retains its last value forever, so "is there a frame?" stays true long
after the source died.

## Nothing blocks the media path

A wall is a *monitoring* surface. It must never apply backpressure to the feeds
it is watching, and on a contribution node those feeds can be very high
bandwidth. Every choice below follows from that:

- **Each tile decodes independently and keeps only its newest frame.** The
  handoff is a `watch` channel: sending overwrites, never queues, never blocks,
  never grows. A tile decoding faster than the canvas ticks simply has its older
  frames dropped, which is what you want — nobody wants a stale frame that was
  queued behind a fresher one.
- **The compositor never waits for a tile.** Each canvas tick takes whatever
  each tile currently has. A dead source cannot stall the wall.
- **A lagged subscription is skipped, never awaited.** `RecvError::Lagged` is
  counted and dropped.
- **Decode and encode run under `block_in_place`**, so the FFmpeg C calls never
  occupy an async worker.
- **A missing source is not fatal, and not permanent.** Each tile subscribes in
  a retry loop rather than once at wall start: inputs come up in whatever order
  the flow starts them, and a wall is usually started alongside the very feeds
  it watches, so a single attempt loses a common race *permanently*. The same
  loop picks a source back up after it is stopped and restarted mid-show.
- **Tile tasks are joined when the wall stops**, so a tile never outlives the
  compositor holding a subscription to a live source.

## What a wall costs

Nothing above is free — it is *non-blocking*, which is a different property.

Every tile is an **independent software decode**: `VideoDecoder::open` is CPU
libavcodec, single-threaded (`open_threaded` exists and is deliberately not used
here), one decoder per tile, plus a libswscale scale into the tile rect. The
canvas encode is CPU too on every release artefact, because the backend selector
prefers x264 over every hardware encoder. So an N-tile wall costs N software
decodes + N scales + one canvas-sized software encode per canvas tick, on a node
that is usually also carrying the very feeds being watched. That is what
`canvas_over_budget` is measuring when it rises.

**Tiles can show H.264, HEVC and MPEG-2 only.** Those are the three video access
units the TS demuxer emits; everything else — audio, data, and any other video
codec — is dropped before a decoder is opened. A source carrying none of the
three therefore produces no frames at all: its tile sits at NO SIGNAL with no
event and no `tile_decode_errors` increment, because nothing ever failed to
decode.

## Telemetry

| Counter | Meaning |
|---|---|
| `canvas_frames` | Canvas frames composited and published. |
| `canvas_over_budget` | Ticks whose composite work outran the canvas period. **Rising means the wall is too expensive for this head** — reduce tiles, canvas size or fps. |
| `canvas_skipped` | Canvas periods that elapsed with no frame. The ticker skips rather than sprinting through stale frames, which is right, but silently skipping would let a wall run at a fraction of its configured rate with everything else reading healthy. This is what an operator sees as a stuttering wall. |
| `tile_input_lagged` | TS packets a tile's subscription missed because that tile fell behind its source. Its picture will have artefacts. **Not** the ordinary case of a decoded frame being superseded by a fresher one — that is healthy, happens constantly, and is not counted. |
| `tile_decode_errors` | Frames a tile could not decode. |

`canvas_over_budget` and `canvas_skipped` are deliberately separate: a wall that
is too expensive and a wall that is being starved are different faults with
different fixes, and one counter conflating them tells an operator nothing.

## Events

| Code | Severity | Meaning |
|---|---|---|
| `mosaic_failed` | Critical | The wall stopped. Usually no encoder compiled in. |
| `mosaic_tile_source_missing` | Warning | A tile's `source_input_id` is not running on this node **yet** — the tile retries and fills in when it appears. |
| `mosaic_tile_self_reference` | Warning | A tile named the wall's own input id. Refused: it would feed the compositor its own output. |

## Two things the design document got wrong

Both were found by measuring rather than reading, and both changed the build.
The measurement lives in
`bilbycast-ffmpeg-video-rs/video-engine/tests/canvas_subrect_blit.rs`.

**The canvas is packed BGRA8, not YUV.** `scale_raw_planes_into_packed` refuses
every planar destination. So a canvas costs 4 bytes/pixel rather than YUV420's
1.5 — **2.7x** what the plan budgeted — and the canvas must be converted to YUV
before it can be encoded. The upside is real: BGRA has no chroma sub-sampling,
so tile rects need no even alignment at all. Odd x, y, width and height are all
exact.

**The scaler's bounds check refused the bottom row of every wall.** It demanded
`pitch * height` from the destination slice, but a bottom-row tile's remaining
tail is exactly `x0 * 4` bytes short of that — on a 2x2 wall at 1080p, 3840
bytes short. Guard bytes proved the true requirement is
`(h-1)*pitch + w*4`, and the check now uses it. It had also made a mosaic
impossible on the panel path outright, since `KmsDisplay::back_buffer()` maps
exactly `pitch * height`.

## Why UHD is refused in phase 1

`SW_BLIT_MAX_W/H` is 1920x1080 in the display output because a 4K libswscale
convert into a write-combining KMS dumb buffer measured **~7 s/frame**. A stream
head composites into ordinary cached sysmem and is *not* bound by that number —
but nobody has measured the stream-head shape, and shipping an unmeasured UHD
path is exactly how the display output earned its ceiling. Raising it is gated
on that measurement, not on an argument.

## Not built yet

- **Proxies and the rendition ladder** (#106) — the compositor's ingest is
  already source-agnostic, so this slots underneath with no compositor rework.
- **Tally and UMD** (#108), **audio metering rasterised into the canvas**
  (#109), **panel and SDI heads** (#110).
- **Audio on the wall itself.** The compositor muxes video only and the PMT
  says so; no audio PID is carried, mixed or monitored. Metering (#109) is the
  first step toward it, not the whole of it.
- **Glyph rendering.** Badges are coloured bars, not text: rendering glyphs
  needs a font stack this binary does not carry, and an operator reading a wall
  across a gallery reads position and colour long before letters.
- **Per-tile QC and click-to-replay** — phase 2.
