# bilbycast-edge Prometheus Metrics Reference

The edge exposes `/metrics` in standard Prometheus text format, served by
`api::stats::prometheus_metrics` and gated behind `/metrics` (public by
default; restricted when `auth.enabled` is on). One scrape returns the
current snapshot of every flow, input, output, tunnel and system resource.

This document lists the metric families emitted today, grouped by subsystem.
Labels are quoted verbatim so you can copy-paste into Grafana / alerting
rules. Every metric is prefixed `bilbycast_edge_`.

## Label conventions

- `flow_id`, `output_id`, `input_id` — stable IDs from `config.json`.
- `leg_role` — `"input"` (receive leg) or `"output"` (send leg). Used where
  the same metric is emitted for both sides.
- `leg` — `"primary"` | `"leg2"` for SMPTE 2022-7 redundancy, or
  `"red"` | `"blue"` for ST 2110 Red/Blue pairs.
- `path_id`, `path_name`, `transport` — per-path labels on bonding metrics.
- `severity` — event-level label on alarm counters.

## Flow-level counters and gauges

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_flow_input_packets_total` | counter | `flow_id` | Packets received on the active input. |
| `bilbycast_edge_flow_input_bytes_total` | counter | `flow_id` | Bytes received on the active input. |
| `bilbycast_edge_flow_input_bitrate_bps` | gauge | `flow_id` | Input bitrate estimate (bits/sec). |
| `bilbycast_edge_flow_input_packets_lost_total` | counter | `flow_id` | Sequence gaps detected. |
| `bilbycast_edge_flow_input_packets_recovered_fec_total` | counter | `flow_id` | Packets recovered by 2022-1 FEC. |
| `bilbycast_edge_flow_output_packets_total` | counter | `flow_id,output_id` | Packets emitted per output. |
| `bilbycast_edge_flow_output_bytes_total` | counter | `flow_id,output_id` | Bytes emitted per output. |
| `bilbycast_edge_flow_output_packets_dropped_total` | counter | `flow_id,output_id` | Packets dropped by a slow output subscriber. |

## SRT / RIST per-leg metrics

Emitted once per leg for every input/output that uses the relevant
transport. Legs appear as `leg="primary"` or `leg="leg2"` (2022-7).

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_srt_rtt_ms` | gauge | SRT round-trip time (milliseconds). |
| `bilbycast_edge_srt_packets_retransmitted_total` | counter | Packets retransmitted on sender side (ARQ). |
| `bilbycast_edge_srt_packets_recovered_total` | counter | Packets recovered via ARQ on receiver side. |
| `bilbycast_edge_rist_rtt_ms` | gauge | RIST RTCP RR-derived RTT (milliseconds). |
| `bilbycast_edge_rist_packets_lost_total` | counter | RIST packets declared lost. |
| `bilbycast_edge_rist_packets_recovered_total` | counter | RIST packets recovered by ARQ or 2022-7. |

Labels: `flow_id,leg_role="input|output",leg="primary|leg2"` plus
`output_id` on outputs.

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

### Aggregate bond counters (one per bond leg, not per path)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_bond_gaps_recovered` | counter | `flow_id,leg_role[,output_id]` | Sequence gaps recovered by the bond ARQ. |
| `bilbycast_edge_bond_gaps_lost` | counter | same | Sequence gaps that could not be recovered. |
| `bilbycast_edge_bond_packets_duplicated` | counter | same | Packets the sender scheduler duplicated across multiple paths. |

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
```

## ST 2110 Red/Blue redundancy

For ST 2110-30/-31/-40 flows with Red/Blue configured.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_st2110_leg_packets_received` | counter | `flow_id,leg="red\|blue"` | Packets received on each leg. |
| `bilbycast_edge_st2110_leg_packets_forwarded` | counter | `flow_id,leg` | Packets accepted post-dedupe (reach downstream). |
| `bilbycast_edge_st2110_leg_packets_duplicate` | counter | `flow_id,leg` | Packets dropped as duplicates by the merger. |
| `bilbycast_edge_st2110_leg_switches` | counter | `flow_id` | Active-leg switch events (2022-7 failovers). |

## TR-101290 analyzer

One counter family per TR-101290 Priority 1 / Priority 2 error class.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_tr101290_ts_packets_total` | counter | `flow_id` | TS packets examined. |
| `bilbycast_edge_tr101290_sync_byte_errors_total` | counter | `flow_id` | 0x47 sync-byte mismatches. |
| `bilbycast_edge_tr101290_cc_errors_total` | counter | `flow_id` | Continuity counter discontinuities. |
| `bilbycast_edge_tr101290_pat_errors_total` | counter | `flow_id` | PAT-related errors. |
| `bilbycast_edge_tr101290_pmt_errors_total` | counter | `flow_id` | PMT-related errors. |
| `bilbycast_edge_tr101290_pcr_errors_total` | counter | `flow_id` | PCR repetition / discontinuity errors. |

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

## Tunnel metrics

Emitted for every active QUIC tunnel.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_tunnel_state` | gauge | `tunnel_id,direction="ingress\|egress"` | 1 = connected, 0 = down. |
| `bilbycast_edge_tunnel_bytes_ingress_total` | counter | same | Bytes received from the peer edge. |
| `bilbycast_edge_tunnel_bytes_egress_total` | counter | same | Bytes sent to the peer edge. |
| `bilbycast_edge_tunnel_rtt_ms` | gauge | same | QUIC RTT estimate. |

## Bandwidth monitor (RP 2129 trust boundary)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bilbycast_edge_flow_bandwidth_limit_mbps` | gauge | `flow_id` | Configured limit (absent when unconfigured). |
| `bilbycast_edge_flow_bandwidth_exceeded` | gauge | `flow_id` | 1 while the flow is over-limit within grace period. |
| `bilbycast_edge_flow_bandwidth_blocked` | gauge | `flow_id` | 1 while ingress is blocked (block action only). |

## System resources

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_system_cpu_percent` | gauge | Whole-system CPU utilisation (0–100). |
| `bilbycast_edge_system_ram_percent` | gauge | Whole-system RAM utilisation (0–100). |
| `bilbycast_edge_system_resource_critical` | gauge | 1 while CPU or RAM is above the configured critical threshold. |

## Event emission

The event stream (`/api/v1/events` on the manager, category constants
listed in [`events-and-alarms.md`](events-and-alarms.md)) is not exposed
as a Prometheus counter family — events are discrete state-change records,
not rates. Alert on event severity via the manager's event stream or via
logs, not `/metrics`.

## Scraping

`/metrics` returns every family above on every scrape. Recommended scrape
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

**Deferred — MP2 / AC-3 / E-AC-3** (stream_types `0x03`, `0x04`,
`0x80` / `0x81`, `0x87` / `0xC2`): these codecs appear in
`audio_pids` with `codec_decoded: false` and a human-readable
`decode_note`. Bitrate, PID counters, and the silence-proxy
fallback still work; R128 values stay `null` until the libavcodec
audio-decode bridge is wired (tracked separately from the
content-analysis subsystem).

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
`0`). The tier requires the `video-thumbnail` Cargo feature (on by
default) — with it disabled the task still runs but never decodes.

### Analyser lag

`ContentAnalysisAccumulator` also carries `lite_drops` /
`audio_full_drops` / `video_full_drops` atomic counters that tick up
on `broadcast::RecvError::Lagged`. A value > 0 means the analyser
task couldn't keep up with the hot path — an informational signal
only; the data path is unaffected.
