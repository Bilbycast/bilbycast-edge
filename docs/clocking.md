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

**The default policy is now `auto` (by flow role).** A flow with no explicit
`master_clock` resolves identically to `master_clock.kind = "auto"`. The
resolution is keyed on **flow role** (in `build_master_clock`), which
overrides the per-input default table:

| Flow role | `auto` resolves to | Notes |
|-----------|--------------------|-------|
| ST 2110-20/-23/-30/-31/-40, MXL | `Ptp` | PTP-domain essence; `ptp4l` `port_state == SLAVE`/`MASTER`, offset within tolerance |
| Single-source **live contribution**: SRT / RTP / UDP / RIST / RTMP / RTSP | cascade **`SourcePcrPll` → `Ptp` → `Wallclock`** | PLL lock criterion: PI loop converges, p99 jitter < 100 µs over 64 samples after ≥ 100 samples; on failure → PTP-if-configured-&-healthy, else wallclock |
| PID-bus assembled flows | cascade (same as live contribution) | the PLL recovers the designated `pcr_source` |
| **Multi-input switcher** (more than one `input_id`) | `Wallclock` | stable clock keeps cuts seamless; genlocking to the active source would step the clock on every cut and re-acquire lock for seconds |
| File / `media_player` / `replay` / WebRTC / `test_pattern` / `rtp_audio` / `bonded` | `Wallclock` | no live source clock to recover; the source plays at wallclock rate |
| `AudioMaster` | (reserved) | not yet implemented; falls through to Wallclock |

**The explicit `master_clock.kind` values** an operator can pin (snake_case
on the wire, `MasterClockKindConfig` in `src/config/models.rs`):
`source_pcr_pll`, `contribution`, `ptp`, `audio_master`, `wallclock`,
`auto`, plus two more:

- **`passthrough`** — "this flow doesn't need a recovered source clock".
  Output PCR comes from source bytes (passthrough) or source PTS
  (transcoded); the runtime backend is `wallclock` but no PLL spawns and no
  fallback alarm fires. The implicit default for most
  contribution-to-distribution flows when the operator hasn't pinned
  `source_pcr_pll` / `contribution` explicitly.
- **`sender_timestamp`** (SRT / RIST inputs only) — recover rate from the
  SRT/RIST sender's per-packet timestamp instead of the MPEG-TS PCR sampled
  from the bytes. Useful for internet-contribution paths where SRT's
  latency buffer makes PCR-from-bytes look jittery to the PLL but the
  underlying libsrt `srctime` reflects the sender's clock cleanly.
  **Framework only today** — the srctime extraction is wired through in a
  follow-up. Config validation rejects this kind on a non-SRT / non-RIST
  input.

**Why the cascade tries the PLL first then PTP, not wallclock.** The
PLL often won't lock on contribution sources that carry per-source-restart
PCR discontinuities — `ffmpeg -re -stream_loop -1 -c copy` on a 30-second
file, looping playout, SCTE-35 splices, encoder restarts. Once the PLL
has failed, source-tracking is already lost, so the fallback prefers the
node's PTP clock (clean, cross-edge-coherent) over bare wallclock, with
`Wallclock` only as the always-locked floor. The encoder-style PES PTS
regenerators (`engine::ts_pts_rewriter`, `TsAudioReplacer::set_av_sync_pacer`)
anchor against whichever rung is active.

Operators who run on PTP-disciplined or clean-PCR contribution
sources and want cross-edge coherence opt in to the PLL via the new
`master_clock.kind = "contribution"` (preferred — flags intent in
telemetry) or the legacy `master_clock.kind = "source_pcr_pll"`
(retained for back-compat with existing deployments).

Operators can pin a master per-flow with the `master_clock` field in
flow config (overrides the auto-policy). Useful when a plant has PTP
everywhere and the operator wants every flow paced against the
grandmaster regardless of input type.

**Auto by flow role: `master_clock.kind = "auto"`.** A config-level policy
rather than a runtime kind of its own — it picks the right reference for
the flow's role. **ST 2110 / MXL** inputs are PTP-domain essence, so they
resolve straight to **PTP** (no PLL-first). A **single-source live
contribution** flow (SRT/RTP/UDP/RIST/RTMP/RTSP) and a **PID-bus
assembled** flow get the cascade **source PCR PLL → PTP → Wallclock**; a
**multi-input switcher** and **file / replay / WebRTC / test-pattern**
flows resolve straight to **Wallclock** (no genlock to step on cuts / no
live source clock to recover). The cascade
first runs the source-PCR PLL against the selected
input (best — output tracks the source clock, zero source-relative
drift). If the PLL can't lock within `pll_lock_timeout_s` (default 30 s),
the fallback watcher drops to the **PTP** rung when the node has a PTP
role configured (`ptp.conf` mode != off) **and** PTP is healthy (slave
`Locked`, or this node is the grandmaster) — a clean, cross-edge-coherent
reference (genlock-free 2022-7). Otherwise it drops to **wallclock**, the
always-available floor.

The PTP-vs-wallclock choice is **latched** when the PLL gives up, and the
data path only ever **demotes one-way** (PTP → wallclock) if PTP later
loses lock — so the output never paces PCR off an unlocked, undisciplined
`CLOCK_REALTIME`, and never oscillates epochs. The PTP rung polls the
node's `ptp.conf` domain, not the flow's SDP `clock_domain`. If the PLL
re-locks at any point, the master self-heals back to the PLL (best rung).

Telemetry reports `configured_kind = "auto"` with `kind` = the active
rung, so the manager UI renders "Auto → Source PCR PLL" / "Auto → PTP" /
"Auto → Wallclock". This order suits **contribution** workflows: prefer
to track the source, use PTP as a clean catch when the source clock is
unrecoverable, wallclock only as the last resort. Caveats: mid-run PTP
loss demotes to wallclock (a PCR discontinuity, but safe); and the PTP
rung buys cross-edge / absolute coherence, **not** drift versus an
un-genlocked source — for true source-rate tracking the PLL must lock, or
the source must be genlocked.

## Encoder-style PES PTS regeneration

Every TS-carrying ingress runs byte-level PES PTS/DTS regeneration in
muxer mode **by default**; the per-input `passthrough_clock: Option<bool>`
config field set to `true` opts OUT (emits source PCR/PTS bytes
unchanged — relay / transparent-forwarder mode). In muxer mode the
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
`anchor_target` 10 s safety lets it. On a flow with a `Wallclock`
master (switcher / file / replay / WebRTC / test-pattern) and a
typical encoder-relative source PTS, the safety triggers and the
anchor falls back to source PTS — effectively a no-op (output
PTS == source PTS). To actually see
master-clock-derived PTS output, the flow needs either:

- `master_clock.kind = "ptp"` with PTP-disciplined sources, OR
- `master_clock.kind = "contribution"` (or legacy `source_pcr_pll`)
  AND the PLL has locked.

This matches the existing pre-rewriter behaviour for the safe path,
and unlocks the master-clock anchor path for the genuinely
clock-coherent configurations where it improves output quality.

**Which PIDs are rewritten.** Every PES-bearing ES learned from the
PMT — audio, video, AND other PES carriers (DVB teletext, DVB
subtitles, KLV metadata, ST 2038 ANC, DSM-CC PES). Once the PCR has
been re-anchored to the master clock, *any* PES timestamp left in the
source timebase is dead on arrival downstream, so partial coverage is
not an option. Roles are resolved from the PMT `stream_type` **plus
the ES-info descriptor loop** (`ts_parse::descriptor_audio_kind`):
DVB-style audio carried as `stream_type 0x06` + AC-3 (0x6A) /
E-AC-3 (0x7A) / AAC (0x7C) / DTS (0x7B) / registration descriptor is
classified **audio** and receives the lipsync trim, exactly like its
ATSC (0x81/0x87) siblings; a 0x06 ES without an audio descriptor
(teletext, subtitles, KLV) is re-anchored without lipsync.
Section-carrying stream types (0x05 private sections, 0x0A–0x0D
DSM-CC, 0x86 SCTE-35) are never PES-rewritten — SCTE-35 timing is
handled by the dedicated `pts_adjustment` rewrite. Before 2026-06-05
only bare-`stream_type` audio/video was rewritten, which left
DVB-0x06 AC-3 PES at source PTS hours away from the regenerated PCR —
silent audio on every compliant receiver (the "Network TEN" bug).

## Module map

| Module | What it does |
|--------|--------------|
| `engine/master_clock.rs` | The `MasterClock` trait, `MasterClockKind` enum, `MasterClockHandle` (Arc + tag + clamped lipsync trim), `WallclockMaster`, `SourcePcrPllMaster`, `PtpMasterClock`, and the auto-select policy |
| `engine/pcr_pll.rs` | Software PI-controller PLL recovering source's 27 MHz from incoming PCR samples. PI loop on `(Δpcr_ticks, Δwall_ns)` with re-anchor on every accepted sample. Discontinuity filter mirrors `pcr_trust.rs` (gaps > 500 ms reset the anchor). Sticky lock-state hysteresis (enter at p99 < 100 µs, exit at > 500 µs). `now_27mhz(wall_ns)` projects forward from the anchor at the recovered rate so PCR generation never quantises to the ingress PCR cadence. |
| `engine/pcr_ingress_sampler.rs` | Per-flow ingress PCR sampler. Sibling broadcast subscriber (drop-on-Lagged) that scans every `RtpPacket` for adaptation-field PCRs and feeds the master's PLL. Handles both raw TS and RTP-wrapped TS via best-effort RTP header skip. Passive observer — never blocks the data path. |
| `engine/av_sync_mux.rs` | `AvSyncPacer` — thin wrapper around `MasterClockHandle` that exposes `pcr_27mhz_for_emit()` (master_now − PCR_PREROLL_27MHZ, modular-aware), `is_locked()`, and the lipsync trim. Plus `pcr_for_emit(pacer, pts)` helper that prefers the pacer when set, falls back to legacy `pts × 300 − preroll` otherwise. |
| `engine/ts_pts_rewriter.rs` | Encoder-style byte-level PES PTS/DTS rewriter, per-PID anchor + source-delta model. Plugs into `input_post_process::InputPostProcess` as a fourth optional stage; on by default (muxer mode) unless per-input `passthrough_clock: true` opts out, plus an attached `AvSyncPacer`. See the "Encoder-style PES PTS regeneration" section above for the model. |
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

## Local PTP grandmaster for testing

ST 2110 and MXL flows refuse to start without a working `ptp4l` —
`PtpStateReporter` reads `/var/run/ptp4l` and the master clock fails to
lock if no grandmaster is present. For development on a workstation
with no broadcast PTP fabric on the wire, the `ptp-gm/` directory at
the monorepo root contains a small helper that runs `ptp4l` (and
`phc2sys` on HW-PTP NICs) as a free-running grandmaster:

```
ptp-gm/
├── bilbycast-ptp-gm.conf   # ptp4l GM config (SMPTE ST 2059-2 timings, domain 127)
├── bilbycast-ptp-gm.sh     # start | stop | status | restart | logs | help
└── README.md               # how-to + tier comparison + troubleshooting
```

It exists so that:

- ST 2110 + MXL bring-up tests work without a hardware PTP grandmaster
  on a separate machine.
- TS flows on the default `master_clock.kind = "wallclock"` get
  NIC-disciplined CLOCK_REALTIME for free when the script is in HW
  mode — `phc2sys -a -r -r` keeps the system clock locked to the NIC
  PHC, so `engine::wire_emit`'s CLOCK_TAI pacing and PCR generation
  inherit that stability.
- Cross-host PCR_AC and 2022-7 hitless measurements have a deterministic
  shared time source even when neither host has GPS.

Tiers (auto-picked from the NIC):

| Tier | Use when | Floor | bilbycast lock |
|---|---|---|---|
| A (software) | proving the master-clock / ST 2110 / MXL code works | ~tens of µs | yes — `PtpStateReporter` reads `/var/run/ptp4l` regardless of timestamping mode |
| B (HW PHC)   | publishing sub-µs claims, narrow VRX, tier-1 PCR_AC | < 1 µs | yes — and `phc2sys` extends the discipline to CLOCK_REALTIME/CLOCK_TAI so TS-wallclock flows benefit too |
| C (GPS)      | compliance demos, UTC traceability | < 1 µs UTC | same — only changes `clockClass` advertised |

Quick start (the script lives outside `bilbycast-edge/`; this doc just
points at it):

```bash
sudo /path/to/monorepo/ptp-gm/bilbycast-ptp-gm.sh start
/path/to/monorepo/ptp-gm/bilbycast-ptp-gm.sh status
```

The full how-to, NIC auto-pick rules, chrony coexistence notes, and
cross-host setup are in `ptp-gm/README.md`. The helper is **for
testing only** — production deployments run `ptp4l` + `phc2sys` from a
distribution package or vendor-supplied unit, locked to a real
grandmaster (Meinberg, ESI, FsPro, or similar).

## See also

- [`wire-pacing.md`](wire-pacing.md) — PCR-anchored / PTP-raster-anchored
  kernel-paced wire emission. Master-clock generates the PCR values
  inside the bitstream; wire pacing ensures those PCR-bearing packets
  hit the wire at the matching wallclock instant. Both are required
  for tier-1 PCR_AC at the receiver.
- [`../../ptp-gm/README.md`](../../ptp-gm/README.md) — local PTP
  grandmaster helper used during development and testbed runs.
- [`../packaging/setup-etf-qdisc.sh`](../packaging/setup-etf-qdisc.sh)
  and [`../packaging/bilbycast-etf-qdisc@.service`](../packaging/bilbycast-etf-qdisc@.service) —
  egress-side ETF qdisc setup for SO_TXTIME wire pacing. Independent of
  the GM helper; production-only, and the userspace
  `clock_nanosleep(CLOCK_TAI)` tier is the default elsewhere.
