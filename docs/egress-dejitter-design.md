# Egress de-jitter / re-clock — design (2026-06-02)

> ⚠️ **SUPERSEDED DESIGN SNAPSHOT (2026-06-02).** This is the original
> design note, kept for rationale + research citations. The **shipped code
> diverged** in the 2026-06-04 rework — read it for the *why*, not the *what
> ships*. Specifically (verify against `src/engine/wire_emit.rs`):
>
> - **The cushion is now OPT-IN.** With `egress_buffer_ms` (per-output) /
>   `BILBYCAST_EGRESS_BUFFER_MS` unset, `seed_cushion = false`: the servo
>   emits byte-for-byte on arrival with only a gentle rate trim and **no 60 ms
>   startup cushion** — zero added latency, zero regression. The 60 ms
>   setpoint cushion is seeded **only** when an operator sets one of those
>   knobs. (Design §"The fix" / §"Defaults" assumed the cushion was always on.)
> - **The Lossless→`clock_nanosleep` / ST 2110→SO_TXTIME class split was
>   reverted.** When SO_TXTIME is enabled, **both** `Lossless` and
>   `EtfEligible` ride etf / SO_TXTIME; late etf packets are handled by the
>   `ETF_LATE_FLOOR_NS` floor (15 ms), not a class split. The **default**
>   release path is `clock_nanosleep` (SO_TXTIME is opt-in via
>   `BILBYCAST_ENABLE_TXTIME`). See [`wire-pacing.md`](wire-pacing.md).
> - **The residence-cap shed fires on `dejitter.enabled` regardless of
>   releaser**, and `dejitter.enabled` is **Lossless-only** (`want_egress_servo
>   = matches!(pacing, WirePacingClass::Lossless)`).
>
> The **numeric defaults below still match** the shipped code: 60 ms setpoint,
> ±5 % authority (`authority_permille = 50`), 32-datagram drain floor,
> residence cap `max(4×setpoint, 1000)` ms. For the shipped ingress
> counterpart, [`ingress-dejitter-design.md`](ingress-dejitter-design.md) is
> accurate and current.
>
> **2026-06-05 update — pacing model + config surface.** The shipped default
> for compressed UDP/RTP is now **forward-cadence** (emit at input cadence,
> no re-pacing; the residence cap stays armed as the runaway backstop) —
> re-pacing held I-frame bursts → GOP-cadence latency swing → receiver
> underrun on a clean SRT feed. The servo described in this note and the
> open-loop PCR-delta pacer survive as **per-output opt-ins** via the
> manager-configurable `egress_pacing: "forward" | "pcr" | "servo"` config
> field; `egress_buffer_ms` (the cushion) is only valid with `"servo"`.
> The `BILBYCAST_EGRESS_PACING` / `BILBYCAST_EGRESS_BUFFER_MS` /
> `BILBYCAST_EGRESS_RESIDENCE_MS` env vars are **gone** — behavior knobs
> live in config, not the environment. Capability `egress_pacing` gates the
> manager UI dropdown.

## Problem
The edge's egress pacer (`engine::wire_emit::TargetState::derive_target_raw`) advances its
output cadence purely by **source-PCR deltas** against a **wall anchor frozen at startup**, with
**no term comparing the internal buffer fill or wallclock rate**. So any sub-% source-PCR-vs-
`CLOCK_TAI` rate offset (or burst) integrates into the wire queue with nothing correcting it:
- `clock_nanosleep` (compressed) → unbounded buffering → the observed **47 s runaway**.
- `etf` (the pre-fix path) → late-packet **drops** → stutter.

This is an **open-loop** pacer where broadcast gear uses a **closed loop**.

## How professional gear copes (researched, cited)
A de-jitter buffer kept **centred at ~50%** whose **fill level feeds back into the output rate**:
- Pro de-jitter buffer ~**120 ms run at 50% → 60 ms** latency (US10313276).
- Remux/gateway channel **300 ms, target D=150 ms** (half-full); over/underflow **re-centres**
  to mid-jitter-range + adjusts output time base (US20060146815A1).
- PCR clock-recovery PLL recovers the 27 MHz STC; freq correction clamped **±30 ppm**, slew
  **0.075 Hz/s** (matches ETR 290 PCR_FO/PCR_DR). The **receiver** owns this for compressed —
  T-STD absorbs ~0.7 s and re-clocks from PCR, so the **sender need not wire-precise-pace
  compressed TS**. (Strict ±500 ns wire pacing is only required for ST 2110 uncompressed.)

## The edge already has the pieces (reuse, don't rebuild)
- `pcr_pll.rs` — a real PI loop (KP=0.10, KI=0.005, clamp ±500 ppm, lock hysteresis) recovering
  source rate; `now_27mhz()` is a continuously-advancing recovered clock. (Ingress only today.)
- `TargetState.observed_rate_bps` EMA + `prev_interval_bytes/ns` — the recovered **average source
  rate**, currently telemetry-only.
- `WireTxReceiver.depth` (AtomicUsize) — exact **queue depth**, read for diag only.
- `record_latency()` → `last_latency_us` — the **true residence** (now − recv_time) per send.
- NULL-pad primitive (`TsContinuityFixer`, 250 ms idle) for underflow.

## The fix — bounded de-jitter + release-rate servo (compressed / `WirePacingClass::Lossless` only)
Replace the open-loop PCR integration with a **leaky-bucket release servo + hard residence cap**:
1. **Nominal rate** = recovered average source rate (`observed_rate_bps`, promoted to always-update).
2. **Buffer-level trim** (the missing feedback): `err=(depth−setpoint)/setpoint` (clamp ±1);
   `release = nominal·(1 + 0.05·err)` → ±5 % authority, slow integral, holds the centre.
3. **Pace** each datagram as a true leaky bucket from `last_returned` at `release` (no frozen
   anchor, no source-PCR integration → a steady ppm offset has no integrator to accumulate).
4. **Underflow floor** `target.max(now)` (ASAP when drained).
5. **Hard residence cap** (controlled overflow, IRD-style): when residence (`now − oldest
   recv_time`, = `last_latency_us`) exceeds the cap, **shed oldest** datagrams (count them) and
   snap forward — bounds latency by construction. The receiver re-clocks from the untouched PCR.
   *(Shipped: the cap is `max(4×setpoint, 250)` ms — 250 ms only at the 60 ms default setpoint —
   and the shed fires on `dejitter.enabled` regardless of releaser, not just on the
   `clock_nanosleep` path.)*

ST 2110 (`EtfEligible`) keeps `derive_target_raw` + SO_TXTIME/etf **unchanged** (wire precision IS
its clock). *(Shipped: the residence-cap shed is **Lossless-only** — `dejitter.enabled` is gated by
`want_egress_servo = matches!(pacing, WirePacingClass::Lossless)`, so ST 2110 does **not** get it.)*

## Defaults (justified)
| Knob | Default | Basis |
|---|---|---|
| setpoint (centre) | 60 ms of content | US10313276 (120 ms @ 50%) |
| residence cap | 250 ms | ~2× setpoint, inside T-STD (≤0.7 s) + SRT RCVLATENCY headroom |
| rate authority | ±5 % | converges <1 s; receiver T-STD absorbs the induced PCR_OJ |
Setpoint in datagrams computed live from `observed_rate_bps` (~34 dgms @6 Mbps). The deep
8192-slot wire channel **stays** as the transient burst reservoir; the servo holds occupancy far
below it. Codec backpressure (75 %) and SRT/RIST input buffers are unchanged + complementary.

## Universal coverage (all input/output types)
- **Outputs:** UDP/RTP go through `wire_emit` → get the servo + cap. **SRT/RIST outputs** pace via
  their own protocol layer (TSBPD / RIST buffer) → already receiver-buffered, correct as-is.
  **HLS/CMAF/WebRTC** are segment/RTP-paced by their own stacks. **ST 2110** strict + safety net.
- **Inputs:** SRT/RIST already de-jitter at ingress (TSBPD/buffer). RTP/UDP/MPEG-TS/media_player
  feed the broadcast channel; the egress servo bounds end-to-end latency regardless of input type,
  so the fix is **input-type-agnostic**. (Optional follow-up: an ingress de-jitter for raw RTP/UDP
  using the existing `pcr_pll` for parity with SRT/RIST.)

## Manager UI + protocol (first-class)
- **Config:** per-flow `egress_buffer_ms` (target/centre) + optional per-output override; validated
  bounds. New `Option<WireDejitterConfig>` (back-compat `#[serde(default)]`).
- **Telemetry (FlowStats/OutputStats → HealthPayload):** `wire_emit_depth`, `wire_emit_residence_us`,
  `wire_emit_shed`, clock-lock state — surfaced in a manager UI card, **capability-gated**
  (`"egress_dejitter"`) so older nodes hide it.
- Wire on **edge + manager + WS protocol** (+ relay/gateways where applicable), bump
  `WS_PROTOCOL_VERSION`, per the protocol-change rule.

## Phasing
- **Phase 1 (safety net, stops the runaway today):** residence-cap shed in the drain loop,
  Lossless-only by construction (additive, no config/signature change). Caps latency → the 47 s
  becomes impossible. Strict subset of the full design (no throwaway). *(Shipped: the shed is gated
  on `dejitter.enabled`, which is Lossless-only via `want_egress_servo`, and fires regardless of
  the active releaser — not scoped to the `clock_nanosleep` path as written here.)*
- **Phase 2 (elegant servo):** the release-rate servo (§ fix) so the shed never fires under normal
  jitter; reuse `observed_rate_bps` + `depth`.
- **Phase 3 (UI/protocol):** config knob + telemetry card on edge + manager.

## Test matrix (per-gate PASS/FAIL/NOT RUN; never aggregate)
Unit: servo holds setpoint; bounds runaway on +200 ppm source; no-starve on −200 ppm; sheds on
sustained overflow; ST 2110 path byte-identical; input-switch recovers.
Integration (testbed, honest gates): `tc netem delay 30ms 20ms` jitter + `netem` bursts + an
off-rate `ffmpeg -re` sender, ≥30 min: zero drops + residence ≤250 ms flat (no monotonic climb) +
output rate = source ±0.1 % (wallclock denom, Gate 1); runaway contrast (old binary climbs, new
flat); honest audio−video ≤±40 ms + SNR>50 dB (Gates 3/6); input-switch <200 ms (Gate 9); ST 2110
PCR_AC tier-1 unchanged (Gate 4). 32 MB SO_RCVBUF captures (Gate 11).

Research basis: US10313276, US20060146815A1, ETSI TR 101 290, ISO/IEC 13818-1 T-STD, Haivision SRT
TSBPD/DriftTracer. Full findings: workflow `dejitter-reclock-design` (2026-06-02).
