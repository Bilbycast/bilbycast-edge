# Wire pacing

Tier-1 broadcast PCR jitter on TS-bearing UDP / RTP outputs, plus the
ST 2110-21 narrow-profile pacing path for uncompressed video.

> **Status (2026-05-09):** Phase 1 wire_emit integration attempt was
> made and reverted same day. Live testbed showed multi-second egress
> latency, channel-overflow drops on passthrough, and TR-101290 P2
> PCR-accuracy errors with the integrated form. UDP / RTP outputs are
> back on the today-stable inline tokio mpsc + `send_to.await` path;
> baseline PCR jitter at the receiver is 5–50 ms (status quo for this
> codebase, tolerated by VLC / ffplay / Appear / Cisco TV / EVS).
> The `wire_emit` module + 12 unit tests are retained in tree under
> `#![allow(dead_code)]`; re-integration requires an integration test
> driving real source feeds + likely a switch to the SO_TXTIME
> executor (Phase 2 below — already in tree, opt-in for ST 2110).
> A prerequisite (master_clock PLL anti-runaway) shipped 2026-05-09 in
> `engine::pcr_pll` and is now sound.

## Why two backends

| Regime | Packet rate | Inter-packet budget | Backend |
|---|---|---|---|
| Compressed TS over UDP / RTP / SRT (~6 Mbps) | ~1 k–10 k pps | ~100 µs–1 ms | `clock_nanosleep` on `SCHED_FIFO` thread (Phase 1) |
| Uncompressed ST 2110-20 (1080p50 ~3 Gbps) | ~250 k pps | ~4 µs | `SO_TXTIME` + ETF qdisc with NIC HW offload (Phase 2) |
| Uncompressed ST 2110-20 (4K60 ~12 Gbps) | ~1 M pps | ~1 µs | `SO_TXTIME` + ETF qdisc with NIC HW offload (Phase 2) |

The Tokio timer wheel resolution is ~1–5 ms on a healthy box, more under
load. `tokio::time::sleep_until` therefore can't deliver the sub-100 µs
PCR cadence professional decoders (Appear, Tektronix) require.
`clock_nanosleep` brings that to ~50–100 µs — fine for compressed TS,
not enough at uncompressed ST 2110 rates where each packet's emit
budget is microseconds and per-packet syscalls would saturate a CPU
core. SO_TXTIME moves the per-packet timing decision into the kernel
(or NIC).

## Architecture: one scheduler, two executors

```
                 ┌───────────────────────────┐
                 │  Scheduler                │
                 │  - PCR / PTP anchor       │
                 │  - Bitrate interpolation  │
                 │  - Late-rebase            │
                 │  - Discontinuity reset    │
                 └─────────────┬─────────────┘
                               │ target_ns per datagram
                ┌──────────────┴──────────────┐
                ▼                             ▼
   ┌─────────────────────┐        ┌──────────────────────┐
   │ Phase 1             │        │ Phase 2              │
   │ clock_nanosleep     │        │ SCM_TXTIME CMSG      │
   │ + send_to           │        │ + sendmmsg → ETF     │
   │ (compressed TS,     │        │ (ST 2110-20/-22,     │
   │  ~kpps regime)      │        │  ~Mpps regime)       │
   └─────────────────────┘        └──────────────────────┘
```

Both executors share the same per-datagram `target_ns` math; what
differs is how the packet is released onto the wire.

# Phase 1 — `engine::wire_emit` (compressed TS)

```
Encoder task (tokio, block_in_place)
   │
   │ try_send(WireDatagram)
   ▼
std::sync::mpsc::sync_channel(1024)
   │
   ▼
wire_emit thread (std::thread, SCHED_FIFO)
   ├─ derive target_ns from datagram PCR + wall_anchor
   ├─ clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, target_ns)
   ├─ optional RFC 2250 RTP wrap
   ├─ blocking socket.send_to
   └─ stats: packets_sent, bytes_sent, record_latency, record_pcr_egress
```

Channel overflow is non-blocking on the encoder side: `try_send`
returns `Full`, the datagram drops, and `packets_dropped` ticks.
Same drop semantic as the broadcast channel's `Lagged` arm — slow
wire never back-pressures the encoder. Cap = 1024 datagrams ≈ 1.7 s
@ 6 Mbps; comfortably absorbs h264_qsv frame-quanta bursts.

## Target derivation

Anchor on the first datagram: `wall_anchor_ns = monotonic_now() +
PREROLL_NS` (50 ms). If the first datagram carries a PCR, `pcr_anchor =
pcr`. The first datagram emits at exactly `wall_anchor_ns`.

For each subsequent datagram:

| Datagram        | Target                                                                |
|-----------------|-----------------------------------------------------------------------|
| **PCR-bearing** | `wall_anchor + (pcr − pcr_anchor) / 27` (ns). Re-anchor: `wall_anchor = target`, `pcr_anchor = pcr`, `bytes_since_anchor = 0`. |
| **No PCR**      | `wall_anchor + bytes_since_anchor × 8 × 1e9 / bitrate_bps`. `bytes_since_anchor` accumulates until the next PCR re-anchor. |

`bitrate_bps` is `declared_bitrate_bps` (passed in by the output —
sourced from `video_encode.bitrate_kbps` × 1000) when set, otherwise an
EMA inferred from inter-PCR observations (1/4 step toward each new
sample, ~10 PCRs to 95 %). Cold-start passthrough with no rate hint
and no inter-PCR observation yet emits at `now` — receivers tolerate
packets-ahead far better than packets-far-behind, and the next PCR
seeds the EMA within ~40 ms.

Re-anchoring on every PCR bounds drift between our wallclock and the
source's PCR clock to one inter-PCR period (~30–40 ms typical).

## Late-rebase

If a PCR-driven target lands more than 5 ms in the past (an encoder
burst pushed us behind), `wall_anchor` snaps to `now`. Without this,
a single late burst pins the anchor permanently behind real time and
every subsequent PCR emits ASAP — a failure mode observed during
development. The cost is a one-time jitter step at the rebase moment,
visible as a single-sample tail on `pcr_trust` rather than a
steady-state degradation.

## Discontinuity

A backwards PCR step or any forward jump > 500 ms (mirroring
`engine::pcr_pll` and `stats::pcr_trust`) resets the anchor to "now"
and resumes from the new PCR.

## Realtime priority

`pthread_setschedparam(SCHED_FIFO, priority=50)` is best-effort.
Failure (typically `EPERM` from missing `RLIMIT_RTPRIO` or
`RestrictRealtime=true`) logs at debug level and the thread continues
at default `SCHED_OTHER`. Two paths to grant it:

1. **Systemd unit (recommended in production):**
   `packaging/bilbycast-edge.service` ships with `RestrictRealtime=false`
   + `LimitRTPRIO=50`. The kernel allows unprivileged
   `sched_setscheduler(SCHED_FIFO)` whenever `RTPRIO` is non-zero, so
   no capability grant is required.
2. **`cargo run` / dev binary:**
   ```bash
   sudo setcap cap_sys_nice+ep target/debug/bilbycast-edge
   # or run as root
   ```

Verify with `ps -eLo pid,tid,class,rtprio,comm | grep bilbycast-edge` —
threads named `wire-emit-<id>` should show `class=FF` and `rtprio=50`.

## Cancellation

`recv_timeout(50 ms)` polls the cancellation token; emitter exits
within ~50 ms of cancel. Channel disconnect (encoder dropped) also
exits the thread cleanly.

## Integration matrix

| Output                          | Wire path                              | Why                                                                                   |
|---------------------------------|----------------------------------------|---------------------------------------------------------------------------------------|
| UDP (all)                       | `wire_emit`, `WireWrap::None`          | UdpOutputConfig has no FEC field; always paced.                                       |
| RTP single-leg, no FEC          | `wire_emit`, `WireWrap::Rtp`           | Common encoded path. Header built inside emitter (random SSRC + monotonic seq).       |
| RTP single-leg with FEC         | Legacy inline tokio wire task          | FEC packets carry a non-MP2T RTP header on the same socket; wire_emit owns the std socket exclusively. |
| RTP 2022-7 dual-leg redundant   | Legacy direct send (no wire task)      | Both legs MUST emit identical RTP seq + SSRC for receiver-side hitless dedup. wire_emit's per-instance random RtpState would diverge them. |
| 302M (UDP `transport_mode: audio_302m`) | Legacy direct send             | Audio-only path; `S302mOutputPipeline` paces by sample-rate already.                  |
| SRT, RIST, HLS, CMAF, RTMP, WebRTC | Protocol-specific (unchanged)        | Either internally paced (SRT, RIST) or fragment-paced (HLS, CMAF) or RTC-paced (WebRTC). |
| ST 2110-20 / -22                | Phase 2 — `SO_TXTIME`                  | Userspace pacing too coarse at 250k+ pps. See Phase 2 below.                          |
| ST 2110-30 / -31 / -40          | Untouched (low-rate, RTP-paced)        | Audio + ANC are RFC-clock paced; clock_nanosleep is sufficient and not yet wired.     |

The dispatch in `output_rtp.rs` is captured by the small `WireSender`
enum in that file — `Paced(SyncSender<WireDatagram>)` vs
`Legacy(tokio::sync::mpsc::Sender<(Bytes, u64, u32)>)` — selected by
`config.fec_encode.is_some()`.

## Verification

End-to-end test plan against the testbed (single edge, clean port —
**not :6010**, the testbed cross-instance pollution gotcha):

1. `cargo test -p bilbycast-edge` — wire_emit's 12 unit tests + RTP
   wrap tests stay green.
2. Run h264_qsv @ 6 Mbps, capture 8 s on a unique port (e.g. 46010):
   `tsp -I ip 46010 -O file capture.ts`
3. `tsp -I file capture.ts -P pcrverify --jitter-max 100 -O drop`.
   **Pass:** ≥ 90 % of PCRs within 100 µs.
4. `tsp -I file capture.ts -P analyze -O drop`. CC errors must remain
   0 on PAT/PMT/SDT/audio; video PID CC errors track socket-drop rate
   only.
5. Visual decode: VLC + ffplay clean, no pixelation. Pixelation =
   channel-cap regression.
6. Stress: scene-change / fireworks clip; confirm
   `OutputStats.packets_dropped` does not accumulate.
7. Passthrough flow (no encoder, no `declared_bitrate_bps`): confirm
   wire_emit's EMA-from-PCR path produces clean output.
8. SCHED_FIFO grant: `ps -eLo pid,tid,class,rtprio,comm | grep bilbycast-edge`
   — wire-emitter threads `FF` class priority 50.

# Phase 2 — `engine::wire_emit_txtime` (uncompressed ST 2110)

> **Status (2026-05-09):** module + per-output config flag + raster
> pacer + capability probe wired in. Disabled by default; opt in per
> output. Operator must install the ETF qdisc separately (see runbook).

ST 2110-20 / -22 narrow profile compliance requires per-packet timing
to within microseconds of the frame raster. At 1080p50 the inter-packet
budget is ~4 µs and at 4K60 it's ~1 µs — `clock_nanosleep` precision
(~50–100 µs under load) cannot hit those targets, and per-packet
syscalls saturate a CPU core well before the bandwidth ceiling.

`SO_TXTIME` + the ETF qdisc let userspace stamp each datagram with a
target egress timestamp the kernel honours at NIC-driver level. With a
NIC supporting hardware tx timestamping (Mellanox CX-6 / CX-7, Intel
E810 / i210), the NIC schedules transmission against its own
PTP-disciplined clock — sub-µs jitter, fully decoupled from CPU
contention. Software ETF (no NIC offload) gets ~1–10 µs depending on
load, which is already an order of magnitude better than userspace
pacing.

## Architecture

```
ST 2110-20 output task
   │
   │ for each packet:
   │   target_ns = pacer.target_for_packet(frame_idx, pkt_in_frame)
   ▼
batch up to ~64 datagrams
   │
   │ sendmmsg(2) with SCM_TXTIME CMSG per datagram
   ▼
ETF qdisc (operator-configured, per egress NIC)
   │
   ▼
NIC HW pacing (if SO_TIMESTAMPING_TX_HARDWARE supported)
```

The pacer (`engine::st2110::pacer::St2110_21Pacer`) computes
`target_tx_time_ns` against the active video frame raster, anchored on
PTP `CLOCK_TAI` if `ptp4l` is running, monotonic with drift
compensation otherwise.

## Opt-in

ST 2110-20 outputs accept an optional `wire_pacing` block:

```json
{
  "type": "st2110_20",
  "id": "video-out-1",
  "wire_pacing": {
    "mode": "tx_time",
    "profile": "narrow"
  },
  ...
}
```

`profile` is one of `narrow` (default — even pacing across the active
video period; the v1 narrow approximation), `narrow_linear` (even
pacing across the whole frame period), or `wide`.

The capability `wire_pacing_txtime` is advertised on
`HealthPayload.capabilities` only when the host successfully opens an
SO_TXTIME-enabled UDP socket at startup. Manager UI gates the dropdown
on this. Older edges, non-Linux hosts, kernels < 4.19, and containers
without the right setsockopt permission all fail the probe and hide
the option.

## Operator runbook

The edge process does NOT install the ETF qdisc itself (qdisc setup
needs `CAP_NET_ADMIN`, deliberately operator-side):

```bash
sudo bash packaging/setup-etf-qdisc.sh enp1s0  # name your egress NIC
```

The script installs:

```bash
tc qdisc add dev "$IF" root mqprio num_tc 3 \
    map 0 0 0 0 1 1 1 1 2 2 2 2 0 0 0 0 \
    queues 1@0 1@1 1@2 hw 0
tc qdisc replace dev "$IF" parent 100:1 etf clockid CLOCK_TAI \
    delta 200000 offload
```

`offload` enables NIC HW pacing on supported drivers (Mellanox CX-6/7,
Intel E810/i210 with PTP4L disciplining the NIC clock). Without it,
software ETF still gives ~1–10 µs jitter.

PTP prerequisite: `ptp4l` + `phc2sys` running, system clock disciplined
to a grandmaster. ST 2110-21 narrow profile is meaningless without a
common PTP reference between sender and receiver.

## Failure modes

| What | What happens |
|---|---|
| Kernel < 4.19 (no SO_TXTIME) | startup probe fails; capability not advertised; output configs that opted in get a `wire_pacing_unavailable` Warning event and fall back to unpaced send. |
| ETF qdisc not installed | `SO_TXTIME` setsockopt succeeds, but the kernel queue is the default pfifo_fast — no actual pacing. Detectable by `tc -s qdisc show dev <if>` showing pfifo not etf. Same observable behavior as today's unpaced ST 2110 path; no regression. |
| `ptp4l` not running | pacer's PTP anchor never updates; falls back to monotonic with drift compensation. Logs a `wire_pacing_no_ptp` Warning at startup. Output still emits but is not GM-aligned; receivers expecting ST 2110-21 narrow may flag VRX overflow. |
| Burst landed in past (`EOVERFLOW` on send) | datagram dropped; `wire_pacing_late` counter increments. Same drop ethos as broadcast `Lagged`. |

## Verification

1. **Build:** `cargo build -p bilbycast-edge` on Linux + macOS; macOS
   path compiles via `#[cfg]` stubs (returns `Unsupported` from the
   probe, capability never advertised).
2. **Capability probe:** start the edge on a Linux box, hit `/health`,
   confirm `wire_pacing_txtime` in `capabilities`.
3. **Live ST 2110-20 1080p50 loopback (operator-side):** configure a
   flow with ST 2110-20 output `wire_pacing: { mode: "tx_time",
   profile: "narrow" }`. Loop the output back into the same edge as a
   ST 2110-20 input (or capture with `tcpdump -ttt`). Without ETF
   qdisc: looks like today (un-paced bursts). With ETF qdisc +
   PTP-disciplined NIC: even pacing within ~few µs of target.
4. **`wire_pacing_late` sanity check:** induce a burst (start the flow
   with `target = now − 10 ms`); confirm the counter increments and
   the Warning event fires.
5. **Negative test:** disable PTP, restart edge — expect
   monotonic-fallback Warning, output still works.

Real ST 2110-21 narrow-profile compliance verification (Bridge Tech
VB330, EBU LIST, Telestream PRISM) is operator-side; a manual checklist
lives in [`docs/st2110.md`](st2110.md).

## Why not unify the two backends today

Phase 1 and Phase 2 share scheduler math conceptually but live in
separate modules (`wire_emit` and `wire_emit_txtime` + `st2110::pacer`)
because:

- The Phase 1 anchor is **PCR** (27 MHz, embedded in the bitstream);
  Phase 2's anchor is **PTP** (1 ns, system-level). Same shape (anchor
  + interpolation), different units.
- Phase 1 reads PCR samples as part of normal datagram parsing; Phase 2
  computes target from raster geometry without touching payload bytes.
- Phase 1's executor is a single thread per output; Phase 2 batches
  packets and uses `sendmmsg` to amortize syscall cost.

Trait extraction is a natural follow-up once both phases have stable
operator-facing surfaces; no protocol-visible churn.
