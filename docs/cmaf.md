# CMAF / CMAF-LL Output

A deep reference for the bilbycast-edge CMAF output type. The short
per-field schema lives in [`docs/configuration-guide.md`](configuration-guide.md#cmaf--cmaf-ll-output);
this document covers architecture, performance characteristics, ingest
compatibility, and DRM workflow.

## Overview

The CMAF output publishes fragmented-MP4 segments (ISO/IEC 23000-19 CMAF
media profile) to an operator-supplied HTTP push ingest. One edge flow
can emit both HLS (`.m3u8`) and DASH (`.mpd`) manifests against the same
segments, so a single CMAF flow reaches Apple and Android/Widevine
players without transcoding twice. Supports:

- **Video**: H.264 or HEVC passthrough, or re-encode via libx264 /
  libx265 / NVENC with explicit GoP alignment.
- **Audio**: AAC-LC / HE-AACv1 / HE-AACv2 passthrough, or re-encode via
  the in-process fdk-aac backend. Audio is **muxed into the same
  fragment as the video** — one `moof` addressing both tracks — so a
  browser needs a single MSE SourceBuffer and there is no second
  timeline to keep aligned. Two configurations are **video-only**, and
  both say so at startup: `low_latency: true` (a chunk carries one
  track) and `encryption` (CENC covers video only). See Limitations.
- **Delivery**: whole-segment HTTP PUT (`low_latency: false`) or
  chunked-transfer streaming PUT (`low_latency: true`, LL-CMAF) with
  per-segment `moof + mdat` chunks emitted every `chunk_duration_ms`
  and advertised via HLS `#EXT-X-PART` and DASH
  `availabilityTimeOffset`.
- **Encryption**: ISO/IEC 23001-7 Common Encryption — `cenc` (AES-128
  CTR) and `cbcs` (AES-128 CBC, 1:9 block pattern; FairPlay) with
  ClearKey PSSH plus verbatim passthrough of operator-supplied Widevine
  / PlayReady / FairPlay PSSH boxes.

The CMAF subsystem lives under `src/engine/cmaf/` as the sibling of
`src/engine/output_hls.rs` — both are HTTP-push segmented outputs, but
HLS emits MPEG-TS `.ts` segments and CMAF emits `.mp4` / `.m4s`.

## Threading and performance

CMAF is designed to never block the broadcast subscriber, matching the
project-wide "never block the data path" invariant.

- The subscriber loop receives `RtpPacket`s from the broadcast channel,
  demuxes MPEG-TS, accumulates samples per track, and cuts segments on
  IDR. All of this is synchronous and in-memory.
- Video / audio re-encoding (Phase 3) runs inside
  `tokio::task::block_in_place` wrapped around `VideoDecoder` /
  `VideoEncoder` (`bilbycast-ffmpeg-video-rs`) and `AacDecoder` /
  `AudioEncoder` (`bilbycast-fdk-aac-rs`). The subscriber task retains
  ordering with the other outputs while the runtime pre-empts other
  tasks onto free workers.
- HTTP uploads use `reqwest` with a process-wide shared `Client` behind
  a `OnceLock` so TLS handshakes are amortised across segments.
- LL-CMAF uses `reqwest::Body::wrap_stream` over a
  `tokio::sync::mpsc::channel(8)`. Chunks are pushed with `try_send`;
  if the channel is full (ingest is too slow) the PUT is aborted, a
  throttled Warning event is emitted, and the current segment is
  discarded — the broadcast subscriber is **never blocked**.

Under 60 s of 15 Mbps 1080p30 H.264+AAC passthrough, peak edge CPU
measured 3 % with zero broadcast lag events
(`testbed/scripts/cmaf_load_test.sh`).

## Segment boundary model

CMAF segments must begin with an IDR / RAP. The segmenter:

1. Tracks the wall-clock DTS of the first sample of the current
   segment (`segment_base_dts`).
2. On each arriving IDR / RAP, checks whether `dts - base >=
   target_duration_90k`. If yes, closes the current segment and opens
   a new one at this sample.
3. Samples before the first IDR are dropped (can't start decoding
   mid-GoP).

The actual segment duration is therefore determined by **both** the
target and the source GoP cadence. For passthrough video, the source
must emit an IDR at least every `segment_duration_secs`; otherwise
segments will drift and the manifest's per-segment `EXTINF` may exceed
`#EXT-X-TARGETDURATION`, which some strict players reject.

For re-encoded video (`video_encode` block set), the edge forces
`gop_size = segment_duration_secs * fps` so boundaries are guaranteed.

## Playlist window (`dvr_window_secs`)

By default the playlist lists the last `max_segments` segments — a live
window, a handful of segments deep. `dvr_window_secs` replaces that
with a **duration**, and the playlist is trimmed to
`ceil(dvr_window_secs / segment_duration_secs)` segments instead
(capped at 21 600, which is 12 hours of 2 s segments).

That is what makes a browser able to seek backwards: `video.seekable`
is derived from what the playlist lists, so a 5-segment live window
gives a viewer 10 seconds of history no matter how much the origin
still holds.

Two things have to agree, and neither is derived from the other:

- **The edge's playlist** must not list segments the origin has already
  evicted, or a seek into them 404s mid-playback.
- **The origin's retention** must not be shorter than the window the
  playlist advertises. On bilbycast-relay this is
  `origin_retention_secs`, or a per-stream override pushed by the
  manager.

Size the origin's retention to the advertised window **plus headroom** —
a viewer parked mid-window must not have the segment under them deleted
while they are watching it.

The playlist also stops declaring `#EXT-X-PLAYLIST-TYPE:EVENT`, which it
previously did unconditionally. `EVENT` promises a playlist that only
ever grows (RFC 8216 §4.3.3.5); a trimmed playlist is not one, and
hls.js computed a seekable range that included segments already dropped.

## Absolute time on the playlist (`#EXT-X-PROGRAM-DATE-TIME`)

Each playlist row carries the wall clock of its own first sample, and **every**
row emits its own `#EXT-X-PROGRAM-DATE-TIME`.

The clock is per row rather than once per stream on purpose. The playlist is a
rolling window: the first row changes as the oldest are trimmed, and a tag
anchored to when the *stream* started would go on naming a segment that is no
longer listed, with the error growing without bound for as long as the session
runs. Nothing would report it.

### The date comes from the media timeline, not from the clock at publish

`program_date_time` was `Utc::now() - segment_duration`, sampled as each
segment closed. That records when the edge got round to writing the segment,
not when the content happened, so the tag carried whatever scheduling and
pipeline delay sat between the two.

It is derived now: `seg.base_dts_90k` — the source's own 90 kHz PTS, which
`PtsUnwrap` does not rebase — placed against a wall-clock epoch consulted
**once per flow**. `FLOW_EPOCHS` is keyed on the flow rather than the output
precisely so that two renditions of one source, which see the same RTP packets
and therefore the same `base_dts_90k`, publish *identical* dates. A sample
implying an epoch more than ten seconds from the held one is a source restart
or a PTS discontinuity rather than jitter, so it re-anchors and logs.

Measured on the demo rig, before and after:

| | before | after |
|---|---|---|
| wander within a rendition | 27 ms (main), 67 ms (proxy) | **0 ms** over 96 segments |
| same segment, the two renditions apart | 31–81 ms, moving ~50 ms per sample | **0 ms** over 96 segments |

Why it mattered: ~68 ms is 1.7 frames at 25 fps, and the DVR player relates its
two renditions through these dates in order to lay a full-resolution still over
a low-resolution picture. The still measured two frames late, differently each
time — which a viewer describes as the picture jumping to a different moment.

**One tag was also not enough.** It is spec-legal, and a player derives the
rest by accumulating `EXTINF` — but that hangs the whole window off a value
belonging to whichever segment is currently first, so the derived timeline
shifts every time the window slides, and a consumer accumulates straight
through a discontinuity with no way to see it. On a 2h30m window that is
several thousand additions resting on one number. A tag per row costs ~50 bytes
against a segment of a couple of megabytes.

### The epoch is steered, because a media timeline is not a wall clock

Deriving the date from the media timeline removes the jitter, but pins wall
clock to a single sample. Measured on the demo rig over 16 minutes, the source
publishes **960.00 s of media in 960.39 s of real time — 407 ppm slow**,
steadily. Held, that is **3.7 s** of walk in the operator's time-of-day readout
across a 2h30m session.

So the epoch tracks it, through a small control loop with three parts. Each
matters, and the middle one was learned the hard way.

**Filter the sample.** `implied` — the wall clock this segment's publish time
suggests for the epoch — carries the publish jitter. It is low-passed into
`FlowClock::filtered` at a gain of 0.02, roughly a 200 s time constant at 2 s
segments.

**Steer towards the filtered value, not the raw one.** This is the part that
was wrong first time round. The correction is a clamp, and publish jitter is
larger than the clamp, so comparing against the raw sample made the clamp bind
on nearly every segment: the loop moved its full step toward whichever side
the noise fell, and only the *imbalance* between those excursions corrected
the drift. It was chasing noise, and it recovered only about three quarters of
the error. Filtered first, the clamp bounds how fast the epoch may move rather
than deciding how far.

**Clamp the step to 5 ms per segment.** That covers a source up to 2500 ppm
out, while no single date moves more than an eighth of a frame — against the
27-67 ms of noise that re-sampling the wall clock produced, and monotonic
rather than random.

**Two renditions must still agree exactly**, and steering threatens that: they
date the same segment at different instants, so the second would otherwise see
an epoch that had already moved. `FlowClock` therefore remembers the epoch in
force for each of the last sixteen segments, and a second caller for the same
`base_dts_90k` reproduces the first answer rather than recomputing it.

Verified on the rig: renditions **0 ms apart over 41 shared segments**, wander
within a rendition **5 ms** — the clamp, by design.

| | drift | walk across 2h30m |
|---|---|---|
| epoch pinned | 407 ppm | 3.7 s |
| clamped against the raw sample | 102 ppm | 0.9 s |
| filtered, then clamped | **89 ppm** | **0.8 s** |

**The filter did not deliver what the theory predicted, and that is unresolved.**
With a gain of 0.02 the filtered estimate should lag the ramp by about 40 ms
and then track it exactly, leaving the epoch moving at the source's own rate
and the residual near zero. Measured, it recovers 78 % of the drift and the
remainder is steady — the lag grew 27, 10, 26 and 22 ms across four
four-minute intervals, with no sign of converging further. So there is a term
here that this model does not account for; it has not been chased, because the
constant below is four times larger and swamps it.

If a future source is genuinely clock-locked, the loop simply never has
anything to do; the clamp only caps how fast it may correct.

#### Testing this: the endpoint is not enough

A tracking test that checks only where the clock ends up **does not fail when
the loop chases noise**. It still arrives in roughly the right place; it gets
there by bouncing. Steering from the raw sample survived exactly such a test.

The tell is in the *steps*: raw-sample steering makes consecutive published
dates differ from a clean segment-length step by the full clamp, every
segment. Assert that, over a run long enough for the filter to settle, and the
fault is unmissable.

### What this does *not* fix: the constant

The live edge sits **~1.6 s behind wall clock**. That is pipeline delay, baked
into the epoch's founding sample and then held. The old implementation hid it
by *defining* the date as publish time — the readout then showed ~0 s behind
while claiming the content happened when the edge finished writing it, rather
than when it was captured.

Neither knows the true capture time, because this input cannot supply it.
**SRT/MPEG-TS carries no absolute clock**: PCR is relative and there is no
RTCP sender-report path. So absolute accuracy is bounded by that constant
whatever the loop does, and the drift figures above sit inside it.

Closing it needs a source of real time. Two exist in principle:

* **Native SDI.** The edge already extracts SMPTE 12M timecode from VANC
  (bilbycast-edge#59) — but on the `sdi_io` input path, not on an SRT ingest
  of an SDI feed. A flow taking SDI directly could date segments from the
  source's own time of day.
* **A source that embeds time** in the transport — an ID3 or KLV timestamp, or
  SCTE-35 with a real `pts_adjustment` reference.

Until one of those is wired in, treat the published time of day as accurate to
about a second in absolute terms, and exact in relative terms — which is what
the DVR player actually depends on.

This is what lets a browser relate a position on its own timeline to a moment in
the real world — hls.js zeroes its timeline at whichever fragment it happened to
load first, so `currentTime` means nothing across sessions. The scrub-preview
index below depends on it, and so does any "what time was that?" surface.

## Thumbnail track (`thumbnails`)

Sprite sheets plus a WebVTT index, PUT to the same ingest as the media so they
age out with it. Off unless configured.

```json
"thumbnails": { "interval_secs": 2, "frames_per_sheet": 20, "width": 160, "height": 90 }
```

**What it is for.** Dragging a scrub bar issues ~20 seeks a second. A seek into
buffered media is immediate; every other position costs a media segment fetch.
Measured with `requestVideoFrameCallback` on a live 1080p feed, 40 seeks over
2 s on spans the player had not visited presented **0–1 frames** — at LAN speed,
at 25 Mbit/s and at 8 Mbit/s alike, and identically on a low-resolution
all-intra rendition, because the cost is the fetch and not the decode. One
sprite sheet is about the size of one media segment and covers a hundred
positions.

**Sizing.** `frames_per_sheet` bounds the *lag*, not the object count: a sheet
only exists once it is full, so the newest `interval_secs × frames_per_sheet` of
the window has no preview. At 100 frames that was the newest 200 s, which on a
300 s window is most of the bar. See #138.

**Layout.** Ten frames wide, not one strip. A hundred 160 px frames in a row is
16 000 px, past the maximum texture size on plenty of Android hardware — and a
browser that refuses the image shows no preview at all, with no error. The
validator refuses a frame width that would cross 4096 px at push time rather
than leaving it to be found on a tablet.

**The index is a rolling window.** Sheets are dropped from it by **age**, so a
sheet leaves no later than the origin evicts it. Pruning by a count derived from
the window is how this was first written, and the arithmetic erred one sheet
long — which meant the oldest stretch of the bar was permanently blank while the
index insisted it was covered.

Cue times are offsets from a UTC epoch written into the file's own header
(`X-BILBYCAST-EPOCH`), and that epoch moves with the window as sheets age out.
Held at the first sheet ever published, every cue would drift further from the
picture it names for as long as the session ran.

Decoding reuses `replay::filmstrip` — a sibling broadcast subscriber that drops
on `Lagged` and never blocks the data path. A failure here costs a preview and
never the media. Reusing it is also why **the thumbnail track needs the
`replay` Cargo feature** (on by default): without it the capture and JPEG
encode it calls do not exist, and a build that has `thumbnails` configured
raises a Warning `config` event naming the rebuild rather than publishing
nothing silently.

Cue times come from each frame's own capture instant, not from
`interval_secs x i`. Captures are skipped rather than padded when a tick yields
no frame, so a derived cadence pulled every cue after a drop earlier by the
length of the gap, accumulating across the sheet. Each cue now runs to the next
frame's real instant, so a gap is covered by the frame before it instead of
becoming a stretch of bar with no preview at all.

## LL-CMAF

LL-CMAF trades compatibility for latency. Enable it with:

```json
{
  "low_latency": true,
  "chunk_duration_ms": 500
}
```

Per segment, the edge:

1. Opens one chunked-transfer PUT request to `{ingest_url}/seg-NNNNN.m4s`.
   The first CMAF chunk carries the `styp` box; subsequent chunks omit
   it (spec-compliant).
2. Every `chunk_duration_ms` of accumulated media, emits one
   `moof + mdat` chunk into the PUT's body stream.
3. Updates `manifest.m3u8` with `#EXT-X-PART:URI="seg-NNNNN.m4s?part=N",DURATION=0.500[,INDEPENDENT=YES]`
   advertising the part. DASH `manifest.mpd` carries
   `availabilityTimeOffset` on the `SegmentTemplate`.
4. On the next segment boundary (next IDR at / past target), closes the
   current PUT and opens the next.

**Ingest requirements.** The ingest endpoint must support HTTP/1.1
chunked transfer encoding on PUT requests with indefinite body length.
Every major CDN (AWS MediaStore, Fastly, Akamai MSL, Wowza, nimble)
supports this natively; static HTTP servers like nginx/apache do not.

**Part URIs end with `?part=N`** — ingests that strip query strings or
treat `?part=` as a cache buster will break LL-HLS part playback. All
mainstream LL-HLS ingests handle this correctly.

## DASH manifest

The DASH writer emits a dynamic MPD conforming to
`urn:mpeg:dash:profile:cmaf:2019` plus
`urn:mpeg:dash:profile:isoff-live:2011`. Key attributes:

- `type="dynamic"` — signals live stream.
- `availabilityStartTime` — Unix epoch of the first emitted segment.
- `minimumUpdatePeriod` — one segment duration; clients re-fetch the
  MPD on that cadence.
- `timeShiftBufferDepth` — `available_segments × segment_duration`.
- `SegmentTemplate` with `$Number%05d$` matching the HLS media
  filenames, so both manifests reference the same `.m4s` files.
- `@codecs` — derived from SPS / AudioSpecificConfig:
  - H.264 → `avc1.{profile_idc:02X}{constraint:02X}{level_idc:02X}`
  - HEVC → `hvc1.{profile}.{compat_hex}.{L|H}{level}`
  - AAC → `mp4a.40.{aot}`
- `availabilityTimeOffset` — set to `segment_duration - chunk_duration`
  when `low_latency: true`.

DASH consumers tested: Shaka Player (ClearKey + Widevine), ExoPlayer,
dash.js 4.x. Edge cases:

- `<AdaptationSet>` is a single-adaptation per content type; for ABR
  (multiple renditions of the same content) operators should run
  multiple CMAF outputs and merge the MPDs at their origin (typical
  practice — one edge is one rendition).

## HEVC `hvc1` vs `hev1`

The init segment emits `hvc1` sample entries — parameter sets (VPS /
SPS / PPS) live only in the init, never in-band. Rationale:

- iOS Safari **requires** `hvc1` and rejects `hev1`.
- Modern Chrome / Edge / Shaka accept both.
- ExoPlayer historically preferred `hev1` but has supported `hvc1`
  since 2.12.x.

If your deployment specifically needs `hev1` (parameter sets in-band
on every IDR), open an issue; the codebase is set up to toggle.

## ClearKey CENC workflow

The default encryption experience uses W3C EME ClearKey — the simplest
DRM and universally supported by Shaka, hls.js, and dash.js.

```json
"encryption": {
  "scheme": "cenc",
  "key_id": "0123456789abcdef0123456789abcdef",
  "key": "fedcba9876543210fedcba9876543210",
  "pssh_boxes": []
}
```

1. The edge emits an `encv` (or `enca`) sample entry in the init that
   wraps `avc1`/`hvc1`/`mp4a` with a `sinf/frma/schm/schi/tenc` chain.
2. Each video sample is subsample-encrypted: NAL length prefix + NAL
   header + ~32 bytes of slice header are left clear; the remainder of
   the VCL NAL is encrypted. Parameter-set NALs stay fully clear.
3. For `cbcs`, the encrypted span is rounded down to a multiple of 16
   bytes (AES block size).
4. AAC samples *would be* whole-encrypted with no subsample split —
   `encrypt_audio_sample` implements it, but nothing calls it. **An
   encrypted output is video-only** (see Limitations), so no audio
   sample reaches this path at all today.
5. `senc` / `saio` / `saiz` boxes with byte-accurate offsets are
   emitted in every `traf`.
6. A version-1 ClearKey `pssh` box is added to `moov` carrying the
   `key_id`.

Clients fetch the clear key via the standard W3C EME ClearKey license
flow — operators return `{keys: [{kty: "oct", kid, k}]}` in JSON from
their `licenseUrl` response. bilbycast-edge does **not** run the
license server — that is operator-managed and lives outside the edge.

### Commercial DRM (Widevine / PlayReady / FairPlay)

The edge does not integrate directly with Widevine / PlayReady license
servers. Instead, operators:

1. Register the content key with their DRM provider (e.g. Google
   Widevine, Microsoft PlayReady, EZDRM, BuyDRM KeyOS, Axinom, Nagra).
   The provider returns a pre-built `pssh` box per system.
2. Paste the hex-encoded box bytes into `pssh_boxes` — one line per
   system:

   ```json
   "pssh_boxes": [
     "00000034707373680000000 ... (Widevine)",
     "00000088707373680000000 ... (PlayReady)"
   ]
   ```

3. The edge wraps each entry verbatim into `moov` alongside the
   ClearKey PSSH. Players pick the system matching their CDM.

**Security note.** The content key itself still lives in the edge
config (`encryption.key`). Operators are responsible for protecting
the node config and, if needed, rotating keys via the secret-rotation
flow documented in the root `CLAUDE.md`.

### FairPlay (cbcs only)

Apple FairPlay requires `cbcs` scheme with a 1:9 block pattern and a
constant all-zero IV. Use:

```json
"encryption": {
  "scheme": "cbcs",
  "key_id": "...",
  "key": "...",
  "pssh_boxes": ["<FairPlay KSM PSSH hex>"]
}
```

The edge emits a `tenc` with `default_crypt_byte_block=1`,
`default_skip_byte_block=9`, `default_Per_Sample_IV_Size=0`, and a
16-byte `default_constant_IV` of zeros. Verified against Safari's
native FairPlay EME path.

## Ingest compatibility

Observed behavior against common production ingests:

| Ingest | Standard CMAF | LL-CMAF | ClearKey | Notes |
|--------|---------------|---------|----------|-------|
| AWS MediaStore (HTTP) | ✓ | ✓ | ✓ | Default Content-Type `video/mp4`, `application/vnd.apple.mpegurl`, `application/dash+xml` work. |
| Fastly OA (CMAF Live) | ✓ | ✓ | ✓ | Requires `Authorization: Bearer` — set `auth_token`. |
| Akamai MSL | ✓ | ✓ | ✓ | MSL requires specific URL layout; `ingest_url` should include the MSL path. |
| nimble / Wowza | ✓ | ✓ | ✓ | |
| static nginx | ✓ (whole-segment) | ✗ | ✓ | nginx by default buffers chunked requests in memory; LL breaks. |

If your ingest rejects the default `application/dash+xml` content type
for `.mpd`, the edge has no override today — open an issue.

## File naming

The edge uses the following filenames under `{ingest_url}`:

- `init.mp4` — init segment (ftyp + moov).
- `seg-NNNNN.m4s` — video / muxed media segment (5-digit zero-padded
  sequence number).
- `aud-NNNNN.m4s` — audio-only media segment. **Reserved and not
  emitted**: when a source has audio it is muxed into `seg-NNNNN.m4s`
  alongside the video, so a separate audio object never appears.
- `thumbs-NNNNN.jpg` — scrub-preview sprite sheet (see below). Only when
  `thumbnails` is configured.
- `thumbs.vtt` — the WebVTT index describing those sheets.
- `manifest.m3u8` — HLS playlist.
- `manifest.mpd` — DASH manifest.
- `seg-NNNNN.m4s?part=K` — LL-HLS part URI (query string distinguishes
  parts within the same segment PUT).

File names are fixed in Phase 5; operators who need custom naming
should set up a URL-rewriting reverse proxy in front of their ingest.

## Testing

- **Unit tests** (`cargo test cmaf::`): 53 tests covering `BoxWriter`
  round-trips, `avcC` / `hvcC` / `esds` shape, SPS resolution parsing,
  m3u8 and MPD golden files, media-segment `data_offset` patching,
  `tfdt` base DTS, AES-CTR / AES-CBC round-trips, CENC subsample
  splitter, PTS 33-bit unwrap, IDR-cut segmenter, and HLS part rows.
- **Interop matrix**
  (`testbed/scripts/cmaf_full_interop_test.sh`): 6 scenarios — H.264
  HLS, H.264 HLS+DASH, HEVC DASH, H.264 LL with chunks, CENC `cenc`,
  CENC `cbcs`. Each scenario feeds real ffmpeg output into the edge
  and validates init.mp4 + segments + manifests via ffprobe + binary
  inspection.
- **Load test** (`testbed/scripts/cmaf_load_test.sh`): 60 s of 15 Mbps
  1080p30 H.264+AAC; verifies no broadcast lag, peak CPU <10 %,
  correct bitrate, segment count, and ffprobe acceptance.
- **CMAF HTTP sink** (`testbed/scripts/cmaf_sink.py`): minimal Python
  HTTP server that accepts PUT + POST (including chunked transfer) and
  saves the body under the last path component. Reusable for local
  development against edge CMAF output.

## Known limitations

- The 32-byte slice-header conservative estimate for CENC subsample
  splitting is safe but leaves ~32 more bytes clear than a bit-accurate
  parser would. If the operator needs maximum encryption coverage,
  parse the slice header precisely and shrink the clear prefix.
- Only single-rendition outputs are supported today. Multi-bitrate ABR
  is produced by running multiple CMAF outputs and merging at the
  CDN / origin (standard workflow).
- **The track list is fixed at the first `init.mp4`.** A browser builds
  its decoders from that file once, so a track cannot be added later:
  declaring an audio track no fragment fills stalls MSE *silently*
  (decoders initialise, nothing ever arrives, nothing errors), and
  sending audio the init never declared fails the same quiet way. The
  first init therefore waits up to 3 s for an audio track to appear
  before committing to video-only; a source whose audio starts after
  that is carried as video-only for the life of the flow, with a
  warning, and needs a flow restart to pick it up.
- **Encrypted (CENC) outputs are video-only.** `encrypt_audio_sample`
  exists but is unwired, and shipping the audio track in the clear
  under an init that declares the output encrypted would be worse than
  omitting it. The decision is taken before "does this source have
  audio", so an encrypted output never declares a track it cannot fill.
- **LL-CMAF outputs are video-only.** A chunk is built by
  `build_segment_chunk`, which writes one `traf` for the video track,
  so `low_latency: true` publishes a video-only `init.mp4` whatever the
  source carries. LL-CMAF also does **not** apply `encryption`, so the two
  are now **refused together at validation** rather than starting an output
  whose chunks go out in the clear while every surface says it is encrypted
  (bilbycast-edge#135).
- No live-to-VOD archival — the rolling playlist caps at `max_segments`
  and old `.m4s` files are not deleted on the ingest side. Operators
  must configure CDN / object-store retention externally.
- HLS `#EXT-X-DISCONTINUITY` is never emitted. A source format change
  mid-stream (e.g. input switch between H.264 and HEVC) will produce an
  incoherent segment sequence. Input-switch flows should restart the
  CMAF output when the source codec family changes.
