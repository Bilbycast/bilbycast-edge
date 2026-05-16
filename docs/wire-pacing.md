# Wire pacing

PCR-anchored / PTP-raster-anchored kernel-paced wire emission for every output that owns a UDP socket directly: UDP, RTP (single-leg, FEC, 2022-7 dual-leg), 302M, ST 2110-20/-23/-30/-31/-40. SRT, RIST, RTMP, HLS, CMAF, and WebRTC are out of scope — they have their own protocol-layer pacing.

## Why

Userspace timers — `tokio::time::sleep`, `std::thread::sleep`, `clock_nanosleep` even on `SCHED_FIFO` — are unreliable at sub-1 ms targets. The Tokio timer wheel resolution is ~1–5 ms; OS scheduler slop, hyperthread sharing, and IRQ steal push that further. At 100 Mbps the inter-packet budget is ~100 µs; at ST 2110 1080p ~4 µs; at 4K 12 Gbps ~1 µs. Userspace can't reach those.

Without pacing, encoder bursts arrive at the wire as bursts. PCR jitter ("PCR_AC" in TR-101290) on transcoded UDP/RTP measures stdev 5–50 ms in this codebase's status quo — well above the T-STD spec of 500 ns and high enough that professional decoders (Appear, Tektronix, EVS) reject the stream.

Kernel-paced delivery via `SO_TXTIME` + ETF qdisc moves the per-packet timing decision out of userspace. With NIC HW offload you're sub-µs; with software ETF you're 1–10 µs; without ETF the kernel ignores the per-packet timestamp and emits ASAP — same observable behaviour as no pacing, no regression. Userspace `clock_nanosleep` on a `SCHED_FIFO` thread is the graceful-degradation fallback when SO_TXTIME isn't available.

## Capability tiers

`engine::wire_emit::spawn_wire_emitter` probes the host at output startup and selects the highest tier the host can deliver. The chosen tier is logged at info level on output start and surfaced on `OutputStats.wire_pacing_tier`.

| Tier | Mechanism | Inter-packet jitter | Requires |
|---|---|---|---|
| 1 | SO_TXTIME + ETF qdisc with `offload` on a PTP-disciplined NIC | Sub-µs | Linux ≥ 4.19, ETF qdisc on `clockid CLOCK_TAI`, NIC with PTP HW tx timestamping (Mellanox CX-6/7, Intel E810/I225/I226, etc.), `ptp4l` + `phc2sys` running |
| 2 | SO_TXTIME + software ETF qdisc | ~1–10 µs | Linux ≥ 4.19, ETF qdisc installed on `clockid CLOCK_TAI` but no NIC offload |
| 3 | SO_TXTIME accepted, ETF qdisc absent | Same as no pacing — kernel ignores `SCM_TXTIME` | Linux ≥ 4.19. Probe still succeeds at setsockopt level, but every datagram sends ASAP. **Diagnose with `BILBYCAST_FORCE_NANOSLEEP=1` to compare against tier 4.** |
| 4 | `clock_nanosleep(CLOCK_TAI, TIMER_ABSTIME)` on SCHED_FIFO thread | ~50–500 µs typical, ms-tail under load | Linux + SCHED_FIFO grant (systemd `LimitRTPRIO=50` or `cap_sys_nice`). Default fallback when SO_TXTIME probe fails. |
| 5 | `clock_nanosleep` at SCHED_OTHER, or `std::thread::sleep` on non-Linux | ~1–5 ms | None |

`engine::wire_emit` always uses **`CLOCK_TAI`** for both `SO_TXTIME` setsockopt and `clock_nanosleep` — required by the kernel ETF qdisc on Intel ice/igc drivers (which reject CLOCK_MONOTONIC) and also gives us PTP discipline for free when `ptp4l` + `phc2sys` are running. `derive_target`'s pacing math is purely ns-relative, so the absolute clock domain doesn't matter for the closed-loop rate-following; what matters is that we use the clockid the kernel + the operator's PTP stack agrees on.

For tier-1 broadcast PCR_AC compliance (≤ 500 ns) you need tier 1 in full — ETF qdisc + `ptp4l` + HW-PTP NIC. Without `ptp4l`, CLOCK_TAI is just system clock + leap seconds; you stay at tier 2/4 precision regardless of qdisc. Operators install ETF qdisc + run linuxptp separately — see [`docs/wire-pacing` operator runbook below](#operator-side-etf-qdisc) and the public [Wire-Time Precision page](https://docs.bilbycast.com/edge/wire-pacing/) on the website.

## Operator escape hatch

`BILBYCAST_FORCE_NANOSLEEP=1` skips the SO_TXTIME probe in `spawn_wire_emitter` and forces tier 4 (`clock_nanosleep`) on every output. Diagnostic only:

- **"tier reports `so_txtime` but PCR_AC is bad"** — typical symptom of ETF qdisc misconfigured (wrong clockid, on the wrong interface, or not installed). Setting the env var forces the predictable userspace path; if PCR_AC improves, you have hard evidence the SO_TXTIME path was silently degraded.
- **Kernel/driver regression workaround** — kernels where SO_TXTIME has a known SO_TXTIME bug under PCR-discontinuity load. Set the env var until upstream fixes land.

Not a production setting. Production runs at tier 1 (SO_TXTIME + ETF + PTP).

## Architecture

```
                  ┌──── encoder pipeline (block_in_place) ────┐
                  │ TsAudioReplacer / TsVideoReplacer / mux   │
                  └─────────────────┬─────────────────────────┘
                                    │
                                    ▼
                ┌──────────── caller wraps payload ────────────┐
                │     UDP raw TS  │  RTP header + TS  │  ...   │
                └──────────────────┬───────────────────────────┘
                                   │  WireDatagram { bytes, recv_time_us, target_tx_time_ns }
                                   ▼
                          ┌────────────────────┐
                          │   WireEmitter      │   one std::thread per output
                          │   (SCHED_FIFO)     │
                          └──────────┬─────────┘
                                     │
                       ┌─────────────┴──────────────┐
                       ▼                            ▼
              ┌────────────────┐         ┌────────────────────┐
              │ SO_TXTIME      │         │ clock_nanosleep    │
              │ + sendmsg      │         │ + send_to          │
              │ (kernel ETF)   │         │ (userspace)        │
              └────────────────┘         └────────────────────┘
```

One `WireEmitter` instance per UDP socket. Two anchor strategies, selected at spawn time via `AnchorSource`:

- **`AnchorSource::Pcr`** (TS regime): the emitter parses each datagram for an MPEG-TS PCR sample. PCR-bearing datagrams re-anchor the wallclock target. Non-PCR datagrams pace by `bytes_since_anchor / observed_rate_bps`. The rate is exclusively from inter-PCR observations (EMA, ~10 PCRs to 95 % convergence). There is **no declared-bitrate parameter** — open-loop pacing on a configured rate drifts when the encoder runs above (or below) the configured target, which is what reverted the prior wire_emit integration on 2026-05-09. Closed-loop on observed rate is the universal fix regardless of codec, RC mode, or encoder backend.
- **`AnchorSource::St2110Raster`** (uncompressed video regime): the caller computes a per-packet `target_tx_time_ns` against the active ST 2110-21 frame raster (via `St2110_21Pacer`) and passes it on the `WireDatagram`. The emitter just delivers at the requested time.

For PCR-anchored datagrams:

- First datagram emits at `wall_anchor = now + 50 ms` (preroll).
- Subsequent PCR-bearing datagrams: `target = wall_anchor + (pcr − pcr_anchor) × 1000 / 27` ns. Re-anchor: `wall_anchor = target`, `pcr_anchor = pcr`.
- Non-PCR datagrams interpolate: `target = wall_anchor + bytes_since_anchor × 8e9 / observed_rate_bps`. Cold-start (no rate yet): target the wall_anchor (preserves FIFO order through preroll).
- **Late-rebase**: if a PCR target lands more than 5 ms behind real time (encoder burst), `wall_anchor` snaps to `now`. One-time jitter step instead of a steady-state degradation.
- **Discontinuity**: backwards PCR or > 500 ms forward jump resets the anchor to `now`. Mirrors `engine::pcr_pll` and `stats::pcr_trust`.

## Caller-side wrap

`WireDatagram::bytes` is on-the-wire-final. RTP wrap, FEC packetisation, RFC 4175 ST 2110 packetisation — all done upstream of the pacer. This unblocks two cases the prior integration excluded:

- **RTP + FEC (single-leg)**: media + FEC packets share one socket. With caller-side wrap, both flow through the same `WireEmitter` instance. Single owner of the std blocking socket; no Mutex needed. FEC packets carry their own SSRC, separate seq, no PCR; the pacer treats them as non-PCR datagrams (paced at observed rate).
- **2022-7 dual-leg redundancy**: receiver dedup requires byte-identical RTP headers (seq + SSRC) on Red + Blue. The encoder task wraps once with shared `RtpWrapState`, then `try_send` the same `Bytes::clone()` (refcount bump only — no copy) to two `WireEmitter` instances bound to Red + Blue sockets. Both pace identically (same input cadence, same observed-rate EMA). Sub-µs cross-leg scheduling skew is well within 2022-7 receiver dedup tolerance (typically 100s of ms).

## Integration matrix

| Output | Anchor | Notes |
|---|---|---|
| UDP (TS) | `Pcr` | Encoder task → `wire_emit` (one instance per output socket). |
| UDP (302M / S302M-in-TS) | `Pcr` | Audio is RTP-clock paced upstream; `wire_emit` rides through. |
| RTP single-leg, no FEC | `Pcr` | RTP wrap done in encoder task before `try_send`. |
| RTP single-leg + FEC | `Pcr` | Media + FEC RTP both flow through one `wire_emit` (same socket). |
| RTP 2022-7 dual-leg | `Pcr` | Two `wire_emit` instances; same `Bytes::clone()` to both. Leg 2 stats are private (avoids double-counting on the shared accumulator). |
| ST 2110-20 / -23 (uncompressed video) | `St2110Raster` | Per-output (-20) or per-sub-stream (-23) `St2110_21Pacer` computes per-packet `target_tx_time_ns`. Always paced. |
| ST 2110-30 / -31 / -40 (audio + ANC) | `Pcr` | These are RTP-paced upstream by the RFC 3550 sender clock; the pacer rides through (cold-start ASAP — same as today, but on the SO_TXTIME path when available). |
| SRT, RIST, RTMP, HLS, CMAF, WebRTC | n/a | Internally paced by their own protocol layers; not touched. |

## Operator-side ETF qdisc

For tier-1 PCR_AC (broadcast-grade, T-STD ≤ 500 ns), install ETF qdisc on the egress NIC. The edge does not install qdiscs (qdisc setup needs `CAP_NET_ADMIN` at install time, deliberately operator-side):

```bash
sudo bash packaging/setup-etf-qdisc.sh enp1s0    # name your egress NIC
```

The script installs `mqprio` + `etf` with `clockid CLOCK_TAI`, `offload`, and `skip_sock_check` set. The `skip_sock_check` flag is non-negotiable on this priomap (`0 0 0 0 1 1 1 1 …`): socket-priority 0 — the kernel default for ARP, DHCP, ssh, ping, and every UDP socket the edge opens — routes to the ETF-bearing class 100:1. Without `skip_sock_check`, ETF refuses any packet whose socket lacks SO_TXTIME and drops it at the qdisc, so ARP can't resolve the next-hop MAC and every `sendmsg` to that interface returns ENETUNREACH. With `skip_sock_check`, non-SO_TXTIME packets fall through to FIFO release (they leave the box but without a hardware-scheduled launch instant); SO_TXTIME packets still get scheduled.

PTP prerequisite for tier-1: `ptp4l + phc2sys` running, system TAI clock disciplined to a grandmaster. **The NIC PHC must be in TAI domain**, not UTC — i.e. `phc2sys` should run with `-O 37` (current TAI–UTC offset, January 2026) or, equivalently, sync FROM the system's `CLOCK_TAI` rather than `CLOCK_REALTIME`. If `phc2sys -O 0` is used (PHC tracks UTC), kernel `CLOCK_TAI` ends up 37 s ahead of the PHC, every offload-mode launch time lands outside the NIC's launch horizon (1–4 s on Intel i225/i226, similar on others), and the NIC silently rejects every packet. Symptom: `tc -s qdisc show dev <NIC>` reports 100 % drops on the etf class with zero packets sent, identical to the missing-`skip_sock_check` symptom; distinguish the two by `getcap` on the edge binary + by reading `phc2sys -O` from the running process line.

Without ETF qdisc the SO_TXTIME path silently degrades to no-op pacing (same observable behaviour as today's unpaced path). The edge does NOT probe ETF presence (querying qdiscs needs CAP_NET_ADMIN); operators verify with:

```bash
tc -s qdisc show dev enp1s0    # look for "etf" in the output, and zero drops once traffic flows
```

## CAP_NET_ADMIN grant (kernel ≥ 6.x)

Mainline kernel ≥ 6.x — and every recent Ubuntu / RHEL / Debian backport — requires **`CAP_NET_ADMIN`** for `setsockopt(SO_TXTIME)` with any non-`CLOCK_MONOTONIC` clockid. Wire pacing uses `CLOCK_TAI` (the only clockid the ETF qdisc on Intel ice / igc / igb and Mellanox mlx5 will accept, and the natural choice when ptp4l + phc2sys are running). Without the cap, the setsockopt returns `EPERM`, `engine::wire_emit` falls back to the `clock_nanosleep` tier — and on any host that also runs the ETF qdisc above, fallback-tier packets get dropped at the qdisc because they're not SO_TXTIME-bearing (see the `skip_sock_check` note above).

Production via the systemd unit `packaging/bilbycast-edge.service` ships with:

```
CapabilityBoundingSet=CAP_NET_ADMIN
AmbientCapabilities=CAP_NET_ADMIN
```

Dev / testbed via `cargo run` or a direct release-binary invocation:

```bash
sudo setcap cap_net_admin+ep target/release/bilbycast-edge      # or target-full/
# or run as root
```

Verify:

```bash
getcap target/release/bilbycast-edge
# expected: target/release/bilbycast-edge cap_net_admin=ep
```

### Testbed quick toggle (`testbed/start-infrastructure.sh`)

The testbed start script makes the SO_TXTIME tier opt-in so plain-NIC runs and full ETF + PTP runs are both reachable from the same script with no manual `setcap` step:

| Invocation | Wire pacer | Caps applied | NIC / kernel needs |
|---|---|---|---|
| `./testbed/start-infrastructure.sh` *(default)* | userspace `clock_nanosleep(CLOCK_TAI)` | none — script also exports `BILBYCAST_FORCE_NANOSLEEP=1` so any caps left over from a previous run are bypassed | nothing — works on any NIC, no qdisc, no PTP |
| `BILBYCAST_ENABLE_TXTIME=1 ./testbed/start-infrastructure.sh` | SO_TXTIME(CLOCK_TAI) when ETF is present, else falls back to `clock_nanosleep` | `cap_net_admin,cap_sys_nice+ep` on both edge binaries via one sudo prompt (one-time per rebuild — caps persist in xattrs) | for tier-1 PCR_AC: `setup-etf-qdisc.sh` on the egress NIC + `ptp4l`/`phc2sys` (see above). Without ETF the SO_TXTIME path silently degrades. |

Both modes log the active tier on each output start (`wire-emit '<id>': starting (anchor=…, tier=…)`) and surface it on `OutputStats.wire_pacing_tier`.

Then on edge start, the log line:

```
wire-emit '<id>': starting (anchor=Pcr, tier=so_txtime)
```

`tier=so_txtime` confirms the SO_TXTIME path. `tier=clock_nanosleep` means setsockopt failed — a `wire-emit: SO_TXTIME(clockid=11) setsockopt failed: Operation not permitted (kernel requires CAP_NET_ADMIN for non-CLOCK_MONOTONIC SO_TXTIME on this kernel; grant via systemd 'AmbientCapabilities=CAP_NET_ADMIN' or 'setcap cap_net_admin+ep <binary>') — falling back to clock_nanosleep tier` warning is emitted at the same time.

## SCHED_FIFO grant

The wire-emit thread requests `SCHED_FIFO` priority 50 best-effort. Production via the systemd unit `packaging/bilbycast-edge.service` (`LimitRTPRIO=50`, `RestrictRealtime=false`) — no separate capability grant required for this, kernel allows unprivileged `SCHED_FIFO` whenever `RTPRIO` is non-zero. Dev via `cargo run` needs:

```bash
sudo setcap cap_sys_nice,cap_net_admin+ep target/debug/bilbycast-edge
# or run as root
```

(Combine with the `CAP_NET_ADMIN` grant above — `setcap` overwrites the capability set on each call, so list both in one invocation.)

Verify:

```bash
ps -eLo pid,tid,class,rtprio,comm | grep bilbycast-edge
# wire-emit-* threads show class=FF, rtprio=50
```

Without the grant, threads run at default `SCHED_OTHER` (tier 5 in the table above) and a debug-level log line shows the failure. Pacing still works at coarser jitter.

## Diagnostics

- **`OutputStats.wire_pacing_tier`**: which tier this output is running at. Surfaced in `/api/v1/stats` and on the manager UI's per-output card.
- **`OutputStats.wire_pacing_late`**: count of datagrams the kernel rejected as late (target landed in the past). Always 0 on the userspace-sleep release path (no errqueue). Drained from MSG_ERRQUEUE every ~1024 packets on the SO_TXTIME path.
- **`OutputStats.pcr_trust`**: per-output PCR accuracy reservoir (4096 samples). Records `|ΔPCR_µs − Δwall_µs|` on every PCR-bearing send. p50 / p95 / p99 / max exposed.
- **Startup log line**: `wire-emit '<id>': starting (anchor=<...>, tier=<...>)` — always present.

## Why the prior integration failed (2026-05-09)

The Phase-1 wire_emit attempt paced between PCRs as `target = wall_anchor + bytes_since_anchor × 8e9 / declared_bitrate_bps`. `declared_bitrate_bps` came from `video_encode.bitrate_kbps × 1000`.

**Open-loop bug**: encoders never run at exactly their declared rate. CRF / capped-VBR / "CBR" with VBV all overshoot transiently. When actual rate > declared rate by N %, packets arrive at the wire emitter ~N % faster than the pacer schedules them. The mpsc channel filled. After 10–15 s of sustained mismatch on a 6 Mbps feed, the 1024-deep channel saturated → 1.7 s of latency baked in → next 1.7 s of new packets dropped. Decoders saw PCR_AC errors plus packet loss → pixelation.

The PCR re-anchor logic + late-rebase is mathematically correct for PCR-bearing packets — those self-correct on every PCR. The drift was on the non-PCR packets between PCRs, paced at the wrong rate.

The current (live) implementation drops `declared_bitrate_bps` entirely. Closed-loop on observed inter-PCR rate eliminates the drift at the root, regardless of codec, RC mode, or encoder backend.
