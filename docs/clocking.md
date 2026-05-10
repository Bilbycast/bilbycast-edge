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
| `SourcePcrPll`     | SRT / RTP / UDP / RIST / RTMP / RTSP carrying TS, plus `media_player` / `replay` (synthesised PCR) | PI loop converges, p99 jitter < 100 µs over 64-sample window after ≥ 100 samples |
| `Ptp`              | ST 2110-20/-23/-30/-31/-40 inputs                  | `ptp4l` reports `port_state == SLAVE` and offset within tolerance |
| `AudioMaster`      | (Reserved for future ALSA-master local-display path) | (Not yet implemented; falls through to Wallclock) |
| `Wallclock`        | WebRTC ingress, no active input, or operator override | Always locked — no convergence concept     |

Operators can pin a master per-flow with the `master_clock` field in
flow config (overrides the auto-policy). Useful when a plant has PTP
everywhere and the operator wants every flow paced against the
grandmaster regardless of input type.

## Module map

| Module | What it does |
|--------|--------------|
| `engine/master_clock.rs` | The `MasterClock` trait, `MasterClockKind` enum, `MasterClockHandle` (Arc + tag + clamped lipsync trim), `WallclockMaster`, `SourcePcrPllMaster`, `PtpMasterClock`, and the auto-select policy |
| `engine/pcr_pll.rs` | Software PI-controller PLL recovering source's 27 MHz from incoming PCR samples. PI loop on `(Δpcr_ticks, Δwall_ns)` with re-anchor on every accepted sample. Discontinuity filter mirrors `pcr_trust.rs` (gaps > 500 ms reset the anchor). Sticky lock-state hysteresis (enter at p99 < 100 µs, exit at > 500 µs). `now_27mhz(wall_ns)` projects forward from the anchor at the recovered rate so PCR generation never quantises to the ingress PCR cadence. |
| `engine/pcr_ingress_sampler.rs` | Per-flow ingress PCR sampler. Sibling broadcast subscriber (drop-on-Lagged) that scans every `RtpPacket` for adaptation-field PCRs and feeds the master's PLL. Handles both raw TS and RTP-wrapped TS via best-effort RTP header skip. Passive observer — never blocks the data path. |
| `engine/av_sync_mux.rs` | `AvSyncPacer` — thin wrapper around `MasterClockHandle` that exposes `pcr_27mhz_for_emit()` (master_now − PCR_PREROLL_27MHZ, modular-aware), `is_locked()`, and the lipsync trim. Plus `pcr_for_emit(pacer, pts)` helper that prefers the pacer when set, falls back to legacy `pts × 300 − preroll` otherwise. |
| `stats/pcr_trust.rs` | Per-output egress PCR accuracy sampler (4096-sample rotating reservoir, exact percentiles). Sibling consumer of the same PCR sample stream as the ingress PLL, but on the egress side. |
| `engine/wire_emit.rs` | Per-output PCR-anchored wire emission engine. Dedicated `std::thread` (Linux: `SCHED_FIFO` best-effort priority 50) pops TS datagrams off a `std::sync::mpsc::sync_channel(1024)` fed by the encoder task. Two release tiers, picked per-output at spawn time: (1) **SO_TXTIME** — kernel-paced via the `etf` qdisc on `CLOCK_TAI`; sub-µs jitter when paired with HW-PTP, ~1–10 µs with software ETF. (2) **`clock_nanosleep(CLOCK_TAI, TIMER_ABSTIME)` fallback** when SO_TXTIME setsockopt fails. Closed-loop on observed inter-PCR rate (no declared-bitrate parameter — open-loop drifts when the encoder runs above/below its configured target). Discontinuity > 500 ms or any backwards step resets the anchor; a per-emitter monotonic-target guard prevents kernel ETF reorder on PCR discontinuities. **Wired into UDP, RTP (single-leg + FEC + 2022-7 dual-leg), 302M, ST 2110-20/-23/-30/-31/-40.** SRT, RIST, RTMP, HLS, CMAF, WebRTC keep their protocol-layer pacing. Operator escape hatch: `BILBYCAST_FORCE_NANOSLEEP=1` skips the SO_TXTIME probe (diagnostic only). Full doc: [`wire-pacing.md`](wire-pacing.md). |

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

- **Ingress transcoders** (`engine::input_transcode`) build a
  `TsVideoReplacer` for inputs with `video_encode` set (RTSP, WebRTC,
  ST 2110-20/-23, Group A inputs). The pacer setter is in place
  (`InputTranscoder::set_av_sync_pacer`) but the spawn-site plumbing
  to attach it from each input task is a future enhancement. Ingress
  PCR therefore still uses the legacy `pts × 300 − preroll` path —
  output-side replacers re-stamp PCR with master-clocked values
  before egress, so end-to-end this is rarely visible.
- **AudioMaster** (ALSA local-display master) is reserved but not
  implemented; the kind tag falls through to Wallclock.
- **Lipsync trim** is plumbed but not yet applied to output PTS
  values.
- **PCR pre-roll** is hard-coded at 80 ms; a future enhancement could
  expose it per-flow for low-latency contribution where 40 ms is
  preferable.

## See also

- [`wire-pacing.md`](wire-pacing.md) — PCR-anchored / PTP-raster-anchored
  kernel-paced wire emission. Master-clock generates the PCR values
  inside the bitstream; wire pacing ensures those PCR-bearing packets
  hit the wire at the matching wallclock instant. Both are required
  for tier-1 PCR_AC at the receiver.
