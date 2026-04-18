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

### Bonded output (`BondedOutputConfig`)

```json
{
  "id": "bond-out-0",
  "name": "To headend",
  "type": "bonded",
  "active": true,
  "bond_flow_id": 42,
  "paths": [ { ... }, ... ],
  "scheduler": "media_aware",
  "retransmit_capacity": 8192,
  "keepalive_ms": 200,
  "program_number": null
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `bond_flow_id` | u32 | *required* | Must match the receiver end |
| `paths` | array | *required, ≥1* | Paths to transmit across |
| `scheduler` | enum | `media_aware` | `round_robin`, `weighted_rtt`, or `media_aware` (see [Scheduler](#scheduler)) |
| `retransmit_capacity` | usize | 8192 | Sender retransmit buffer capacity (packets). Must exceed `send_rate_pps × max_nack_round_trip_s` |
| `keepalive_ms` | u32 | 200 | Keepalive interval |
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
{ "type": "udp", "bind": "10.0.0.1:5000", "remote": "203.0.113.5:6000" }
```

Sender: `remote` required, `bind` optional (ephemeral if omitted).
Receiver: `bind` required, `remote` ignored.

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

### Scheduler

| Value | Behaviour |
|---|---|
| `round_robin` | Equal-weight rotation. Fine when path health is near-identical (two matched fibre legs) |
| `weighted_rtt` | RTT-weighted rotation — sends more traffic to lower-RTT paths. `Critical`-priority packets (set by upstream tagging, rare without media awareness) are duplicated across the two lowest-RTT paths |
| **`media_aware`** (default) | `weighted_rtt` plus NAL walking: detects H.264 and HEVC IDR frames (H.264 types 5/7/8; HEVC 19/20/21/32/33/34) inside the outbound TS and duplicates them across the two best paths. Non-IDR frames go single-path. **This is the broadcast default** — it gives Peplink-class failover on IDR boundaries without doubling the whole stream |

On an 84/16 theoretical split (5G vs Starlink in testing), the
`media_aware` scheduler delivers **zero lost gaps** under 200 ms RTT /
3 % loss on the Starlink leg because every IDR rides both paths.

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
| `reassembly_overflow` | receiver | Sequence space exceeded buffer — tune `hold_ms` down or fix a path |

**Per-path fields** (`paths` array, one entry per leg):

`id`, `name`, `transport`, `state` (`"alive"` or `"dead"`), `rtt_ms`,
`jitter_us`, `loss_fraction`, `throughput_bps`, `queue_depth`,
`packets_sent`, `bytes_sent`, `packets_received`, `bytes_received`,
`nacks_sent`, `nacks_received`, `retransmits_sent`, `retransmits_received`,
`keepalives_sent`, `keepalives_received`.

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
- **Scheduler choice**: `media_aware` is the right default for video
  flows carried over MPEG-TS. Use `round_robin` for bonded non-video
  data (e.g. bulk file transfers) where IDR detection is a no-op.

## Limitations

- **SRT paths are deferred.** Current transports are UDP / QUIC / RIST.
  An SRT path adapter would give per-leg ARQ + encryption without the
  bond layer reimplementing them; tracked in the `bilbycast-bonding`
  repo as Phase 3.
- **No congestion control.** The bond layer does not probe or back off
  on path saturation. Use the scheduler's `weight_hint` to shape a
  known bandwidth ceiling, or rely on path-layer congestion control
  where available (QUIC, RIST).
- **No cross-path encryption aggregate.** Each path's confidentiality
  is independent: QUIC legs are TLS-encrypted, UDP legs are plaintext,
  RIST legs are plaintext until the RIST library ships Main Profile
  encryption. Wrap an already-encrypted inner protocol (SRT-encrypted
  TS) if end-to-end confidentiality matters and you can't use QUIC for
  all legs.
- **Web UI does not yet render the stats in real time on the topology
  view.** The node detail page shows full per-path tables; topology
  only shows aggregate `up/degraded/idle` state.
