# bilbycast-edge Prometheus Metrics Reference

The edge exposes `/metrics` in standard Prometheus text format, served by
`api::stats::prometheus_metrics`. It is **public by default and turning auth
on does not change that**: the route is registered on the unauthenticated
router whenever `auth.public_metrics` is `true`, which is its serde default
even with `auth.enabled: true`. Only setting `auth.public_metrics: false`
moves `/metrics` onto the authenticated router (JWT, any role) — see
[`api-security.md`](api-security.md). One scrape returns the current
snapshot of every flow, input and output, plus node-level system resources.

This document lists the metric families emitted today, grouped by subsystem.
Labels are quoted verbatim so you can copy-paste into Grafana / alerting
rules. Every metric is prefixed `bilbycast_edge_`.

## Label conventions

- `flow_id`, `output_id` — stable IDs from `config.json`. There is no
  `input_id` label on any Prometheus series — it appears only as a JSON
  field, on the WS `per_es` / `inputs_live` payloads and on the REST
  `/api/v1/stats` input inventory.
- `leg_role` — `"input"` (receive leg) or `"output"` (send leg). Emitted on
  the RIST and bonding families only; SRT carries no `leg_role`.
- `leg` — SRT uses `"input"` / `"input_leg2"` on inputs and `"leg1"` /
  `"leg2"` on outputs; RIST uses `"leg1"` / `"leg2"` on both sides. No metric
  emits `leg="primary"`, and no metric emits `"red"` / `"blue"` — ST 2110
  Red/Blue counters are not Prometheus families at all (see below).
- `path_id`, `path_name`, `transport` — per-path labels on bonding metrics.
- `stat` — `"min"` / `"avg"` / `"max"` on `bilbycast_edge_flow_output_latency_us`.
- `version`, `domain`, `state`, `pid`, `codec`, `resolution`, `profile`,
  `level`, `sample_rate`, `channels`, `language`, `type` — subsystem-local
  labels on the app-info, PTP and media-analysis families.

## Node-level gauges

Emitted unconditionally at the top of every scrape.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_info` | gauge | `version` | Always 1; carries the build version as a label. |
| `bilbycast_edge_uptime_seconds` | gauge | — | Seconds since process start. |
| `bilbycast_edge_flows_total` | gauge | — | Configured flows (`config.flows.len()`). |
| `bilbycast_edge_flows_active` | gauge | — | Flows currently running. |

## Flow-level counters and gauges

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_flow_input_packets_total` | counter | `flow_id` | Packets received on the active input. |
| `bilbycast_edge_flow_input_bytes_total` | counter | `flow_id` | Bytes received on the active input. |
| `bilbycast_edge_flow_input_bitrate_bps` | gauge | `flow_id` | Input bitrate estimate (bits/sec). |
| `bilbycast_edge_flow_input_packets_lost` | counter | `flow_id` | Sequence gaps detected. |
| `bilbycast_edge_flow_input_fec_recovered_total` | counter | `flow_id` | Packets recovered by 2022-1 FEC. |
| `bilbycast_edge_flow_output_packets_total` | counter | `flow_id,output_id` | Packets emitted per output. |
| `bilbycast_edge_flow_output_bytes_total` | counter | `flow_id,output_id` | Bytes emitted per output. |
| `bilbycast_edge_flow_output_packets_dropped` | counter | `flow_id,output_id` | Packets dropped by a slow output subscriber. |
| `bilbycast_edge_flow_output_latency_us` | gauge | `flow_id,output_id,stat` | End-to-end output latency (µs), one series each for `stat="min"`/`"avg"`/`"max"`. Emitted only when the output has a latency sample. |
| `bilbycast_edge_flow_output_latency_frames` | gauge | `flow_id,output_id` | The same latency expressed in video frames. Emitted only when the frame duration is known. |

`_packets_lost` and `_packets_dropped` are counters despite carrying no
`_total` suffix — that is what `api::stats::prometheus_metrics` emits, so
don't "correct" the names in a dashboard. Six further per-flow families —
`bilbycast_edge_flow_input_redundancy_switches_total`,
`bilbycast_edge_flow_input_packets_filtered`,
`bilbycast_edge_flow_pdv_jitter_us`, `bilbycast_edge_flow_iat_avg_us`,
`bilbycast_edge_flow_output_bitrate_bps` and
`bilbycast_edge_flow_output_fec_sent_total` — are catalogued in
[`api-reference.md`](api-reference.md#get-metrics). The two `_us` gauges are
emitted only when the flow has a jitter / inter-arrival sample.

## SRT / RIST per-leg metrics

Emitted once per leg for every input/output that uses the relevant
transport. **The two transports do not share a label convention**, so a
dashboard selector written for one matches nothing on the other.

### SRT

Two families only. Labels are `flow_id` plus `leg` — there is **no**
`leg_role` on any SRT series. Input legs are `leg="input"` and
`leg="input_leg2"`; output legs carry `output_id` plus `leg="leg1"` or
`leg="leg2"`.

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_srt_rtt_ms` | gauge | SRT round-trip time (milliseconds). |
| `bilbycast_edge_srt_loss_total` | counter | SRT cumulative packet loss (`pkt_loss_total`). |

`srt_loss_total` currently has no 2022-7 arm: it is emitted for the primary
input leg and for `leg="leg1"` on outputs only, so a leg2 loss series does
not exist even though `srt_rtt_ms` has one. Sender-retransmit and
receiver-recovery counters are **not** on `/metrics` at all — they ride the
WS `stats` message as `pkt_retransmit_total` / `pkt_recv_retransmit_total`
on the SRT block of `FlowStats`.

### RIST

Six families, all emitted together per leg. Labels are `flow_id`,
`leg_role="input"|"output"` (plus `output_id` on the output side) and
`leg="leg1"|"leg2"`.

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_rist_rtt_ms` | gauge | RIST RTCP RR-derived RTT (milliseconds). |
| `bilbycast_edge_rist_nack_sent_total` | counter | RIST NACK messages sent by the receiver. |
| `bilbycast_edge_rist_nack_received_total` | counter | RIST NACK messages received by the sender. |
| `bilbycast_edge_rist_retransmit_total` | counter | RIST packets retransmitted by the sender. |
| `bilbycast_edge_rist_packets_lost_total` | counter | RIST packets not recovered by ARQ. |
| `bilbycast_edge_rist_packets_recovered_total` | counter | Packets that filled a slot already flagged as a gap. The HELP string says "recovered via retransmit", but the reorder buffer sets the flag on *any* arrival that closes a gap, so late-but-in-window packets are counted too; `retransmits_received` (WS-only) is the authoritative ARQ count. |

**A leg only ever fills its own half of this table.** `nack_sent_total`,
`packets_lost_total` and `packets_recovered_total` are receiver-side, so
they move on `leg_role="input"` and sit at 0 on `leg_role="output"`;
`nack_received_total` and `retransmit_total` are sender-side and do the
reverse. A ratio that mixes the two halves on one leg is always a division
by zero — pair `packets_recovered_total` with `nack_sent_total` on the
receiving edge, and `retransmit_total` with `nack_received_total` on the
sending one. `packets_lost_total` alone won't separate a genuinely clean
link from one whose ARQ is quietly carrying it: both read ~0.

## PTP clock metrics

Node-level, sampled by the PTP monitor task (every 1 s) which reads
`ptp4l` over its management socket. Always present (even with no ST 2110
flow). All labeled by `domain`. Graph `offset_ns` / `mean_path_delay_ns`
over time to spot clock excursions, asymmetry, or grandmaster drift; alert
on `bilbycast_edge_ptp_locked == 0`.

| Metric | Type | Labels | Notes |
|--------|------|--------|-------|
| `bilbycast_edge_ptp_locked` | gauge | `domain` | 1 when the clock is healthy (`locked` slave **or** `master`), else 0. |
| `bilbycast_edge_ptp_state` | gauge | `domain,state` | State-set: the active `state` label (`locked` / `holdover` / `master` / `acquiring` / `unavailable` / `unknown`) carries value 1. |
| `bilbycast_edge_ptp_offset_ns` | gauge | `domain` | Offset from the grandmaster (ns). **Only emitted while slaved** (`locked`/`holdover`) — the field is meaningless in other states, so the series gaps honestly rather than reporting a stale 0. |
| `bilbycast_edge_ptp_mean_path_delay_ns` | gauge | `domain` | Mean path delay (ns). Slaved-only (same gating). |
| `bilbycast_edge_ptp_steps_removed` | gauge | `domain` | Hops from the grandmaster (BMCA). Slaved-only. |

Discrete excursions also surface as `ptp` events (`ptp_offset_high` /
`ptp_path_delay_high` + recovery, `ptp_grandmaster_changed`) when the
operator sets the `offset_warn_ns` / `path_delay_warn_ns` thresholds on the
Time page — see [`events-and-alarms.md`](events-and-alarms.md) and
[`ptp.md`](ptp.md).

## Bonding metrics (`bilbycast-bonding` + libsrt socket groups)

The custom bonding transport (`bilbycast-bonding`) and native libsrt
socket-group bonding both surface per-path metrics under the same families.
Use the `transport` label to distinguish. `leg_role` is `"input"` on the
receive side and `"output"` on the send side.

### Per-path gauges

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_bond_rtt_ms` | gauge | `flow_id,leg_role,[output_id,]path_id,path_name,transport` | Path round-trip time (ms). |
| `bilbycast_edge_bond_loss_fraction` | gauge | same | Recent loss rate on this path (0.0–1.0). |
| `bilbycast_edge_bond_path_dead` | gauge | same | 1 = path flagged dead by the liveness probe, 0 = alive. |
| `bilbycast_edge_bond_path_throughput_bps` | gauge | same | Per-path bond bandwidth (bits/sec). |

### Per-path counters

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_bond_path_packets_sent` | counter | Packets transmitted on this path. |
| `bilbycast_edge_bond_path_packets_received` | counter | Packets received on this path. |
| `bilbycast_edge_bond_path_retransmits_sent` | counter | ARQ retransmits emitted on this path (sender side). |
| `bilbycast_edge_bond_path_nacks_sent` | counter | NACKs emitted on this path (receiver side). |
| `bilbycast_edge_bond_path_nacks_received` | counter | NACKs received on this path (sender side). |
| `bilbycast_edge_bond_path_keepalives_sent` | counter | Keepalive packets sent to hold the path open. |

Labels on every counter above: `flow_id,leg_role,[output_id,]path_id,path_name,transport`.

### Aggregate bond metrics (one per bond leg, not per path)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_bond_gaps_recovered` | counter | `flow_id,leg_role[,output_id]` | Sequence gaps recovered by the bond ARQ. |
| `bilbycast_edge_bond_gaps_lost` | counter | same | Sequence gaps that could not be recovered. |
| `bilbycast_edge_bond_packets_duplicated` | counter | same | Packets the sender scheduler duplicated across multiple paths. |
| `bilbycast_edge_bond_throughput_bps` | gauge | same | Aggregate bond bandwidth (bits/sec), the sum of the per-path gauges. |

Both `_throughput_bps` families track the JSON `throughput_bps` field —
media + ARQ + duplicates + bond header — so they exclude FEC repair and
AEAD overhead. The JSON-only `fec_throughput_bps` and `wire_throughput_bps`
have no Prometheus family; see [`bonding.md`](bonding.md).

### Useful PromQL for bonding

```promql
# Path loss rate sorted desc — top problem paths across all flows
topk(10, bilbycast_edge_bond_loss_fraction)

# Per-flow RTT spread between paths (max - min)
max by (flow_id) (bilbycast_edge_bond_rtt_ms)
  - min by (flow_id) (bilbycast_edge_bond_rtt_ms)

# Alert on any dead path
bilbycast_edge_bond_path_dead == 1

# Rolling 60s duplication ratio (bandwidth overhead of dup mode)
rate(bilbycast_edge_bond_packets_duplicated[60s])
  / rate(bilbycast_edge_bond_path_packets_sent[60s])

# Share of the bond each path is actually carrying — a leg near 0 is dead weight
bilbycast_edge_bond_path_throughput_bps
  / ignoring (path_id, path_name, transport) group_left
    bilbycast_edge_bond_throughput_bps
```

## ST 2110 Red/Blue redundancy

> **Not exposed through Prometheus.** No `bilbycast_edge_st2110_*` family is
> emitted by `/metrics`. The per-leg counters ride the WS `stats` message and
> `GET /api/v1/stats[/{flow_id}]` as `FlowStats.network_legs`, present only
> when the input has `redundancy` configured.

| Field | Meaning |
|---|---|
| `red.packets_received` / `blue.packets_received` | Packets received on each leg. |
| `red.bytes_received` / `blue.bytes_received` | Bytes received on each leg. |
| `red.packets_forwarded` / `blue.packets_forwarded` | Packets accepted post-dedupe (reach downstream). |
| `red.packets_duplicate` / `blue.packets_duplicate` | Packets dropped as duplicates by the merger. |
| `leg_switches` | Active-leg switch events (2022-7 failovers). |

**Source:** `src/stats/models.rs::NetworkLegsStats` / `LegCounters`,
populated in `src/stats/collector.rs`.

## TR-101290 analyzer

One counter family per TR-101290 Priority 1 / Priority 2 error class. PCR is
split across **two** families — there is no combined `_pcr_errors_total`.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_tr101290_ts_packets_total` | counter | `flow_id` | TS packets examined. |
| `bilbycast_edge_tr101290_sync_byte_errors_total` | counter | `flow_id` | 0x47 sync-byte mismatches. |
| `bilbycast_edge_tr101290_cc_errors_total` | counter | `flow_id` | Continuity counter discontinuities. |
| `bilbycast_edge_tr101290_pat_errors_total` | counter | `flow_id` | PAT-related errors. |
| `bilbycast_edge_tr101290_pmt_errors_total` | counter | `flow_id` | PMT-related errors. |
| `bilbycast_edge_tr101290_pid_errors_total` | counter | `flow_id` | PID errors — expected ES PIDs missing. |
| `bilbycast_edge_tr101290_tei_errors_total` | counter | `flow_id` | Transport error indicator set. |
| `bilbycast_edge_tr101290_crc_errors_total` | counter | `flow_id` | CRC-32 errors on PAT/PMT sections. |
| `bilbycast_edge_tr101290_pcr_discontinuity_errors_total` | counter | `flow_id` | PCR discontinuity errors. |
| `bilbycast_edge_tr101290_pcr_accuracy_errors_total` | counter | `flow_id` | PCR accuracy errors. |

Every family here is emitted per flow only while `FlowStats.tr101290` is
populated — i.e. while the analyzer is running. A stopped analyzer gaps the
series rather than reporting zero, so alert on `absent()` if you need to
distinguish "no errors" from "not watching".

## Media-analysis metrics

Emitted per running flow while `FlowStats.media_analysis` is populated.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_media_video_info` | gauge | `flow_id,pid,codec,resolution,profile,level` | Always 1; carries the video ES description as labels. Unknown fields render as `"unknown"`, never absent. |
| `bilbycast_edge_media_video_framerate` | gauge | `flow_id,pid` | Detected frame rate. Emitted only when a frame rate was resolved. |
| `bilbycast_edge_media_audio_info` | gauge | `flow_id,pid,codec,sample_rate,channels[,language]` | Always 1; `language` is present **only** when the elementary stream declares one, so the label set differs between series of the same family. |
| `bilbycast_edge_media_pid_bitrate_bps` | gauge | `flow_id,pid,type="video\|audio"` | Per-ES bitrate. Suppressed when the measured bitrate is 0. |
| `bilbycast_edge_media_total_bitrate_bps` | gauge | `flow_id` | Total measured bitrate. Suppressed when 0. |

## PID-bus per-ES counters

Populated only when the flow has an active `assembly` (passthrough flows report per-program bitrate via `media_analysis.program_bitrates` instead). One entry per `(input_id, source_pid)` currently tracked on the flow's `FlowEsBus`. Entries for PIDs the current plan is actively routing also carry `out_pid` so operators can pivot off the egress PID.

Shipped on the WS `stats` message as `FlowStats.per_es: Vec<PerEsStats>` — not currently exposed through Prometheus (manager UI consumes the WS snapshot directly). Schema:

| Field | Description |
|-------|-------------|
| `input_id` | Flow-local input ID the ES is pulled from. |
| `source_pid` | Source-side PID on that input. |
| `out_pid` | Egress PID after the assembler's PID remap. `null` on passthrough or when the bus key is observed but not routed. |
| `stream_type` | PMT `stream_type` last observed for this PID (0 before the first PAT/PMT round-trip). |
| `kind` | High-level kind derived from `stream_type` (`video` / `audio` / `subtitle` / `data`; empty until resolved). |
| `packets` | Lifetime TS packets observed on this PID (always 188 × packets = bytes). |
| `bytes` | Lifetime bytes observed. |
| `bitrate_bps` | Rolling 1 Hz bitrate estimate from the shared `ThroughputEstimator`. |
| `cc_errors` | Continuity-counter discontinuities seen on this PID. |
| `pcr_discontinuity_errors` | PCR discontinuities (100 ms threshold matching flow-level TR-101290) — populated only for PCR-bearing PIDs. |

**Source:** `src/engine/ts_es_analysis.rs`, `src/stats/collector.rs`. One lightweight task per bus key — no blocking, no per-packet allocations.

## PCR accuracy trust (`pcr_trust`)

Per-output and flow-rollup PCR accuracy metric. Measures `|ΔPCR_µs − Δwall_µs|` on every successful `socket.send_to` of a PCR-bearing TS packet, fed into a fixed-size rotating reservoir (4096 samples) with exact percentiles computed on snapshot. Catches muxer clock drift, kernel scheduling stalls that slip packets past their PCR cadence, and upstream feeds whose PCRs don't match wall-clock reality.

Reported in microseconds. Exposed per-output on `OutputStats.pcr_trust` (MPTS UDP + raw-TS-over-RTP + RTP-wrapped TS paths only; 302M / RTP-ES / non-PCR outputs carry no samples). Flow rollup on `FlowStats.pcr_trust_flow` aggregates every output's reservoir — max p50 / p95 / p99 / max across outputs.

| Field | Description |
|-------|-------------|
| `samples` | Samples currently in the rotating reservoir (caps at 4096). |
| `cumulative_samples` | Lifetime sample count since output start. |
| `avg_us` | Mean drift across the reservoir. |
| `p50_us` | Median drift. |
| `p95_us` | 95th percentile. |
| `p99_us` | 99th percentile. |
| `max_us` | Worst sample in the reservoir. |
| `window_samples` | Samples in the last ~1 s rolling window. |
| `window_p95_us` | p95 on that window — faster-reacting signal for live dashboards. |

**Sample-skip rules** (important for clean percentiles): the sampler discards and resets state when Δ exceeds 500 ms in either direction. This filters startup jitter, keyframe PCR gaps, stream restarts, and 33-bit PCR wrap. The metric is meaningful only for adjacent PCR-bearing packets within a normal PCR cadence (≤ 100 ms per broadcast standard).

**Source:** `src/stats/pcr_trust.rs`, wired into `OutputStatsAccumulator.pcr_trust`. Recorded from `src/engine/output_udp.rs` (MPTS path) and `src/engine/output_rtp.rs` (raw-TS-over-RTP + RTP-wrapped TS passthrough path).

## Master-clock telemetry (`FlowStats.master_clock`)

> **Not exposed through Prometheus.** Master-clock state is a structured
> per-flow object on the WS `stats` snapshot (`FlowStats.master_clock`,
> `src/stats/models.rs::MasterClockStats`) consumed by the manager UI's
> per-flow telemetry card. It does **not** appear under `/metrics`.

Present on every running flow (every flow has a master clock; `wallclock`
is the last-resort default). Absent only on older edges. Fields:

| Field | Meaning |
|---|---|
| `kind` | The **actual** driving clock: `"source_pcr_pll"` / `"ptp"` / `"audio_master"` / `"wallclock"`. When `fallback_active`, this reflects the rung the data path is really on (e.g. `"wallclock"`). |
| `locked` | `true` when the clock is converged enough for broadcast-grade emit. |
| `rate_offset_ppm` | Recovered rate vs. local CPU clock. Meaningful for PLL masters; `0.0` for `wallclock` / `ptp`. |
| `jitter_us` | Recent p99 jitter over the last 1 s window (µs). |
| `lipsync_offset_90k` | Operator-set lipsync trim (90 kHz ticks, bounded ±18 000). |
| `configured_kind` | What the operator / auto-policy configured (e.g. `"auto"`, `"source_pcr_pll"`). Set when a fallback has fired so the UI can render "PCR PLL → Wallclock (fallback)". `None` in normal operation. |
| `fallback_active` | `true` when the PLL fallback watcher has activated a degraded rung. |
| `fallback_reason` | `"no_pcr_observed"` / `"insufficient_samples"` / `"jitter_too_high"`. Set only when `fallback_active`. |

The fallback transitions also surface as discrete `master_clock` events —
see [`events-and-alarms.md`](events-and-alarms.md#master-clock-master_clock).

**Source:** `src/engine/master_clock.rs` (telemetry snapshot), mirrored into `src/stats/models.rs::MasterClockStats`. Full model: [`clocking.md`](clocking.md#telemetry).

## Edge-added A/V skew (`FlowStats.av_skew`, `OutputStats.av_skew`)

> **Not exposed through Prometheus.** Structured object on the WS `stats`
> snapshot, consumed by the manager UI's "Lip-sync (edge-added)" strip.

The **exact lip-sync error this edge introduces**, derived from the
PTS-touching stages' own `(output − source)` PTS deltas — not estimated
from the mux. At a TS tap true lip-sync is not observable (receivers pair
A/V by PTS regardless of interleave); what IS exactly knowable is how much
the edge shifted the audio↔video PTS relationship vs the source:

| Field | Meaning |
|---|---|
| `skew_ms` | Signed edge-added skew. **> 0 ⇒ audio presented LATER than video** relative to the source's own alignment. EBU R37 applies to THIS number: warn > 20 ms, error > 40 ms. |
| `worst_abs_ms` | Worst \|skew\| since flow start / input switch. |
| `lipsync_trim_ms` | The operator-configured lipsync trim portion included in `skew_ms` (deliberate offset, not defect). |
| `mode` | `"passthrough"` (no PTS-modifying stage active — source A/V preserved bit-exactly, skew 0 by construction) or `"measured"`. |

Contributing stages: `engine::ts_pts_rewriter` (single shared anchor → only
the lipsync trim), `engine::ts_audio_replace` (re-encode sample clock —
where loop-seam drift historically lived), `engine::ts_video_replace`
(source-PTS queue → 0 by design, reported for regression visibility).
`FlowStats.av_skew` covers the ACTIVE input's path; `OutputStats.av_skew`
appears additionally on outputs with their own `audio_encode` /
`video_encode`. Capability bit: `av-skew`.

**Source:** `src/stats/av_skew.rs`.

## A/V mux interleave (`FlowStats.av_interleave_flow`, `OutputStats.av_interleave`)

> Hard-renamed from `av_sync_flow` / `av_sync` 2026-06-06. **This is NOT
> lip-sync** — the old name plus EBU coloring caused repeated false drift
> alarms.

Signed `video_PES_PTS − audio_PES_PTS` as last seen in the byte stream at
egress = how far apart the two ES sit in MUX POSITION. Positive = video
muxed ahead (normal broadcast T-STD geometry, legitimately 0.3–1.5 s).
What it bounds is the **receiver buffering requirement**: a player must
buffer at least this much to pair late-muxed audio with video — consumer
players defaulting to ~1 s caching (VLC) starve their audio queue when
sustained interleave exceeds it.

| Field | Meaning |
|---|---|
| `ewma_ms` | Signed EWMA (~4–6 s time constant). Replaces the old lifetime average, whose slow post-switch convergence read as "drift". |
| `p50/p95/p99_abs_ms`, `max_abs_ms` | Over the ~4096-sample rolling reservoir (≈1 min at PES cadence). |
| `window_p95_abs_ms` | Short 256-sample window. |
| `video_pid` / `audio_pid` | Self-discovered from PAT/PMT. |

Also serialized: `samples` (reservoir occupancy) and
`cumulative_samples`. Resets automatically on input switch (the collector
hooks `set_active_input_id`). **Known limitation**: deltas beyond ±2 s are
treated as discontinuities and discarded, so interleave that jumps past
2 s stops producing samples — the percentiles go stale rather than
reporting the (extreme) value, and `av_interleave_deep` will not fire for
it.

**Source:** `src/stats/av_interleave.rs`, fed per TS packet from
`engine::wire_emit` (UDP/RTP), `engine::output_srt`, `engine::output_rist`.

## Wire-pacing and egress de-jitter telemetry (`OutputStats`)

> **Not exposed through Prometheus.** These are additive fields on the
> per-output WS `stats` snapshot (`src/stats/models.rs::OutputStats`),
> rendered by the manager UI's egress card. They do **not** appear under
> `/metrics`.

Present on outputs that own a UDP socket directly (UDP / RTP / 302M / ST
2110); absent on SRT / RIST / RTMP / HLS / CMAF / WebRTC. Older managers
ignore unknown fields.

| Field | Meaning |
|---|---|
| `wire_pacing_tier` | Active release tier, set once at output start: `"so_txtime"` / `"clock_nanosleep_fifo"` / `"clock_nanosleep"` / `"unpaced"`. |
| `egress_pacing_effective` | Egress pacing mode this output actually runs, resolved at spawn (UDP/RTP-family only). Bare `"forward"` / `"pcr"` / `"servo"` for an explicit config value; `"auto (pcr)"` / `"auto (forward)"` when `egress_pacing` was unset and the engine resolved it (`pcr` iff the flow had a `bonded` input at spawn). |
| `wire_pacing_late` | Datagrams the kernel rejected as late on the SO_TXTIME path. Always 0 on the userspace-sleep paths. |
| `wire_pacing_pinned_cpu` | CPU index the wire-emit thread was pinned to (`BILBYCAST_WIRE_EMIT_CPUS`); `None` when not pinned. |
| `egress_shed` | Datagrams shed by the egress residence cap (compressed / `Lossless` outputs). Non-zero means the release-rate servo hit its ±authority ceiling and the buffer was trimmed to stay inside the receiver T-STD; the receiver re-clocked from PCR. Always 0 on ST 2110 / protocol-paced outputs. |
| `wire_emit_depth` | Current wire-emit queue depth (datagrams in flight between the broadcast subscriber and the wire). The servo holds this near its setpoint; a sustained climb is the early signature of the latency runaway the servo + shed prevent. |

**Source:** `src/engine/wire_emit.rs`, `src/stats/models.rs::OutputStats`. Background: [`wire-pacing.md`](wire-pacing.md), [`egress-dejitter-design.md`](egress-dejitter-design.md).

## Ingress de-jitter telemetry (`InputStats`)

> **Not exposed through Prometheus.** Additive fields on the per-input WS
> `stats` snapshot (`src/stats/models.rs::InputStats`). Not under `/metrics`.

Present on raw UDP / RTP inputs running the ingress de-jitter buffer
(`ingress_dejitter_ms` set); absent otherwise.

| Field | Meaning |
|---|---|
| `ingress_dejitter_shed` | Cumulative packets shed by the input's release-rate servo residence cap. Non-zero means a burst / source-rate offset exceeded the ±5 % servo authority and the buffer was trimmed to bound input latency; the receiver re-clocks from PCR. |
| `ingress_buffer_depth` | Current de-jitter buffer occupancy (packets). The servo holds this near the configured `ingress_dejitter_ms` of content. `None` on inputs without a de-jitter buffer. |

**Source:** `src/engine/ingress_dejitter.rs` (wired via `input_udp.rs` / `input_rtp.rs`), `src/stats/models.rs::InputStats`. Background: [`ingress-dejitter-design.md`](ingress-dejitter-design.md).

## Tunnel telemetry

> **Not exposed through Prometheus.** No `bilbycast_edge_tunnel_*` family is
> emitted — `/metrics` contains no tunnel data at all. The same
> `TunnelStatus` list reaches two other surfaces: the WS `stats` message
> carries it as `payload.tunnels` on every tick, and REST serves it at
> `GET /api/v1/tunnels` for all, `GET /api/v1/tunnels/{id}` for one. A
> tunnel-only node is not a blind spot on the WS path — the periodic arm
> sends `"flows": []` alongside the same tunnel list.

Each entry is a `TunnelStatus` — `id`, `name`, `protocol`, `mode`,
`direction`, `local_addr`, `state`, plus `relay_addrs` /
`active_relay_idx` / `active_relay_addr` on relay-mode tunnels — carrying a
`stats` object:

| Field | Meaning |
|---|---|
| `packets_sent` / `packets_received` | Datagram counts in each direction. |
| `bytes_sent` / `bytes_received` | Byte counts in each direction. |
| `bitrate_in_bps` / `bitrate_out_bps` | Estimated throughput each way. |
| `send_errors` | Transmit failures on the carrier socket. |
| `decrypt_errors` | Inbound AEAD failures (native-UDP carrier). Sustained growth with flat `packets_received` ⇒ `tunnel_encryption_key` mismatch. |
| `connections_total` / `connections_active` | Cumulative and current carrier connections. |

There is **no per-tunnel RTT on any surface** — `TunnelStatsSnapshot` has no
such field, so the QUIC RTT estimate this section used to promise cannot be
served by REST or the WS feed either.

**Source:** `src/tunnel/manager.rs::TunnelStatus` / `TunnelStatsSnapshot`,
`src/api/tunnels.rs`, and the `"tunnels"` key built in
`src/manager/client.rs`.

## Bandwidth monitor (RP 2129 trust boundary)

> **Not exposed through Prometheus.** No `bilbycast_edge_flow_bandwidth_*`
> family is emitted. Bandwidth-limit state rides the WS `stats` message and
> `GET /api/v1/stats[/{flow_id}]` as fields on `FlowStats`.

| Field | Meaning |
|---|---|
| `bandwidth_exceeded` | `true` while the flow is over-limit within the grace period. Omitted from the JSON when false. |
| `bandwidth_blocked` | `true` while ingress is blocked (block action only). Omitted from the JSON when false. |
| `bandwidth_limit_mbps` | Configured limit, for dashboard display. Omitted when no limit is configured. |

**Source:** `src/stats/models.rs::FlowStats`, populated in
`src/stats/collector.rs`.

## System resources

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_system_cpu_percent` | gauge | Whole-system CPU utilisation (0–100). |
| `bilbycast_edge_system_ram_percent` | gauge | Whole-system RAM utilisation (0–100). |
| `bilbycast_edge_system_ram_used_bytes` | gauge | System RAM used, bytes. |
| `bilbycast_edge_system_ram_total_bytes` | gauge | System RAM total, bytes. |
| `bilbycast_edge_system_resources_critical` | gauge | 1 while CPU or RAM is above the configured critical threshold. Note the plural `resources` — the singular form matches no series. |

All five are unlabelled and emitted on every scrape, whether or not
`resource_limits` is configured.

## Event emission

The event stream (`/api/v1/events` on the manager, category constants
listed in [`events-and-alarms.md`](events-and-alarms.md)) is not exposed
as a Prometheus counter family — events are discrete state-change records,
not rates. Alert on event severity via the manager's event stream or via
logs, not `/metrics`.

## Scraping

`/metrics` is a snapshot of what exists right now, not a fixed family list:
the node-level and system gauges are unconditional, as are the four replay
recording / orphan gauges in any build carrying the `replay` feature (they
report 0, they don't vanish), but the per-flow, per-leg, bond, TR-101290 and
media-analysis families appear only while their subsystem is running and
populated, and a few (output latency, media bitrates) are suppressed even
then when the underlying sample is missing. Those series gap rather than
reporting zero. Recommended scrape
interval is 10 s for the bonding + redundancy metrics (fast-moving) and
30 s for everything else. A single 10 s interval works fine; scrape volume
is dominated by the counter cardinality, not sampling frequency.

## Content-analysis metrics (Phase 1–3)

Populated when the flow has `content_analysis.lite | audio_full |
video_full` enabled. Exposed on `FlowStats.content_analysis` as a
structured object (not flat Prometheus counters — the shape is too
hierarchical to map well, and the field is rendered on the manager
flow-detail "Content Health" section). Tier implementations live in
[`src/engine/content_analysis/`](../src/engine/content_analysis/).

### Lite tier (`content_analysis.lite`, default **on**)

All fields are `Option` on the wire — `None` means "data not yet
observed" (SPS not seen, no cues fired, etc.).

| Field | Source | Meaning |
|---|---|---|
| `gop.codec` | PMT stream_type + NAL scan | `"h264"`, `"h265"`, `"mpeg2"`, `"other"` |
| `gop.idr_count` | AVC NAL type 5 / HEVC 16–21 / MPEG-2 GOP header | Lifetime IDR count |
| `gop.idr_interval_frames` | Derived | Mean frames between IDRs |
| `signalling.aspect_ratio` | H.264 / H.265 SPS VUI `aspect_ratio_idc` + crop + pic size | DAR string (`"16:9"`, `"4:3"`, `"21:9"`, else `"N:M"`) |
| `signalling.colour_primaries` | SPS VUI `colour_description_present_flag` | `"bt709"`, `"bt2020"`, `"bt601"`, `"smpte240m"`, … |
| `signalling.transfer_characteristics` | SPS VUI | `"bt709"`, `"smpte2084"`, `"arib-std-b67"`, `"linear"`, … |
| `signalling.matrix_coefficients` | SPS VUI | `"bt709"`, `"bt2020-ncl"`, `"bt2020-cl"`, … |
| `signalling.video_range` | SPS VUI `video_full_range_flag` | `"limited"` / `"full"` |
| `signalling.hdr` | Derived from transfer characteristics | `"sdr"` / `"hdr10"` / `"hlg"` / `"unknown"` |
| `signalling.max_cll` / `max_fall` | SEI payload type 144 (content light level) | cd/m² — HDR10 static metadata |
| `signalling.afd` | ATSC A/53 user-data (country `0xB5`, ATSC `GA94`, type `0x05`) | 4-bit Active Format Description |
| `timecode.seen` | H.264 / H.265 `pic_timing` SEI (payload type 1) | `true` once any timecode has been decoded |
| `timecode.last` | Decoded from `pic_timing` SEI | `"HH:MM:SS:FF"` |
| `timecode.non_monotonic_count` | Lifetime | Count of backward-stepping timecode samples |
| `captions.present` | SEI `user_data_registered_itu_t_t35` + ATSC `GA94` | Captions observed in the last 5 s |
| `captions.packet_count` | Lifetime SEI caption-carrier count | |
| `captions.services` | Derived from T.35 `user_data_type_code` | `["cea-608", "cea-708"]` when GA94 cc_data is detected |
| `scte35.pids` | PMT stream_type 0x86 scan | PIDs carrying SCTE-35 |
| `scte35.cue_count` | Decoded `splice_info_section` count | |
| `scte35.last_command` | Spec table | `"splice_insert"` / `"time_signal"` / etc. |
| `scte35.last_pts` | `splice_time()` in the last cue | 90 kHz ticks |
| `mdi.mdi` | RFC 4445 | `"NDF:MLR"` string |
| `mdi.delay_factor_ms` | `max_iat − avg_iat` over the 1 s window | Peak jitter-buffer depth |
| `mdi.loss_rate_pps` | TS CC discontinuities / window wall-clock | Packets lost per second |

### Audio Full tier (`content_analysis.audio_full`, default **off**)

Published as a `serde_json::Value` so per-PID rows can add fields
without a wire-protocol bump. The top-level `ingress` field reports
which depacketization path the analyser is running:

- `"ts"` — MPEG-TS broadcast (AAC ADTS / LATM decoded via fdk-aac)
- `"pcm"` — ST 2110-30 PM/AM (L16 / L24) or generic RtpAudio
- `"aes3"` — ST 2110-31 (32-bit AES3 subframes, 24-bit audio extracted)

```json
{
  "tier": "audio_full",
  "version": 3,
  "ingress": "pcm",
  "audio_pids": [
    {
      "pid": 0x100,
      "codec": "aac_adts",
      "bitrate_bps": 128000,
      "codec_decoded": true,
      "decode_note": null,
      "sample_rate": 48000,
      "channels": 2,
      "likely_silent": false,
      "mute": false,
      "clip_rate_pps": 0,
      "true_peak_dbtp": -1.2,
      "r128": {
        "m_lufs": -22.1,
        "s_lufs": -22.5,
        "i_lufs": -23.0,
        "lra": 4.2
      }
    }
  ]
}
```

**Decode pipeline** depends on `ingress`:

- **`ts`** — ADTS framing in-task →
  [`crate::engine::audio_decode::AacDecoder`] (Fraunhofer FDK-AAC) →
  planar f32 PCM → [`ebur128`](https://crates.io/crates/ebur128) (pure
  Rust, BS.1770 / EBU R128) with `I | M | S | LRA | TRUE_PEAK` modes.
- **`pcm`** — RTP-payload unpack (L16 BE → i16 → f32 / L24 BE →
  sign-extended i32 → f32) → R128. No decoder. Sample rate / channel
  count come from the input config (`St2110AudioInputConfig.sample_rate`
  / `.channels`, etc.).
- **`aes3`** — Each 4-byte AES3 subframe split into preamble + 24-bit
  audio + V/U/C/P bits; the 24-bit audio is sign-extended and fed to
  R128 with the rest discarded.

All four LUFS values are refreshed every 500 ms at the analyser
publish tick. The wire shape is identical across all three paths so
the manager UI doesn't need separate renderers.

- **`mute`** — hard-mute: set when 2000+ consecutive samples across
  all channels are bit-exact zero (≈ 41.7 ms @ 48 kHz).
- **`clip_rate_pps`** — rolling 1 s count of samples whose magnitude
  meets or exceeds 0.9975 (−0.02 dBFS).
- **`true_peak_dbtp`** — maximum `|sample|` observed since the last
  publish tick, converted to dBTP.
- **`likely_silent`** — preferred path: M-LUFS ≤ −60 for ≥ 2 s. On
  codecs the analyser can't decode it falls back to the
  bitrate-below-1 kbps proxy.

**MP2 / AC-3 / E-AC-3** (stream_types `0x03`, `0x04`, `0x80` /
`0x81` / `0xC1`, `0x87` / `0xC2`): decode in-process via the FFmpeg
audio bridge (`media-codecs` feature, default on) and run the
full R128 / true-peak / mute / clip pipeline. `codec_decoded: true`
on the snapshot, `decode_note: null`. The silence-proxy fallback
remains as a backstop on builds without the `media-codecs`
feature.

### Video Full tier (`content_analysis.video_full`, default **off**)

One decoded frame per sample tick via the in-process FFmpeg decoder
([`video_engine::VideoDecoder`]), metrics computed on the decoded Y
plane:

```json
{
  "tier": "video_full",
  "version": 2,
  "sample_hz": 1.0,
  "samples_taken": 42,
  "samples_decoded": 40,
  "width": 1920,
  "height": 1080,
  "mean_y": 128.54,
  "yuv_sad_freeze": 5.23,
  "freeze_active": false,
  "blur_variance": 4200.1,
  "blockiness": 3.218,
  "letterbox_rows": 0,
  "pillarbox_cols": 0,
  "colour_bar": false,
  "slate": false
}
```

| Field | Meaning |
|---|---|
| `width` / `height` | Decoded frame resolution |
| `mean_y` | Mean Y value (0–255) across the decoded frame |
| `yuv_sad_freeze` | Per-pixel mean absolute Y difference against the previous decoded frame. Near-zero ⇒ frozen. Drives the `content_analysis_video_freeze` event below 0.75 for ≥ 3 s |
| `freeze_active` | Latched freeze alarm state |
| `blur_variance` | Variance of the 3×3 Laplacian on a stride-4 Y sample. Lower ⇒ blurrier |
| `blockiness` | 8×8 DCT-block-boundary gradient minus interior gradient (Wang / Sheikh style). Higher ⇒ over-compressed |
| `letterbox_rows` / `pillarbox_cols` | Count of near-black rows / columns at the top+bottom / left+right edges (`mean Y ≤ 20`) |
| `colour_bar` | `true` when ≥ 80 % of sampled columns have Y variance < 25 — the hallmark of SMPTE EG 1-1990 bars |
| `slate` | Combined freeze + mid-brightness heuristic (freeze SAD below threshold AND `40 ≤ mean_y ≤ 220`) |

Decode runs in `tokio::task::block_in_place` so the tokio reactor is
never held during FFmpeg work. One decode per sample tick bounds CPU
proportionally to `sample_hz`. Only H.264 and H.265 are supported;
other codecs publish no decoded metrics (`samples_decoded` stays at
`0`). The tier requires the `media-codecs` Cargo feature (on by
default) — with it disabled the task still runs but never decodes.

### Analyser lag

`ContentAnalysisAccumulator` also carries `lite_drops` /
`audio_full_drops` / `video_full_drops` atomic counters that tick up
on `broadcast::RecvError::Lagged`. A value > 0 means the analyser
task couldn't keep up with the hot path — an informational signal
only; the data path is unaffected.


## Replay-server metrics

Phase 1 of the in-edge replay server exposes per-recording counters
under `RecordingStats` (in [`src/replay/writer.rs`](../src/replay/writer.rs)).
Each is a `std::sync::atomic::AtomicU64`, surfaced on the
`FlowStats.recording` snapshot when the flow has a `recording`
attribute.

| Counter | Meaning |
|---|---|
| `segments_written` | Number of completed (rolled + atomically-renamed + fsynced) segment files |
| `bytes_written` | Total bytes appended to segment files since flow start |
| `segments_pruned` | Segments removed by retention (oldest-first by mtime) |
| `packets_dropped` | Packets dropped by the broadcast subscriber when the writer's bounded mpsc was full — paired with `replay_writer_lagged` Critical events |
| `index_entries` | Number of entries appended to `index.bin` (one per IDR) |
| `current_pts_90khz` | Most recent PCR-derived PTS observed by the writer; `0` when no PCR seen yet |
| `armed` (bool) | `true` while the writer is actively recording. `false` after `stop_recording`, on a fatal error, **and** while in `PreBuffer` mode (writer is rolling segments to disk but the operator hasn't pressed Start yet) — the manager UI uses this distinction to render "Pre-roll" vs "Recording" labels, and the stall detector gates on `armed = true` |
| `mode` (string, Phase 2 / 1.5) | Wire-string mirror of [`WriterMode`]: `"armed"` when a session is live, `"pre_buffer"` when rolling pre-roll TS, `"idle"` when the writer is stopped. `armed` is a strict subset of `mode == "armed"`; the field exists so the manager UI's tri-state badge and the flow-card `● PRE-ROLL` chip can render without back-deriving state from `armed` plus the flow's `recording.pre_buffer_seconds` config. Older edges omit the field; the manager falls back to `armed`-derived labels |

The writer's `replay_writer_lagged` Critical event is emitted
rate-limited (one per 5 s under sustained lag) so the events feed
isn't drowned during a disk hiccup. `packets_dropped` continues to
increment on every drop so dashboards can chart the real impact.

### Prometheus families

Node-level, unlabelled, gated on the `replay` Cargo feature (default on) —
a binary built without it emits none of these.

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_replay_recordings_count` | gauge | Recordings on disk under the replay root. |
| `bilbycast_edge_replay_recordings_bytes` | gauge | Bytes consumed by those recordings. |
| `bilbycast_edge_replay_orphan_recordings_count` | gauge | Recordings with no flow currently armed against them. |
| `bilbycast_edge_replay_orphan_bytes` | gauge | Bytes consumed by orphan recordings. |
| `bilbycast_edge_replay_root_free_bytes` | gauge | Free bytes on the replay-root filesystem. Emitted only when `replay::replay_disk_usage()` resolves the filesystem. |
| `bilbycast_edge_replay_root_total_bytes` | gauge | Total bytes on that filesystem. Same conditional. |

Alarm guidance for orphan creep is in the manager's
[`USER_GUIDE.md`](../../bilbycast-manager/docs/USER_GUIDE.md).

## SDI telemetry (`sdi_stats` / `sdi_devices`)

Native SDI (Blackmagic DeckLink, `sdi-decklink` Cargo feature) reports on
three surfaces: per-input capture, per-output playout, and a per-host
port enumeration. All are additive and absent on non-SDI entities, so
older managers and non-SDI flows see no field.

The governing property: **an SDI input keeps delivering frames when the
cable is pulled** — the card substitutes bars/black and the edge encodes
them deliberately, because holding the transport stream up is what
downstream wants. So `state`, `bitrate_bps` and every transport-side
counter read perfectly healthy on a dead feed. The `signal_present` bit
is the only thing that says otherwise, which is why it exists.

### Per-input capture (`InputStats.sdi_stats`)

Lock-free atomics (`stats::collector::SdiCaptureStats`), snapshotted on
the regular 1-second cadence. Registered before the first device open,
so a manager that connects while a device is still being retried sees an
honest `signal_present: false` rather than a missing input.

| Field | Meaning |
|---|---|
| `signal_present` | Card is locked to a signal **right now** (`!bmdFrameHasNoInputSource`). Queryable state, not an edge-triggered event, so a manager that connects after a loss still sees it. Starts `false` — an input that has never received a frame is not locked, and reporting `true` before the first frame would be a lie |
| `signal_losses` | Cumulative signal-loss transitions since the input started. Non-zero while `signal_present: true` is the flapping-cable signature — a clean run reads `0` |
| `frames_dropped` | Cumulative frames dropped **in the capture shim** because this edge fell behind the SDI cadence (encoder saturated, thread starved). Invisible to every transport-side counter; distinct from `packets_lost`, which measures the wire. Carried across device re-opens (the shim's own counter restarts per handle, so the edge adds a base) |
| `sessions` | Capture sessions opened. `> 1` means at least one raster change or device re-open has happened |

`PerInputLive.signal_present` (`Option<bool>`) mirrors the same bit onto
the per-input live view, for **every** input in the flow rather than just
the active one — a passive SDI leg with no signal is exactly what an
operator needs to see *before* cutting to it. **`None` means "not an SDI
input, or the card did not say" — never "no signal".** Only
`Some(false)` is a definitive no-signal; collapsing the two would paint
NO SIGNAL onto every non-SDI input.

### Per-output playout (`OutputStats.sdi_stats`)

Sourced from the DeckLink scheduled-playback completion callbacks, not
from the byte stream.

| Field | Meaning |
|---|---|
| `frames_sent` | Cumulative video frames successfully scheduled onto the card |
| `frames_late` | Cumulative frames the card displayed **late** — behind their scheduled slot, but still shown. Scheduling/CPU-pressure signal, **not** lost picture. Informational; deliberately **not** counted as a drop and **not** folded into `packets_dropped` |
| `frames_dropped` | Cumulative frames **dropped** — never presented (card fell behind the cadence, or the edge skipped a frame against a wedged card). Real lost picture, and **also** folded into the generic `packets_dropped` so every output view sees it |

**Keep `frames_late` and `frames_dropped` distinct.** They answer
different questions — "the host is under load" vs "we are losing
picture" — and the card reports them as separate outcomes. Summing them
into one "drops" figure destroys that, and reads a busy-but-correct
output as a broken one. The edge integration made exactly this mistake
in its first cut. Both counters are cumulative across device re-opens
(the card's per-session counters restart at zero, so the edge rebases
them on re-open).

### Per-host port enumeration (`HealthPayload.sdi_devices[]`)

One entry per DeckLink port, refreshed by a 10 s background poller
(`engine::decklink::status`). `IDeckLinkStatus` needs no open handle, so
this covers **all** ports — idle ones and ports held by other processes
— without disturbing live flows, and reads correctly while another
process captures from the same port (`busy: true`).

| Field | Meaning |
|---|---|
| `index` / `name` | Enumeration index and SDK display name (`"DeckLink Quad (1)"`) — the `name` is what a config's `device` matches |
| `sdi_channel` | Connector number parsed from the name |
| `signal_locked` / `reference_locked` / `ancillary_locked` | Input locked to a signal / locked to house reference (genlock) / ANC stream locked. `ancillary_locked` is a **status bit only** — nothing extracts VANC |
| `busy` | Device is held open by some process, this edge included |
| `detected_mode` | Detected raster as a DeckLink mode FourCC (`"Hi50"`). The honest one — `CurrentVideoInputMode` returns a bogus `'ntsc'` default on an unlocked port and is deliberately not exposed |
| `detected_colorspace` / `detected_field_dominance` | e.g. `"r709"` / `"uppr"` |
| `detected_dynamic_range` | `BMDDynamicRange`; `0` = SDR, non-zero = an HDR transfer (HLG / PQ). Lets the manager flag an HDR feed on an SDR chain |
| `sdi_link_config` / `reference_mode` | SDI link configuration (`"lcsl"` = single link) / raster of the house reference when one is patched |
| `pcie_link_speed` / `pcie_link_width` | Negotiated PCIe generation and lane count. A card in an undersized slot is a classic, otherwise-invisible cause of capture drops |

**Every field is optional on the wire, and absent means "the card did not
say" — never "no".** On an unlocked input every `Detected*` field returns
`E_FAIL` and several return `bmdModeUnknown` rather than an error, so
each maps to `Option`. Rendering a missing answer as `false` would
invent a fact the hardware refused to state.

### Capability

`sdi-decklink` on `HealthPayload.capabilities`, advertised only when the
feature is compiled in, the boot probe reached the SDK (Desktop Video
present) **and** at least one card is enumerated in the live status cache.
A host with Desktop Video but no card advertises nothing, and the manager
hides the SDI surfaces entirely — the bit means "this edge has SDI ports",
not "this edge could do SDI". The device list is re-probed every 10 s, so
the bit tracks hot-plug in both directions: a card fitted at runtime starts
offering SDI within a poll interval, with no edge restart.

Config schema: [`configuration-guide.md`](configuration-guide.md#sdi-input-blackmagic-decklink).
Events: [`events-and-alarms.md`](events-and-alarms.md#sdi-input-flow-sdi-decklink-feature).
Subsystem reference: [`sdi.md`](sdi.md).

## Media-player playout telemetry (`InputStats.media_player_stats`)

Lock-free atomics (`stats::collector::MediaPlayerStats`), same
per-input-registered / snapshot-on-cadence shape as `SdiCaptureStats`
above. Present only on `media_player` inputs.

The governing property here mirrors SDI's: `play_source()` returning
`Ok(())` only means the demuxer/muxer didn't error — it says nothing
about whether usable video ever reached the wire. A source with a large
or bursty compressed video sample (e.g. an oversized IDR) can be accepted
and "play" with no error while producing no usable output; see the
writeup filed as issue #67. These fields exist to make that observable
instead of silent.

| Field | Meaning |
|---|---|
| `state` | `starting` / `playing` / `stalled` / `failed` / `exhausted` |
| `current_source_index` | Index of the source currently (or most recently) playing within the input's playlist |
| `video_samples_read` / `video_samples_emitted` | Video access units read from the container / muxed onto the wire |
| `audio_samples_read` / `audio_samples_emitted` | Audio access units read from the container / muxed onto the wire |
| `largest_video_sample_bytes` | Largest single compressed video access unit observed (bytes, pre-Annex-B expansion). A healthy low-bitrate H.264 file's IDR is typically ~1-4 KB; hundreds of KB is the signature of a source likely to trigger bursty delivery |
| `seconds_since_video` | Seconds since the last video sample was muxed onto the wire. `None` before the first video sample of the current/most recent source |
| `pacer_queue_depth` | Current occupancy of the OS-thread pacer's bounded hand-off queue (16 slots). The producer demuxes far faster than realtime, so it fills the queue and then parks on backpressure — a depth at or near capacity is the normal steady state and is **not** on its own a problem signal. Read it together with `pacer_lateness_*`: depth high *and* lateness rising means the pacer cannot drain fast enough; depth high with lateness flat is just the producer being ahead, which is what the queue is for. Reset to 0 when a source's pacer thread exits |
| `pacer_lateness_current_ms` / `pacer_lateness_max_ms` | How far behind its own computed wall-clock deadline the pacer's most recently emitted bundle was, and the high-water-mark since the input started |
| `pacer_lagging` | Latched — true while `pacer_lateness_current_ms` has crossed 250 ms and hasn't yet recovered below 100 ms. Mirrors `media_player_pacer_lagging` / `media_player_pacer_recovered` events (see `docs/events-and-alarms.md`) |
| `generation` | Playback generation — increments exactly once per committed transition. `0` under the legacy loop, which never transitions. The manager echoes it back as `expected_generation` on a `media_player_next` command so a double-click or a retry cannot skip two items; the field is required, because `0` is itself a valid generation and cannot double as "unset". Refreshed by the `media_player_transition_*` events as well as by the stats tick |
| `current_source_elapsed_ms` | Milliseconds elapsed within the current source, measured on the wall clock so it is meaningful for every source kind. Absent before the first source starts |
| `current_source_duration_ms` | Total duration of the current source, when known — the MP4/MOV movie header, or a TS file's size ÷ head-probed mux rate (approximate on VBR), or a still image with `duration_secs` set. Absent for an indefinite still or an unmeasurable TS: render an indeterminate progress bar, never a zero-length one |
| `next_source_ready` | Whether the next playlist item can be opened right now. `true` ready, `false` missing or unresolvable — a `Next` would cut to dead air — absent when unknown or there is no next item |
| `reader_mode` | Which playout path is driving the current source: `ts`, `mp4_whole_file`, `mp4_incremental`, `image`, or `unknown`. The only operator-visible signal of whether the bounded incremental MP4 reader (the default) or the whole-file demux is live, which is the first thing to check when diagnosing media-player RSS |
| `current_source_has_video` | `false` when the current source is audio-only playout (PCR carried on the audio PID), `true` when it has video, absent when the player has not determined it — e.g. a TS source it does not pre-parse. A `false` here is a layout, not a fault |
| `cache_entries` / `cache_resident_bytes` / `cache_max_bytes` | MP4 demux + warm-reader cache occupancy and byte budget. **Sampled at source open, not live**, and `cache_resident_bytes` counts whole-file residency only — it excludes the incremental reader's parsed-table footprint, so it under-reports on the default path. Label it accordingly in any UI |
| `cache_hits` / `cache_misses` | Cumulative across the input's lifetime |

**Scope note**: the media player's async loop hands bundles to the OS-thread
pacer and never blocks on a slow downstream broadcast subscriber (see root
`CLAUDE.md` "Backpressure rule" — input is never blocked). `pacer_lateness_*`
is therefore the closest signal the player itself can observe for "am I
keeping up with the file's own timeline" — it cannot see receiver-side
decode stalls, only whether its own paced delivery is falling behind the
schedule it computed for the file.

## Display-output metrics (`OutputStats.display_stats`)

Local-display outputs (Linux-only, `display` Cargo feature) populate
the `display_stats` sub-block on `OutputStats`. Absent on every
network-egress output, so old managers / non-display outputs see no
field. All counters are lock-free atomic loads sampled at the regular
1-second snapshot cadence.

| Field | Meaning |
|---|---|
| `frames_displayed` | Total frames page-flipped to the connector since the output started |
| `frames_dropped_late` | Frames skipped by the **catch-up drain**. Three conditions must all hold: the frame is more than `max(2 × frame period, 50 ms)` behind the pacing reference (the measured ALSA playout position, or the wall anchor on a muted / video-only output), a **fresher frame is already queued** behind it, and the drift is inside 2 s. A late frame with nothing fresher queued is *presented*, not dropped — holding the panel on an even staler frame would be worse. Drift beyond 2 s is treated as a clock re-base (the usual cause is an input switch, where the new stream's PTS epoch sits below the still-old audio playout) and is also presented. So this counter is the pipeline catching up, not a fault; `display_frame_loss_sustained` deliberately excludes it from both sides of its ratio |
| `frames_repeated` | **Always 0 — retained for dashboard back-compat only.** Nothing increments it: sleep-pacing leaves the previous frame scanned out between flips, so there is no explicit repeat to count (`engine::output_display` says so in its module header). Frames held deliberately show up instead as `present_bucket` mass in the long bins and — with `present_vblank_cadence` on — as the scheduler's hold length plus `cadence_drift_absorbed`. Do not read `0` here as evidence that no frame was ever held; the field cannot report otherwise |
| `frames_dropped_mpsc_full` | Frames the demux+decode child produced and then threw away because the bounded hand-off to the display thread was full — **the dominant "blit/present is too slow" signal**, as distinct from `frames_dropped_late` (arrived too late to show) and `subscriber_lag_events` (decode is too slow). Non-zero is **not** by itself a fault: a source whose frame rate exceeds the panel mode (1080p60 into a 3840x2160@30 panel) decimates here on purpose and sits at a permanent ~50 %. The `display_frame_loss_sustained` Warning therefore latches on presented frames falling ≥ 20 % short of `min(frames handed over, vblanks the panel offered)` over 30 s (clearing at ≤ 5 %), not on this counter's ratio — see `docs/events-and-alarms.md` |
| `audio_underruns` | ALSA `EPIPE` (xrun) recoveries observed by the audio task |
| `av_sync_offset_ms` | Signed **raw** video-vs-audio offset in milliseconds (positive = video ahead of the audio playout), sampled once per presented frame and not smoothed. **It is sampled *before* that frame's present-time sleep**, so it is not an error signal that sits at zero when all is well. The pacer sleeps until the frame is due (`drift − 2 ms`) and then immediately reads the *next* queued frame, which is one source frame period further ahead — so whenever the hand-off queue is backed (the normal case for a decoder running ahead of the panel) this field structurally reads about **one source frame period**: ~40 ms at 25 fps, ~20 ms at 50 fps. That baseline is healthy, not lip-sync error; the frames themselves are presented on time. Read it for *departures* from that baseline — a value that drifts well past one frame period, goes persistently negative (video behind the audio, past the point the pacer can wait for), or wanders — rather than for its distance from zero. With audio muted the reference is the wall anchor rather than the measured `AudioClock` playout position, and the same structure applies. |
| `current_resolution` | Negotiated KMS mode resolution (e.g. `"1920x1080"`) — fixed at modeset, surfaced for the manager UI's flow-card subtitle |
| `current_refresh_hz` | Negotiated refresh rate in Hz |
| `pixel_format` | **Always `"XRGB8888"` today.** It is a literal passed once at the single `set_display_stats` registration, not derived from the live scanout format, so it still reads `XRGB8888` while NV12 / P010 DMA-BUF PRIME frames are being scanned out zero-copy. Use `decoder_kind` plus `download_count` to tell the CPU-blit path from the zero-copy one. Reporting the real scanout fourcc here is not implemented |
| `decoder_kind` | What the runtime decoder resolver actually opened on this host: `"cpu"`, `"cpu (hw unavailable)"` (operator requested a HW backend the host can't open), `"nvdec"`, `"qsv"`, `"vaapi-zerocopy"`, or `"rkmpp-zerocopy"` (the aarch64 Rockchip HW decode path) |
| `video_codec` | Source video codec — `"h264"`, `"hevc"`, `"mpeg2"`, or `"unknown"` before the first decoded frame. `"mpeg2"` is the value behind the `mpeg2_cpu_decode` output field and the `display_hw_decode_mpeg2_pinned` event |
| `audio_codec` | Source audio codec — `"aac"`, `"mp2"`, `"ac3"`, `"eac3"`, `"opus"`, `"ac4"`, `"none"` when audio is muted, or `"unknown"` before the first audio block. **`"ac4"` means silence by design**: there is no AC-4 decoder, so video continues and audio is dropped, with a one-shot Warning `display_audio_ac4_undecodable` |
| `present_interval_count` | Successful page-flips with a predecessor to measure against — the denominator for every field below |
| `present_interval_us_min` / `_us_max` | Shortest / longest gap between successive flips, in µs, since the output started. **`min` is the diagnostic**: a value below one vblank period (e.g. 13 000 µs on a 50 Hz panel) cannot be produced by a vblank-locked flip, so it is direct evidence that two frames were presented back-to-back with no wait |
| `present_bucket` | Eight-bin histogram of the flip interval, in **fixed** µs boundaries: `<10k`, `10–20k`, `20–30k`, `30–38k`, `38–42k`, `42–60k`, `60–100k`, `≥100k`. Bin 4 (`38–42k`) is "on target" **for a 25 fps source specifically** — at 50 fps that role falls to bin 2. Read as a distribution, not a ratio |
| `present_interval_outliers` | Intervals ≥ 10 ms off the running frame period. **Single-run use only** — its reference is the `frame_period_ms` EMA, fed by the very deltas being measured, so the threshold moves with the signal; it scored one identical configuration at 1.4 % and then 34.6 % (#104). Compare arms with `present_bucket`, never with this |
| `present_no_sleep` | Frames presented immediately because they reached the display task at or past their due time, leaving nothing to sleep. Separates "the decoder handed it over late" from "we slept correctly and still missed the vblank" — the outlier count conflates the two and they need different fixes. This is the counter `present_lead_ms` exists to drive to zero |
| `cadence_drift_absorbed` | Frames whose vblank hold departed from the cadence's nominal length, i.e. the source-vs-panel crystal difference being paid off a whole vblank at a time. Only meaningful with `present_vblank_cadence` on. **Expected and periodic** — a 33 ppm difference at 25 fps absorbs roughly once every ten minutes. The diagnostic is the *rate*: frequent absorbs mean the scheduler was handed the wrong rates, not that the panel is drifting fast |
| `cadence_hold_aborted` | Holds abandoned because the driver stopped posting vblanks part-way through. Non-zero means the panel is not receiving the computed cadence at all, so treat any timing measurement from that window as describing something other than the scheduler |
| `cadence_disengaged` | Times the runaway guard switched the cadence off and reverted to wall-clock pacing. **Any non-zero value means the feature failed closed on this output**: the scheduled cadence ran slower than the source, the decode queue shed frames, and the shed frames corrupted the very rate estimate driving the cadence. Expect it to stay at 0 on a host where the cadence suits; three failures retire the feature for the life of the flow |
| `cadence_disengaged_stale` | Times the cadence was switched off because the measured rates stopped matching the running scheduler — a panel mode change or a source rate change. **Unlike `cadence_disengaged` this is an ordinary lifecycle event and says nothing bad about the host**: it is expected to be non-zero on any output that switches sources. What it costs depends on which change caused it. A **source rate change** performs no modeset, so the flip clock's trust bit is never cleared and the cadence re-engages within a couple of seconds — a 10 s rebuild rate-limit that has usually already elapsed, plus 40 frames of rate re-stabilisation. A **panel mode change** does clear that bit, so re-engagement additionally waits a full 600-flip re-measurement window (~24 s at 25 fps) of wall-clock pacing. It is deliberately not charged against the three-strike budget behind `cadence_disengaged` |
| `subscriber_lag_events` | Times the broadcast subscriber returned `Lagged(n)` — one per event regardless of `n`. Sustained growth means demux + decode + display cannot keep up with the input rate. Each one flushes both decoders and resyncs on the next IDR, so it costs a GOP of picture. Raised as the rate-limited `display_subscriber_lagged` Warning |
| `frames_dropped_stale_gen` | Decoded frames shed on arrival at the display task because a switch, a `Lagged` or a PTS jump bumped the shared `frame_gen` after they were produced. Shedding them costs microseconds instead of a full blit + vsync each, which is what gets the new stream's first frame on screen inside one frame period. Growth **in steady state** (no switching) means the flush triggers are firing spuriously. Excluded from `display_frame_loss_sustained` |
| `frames_dropped_unsupported_pixfmt` | Frames dropped because the decoder produced a pixel format the display path has no plane accessor for. The dispatch covers planar YUV 4:2:0 / 4:2:2 / 4:4:4 (8/10/12-bit) and semi-planar NV12 / NV16 / P010LE / P016LE / P210LE / P216LE; everything else drops silently between the throttled `display_unsupported_pixfmt` warnings, which is what this counter is for |
| `pts_jumps_observed` | PTS jumps caught by the demux loop's 1 s heuristic. Spikes on Reolink cameras point at SRT-FEC repair producing out-of-order PTS |
| `aus_skipped_awaiting_keyframe` | Video access units shed while waiting for the first keyframe after a decoder flush (startup, operator switch, `Lagged`, PTS jump). Pre-IDR slices are guaranteed `send_packet` rejections on a flushed decoder, so feeding them would only inflate `send_packet_errors`. Bounded by one GOP per switch when healthy; sustained steady-state growth means the flush triggers are firing spuriously |
| `frame_pts_fallbacks` | Decoded frames whose `frame.pts()` was `None`, forcing the display loop back onto the most recent input PTS. Growth implicates the B-frame display-PTS plumbing |
| `send_packet_errors` | Cumulative `send_packet` failures into the active video decoder. A sustained run (≥ 30 consecutive) triggers a decoder reset and then, if that does not clear it, the `display_hw_decode_runtime_failed` demotion |
| `decoder_demotions` | HW→CPU demotions on this output. `0` is the happy path; `1+` means the operator's chosen backend stopped working and the output silently fell back — the manager renders it as e.g. "qsv (1 demotion)" |
| `decoder_resets` | HW decoder reset-and-reopen cycles on the **same** backend — a wedged session recovered rather than demoted. Rising while `decoder_demotions` stays `0` is the recoverable-but-recurring state: hardware decode is being held, but each cycle costs at least 30 frames of picture, so a steady trickle is a real fault |
| `frames_received_since_open` | Decoded frames produced since the active decoder was last opened. Resets on every demotion and every codec switch, which is what drives the manager's "warming up" badge and the `display_input_switch_acquired` timing |
| `audio_stalled` | `true` when the audio decode stage delivered blocks at some point and has since been frozen for ≥ 5 s while the output runs — audio-only death (a dropped audio PID, a silent demux). Deliberately distinct from `audio_underruns`, which only counts inside an ALSA write attempt and stays `0` when no audio reaches the writer at all. Always `false` for sources that never carried audio |
| `audio_dropped_mpsc_full` | Audio blocks the decode child dropped because the bounded hand-off to the audio task was full. The audio counterpart to `frames_dropped_mpsc_full`, and the only visible sign of the "audio mpsc full → silent drop → audio clock lags real time → `av_sync_offset_ms` drifts positive" failure, which `audio_underruns` cannot see |
| `bars_overlay_enabled` | `true` when the audio-bars + stream-info KMS overlay plane was acquired at display-task start. `false` means the per-frame rasterise has no plane to reach the panel with, and `show_audio_bars: true` outputs fall back to a CPU bake — which is one of the reasons `download_count` can be non-zero on a working zero-copy host |
| `meter_publishes` | `MeterPublisher::publish` calls — each hands a fresh per-PID level snapshot to the display loop. Stuck at `0` while the audio-meter task is alive means it never decoded audio (broadcast lagged, decoder open failed, stream type misclassified). Read it with `bars_overlay_enabled`: the pair separates "no levels computed" from "levels computed, nowhere to draw them" |
| `blit_us_avg` / `blit_us_max` | Mean and worst `blit_and_present` duration, µs. `kms.present()` blocks one vblank (~16 700 µs at 60 Hz), so both are one-vblank-floored; a max past ~33 000 µs means per-frame work is missing vblank slots. **Read with the caveat that an engaged per-vblank cadence inflates the average** — `blit_us` is sampled after the hold loop, so a held frame charges its own hold here |
| `decode_us_avg` / `decode_us_max` | Mean and worst decode-AU duration, µs — `send_packet` + reorder-buffer drain + plane copy. A max past the source frame period (40 ms at 25 fps) on motion-heavy segments is what explains audio momentarily outrunning video. This is the pair `display_frame_loss_sustained`'s remediation tells the operator to compare `blit_us_avg` against to find the bottleneck stage |
| `receive_frame_us_avg` / `receive_frame_us_max` | The raw `avcodec_receive_frame` call duration, µs, split out of `decode_us` — diagnostic instrumentation from the RKMPP stutter investigation, to tell "MPP itself stalled" apart from "our transfer stalled" |
| `rkmpp_transfer_us_avg` / `rkmpp_transfer_us_max` | The other half of that split: RKMPP DRM_PRIME→sysmem transfer duration, µs. Populated only on outputs using the RKMPP backend; `0` everywhere else |
| `upstream_frame_period_us` | Source frame period measured in the decode task, **before** the display queue. Deliberately not the display loop's own estimate, which only sees frames that survived the queue and so reports a slower source once anything is shed — a feedback path that made the cadence hold longer the more it dropped. Compare against the presented rate: a large divergence means frames are being shed between decode and panel |
| `download_us_avg` / `download_count` | Mean cost and count of `download_to_sysmem` (the GPU→CPU copy). Non-zero whenever a hardware frame has to be copied to sysmem — which is **not** the same as "this host cannot do zero-copy". Only one of the four arming conditions means that (the permanent PRIME rejection behind `display_prime_fallback_engaged`; on Intel Gen9 it is every frame, because the plane advertises NV12 on no modifier, #116). The other three fire on hosts where zero-copy works perfectly: an HDR source on an SDR panel (the LUT tonemap needs sysmem), an interlaced source (bob deinterlace needs sysmem), and the audio-bars CPU-bake fallback after the KMS overlay plane is lost. A single frame whose VAAPI / RKMPP PRIME *export* failed is timed here too. Read it against `decoder_kind` and `bars_overlay_enabled` before concluding anything about the host. **One trap on Rockchip**: with `rga-transfer` compiled in, the RGA hardware path satisfies the same condition without calling `download_to_sysmem`, so `download_count` stays flat even though a transfer happened every frame — a flat counter there is not proof of zero-copy. This is the number that says whether removing the copy is worth doing, and it is charged separately from `blit_us_avg` because `decode_us` spans feed + drain and cannot tell a slow decoder from an expensive transfer |

**Why these exist.** Every other counter in this table records a *loss*,
and the stutter class in issue #104 loses nothing — the frames are all
presented, just at irregular times, so `frames_dropped_late` sat at 0–3
while the panel visibly hitched. These are the only fields that measure
what the panel actually shows. The bucket boundaries are **constants on
purpose**; see `present_interval_outliers` above for what happens when
they are not.

Manager UI wiring:

- `static/js/detail/flows.js` reads `display_stats` to render the
  resolution annotation in the per-output table type cell
  (`display (1920x1080@60Hz)`) and the green `DISPLAY` badge in the
  name column.
- The `Displays` Resources sub-card on `/nodes/{id}` reads the
  separate `HealthPayload.display_devices` enumeration (not
  `display_stats`) — that field is the static enumeration of every
  connector the box has, regardless of whether any output is using
  it.
- The flow-card resource impact tile keys off
  `FlowCostPlan.display_outputs` × 275 units (1080p30 baseline) so
  the operator sees the cost before saving the flow.
