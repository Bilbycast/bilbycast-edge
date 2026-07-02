# Multi-Path Bonding

bilbycast-edge supports media-aware packet bonding across N heterogeneous
network paths via the `bilbycast-bonding` library. A bonded hop aggregates
cellular, fibre, satellite, and any other IP-reachable links into a single
reliable flow, with IDR-frame duplication on the two lowest-RTT paths for
frame-accurate failover.

This document is the authoritative reference. For the UI walkthrough see
[bilbycast-manager/docs/USER_GUIDE.md](../../bilbycast-manager/docs/USER_GUIDE.md#bonded-flows);
for library-internal architecture see
[bilbycast-bonding/CLAUDE.md](../../bilbycast-bonding/CLAUDE.md).

---

## Table of Contents

- [When to use bonding](#when-to-use-bonding)
- [Topology](#topology)
- [Config reference](#config-reference)
  - [Bonded input](#bonded-input-bondedinputconfig)
  - [Bonded output](#bonded-output-bondedoutputconfig)
  - [Path transports](#path-transports)
  - [Scheduler](#scheduler)
- [Worked examples](#worked-examples)
  - [Edge-to-edge SRT over two UDP paths](#edge-to-edge-srt-over-two-udp-paths)
  - [QUIC + UDP hybrid (public internet + LTE)](#quic--udp-hybrid-public-internet--lte)
  - [Three-path heterogeneous bonding (5G + Starlink + fibre)](#three-path-heterogeneous-bonding-5g--starlink--fibre)
- [Stats, events, Prometheus](#stats-events-prometheus)
- [Tuning](#tuning)
- [Limitations](#limitations)

---

## When to use bonding

bilbycast already ships two protocol-native bonding options. Pick the one
that matches your topology:

| Situation | Use |
|---|---|
| Two **SRT** legs to the same SRT receiver (Broadcast or Backup group) | libsrt socket groups (native, via `bilbycast-libsrt-rs`) — configured inline on the SRT output `redundancy` block |
| Two **RIST** legs to a RIST receiver | RIST SMPTE 2022-7 bonding (native, via `bilbycast-rist`) — configured inline on the RIST input/output |
| **N ≥ 2 heterogeneous paths** carrying any inner protocol (SRT, RTMP, RTSP, ST 2110, whatever), with media-aware IDR duplication | **`bonded` input/output** (this document) |

The `bonded` type is transport-agnostic — it wraps raw bytes (or MPEG-TS)
and rides over UDP / QUIC / RIST paths. It does not *replace* the native
options, it's for the heterogeneous-links case they don't cover.

## Topology

Bonding lives at the transit layer between two edges. The source and
destination protocols (SRT, RTMP, RTSP, ST 2110, …) terminate on the edge
endpoints; the public-internet hop rides the bond:

```
[Source device] ── SRT / RTMP / RTSP / ST 2110 / … ──►
                                                        Edge A
                                                          │ flow: srt_input → bonded_output
                                                          ▼
                                     [bond paths: UDP / QUIC / RIST × N]
                                                          │
                                                          ▼
                                                        Edge B
                                                          │ flow: bonded_input → srt_output
                                                        ── SRT / RTMP / RTSP / ST 2110 / … ──►
                                                                                              [Destination]
```

Each edge runs **one flow**. The `bonded_output` on edge A and
`bonded_input` on edge B are peered by matching `bond_flow_id`.

## Config reference

### Bonded input (`BondedInputConfig`)

```json
{
  "id": "bond-in-0",
  "name": "From field unit",
  "type": "bonded",
  "bond_flow_id": 42,
  "paths": [ { ... }, ... ],
  "hold_ms": 500,
  "nack_delay_ms": 30,
  "max_nack_retries": 8,
  "keepalive_ms": 200
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `bond_flow_id` | u32 | *required* | Bond-layer flow ID; must match the sender end |
| `paths` | array | *required, ≥1* | Paths to bind on (see [Path transports](#path-transports)) |
| `hold_ms` | u32 | 500 | Reassembly hold time — how long a gap is held before declaring loss |
| `nack_delay_ms` | u32 | 30 | Base NACK delay after detecting a gap. Gives natural out-of-order arrivals a chance to fill before an ARQ round-trip |
| `max_nack_retries` | u32 | 8 | Max NACK retries per gap before giving up |
| `keepalive_ms` | u32 | 200 | Keepalive interval; drives per-path RTT / liveness |
| `equalization` | enum | `auto` | Per-leg latency equalization mode: `auto` (measure always, time-align legs only when the inter-leg skew is worth the latency — self-configuring, a no-op on a homogeneous bond), `off` (aggregate but never align — lowest latency), `on` (force-align). Accepts the legacy boolean (`true`→`auto`, `false`→`off`). Use the same mode + budget on the matching bonded **output**. See [`bilbycast-bonding/docs/per-leg-equalization.md`](../../bilbycast-bonding/docs/per-leg-equalization.md) |
| `max_bonding_latency_ms` | u32 | 1000 | The single bonding-latency budget — how far a fast leg may be held to align a slow one, and the loss-recovery deadline. Distinct from `hold_ms` (the residual-jitter floor). Should equal the bonded output's value. Falls back to `hold_max_ms` when unset |

### Bonded output (`BondedOutputConfig`)

```json
{
  "id": "bond-out-0",
  "name": "To headend",
  "type": "bonded",
  "active": true,
  "bond_flow_id": 42,
  "paths": [ { ... }, ... ],
  "scheduler": "adaptive",
  "retransmit_capacity": 8192,
  "keepalive_ms": 200,
  "program_number": null
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `bond_flow_id` | u32 | *required* | Must match the receiver end |
| `paths` | array | *required, ≥1* | Paths to transmit across |
| `scheduler` | enum | `adaptive` | `adaptive` (default), `media_aware`, `weighted_rtt`, or `round_robin` (see [Scheduler](#scheduler)) |
| `congestion` | object | — | Optional congestion-control tuning for the `adaptive` scheduler (see [Adaptive scheduler & congestion control](#adaptive-scheduler--congestion-control)) |
| `retransmit_capacity` | usize | 8192 | Sender retransmit buffer capacity (packets). Must exceed `send_rate_pps × max_nack_round_trip_s` |
| `keepalive_ms` | u32 | 200 | Keepalive interval |
| `equalization` | enum | `auto` | Per-leg latency equalization mode (`auto`/`off`/`on`; legacy bool accepted). In `auto`/`on` the sender stamps a 16-byte v2 header so the receiver can time-align legs. Duplicate-all redundancy auto-suppresses alignment (ride-fastest); `on` overrides. Use the same mode + budget on the matching bonded **input** |
| `max_bonding_latency_ms` | u32 | 1000 | The single bonding-latency budget. A leg whose one-way delay would exceed this is benched from carrying unique media (still used for redundancy/FEC) rather than aligning the whole flow to it. Should equal the bonded input's value |
| `path_mtu` | u32 | 1500 | Smallest IP-layer path MTU across the legs, `[576, 9000]`. The sender re-chunks outbound TS payloads at 188-byte boundaries into datagrams that fit this MTU after every per-datagram overhead (IP/UDP 28 B, or 48 B when a leg dials an IPv6 literal or a hostname; relay tunnel framing 16/44 B; RIST/RTP framing 12 B; bond header 12/16 B; AEAD envelope 29 B; FEC repair headroom when `fec` / per-leg FEC is set), so no leg emits an IP-fragmented datagram — cellular CGNAT paths drop fragments and black-hole PMTU discovery, losing oversized datagrams wholesale. The 1500 default derives the classic 1316 B (7 × 188) datagram; a measured ~1000 B cellular bearer derives 752 B (4 × 188). Measure with a DF ping sweep (`ping -M do -s <n>`) over the constrained leg. Sender-side only; the bonded input reassembles in `bond_seq` order regardless of datagram size. Payloads that are not 188-aligned (non-TS essence) cannot be re-chunked and are sent whole (flagged via `oversize_payloads` + the `bond_payload_exceeds_mtu` event) |
| `program_number` | u16 | — | Optional MPTS → SPTS filter applied before bonding (same semantics as other TS-native outputs) |

### Path transports

Each entry in `paths` has a common shell plus a `transport` block:

```json
{
  "id": 0,
  "name": "lte-0",
  "weight_hint": 1,
  "transport": { "type": "udp|rist|quic", ... }
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `id` | u8 | *required* | Path identifier — must be unique within the paths array. Echoed in NACKs so the sender knows which path to fault |
| `name` | string | *required* | Operator-visible label (`"lte-0"`, `"starlink"`, …) |
| `weight_hint` | u32 | 1 | Scheduler weight hint. Higher = more traffic at steady state. `weighted_rtt` / `media_aware` combine this with live RTT |
| `transport` | object | *required* | Per-leg protocol (below) |

**UDP path** (bidirectional, simplest):

```json
{ "type": "udp", "bind": "10.0.0.1:5000", "remote": "203.0.113.5:6000", "interface": "wwan0" }
```

Sender: `remote` required, `bind` optional (ephemeral if omitted).
Receiver: `bind` required, `remote` ignored.

**`interface`** (optional, string, 1–15 chars) — pins egress to a
specific NIC (e.g. `"wwan0"`, `"eth0"`). Critical when multiple paths
share a destination IP: without pinning, the kernel routing table
collapses them onto the same default route and the bond is cosmetic.
On Linux the leg first tries the hard `SO_BINDTODEVICE` bind
(`CAP_NET_RAW`); **if the capability is absent (the normal case for the
edge process) it automatically falls back to the unprivileged
`IP_UNICAST_IF` / `IPV6_UNICAST_IF` egress hint** — so the pin works
with or without the capability (the active mechanism is reported in the
per-leg stats). Granting `CAP_NET_RAW` (via `setcap cap_net_raw+ep` or a
systemd `AmbientCapabilities=CAP_NET_RAW` line) additionally pins the
*receive* side; the edge never needs root. macOS / FreeBSD use
`IP_BOUND_IF` and are unprivileged. Omit the field to let the kernel
decide (or to use source-IP binding + `ip rule` policy routing instead).
Full reference:
[`bilbycast-bonding/docs/nic-pinning.md`](../../bilbycast-bonding/docs/nic-pinning.md).

**RIST path** (unidirectional at the bond layer; per-leg ARQ from the RIST
protocol itself):

```json
{
  "type": "rist",
  "role": "sender",
  "remote": "203.0.113.5:8000",
  "local_bind": null,
  "buffer_ms": 1000
}
```

`role` must be `sender` or `receiver` and should match the bonded
input/output side. `buffer_ms` is the RIST jitter/retransmit buffer
(default 1000 ms). RIST uses `port P` for RTP and `P+1` for RTCP — make
sure both are reachable.

**QUIC path** (TLS 1.3 + DATAGRAM extension, full-duplex):

```json
{
  "type": "quic",
  "role": "client",
  "addr": "203.0.113.5:7000",
  "server_name": "edge-b.example.com",
  "tls": { "mode": "self_signed" }
}
```

| Subfield | Meaning |
|---|---|
| `role` | `"client"` (dial) or `"server"` (accept) |
| `addr` | Client: remote `host:port`. Server: local bind `ip:port` |
| `server_name` | Client SNI / ALPN. Ignored on server role |
| `tls.mode` | `"self_signed"` (dev / loopback / trusted LAN) or `"pem"` |

PEM mode:

```json
{
  "mode": "pem",
  "cert_chain_path": "/etc/bilbycast/bond.crt",
  "private_key_path": "/etc/bilbycast/bond.key",
  "client_trust_root_path": null
}
```

ALPN `bilbycast-bond` is negotiated automatically; other protocols on the
same UDP port (HTTP/3, bilbycast-relay tunnels) stay isolated.

### Relayed and NIC-pinned legs

Each leg is independent: it can go **direct** (point its `remote` / `addr`
at the far edge's public address) or **over a relay**, in any
combination, and both ends can be behind NAT.

A **relayed leg** is just a UDP leg whose `remote` points at a local
**native plain-UDP tunnel** (`transport: "udp"`, see
[CONFIGURATION.md → Native SRT/RIST over relay](CONFIGURATION.md#native-srtrist-over-relay-plain-udp-carrier))
that is loopback-bridged to the leg. The bond's ARQ / FEC / reordering
still run **end-to-end edge ↔ edge**; the relay only forwards
`[tunnel_id][AEAD]` opaquely and cannot read bond traffic. Because both
the tunnel and the far edge dial the relay outbound, a relayed leg
traverses NAT on both ends, and each leg's tunnel can carry a primary +
backup relay for failover. The manager wires this up automatically when
you mark a leg "via relay"; there is no bond-side terminate/re-originate
("bond bridge") — the relay is a generic per-path forwarder for every
path type (tunnels, native SRT/RIST, and individual bond legs alike).

**Per-leg uplink pinning** on a relayed (or direct) UDP leg uses the same
`interface` / `source` / `gateway` fields as any UDP leg, with the same
`SO_BINDTODEVICE` → unprivileged `IP_UNICAST_IF` fallback, so each leg can
egress out its own uplink (5G vs Starlink vs ISP) even on a box with no
`CAP_NET_RAW`.

### Scheduler

| Value | Behaviour |
|---|---|
| `round_robin` | Equal-weight rotation. Fine when path health is near-identical (two matched fibre legs) |
| `weighted_rtt` | RTT-weighted rotation — sends more traffic to lower-RTT paths. `Critical`-priority packets (set by upstream tagging, rare without media awareness) are duplicated across the two lowest-RTT paths |
| `media_aware` | `weighted_rtt` plus NAL walking: detects H.264 and HEVC IDR frames (H.264 types 5/7/8; HEVC 19/20/21/32/33/34) inside the outbound TS and duplicates them across the two best paths. Non-IDR frames go single-path. Gives Peplink-class failover on IDR boundaries without doubling the whole stream, but does **not** congestion-control — superseded by `adaptive` |
| **`adaptive`** (default) | The same NAL-aware IDR duplication as `media_aware`, layered on the **capacity-aware congestion controller** (`CapacityAwareScheduler`). Each leg discovers its usable bitrate and is filled to (not past) it, so the split is proportional to *measured* capacity and a saturated leg spills to one with headroom. **This is the broadcast default** and the right policy for heterogeneous cellular + satellite bonds. Tune it via the per-output [`congestion`](#adaptive-scheduler--congestion-control) block |

On an 84/16 theoretical split (5G vs Starlink in testing), the
`media_aware` / `adaptive` IDR duplication delivers **zero lost gaps**
under 200 ms RTT / 3 % loss on the Starlink leg because every IDR rides
both paths.

### Adaptive scheduler & congestion control

The default `adaptive` scheduler runs a per-leg hybrid loss + delay
controller that *discovers* each link's usable bitrate — probing up while
the link is clean and backing off toward the delivered rate the instant
loss or queue-building delay appears. The split is then proportional to
measured capacity, not RTT. Tune it with the optional per-output
`congestion` block; every field is optional and falls back to the
broadcast-tuned default:

| Field | Unit | Default | Meaning |
|---|---|---|---|
| `min_rate_kbps` | kbps | lib default | Floor a leg's capacity estimate never drops below (while it still carries unique media) |
| `start_rate_kbps` | kbps | lib default | Starting estimate for a unit-weight leg before measurement |
| `loss_low_pct` | % | 0.5 | Below this loss a leg is "clean" and probes up |
| `loss_high_pct` | % | lib default | At/above this loss a leg backs off hard |
| `delay_inflation_ms` | ms | 40 | RTT inflation over the leg's own minimum treated as queue-building congestion. With `delay_inflation_auto` on, this is the *floor* of the auto-derived threshold |
| `delay_inflation_auto` | bool | **on (edge bonded outputs)** | Derive the queue-build threshold per leg from its own windowed baseline RTT instead of a fixed `delay_inflation_ms`. A bufferbloated cellular leg (5G ~100 ms, Starlink ~600 ms baseline) gets a proportionally looser threshold so normal radio jitter doesn't read as congestion; a terrestrial low-RTT leg keeps the tight 40 ms floor. The library default is **off**, but the edge enables it for bonded outputs — set `false` to force the fixed threshold |
| `burst_ms` | ms | lib default | Token-bucket burst depth |
| `probe_cap_mult` | × | 2.0 | Evidence bound — a leg's estimate never exceeds `delivered × this` (except while it's the clean bottleneck). Valid [1.1, 10.0] |
| `rtt_min_window_ms` | ms | 10000 | Window the per-leg minimum-RTT baseline is tracked over (BBR-style). Valid [1000, 120000] |
| `jitter_demote_ms` | ms | 150 | Smoothed jitter above which a leg stops carrying *unique* media (keeps carrying redundancy/FEC copies) so it can't head-of-line-block reassembly. `0` disables. Valid [0, 5000] |

**Clean-bottleneck capacity discovery.** A clean but RTT-jittery radio leg
(5G / Starlink, whose smoothed RTT sits tens of ms above its windowed
minimum even at zero loss) is **no longer pinned at `min_rate`**. The
discovery signal is *delivered-rate vs the current estimate*, not RTT:
when a leg delivers ≥ 85 % of its capacity estimate **and** loss is below
`loss_low`, it's treated as capacity-limited and the estimate probes up
(slow-start, so a starved leg reaches real capacity in ~1 s of control
rounds). Radio jitter in the band between "clean" and full congestion no
longer masquerades as congestion. **Loss is the safety bound** — the
instant probing up induces real loss the controller backs off toward the
delivered rate, so discovery cannot run away. A leg delivering well *under*
its estimate is merely under-offered and holds its estimate.

## Worked examples

### Edge-to-edge SRT over two UDP paths

Source: SRT listener on edge A. Destination: SRT caller pulling from
edge B. Bond over two UDP paths (e.g. two SIMs on a mobile router).

**Edge A (sender side):**

```json
{
  "inputs": [{
    "id": "cam-in",
    "name": "Camera SRT",
    "type": "srt",
    "mode": "listener",
    "local_addr": "0.0.0.0:9000"
  }],
  "outputs": [{
    "id": "bond-out",
    "name": "Bond to Edge B",
    "type": "bonded",
    "bond_flow_id": 42,
    "scheduler": "media_aware",
    "paths": [
      { "id": 0, "name": "sim-a", "transport": { "type": "udp", "remote": "203.0.113.5:5000" }},
      { "id": 1, "name": "sim-b", "transport": { "type": "udp", "remote": "203.0.113.5:5001" }}
    ]
  }],
  "flows": [{
    "id": "feed",
    "name": "Camera feed",
    "input_ids": ["cam-in"],
    "output_ids": ["bond-out"]
  }]
}
```

**Edge B (receiver side):**

```json
{
  "inputs": [{
    "id": "bond-in",
    "name": "Bond from Edge A",
    "type": "bonded",
    "bond_flow_id": 42,
    "paths": [
      { "id": 0, "name": "sim-a", "transport": { "type": "udp", "bind": "0.0.0.0:5000" }},
      { "id": 1, "name": "sim-b", "transport": { "type": "udp", "bind": "0.0.0.0:5001" }}
    ]
  }],
  "outputs": [{
    "id": "srt-out",
    "name": "To studio",
    "type": "srt",
    "mode": "listener",
    "local_addr": "0.0.0.0:9999"
  }],
  "flows": [{
    "id": "feed",
    "name": "Camera feed",
    "input_ids": ["bond-in"],
    "output_ids": ["srt-out"]
  }]
}
```

`bond_flow_id` must match. `id` within each `paths` array must match
(path 0 on the sender is path 0 on the receiver — NACKs use this
identifier).

### QUIC + UDP hybrid (public internet + LTE)

One QUIC leg for the trusted primary path (with TLS), one raw UDP leg
for the LTE secondary:

```json
"paths": [
  {
    "id": 0, "name": "fibre",
    "transport": {
      "type": "quic", "role": "client",
      "addr": "203.0.113.5:7000",
      "server_name": "edge-b.example.com",
      "tls": { "mode": "pem",
               "cert_chain_path": "/etc/bilbycast/bond.crt",
               "private_key_path": "/etc/bilbycast/bond.key" }
    }
  },
  {
    "id": 1, "name": "lte",
    "transport": { "type": "udp", "remote": "203.0.113.5:5000" }
  }
]
```

The QUIC leg gets TLS end-to-end; the UDP leg is plaintext (wrap an
encrypted payload like SRT-encrypted TS if confidentiality is required
on the LTE leg).

### Three-path heterogeneous bonding (5G + Starlink + fibre)

With `media_aware` scheduling and very different RTTs, IDR frames ride
the two fastest paths; non-IDR traffic rides a single path weighted by
live RTT. Integration test coverage (`bonding-transport/tests/bond_heterogeneous.rs`)
validates 84/16-ish traffic splits under 30 ms/200 ms RTT asymmetry.

```json
"paths": [
  { "id": 0, "name": "fibre",    "weight_hint": 4, "transport": { "type": "udp", "remote": "host:5000" }},
  { "id": 1, "name": "5g",       "weight_hint": 2, "transport": { "type": "udp", "remote": "host:5001" }},
  { "id": 2, "name": "starlink", "weight_hint": 1, "transport": { "type": "udp", "remote": "host:5002" }}
]
```

## Stats, events, Prometheus

Per-path stats surface through the standard stats pipeline. Every
bonded input or output carries a `bond_stats` field with:

**Aggregate fields** (sender or receiver as appropriate):

| Field | Side | Meaning |
|---|---|---|
| `state` | both | `"up"`, `"degraded"`, or `"idle"` |
| `flow_id` | both | Matches `bond_flow_id` |
| `role` | both | `"sender"` or `"receiver"` |
| `scheduler` | sender | `"round_robin"`, `"weighted_rtt"`, `"media_aware"` |
| `packets_sent` / `bytes_sent` | sender | |
| `packets_retransmitted` | sender | Count of ARQ retransmits |
| `packets_duplicated` | sender | Packets intentionally duplicated (IDR frames on two paths) |
| `packets_dropped_no_path` | sender | Scheduler couldn't dispatch — bond is hard-failed |
| `packets_received` / `bytes_received` | receiver | |
| `packets_delivered` | receiver | Packets delivered to the application after reassembly |
| `gaps_recovered` | receiver | Gaps filled by ARQ or a second path |
| `gaps_lost` | receiver | Gaps that exceeded `hold_ms` — packet loss |
| `duplicates_received` | receiver | Duplicates absorbed by reassembly |
| `late_stale_drops` (was `reassembly_overflow`) | receiver | Datagrams that arrived too late to use — `bond_seq` already delivered or aged out (a slow-leg copy or retransmit that lost the race), or out of the ring window. NOT ring exhaustion (the ring is fixed at 64k slots). High = a leg is contributing copies too late to help; demote or fix that leg, not the buffer. |

**Per-path fields** (`paths` array, one entry per leg):

`id`, `name`, `transport`, `state` (`"alive"` or `"dead"`), `rtt_ms`,
`jitter_us`, `loss_fraction`, `throughput_bps`, `fec_throughput_bps`,
`wire_throughput_bps`, `queue_depth`, `packets_sent`, `bytes_sent`,
`packets_received`, `bytes_received`, `nacks_sent`, `nacks_received`,
`retransmits_sent`, `retransmits_received`, `keepalives_sent`,
`keepalives_received`.

The three bandwidth fields decompose a leg's send-direction wire load
(sender side; all `0` on the receiver):

- **`throughput_bps`** — media payload rate off the byte counter the
  congestion controller manages: media + ARQ retransmits + duplicated
  (Critical / redundancy) packets + the 12-byte bond header. This is the
  "Bitrate" column.
- **`fec_throughput_bps`** — proactive **FEC repair** rate (combined or
  per-leg XOR / Reed-Solomon). Pure redundancy *on top of* the media
  rate; deliberately kept out of `throughput_bps` so folding it in can't
  read as loss (sent > delivered) to the controller. `0` when FEC is off.
- **`wire_throughput_bps`** — the honest **total** on the leg's wire:
  media + retransmits + duplicates + FEC repair, each including its bond
  header and (on an encrypted UDP leg) the 29-byte AEAD envelope. Equals
  `throughput_bps` + FEC + encryption overhead; excludes only the
  OS-level UDP/QUIC/IP framing below the bond. The aggregate
  `BondLegStats` carries the bond-wide sums of the FEC + wire rates
  alongside `throughput_bps`.

**Prometheus counters** (labels: `flow_id`, `output_id`, `leg_role`,
`path_id`, `path_name`, `transport`):

```
bilbycast_edge_bond_rtt_ms
bilbycast_edge_bond_loss_fraction
bilbycast_edge_bond_path_packets_sent
bilbycast_edge_bond_path_packets_received
bilbycast_edge_bond_path_retransmits_sent
bilbycast_edge_bond_path_nacks_sent
bilbycast_edge_bond_path_nacks_received
bilbycast_edge_bond_path_keepalives_sent
bilbycast_edge_bond_path_dead
bilbycast_edge_bond_gaps_recovered
bilbycast_edge_bond_gaps_lost
bilbycast_edge_bond_packets_duplicated
```

**Events:** category `bond`, severity `info` / `warning` / `critical`.
Path-up / path-down transitions fire as `info` / `warning`; bond-idle
(no alive paths) fires as `critical`.

## Tuning

- **`hold_ms`**: tune to the *worst* path's expected RTT × 2 plus a
  margin for jitter. Too low → `gaps_lost` from late arrivals; too high
  → end-to-end latency grows.
- **`nack_delay_ms`**: should be comparable to the *median* path RTT.
  Lower values retry faster; higher values give natural reordering a
  chance.
- **`retransmit_capacity`**: must exceed `send_rate_pps × max_nack_round_trip_seconds`.
  At 10 kpps and a worst-case 500 ms NACK round-trip, needs ≥ 5000.
  Default 8192 is fine for typical broadcast bitrates.
- **`keepalive_ms`**: faster keepalives detect dead paths sooner but
  consume more bandwidth. 200 ms is a reasonable default.
- **Scheduler choice**: `adaptive` is the right default for video flows
  over heterogeneous links — it congestion-controls per leg. Use
  `media_aware` only if you want IDR duplication without capacity
  discovery, or `round_robin` for bonded non-video data (e.g. bulk file
  transfers) where IDR detection is a no-op.
- **ARQ accounting**: under the `adaptive` scheduler, NACK-driven
  retransmits are charged their real byte size against each leg's
  capacity token bucket, so a NACK storm can't silently self-amplify load
  on a leg that is already dropping. No tuning knob — this just keeps the
  capacity estimate honest under loss.

## Limitations

- **SRT paths are deferred.** Current transports are UDP / QUIC / RIST.
  An SRT path adapter would give per-leg ARQ + encryption without the
  bond layer reimplementing them; tracked in the `bilbycast-bonding`
  repo as Phase 3.
- **Congestion control: on by default.** The `adaptive`
  (`CapacityAwareScheduler`) scheduler — the edge default — runs a
  per-path hybrid loss + delay controller that discovers each leg's
  usable bitrate and splits traffic capacity-proportionally, backing
  off on RTT inflation / loss. Tune it via the per-output `congestion`
  block and cap a metered leg with the per-path `max_bitrate_bps` — full
  field reference in [Adaptive scheduler & congestion
  control](#adaptive-scheduler--congestion-control). The older
  `media_aware` / static schedulers do **not** congestion-control — pick
  `adaptive` for heterogeneous cellular/satellite links.
- **`ingress_dejitter_ms` is ignored on a bonded input.** The bond
  reassembler releases in HOL-gated bursts (a straggler on a slow leg
  stalls the head, then a clump flushes), which the rate-paced ingress
  de-jitter servo can't drain — it would *shed* 15–30 % of the media.
  The bond already delivers in order, and the downstream egress wire-emit
  servo (UDP/RTP) and display A/V clock (HDMI) re-time the bursty stream,
  so a bonded input passes through directly. Setting `ingress_dejitter_ms`
  on a bonded input logs a Warning (`bond_ingress_dejitter_ignored`) and
  has no effect — use `hold_ms` / `max_bonding_latency_ms` to control bond
  latency instead.
- **Cross-path encryption is opt-in and OFF by default.** Set a
  64-hex-char `encryption_key` (same value both ends) to ChaCha20-
  Poly1305-encrypt every UDP and RIST leg of the bond. **Without it,
  UDP/RIST legs are plaintext.** QUIC legs are always TLS-encrypted and
  ignore the key. For confidentiality either set `encryption_key`, use
  QUIC for all legs, or wrap an already-encrypted inner protocol
  (SRT-with-passphrase TS). A bond carrying non-QUIC legs with no
  `encryption_key` set logs a startup warning (`bond_legs_unencrypted`).
- **Web UI does not yet render the stats in real time on the topology
  view.** The node detail page shows full per-path tables; topology
  only shows aggregate `up/degraded/idle` state.
