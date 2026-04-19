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
  the in-process fdk-aac backend.
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
4. AAC samples are whole-encrypted with no subsample split.
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
- `aud-NNNNN.m4s` — audio-only media segment (reserved; not emitted in
  the default muxed-segment configuration).
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
- No live-to-VOD archival — the rolling playlist caps at `max_segments`
  and old `.m4s` files are not deleted on the ingest side. Operators
  must configure CDN / object-store retention externally.
- HLS `#EXT-X-DISCONTINUITY` is never emitted. A source format change
  mid-stream (e.g. input switch between H.264 and HEVC) will produce an
  incoherent segment sequence. Input-switch flows should restart the
  CMAF output when the source codec family changes.
