# Wire pacing

Tier-1 broadcast PCR jitter on TS-bearing UDP / RTP outputs.

> **Status (2026-05-09):** module + 12 unit tests landed; integration
> into `output_udp` / `output_rtp` is **deferred**. Live testing of the
> encoded h264_qsv + aac_lc path showed the bitstream's own PCR values
> are non-uniform — `tsp pcrverify` reports the same `0/197 jitter > 100 µs`
> result whether read off the network OR off a captured TS file. Wire
> pacing alone cannot fix non-uniform bitstream PCRs. The next change
> on this track is fixing PCR placement in `ts_video_replace` /
> `av_sync_mux::pcr_for_emit`; once that lands, this module will wire
> in. The unit tests below already cover the algorithmic behaviour and
> stay enforced.

## Why

The Tokio timer wheel resolution is ~1–5 ms on a healthy box, more under
load. `tokio::time::sleep_until` therefore can't deliver the sub-100 µs
PCR cadence professional decoders (Appear, Tektronix) require. Output
emission timing has to leave the async runtime.

`engine::wire_emit` is the per-output dedicated wallclock pacer that
drives `socket.send_to` from a `std::thread` with `SCHED_FIFO`
best-effort priority and `clock_nanosleep(CLOCK_MONOTONIC,
TIMER_ABSTIME, ...)` as the wait primitive.

## Architecture

```
Encoder task (tokio, block_in_place)
   │
   │ try_send(WireDatagram)
   ▼
std::sync::mpsc::sync_channel(256)
   │
   ▼
wire_emit thread (std::thread, SCHED_FIFO)
   ├─ derive target_ns from datagram PCR + wall_anchor
   ├─ clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, target_ns)
   ├─ optional RFC 2250 RTP wrap
   ├─ blocking socket.send_to
   └─ stats: packets_sent, bytes_sent, record_latency, record_pcr_egress
```

Channel overflow is non-blocking on the encoder side: `try_send` returns
`Full`, the datagram drops, and `packets_dropped` ticks. Same drop
semantic as the broadcast channel's `Lagged` arm — slow wire never
back-pressures the encoder.

## Target derivation

Anchor on the first datagram: `wall_anchor_ns = monotonic_now() +
PREROLL_NS` (50 ms). If the first datagram carries a PCR, `pcr_anchor =
pcr`. The first datagram emits at exactly `wall_anchor_ns`.

For each subsequent datagram:

| Datagram        | Target                                                                |
|-----------------|-----------------------------------------------------------------------|
| **PCR-bearing** | `wall_anchor + (pcr − pcr_anchor) / 27` (ns). Re-anchor: `wall_anchor = target`, `pcr_anchor = pcr`, `bytes_since_anchor = 0`. |
| **No PCR**      | `wall_anchor + bytes_since_anchor × 8 × 1e9 / bitrate_bps`. `bytes_since_anchor` accumulates until next PCR re-anchor. |

`bitrate_bps` is `declared_bitrate_bps` (passed in by the output —
encoder targets + 5 % overhead) when set, otherwise an EMA inferred from
inter-PCR observations (1/4 step toward each new sample, ~10 PCRs to
95 %). Cold-start passthrough with no rate hint and no inter-PCR
observation yet emits at `now` — receivers tolerate packets-ahead far
better than packets-far-behind, and the next PCR seeds the EMA within
~40 ms.

Re-anchoring on every PCR bounds drift between our wallclock and the
source's PCR clock to one inter-PCR period (~30–40 ms typical).

## Discontinuity

A backwards PCR step or any forward jump > 500 ms (mirroring
`engine::pcr_pll` and `stats::pcr_trust`) resets the anchor to "now"
and resumes from the new PCR.

## Realtime priority

`pthread_setschedparam(SCHED_FIFO, priority=50)` is best-effort. Failure
(typically `EPERM` from missing `CAP_SYS_NICE`) logs at debug level and
the thread continues at default `SCHED_OTHER`. Grant the capability for
lowest jitter:

```bash
sudo setcap cap_sys_nice+ep /opt/bilbycast/edge/current/bilbycast-edge
```

The systemd unit shipped under `packaging/` already requests this
capability via `AmbientCapabilities=CAP_SYS_NICE`.

## Cancellation

`recv_timeout(50 ms)` polls the cancellation token; emitter exits within
~50 ms of cancel. Channel disconnect (encoder dropped) also exits the
thread cleanly.

## Where it's wired

No outputs are wired in yet (see status banner above). Once enabled,
the natural rollout order is:

| Output | File | Wrap |
|--------|------|------|
| UDP    | `engine::output_udp` (single-leg) | `WireWrap::None` |
| RTP    | `engine::output_rtp` (single-leg) | `WireWrap::Rtp` (header built in emitter) |

302M, SRT, RIST, HLS, CMAF, RTMP, WebRTC, ST 2110, and the redundancy
(2022-7) RTP path will keep their existing emit code. Tier-1 PCR
jitter is specifically a UDP / RTP single-leg concern; the dual-leg
path needs identical seq+SSRC across both legs and would require a
slightly different shared-state design — out of scope.

## Future: SO_TXTIME

The next jitter floor is the kernel scheduler. `SO_TXTIME` + the `fq`
qdisc let userspace stamp each datagram with a target egress timestamp
the kernel honours at NIC-driver level — sub-microsecond precision on
hardware that supports `SCM_TXTIME`. The `wire_emit` `target_ns`
contract is what would be handed to `sendmsg(SCM_TXTIME)`. Not in this
release; a previous prototype was buggy and reverted. Tracked as a
future optional optimization on top of the current `clock_nanosleep`
baseline.
