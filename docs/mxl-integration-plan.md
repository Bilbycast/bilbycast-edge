# MXL (Media eXchange Layer) — integration plan

> **Status (updated):** **implemented feature-gated (the `mxl` feature, default off).** The **video path works** — input encode and output decode-to-grain bridges are live in `src/engine/mxl_video_io.rs`; ANC pass-through is wired. The **audio codec bridge is the remaining gap** — `src/engine/mxl_io.rs` consumes/produces Float32 PCM but does not yet bridge it through `audio_encode` / `audio_decode` (input emits `mxl_audio_no_encode_set`, output emits `mxl_audio_decode_pending`). The original status line below ("design analysis only — implementation deferred") and the "no grep hits / clean slate" note in [Where bilbycast stands today](#where-bilbycast-stands-today) are **historical** — the design-analysis body that follows still holds and is preserved for rationale.
>
> **Original status (2026-05-13):** design analysis only — implementation deferred. Pick this back up when ready to greenlight. Upstream MXL state verified directly from `dmf-mxl/mxl` on this date; re-check before implementation if more than ~2 weeks have passed.

## Context

This doc captures the architectural analysis for adding MXL support to bilbycast-edge. Two questions were asked:

1. **Should bilbycast implement MXL at all?** → Yes, feature-gated off by default.
2. **As an external Input/Output type, or via Flow redesign?** → External Input/Output type. Don't redesign Flow.

## What MXL is — upstream snapshot 2026-05-13

The repo lives at **[github.com/dmf-mxl/mxl](https://github.com/dmf-mxl/mxl)** (the EBU-led project moved under a Linux Foundation umbrella — DMF = Dynamic Media Facility, with EBU + Linux Foundation + NABA + broadcaster backing).

- **License**: Apache-2.0 (compatible with bilbycast's AGPL-3.0-or-later combined work).
- **Status**: **v1.0.1 released 2026-05-07; v1.0.0 released 2026-02-26.** The API and feature set are explicitly **frozen** at v1.0. EBU's own marketing reads "Ready for production: Media eXchange Layer v1.0.0 published." Not alpha.
- **Codebase**: C++ internals, **C ABI surface**, plus **first-party Rust bindings in-tree** (released with v1.0.0-rc1 in Dec 2025).
- **Essence types supported at v1.0**:
  - Video: **V210** (uncompressed Y'Cb'Cr 4:2:2 10-bit progressive) — narrow, no v210a alpha confirmed at v1.0.1, no interlaced
  - Audio: **Float32 PCM @ 48 kHz**, mono or interleaved channels
  - Ancillary: **RFC 8331** (was experimental in rc1, **officially supported as of v1.0.1**)
- **Still experimental at v1.0.1**: sub-grain I/O / slices (ultra-low-latency mode), Fabrics (cross-host transport)
- **Scope at v1.0**: same-host, shared-memory only. Cross-host is the Fabric API which is `(tbc)`.
- **GStreamer tooling**: example tools shipping (`mxl-gst-testsrc`, AV sink, etc.).
- **Adoption (per SVG Europe / TVB Europe coverage)**: no shipping commercial MXL-native product yet as of Jan 2026; vendors targeting NAB 2026 / IBC 2026 announcements. Demos at NTS June 2026 and IBC 2025/2026. Initial use cases: clipping, multi-angle replay, graphics processing.

## Where bilbycast stands today

bilbycast-edge already has the exact abstraction MXL needs:

- **`InputConfig` / `OutputConfig` are tagged enums** with **15 input variants and 17 output variants** today (SRT, RIST, RTP, UDP, RTMP, RTSP, WebRTC, WHEP, HLS, CMAF, ST 2110-20/-23/-30/-31/-40, Bonded, Display, Replay, MediaPlayer, TestPattern, RtpAudio). Adding a transport is an additive enum-variant + dispatch-arm pattern, done many times across the codebase.
- **ST 2110-20/-23/-30/-31/-40 — the closest analog to MXL in concept** (uncompressed essence, PTP-anchored, RFC-formatted) — are already plumbed in. They live in `bilbycast-edge/src/engine/st2110/` and are dispatched from `flow.rs::spawn_single_input()`.
- The Flow architecture has **two parallel pipelines**: passthrough (no reassembly) and assembly (PID bus → SPTS/MPTS synthesis). The PID bus is **MPEG-TS only**. Uncompressed essence that doesn't encode-to-TS already bypasses it (see ST 2110-30 with no `audio_encode`, ST 2110-40 always). MXL would fit the same model.
- **C-library FFI wrappers are an established pattern**: `bilbycast-libsrt-rs`, `bilbycast-fdk-aac-rs`, `bilbycast-ffmpeg-video-rs`. A `bilbycast-mxl-rs` wrapping libmxl would slot in alongside them as a sibling crate — but only as a fallback (see Design sketch).
- ~~No grep hits for `mxl` / "Media eXchange Layer" in the codebase today — clean slate.~~ **(Historical — superseded.)** MXL is now implemented behind the `mxl` feature: `src/engine/mxl_video_io.rs` + `src/engine/mxl_io.rs`, config variants in `src/config/models.rs`, capability gating in `src/manager/client.rs`. Video + ANC bridges work; the audio codec bridge is the remaining gap.

## Recommendation

### 1. Should flows be redesigned around MXL? — **No, strongly.**

Redesigning flows around MXL would:

1. **Couple bilbycast to one vendor IPC layer** — antithetical to the deliberate multi-protocol identity that already supports 12+ wire protocols. MXL is a transport, not a fundamentally new media abstraction.
2. **Mismatch scope.** MXL today is on-host. bilbycast flows are cross-node by design (edge ↔ relay ↔ edge, contribution → distribution). The current Input → Flow → Output model is the right shape for both worlds.
3. **Throw away matched-shape work.** The existing tagged-enum + dispatch-arm pattern already accommodates additive transport changes with zero structural friction. ST 2110-20 — the closest twin in concept — already proves this shape works for uncompressed PTP-anchored essence.
4. **Force-fit.** MXL's domain/flow model doesn't map 1:1 onto bilbycast's Flow (which is a media-graph with arbitrary inputs/outputs and an optional PID-bus assembler). bilbycast Flow is the *consumer* of MXL flows on input and the *producer* of MXL flows on output — they live at different layers.

### 2. Should it be implemented? — **Yes.**

Why yes:

- bilbycast's natural positioning is "ST 2110 + cloud-grade broadcast on commodity hardware." MXL is the on-host complement of ST 2110. Skipping it lets EBU partners, NVIDIA-stack vendors, and the bigger SDI/IP-bridge players define cloud-native broadcast composition without us.
- The use case is real and aligned: SRT-in from venue → branding pod (over MXL) → audio mixer pod (over MXL) → contribution-encoder pod → SRT/RIST-out. One node, one PTP clock, no loopback-NIC tax. This is where bilbycast-edge running as a sidecar in a Kubernetes deployment becomes uniquely useful.
- Cost is *bounded* because the heavy lifting (uncompressed-video decode/encode, PTP timing, RFC packetisation, manager-side capability gating) is already done for ST 2110. MXL only swaps the transport surface.
- **First-party Rust bindings ship in-tree** — we don't need to write `bilbycast-mxl-rs` from scratch. We just depend on upstream's Rust crate (or vendor it if it's not on crates.io yet) and write thin glue at the engine layer.
- **Apache-2.0 license** is clean against bilbycast-edge's AGPL combined work — no GPL contamination concerns.
- **Timing window**: NAB 2026 + IBC 2026 are when vendors are announcing MXL support. Beating or matching that window has marketing leverage; missing it means catching up later.

Why still feature-gated off by default:

- Build complexity: brings CMake + libmxl C++ build deps onto every install that doesn't need it. Same pattern as `display` (Linux-only, off by default) and the optional video-encoder backends.
- Adoption is currently concentrated at large EU broadcasters. Contribution-grade and small-prod customers won't ask for it for 12–24 months. No reason to force every install to carry the C++ dep until then.
- Same release-channel pattern as the `video-encoders-full` artefact: the wiring is permanently in tree, the feature flag flips it on, the GitHub Actions matrix builds an extra variant (`*-linux-mxl` or roll it into `*-linux-full`).

## Design sketch

**FFI**: Use the **upstream Rust bindings** shipped in `dmf-mxl/mxl` rather than rolling our own. Depend on the upstream crate (path or git dep until/unless they publish to crates.io). Only fall back to a hand-written `bilbycast-mxl-rs` wrapper if the upstream bindings are incomplete or have undesirable ergonomics — and even then, that wrapper should sit on top of the upstream bindings, not bypass them.

### bilbycast-edge changes

| Surface | Change |
|---|---|
| Feature flag | New `mxl` feature, **default off** (like `display`) |
| Config enum | `InputConfig::Mxl(MxlInputConfig)`, `OutputConfig::Mxl(MxlOutputConfig)` — additive variants in `src/config/models.rs` |
| Engine | New `src/engine/mxl/` directory with `input_mxl_video.rs`, `input_mxl_audio.rs`, `input_mxl_anc.rs`, `output_mxl_*.rs` siblings |
| Dispatch | New arms in `flow.rs::spawn_single_input()` and `spawn_output()`; `is_ts_carrier()` returns true only when video-encode is configured (mirrors ST 2110-30's pattern, not ST 2110-20's mandatory-encode pattern) |
| Reuse | Decode-back-to-pixel-planes path from `output_st2110_20`; H.264/HEVC encode path from `input_st2110_20`; optional `audio_encode` toggle from ST 2110-30 |
| Format conversion | **MXL v1.0 only accepts V210 video and Float32 PCM @ 48 kHz.** Boundary conversions: planar 10-bit (e.g. `YUV422P10LE` from libavcodec / our 2110-20 path) → V210 packed; planar / interleaved Float32 ↔ AES3/L16/L24/L32 PCM. Reuse `bilbycast-ffmpeg-video-rs` `libswscale` for the video pixel format conversion path |
| Capability bit | `"mxl"` added to `HealthPayload.capabilities` so manager UI gates the option |
| Validation | Rules in `src/config/validation.rs`. Reject non-V210-compatible source formats with a clear error rather than silent transcode (so operators don't get surprise CPU spikes) |
| Build deps | libmxl (vendored via CMake + vcpkg per upstream), C++ toolchain. Build prereq doc in `bilbycast-edge/docs/installation.md`. The `*-linux-full` release artefact channel is the natural home for the MXL feature bit |
| PTP / clocking | bilbycast-edge already has the per-flow master clock and PTP slaving (`master_clock` capability). MXL flows must use the **PTP-derived** clock to align with the upstream MXL "well-defined timing model"; refuse `master_clock=wallclock` for MXL flows |

### bilbycast-manager changes

Mostly JSON-schema reflection — the config struct's serde fields auto-surface. UI dropdowns gated on the `"mxl"` capability bit. Per-driver UI updates in `ui/static/js/devices/edge/`. No protocol-version bump needed (additive variant; existing string-based dispatch with catch-all arms handles unknown types gracefully).

### Effort estimate

Roughly half a dozen Claude Code sessions to land cleanly:

1. FFI / upstream-bindings integration + first compile
2. Engine glue — video (V210 pack/unpack + format-conversion path)
3. Engine glue — audio (Float32 ↔ AES3/L16/L24/L32) and ANC (RFC 8331 mostly straight-through)
4. Manager UI + capability gating + JSON-schema reflection
5. Broadcast-quality test rig — MXL→MXL loop on one host, ST 2110↔MXL bridge as Gate-7 receiver
6. Docs (this file → user-facing) + `*-linux-full` packaging hook + installation notes

Most cost is in format-conversion correctness and the broadcast-quality gates — *not* in MXL plumbing itself.

### Broadcast-quality gating

An MXL input/output flow is *transport* — its content quality must still pass the 12-gate broadcast quality matrix end-to-end (see `testbed/BROADCAST_QUALITY_GATES.md`). Wallclock-rate, two-decoder round-trip, A/V drift, PCR_AC, sample-level SNR, receiver compat, long-soak. MXL implementation isn't "done" until those pass on a real chain.

**Gate 7 (receiver compat)**: cross-vendor MXL receivers don't exist as shipping products yet; substitute with the upstream `mxl-gst` example tools and an MXL→ST 2110-20→Cobalt 9202 (or Appear X / Cisco D9824) bridge.

## What to NOT do

- Don't redesign Flow or PID bus around MXL.
- Don't introduce MXL as a special-case path that bypasses the Input/Output enum dispatch — that breaks the manager's JSON-schema reflection and the validation pipeline.
- Don't roll a from-scratch `bilbycast-mxl-rs` if the upstream Rust bindings are usable — first-party bindings track the C ABI and reduce maintenance.
- Don't enable MXL's still-experimental Fabric API (cross-host) — bilbycast already has bonding + relay + SRT/RIST for cross-node; MXL's value is on-host composition.
- Don't ship the `sub-grain I/O / slices` experimental low-latency mode in the first cut — wait for it to leave experimental.
- Don't silently transcode source formats into V210 / Float32 — surface the format restriction in validation so operators don't get hidden CPU spikes.

## Files referenced (for follow-up work)

- `bilbycast-edge/src/config/models.rs` — `InputConfig` (line 1153+), `OutputConfig` (line 2316+), `is_ts_carrier()` (line 1583+)
- `bilbycast-edge/src/engine/flow.rs` — `spawn_single_input()` (line 2097+), `spawn_output()` (line 1476+), `FlowRuntime` (line 122+)
- `bilbycast-edge/src/engine/st2110/` — closest analog pattern (input/output for -20, -23, -30, -31, -40)
- `bilbycast-edge/src/engine/ts_es_bus.rs` — PID bus (TS-only, MXL would bypass)
- `bilbycast-edge/src/engine/ts_assembler.rs` — SPTS/MPTS synthesis (TS-only)
- Reference FFI wrapper crates: `bilbycast-libsrt-rs/`, `bilbycast-fdk-aac-rs/`, `bilbycast-ffmpeg-video-rs/`
- Reference cloud-positioning pages: `bilbycast-website/` (for landing-page copy if MXL becomes a public differentiator)

## Verification (when implemented)

End-to-end golden path:

1. `cargo build --release --features mxl,video-encoders-full` on Linux with libmxl installed via CMake/vcpkg.
2. Run a flow: SRT input → bilbycast-edge → MXL output. Run a second bilbycast-edge process on the same host: MXL input → ST 2110-20 output. Confirm a third receiver (Appear X / Cobalt 9202 / ffmpeg-on-st2110) locks cleanly. This is also the substitute for Gate 7 (no shipping MXL-native receivers exist yet).
3. Cross-check the MXL flow with the upstream `mxl-gst` example tools — confirm a third-party MXL consumer can read what we publish, and we can read what the upstream sample publisher produces.
4. Confirm capability bit visible in manager UI; confirm UI option gated when feature is off.
5. Run broadcast-quality gates 1–6 + 10 on the chain (per `testbed/BROADCAST_QUALITY_GATES.md`).
6. 4-hour long-soak with input-switch every 30 s; confirm no leak in MXL shared-memory regions (track via `/proc/<pid>/status` `VmRSS` and `/dev/shm` size or wherever libmxl backs its domains — verify against `mxlIsTmpFs()` per upstream v1.0.1 notes).
7. Verify PTP master-clock alignment: `master_clock=ptp` flow → MXL output → second pod consuming MXL → confirm grain timestamps are coherent across processes (i.e. MXL's timing model agrees with our PTP-anchored 27 MHz clock).

## Sources

Verified 2026-05-13:

- [github.com/dmf-mxl/mxl — releases](https://github.com/dmf-mxl/mxl/releases) — v1.0.0 (2026-02-26), v1.0.1 (2026-05-07), Apache-2.0
- [EBU: Ready for production: MXL v1.0.0 published (2026)](https://tech.ebu.ch/news/2026/ready-for-production-media-exchange-layer-v1-0-0-published)
- [EBU: MXL v1.0.0 RC moves closer to market use (2026)](https://tech.ebu.ch/news/2026/mxl-v1-0-0-release-candidate-moves-closer-to-market-use)
- [EBU + Linux Foundation + NABA: open-source MXL SDK announcement (2025-06)](https://tech.ebu.ch/news/2025/06/ebu-launches-open-source-mxl-sdk-for-software-based-media-exchange)
- [Linux Foundation: intent to form MXL Project](https://www.linuxfoundation.org/press/linux-foundation-announces-intent-to-form-the-media-exchange-layer-project)
- [SVG Europe: DMF and MXL in practice — vendor adoption pace](https://www.svgeurope.org/blog/headlines/dmf-and-mxl-in-practice-which-vendors-are-adopting-it-and-how-fast-is-the-ecosystem-maturing/)
- [TVB Europe: How MXL could reshape software-based broadcast workflows](https://www.tvbeurope.com/ip-migration/how-the-media-exchange-layer-mxl-could-reshape-software-based-broadcast-workflows)
- [The Broadcast Bridge: Introducing MXL](https://www.thebroadcastbridge.com/content/entry/21669/broadcast-standards-introducing-mxl-the-media-exchange-layer)
- [EBU Tech: MXL landing](https://tech.ebu.ch/dmf/mxl)
