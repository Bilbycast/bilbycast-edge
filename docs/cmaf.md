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
segments run long — up to a GOP longer than the target — and each row's
`EXTINF` says so. `#EXT-X-TARGETDURATION` is derived from the longest row in
the window rather than from the config precisely so that stays legal; see
[Target duration](#target-duration-ext-x-targetduration) for what it costs
(a reload cadence and a low-latency hold-back sized for the longest segment
the flow has produced).

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

## Target duration (`#EXT-X-TARGETDURATION`)

The tag is derived from the **rows** — the longest one in the window, held as a
high-water mark — not from `segment_duration_secs`.

RFC 8216 §4.3.3.1 puts the constraint on the rows: every `EXTINF`, rounded to
the *nearest* integer, must be no greater than the advertised target. Nearest,
not up — a 2.4 s row is 2 against a target of 2, and only 2.5 s needs 3. The
configured target is kept as a floor, because segments cut at or past it and a
window that has not yet seen a long segment should still advertise the familiar
number.

This matters because the segmenter cuts on the first IDR **at or past** the
target, so a GOP that does not divide it produces rows longer than it — 5 s rows
against a 2 s target for a 5 s GOP. Rows carry their real length (see [A segment
is dated by the length it actually ran](#a-segment-is-dated-by-the-length-it-actually-ran)),
so deriving the tag from the config published `#EXT-X-TARGETDURATION:2` over
three `#EXTINF:5.000` rows. Apple's `mediastreamvalidator` errors on that, and
on a low-latency playlist it took `HOLD-BACK` down with it: three times the
*configured* target is `6.000` s against 5 s segments — 1.2 segments of
hold-back, where the tag's own definition requires three target durations, so a
player started inside a segment the origin had not finished writing. `HOLD-BACK`
is now three times the target that was actually published.

**The published value is a high-water mark**, held per output in
`CmafState::target_duration_published`. It rises the moment a longer row enters
the window; it does not fall when that row is trimmed. A player reads this once
and sizes its reload cadence, its buffer and — in low-latency mode — the point
it starts playing from against it, so a value that shrinks between reloads
retracts a decision it has already acted on — the spec's model is a playlist a
server appends to and trims, not one whose declared bounds move under a player.
A source alternating GOP lengths would otherwise move the tag on every trim. The cost of holding the high
mark is a reload interval sized for the longest segment the flow has produced,
which is the conservative direction; the cost of oscillating it is a player
re-planning mid-session.

Be aware how far that cost reaches. The mark is only ever raised — not on a
re-anchor, not on a source restart — so **one IDR-starved segment sets it for
the life of the output**, and because `HOLD-BACK` is three times the published
tag, a single 15 s segment pins `HOLD-BACK` at 45 s and triples the distance
back from the live edge at which a non-low-latency player starts. Legal, and in
the safe direction, but it is start latency and it does not come back. It does
reset on a flow restart, which builds a fresh `CmafState` — the one path where
the no-shrink property does not hold, and harmless only because the playlist is
empty at that moment anyway.

Operators who want the tag to sit exactly on the configured value should set
`segment_duration_secs` to a multiple of the encoder's GOP.

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
and therefore the same `base_dts_90k`, publish *identical* dates — subject to
one qualification about the 33-bit PTS wrap, below. A sample implying an epoch
more than ten seconds from the held one is a source restart or a PTS
discontinuity rather than jitter, so it re-anchors, logs, and declares itself on
the playlist.

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

### Reading the clock is not the same as steering it

A low-latency output publishes a row for the segment it is *currently writing*,
and republishes it on every chunk emission — four times a segment at the default
500 ms chunk against a 2 s segment, ten times at 200 ms. Those calls may only
read the clock.

Dating an open segment with the close-time arithmetic — which is what
`publish_ll_hls` did — feeds the loop a sample resting on an assumption that is
false. `implied = now - segment_duration - media_secs` says "this segment closed
about now"; the segment has in fact only just started. The sample lands
`segment_duration - chunk_duration` early, so the epoch does, and so does every
date the flow publishes: **1.5 s early at the defaults**, 1.8 s at 200 ms
chunks. It is wrong from segment zero rather than converging, because the first
such call is the one that *founds* the flow's epoch — and because the biased
call also remembered the segment, the honest sample taken when it genuinely
closed hit the per-segment cache and was discarded. Both renditions of a flow
share the epoch, so a plain sibling doing everything right was dragged with it:
the two agreed exactly, and both were wrong by twenty to seventy times the
wander this whole mechanism exists to remove.

`FlowClock::date_open_segment` is therefore read-only. It looks the position up
in the per-segment history, falls back to the current epoch, and writes nothing
back. It returns `None` — no `#EXT-X-PROGRAM-DATE-TIME` on that row at all —
when the flow has not closed a segment yet, or when the position sits on a
timeline the held epoch does not describe. So a low-latency output's opening
playlist carries no date for about one segment. The tag is optional (RFC 8216
§4.3.2.6); a date wrong by most of a segment is not the better trade. Seeding
the epoch from the open segment was the alternative, and it founds the flow's
clock — and its sibling's — on a sample taken at an arbitrary instant inside a
segment, carrying whatever the pipeline delay was at that moment, in exchange
for one segment's worth of tag.

The suite was green through all of this, because every date test called the
close path.

**And the first two tests written for the fix did not close that hole either.**
`the_low_latency_path_publishes_open_segments_on_wall_time` drives the real
sequence — open, ten chunk publishes, close — and
`a_low_latency_rendition_does_not_drag_its_plain_sibling` asserts against wall
clock rather than against the sibling, which is what catches a flow where both
renditions are wrong together. But both call `open_segment_date` themselves, so
they pin the *arithmetic* and not the *wiring*, and the fix is a wiring change:
one call site choosing the read-only function. Reverting exactly that line, with
`FlowClock` untouched, put the flow epoch back to 1.8 s early and left every one
of those tests passing. The only thing that noticed was `dead_code` under
`-D warnings`, which is a lint, not a test.

The seam that closes it is `ll_playlist_entries`. `publish_ll_hls` cannot be
called from a test — it takes a `CmafState` holding a live chunked-PUT handle
and ends in an HTTP request — so the row-building was lifted out of it into a
pure function taking an `OpenSegmentRow` (sequence number, URI, parts, base
DTS) instead of the `LlSegment` that owns the socket.
`a_low_latency_playlist_never_founds_the_flow_clock` asserts that publishing an
in-progress row on a flow that has closed nothing leaves `FLOW_EPOCHS` without
an entry at all, and
`publishing_a_low_latency_playlist_does_not_drag_the_flow_epoch` drives thirty
segments of the real publish sequence through the same functions the output
calls. Under the revert the first fails on the founding call and the second
fails on segment 0, chunk 1, at −1800 ms.

### A segment is dated by the length it actually ran

The segmenter cuts on the first IDR **at or past** the target, so a segment is
as long as its last GOP made it: equal to the target only when the GOP divides
it, and up to a GOP longer when it does not. A 1.5 s GOP against a 2 s target
produces 3 s segments; a 5 s GOP against the same target produces 5 s ones.

The plain path has always passed the closed segment's real
`duration_90k / 90 000` to the flow clock. The low-latency close passed
`config.segment_duration_secs` — the configured target. That tells the clock the
segment ended `actual − nominal` earlier than it did, so `implied` lands late by
exactly that, and so does the epoch it founds: **+1000 ms** in the 1.5 s-GOP
example, **+3000 ms** for a 5 s GOP against a 2 s target. It does not converge,
because every sample says the same wrong thing — there is no error for the slew
to correct against.

It is the open-segment bug one magnitude down, and it spreads the same way.
Both renditions share the epoch and the per-segment cache makes the second
reproduce the first's answer, so whichever dates first decides for both — and
the low-latency output usually did, because it had only its PUT's tail
outstanding at the close while the plain path was still pushing a whole segment
body. Both paths now sample the clock *before* their upload rather than after it
(see [The close-time sample is taken before the
upload](#the-close-time-sample-is-taken-before-the-upload)), so that race is
down to scheduling — but the sharing is unchanged, and so is the rule that a
biased sample poisons the flow rather than one output of it.

The real length is already in hand at the close: `push()` runs in
`handle_video`, before `handle_ll_cmaf`, so the segmenter has already opened the
next segment and `open_segment_base_dts_90k()` reports where the closed one
ended. `closed_ll_entry` takes that as `next_base_dts_90k` and uses the
difference for both the date and the row's `EXTINF`, falling back to the nominal
figure only when there is no next segment to measure against. Leaving `EXTINF`
nominal while the date came from the real length would have been worse than
either: the playlist would then state a 2.000 s step between two rows dated 3 s
apart.

That derivation is a *call site*, and call sites are where every bug in this
area has shipped. `closed_ll_entry` takes the next segment's base as an
argument, so a test can hand it a literal and prove nothing about where the
output gets it; the derivation itself is
`CmafState::closed_segment_end_dts_90k`, pinned by
`the_closed_row_takes_its_length_from_the_segmenter`, which drives a real
`VideoSegmenter` and fails when the derivation is reverted to `None`. The
identity it rests on — after a segment-closing `push`, the open segment begins
exactly where the closed one ended — is pinned in the segmenter's own tests by
`the_open_segment_begins_where_the_closed_one_ended`.

Rows carrying their real length is also why `#EXT-X-TARGETDURATION` had to stop
coming from the config — see [Target duration](#target-duration-ext-x-targetduration).

### One flow, one lap of the PTS clock

The epoch is keyed on the flow because two renditions see the same RTP packets
and `PtsUnwrap` does not rebase, so `base_dts_90k` is the same number in both.
That holds only while both outputs have counted the same 33-bit PTS wraps, and
`PtsUnwrap` lives in the segmenter — one per output *and* per track, counting
from whenever that output started.

An MPEG-TS PTS wraps every 2^33 ticks: **26 h 30 m** at 90 kHz. Add a second
rendition to a 24/7 channel after that, or let an `UpdateFlow` restart one, and
the new instance reports the same content 2^33 ticks — 95 443.7 s — below its
sibling. Each then implies an epoch more than a day from the other's, so each
re-anchors the other: `recent` is cleared on every segment and the two can never
agree, the epoch collapses back to `now - segment_duration` (the per-segment
sampling this mechanism replaced), and the node logs a source restart twice a
segment forever, naming a cause that never happened.

`FlowClock::align` corrects it where it is only a presentation detail: an
incoming position is moved by whole laps until it lands within half a lap of the
flow's last one. A wrap *inside* one output is already counted by that output's
own `PtsUnwrap` and passes through untouched; a genuine restart moves by minutes
or hours, nowhere near the 13-hour half-lap, and reaches the re-anchor test
unchanged.

**Rebasing inside `PtsUnwrap` itself (returning `pts - first_pts`) would fix it
at the source, and must not be done.** Video and audio hold separate
`PtsUnwrap` instances whose first samples are different frames, while
`build_muxed_segment` writes both tracks' `base_media_decode_time` into one
`moof` and relies on them sharing the source's absolute timeline. Rebasing each
independently would offset audio from video by the gap between their first
samples — permanently, on every flow, on the one path a browser decodes.

### A re-anchor is declared, not absorbed

Two things move the published dates discontinuously, and neither is a fault to
be removed:

* **A source restart or PTS discontinuity.** The implied epoch moves by the
  whole elapsed time, crosses `EPOCH_REANCHOR_SECS` (10 s), and the clock
  re-anchors on the new sample.
* **A source outside the slew band.** The epoch corrects by at most 5 ms per
  segment — about 2500 ppm at 2 s segments — so a source further out than that
  falls behind until the error crosses the same 10 s and snaps. Simulated, a
  4000 ppm source snaps ten seconds every ~110 minutes and a 10 000 ppm source
  every ~22 minutes, indefinitely.

**The re-anchor test runs before the per-segment cache, not after it.** The
other way round — which is how it shipped — a source coming back at PTS 0 while
`0` was still one of the sixteen remembered positions was answered out of the
cache with the epoch from an hour earlier: the re-anchor branch unreachable,
`recent` never cleared, no log line, nothing on the Events page. Sixteen entries
is about 32 s of a 2 s-segment flow, which is exactly when a flapping source
comes back. The cache exists to make two renditions agree about one segment; it
does not get to outvote the discontinuity detector.

The log escalates with the shape of the fault. One re-anchor is news and goes
out at INFO. A *run* of them — three or more inside a minute of each other — is
reported at WARN, and then once every hundred, because a re-anchor loop reading
as routine news is what made the PTS-wrap case above so hard to see.

And the playlist says so. `date_closed_segment` reports whether it re-anchored,
the flag rides on the playlist row, and `build_hls_playlist` writes
`#EXT-X-DISCONTINUITY` immediately before that row's date. Without it the window
carries the contradiction in silence: on `dvr_window_secs: 9000` that is 4499
rows on the old epoch and one on the new, every one advertising a clean
`#EXTINF:2.000` step from its neighbour — which RFC 8216 §6.2.1 makes the
server's job to signal.

`#EXT-X-DISCONTINUITY-SEQUENCE` carries the count of tagged rows that have
already been trimmed off the front of the window. It is counted as they leave
(`CmafState::trim_playlist`) rather than derived from what remains — once the
row is gone the playlist holds no trace of it, and a player reloading across
that trim would see its own count go backwards. It is omitted while the count is
zero, which an absent tag already means, so a stream that never re-anchors
writes exactly the playlist it always did.

**Both renditions carry the tag, or the count they publish diverges.** A
re-anchor is discovered exactly once — by whichever rendition closes the segment
first. The sibling closing that same segment lands within tens of milliseconds
of the epoch that was just re-anchored to, so it never trips the ten-second test
itself; it is answered out of the per-segment cache. That cache remembered the
epoch and returned a hard-coded `false` for the flag, so the second rendition
published the *identical* hour-long jump with a clean `#EXTINF:2.000` step and
no `#EXT-X-DISCONTINUITY` at all — and from then on the two disagreed about
`#EXT-X-DISCONTINUITY-SEQUENCE`, which RFC 8216 §4.3.3.3 makes a number a player
carries with it across a rendition switch. `recent` therefore holds
`(base_dts_90k, epoch, discontinuity)`: the sibling reproduces both, exactly, or
neither.

The low-latency in-progress row never *declares* a discontinuity — one is
discovered by the sample that re-anchors the clock, and the open-segment path
takes no sample. It does reproduce one a sibling has already found under that
segment, for the same reason: the re-anchor has already moved the date the row
is about to publish, and a moved date with no tag is the contradiction the tag
exists to close.

#### Both renditions carry the tag only while their skew is under one segment

"Both renditions tag the same row" is conditional, and the condition is how far
apart in wall time the two renditions close the *same* segment. The bound is one
segment duration.

Modelled against a restart on segment 20 of a 2 s-segment flow: 45 ms, 0.5 s,
1.5 s and 1.9 s of skew produce identical dates and identical flags on both
renditions, and one tag each. At 2.5 s — one segment — the leader tags rows 20
and 21 while the laggard tags 19 and 21: two date disagreements and two flag
disagreements, and `#EXT-X-DISCONTINUITY-SEQUENCE` diverges as soon as the
window rolls past row 19. At 8 s of skew it is six of each.

The mechanism is `reanchor`'s `recent.clear()`. A rendition still holding a
*pre*-restart segment to close when the leader re-anchors finds the per-segment
cache empty, takes a sample of its own implying the old epoch, and re-anchors
back — after which the two take turns re-anchoring each other for as long as the
skew lasts. `FlowClock`'s own comment assumes the renditions run "within a
segment or two of each other"; this is what that assumption is worth, and the
`recent` field says so where the cross-rendition invariant is claimed.

**The clear is load-bearing, which is why the bound is not simply widened.** A
source restarting at PTS 0 re-uses `base_dts` values the cache is still holding,
and the lookup returns the *oldest* match, so keeping the entries across a
re-anchor would answer the next post-restart segment out of the pre-restart
epoch — an hour-stale date published with no tag at all, which is strictly worse
than a skewed rendition disagreeing. Widening it means letting the cache be
consulted before the re-anchor test (the reverse of the ordering [a previous
fix](#a-re-anchor-is-declared-not-absorbed) established) with an entry honoured
only when the fresh sample agrees with it to within `EPOCH_REANCHOR_SECS`. That
is a change to the detector's contract on both dating paths, and it is not worth
making blind: the mechanism that could realistically push two renditions of one
flow seconds apart was the origin, and that one is closed below.

The failure is at least reported rather than silent — a run of three re-anchors
inside a minute escalates to the WARN that names this exact cause ("either two
outputs of this flow disagree about the timeline, or the source restarts on
every segment").

### The close-time sample is taken before the upload

`date_closed_segment` reads its `now` as the instant the segment closed: it
subtracts the segment's length and its media position from it to imply the
flow's epoch. The segment closes in `push()`. Both publish paths used to sample
the clock *after* the upload returned — after `http_put` on the plain path,
after the chunked PUT's `finish()` on the low-latency one — so the origin's
response time was inside every sample.

The upload client's request timeout is 30 s. A slow-but-**successful** PUT could
therefore hand the clock a close-time sample past `EPOCH_REANCHOR_SECS` and
re-anchor a timeline that never moved: modelled, 5.0 s and 9.9 s stalls produce
no tag, while 10.1 s, 15 s and 29 s stalls each produce two — the stalled
segment and the one after it — before the clock settles again. Both renditions
took the tag from the cache, so they stayed in agreement, and the dates really
had jumped, so the tag was not dishonest. It was still a discontinuity caused by
a busy origin rather than by the source.

Sampling before the upload removes the origin from the sample entirely. It also
takes the origin's latency out of every *ordinary* sample, where the filter had
been absorbing it, stops two renditions publishing to different origins
disagreeing by the difference in their response times, and shrinks the skew that
the bound above depends on to encode and scheduling jitter — tens of
milliseconds, measured.

### A re-anchor reaches HLS only, not DASH

`build_dash_mpd` has no discontinuity input, and `availability_start_unix` is
latched at the first segment and never re-anchored, yet is still fed to the MPD
after a restart has moved every date. An output configured
`manifests: ["hls", "dash"]` therefore declares the re-anchor to HLS players and
hides the same jump from DASH players. This asymmetry is **new** — it was
created by adding the HLS tag.

Re-anchoring `availabilityStartTime` would not fix it and would make things
worse: it is the anchor from which the availability of every segment in a
dynamic MPD is computed, including segments already delivered, so moving it
retroactively redefines the timeline rather than declaring a break in it. DASH's
signal for a discontinuity is a new `Period`, with `Period@start` at the break
and the new Period's own `SegmentTemplate` anchoring.

That cannot be bolted on to this MPD as it stands. `write_segment_template` sets
`@startNumber` to the *newest* segment on every publish, so segment N maps to
presentation time 0 in the current Period and the mapping between number and
presentation time moves on every MPD update — a Period boundary placed on that
timeline would carry no information. Fixing the discontinuity signal means
fixing DASH segment addressing first (a stable `@startNumber` plus
`@presentationTimeOffset`, or `SegmentTimeline` with explicit `S@t`), which
wants a `dashif-conformance` run before it lands. Tracked as
[bilbycast-edge#144](https://github.com/Bilbycast/bilbycast-edge/issues/144);
until it is closed, an output that needs the signal should publish HLS.

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
   `availabilityTimeOffset` on the `SegmentTemplate`. The in-progress row
   carries `#EXT-X-PROGRAM-DATE-TIME` only once the flow has closed its first
   segment — see [Reading the clock is not the same as steering
   it](#reading-the-clock-is-not-the-same-as-steering-it) — and its `#EXTINF`
   is the greater of the configured target and the parts already listed under
   it, since the segment's real length is not settled until the IDR that ends
   it arrives. Pinned at the target, a 5 s GOP against a 2 s target published
   `#EXTINF:2.000` above twenty-five `#EXT-X-PART:DURATION=0.200` rows
   accounting for 5 s, in the same playlist.
4. On the next segment boundary (next IDR at / past target), closes the
   current PUT and opens the next. The row the closed segment contributes is
   dated — and given its `EXTINF` — by how long it *actually* ran, which is the
   next segment's base DTS minus its own; see [A segment is dated by the length
   it actually ran](#a-segment-is-dated-by-the-length-it-actually-ran).

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
- **One `Period`, and no discontinuity signal.** A media-timeline re-anchor —
  a source restart, a PTS discontinuity, or a snap outside the slew band — is
  written to the HLS playlist as `#EXT-X-DISCONTINUITY` and is invisible here.
  See [A re-anchor reaches HLS only, not
  DASH](#a-re-anchor-reaches-hls-only-not-dash) for why the answer is a new
  `Period` and not a moved `availabilityStartTime`
  ([bilbycast-edge#144](https://github.com/Bilbycast/bilbycast-edge/issues/144)).
- `@startNumber` is set to the newest segment on every publish, so the MPD
  addresses the live edge rather than the rolling window that
  `timeShiftBufferDepth` advertises. Browser DVR is served over HLS today.

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

- **Unit tests** (`cargo test cmaf::`): 116 tests covering `BoxWriter`
  round-trips, `avcC` / `hvcC` / `esds` shape, SPS resolution parsing,
  m3u8 and MPD golden files, media-segment `data_offset` patching,
  `tfdt` base DTS, AES-CTR / AES-CBC round-trips, CENC subsample
  splitter, PTS 33-bit unwrap, IDR-cut segmenter, HLS part rows, the
  target-duration derivation, the thumbnail index, and the flow clock — the
  last of which takes `now` as an argument, so the low-latency open-segment
  sequence, a restart onto a remembered position, a rendition added across a
  PTS wrap, a snap outside the slew band and two renditions skewed across a
  re-anchor are all driven without a wall clock.

  They are *not* driven without `FLOW_EPOCHS`: only two of the twenty-three
  construct a `FlowClock` directly, and the rest reach it through
  `segment_date_marking` / `open_segment_date`, which take the process-global
  lock. That is deliberate — the bugs here were call sites, so the tests go
  through the same door the output does — but it means the map is shared state
  the suite never clears, and the tests are isolated only by each using a flow
  id of its own. A new test that reuses another's id inherits its epoch and
  will fail somewhere far from the cause.

  Nine of the twenty-three flow-clock tests cover the low-latency path
  specifically, and the rule they follow is that **every bug that has shipped here was a call site**, so a
  test which calls the arithmetic directly proves nothing about the output. The
  open path is driven through `ll_playlist_entries`, which is why
  `OpenSegmentRow` exists at all: `LlSegment` owns a live chunked-PUT handle and
  cannot be built without a socket. The close path was *not* — `closed_ll_entry`
  takes the next segment's base as a parameter and both tests that drove it
  handed it a literal `Some(base + ticks)`, so the derivation the output
  actually uses had no test caller and reverting it to `None` restored the whole
  bug with the suite green. It now runs through
  `CmafState::closed_segment_end_dts_90k`, pinned by
  `the_closed_row_takes_its_length_from_the_segmenter` (a real `VideoSegmenter`,
  pushed until it cuts) and by the segmenter's own
  `the_open_segment_begins_where_the_closed_one_ended`, which pins the identity
  underneath it. Verified by mutation: reverting the derivation to `None` fails
  exactly that one test and leaves the other 115 green.
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

- **`EXT-X-PART` rows are emitted after their own segment's `#EXTINF`.**
  RFC 8216bis §4.4.4.9 places a segment's partial-segment rows *before*
  its `#EXTINF`; trailing parts belong to the next, not-yet-complete
  segment. `build_hls_playlist` hangs the open segment's parts off the
  last entry, which is that segment's own row, so they land after it. A
  conforming player therefore attributes them to the following media
  sequence number, which breaks `_HLS_msn` / `_HLS_part` blocking-reload
  addressing, and reads the open segment as complete and fetchable while
  its chunked PUT is still running. Pre-dates the flow-clock work and is
  not fixed by it. Fixing it means restructuring how parts attach to
  entries, and wants a `mediastreamvalidator` run to confirm.
- **`EXT-X-PART:DURATION` is the nominal chunk target, not the chunk's
  real span.** `take_pending_chunk` takes samples spanning *at least*
  `chunk_duration_90k`, so the advertised figure under-claims. The
  direction is safe — it is what lets the in-progress row's floor never
  advertise media the origin does not hold — but the parts do not sum to
  the segment's `#EXTINF` when it lands.

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
- **A re-anchor of the media timeline is signalled on HLS only.** DASH players
  of the same output are told nothing — see [A re-anchor reaches HLS only, not
  DASH](#a-re-anchor-reaches-hls-only-not-dash) for why the fix is a new
  `Period` rather than a moved `availabilityStartTime`, and why it is blocked on
  DASH segment addressing. Tracked as
  [bilbycast-edge#144](https://github.com/Bilbycast/bilbycast-edge/issues/144).
- **The low-latency in-progress row carries an `#EXTINF` at all.** RFC 8216's
  model for a segment still being written is that it has no `#EXTINF` until it
  is complete and is advertised by its `#EXT-X-PART` rows alone. This
  implementation emits the row early because that is what makes its part rows
  reachable — `build_hls_playlist` hangs them off the last entry — so the row's
  duration is a floor rather than a guess: the greater of the configured target
  and what the parts already published account for, never more. It is rewritten
  with the segment's real length the moment it closes.
- No live-to-VOD archival — the rolling playlist caps at `max_segments`
  and old `.m4s` files are not deleted on the ingest side. Operators
  must configure CDN / object-store retention externally.
- **`#EXT-X-DISCONTINUITY` covers the clock, not the codec.** It is emitted
  when the flow clock re-anchors — a source restart, a PTS discontinuity, or a
  source clock too far out for the epoch to slew after (see [A re-anchor is
  declared, not absorbed](#a-re-anchor-is-declared-not-absorbed)). A source
  *format* change mid-stream is a different break and the tag does not rescue
  it: `init.mp4` has already declared the track list, so an input switch
  between H.264 and HEVC produces a segment sequence the player cannot decode
  whatever the playlist says. Input-switch flows should still restart the CMAF
  output when the source codec family changes.
