# Clocking and A/V sync

Every flow on bilbycast-edge runs against a per-flow **master clock**.
PCR generation, output emission timing, and lipsync trim all bottom out
on the same `MasterClock::now_27mhz()` call. Source-PCR PLL recovers the
upstream 27 MHz; PTP slaves the local realtime clock to a grandmaster;
Wallclock is the degraded fallback.

## Why a master clock

Before this work landed, every output stage owned its own emission
timing. Output PCR was derived from PES PTS (`pts × 300 − preroll`),
which means PCR jitter mirrored the encoder pipeline depth. On a
transcoded SRT→RTP flow we measured **30–50 ms** of residual A/V drift
even after fixing every other PCR / PTS bug along the way.

A single per-flow clock fixes this:

- **PCR is generated from the master clock**, not derived from PTS.
  Multiple outputs of the same flow emit identical PCR sequences.
- **PTS still flows from the source** via `src_pts_queue` in the audio
  + video replacers, so A/V offset versus source is preserved.
- **Cross-edge coherence is free** when two edges slave to the same
  source PCR or same PTP grandmaster. 2022-7 hitless redundancy at the
  receiver works without an external genlock.

| Master kind        | When auto-selected                                 | Lock criterion                             |
|--------------------|----------------------------------------------------|--------------------------------------------|
| `Wallclock`        | **Default** for SRT / RTP / UDP / RIST / RTMP / RTSP / `media_player` / `replay` / `test_pattern` / `rtp_audio` / `bonded` inputs; WebRTC ingress; no active input | Always locked — no convergence concept     |
| `SourcePcrPll`     | PID-bus assembled flows (assembler needs the source's recovered clock to keep cross-program PCR coherent) | PI loop converges, p99 jitter < 100 µs over 64-sample window after ≥ 100 samples |
| `Ptp`              | ST 2110-20/-23/-30/-31/-40 + MXL inputs            | `ptp4l` reports `port_state == SLAVE` and offset within tolerance |
| `AudioMaster`      | (Reserved for future ALSA-master local-display path) | (Not yet implemented; falls through to Wallclock) |

**Why Wallclock is the default for contribution-class TS sources.**
The earlier auto-policy auto-selected `SourcePcrPll` for SRT / RTP /
UDP / RIST / RTMP / RTSP. In practice the PLL never locks on
contribution sources that carry per-source-restart PCR
discontinuities — e.g. `ffmpeg -re -stream_loop -1 -c copy` on a
30-second file, every kind of looping playout, SCTE-35 splice
insertions, source encoder restarts. With `Wallclock` as the default
the master is always locked, always monotonic, and the encoder-style
PES PTS regenerators (`engine::ts_pts_rewriter` and
`TsAudioReplacer::set_av_sync_pacer`) can anchor against a clean
timeline immediately.

Operators who run on PTP-disciplined or clean-PCR contribution
sources and want cross-edge coherence opt in to the PLL via the new
`master_clock.kind = "contribution"` (preferred — flags intent in
telemetry) or the legacy `master_clock.kind = "source_pcr_pll"`
(retained for back-compat with existing deployments).

Operators can pin a master per-flow with the `master_clock` field in
flow config (overrides the auto-policy). Useful when a plant has PTP
everywhere and the operator wants every flow paced against the
grandmaster regardless of input type.

## Encoder-style PES PTS regeneration

Every TS-carrying ingress can opt in to byte-level PES PTS/DTS
regeneration via the per-input `regenerate_pts: bool` config field
(default `false`). When set, the new
[`engine::ts_pts_rewriter`](../src/engine/ts_pts_rewriter.rs) stage
inside `engine::input_post_process::InputPostProcess` rewrites each
PES header's PTS (and DTS when present) so emitted timestamps come
from the per-flow master clock instead of the source TS bytes.

The model is per-PID **anchor + source-delta**:

```text
On first PES of PID (or on >500 ms source-PTS discontinuity):
    anchor_out_90k = master.now_27mhz()/300 + PCR_PREROLL_90K  (= 7_200, 80 ms)
                     + lipsync_offset_90k (audio PIDs only)
    anchor_src_90k = source PES PTS

On every PES:
    delta_src   = source_pts - anchor_src_90k          (wrapping, 33-bit)
    out_pts     = anchor_out_90k + delta_src
    out_dts     = out_pts - (source_pts - source_dts)  if DTS present
```

This preserves the source's PES inter-arrival timing exactly (no
per-PES master_now jitter injection) while making absolute PTS
values master-clock-derived. DTS preserves the source PTS-DTS delta
so H.264 / HEVC B-frame reorder still decodes correctly.

A **10 s safety check** on the anchor candidate falls back to the
raw source PTS when master and source are wildly uncorrelated
(Wallclock master + small-offset encoder PTS — the common case
today): preserves the existing anchor-to-source behaviour when the
master clock can't help, kicks in only when the two agree to within
10 s (PTP master + PTP-disciplined source, or a locked
`SourcePcrPll` master).

The transcoded path landed the same model in
[`engine::ts_audio_replace::TsAudioReplacer::set_av_sync_pacer`](../src/engine/ts_audio_replace.rs).
On first PES + every >500 ms source-PTS discontinuity, the audio
replacer anchors via the same `anchor_target` helper with the same
10 s safety. Wired through
[`engine::transcode_chain::build_for_output`](../src/engine/transcode_chain.rs)
(symmetric to the existing video pacer wire) and
[`engine::input_transcode::InputTranscoder::set_av_sync_pacer`](../src/engine/input_transcode.rs)
(audio + video together).

**When does the rewriter actually rewrite?** Only when the
`anchor_target` 10 s safety lets it. On a flow with `Wallclock`
master (the new default) and a typical encoder-relative source PTS,
the safety triggers and the anchor falls back to source PTS —
effectively a no-op (output PTS == source PTS). To actually see
master-clock-derived PTS output, the flow needs either:

- `master_clock.kind = "ptp"` with PTP-disciplined sources, OR
- `master_clock.kind = "contribution"` (or legacy `source_pcr_pll`)
  AND the PLL has locked.

This matches the existing pre-rewriter behaviour for the safe path,
and unlocks the master-clock anchor path for the genuinely
clock-coherent configurations where it improves output quality.

## Module map

| Module | What it does |
|--------|--------------|
| `engine/master_clock.rs` | The `MasterClock` trait, `MasterClockKind` enum, `MasterClockHandle` (Arc + tag + clamped lipsync trim), `WallclockMaster`, `SourcePcrPllMaster`, `PtpMasterClock`, and the auto-select policy |
| `engine/pcr_pll.rs` | Software PI-controller PLL recovering source's 27 MHz from incoming PCR samples. PI loop on `(Δpcr_ticks, Δwall_ns)` with re-anchor on every accepted sample. Discontinuity filter mirrors `pcr_trust.rs` (gaps > 500 ms reset the anchor). Sticky lock-state hysteresis (enter at p99 < 100 µs, exit at > 500 µs). `now_27mhz(wall_ns)` projects forward from the anchor at the recovered rate so PCR generation never quantises to the ingress PCR cadence. |
| `engine/pcr_ingress_sampler.rs` | Per-flow ingress PCR sampler. Sibling broadcast subscriber (drop-on-Lagged) that scans every `RtpPacket` for adaptation-field PCRs and feeds the master's PLL. Handles both raw TS and RTP-wrapped TS via best-effort RTP header skip. Passive observer — never blocks the data path. |
| `engine/av_sync_mux.rs` | `AvSyncPacer` — thin wrapper around `MasterClockHandle` that exposes `pcr_27mhz_for_emit()` (master_now − PCR_PREROLL_27MHZ, modular-aware), `is_locked()`, and the lipsync trim. Plus `pcr_for_emit(pacer, pts)` helper that prefers the pacer when set, falls back to legacy `pts × 300 − preroll` otherwise. |
| `engine/ts_pts_rewriter.rs` | Encoder-style byte-level PES PTS/DTS rewriter, per-PID anchor + source-delta model. Plugs into `input_post_process::InputPostProcess` as a fourth optional stage; gated by per-input `regenerate_pts` config + an attached `AvSyncPacer`. See the "Encoder-style PES PTS regeneration" section above for the model. |
| `stats/pcr_trust.rs` | Per-output egress PCR accuracy sampler (4096-sample rotating reservoir, exact percentiles). Sibling consumer of the same PCR sample stream as the ingress PLL, but on the egress side. |
| `engine/wire_emit.rs` | Per-output PCR-anchored wire emission engine. Dedicated `std::thread` (Linux: `SCHED_FIFO` best-effort priority 50) pops TS datagrams off a `std::sync::mpsc::sync_channel(1024)` fed by the encoder task. Two release tiers: (1) **`clock_nanosleep(CLOCK_TAI, TIMER_ABSTIME)`** on SCHED_FIFO — the **default**; ~50–500 µs typical jitter, no kernel / NIC / PTP prerequisites. (2) **SO_TXTIME** — kernel-paced via the `etf` qdisc on `CLOCK_TAI`; sub-µs jitter when paired with HW-PTP, ~1–10 µs with software ETF. **Opt-in** via `BILBYCAST_ENABLE_TXTIME=1`; the probe is not attempted by default because on a host without the ETF qdisc the kernel accepts `setsockopt(SO_TXTIME)` and the `SCM_TXTIME` cmsg silently but emits each packet immediately, producing silent degradation. Closed-loop on observed inter-PCR rate (no declared-bitrate parameter — open-loop drifts when the encoder runs above/below its configured target). Discontinuity > 500 ms or any backwards step resets the anchor; a per-emitter monotonic-target guard prevents kernel ETF reorder on PCR discontinuities. **Wired into UDP, RTP (single-leg + FEC + 2022-7 dual-leg), 302M, ST 2110-20/-23/-30/-31/-40.** SRT, RIST, RTMP, HLS, CMAF, WebRTC keep their protocol-layer pacing. The legacy `BILBYCAST_FORCE_NANOSLEEP=1` env var is kept as a no-op alias for back-compat (the default is already nanosleep). Full doc: [`wire-pacing.md`](wire-pacing.md). |

## Data flow

```
                            ┌──────────────────┐
                  ┌────────►│  PcrPll (PLL)    │◄──── 1 Hz telemetry tick →
                  │         └────────┬─────────┘      FlowStatsAccumulator
   Per-input ┌────┴────┐             │                  ↓
   forwarder │broadcast│             │ now_27mhz()     FlowStats.master_clock
       ──────►   _tx   │             ▼                       (over WS)
              └────┬───┘    ┌──────────────────┐
                   │        │ AvSyncPacer      │◄───┐
            ┌──────┴─────┐  │ pcr_for_emit()   │    │ holds Arc<MasterClockHandle>
            │ PcrIngress │  └──────┬───────────┘    │
            │  Sampler   │         │                │
            └────────────┘         │ master_now-preroll
                                   ▼
                           ┌──────────────────┐
                           │ TsVideoReplacer  │── master-clocked PCR ──→ TS bytes ──→ broadcast_tx
                           │ TsAudioReplacer  │  (PTS still from src_pts_queue)
                           └──────────────────┘
```

## PCR pre-roll

Every master-clocked PCR is emitted as `master_now − PCR_PREROLL_27MHZ`,
where the pre-roll is **80 ms** (2 160 000 ticks). This matches the
ISO/IEC 13818-1 Annex L T-STD model — receivers need PCR to lead PTS by
at least the transport-buffer + CPB pre-roll. Choosing 80 ms also limits
the apparent A/V offset on receivers that don't apply T-STD scheduling
to audio.

The pre-roll is enforced in two places:

- `engine::av_sync_mux::PCR_PREROLL_27MHZ` (the canonical constant).
- `engine::ts_video_replace::PCR_PREROLL_27MHZ` (kept for the legacy
  fallback path; will fold into the canonical constant in a future
  cleanup).

## Lipsync trim

The handle exposes a per-flow lipsync offset in 90 kHz ticks, bounded
±18 000 (±200 ms). Updates are lock-free (`AtomicI64::store`). Manager
operators nudge it via the WS command `set_master_clock_lipsync` — see
`bilbycast-manager/docs/...` for the REST mirror.

The trim is currently surfaced in telemetry but not yet applied to
output PTS values (the field is plumbed through to make the future
audio-delay path mechanical).

## Telemetry

Every running flow surfaces a `master_clock` block on `FlowStats`:

```json
{
  "master_clock": {
    "kind": "source_pcr_pll",
    "locked": true,
    "rate_offset_ppm": -2.34,
    "jitter_us": 18,
    "lipsync_offset_90k": 0
  }
}
```

The 1 Hz background task in `FlowRuntime::start` mirrors the master's
`telemetry()` snapshot into the stats accumulator. Manager UI renders
the kind label, lock chip, rate offset, p99 jitter, and the trim knob.

## Capability gating

Edges advertise `master_clock` on `HealthPayload.capabilities`. Manager
UI gates the per-flow telemetry card + trim knob on this string. Older
edges (no master-clock work) keep their existing behaviour unchanged
and the manager UI hides the controls automatically.

## Tests

Lib-level tests cover:

- `MasterClockHandle`: construction, lock-state, kind-tag-preservation,
  clamped lipsync, one-shot degraded-warning marker.
- `PcrPll`: convergence on perfect 25 Hz cadence within 5 s, drift
  tracking (100 ppm fast source converges to +100 ppm offset),
  discontinuity filter, modulus wrap, p99 jitter bound, pre-sample
  monotonic fallback.
- `AvSyncPacer`: wallclock pacer always locked, PCR emit trails master
  by pre-roll, modular wrap when master_now < pre-roll, legacy
  fallback parity.
- `PcrIngressSampler`: raw-TS sampling, RTP header skip with CSRC +
  extension, no-sync-byte payload silently dropped.
- `PtpMasterClock`: unavailable defaults, telemetry kind tag.

Run: `cargo test --features video-encoders-full`. All 993 lib tests
pass with the master-clock work in.

## Known limitations

- **AudioMaster** (ALSA local-display master) is reserved but not
  implemented; the kind tag falls through to Wallclock.
- **Lipsync trim** applies to the PES PTS rewriter
  (`engine::ts_pts_rewriter`) and the audio replacer
  (`TsAudioReplacer::set_av_sync_pacer`); it is **not** yet applied
  to the transcoded video replacer's output PTS values.
- **PCR pre-roll** is hard-coded at 80 ms; a future enhancement could
  expose it per-flow for low-latency contribution where 40 ms is
  preferable.
- **PCR bytes in passthrough TS** are not rewritten by
  `engine::ts_pts_rewriter` — only PES PTS/DTS. PCR continues to
  ride the source bytes through to the per-output wire pacer (which
  paces the wallclock egress correctly regardless). For passthrough
  flows where the source PCR-PTS delta is consistent, this is fine;
  for flows where rewriting both PCR and PES PTS would tighten the
  delta further, a follow-up could add a PCR-rewrite stage to the
  rewriter.

## See also

- [`wire-pacing.md`](wire-pacing.md) — PCR-anchored / PTP-raster-anchored
  kernel-paced wire emission. Master-clock generates the PCR values
  inside the bitstream; wire pacing ensures those PCR-bearing packets
  hit the wire at the matching wallclock instant. Both are required
  for tier-1 PCR_AC at the receiver.
