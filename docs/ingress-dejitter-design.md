# Ingress de-jitter — design (2026-06-02)

The ingress counterpart to [`egress-dejitter-design.md`](egress-dejitter-design.md).
Same closed-loop servo, applied at the **input** instead of the wire.

## Problem

The egress session added a release-rate servo + residence-cap shed in
`engine::wire_emit` so a compressed UDP/RTP **output** never lets a
source-rate-vs-wallclock offset or burst integrate into latency. It did
**not** add an input counterpart. The only per-input buffer that existed —
the `Delayed` mode of `engine::ingress_publisher::IngressPublisher`, driven
by the `ingress_delay_ms` knob (renamed from the former
`ingress_smoothing_ms`) — is a **pure delay line**: it releases each packet
at `recv_time_us + ingress_delay_ms`, which reproduces the inter-arrival
spacing exactly. It shifts the timeline; it does **not** remove packet-delay
variation (PDV). (It's a cross-device-sync alignment tool, and labelled as
such.)

So raw UDP / RTP inputs — the only TS-carrying inputs with no
transport-layer de-jitter — fan their network jitter straight into every
broadcast-channel consumer: the TR-101290 + content analysers, the PCR
PLL sampler (degrading lock on `contribution` / `source_pcr_pll` master
clocks), the PID-bus demuxer/assembler, the replay recorder, and the
protocol-paced outputs (SRT / RIST) that read the broadcast channel and
emit at whatever cadence they read it.

## Which inputs benefit (and which must be skipped)

Wired on **raw UDP** and **raw RTP** (single-leg + SMPTE 2022-7 dual-leg).
Everything else either has its own transport de-jitter or must not be
media-rate re-paced:

| Input | Transport de-jitter today | Verdict |
|-------|---------------------------|---------|
| **UDP** (`input_udp`) | none | **de-jitter** (primary) |
| **RTP** (`input_rtp`, incl. 2022-7) | none | **de-jitter** |
| SRT (`input_srt`) | libsrt TSBPD | skip (warns on double-buffer) |
| RIST (`input_rist`) | `RistSocket` NACK + reorder buffer | skip |
| WebRTC | str0m jitter buffer | skip |
| RTMP / RTSP | TCP / retina (locally-synthesised TS clock) | skip |
| ST 2110-* / MXL | PTP raster-timed, uncompressed | **must not** re-pace |
| media_player / replay / test_pattern / bonded | locally paced / own scheduler | skip |

This is exactly the egress servo's scope (compressed UDP/RTP), the right
symmetry.

## How it works (reuse the proven servo, don't rebuild)

`engine::ingress_dejitter` mirrors `wire_emit::TargetState::derive_target_servo`
in the microsecond domain, driving the `IngressPublisher`'s drainer task:

1. **Recover the nominal source rate** from inter-PCR observations — the
   same EMA, PID-locked via `ts_parse::first_pcr_in_ts_buffer_pid` (every
   MPTS program has its own 27 MHz clock; we measure one program's
   cadence, not the cross-program average). RTP-wrapped packets have their
   header skipped via `input_transcode::rtp_header_length`; raw-TS scan
   from byte 0. PCR is used **only** to measure the rate — pacing is by
   byte/rate, and the PCR bytes are never rewritten (the receiver
   re-clocks from them exactly as before).
2. **Leaky-bucket release** at that rate, trimmed `±authority` (‰) by the
   buffer-fill error so the queue is held at `setpoint_ms` of content. A
   steady source-rate-vs-wallclock offset is absorbed as a steady rate
   trim — no accumulation, no loss, no integrator for a runaway.
3. **Residence-cap shed** bounds latency by construction: a packet older
   than the cap (and the stale backlog behind it) is dropped, the pacing
   re-anchored, `ingress_dejitter_shed` bumped. The downstream re-clocks
   from PCR exactly as it would after any network loss.

`recv_time_us` is re-stamped to the release instant on the way out, so the
input buffer and the egress buffer each measure their own residence — the
egress shed (which keys off `recv_time_us`) never double-counts this
buffer's hold toward a false shed. `sequence_number` / `rtp_timestamp` are
untouched.

## Cooperation with SMPTE 2022-7

On a 2022-7 dual-leg RTP input the de-jitter sits **after** the
`redundancy::merger::BufferedHitlessMerger` in the pipeline
(`recv → filters → FEC → merge → transcode/post → publisher → broadcast`).
The merger de-duplicates + gap-fills in seq order and drains in **bursts**
at its `path_differential_ms` deadline; the de-jitter then re-paces that
bursty drain to the recovered rate. They are complementary:

- merger `path_differential_ms` absorbs **inter-leg skew**,
- de-jitter `ingress_dejitter_ms` absorbs **residual PDV / rate offset**.

Total added latency is the sum of the two — the documented cost of running
both. The de-jitter buffer is FIFO, so it preserves the merger's
strict-monotonic seq-order output; it never reorders.

**The residence cap must not double-count the merger hold.** `recv_time_us`
is stamped at socket-arrival (pre-merge) and the merger preserves it across
its `path_differential_ms` hold. The de-jitter therefore **re-stamps
`recv_time_us` to the buffer-entry instant** so its residence cap measures
only its own queue, not `merger_hold + dejitter_hold`. Without that re-stamp
a satellite-class `path_differential_ms` (200–500 ms) would exceed the
250 ms cap on the very first check and trigger a continuous false shed storm
— the egress design's exact failure mode, inverted onto ingress. With the
entry re-stamp the two stages are independent: the merger's hold is a fixed,
bounded latency, and only the de-jitter queue's own residence lives under
the cap.

## Defaults (justified)

- **setpoint 60 ms** — same as egress; one inter-PCR window plus headroom,
  inside any pro receiver's T-STD envelope. Precedence: per-input
  `ingress_dejitter_ms` > `BILBYCAST_INGRESS_BUFFER_MS` env > 60 ms,
  clamped [20, 2000] ms.
- **residence cap `max(4×setpoint, 250)` ms** (env `BILBYCAST_INGRESS_RESIDENCE_MS`),
  so a bigger buffer gets proportionally more burst headroom before the
  hard shed.
- **authority ±5 %** — enough to absorb any realistic source-vs-CLOCK
  offset, small enough that the induced PCR jitter stays inside the
  receiver T-STD.
- **default OFF.** A passthrough UDP→UDP flow whose only consumer is one
  egress-de-jittered output gets no benefit from also de-jittering the
  input (the egress servo already re-paces the wire) — it just adds
  latency. The value is for the *other* consumers (analysers, PCR PLL,
  PID-bus, SRT/RIST outputs). Operators opt in per-input based on their
  consumer mix. De-jitter supersedes `ingress_delay_ms` if both are
  set (warned at startup).

## Telemetry + protocol (first-class, capability-gated)

- Capability `ingress_dejitter` on `HealthPayload.capabilities`. **No
  `WS_PROTOCOL_VERSION` bump** — additive + capability-gated, per the
  `egress_dejitter` precedent.
- `InputStats.ingress_dejitter_shed` (u64, skip-if-0) + `ingress_buffer_depth`
  (`Option<u64>`) — flow-level atomics surfaced on the active input, the
  same scope as `redundancy_switches` / `buffered_hitless`.
- Per-input `ingress_dejitter_ms: Option<u32>` on UDP + RTP `InputConfig`
  (20–2000 ms, `validate_ingress_dejitter`).
- Manager mirrors all of it in `device-edge` (`flow.rs` + `validation.rs`);
  UI: `config/inputs.js` knob gated `.ingress-dejitter-only` via
  `flows.js applyNodeCapabilities`; `detail/flows.js` renders
  De-jitter Buffer / De-jitter Shed on the input stat panel.

## Test matrix (per-gate PASS/FAIL/NOT RUN — never aggregate)

See [`../../testbed/input-dejitter-validation-2026-06-02/README.md`](../../testbed/input-dejitter-validation-2026-06-02/README.md).
Honest gates (per the root `CLAUDE.md` "Verifying Broadcast Quality"):

1. **Wallclock rate** — out rate == in rate ±0.1 % with de-jitter on.
2. **Decode round-trip** — output decodes clean, ≥2 decoders, 0 errors.
3. **PDV reduction** — downstream inter-packet jitter materially lower
   with de-jitter on than off, under an injected-jitter source.
5. **PCR-wallclock drift** — residual jitter falls with de-jitter on.
9. **Input-switch robustness** — anchor_pid re-locks on switch.
   **2022-7 cooperation** — hitless still hitless with de-jitter engaged;
   `buffered_hitless.late_dropped` not worsened; the merger's seq order
   preserved.
11. **Capture hygiene** — 32 MB `SO_RCVBUF` on every capture socket.

Unit coverage shipped (`engine::ingress_dejitter::tests`): PCR decode +
PID lock, rate recovery from inter-PCR, cold-start ASAP, fill-responsive
pacing, drained-buffer floor, shed re-anchor (rate + PID survive),
discontinuity keeps rate, config clamp/cap derivation. Plus
`config::validation` range tests.
