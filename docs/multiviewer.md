# Multiviewer — the mosaic compositor and stream head

Composite N node-local inputs into one canvas and publish it as a fresh
MPEG-TS feed. The wall is then an ordinary flow source: it restreams over
SRT/RTP/UDP/WebRTC/CMAF, records, nests inside another wall, and produces
thumbnails, all without a line of new output code.

Phase 1 of the distributed multiviewer (edge #107). Design and phasing:
[`../../docs/MULTIVIEWER_PLAN.md`](../../docs/MULTIVIEWER_PLAN.md).

## Building it

**Off by default**, like the encoders it depends on. A wall is a deployment
choice, compositing is the most expensive thing this binary does, and it is
pure cost on a node that will never run one.

```bash
# libx264 — GPL v2+, so the binary becomes an AGPL combined work
cargo build --release --features "multiviewer,video-encoder-x264"

# NVENC — LGPL-clean, needs an NVIDIA driver at runtime
cargo build --release --features "multiviewer,video-encoder-nvenc"

# The *-full release artefacts already carry video-encoders-full, so
# adding multiviewer there needs no further encoder choice
cargo build --release --features "multiviewer,video-encoders-full"
```

**An encoder is not optional.** The flow bus carries MPEG-TS, so a composite
reaches an output by being **encoded and muxed**, never by being handed over.
A build with `multiviewer` but no encoder compiles, and refuses at flow start
with a message naming the rebuild — verified: a default `cargo build` resolves
every encoder backend to `FeatureDisabled`.

The node advertises `mv-compositor` on `HealthPayload.capabilities` only when
both halves are present, so the manager hides the surface on a node that could
not run a wall.

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
| `fps` | 25 | The **canvas's own** cadence, independent of any source's rate. |
| `video_bitrate_kbps` | 8000 | 100–200000. |
| `codec` | `h264_auto` | Same names as an output's `video_encode.codec`. |
| `tiles[].id` | — | Stable identity. Routing keys on it, so renaming a tile cannot re-point a signal. |
| `tiles[].source_input_id` | — | Node-local input id. `null` renders `UNASSIGNED`. |
| `tiles[].z` | 0 | Paint order, higher on top. Overlap is legal — it is how a PiP is built. |

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
- **Glyph rendering.** Badges are coloured bars, not text: rendering glyphs
  needs a font stack this binary does not carry, and an operator reading a wall
  across a gallery reads position and colour long before letters.
- **Per-tile QC and click-to-replay** — phase 2.
