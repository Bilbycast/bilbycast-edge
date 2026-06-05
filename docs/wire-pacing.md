# Wire pacing

PCR-anchored / PTP-raster-anchored wire emission for every output that owns a UDP socket directly: UDP, RTP (single-leg, FEC, 2022-7 dual-leg), 302M, ST 2110-20/-23/-30/-31/-40. SRT, RIST, RTMP, HLS, CMAF, and WebRTC are out of scope — they have their own protocol-layer pacing.

## Two paths, one default

The wire emitter has two release tiers and **defaults to userspace `clock_nanosleep(CLOCK_TAI, TIMER_ABSTIME)` on a SCHED_FIFO thread**. That path needs no special hardware, no PTP grandmaster, no ETF qdisc, no NIC-firmware capabilities — it just works on commodity Linux. Measured at 2 Gbps on a standard `igc` NIC (no ETF / no PTP / 36 parallel flows): p99 PCR_AC = 177 µs, max = 2.36 ms, zero packet drops across 17.7 M packets. **This is the path every operator should use unless they have an explicit reason to go to tier 1.**

The kernel-paced **`SO_TXTIME` + ETF qdisc** path is the opt-in upgrade for the small minority of deployments that genuinely need sub-µs PCR_AC — typically uncompressed ST 2110-20 contribution feeds, T-STD-strict broadcast contribution decoders (Appear X10, Cobalt 9202, Cisco D9824) running with PCR-error alarms, or single-flow rates approaching the 1 Gbps single-packet ceiling. Enabling it requires PTP + a HW-PTP NIC + an `etf` qdisc installed on the egress interface — see [When to enable ETF qdisc](#when-to-enable-etf-qdisc) for the decision matrix, [Installing ETF qdisc](#installing-etf-qdisc) for the recipe, and [Booting ETF qdisc as a service](#booting-etf-qdisc-as-a-service) for the systemd unit that persists it across reboots.

## Why

Userspace timers — `tokio::time::sleep`, `std::thread::sleep`, `clock_nanosleep` at `SCHED_OTHER` — are unreliable at sub-1 ms targets. The Tokio timer wheel resolution is ~1–5 ms; OS scheduler slop, hyperthread sharing, and IRQ steal push that further. **But `clock_nanosleep(CLOCK_TAI, TIMER_ABSTIME)` on a SCHED_FIFO thread is a different animal**: it returns to userspace at the requested wall-time instant ±50–500 µs typical (worst case ms-tail under heavy load on the box), which is well inside the broadcast tier-2 envelope (≤ 30 ms) — orders of magnitude inside, in fact. That's why it's the default.

The reason `SO_TXTIME` + ETF exists as a higher tier is that at ST 2110-20 1080p the inter-packet budget shrinks to ~4 µs and at 4K to ~1 µs; userspace round-trip can't reach those numbers, but the kernel ETF qdisc + NIC-firmware launch-time offload can.

Without pacing of *any* kind, encoder bursts arrive at the wire as bursts. PCR jitter ("PCR_AC" in TR-101290) on transcoded UDP/RTP measures stdev 5–50 ms — well above the T-STD spec of 500 ns and high enough that strict-broadcast professional decoders reject the stream.

## Capability tiers

`engine::wire_emit::spawn_wire_emitter` only takes the SO_TXTIME path when **all three** of the following hold: `BILBYCAST_ENABLE_TXTIME=1` (alias `BILBYCAST_ENABLE_SO_TXTIME=1`) is set and `BILBYCAST_FORCE_NANOSLEEP` is not, the output is **etf-eligible** (`WirePacingClass::EtfEligible` — ST 2110 uncompressed essence), **and** the `try_enable_so_txtime(&socket, clockid)` probe succeeds on the socket. If any of the three fails — including a compressed (`Lossless`) output even with the env var set — it falls back to the `clock_nanosleep` releaser. The chosen tier is logged at info level on output start and surfaced on `OutputStats.wire_pacing_tier`.

| Tier | Mechanism | Inter-packet jitter | Requires | When |
|---|---|---|---|---|
| 1 | SO_TXTIME + ETF qdisc with `offload` on a PTP-disciplined NIC | Sub-µs | Linux ≥ 4.19, ETF qdisc on `clockid CLOCK_TAI`, NIC with PTP HW tx timestamping (Mellanox CX-6/7, Intel E810/I225/I226, etc.), `ptp4l` + `phc2sys` running, `BILBYCAST_ENABLE_TXTIME=1` | ST 2110-20 contribution, T-STD-strict receivers |
| 2 | SO_TXTIME + software ETF qdisc | ~1–10 µs | Linux ≥ 4.19, ETF qdisc on `clockid CLOCK_TAI` but no NIC HW offload, `BILBYCAST_ENABLE_TXTIME=1` | Ditto — falls back from tier 1 when the NIC lacks PHC HW tx |
| 4 ⭐ | `clock_nanosleep(CLOCK_TAI, TIMER_ABSTIME)` on SCHED_FIFO thread | ~50–500 µs typical, ms-tail under load | Linux + SCHED_FIFO grant (systemd `LimitRTPRIO=99` ships in the production unit). **No qdisc. No PTP. No HW-PTP NIC.** | **Default.** Sufficient for compressed TS through at least 2 Gbps, A/V passthrough, transcoded outputs. |
| 5 | `clock_nanosleep` at SCHED_OTHER, or `std::thread::sleep` on non-Linux | ~1–5 ms typical | Linux without RT grant, or non-Linux | Dev / testbed without the production systemd unit. Still tier-2 broadcast envelope-compliant on a lightly-loaded box. |

`engine::wire_emit` always uses **`CLOCK_TAI`** for both `SO_TXTIME` setsockopt and `clock_nanosleep` — required by the kernel ETF qdisc on Intel ice/igc drivers (which reject CLOCK_MONOTONIC) and gives PTP discipline for free when `ptp4l` + `phc2sys` are running. `derive_target`'s pacing math is purely ns-relative, so the absolute clock domain doesn't matter for the closed-loop rate-following — what matters is that the kernel and the operator's PTP stack agree on which clockid to use.

The pre-2026-05-16 default of "try SO_TXTIME first, fall back to clock_nanosleep" was reverted. On a host without ETF qdisc, the kernel accepts `SO_TXTIME(setsockopt)` and the `SCM_TXTIME` cmsg silently but emits each packet immediately on `sendmsg`, so the wire-emit thread degenerated into a producer-paced loop, propagating any upstream burst straight to the wire while telemetry still reported tier `so_txtime`. The inverted default removes that silent-degradation trap. See operator-feedback memory `feedback_no_etf_qdisc.md`.

## When to enable ETF qdisc

**Default answer: don't.** The userspace tier handles compressed TS through at least 2 Gbps with sub-3 ms PCR_AC max on a normal NIC. ETF only earns its keep in three specific cases:

| Case | Reason | Action |
|---|---|---|
| Per-packet budget < 50 µs (typical: single-flow ST 2110-20 1080p ≈ 4 µs, ST 2110-20 4K ≈ 1 µs) | Userspace round-trip can't reach µs precision at that rate | Enable tier 1, accept the PTP + HW-PTP NIC + ETF dependency stack |
| Strict T-STD compliance for contribution-grade receivers (Appear X10, Cobalt 9202, Cisco D9824 with `PCR_AC` alarm enabled) | These reject streams with PCR jitter > 500 ns | Enable tier 1 |
| Sustained CPU contention pushing tier-4 p99 above ~30 ms (e.g. many transcoded outputs on a tight box) | Kernel ETF moves pacing off the SCHED_FIFO thread, so CPU contention no longer perturbs it | Enable tier 2 (software ETF, no HW-PTP NIC needed) |

If none of those apply — keep the default. Enabling ETF without the full prerequisite stack (PTP, `phc2sys -O 37`, the qdisc itself, `CAP_NET_ADMIN`) produces *silent degradation* worse than the default, not better.

## Installing ETF qdisc

The qdisc is installed by `packaging/setup-etf-qdisc.sh`. The edge **does not** install qdiscs itself — `tc qdisc` requires `CAP_NET_ADMIN` at install time, which is operator-policy, not application-policy.

```bash
# Identify the egress NIC (typically the one with the edge's UDP destinations
# reachable through it). One-shot install — qdisc applies immediately.
sudo bash packaging/setup-etf-qdisc.sh enp1s0

# Verify
tc -s qdisc show dev enp1s0       # look for `etf` in output, zero drops once traffic flows
```

The script installs `mqprio` + `etf` with `clockid CLOCK_TAI`, `offload` (HW where available), and `skip_sock_check on` set. The `skip_sock_check` flag is non-negotiable — without it ARP / DHCP / ssh / every default-priority UDP socket gets dropped at the qdisc because those sockets don't set SO_TXTIME. See [`project_etf_skip_sock_check.md`](../../.claude-memory/monorepo/project_etf_skip_sock_check.md) for the bug we hit when it was missed.

## Booting ETF qdisc as a service

The one-shot `tc` call from `setup-etf-qdisc.sh` doesn't survive a reboot. For deployments that need ETF tier persistence, install the templated systemd unit `packaging/bilbycast-etf-qdisc@.service`:

```bash
# Install + enable for the named NIC. The unit calls
# setup-etf-qdisc.sh at boot, after the NIC is up, before bilbycast-edge.
sudo install -m 0644 packaging/bilbycast-etf-qdisc@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now bilbycast-etf-qdisc@enp1s0

# Verify
systemctl status bilbycast-etf-qdisc@enp1s0
tc -s qdisc show dev enp1s0
```

The unit is **opt-in** (not enabled by default in the curl-pipe-bash installer) — it only makes sense once you've also got PTP + a HW-PTP NIC + the `BILBYCAST_ENABLE_TXTIME=1` env-var set. To remove:

```bash
sudo systemctl disable --now bilbycast-etf-qdisc@enp1s0
# Optional manual teardown:
sudo tc qdisc del dev enp1s0 root
```

## Enabling the SO_TXTIME tier on the edge

After the qdisc is in place + PTP is running, opt in to the SO_TXTIME release path by setting **one** of these env vars on the edge process:

```
BILBYCAST_ENABLE_TXTIME=1       # short form (preferred)
BILBYCAST_ENABLE_SO_TXTIME=1    # long form (accepted alias)
```

Production via the systemd unit: add the line to `/etc/bilbycast/edge.env`. The installer's default env file ships with this line commented — uncomment it after the qdisc + PTP prerequisites are confirmed.

```bash
# /etc/bilbycast/edge.env
BILBYCAST_MLOCKALL=1
# Opt in to kernel-paced wire emission via SO_TXTIME. Requires the ETF
# qdisc on the egress NIC and ptp4l + phc2sys for tier-1 precision.
BILBYCAST_ENABLE_TXTIME=1
```

After restart, the edge log shows `tier=so_txtime` on each output start; otherwise it falls back to `tier=clock_nanosleep` if the setsockopt probe fails (typically because `CAP_NET_ADMIN` isn't granted — see below).

## Operator escape hatch

`BILBYCAST_FORCE_NANOSLEEP=1` is kept as a no-op alias for back-compat — since `clock_nanosleep` is the default already, the flag does nothing now. Old configs that set it still work.

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

## ETF qdisc / PTP prerequisite chain (for tier-1 deployments)

See [When to enable ETF qdisc](#when-to-enable-etf-qdisc) for *whether* you need this and [Installing ETF qdisc](#installing-etf-qdisc) for the install recipe; this section is the prerequisite chain you have to land all of for the tier-1 path to actually deliver sub-µs PCR_AC.

1. **ETF qdisc installed** with `skip_sock_check on`, `clockid CLOCK_TAI`, and `offload` (where HW supports it). The `skip_sock_check` flag is non-negotiable on the default priomap (`0 0 0 0 1 1 1 1 …`): socket-priority 0 — the kernel default for ARP, DHCP, ssh, ping, and every UDP socket the edge opens — routes to the ETF-bearing class. Without `skip_sock_check`, ETF refuses any packet whose socket lacks SO_TXTIME and drops it at the qdisc, so ARP can't resolve the next-hop MAC and every `sendmsg` to that interface returns ENETUNREACH. `packaging/setup-etf-qdisc.sh` sets the flag.

2. **PTP discipline**: `ptp4l + phc2sys` running, system TAI clock disciplined to a grandmaster. **The NIC PHC must be in TAI domain**, not UTC — i.e. `phc2sys` should run with `-O 37` (current TAI–UTC offset, January 2026) or, equivalently, sync FROM the system's `CLOCK_TAI` rather than `CLOCK_REALTIME`. If `phc2sys -O 0` is used (PHC tracks UTC), kernel `CLOCK_TAI` ends up 37 s ahead of the PHC, every offload-mode launch time lands outside the NIC's launch horizon (1–4 s on Intel i225/i226, similar on others), and the NIC silently rejects every packet. Symptom: `tc -s qdisc show dev <NIC>` reports 100 % drops on the etf class with zero packets sent; distinguish from the missing-`skip_sock_check` symptom by `getcap` on the edge binary + by reading `phc2sys -O` from the running process line.

3. **HW-PTP-capable NIC** (Mellanox CX-6 / CX-7, Intel E810, Intel I210). NICs that *look* like they have PTP (I225/I226 desktop revisions) sometimes lack the PHC clock; verify with `ethtool -T <NIC>` and look for `hardware-transmit` in the capabilities.

4. **`BILBYCAST_ENABLE_TXTIME=1`** in the edge's environment — without this, the edge stays on the default `clock_nanosleep` path regardless of whether ETF is installed.

5. **`CAP_NET_ADMIN`** on the edge process (mandatory for `setsockopt(SO_TXTIME, CLOCK_TAI)` on kernel ≥ 6.x) — see below.

Verify all of the above with the operator runbook on the website: [Wire-Time Precision](https://docs.bilbycast.com/edge/wire-pacing/).

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
| `./testbed/start-infrastructure.sh` *(default)* | userspace `clock_nanosleep(CLOCK_TAI)` on SCHED_FIFO | none | nothing — works on any NIC, no qdisc, no PTP |
| `BILBYCAST_ENABLE_TXTIME=1 ./testbed/start-infrastructure.sh` | SO_TXTIME(CLOCK_TAI) on tier 1/2 | `cap_net_admin,cap_sys_nice+ep` on both edge binaries via one sudo prompt (one-time per rebuild — caps persist in xattrs) | for tier-1 PCR_AC: `setup-etf-qdisc.sh` on the egress NIC + `ptp4l`/`phc2sys` (see above). Without ETF the SO_TXTIME path silently degrades back to producer-paced (worse than the default). |

Both modes log the active tier on each output start (`wire-emit '<id>': starting (anchor=…, tier=…)`) and surface it on `OutputStats.wire_pacing_tier`.

Then on edge start, the log line:

```
wire-emit '<id>': starting (anchor=Pcr, tier=so_txtime)
```

`tier=so_txtime` confirms the SO_TXTIME path. `tier=clock_nanosleep` means setsockopt failed — a `wire-emit: SO_TXTIME(clockid=11) setsockopt failed: Operation not permitted (kernel requires CAP_NET_ADMIN for non-CLOCK_MONOTONIC SO_TXTIME on this kernel; grant via systemd 'AmbientCapabilities=CAP_NET_ADMIN' or 'setcap cap_net_admin+ep <binary>') — falling back to clock_nanosleep tier` warning is emitted at the same time.

## SCHED_FIFO grant

The wire-emit thread requests `SCHED_FIFO` priority 50 best-effort. Production via the systemd unit `packaging/bilbycast-edge.service` (`LimitRTPRIO=99`, `RestrictRealtime=false`) — no separate capability grant required for this, kernel allows unprivileged `SCHED_FIFO` whenever the requested priority is at or below `RLIMIT_RTPRIO`. The shipped unit ships `LimitRTPRIO=99` (covers any priority 1–99 with headroom); the smaller `LimitRTPRIO=50` would also work for the current wire-emit priority. Dev via `cargo run` needs:

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
