# Production tuning for bilbycast-edge

This doc is the operator recipe for getting bilbycast-edge to its **tier-1
pacing target** — sub-µs PCR_AC, broadcast-grade A/V sync, predictable
behaviour under load. It assumes the systemd install
(`packaging/install-edge.sh` + `packaging/bilbycast-edge.service`); a
manual install needs to grant the same capabilities + limits by hand.

The recipe stacks. Each layer is independent — you can stop at any
point and the edge still works; you just won't reach the next jitter
floor. Run each layer's verification before moving on.

## Layer 0 — what you get out of the box

The default `install-edge.sh` already configures:

| What | How | Effect |
|---|---|---|
| **Wire-emit thread** | dedicated `std::thread` per output socket (not a Tokio task) | Pacing decoupled from the async runtime |
| **SCHED_FIFO priority 50** | `LimitRTPRIO=99` in the systemd unit | Wire-emit beats SCHED_OTHER workloads |
| **`mlockall` at startup** | `BILBYCAST_MLOCKALL=1` seeded in `/etc/bilbycast/edge.env` + `LimitMEMLOCK=infinity` | No major page faults on hot path |
| **CAP_NET_ADMIN** | `AmbientCapabilities=CAP_NET_ADMIN` | `setsockopt(SO_TXTIME, CLOCK_TAI)` succeeds |
| **CLOCK_TAI pacing** | `engine::wire_emit` reads + sleeps on CLOCK_TAI | Compatible with PTP-disciplined NICs |

**Expected PCR_AC p99 at this layer**: ~50–500 µs (the `clock_nanosleep_fifo` tier
of `engine::wire_emit`).

**Verify**:
- `journalctl -u bilbycast-edge -g 'wire-emit'` should show one
  `starting (tier=clock_nanosleep_fifo, sched_fifo=true, …)` line per
  output. If `sched_fifo=false` appears, `LimitRTPRIO=99` didn't take
  effect — check `systemctl status bilbycast-edge` for the active
  limit.
- The manager UI's per-node **Resources** card shows
  `scheduling_status.sched_fifo_granted = true` and
  `mlockall = "locked"`.

## Layer 1 — CPU pinning for wire-emit threads

The kernel scheduler can move the wire-emit thread off its core under
contention; if it lands on a core that's hosting a Tokio worker mid-encode,
pacing tail latency spikes. Pinning each wire-emit thread to a dedicated
core fixes this.

**Configure**: edit `/etc/bilbycast/edge.env`, set
```
BILBYCAST_WIRE_EMIT_CPUS=2,3
```
(comma-separated list, ranges allowed e.g. `2-5,9`). Multiple wire-emit
threads round-robin across the set. Pick cores that you also isolate in
layer 2.

`sudo systemctl restart bilbycast-edge` to apply.

**Verify**: the manager UI's per-output **Wire pacing** chip shows
`pinned_cpu=2` (or whichever core landed on that output). `taskset -p
$(pidof bilbycast-edge)` shows the process-level affinity (still
spans every core — pinning is per-thread, not per-process). `ps -o
pid,psr,comm -T -p $(pidof bilbycast-edge)` lists every thread's
last-run CPU; the `wire-emit-…` threads should stay on the configured
cores.

**Expected PCR_AC p99 after layer 1**: still ~50–500 µs on this layer
alone, but with a much tighter tail. Steady-state p99 unchanged; max
(p100) drops measurably.

## Layer 2 — kernel-level CPU isolation

`BILBYCAST_WIRE_EMIT_CPUS` pins the threads, but the kernel still
schedules other tasks (timer ticks, RCU callbacks, kernel threads, other
user processes) onto the same cores. To make the pinned cores belong
*exclusively* to bilbycast, isolate them at boot.

**Configure**: edit `/etc/default/grub`, append to
`GRUB_CMDLINE_LINUX_DEFAULT`:
```
isolcpus=2,3 nohz_full=2,3 rcu_nocbs=2,3
```
`sudo update-grub && sudo reboot`. Pick cores that are physically *not*
on the same SMT pair as a busy Tokio worker, to avoid hyperthread cache
fighting.

| Kernel option | What it does |
|---|---|
| `isolcpus=N-M` | Removes those cores from the default scheduler's load-balancing pool. Tasks land there only via explicit affinity. |
| `nohz_full=N-M` | Disables the per-core scheduler tick when only one task is runnable. Removes 1 ms periodic interrupts. |
| `rcu_nocbs=N-M` | Moves RCU callbacks off the isolated cores. Removes a deferred-work jitter source. |

**Verify**: `cat /sys/devices/system/cpu/isolated` shows the configured
cores. `grep -E '(LOC|RES|TLB)' /proc/interrupts` should show very few
ticks on the isolated cores after a few minutes of idle.

**Expected PCR_AC p99 after layer 2**: still on `clock_nanosleep_fifo`
unless you also wire up PTP + ETF qdisc (layer 3) — but the per-thread
tail is now bounded by hardware interrupts, not by user-space scheduler
slop. p99 typically drops to ~50–150 µs, max stays under 5 ms even
under heavy CPU load on the other cores.

## Layer 3 — PTP grandmaster + ETF qdisc (sub-µs pacing tier)

This is the layer that puts you on the **tier-1** path: `SO_TXTIME` +
kernel ETF + (optionally) NIC hardware-offload TX scheduling. PCR_AC
becomes a function of the NIC's PTP timestamping accuracy and the
qdisc's `delta` budget — typically ≤ 500 ns with hardware offload, ~1–10
µs with software-only ETF.

Prerequisites:
- A NIC with **hardware PTP timestamping** (PHC + ETF support). Common
  cards: Intel E810, Intel I210/I225/I226 (limited), Mellanox CX-5/CX-6,
  Solarflare SFN8542.
- A **PTP grandmaster** on the LAN — facility GM ideal, or a local GPS-
  disciplined daemon (e.g. `chrony` + `gpsd` on a Pi with PPS in).

**Install + configure**:
```bash
sudo apt install linuxptp ethtool iproute2

# Start ptp4l in slave mode + phc2sys to discipline CLOCK_TAI.
# Replace ${IFACE} with your egress NIC.
sudo systemctl enable --now ptp4l@${IFACE}.service
sudo systemctl enable --now phc2sys@${IFACE}.service

# Install the ETF qdisc on the egress NIC.
# bilbycast ships a setup script that handles the priomap +
# `skip_sock_check on` flag (critical — see KNOWN ISSUES below).
sudo packaging/setup-etf-qdisc.sh ${IFACE}
```

**Verify**:
- `journalctl -u bilbycast-edge -g 'tier='` shows `tier=so_txtime` after
  the next restart.
- `ethtool -T ${IFACE}` shows hardware TX timestamping capabilities.
- `chronyc tracking` (or equivalent) shows PHC discipline error well
  under 1 µs.
- The manager UI's per-output Wire-pacing chip flips to `so_txtime`.
- PCR_AC p99 (manager UI flow detail) drops below 1 µs steady-state.

### KNOWN ISSUES on this layer

- **ETF qdisc + missing `skip_sock_check`**: a bare `tc qdisc add etf …`
  call routes *every default-priority packet* (ARP, ssh, every
  non-SO_TXTIME UDP socket) through the etf class, which drops anything
  without a `SCM_TXTIME` cmsg. This kills ARP resolution and produces
  ENETUNREACH on the host. `packaging/setup-etf-qdisc.sh` installs with
  `skip_sock_check on` so only sockets that explicitly opt into
  SO_TXTIME are subject to ETF scheduling.
- **CLOCK_TAI not disciplined by `phc2sys`**: if `phc2sys` isn't running
  with `-O 37` (or equivalent leap-second handling), CLOCK_TAI is ~37 s
  ahead of the PHC. `setsockopt(SCM_TXTIME, CLOCK_TAI)` succeeds but
  every per-packet target lands far in the future; the kernel ETF qdisc
  silently drops the lot. Symptom: tier reports `so_txtime` but packets
  vanish from the wire. Verify with `chronyc tracking` showing PHC
  discipline.
- **`igc` driver (Intel I225/I226)** is on the theoretical PTP list but
  the desktop variants lack a functional PHC. Use I210 (server variant)
  or step up to Intel E810 / Mellanox CX-6 for production.

## Layer 4 — PREEMPT_RT kernel (optional)

The mainline kernel preempts kernel-mode work only at preemption points
(syscall returns, etc.). Under heavy I/O or interrupt load, this can
stretch latency tails into the milliseconds even for SCHED_FIFO threads.
PREEMPT_RT converts kernel spinlocks to mutexes and enables preemption
everywhere, shrinking the worst-case latency by an order of magnitude.

For broadcast use this is usually overkill — layer 2 + layer 3 reach
the same envelope without the PREEMPT_RT operational overhead. Consider
it only if you're contractually committed to sub-100 µs *max*
(not just p99).

```bash
# Ubuntu 24.04+ ships PREEMPT_RT in the HWE kernel; check /proc/version
# for "PREEMPT_RT" substring after enabling.
sudo apt install linux-image-generic-hwe-24.04
# Manual kernel build is also an option — see https://wiki.linuxfoundation.org/realtime
```

**Verify**: `uname -v | grep PREEMPT_RT` shows the build flag.
`cyclictest -p 50 -t1 -n -h100 -l1000000` from rt-tests should show
worst-case latency well under 50 µs on an otherwise-idle box.

## Reference: full set of environment variables

| Variable | Default | Effect |
|---|---|---|
| `BILBYCAST_MLOCKALL` | `0` (unset) — seeded `1` by installer | Lock all current + future pages with `mlockall` at startup |
| `BILBYCAST_WIRE_EMIT_CPUS` | unset | Comma-separated CPU set for round-robin wire-emit thread pinning |
| `BILBYCAST_FORCE_NANOSLEEP` | `0` | Skip SO_TXTIME probe (diagnostic; degrades pacing) |
| `BILBYCAST_PROBE_SESSION_LIMITS` | `1` | HW encoder session-capacity probe at startup |
| `BILBYCAST_ALLOW_INSECURE` | `0` | Allow `accept_self_signed_cert: true` |

## Reference: full set of systemd unit limits

| Setting | Default in our unit | Purpose |
|---|---|---|
| `LimitMEMLOCK` | `infinity` | Allows `mlockall` without `CAP_IPC_LOCK` |
| `LimitRTPRIO` | `99` | Allows `SCHED_FIFO` up to priority 99 without `CAP_SYS_NICE` |
| `LimitNOFILE` | `65536` | Plenty of headroom for many concurrent flows / outputs |
| `AmbientCapabilities` | `CAP_NET_ADMIN` | Required for `setsockopt(SO_TXTIME, CLOCK_TAI)` on recent kernels |
| `RestrictRealtime` | `false` | Allows `SCHED_FIFO` requests at all |

## Verifying the full ladder

The 12 broadcast-quality gates in [`testbed/BROADCAST_QUALITY_GATES.md`](../../testbed/BROADCAST_QUALITY_GATES.md)
are the source of truth for "is this layer doing what the docs claim".
Run them per-layer in this order: gate 4 (PCR_AC) → gate 6 (sample SNR) →
gate 8 (long soak) → gate 7 (real receiver). A layer is "in" only when
all four pass at the expected envelope; never report aggregate results
(see `feedback_broadcast_quality_testing.md` rationale).

## Cross-references

- [`wire-pacing.md`](wire-pacing.md) — the wire pacer's design, tier
  table, and why userspace timers are unreliable below ~1 ms.
- [`clocking.md`](clocking.md) — per-flow master clock model
  (SourcePcrPll / PTP / Wallclock) and how it integrates with wire
  pacing.
- [`../packaging/bilbycast-edge.service`](../packaging/bilbycast-edge.service) — the systemd unit
  this doc references.
- [`../packaging/install-edge.sh`](../packaging/install-edge.sh) — the installer that seeds the
  default env file.
- [`../packaging/setup-etf-qdisc.sh`](../packaging/setup-etf-qdisc.sh) — the ETF qdisc installer with
  the `skip_sock_check on` flag.
