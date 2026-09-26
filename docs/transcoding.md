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
| **CMAF / CMAF-LL** | ✅ (AAC family only) | ✅ (requires `audio_encode`; channel routing in the stage, the rate in the encoder's resampler — accepted and ignored before 2026-09) | ✅ | fMP4 / CMAF segments with HLS m3u8 + DASH MPD; the operator's `gop_size` is honoured when `video_encode` is set, and segments cut on that GOP's IDRs, so a set one should divide `segment_duration × fps`; unset, the GOP tiles the segment at the measured source rate (at most 2 s per GOP — 50 frames for 2 s segments at 25 fps, 60 at 29.97). Codec work runs in `block_in_place`. See [`docs/cmaf.md`](cmaf.md) for the full reference. |

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
| **ST 2110-31** | ✅ (`s302m` only) | ❌ | ❌ | AES3 opaque. `transcode` is rejected outright — a linear-PCM stage would destroy the SMPTE 337M sub-frames. `audio_encode` is accepted for `s302m` alone, which re-packetises the AES3 bytes into a SMPTE 302M private PES with no decode step. See "Allowed codecs per input" below. |
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
sets `AVFrame.pict_type = AV_PICTURE_TYPE_I`, which libx264 / libx265
honour by emitting an IDR for that frame. **NVENC does not**: it codes a
forced *intra* picture (`NV_ENC_PIC_FLAG_FORCEINTRA`), not an IDR, unless
the encoder's `forced-idr` private option is set, which the wrapper does
not do — and only an IDR gets `AV_PKT_FLAG_KEY`, so the frame comes back
`keyframe = false` and a receiver still resyncs on NVENC's next natural
IDR.

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
  "codec": "aac_lc",         // not one closed union — the accepted set is
                             // per input / output class, see below
  "source_audio_pid": 4352,  // optional; pin the source audio ES PID
  "bitrate_kbps": 128,       // optional; per-codec default
  "sample_rate":  48000,     // optional; defaults to source
  "channels":     2,         // optional; defaults to source
  "silent_fallback": false,  // optional; RTMP / WebRTC / CMAF only
  "ts_signalling": "auto",   // optional; codec = "ac3" on TS outputs / TS
                             // inputs only: "auto" | "dvb" | "atsc" — see
                             // "What the output PMT says"

  // Opus only — REFUSED at validation on any other codec (see below):
  "opus_vbr_mode":          "cbr",  // optional; unset | "vbr" | "cbr"
  "opus_fec":               false,  // optional; default false
  "opus_dtx":               false,  // optional; default false
  "opus_frame_duration_ms": 20      // optional; 5 | 10 | 20 | 40 | 60
}
```

### `source_audio_pid` — pinning the source track

Unset (the default), the replacer locks onto the **first** audio stream
in the active program's PMT whose `stream_type` is one the replacer can
decode: `0x0F` (AAC ADTS), `0x11` (AAC LATM), `0x03` / `0x04`
(MPEG-1/2), `0x80` / `0x81` / `0xC1` (AC-3), `0x87` / `0xC2` (E-AC-3),
or `0x06` (DVB private) resolved through its descriptor to one of AC-3 /
E-AC-3 / LATM-AAC — a `0x06` that resolves to DTS, Opus, SMPTE 302M or
AC-4 is walked past, not locked onto. Set it to
pin a specific elementary PID instead — the case that matters is a
program carrying several audio tracks (EN / FR / 5.1) where first-match
would pick the wrong one, and the case where an input swap reorders the
PMT. Range `0x0010..=0x1FFE`; anything outside that is rejected at
config load (`source_audio_pid … out of range`), so the reserved
system PIDs and the NULL PID cannot be named.

If the pinned PID is **absent from the live PMT** (or carries a codec
the replacer cannot decode) the replacer does not fail — it falls back
to first-matching-codec and raises a **Warning event**,
`audio_source_pid_not_found` (category `flow`, output-scoped on an
output, input-scoped on an input transcode), with `details` =
`{ error_code, pinned_pid, actual_pid, actual_stream_type }`, plus the
matching `tracing::warn!` line. It fires at PMT parse time — no timer —
once per distinct (pinned, actual) pair, and re-arms when the pin
reappears. Until this release it was a log line only, so a mis-typed
pin silently transcoded the wrong track; see
[`events-and-alarms.md`](events-and-alarms.md) ("Transcode engage").

**The in-place transcoder is single-program**, so one output transcodes
exactly one audio PID. To pull two tracks out of an MPTS — say an
English AAC output and a French AAC output — create **one output per
track**, each with its own `program_number` filter *and* its own
`source_audio_pid` pin. The validator refuses multi-program
`pid_overrides` combined with `audio_encode` / `video_encode` on the
same output.

### What the output PMT says

The replacer does not flip the `stream_type` byte in place any more. It
**rebuilds** the program's PMT section — new ES_info loop, new
`section_length`, new CRC — through `engine::ts_pmt_edit`, and applies a
per-target descriptor policy to the re-encoded ES only. Every other ES,
and every other section on the PMT PID, is copied byte-for-byte.

- **Kept** on the re-encoded ES: everything that does not describe the
  codec — ISO 639 language (0x0A), stream_identifier (0x52), a DVB
  private_data_specifier (0x5F) and the private descriptors it governs,
  supplementary_audio and other non-codec 0x7F extensions. Order is
  preserved.
- **Dropped**: maximum_bitrate (0x0E — it describes the source rate), an
  ES-level CA descriptor (0x09 — a re-encoded ES leaves in the clear),
  and every codec-identity descriptor that does not describe the target:
  0x03, 0x6A, 0x7A, 0x7B, 0x7C, 0x7F ext {DTS-HD, DTS Neural, AC-4,
  DTS-UHD}, ATSC 0x81 / 0xCC / 0xAC (unless a private_data_specifier
  governs them), and any registration (0x05) that is not the target's.
  The tag set is the one `ts_parse::descriptor_audio_kind` classifies
  with, so the rewriter and the classifier cannot drift.
- **Per target**: AAC → `stream_type 0x0F`, an existing 0x7C normalised to
  `7C 01 FE` (never added). MP2 → `0x03` (`0x04` at 16 / 22.05 / 24 kHz,
  MPEG-2 LSF), an existing 0x03 rewritten to `03 01 67` (`03 01 27` for
  LSF). AC-3 → always the "AC-3" registration (0x05), and the carriage
  `ts_signalling` selects:

| `ts_signalling` | AC-3 carriage |
|---|---|
| `auto` (default when unset) | follows the **first** source PMT the output sees: DVB carriage if the source is DVB-flavoured, otherwise ATSC. Evidence, in order: an audio ES already carried `0x06` + 0x6A / 0x7A / 0x7C ⇒ DVB; `stream_type` 0x81 / 0x87, ATSC descriptor tags 0x81 / 0x86 / 0xCC / 0xA3, or a "GA94" registration ⇒ ATSC; DVB descriptor tags (0x45, 0x46, 0x52, 0x56, 0x59, 0x5F, 0x66, 0x6A, 0x7A, 0x7B, 0x7C, 0x7F) ⇒ DVB; nothing ⇒ ATSC. A user-private table_id on the PMT PID is deliberately not evidence. |
| `dvb` | `stream_type 0x06` + "AC-3" registration + AC-3_descriptor `6A 01 00` — the carriage ETSI EN 300 468 / TS 101 154 specify for AC-3. **Not yet verified on a professional DVB IRD** (broadcast gate 7 has not been run on it). |
| `atsc` | `stream_type 0x81` + "AC-3" registration (ATSC A/52 Annex A). The optional ATSC AC-3 audio descriptor (tag 0x81) is not generated. |

  The flavour is latched **once per output lifetime** and never follows
  an input switch, so a flow switching between a DVB and an ATSC input
  does not flip its AC-3 signalling 0x81 ↔ 0x06. **Behaviour change:** an
  AC-3 output of a DVB source used to go out as `0x81` with no
  descriptors; under `auto` it is now `0x06` + "AC-3" + 0x6A. Set
  `ts_signalling: "atsc"` to keep the old stream_type. `ts_signalling` is
  refused on any codec but `ac3`, on HLS (always ATSC — what Apple HLS and
  hls.js expect in TS segments) and on PCM inputs (their TS comes from
  the shared muxer).

  The HLS pin covers HLS's **own** `audio_encode` remux only. An HLS
  output without `audio_encode` segments whatever the flow carries, so
  behind an **ingress** AC-3 transcode (`audio_encode` on the input) of a
  DVB source it ships `0x06` + "AC-3" + 0x6A — exactly as it would for a
  DVB source that carries AC-3 that way natively. Whether an HLS player
  accepts that carriage is player-specific (it has not been tested here);
  set `ts_signalling: "atsc"` on the input if a player needs `0x81`. The edge's own consumers of the flow — the shared demuxer
  (RTMP / WebRTC / CMAF / display / SDI / thumbnails / replay export), the
  display audio meter and the `audio_full` content-analysis tier —
  resolve `0x06` + 0x6A to AC-3 (`audio_full` used to ignore `0x06`
  audio altogether, so it metered no DVB-carried AC-3 at all).
- **PES stream_id**: re-encoded AC-3 rides PES `private_stream_1`
  (`0xBD`) in both carriages, as ATSC A/52 Annex A and ETSI TS 101 154
  require and FFmpeg's `mpegtsenc` does; MP2 and AAC keep the MPEG audio
  id `0xC0`. **Behaviour change:** every release before this one wrote
  AC-3 PES with `0xC0`. The same applies to the HLS remux.
- **Growth fallback**: the AC-3 additions are the only edit that grows a
  section. When the grown PMT would need more TS packets than the
  source's, or would exceed the 1021-byte PMT limit, the output falls back
  to `0x81` with no additions (still self-identifying), so a single-packet
  PMT stays single-packet for every single-packet parser downstream. (The
  overflow case used to skip the fallback and emit the source PMT
  untouched — the old stream_type over the re-encoded ES.)
- **Program level**: once any ES of the program is re-encoded,
  multiplex_buffer_utilization (0x0C), maximum_bitrate (0x0E),
  smoothing_buffer (0x10) and STD (0x11) are dropped from program_info —
  an audio 128 → 448 kbps change invalidates them as much as a video
  re-encode does.
- **Version**: the output `version_number` is derived from content. It
  bumps whenever the rebuilt PMT differs from the last one (a source PMT
  update that adds an ES, a codec or PID change) and on every source
  reset, and otherwise holds — an unchanged PMT never flaps. The audio
  and video replacers each track their own input-derived output, so in a
  chain the video stage sees the audio stage's changed section and bumps
  too (it used to re-stamp its own unchanged counter over the audio
  stage's bump). Once a replacer has stamped a PMT it also stamps the ones
  it passes through unedited — an input switch to a source it cannot
  decode (DTS-only audio, VC-1 video) — so the output carries one version
  sequence. A passthrough PMT used to keep its source version, which could
  equal the version the rebuilt PMT had carried, and a receiver caching by
  version then kept the previous input's PMT. Until a replacer's first
  stamp, a passthrough PMT stays byte-identical.
- **Damaged PMTs** (a CRC that does not verify) are never learned from or
  rebuilt; they pass through untouched.

**Multi-section and multi-packet PMTs.** Every packet on the PMT PID goes
through a reassembling stage (`ts_pmt_edit::PsiUnitStage`). A PMT PID may
carry several sections per payload unit, and tables other than the PMT —
ATSC / DigiCipher muxes put a 0xC0 section ahead of the PMT in every
PMT-PID packet (VH1: the PMT sits at packet offset 29). The replacer used
to read only the section at the pointer target, found no PMT, learned no
audio PID and silently passed everything through. The stage also
reassembles a PMT that spans packets: a target ES in the second packet is
found, and an edited multi-packet PMT is re-packetised with a valid CRC —
the old in-place edit changed the stream_type in the first packet and
left the CRC, which lives in the continuation packet, stale, so receivers
discarded the whole PMT. Details: a unit is held until every section that
started in it is complete (PMT packets are delayed by their own span); a
long-form section reassembled across packets must pass its CRC or the
unit is dropped (counted, logged once) — the CRC rather than the CC is the
gate because some muxers never advance the CC on PSI (the shared
`SectionAssembler::push_packet` used by the PTS rewriter, the PSI catalog,
the continuity fixer and HLS takes a same-CC continuation the same way,
CRC-gated); an unchanged unit is re-emitted byte-identical; a changed one
is laid out with a pointer_field and PUSI on every packet in which a
section starts and 0xFF stuffing, reusing each source packet's header
bits and adaptation field. A payload-less packet on the PMT PID (a PCR in
an adaptation-field-only packet) that arrives while a unit is held keeps
its place behind the payload packet it followed, repeating that packet's
CC — it used to overtake the held unit, a CC error on every repetition.
Source CCs are kept while the packet count is unchanged; once it changes
the stage owns the CC on that PID. The program's PMT is matched on its
`program_number` (two programs sharing one PMT PID each get their own);
on a PID the PAT maps to one program only, the first PMT section is
accepted too, as before — the rule the shared demuxer and the display
audio meter apply as well.

### Before the PMT: nothing but PSI

Until the program's PMT has been parsed, both replacers forward only PSI /
SI (PIDs 0x00–0x1F — PAT, NIT, SDT, EIT, TDT — the PMT PID) and null
packets, and drop everything else (`pre_pmt_dropped_packets` on the encode
stats). An output used to open with the source's own video PES (raw
timestamps, no SPS / PPS), source audio and source PCRs, then jump its CC
when the replacer took the PID over; nothing ahead of the PMT is decodable
anyway. The gate re-arms when the PAT moves the program to a new PMT PID.
A PMT that never parses opens it after **5 s** — today's passthrough; the
engage watchdog below says why. When a replacer takes over a PID that
carried passthrough packets (the gate fallback, a program re-layout, a
switch from a codec it cannot decode), its first packet continues that
PID's CC.

### When the transcoder finds nothing to re-encode

Passing the source ES through is the intended fallback when there is
nothing the replacer can decode, but it is no longer silent. Each
replacer runs an engage watchdog on its codec thread (no timers — it is
evaluated on every `process()` call). It starts on the first call and
again on every source reset / input switch, and raises **one** Warning —
`audio_transcode_source_not_found` / `video_transcode_source_not_found`
(category `flow`, scoped like `video_transcode_decode_stalled`) — when,
5 s in, the replacer has still not locked and either the PMT PID has
carried at least 10 PUSI packets, or 10 s and 1000 TS packets have
passed. `details.reason` says why: `no_pat`, `pmt_not_parsed` (with
`first_table_id` — what the PID does carry), `no_supported_es`, or
`codec_not_replaceable` (the program has audio / video, but only in a
codec the replacer cannot decode: DTS, Opus, AC-4, SMPTE 302M, VC-1,
JPEG XS, …). A later lock raises the Info `*_transcode_source_found` —
including the first lock after an input switch, which used to close the
Warning silently; a PMT update that removes the ES re-arms the watch. Catalogue:
[`events-and-alarms.md`](events-and-alarms.md) ("Transcode engage").

### Opus-specific options

All four are **refused**, not ignored, when `codec` is not `opus`: config load / save fails with
`audio_encode.opus_* fields only apply to codec=opus`. That is deliberate — a mis-set knob is
made visible instead of running silently with default Opus behaviour. (The two booleans only
trip the check when set to `true`; `false` is indistinguishable from unset.)

| Field | Default | Notes |
|---|---|---|
| `opus_vbr_mode` | unset | Rate control. Unset uses libopus's own default (full VBR); `"vbr"` is *constrained* VBR; `"cbr"` is constant bitrate. Maps to ffmpeg `-vbr on` / `constrained` / `off` respectively. |
| `opus_fec` | `false` | In-band forward error correction — each packet carries redundancy for the previous frame. Worth turning on over a lossy WebRTC path; it costs bitrate that would otherwise go to the primary frame. |
| `opus_dtx` | `false` | Discontinuous transmission: the encoder skips frames during silence to save bandwidth. **Leave it off for broadcast.** A gapped audio stream costs receivers their A/V sync; DTX is appropriate for conversational audio, not contribution. |
| `opus_frame_duration_ms` | `20` | One of 5, 10, 20, 40, 60. Shorter frames lower latency at the cost of coding efficiency; longer frames are higher quality at low bitrates. |

### `silent_fallback`

When `true`, the edge injects a zero-filled (silent) PCM track into the
encoder whenever the upstream source has no audio PID, or stops
delivering audio mid-stream (500 ms grace window). Guarantees the
output container carries a valid, continuous audio track **from the
first access unit onward**.

On RTMP there is one deliberate exception at the front of that window:
silence does not start until a real essence — audio or video — has
anchored the connection's FLV epoch. Silence must never anchor it, or
the audio timeline pins to 0 while video runs on the source clock. So
an RTMP output that connects *before* any essence arrives opens
video-only rather than emitting silence from t≈0, which is what it used
to do. For the headline case here (a genuinely video-only source) video
anchors the epoch on its first access unit and the silent track resumes
immediately after, so the exposed window is publish-to-first-essence
only.

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

On CMAF the silence is **measured against the picture, not ticked by
the clock**. The 500 ms grace is the trigger; once it has elapsed, each
tick lays down as much silence as it takes to bring the audio up to
where the audio would be — the newest video DTS less the picture's lead
over the audio, measured over the preceding run of real audio (a
hardware encoder sends video ahead of its DTS by its VBV delay) — and no
further. So a delivery stall inserts nothing (neither track moves), a
tick the output loop observed late is made up on the next, and real
audio returning lands where the silence ends. Real and silent frames are
stamped from one encoder timeline: the re-encoder tracks where the
encoder's input is on the source's timeline and re-anchors the encoder
when a real frame is more than a frame past it (a splice, a lost PES, a
gap), holds a frame back that is behind it (silence overshot the return
by the part of the lead the estimate missed), and otherwise continues.
The eager silent-fallback encoder is built at the declared rate and
rebuilt, with a resampler, on the first real frame whose rate or channel
count differs — a 44.1 kHz source through a 48 kHz encoder used to run
8.8 % slow.

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

### Allowed codecs per input

The set is per input class, not one closed union — `codec` is validated
against a different list depending on what the input carries.

| Input | Allowed codecs |
|---|---|
| TS-carrying (SRT / UDP / RTP / RIST / RTMP / RTSP / WebRTC / test pattern / media player / replay) | `aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3` — `opus` is excluded here too, for the same missing-TS-mapping reason. |
| PCM (**ST 2110-30**, `rtp_audio`) | the TS set **plus `s302m`**. At runtime only the AAC family and `s302m` are wired; `mp2` / `ac3` pass validation and then fail bring-up with `pid_bus_audio_encode_codec_not_supported_on_input` (see the input matrix at the top of this document). |
| AES3 (**ST 2110-31**) | `s302m` and nothing else — a 302M wrap preserves the SMPTE 337M sub-frames bit-for-bit; every decode-and-re-encode codec is refused. |

### `s302m` field contract

`s302m` is a lossless PCM wrap, not an encoder, so it takes a different
field set from the compressed codecs and validation checks it apart:

- `bitrate_kbps` is **rejected**, not ignored — "not applicable to
  s302m (302M is a lossless PCM wrap, not a compressed codec)".
- `sample_rate`, if set, must be exactly `48000`.
- `channels`, if set, must be `2`, `4`, `6` or `8` (SMPTE 302M-2007).

### Rejected combinations (validation bails at load time)

- `audio_encode` + `transport_mode: "audio_302m"` — the 302M path owns the TS stream already.
- `audio_encode` + SMPTE 2022-7 redundancy (RTP / SRT).
- `audio_encode` + SMPTE 2022-1 FEC encode (RTP).
- `audio_encode` + SRT FEC (`packet_filter`).

### Audio timing in the TS audio replacer

On the TS outputs (SRT / UDP / RTP / RIST) and on TS-carrying inputs the
`TsAudioReplacer` keeps the re-encoded audio on its source's timeline, to
the sample:

- **Per access unit, across PES boundaries.** The audio PID's payload is
  reassembled into one continuous elementary stream and cut into access
  units (ADTS / LOAS / MPEG audio / AC-3 / E-AC-3 headers) as soon as each
  is complete; a PES's PTS belongs to the first AU that *commences* in it
  (ISO/IEC 13818-1 §2.4.3.7). An AU that straddles two PES packets — legal
  whenever `data_alignment_indicator` is 0, and Sky Sports Arena does it
  three times a loop — decodes like any other. It used to be lost together
  with the whole next PES: 149 ms of audio replaced by 128 ms of silence,
  moving the audio 21.3 ms early for good at every event. A header is
  trusted where the previous AU ended; anywhere else (the first AU, after a
  resync, after a TS continuity-counter break) only once its successor is a
  consistent header or it ends on a PES boundary, so a sync pattern inside a
  payload is never decoded. A candidate a scan finds (after a break) whose
  stream parameters differ from the last AU's, and which no PES begins
  with, is dropped at once rather than waited out for the length it claims
  (up to 8 KB of ADTS: half a second of audio that would then leave in a
  burst, late against the PCR, and raise the PCR stage's delay for good);
  every PES start inside a candidate is looked at. An AU into which a new
  PES begins with a header of its own was cut short upstream (a file's
  truncated last PES at a loop wrap) and is dropped, never glued to the next
  PES — also when the next PES is a stream of other parameters (a playlist
  moving to a 5.1 or 44.1 kHz file, a splice), once that header's own
  successor agrees. AC-3 and E-AC-3 frames are one stream: an AC-3 core
  followed by E-AC-3 dependent substreams (Annex E's backward-compatible
  7.1) is one stream — every core used to be discarded as a false sync,
  leaving no audio at all. **A dependent substream frame is cut into the AU
  of the independent frame before it**, one AU per time slot: libavcodec
  merges a dependent frame only when it follows its independent frame in
  the same packet and silently ignores one sent alone, and every decode
  path sends one AU per packet — a 7.1 E-AC-3 programme decoded as its 5.1
  core everywhere, with no error counted. The cutter holds an independent
  frame until the bytes after it show no dependent frame follows (the next
  header, or the end of its PES — so a slot that ends its PES goes out at
  once); a dependent frame with no independent frame before it (a join)
  goes out alone. The splitter behind the demuxer's `OtherAudio` consumers
  (`audio_decode::split_audio_codec_frames`) keeps a dependent frame with
  its independent frame the same way. A duplicate TS packet (same CC, same
  payload) is dropped.
  **The same cutter frames the audio on every other path that decodes
  it** (2026-09): the shared demuxer (`ts_demux::TsDemuxer`, behind the
  display, SDI, CMAF, RTMP, WebRTC, ST 2110-30, `rtp_audio` outputs and
  replay export), the HLS in-process remux, and the meters (content
  analysis `audio_full`, the display's level bars —
  `audio_decode::PidAudioDecoder`). Each parsed every PES on its own: the
  ADTS walk stopped at the first byte of a PES that was not a sync word,
  so a straddling AU **and every AU of the next PES** were lost; the MP2 /
  AC-3 / E-AC-3 / LATM splitters dropped the two halves of a straddling
  AU; content analysis never decoded AAC-LATM at all (it looked for ADTS
  syncs in it). The demuxer now emits an `Aac` frame per AU as soon as it
  is complete, timed from its PES's PTS or from the samples before it,
  and an `OtherAudio` per PES holding the whole AUs that commenced in it
  (one straddling into the next PES included) under that PES's PTS —
  emitted at the next PUSI when nothing is pending, as before, or once
  the straddling AU is complete.
- **Latency.** Audio leaves the replacer about one AU after its last byte
  arrives instead of one PES — which on sources packing seven AUs per PES
  was up to 150 ms, and made an audio-only transcode's first AU of every PES
  late against a passthrough PCR. The replacer holds the last 2 ms of
  decoded audio back so a correction (below) can crossfade into it, which
  can delay an output frame by one more source AU.
- **Stamps cancel the codec pipeline's latency.** Output PTS come from a
  sample-count model anchored on the source PTS (one rounding per frame, so
  44.1 kHz never drifts), **minus the latency the libraries declare for this
  pipeline**, latched when it opens:
  - the source decoder's: none — fdk-aac is opened with noise-substitution
    concealment and the PCM limiter off (`bilbycast-fdk-aac-rs`), and the
    libavcodec decoders add none;
  - the resampler's, when the rate changes (rubato's `output_delay`, half
    its sinc length scaled to the output rate: 117 samples for 48 → 44.1 kHz
    at `src_quality: high`);
  - the encoder's priming: fdk-aac `nDelay` (AAC-LC 2048 samples = 42.7 ms
    at 48 kHz; HE-AAC's includes the decoder's SBR delay) or libavcodec
    `initial_padding` (MP2 481, AC-3 256).

  A receiver therefore presents each sample at its source PTS. Before, an
  AAC source re-encoded to AAC-LC / MP2 / AC-3 presented its audio **79.0 /
  46.4 / 41.6 ms late** (measured on Sky): fdk-aac's default decoder delay
  of 1744 samples (one frame of energy-interpolation concealment plus the
  limiter's 15 ms lookahead) plus the encoder priming. The Info log line
  `ts_audio_replace: re-encoded audio stamped earlier by the codec
  pipeline's declared latency` gives the latched figure. An HE-AAC
  *source's* SBR delay is left in: the source's own timestamps already
  assume a reference decoder that has it.
- **Held to the source PTS, without a clock.** At the first AU of every PES
  the replacer compares the PES PTS with where the decoded content ends
  (samples decoded, plus silence inserted, minus samples dropped, since the
  anchor):
  - within ±5 ms: nothing (PES timestamp jitter);
  - a **gap** of 100 ms or more, or one over 5 ms that has kept its sign for
    150 ms (and at least two PES) — time in which nothing was decoded: lost
    or undecodable AUs, an off-air stretch, a splice: exactly that much
    silence, inserted at the source rate ahead of the PES's audio and
    passed through the rate / channel stage; on a `media_player` input the
    output PTS step over the gap instead, because its file splices step PTS
    over audio that is continuous — at the PES that shows the step, since a
    file's timestamps carry no jitter to wait out (waiting 150 ms presented
    that much audio early by the step at every loop);
  - an **overlap**, by the same rules: that much audio dropped from the
    head of what follows (the count of placed content may go below the
    anchor: a PES stamped 150 ms behind one AU after an anchor drops
    exactly 150 ms);
  - a step over 500 ms, **either way**: re-anchor at the new PTS. A source
    that resets its clock back (an encoder restart, an ffmpeg
    `-stream_loop` wrap) takes its PCR and its video back with it, and the
    transcode chain's PCR stage drops re-encoded PES left on the old
    timeline after a step back of over 1 s as frames of the previous epoch
    — before 2026-09 the audio kept its old, monotonic PTS there and was
    dropped for as long as the reset was deep (for good on a looping
    source).

  Insertions and drops crossfade over 2 ms. An AU that fails to decode is
  replaced by silence of its nominal length where it stood. Nothing in it
  reads the host's clock (the one clock it reads is the program's own PCR,
  for the gap fill below), so host load, encoder warm-up and wire
  backpressure can no longer move the audio: the **master-clock catch-up**
  that did — it
  compared the master clock at the moment the codec thread reached a PES
  with the samples emitted, inserted 32 ms of silence per firing (7 times
  in 200 s at load average 40 on an AC-3 output, a gate-1 failure), and
  counted a real gap twice — is removed, and so is the per-flow pacer
  plumbing into the transcode chains. So is the **PCR-jump signal** from
  the input's clock rewriter (and the media player's per-loop splice-gap
  signal on the same channel): the replacer follows its audio's own PTS
  across any step, as passthrough does, and the signal either re-anchored
  where nothing had moved (discarding a pending drop) or, from the media
  player, put the re-encoded audio later by the loop's video/audio end gap
  at every loop. A source whose audio clock is not
  locked to its PCR is held within ~5 ms by one short insert or drop about
  every 100 s at 50 ppm; `timeline_corrections`, `silence_inserted_samples`
  and `dropped_samples` on the output's `audio_encode_stats` count them.
- **A gap is filled as the program's clock passes it, not when the audio
  returns.** Found only at the next PES's PTS, a gap's silence used to be
  encoded in one burst when the audio came back, stamped for when the gap
  began: frames up to the whole gap behind the program's PCR. At a
  `media_player` loop of an MPTS played whole the program's audio pauses as
  long as the most demanding program needs (Spain program 186: 244 ms,
  770_H program 4030: 464 ms); the burst arrived up to 203 ms late, and the
  transcode chain's PCR stage (`ts_pcr_remux`) raised its delay by 163–290 ms
  with a DI at every loop, which pushed the video's T-STD residency past 1 s.
  Now the replacer reads the program's clock — its PMT's PCR_PID, each PCR
  interpolated by packet count up to the next, never past one PCR interval —
  and after every packet compares it with where the decoded content ends
  (the *headroom*). Over the last 10 s of that clock it keeps the lowest
  headroom the source left and the longest stretch its content end stood
  still. When the content end has stood still 10 ms longer than that
  stretch and the headroom has fallen 10 ms below that floor, the source is
  missing audio it always had by now: silence is placed, as the clock goes
  on, to keep the headroom at the floor, until the audio returns, which then
  settles what is left at once (a gap filled, an overlap dropped — the
  150 ms persistence rule is for timestamp jitter, not this). Each silent
  frame so leaves where, and against the same PCR, a source frame would have
  at the source's worst; nothing is placed ahead of audio a source keeping
  its own lead still has to send. The first 2 s after an anchor are watched,
  not acted on; a PCR that steps back, jumps over 1 s or carries DI starts
  the learning over; past 500 ms of such silence the audio has stopped
  rather than paused, and its return re-anchors. Silence placed this way is
  counted in `silence_inserted_samples`, each run once in
  `timeline_corrections`; the Info log line `ts_audio_replace: source audio
  back after a gap filled on the program clock` gives each run's length and
  remainder. It applies to the gap-fill-by-silence mode only (TS outputs, and
  input-side transcodes of live inputs); a `media_player` input's own
  transcode steps its PTS over the gap instead.
- **`audio_encode.sample_rate` / `channels` convert even without a
  `transcode` block.** The encoder is opened at those values; the decoded
  PCM used to reach it unconverted, so a 48 kHz source through
  `sample_rate: 44100` played 8.8 % slow, and `channels: 2` on a 5.1 source
  kept L and R and lost the centre. The replacer now builds the conversion
  itself: rubato SRC, and for channel counts the standard downmix (ITU-R
  BS.775 for 3.0 / 5.0 / 5.1 / 7.1 → stereo and for any multichannel
  layout → mono, Lt/Rt for quad → stereo, the transcode stage's defaults
  for mono ↔ stereo; any other pair keeps the channels in order, silence
  for the missing ones — see the resolution rule below).
- **A format change in-band is converted to the output's.** The output
  format — the configured `sample_rate` / `channels`, else the source's at
  the first decoded frame — is fixed for the stream: the encoder stays
  open and the PMT unchanged. When the source changes channel count or rate
  mid-stream (AC-3 and AAC decoders follow acmod / channel_config frame by
  frame; broadcast services switch 5.1 ↔ 2.0 between programme and ads) the
  channel / rate stage is rebuilt to convert the new format to it — the
  `transcode` block pinned to the output format, or, when its routing is
  for another layout (a 5.1 preset over a stereo stretch), the default
  conversion above. What the old stage holds (the crossfade tail, its
  resampler's queue and delay line) goes out first, up to exactly the
  content it was given, and the new resampler's zero history is dropped, so
  no sample moves and the stamps keep their latency. Before 2026-09 a
  downmix built for 5.1 refused the stereo frames that followed (the output
  went silent until the source returned to 5.1); without a conversion,
  frames of a new channel count bypassed the timeline's drops and a new
  rate reached the encoder unconverted.
- **`av_skew`** on the output (or, for an input transcode, the flow) reports
  what remains: where each PES's first sample will be presented minus its
  source PTS — the declared latency the stamps do not cancel plus any
  correction still pending. See [metrics.md](metrics.md#edge-added-av-skew-flowstatsav_skew-outputstatsav_skew).
- **Stats.** `audio_encode_stats.target_sample_rate_hz` / `target_channels`
  carry the format the encoder was opened at — registered as 0 when
  `audio_encode` leaves them to follow the source, and published once the
  first frame resolves them — and `audio_decode_stats.output_sample_rate_hz`
  / `output_channels` the decoded source format (they used to carry the
  output's).

**Release note — every fdk-aac decode is 36.3 ms earlier.** The decoder
change above is in `bilbycast-fdk-aac-rs` and applies to every AAC decode in
the edge: this replacer, the display output, SDI, CMAF, RTMP, WebRTC,
ST 2110-30, HLS and content analysis all get AAC audio 1744 samples
(36.3 ms at 48 kHz, 1685 at 44.1 kHz) earlier than before, i.e. on time.
Operator-calibrated `audio_offset_ms` trims on SDI outputs shift by that
much. With the limiter off, a sample reconstructed past full scale
hard-clips instead of being soft-limited, and a stream carrying MPEG-4
`prog_ref_level` other than -24 dB now decodes at its encoded level instead
of being re-levelled, so `audio_full` loudness readings change for such
streams.

### AC-3 / E-AC-3 sources: no dynamic-range compression, consistent dither, dialnorm carried

Every libavcodec audio decode in the edge — the TS replacer, RTMP,
WebRTC, HLS, CMAF, SDI, ST 2110-30, the display (and its level bars) and
content analysis — opens AC-3 and E-AC-3 through
`audio_decode::open_ff_decoder` with two private options
(`audio_decode::ff_decoder_options`):

- **`drc_scale` 0 — no dynamic-range compression.** libavcodec applies the
  bitstream's line-mode `dynrng` gains by default (`drc_scale` 1). The
  re-encoded AC-3 carries no `dynrng`, so the compression was baked into
  the programme and the receiver — which chooses line, RF or no
  compression from the metadata — could no longer choose. A source
  carrying it (ESPN, 448 kbps 5.1) decodes 24.7 dB SNR apart with and
  without it; on the rig the edge's AC-3 re-encode of it matched the
  compressed programme (27.9 dB SNR against a `drc_scale` 1 reference,
  18.5 dB against the uncompressed one) and now matches the uncompressed
  one (28.2 dB against `drc_scale` 0, 18.1 dB against 1 — 28.2 dB is what
  FFmpeg's own AC-3 encoder reaches re-encoding that reference at 448 kbps
  offline: 28.5 dB). Sources without `dynrng` (VH1, Nine) decode
  bit-identically either way. Loudness measurement (BS.1770 in content
  analysis) and baseband playout (SDI, ST 2110-30, the display) want the
  uncompressed programme too. A source's heavy-compression word
  (`compr`, RF mode) was never applied (libavcodec's `heavy_compr` is off)
  and is not carried.
- **`cons_noisegen` 1 — the dither that fills zero-bit mantissas is seeded
  from each frame.** Run on across frames, as libavcodec does by default,
  it made two decodes of the same frame differ unless both started at the
  same frame: two decodes of the 192 kbps VH1 source differ at 33.5 dB
  SNR. Seeded per frame, a frame always decodes to the same PCM, so a
  re-encode is reproducible whatever frame the edge joined at, and a
  reference decoded with `-cons_noisegen 1` measures only the re-encode.
  **Measurement note:** a gate-6 reference decoded with ffmpeg's defaults
  (run-on dither) now reads 33.5 dB against a VH1 AC-3 → AC-3 output; the
  39.3 dB phase-2 figure came from the edge and the reference both
  starting at the file's first frame, so their run-on dither happened to
  coincide. Decode AC-3 / E-AC-3 references with
  `-drc_scale 0 -cons_noisegen 1`.

**`dialnorm` is carried into an AC-3 re-encode.** The dialogue level a
receiver normalises to (line mode: to -31 dBFS) was libavcodec's default
-31 on every AC-3 the edge wrote, so a -24 dB programme's transcode played
**7 dB louder than its source** on every receiver that honours it (VH1,
ESPN and Nine all carry -24; the broadcast SPTS sample -23). The TS
replacer reads it from each AC-3 / E-AC-3 AU of the source
(`audio_decode::ac3_dialnorm`; an E-AC-3 independent substream 0) and
opens the AC-3 encoder with it and `per_frame_metadata`, and a change
(programme vs ads) follows through `AudioEncoder::set_option` from the
encoder's next frame — within one frame plus the pipeline's latency of
where the source changed. HLS writes the segment's first value. An MP2 /
AAC source has none, and the default -31 stays. Not carried: `bsmod`,
`dsurmod`, the mix levels, `dynrng` / `compr` (libavcodec's AC-3 encoder
writes no DRC words); an AC-3 source re-encoded to AAC or MP2 loses its
dialnorm, so such a transcode plays louder than its source by
(-31 - dialnorm) dB on a receiver that normalised the source.

**What the remaining VH1 gap is.** Phase 2 measured an AC-3 → AC-3
transcode of VH1 at 39.3 dB against the 59 dB of MP2 / AAC → AC-3 cells.
It is not DRC (VH1 carries none) and not the edge: against a
`-drc_scale 0 -cons_noisegen 1` reference the edge's 448 kbps output
reads 39.1 dB, and FFmpeg's AC-3 encoder re-encoding that same reference
offline reads 39.25 dB (45.7 dB at 640 kbps). A 192 kbps AC-3 source is
full of dither noise in its zero-bit bands, which a re-encode has to spend
bits on; band-limited MP2 / AAC sources are not.

### Engine internals

- Core stage: `src/engine/ts_audio_replace.rs` (streaming `TsAudioReplacer`);
  access-unit framing and the continuous-ES cutter: `src/engine/audio_au.rs`.
- PMT: `src/engine/ts_pmt_edit.rs` (`PsiUnitStage`, `rebuild_pmt_section`,
  `OutVersion`, `detect_flavour`); the engage watchdog is
  `src/engine/transcode_engage.rs`.
- Wiring: `output_udp.rs`, `output_rtp.rs`, `output_srt.rs` insert a
  `block_in_place` call between the program filter and the egress buffer.
- Decoder: `bilbycast-fdk-aac-rs::AacDecoder::open_adts` (Fraunhofer FDK
  AAC, opened with no added delay) for ADTS; libavcodec for LATM / MP2 /
  AC-3 / E-AC-3.
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

One rule, on every output and input that re-encodes audio (the TS outputs
and TS-carrying inputs through the audio replacer, RTMP, HLS, WebRTC,
CMAF): the stage between the decoder and the encoder is
`audio_transcode::encoder_stage`.

- **A `transcode` block wins.** `audio_encode.sample_rate` / `channels`
  fill the fields it leaves unset.
  - `audio_encode.channels = 2` + `transcode.channels = 1` → the stage
    mixes to mono (transcode wins).
  - `audio_encode.sample_rate = 44100` + `transcode.sample_rate` unset →
    the stage resamples to 44.1 kHz and the encoder takes 44.1 kHz PCM.
- **No block: `audio_encode.sample_rate` / `channels` alone convert** when
  they differ from the decoded source — rubato SRC for the rate; for the
  channel count the standard downmix, for the channel order every decoder
  here hands out (L R C LFE Ls Rs, then the back pair):
  - **→ stereo**: ITU-R BS.775 for 5.1 / 7.1 (and 3.0 / 5.0: the centre at
    −3 dB into both sides, each surround at −3 dB into its own), Lt/Rt for
    quad, the transcode stage's default for mono;
  - **→ mono**: BS.775's mono downmix — L and R at −3 dB, C at unity, each
    surround at −6 dB, the LFE left out (3.0, quad, 5.0, 5.1, 7.1); a
    layout it cannot name is averaged over every channel; stereo is the
    stage's `stereo_to_mono_3db`. Until 2026-09 every N → 1 took channel 0
    alone: a 5.1 programme's mono output was its front-left channel, the
    centre (the dialogue) dropped — on CMAF a regression from its own
    average-to-mono, since CMAF shares this rule; 3.0 / 5.0 → stereo
    dropped the centre the same way;
  - otherwise the channels in order with silence for any missing.

  They used to be bare encoder parameters: the TS replacer and
  HLS opened the encoder at the new rate and fed
  it the source's PCM, so a 48 kHz source through `sample_rate: 44100`
  played 8.13 % slow and through `32000` 33.3 % slow — the gate-2
  arrival slope measured exactly that — while RTMP refused a channel
  count other than the source's (a Critical `audio_encode` event, no
  audio) and WebRTC dropped every frame (`planar channel count !=
  configured`). CMAF took L / R of a 5.1 source and lost the centre, and
  ignored a `transcode` block it accepted.
- **Neither set → the encoder follows the source.**
- **Once the encoder is open its format is fixed**; a source that changes
  rate or channel count in-band is converted to it (a block whose routing
  does not fit the new layout gives way to the default conversion).
- **The source's format is what its decoder hands out.** RTMP and WebRTC
  resolve the stage and open the encoder from the first AAC frame decoded,
  not from the ADTS header: an HE-AAC header gives the core's rate (24 kHz
  for a 48 kHz service, SBR doubles it) and HE-AAC v2's mono core (PS
  widens it to stereo). Set up from the header, a DVB-T2 HE-AAC 48 kHz
  stereo service re-encoded to AAC-LC went out at 24 kHz — a 12 kHz audio
  bandwidth — and, for v2, in mono. (With no overrides and an `aac_lc`
  target the frames pass untouched, as before.)
- **A silent-fallback encoder takes the block's format first.** Built
  before any source audio so the silence has somewhere to go, it is sized
  from `transcode.channels` / `sample_rate`, then `audio_encode`'s, then
  48 kHz stereo — the order every other build of the stage uses (CMAF did
  since 2026-09; RTMP and WebRTC now too). Sized from `audio_encode` alone,
  a block's `channels: 1` with its own routing met an encoder opened in
  stereo, and the stage fell back to the default conversion: stereo out,
  the routing dropped. (Opus stays at 48 kHz on the wire.)

The stage always runs in **streaming mode** (fixed 256-frame resampler
chunks, `STREAM_CHUNK_FRAMES`), so its delay is a constant, and each
output takes it off its stamps:

- TS outputs / TS inputs: latched with the codec pipeline's latency (see
  "Audio timing in the TS audio replacer").
- RTMP, WebRTC: `AudioEncoder::set_upstream_delay` — the encoder's anchor
  (first submit, or first after a re-anchor) subtracts it with the codec's
  priming. Every submit goes through `AudioEncoder::submit_through`, which
  declares the stage's delay as it stands first: a stage built after the
  encoder (a silent fallback's, on the first real frame) or rebuilt for
  another source format gains its resampler late, and declared once at
  build time (as 0) its delay stayed on every stamp.
- HLS: each segment is re-encoded on its own, through a
  `BatchStage` that drops the resampler's zero history at the head and runs
  its queue and delay line out at the end, so the segment's audio lines up
  with its source sample for sample and needs no correction. A segment
  whose source changes format in-band is converted stretch by stretch — a
  `BatchStage` per run of frames in one decoded format, each pinned to the
  segment's output format — and the rendition keeps the format its first
  segment resolved (`ResolvedAudioEncode::out_format`): a playlist cannot
  signal a channel count or rate changing between segments. The one stage
  built for a segment's first frame used to refuse every frame of another
  layout (a 5.1 programme into a stereo break with `channels: 2` lost the
  rest of the segment, up to 6 s of silence at every such switch) or, with
  no override, feed 44.1 kHz PCM to an encoder opened at 48 kHz (the
  wrong speed until the segment ended).
- CMAF: the stage routes channels only, at the source's rate; the rate
  goes to the encoder's own resampler (next bullet).

`AudioEncoder`'s own resampler (the rate conversion RTMP / WebRTC / CMAF
reach when the encoder's input rate differs from its target, and Opus's
44.1 → 48 kHz) now has its delay taken off the anchor too — rubato's
figure for the sinc-64 filter, 32 output frames and more (0.7 ms at
44.1 → 48 kHz), which presented the audio that much late.

Exceptions that do not go through the stage: the PCM inputs (ST 2110-30,
`rtp_audio`, MXL audio) refuse an AAC `audio_encode.sample_rate` /
`channels` that differs from the input at bring-up
(`AacSampleRateMismatch` / `AacChannelCountMismatch`), loudly; the SDI
input opens its encoder at the capture's format and does not apply them
(open item — the `sdi-decklink` build could not be compiled here).

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
- Built by `audio_transcode::encoder_stage` (the resolution rule above)
  in **streaming mode** (`with_fixed_chunk`, `STREAM_CHUNK_FRAMES` = 256):
  the resampler runs on fixed chunks from a queue, so it is built once and
  its delay is a constant the stamps cancel. The default mode — which
  RTMP, HLS and WebRTC used until 2026-09 — builds the resampler for the
  first call's length and rebuilds it (restarting its delay line, a
  dropout, and moving the audio by the delay) whenever the decoder's frame
  size changes: AC-3 1536 next to E-AC-3 256-sample blocks, an MP2 1152,
  an AAC-LC / HE-AAC switch.
- Wiring:
  - TS outputs (SRT / UDP / RTP / RIST) and TS inputs: inside
    `ts_audio_replace::TsAudioReplacer` between decoder and encoder.
  - RTMP: `EncoderState::Active.stage` (`audio_transcode::EncoderStage`,
    which follows an in-band format change to the output format; the
    samples queued in the old resampler, at most a chunk plus its delay
    line, are lost at such a change), for AAC and for MP2 / AC-3 /
    E-AC-3 sources alike; disables the same-codec fast path because PCM
    must be decoded to apply the shuffle.
  - HLS: `audio_transcode::BatchStage`s, fresh per segment — one per run of
    frames in one decoded format — inside `remux_ts_audio_inprocess_pinned`,
    converting to the rendition's format.
  - WebRTC: `WebrtcEncoderState::Active.stage`, on both the WHEP viewer
    loop and the WHIP client loop.
  - CMAF: `cmaf::encode::AudioReencoder`, channel routing only (the rate
    in the encoder's resampler).

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
CMAF-LL (the operator's `gop_size` is honoured), and ST 2110-20 / -23
(mandatory on those inputs). **HLS is the only remaining output
without `video_encode`** (deferred — see below).

Decodes the source video ES (H.264 or HEVC) in-process via
`video-engine::VideoDecoder`, re-encodes via a feature-gated backend,
and muxes the new bitstream back into the output TS. The program's PMT
is rebuilt through the same `ts_pmt_edit` stage as the audio replacer
(see "What the output PMT says" above): the target `stream_type`,
`PCR_PID` forced to the video PID (the input's PCRs are carried onto it —
see [Output PCR](#output-pcr--the-remux-model) below), and the
video descriptor policy on the re-encoded ES — the source codec's video
descriptors (MPEG-2 0x02, MPEG-4 0x1B, AVC 0x28 / 0x2A, HEVC 0x38),
maximum_bitrate (0x0E), an ES-level CA descriptor (0x09) and a
registration that is not the target's ("HEVC" on an HEVC → H.264
transcode) are dropped; 0x52, 0x0A, 0x06 and the rest are kept — plus
the program-level rate descriptors, and the content-tracked version.

### Schema

```jsonc
"video_encode": {
  "codec":       "x264" | "x265" | "h264_nvenc" | "hevc_nvenc" | "h264_qsv" | "hevc_qsv"
  //             | "h264_vaapi" | "hevc_vaapi" | "h264_rkmpp" | "hevc_rkmpp"
  //             | "h264_auto" | "hevc_auto" | "auto",
  "source_video_pid": 4113,  // optional; pin the source video ES PID
  "hw_decode":   "auto",     // optional; "auto" | "cpu" | "nvdec" | "qsv"
                             //           | "vaapi" | "rkmpp"
  "width":       1920,       // optional — see "Limitations"
  "height":      1080,       // optional — see "Limitations"
  "scan":        "auto",     // optional; "auto" (default) | "progressive"
                             // | "interlaced" — see "Scan" below
  "fps_num":     30,         // set to MATCH THE SOURCE; may be omitted on TS
                             // outputs (auto-detected). Not a resampler.
  "fps_den":     1,
  "bitrate_kbps": 4000,      // optional, default 4000; range 100–100000
  "gop_size":    60,         // optional, default 2 × fps_num (CMAF: tiles
                             // the segment — see "Frame rate" below)
  "preset":      "medium",   // optional, default medium; `ultrafast`..`veryslow`
  "profile":     "high",     // optional, auto if unset; `baseline` / `main` / `high`

  "chroma":      "yuv420p",  // optional; see the per-vendor chroma matrix below
  "bit_depth":   8,          // optional; 8 (default) or 10 — same matrix

  "rate_control": "vbr",     // optional; "vbr" (default) | "cbr" | "crf" | "abr"
  "crf":          23,        // optional; only for rate_control "crf"
  "max_bitrate_kbps": 6000,  // optional; VBV ceiling — vbr / abr only
  "bframes":      0,         // optional; default 0
  "refs":         3,         // optional; encoder default when unset
  "level":        "4.0",     // optional; encoder picks when unset
  "tune":         "zerolatency",  // optional; x264 / x265 vocabulary —
                             // NVENC wants hq / ll / ull / lossless

  "color_primaries": "bt709",
  "color_transfer":  "bt709",
  "color_matrix":    "bt709",
  "color_range":     "tv"
}
```

| Field | Default | Notes |
|---|---|---|
| `rate_control` | `vbr` | One of `vbr`, `cbr`, `crf`, `abr`. In `crf` mode `bitrate_kbps` is ignored and `crf` drives quantisation instead. |
| `crf` | unset | 0–51, lower is better quality; broadcast typical 18–28. Only meaningful with `rate_control: "crf"`. Translated to `cq` on NVENC. |
| `max_bitrate_kbps` | unset | VBV ceiling in `vbr` / `abr` only: sets `rc_max_rate` plus a 2 s VBV buffer. **Ignored in `cbr`** — that mode pins `bit_rate` = `rc_min_rate` = `rc_max_rate` to `bitrate_kbps`, which is the ceiling — and ignored in `crf`. Validation still enforces 100–100 000 and `max_bitrate_kbps >= bitrate_kbps` in every mode. |
| `bframes` | `0` | Consecutive B-frames, 0–16. **Not available on RTMP or CMAF outputs** — see *No B-frames on RTMP or CMAF* under "Known limitations"; the encoder is opened with 0 regardless and logs `rtmp_bframes_unsupported` / `cmaf_bframes_unsupported`. |
| `refs` | unset | Reference frames, 1–16. The encoder's own default when unset. |
| `level` | unset | Codec level, e.g. `"3.0"`, `"4.0"`, `"5.1"`. Unset lets the encoder pick from resolution / bitrate / frame rate. |
| `tune` | backend-resolved: `zerolatency` on x264 / x265, **unset on every hardware backend** | The vocabularies are disjoint. x264 / x265 accept `zerolatency`, `film`, `animation`, `grain`, `stillimage`, `fastdecode`, `psnr`, `ssim`; NVENC accepts `hq`, `ll`, `ull`, `lossless`; QSV and VAAPI expose no `tune` option at all. Config validation is permissive over the union, because an `h264_auto` / `hevc_auto` output does not know its backend until flow start. A tune the resolved backend cannot accept is therefore **dropped** at flow start (`video_encode_util::sanitise_tune`), with a log line carrying `error_code = encoder_tune_not_supported` — a log line only, no manager event. Dropping matters: handing NVENC `zerolatency` makes `avcodec_open2` fail with `EINVAL (-22)`. An empty string means "unset — encoder chooses". |
| `chroma` / `bit_depth` | `yuv420p` / `8` | Which backend can carry which combination is genuinely per-vendor; the matrix is later in this document rather than duplicated here. |
| `source_video_pid` | unset | Pin the source video elementary PID instead of taking the first video stream in the active program's PMT (`stream_type` `0x01` / `0x02` / `0x1B` / `0x24`). Range `0x0010..=0x1FFE`, enforced at config load. Behaves exactly like `audio_encode.source_audio_pid` above, including the single-program rule and the fallback, which raises the Warning event `video_source_pid_not_found` — see that section for the MPTS recipe. |
| `scan` | `auto` | Progressive or interlaced (field) coding of the output. `auto` field-codes an interlaced source on MPEG-TS re-encodes when the output is unscaled vertically and the resolved backend can; `progressive` is the pre-2026-09 behaviour; `interlaced` forces field coding (H.264 only). Capability `video-encode-scan`. See [Scan](#scan-interlaced-sources) below. |
| `hw_decode` | `auto` | Which backend decodes the **source** ES before re-encode. `auto` walks VAAPI ≻ NVDEC ≻ QSV ≻ RKMPP ≻ CPU against the host's probed capabilities and the compiled-in `video-decoder-*` features; `cpu` forces software libavcodec, which is how you keep the host's HW decode sessions free for other flows. A forced backend the build or host cannot satisfy **does not fail the flow** — it logs `ts_video_replace: hw_decode preference … unavailable …; falling back to CPU` (a bare `warn`, no `error_code`, no manager event) and runs on CPU, and so does an edge whose startup probe never ran. That is deliberately unlike the display output, which raises `display_hw_decode_unavailable_falling_back`. Verification recipe: [`codec-matrix.md`](codec-matrix.md#verification-commands-per-host-class), item 4. |

#### Colour metadata

Four fields carry the signalling a re-encode would otherwise lose. **A
re-encode does not infer them from the source** — leave them unset on an
HDR or BT.2020 contribution feed and the output carries no colour
signalling at all, which a downstream display reads as BT.709 SDR even
though the pixels are not, and has no way to recover from.

| Field | Default | Accepted values |
|---|---|---|
| `color_primaries` | unset | `bt709`, `bt2020`, `smpte170m`, `smpte240m`, `bt470m`, `bt470bg` |
| `color_transfer` | unset | `bt709`, `smpte170m`, `smpte2084` (alias `pq`), `arib-std-b67` (alias `hlg`), `bt2020-10`, `bt2020-12` |
| `color_matrix` | unset | `bt709`, `bt2020nc`, `bt2020c`, `smpte170m`, `smpte240m` |
| `color_range` | unset | `tv` (aliases `limited`, `mpeg`) or `pc` (aliases `full`, `jpeg`). Unset really is unset — nothing is signalled and the encoder's own default stands. |

### Scan: interlaced sources

`video_encode.scan` (unset = `auto`) decides whether the re-encoded picture is
coded progressive or as fields. Every release before this field coded
progressive frames holding both fields woven together — `frame_mbs_only_flag`
1, no `pic_struct`, the source's field order lost — and a vertical resize
scaled the woven frame, blending its two fields into each other.

It is settled once, when the encoder lazy-opens, from the frame in hand:

| `scan` | Coded interlaced when | Otherwise |
|---|---|---|
| `auto` (default) | a TS **output**'s re-encode (SRT / UDP / RTP / RIST — not the TS ingress transcoder, see below) **and** the frame is a woven interlaced frame (an interlaced H.264 or MPEG-2 decode) **and** the output is not scaled vertically (`height` unset or equal to the source's) **and** the backend the resolver lands on can code fields on this host | progressive |
| `progressive` | never | progressive — the old bitstream, byte for byte |
| `interlaced` | always, on the first backend in the chain that can code fields — from a progressive source too (both fields from one instant, top first) | progressive with Warning `video_encode_interlace_unavailable` when no backend in the chain opens for fields, or the source's decoder hands out one field per picture |

Field coding is **H.264 MBAFF** with `pic_struct` in the picture-timing SEI,
in the source's field order (TFF / BFF, followed frame by frame across an
input switch), on the backends FFmpeg gives an interlaced tool: **libx264**
always, **h264_nvenc** and **h264_qsv** where the GPU allows it (the open is
refused otherwise — an Intel Arrow Lake iGPU, for one, refuses QSV field encode). No HEVC
encoder codes field pictures, and `h264_vaapi` / `h264_rkmpp` ignore the
request, so validation refuses `interlaced` with those codecs. `auto` follows
the backend the resolver lands on: on an Intel host whose `h264_auto` chain
starts with a QSV that refuses fields, `auto` codes progressive on QSV rather
than demoting to libx264 to get them; `interlaced` walks the chain for one
that can. MBAFF costs libx264 roughly 20-30 % more CPU.

With `interlaced`, a resize is done **per field**: each field is scaled on
its own and the two are woven back, so a 1080i → 576i conversion keeps its
fields apart; a 4:2:0 `height` must then be divisible by 4 (two fields of
whole chroma rows — validated). `auto` does not field-code a scaled output,
and a progressive resize of an interlaced source still scales the woven
frame (the old behaviour); pin `scan: interlaced` for a field-correct
interlaced conversion.

Where it applies: `auto` field-codes only on a TS output's re-encode —
broadcast receivers display interlace natively. The **TS ingress
transcoder** (an input's `video_encode`) treats `auto` as progressive: its
output is the flow's source for every output on it, and a browser-facing
passthrough output — WebRTC / WHIP (and the relay's WHEP SFU and DVR origin
it feeds), RTMP, HLS / CMAF without a `video_encode` of their own — would
hand MBAFF to decoders that cannot take it (OpenH264 has no interlaced
tools). Pin `scan: interlaced` on the input to field-code there anyway. RTMP
and CMAF honour an explicit `interlaced`; WebRTC refuses it (browsers
display progressive only), and `webrtc_compatible` pins `progressive`. The
raw-frame ingests (ST 2110-20 / -23, SDI, MXL) refuse `interlaced` — their
frames carry no field order for the encoder to follow — and code
progressive under `auto`.

Not covered: an **HEVC field_seq** source (770_H's 1920x540 field pictures)
decodes to one field per picture; weaving them back into frames is not done,
so it is re-encoded as progressive 540-line pictures at the field rate
(50 fps). Its SAR describes the frame the fields make (see *Sample aspect
ratio*), so the 540-line output signals 1:2 and displays at 16:9 — it used
to leave SAR-less and display at 32:9. Under `interlaced` it falls back to
progressive with the Warning.

Measured on the broadcast test captures (libx264, 8000 kbps, 2-minute
captures): Sky Sports 1080i25 (PAFF)
and Sky Witness (MBAFF) come out MBAFF, `pic_struct` signalled,
`field_order=tt`, 25 fps; Spain (MPEG-2 720x576i) MBAFF at 64:45; with
`interlaced` and 720x576 the 1080i source is scaled per field to 576i at
64:45 (DAR 16:9); `auto` scaled to 1280x720 stays progressive; 770_H (HEVC
field_seq) stays progressive 1920x540 at 50 fps. On Sky, MBAFF measured
**+1.5 dB** mean PSNR over progressive at the same bitrate (38.5 vs
37.0 dB, 1500 frames). Audio (gates 1, 6) is unchanged, and so is
content-anchored lip-sync (within ±0.003 ms on the 2-minute capture — not
the 30-minute gate 3 window).

Behaviour change: an interlaced H.264 / MPEG-2 source on a TS output's
transcode with no `scan` set now comes out MBAFF where the host's backend can
code fields (an input's transcode is unchanged). Gate 7 (a professional IRD)
has not been run on it — only ffmpeg decodes were checked; `scan:
progressive` restores the old bitstream.

### Sample aspect ratio

The re-encode signals the source's sample aspect ratio in the VUI (nothing
set it before, so every output left SAR-less and a receiver assumed square
pixels: a 720x576 16:9 anamorphic service at 64:45 displayed at 5:4). The SAR
is the decoded frame's (the decoder's, when the frame carries none).

- **Only a signalled ratio is carried.** An unspecified source stays
  unspecified, scaled or not — what every encode signalled before. It is not
  taken as square: the raw-frame ingests (ST 2110, SDI, MXL) carry no SAR
  and an SD capture is anamorphic, so a 720x576 16:9 SDI source upconverted
  to 1920x1080 "as if square" would signal 45:64 and show at 5:4.
- **Scaled, the display aspect ratio is kept**: `out = src_sar × (src_w ×
  dst_h) / (src_h × dst_w)`, reduced — 720x576 at 64:45 scaled to 1024x576
  signals 1:1, and 1920x1080 at 1:1 scaled to 720x576 signals 64:45.
- **A single-field source's SAR is its frame's.** An HEVC field_seq decode
  hands out 1920x540 fields that signal 1:1 for the 1920x1080 frame they
  make, so the geometry is taken as 1920x1080: an unscaled (540-line)
  output signals 1:2, one scaled to 1920x1080 signals 1:1 — both 16:9.
- **It follows the source.** An in-band aspect change (an SD DVB service
  switching between 16:9 programmes and 4:3 inserts, 64:45 ↔ 16:15) or an
  input switch to a source of another shape or size is followed once the new
  ratio has held for 3 frames, with an IDR so the new SPS goes out at once —
  on **libx264** (`VideoEncoder::set_sample_aspect_ratio`). Every other
  backend (x265, NVENC, QSV, VAAPI, RKMPP) fixes the ratio at open: the
  output keeps it, with a warning log, until it restarts. So does RTMP,
  whose SPS travels once in the FLV sequence header. libx264 cannot
  withdraw a ratio, so a later source that signals none is signalled 1:1,
  which a receiver reads the same way.

Measured (libx264, 40 s captures): 770_H's 1920x540 field pictures come out
SAR 1:2, DAR 16:9 (they were SAR-less, 32:9), and with `height: 1080`
1920x1080 at 1:1, DAR 16:9 (the first cut of this signalled 2:1, 32:9);
Spain stays 720x576 at 64:45 and Sky Sports at 1:1, with no ratio change
logged on any of the three.

Every decode → encode path gets it (TS outputs and ingress, RTMP, WebRTC,
CMAF). There is no operator override yet.

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
| `h264_rkmpp` | `video-encoder-rkmpp`     | Rockchip MPP HW encode (`h264_rkmpp`) on RK3568 / RK3588 SoCs. Build: `librga` + `rockchip_mpp` headers (pkg-config). **4:2:0 8-bit only.** aarch64 only; ships in the `*-aarch64-linux-rockchip` release artefact (not in `*-linux-full`). | Royalty-free; LGPL API layer. No GPL bundle. |
| `hevc_rkmpp` | `video-encoder-rkmpp`     | HEVC on the same MPP backend; same build deps + 4:2:0 8-bit limit. | same |

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

A plain `cargo build --release` (the default feature set — no software
video encoders, AGPL-only binary) is a valid local build but is **not**
a published release artefact. The GitHub Actions release workflow ships
only **full** variants: `*-x86_64-linux-full`, `*-aarch64-linux-full`,
and `*-aarch64-linux-rockchip`. The composite `video-encoders-full`
feature bundles every video codec backend the edge knows about —
encoders (x264 + x265 + NVENC + QSV + VAAPI) **and** HW decoders for the
local-display + transcode-input paths (NVDEC + QSV-decode + VAAPI-decode)
— and drives the `*-x86_64-linux-full` variant. See
[`docs/installation.md`](installation.md) for the release-channel
reference. The `*-aarch64-linux-full` artefact intentionally
drops QSV (encode + decode; Intel iGPU is x86_64-only) and lists
the remaining features explicitly: x264 + x265 + NVENC + NVDEC +
VAAPI encode/decode. The `*-aarch64-linux-rockchip` artefact carries
the Rockchip MPP encoders/decoders (`h264_rkmpp` / `hevc_rkmpp`) plus
the RGA transfer path instead.
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
# Linux — default build (no software video encoders; valid local build,
# but not a published release artefact — the workflow ships full variants only):
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
  `program_filter → audio_replacer → video_replacer → pcr_remux → egress`.
- **Output DTS never steps back.** A decoded frame whose PTS does not
  advance past the last one admitted to the encoder — by up to 1 s — is
  dropped *before* it is queued or encoded: FFmpeg's H.264 decoder emits the
  leading pictures of a splice without a clean random-access point in decode
  order, and their PTS used to go straight to the wire (+11 572, +10 800,
  −25 200, +28 800 ticks at every loop of a looping media file, with the
  PTS-derived PCR stepping −279 ms alongside). A drop leaves a pending
  force-IDR and the frame counter for the next admitted frame; it counts in
  `dropped_frames` and in `non_monotonic_frames_dropped` on the video encode
  stats. A step of more than 1 s either way is a new epoch and passes. A
  decoded frame without a PTS takes the last admitted PTS plus the
  *measured* frame interval (not the per-field DTS step of a PAFF source).
  That interval is learned from decoder PTS only — the span between two
  frames that carried one, divided by the frames decoded across it, within
  10–200 fps — never from a PTS it derived itself, and otherwise falls back
  to the pinned `fps_num` / `fps_den`, else 25 fps. Learning from derived
  steps confirmed whatever guess it started from: on a 29.97 fps source
  that stamps only every 12th picture, the 25 fps default ran each run of
  derived PTS past the next real one, which was then dropped as out of
  order — every real timestamp lost and the output 20 % fast. Now the first
  GOP or two can still lose their real frame while the interval is learned;
  after that every real timestamp is admitted as it is. A PES without a PTS
  reaches the decoder without one (it used to go in as a real PTS of 0).
- **The decoder opens on the SPS, seeded from it.** An H.264 source's decoder
  is opened on the first PES that carries an SPS (PES before it are passed
  over — nothing decodes before one; bounded at 300 PES), through
  `VideoDecoder::open_opts` with `ReorderSeed::FromAccessUnit`: its reorder
  depth (`has_b_frames`) starts at 0 when the SPS declares
  `max_num_reorder_frames` (libavcodec then applies the declared depth, 0 for
  IPPP — no added latency) and at 1 when it does not. libavcodec's default of 0
  let a join on a non-IDR I picture mark the synthesised frame_num-gap
  placeholders as recovered, and the join GOP read uninitialised memory (13-15
  wrong frames per join on Sky Sports). Every other lazy decoder open in the
  edge takes the same seed from the AU it opens on, and waits for an AU that
  can give it one: the RTMP, WebRTC and CMAF re-encoders (the DVR clip
  exporter included), the ST 2110-20 / -23 and MXL egress decoders and the
  mosaic tiles open on the first AU that carries an SPS, bounded at 300 AUs
  (`video_encode_util::SpsOpenGate`), and the display and SDI outputs on a
  keyframe. Opened on whatever AU came first — a P picture, at almost any
  join — a source that declares no reordering (x264 `zerolatency`, the edge's
  own encodes, most contribution encoders) was seeded 1, and libavcodec never
  lowers the depth: a frame held for the life of the output. Only the warm
  thumbnail opens on the first AU, where a frame of latency is immaterial.
  The display / SDI outputs **drop and re-open** a seeded decoder on an
  operator switch instead of flushing it — a flush keeps the depth learned
  from the old source and cannot re-apply a seed; a display PTS jump (an SRT
  FEC repair out of order, a media-player loop: the same source) still only
  flushes, so a source change upstream that arrives without a PMT version
  bump keeps the old depth. Two residuals, documented rather than
  fixed: a join on a stream whose true depth is 2+ and undeclared can still drop
  1-3 decodable leading B-pictures once (seeding the level's DPB size would fix
  it at the price of permanent latency), and an undeclared IPPP source now
  carries one frame (40 ms at 25 fps) of decoder latency — which also moves the
  transcoded output's mux interleave by that frame. NVDEC / QSV / RKMPP
  decoders take no seed. Measured on Sky Sports played through a `media_player`
  input (the capture starts mid-GOP): the first GOP of the re-encoded output used to
  decode at 7.7 dB PSNR against the source (15 frames under 25 dB); seeded,
  the minimum over 1500 frames is 34.3 dB.

## Output PCR — the remux model

Both replacers keep the **input's PCR timeline** and neither generates a
PCR of its own. Every input PCR on the source PCR_PID — riding in a video
payload packet or in an adaptation-field-only packet, on the video, the
audio or a dedicated PID — leaves the chain at the same stream position,
value and discontinuity_indicator unchanged, as an adaptation-field-only
packet (on the video PID when video is re-encoded, on the audio PID when
the PCR rides there). A trailing stage, `engine::ts_pcr_remux`, then
delays that timeline by one **measured** transcode allowance `D`:
output PCR = input PCR − `D`, at the same positions and the same cadence
as the input's.

`D` starts at 80 ms. The first re-encoded PES of each PID latches it to at
least how late that PES arrived behind its own decode time plus 80 ms; a
PES that is still late afterwards raises it again (DI on the next PCR,
Warning `transcode_pcr_late`, `late_frames` on the stats). Lateness that a
pause in the source's own PCR explains — a paused or variable-frame-rate
ingest that stamps a PCR per frame — does not raise it; nor does the wait
of a frame a **silent video PID** kept in the pipeline (the last picture
or two of a media-player file, which wait for the next loop's first
video): the video replacer reports that wait, and such a frame, if still
late, is dropped as stale — before the encoder, so the encoded stream
loses no reference picture — instead of raising `D` (a loop used to raise
it 80 → 261 ms for the rest of the run). The margin is cut to keep the
program's largest video lead within the 1 s T-STD residency, never below
40 ms, re-checked as that lead grows — once per epoch `D` itself comes
down to meet it with 20 ms to spare, never below the largest measured
lateness + 40 ms (one forward PCR step, DI); Warning
`transcode_pcr_residency_exceeded` for what is left. A forward input PCR step without DI is the input's clock however
long — a 5 fps or 0.5 fps PCR-per-frame source is one timeline, not an
epoch per frame — and a DI on a PCR the input's cadence predicts is no
epoch either. The PCR carrier of a video packet leaves after the
re-encoded frame that packet completed, so a PCR-per-frame source latches
no frame interval into `D`. While nothing is re-encoded (a codec the
replacers cannot decode) the stream passes byte-identical, with no `D`. A
source with no PCR — none ever, or none for a second of the input's own
video decode time — gets one synthesised from the re-encoded video (Info
`transcode_pcr_synthesized`), on its own allowance: the synthetic clock
never moves `D`.
Full model, the epoch rules and the numbers it replaced:
[`clocking.md`](clocking.md#transcoded-output-pcr-the-remux-model).

On an **ingress** transcode a raise of `D` reaches the input's muxer-mode
clock rewriter as a backward PCR step with DI, and passes through it as
that — PES timestamps stay continuous on every output, so HLS / CMAF /
WebRTC / RTMP / display see no gap; at flow start the first latch usually
makes one such step.

**Behaviour change.** Output PCR used to be `video PTS × 300 − 80 ms`,
floored on a decaying audio lag: a clock that ran ~15 500 ppm fast against
its own PTS (a 40.6 ms PCR step per 40 ms frame on a PAFF source), with
zero and backward steps and a PCR only once per frame. Now the PCR rate is
the input's and the **PCR→PTS relationship of every transcoded TS output
changes**: each re-encoded ES leads the PCR by its source lead plus `D`
minus its own pipeline delay. PES PTS are untouched, so lip-sync is too.
An **audio-only** transcode is now delayed as well — its PCR used to pass
through unchanged, and the audio replacer (which emits PES *k* only when
PES *k + 1* arrives) was late against it on 3 930 of Sky's 4 016 PES.

**Stats.** `transcode_pcr` on the output's stats (and, for an ingress
transcode, on the flow's, for the active input): `offset_ms`,
`late_frames`, `offset_raises`, `stale_frames_dropped`,
`synthesized_pcrs`, `epochs`.

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

### Audio re-encode gaps found in 2026-09 (not fixed)

- **HLS re-encodes each segment on its own.** Every segment's encoder
  starts from priming (AAC-LC 2048 samples, MP2 481, AC-3 256) and the
  segment's audio is stamped from its first source PTS, so the content
  presents that much late; the fdk AAC path never flushes the encoder, so
  the samples in its delay line at the segment's end are not emitted. The
  channel / rate stage lines up exactly (`BatchStage`); the encoder does
  not. A continuous encoder across segments is the fix.
- **RTMP (and WebRTC) `audio_encode` on an MP2 / AC-3 / E-AC-3 source
  without `silent_fallback`** never builds its encoder — it is built from
  the demuxer's cached ADTS config, which such a source never provides —
  so the output carries no audio. With `silent_fallback` the eager encoder
  takes the source through the stage.
- **The SDI input ignores `audio_encode.sample_rate` / `channels`** (the
  encoder is opened at the capture's format).

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
2. **Frame rate: set it to match the source. It is never a resampler.**
   No path converts frame rate — every source frame is encoded, so
   `fps_num` / `fps_den` *declares* the rate rather than resampling to
   it. Where the value comes from differs by path:
   - **SRT / UDP / RTP / RIST outputs and the TS ingress transcoder**
     (`engine::ts_video_replace`) measure the source rate from the
     **decoded frames' PTS** — the pictures the encoder is actually
     handed, once per call (`video_encode_util::FrameCadence`) — and lock
     the encoder to it before it opens, **only when the field is unset**
     (a pin is never overridden, only reported). Until 2026-09 the rate
     came from the first PES DTS delta: that is the rate of *coded
     pictures*, so a PAFF H.264 or MPEG-2 field-picture source, which
     carries one field per PES 1800 ticks apart, locked 50 fps while the
     decoder handed out 25 woven frames a second — the VUI said 50 fps, CBR
     budgeted for 50 frames (the video came out at half the configured
     bitrate) and the default GOP ran 4 s. The meter takes the mean of four
     frame deltas that agree within 0.1 % with every delta seen so far (of
     two, on a source stamping only every Nth picture: each of its spans
     is already a mean over N frames), or from 12 deltas the span of the
     deltas over the frames they cover (how many each covers read off the
     median of 4-delta sums, so one dropped frame does not move it — a
     delta counts as two frames only when it stands clear of the others'
     spread): for a steady or periodic cadence the median over windows of
     12 (24 once there are that many) — whole cycles of a 3:2 or 2:3:3:2
     pulldown, which measures exactly 24000/1001 — or, for uneven
     millisecond timestamps (an RTMP-ingest source) and jittered stamps
     (a browser's capture clock), the least-squares slope of the stamps
     over the frames. It snaps to the **nearest** standard rate within
     0.1 %, or within what the stamps resolve when that is more — a
     millisecond's rounding over the span, or 4.5 standard errors of the
     slope — and says nothing yet while that tolerance is past 5 % or holds
     two rate families (24 and 25, 48 and 50). Jittered stamps wait for 16
     deltas and lock only a standard rate: the encoder opens at the
     meter's first answer, and the first dozen deltas of ±8 ms stamps move
     the estimate by up to 4 % — the meter used to snap within half the
     per-frame spread there, and over 2000 starts 25 fps locked 24/1 in
     7 % and a non-standard rate in 9 %, 60 fps a non-standard rate in
     81 %, 50 fps 48/1 in 22 %, and ±0.5 ms stamps (four agreeing within
     0.5 % now and then) a non-standard rate in 7-16 %. Now each locks its
     own rate family (one start in 2000 at 50 fps ±8 ms took 48/1), at
     frame 17 (about 21 at 50 / 60 fps ±8 ms). A stamp that jitter puts
     within a millisecond of the one before is counted as a frame whose time
     joins the next span, not dropped as a discontinuity. The median of 4-delta sums alone read an RTMP publish's
     33 / 33 / 34 ms steps at 30 fps as 2992.5 ticks and opened the encoder
     at 90000/2993 (30.07 fps; 60 fps at 22500/377, 24 fps at 24000/1001).
     A dozen millisecond stamps cannot tell 30/1 from 30000/1001 (they
     drift apart by a millisecond a second): an RTMP source may lock at
     its rate's 1001 neighbour, 0.1 % off; a full window (32) tells them
     apart. A frame the decoder hands
     out without a PTS measures nothing but is counted: MPEG-TS needs a
     PTS only every 700 ms, and a source that stamps every Nth picture (or only its I pictures) measures the
     span between two stamped frames over the frames decoded across it —
     taken as one frame, a 29.97 fps source stamping every 12th picture
     locked 2500/1001 fps (CBR budgeting 12x the bitrate per frame, a
     4-frame GOP). Frames decoded before the rate is known — about four at
     startup, two stamped spans on a sparse source — are dropped, since
     the encoder cannot open without it; after 60 decoded frames with no
     usable PTS it opens at the PES DTS step (a span over PES without a
     timestamp divided by the PES it covers) times the PES-per-frame
     ratio, else 30/1. On a sparse source the wait runs past its first
     stamp for three of its spans (at most 120 frames past it:
     `FrameCadence::lock_wait_frames`), which covers a PTS every 700 ms —
     MPEG-TS's limit — at 60 fps joined anywhere, and a 2 s GOP at 25 fps.
     It used to take four spans within 60 frames, so a source stamping its
     I pictures every 15 or more frames (a 1 s GOP at 25 fps, every 25th
     picture at 50 fps) opened at the fallback before its rate was known. An input switch with the encoder already open drops
     nothing (its rate cannot change).
     HEVC field_seq sources measure the field rate, 50/1, which is right
     for the 540-line pictures they are coded as (see *Scan*). Measured on
     Sky Sports 1080i25 with `bitrate_kbps: 8000`: the VUI went from
     `time_scale` 100 (50 fps) to 50 (25 fps), the video ES from 4002 to
     7981 kbps, and the IDR interval from 100 to 50 frames (2 s).
   - **RTMP, WebRTC and CMAF outputs** lock the same way, **only when
     the field is unset**: the decoded frames' PTS go through the same
     `FrameCadence` meter (`video_encode_util::EncoderRateLock`), the
     frames decoded before it can say (about four; two stamped spans on a
     sparse source, which extends the wait as above) are dropped, and
     after 60 decoded frames with no usable PTS the encoder opens at 30/1
     — these paths have no DTS fallback, so a sparse source that ran out
     the flat 60 frames opened at 30/1: VUI 30 fps, CBR 25/30 or 50/30 off,
     a 60-frame CMAF GOP. An
     access unit whose PES carried no PTS goes to the decoder without one
     (`DemuxedFrame::{H264, H265}::pts_known`), so its picture is
     counted, not measured, as on the TS path; and each such picture is
     stamped from the last one that had a PTS plus a frame for each since
     (`video_encode_util::FramePtsStamper`) — the frame measured from
     decoder PTS only, the last span between two stamped pictures over the
     pictures it covers (10–200 fps), as the TS replacer learns it; the
     encoder's rate only until a span is known. Stepped by the encoder's
     rate throughout (a pin, the 30/1 fallback, an earlier input's rate),
     a 50 fps source stamping every 25th picture on an encoder at 30/1
     stamped each GOP's last picture 27 000 ticks past the next real PTS —
     the timeline stepped back once a GOP (zero-duration CMAF samples, RTP
     timestamps going backwards). The
     demuxer used to hand these paths PTS 0 for it: the pictures of a
     source stamping only its I pictures came back stamped 0, the meter
     never measured (60 frames — 2.4 s at 25 fps — dropped, then 30/1), and
     their WebRTC RTP timestamps and CMAF samples carried 0. They
     used to open at a flat 30/1 whatever the source ran at: a 25 fps
     source's SPS VUI said 30 fps, CBR budgeted 25/30 of the configured
     bitrate and the default GOP ran 20 % long. The WebRTC decoder is now
     handed each access unit's PTS (it had none to measure). On RTMP the
     rate never affected A/V sync — FLV timestamps come from the source
     clock. These paths have no fallback on the PES DTS step and raise no
     `video_encode_fps_mismatch`: a pin there is used as it is. Measured
     (release build, unpinned x264): Sky Sports 1080i25 (PAFF) locks 25/1
     on all three — SPS VUI `time_scale` 50 with `num_units_in_tick` 1,
     where the same build before the change signalled 30 fps
     (`time_scale` 60); a 29.97 fps H.264 source locks 30000/1001 (VUI
     1001 / 60000, was 30 fps).
   - **Every re-encoded WebRTC and CMAF frame carries its own picture's
     source PTS** — the decoder propagates each access unit's PTS through
     its reorder queue, and the encoder's output is matched back to it by
     the frame counter it echoes (`video_encode_util::EncodedPtsMap`).
     Both used to stamp whatever came out with the PTS of the access unit
     being fed at the time: a pipeline's depth late, and out of order on a
     source with B-frames — Sky Sports re-encoded to WebRTC arrived with
     RTP timestamps stepping +160 / -80 ms, and CMAF samples carried
     decode-order times (a 29.97 source's CMAF track measured 0.1 fps from
     its timestamps). WebRTC now sends each encoded frame as its own RTP
     frame with its own marker bit (several frames handed back by one call
     used to share one timestamp and one marker). RTMP already stamped
     from the source PTS (a FIFO; its encoder never reorders). An encoder
     that reorders hands frames back in decode order (counters 0, 3, 1, 2,
     …): each is found by its counter, and an entry 32 frames behind the one
     coming back is taken as dropped (all those behind it were, and every
     B-frame lost its PTS). **CMAF pins `bframes` to 0**, as RTMP does, and
     warns `cmaf_bframes_unsupported`: its segmenter takes each stamp as
     the sample's decode time and writes no composition offsets, so
     display-order stamps in decode order stepped its timeline back.
   - **CMAF's default GOP tiles the segment** at the rate the encoder
     opens at (`cmaf::encode::cmaf_default_gop`): the fewest GOPs of at
     most 2 s that cover `segment_duration_secs`, each rounded **up** to a
     whole frame — 50 frames for 2 s segments at 25 fps, 60 at 29.97 and
     30, 48 at 23.976, three 2 s GOPs in a 6 s segment. The segmenter cuts
     on the first IDR at or after the target, so the segment's last GOP
     ending at or just past it closes the segment on time; one ending a
     frame short would run it on to the next IDR. It used to force 60
     frames at any rate — 2.4 s at 25 fps, so a 2 s target cut 2.4 s
     segments. A set `gop_size` is honoured as it is. Either way an IDR is
     **forced on every GOP boundary** of the encoded-frame count: x264
     codes a scene cut as an IDR and restarts its GOP count there, so
     without it the next natural IDR — and the segment boundary — landed a
     partial GOP late (on Sky Sports, 3.2 s and 3.56 s segments among the
     2 s ones). Scene-cut IDRs in between stay; they cost bits, never a
     boundary. (The RKMPP encoders ignore a forced IDR — see
     `force_next_keyframe` in the sibling crate.) Measured: Sky Sports
     with 2 s segments — 30 of 30 segments `EXTINF:2.0`, 50 frames each,
     every one opening on an IDR, `#EXT-X-TARGETDURATION:2` (before:
     2.0-4.36 s, target 4); the 29.97 fps source — 29 of 29 at 2.002 s,
     60 frames each (before: 2.0-3.84 s).

   A pin that disagrees with the source does **not** cause a
   proportional lipsync drift on the TS path (output PES PTS carry the
   source clock), but it does mistune the encoder: actual bitrate runs
   at the rate ratio times the configured value, the default GOP
   (`2 × fps`) spans the wrong duration, and the SPS VUI advertises the
   wrong rate. The edge logs `video_encode_fps_mismatch` once per
   source when the measured rate disagrees with the rate the encoder runs
   at, and `cause` says why it runs at that rate: a pin (`pinned`); on
   the TS path an earlier source's rate kept across a source reset
   (`input_switch`), the fallback taken because the first decoded frames
   carried no usable PTS (`fallback`), or this same source's cadence
   having changed since the lock (`cadence_change` — video to film, a
   playlist item at another rate). The encoder cannot reopen at a new
   rate, so restart the output to lock the measured one. A splice is not
   a cadence change: a step between two decoded frames more than four
   times the frame the meter has measured (a media-player loop, a PTS
   jump the source made) is left out of the measurement, and only two
   such steps in a row start it afresh at the new cadence. Counted as
   frames, a loop whose step is not a whole number of them (770_H at
   50 fps steps 18.8 frames, the audio's whole frames setting it) read
   50.083 fps and warned `cadence_change` on every output at the first
   loop.
3. **No rate-control tuning knobs.** We pass `bitrate_kbps` + a
   `tune=zerolatency` option and rely on defaults for VBV buffer size,
   CRF, look-ahead, etc. CBR-strict profiles (true constant-bitrate
   muxing) may need extra work for hard-rate contribution paths.
4. **No B-frames on RTMP or CMAF.** `bframes` is configurable (0–16) on the
   TS paths, but the RTMP output pins it to 0 and warns
   (`rtmp_bframes_unsupported`) if asked otherwise: FLV carries DTS in the
   tag timestamp and both tag writers hard-code the composition-time offset
   to 0 (the Enhanced-RTMP HEVC path has no CTS field at all), so a
   reordering encoder would drive DTS backwards and most ingests answer that
   by dropping the publisher. The CMAF re-encode pins it too
   (`cmaf_bframes_unsupported`): the segmenter takes each re-encoded
   frame's stamp as its decode time and writes no composition offsets.
   (WebRTC and clip export were already pinned.) Elsewhere
   `max_b_frames = 0` is the default to simplify decoder interop. Enabling them later would improve quality at a
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

### The `webrtc_compatible` output flag (browser-safe H.264)

`WebRTC` and `SRT` outputs carry a `webrtc_compatible: bool` (default
`false`). It is a convenience wrapper for operators who need to reach a
**WebRTC audience** — a browser, the relay WHEP SFU, or an external WebRTC
gateway (mediamtx / Janus) — without hand-tuning H.264 profile / NAL
internals.

**Why it exists.** A passthrough output forwards whatever the source
contains. If the source H.264 carries **B-frames** (any `media_player` file
or contribution feed encoded with them), RTP timestamps go non-monotonic
(B-frames are transmitted in decode order) and browser jitter buffers / WHEP
SFUs freeze — the classic *"WebRTC doesn't support H.264 streams with
B-frames"* failure. The real constraint is RFC 7742 (WebRTC's mandatory
Constrained-Baseline profile forbids B-frames), not RFC 6184. Note the
built-in **test pattern is already B-frame-free** (`bframes = 0`); only
passthrough of an external / file B-frame source is affected.

**What it does.** When `true`, the edge **always** re-encodes video to a
browser-safe H.264 stream via `config::models::webrtc_safe_video_encode`:

- **H.264** — `h264_auto` when no `video_encode` is set; an HEVC codec is
  forced to H.264 (and rejected at validation — browsers can't decode HEVC);
- **zero B-frames** — `bframes` hard-pinned to `0` (the crux);
- **8-bit 4:2:0** — `chroma = yuv420p`, `bit_depth = 8`; `high10` / `high422`
  / `high444` / `main10` profiles downgrade to `main`;
- **inline SPS/PPS** on every IDR (`global_header = false`, already the TS /
  WebRTC default);
- `tune = zerolatency`.

An explicit `video_encode` block is preserved (bitrate / resolution) but
pinned to the safe settings above. Because it always re-encodes, the flag
**requires an H.264 encoder backend compiled in** (`video-encoder-x264` or a
HW H.264 encoder) — validation rejects it with a clear message on an
encoder-less build. Profile is left at `main` / `high` rather than forced to
`baseline`: Main/High **without B-frames** is WebRTC-valid and higher quality.

The relay's WHEP SFU is a pure elementary-stream repacketizer with no
reorder / DTS machinery, so B-frame elimination is enforced **edge-side** (the
only place it can be) — no relay change is needed.

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
- **~~Measured frame rate on RTMP, WebRTC and CMAF.~~** Done
  (`video_encode_util::EncoderRateLock`, see *Frame rate* above); CMAF's
  default GOP now tiles the segment at that rate.
- **HEVC field_seq weave.** Pair an HEVC field_seq source's top / bottom
  field pictures back into frames (drop an orphan field, re-pair after a
  reset), so it re-encodes as 1080i at the frame rate instead of 540-line
  pictures at the field rate.
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
| `video-encode`              | Any of `video-encoder-x264` / `-x265` / `-nvenc` / `-qsv` / `-rkmpp` is enabled. (`video-encoder-vaapi` alone does **not** raise this bit — it emits only its own `video-encoder-vaapi` string below.) |
| `video-encoder-x264`        | Built with `--features video-encoder-x264`.                    |
| `video-encoder-x265`        | Built with `--features video-encoder-x265`.                    |
| `video-encoder-nvenc`       | Built with `--features video-encoder-nvenc`.                   |
| `video-encoder-qsv`         | Built with `--features video-encoder-qsv` (x86_64 only).       |
| `video-encoder-vaapi`       | Built with `--features video-encoder-vaapi` (Linux).          |
| `video-encoder-rkmpp`       | Built with `--features video-encoder-rkmpp` (aarch64 Rockchip only). |
| `video-encode-scan`         | Always (this release on): `video_encode.scan` is honoured. An older edge ignores the field on a push and codes progressive, so a UI must gate the scan picker on it. |

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
`video-encoder-vaapi`; `h264_rkmpp` and `hevc_rkmpp` both gate on
`video-encoder-rkmpp`. The `h264_auto` / `hevc_auto` / `auto` strings
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
