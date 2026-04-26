# bilbycast-edge Measurement Reference

This doc answers a single question: **for every value the bilbycast manager
shows on a node's status page, can a broadcast engineer compare it against
an off-the-shelf reference tool and expect the numbers to match?**

For each metric we list:

1. **Where it comes from** — the edge calculation site (file:line) and the
   library doing the heavy lifting.
2. **Verdict** — *Matches reference* / *Approximate* / *Diverges (mislabelled
   or non-standard)* / *Heuristic*.
3. **Reference tool** — the most common off-the-shelf instrument an engineer
   would compare against.
4. **Known divergences** — when, why, and by how much.

Metrics are grouped by the section of the node-status page they appear in.

The status page is rendered by
[`bilbycast-manager/ui/static/js/flows.js`](../../bilbycast-manager/ui/static/js/flows.js)
(lines 865–3811). The manager is a near-transparent passthrough — it does
not recompute or smooth most values. The few exceptions are listed under
[Manager-side caveats](#manager-side-caveats).

> **A note on PCR-derived bitrate vs network bitrate.** The "Media Analysis"
> section reports bitrate derived from PCR cadence (the *nominal* mux rate
> declared by the encoder). The "Input" / "Output" sections report bitrate
> derived from byte-counter deltas on the wire (what your NIC actually sees).
> They are different measurements of different quantities — null-packet
> padding, FEC overhead, and SRT/RIST retransmits all show up in one but
> not the other. Wireshark equals network bitrate; `dvbinspector`'s
> "muxrate" equals PCR-derived bitrate.

---

## Status of the three findings called out in the original audit

All three were addressed.

### 1. "True Peak" is now BS.1770 true peak — FIXED

[`engine/content_analysis/audio_full.rs`](../src/engine/content_analysis/audio_full.rs)
now sources `last_true_peak_dbtp` from `EbuR128::prev_true_peak` after
every `add_frames` call (helper: `update_window_true_peak`), reduced over
all channels per publish window. The crate's polyphase oversampler
(4× < 96 kHz, 2× < 192 kHz) is what BS.1770 specifies. Old per-sample
`max(|s|)` tracker removed.

**Engineer comparison:** Now agrees with Nugen MasterCheck / Dolby DPMS /
`ffmpeg -af ebur128=peak=true` within library rounding.

### 2. MDI is honestly labelled — FIXED (label, not algorithm)

[`stats/models.rs`](../src/stats/models.rs) `MdiStats` carries a new
`model: &'static str = "approx-iat-spread"` discriminator. Docstring is
explicit that this is **not** a strict RFC 4445 implementation and that
absolute numbers will diverge from IneoQuest / Telestream / Bridge VB.
The manager UI now renders the label as **"MDI (approx)"** with a
tooltip that explains the simplification.

The algorithm itself is unchanged — implementing the RFC 4445 VB-overflow
model is still future work. But operators are no longer led to expect
spec-conformant numbers from this field.

### 3. "Seq Gaps" tooltip — FIXED

The UI tooltip on the SMPTE Trust Boundary "Seq Gaps" row now reads:

> Transport-layer sequence-number gaps — packets the RTP / SRT / RIST
> receiver never saw (lost or grossly out of order). Always 0 for raw
> TS-over-UDP inputs (no per-packet sequence number); for those, watch
> the TR-101290 panel's CC Errors counter instead.

The original audit overstated this one — `input_loss` is **actually**
populated from RTP/SRT/RIST sequence-number gaps (see
[`engine/input_rtp.rs:265–278`](../src/engine/input_rtp.rs:265),
[`engine/input_srt.rs:438`](../src/engine/input_srt.rs:438)), not from TS
continuity-counter errors. The label "Seq Gaps" is correct for those
inputs; the only broken bit was the tooltip claiming it covered TS-layer
loss too. Raw TS-over-UDP inputs have no per-packet sequence number and
will read 0 here — operators must look at the TR-101290 CC Errors counter
for TS-layer loss visibility.

---

## Info-card row (top of node-status page)

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| Status | `node.status` (manager-side WS connection) | Matches | n/a | |
| Version | `node.software_version` (auth payload) | Matches | n/a | |
| Uptime | `health.uptime_secs` (boot-time delta) | Matches | `uptime` | |
| Active Flows / Tunnels / Alarms | counters off `FlowManager` / `TunnelManager` / driver | Matches | n/a (own data) | Lags by health interval (15 s). |
| API Address / Monitor Address | from health payload | Matches | n/a | |
| CPU % | [`engine/resource_monitor.rs:119–136`](../src/engine/resource_monitor.rs) (`sysinfo` crate) | Matches | `htop`, `top` | First sample after start may read 0 % — needs ≥ 2 refreshes for accuracy. |
| RAM | same | Matches | `free -h`, `vm_stat` | |

## Per-flow card — Input section

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| State | `flow.input.state` | Matches | n/a | |
| Packets / Bytes received | atomic counter on input task | Matches exactly | Wireshark IO graph | |
| Media Bitrate / Aggregate RX | [`stats/throughput.rs:44–70`](../src/stats/throughput.rs) — counter delta over wall-clock, 1 s minimum window | Matches within ±1 s window | Wireshark "IO Graph (Bits/s, 1 s)", `tshark -q -z io,stat,1` | `Instant`-based monotonic clock; CAS-loop on 1 Hz cadence; sub-bit `f64→u64` cast at the end. |
| Lost | `packets_lost` (MPEG-TS CC discontinuities or RTP seq gaps depending on input type) | Matches per the layer it measures | Wireshark RTP analysis (RTP) or `tsanalyze` (TS) | Mislabelled as "Seq Gaps" in some places — see issue #3. |
| FEC Recovered | counter on the FEC decoder | Matches | librist `ristreceiver --status` (RIST), libsrt stats (SRT) | |
| Redundancy Switches | 2022-7 merger counter | Matches | n/a | |

## Per-flow card — TR-101290 analysis

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| Sync byte / TEI / CC / PAT / PMT / PCR-disc / PCR-acc errors | [`engine/tr101290.rs:130–250`](../src/engine/tr101290.rs) | Matches | `tsduck tsanalyze`, `dvbinspector`, TSReader, `pidview` | CC logic permits duplicate (per spec). PCR jitter is RFC 3550 EWMA — close to most analysers, may differ in the first few samples. |
| Priority 1 / Priority 2 OK | derived booleans | Matches | n/a | |
| TS Packets Analyzed / PAT count / PMT count | counters | Matches exactly | n/a | |
| VSF TR-07 (JPEG XS PID) | PMT scan for stream_type 0x32 | Matches | dvbinspector PMT view | |

## Per-flow card — Content Analysis (Lite)

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| Codec / IDR count / IDR interval | [`engine/content_analysis/lite.rs`](../src/engine/content_analysis/lite.rs), `gop.rs` (NAL-type scan) | Matches | `ffprobe -show_frames`, `mediainfo` | |
| HDR / colour primaries / transfer / matrix / range / aspect | [`engine/content_analysis/signalling.rs`](../src/engine/content_analysis/signalling.rs) + `bitreader.rs` (pure-Rust SPS/VUI decode) | Matches | `ffprobe`, `mediainfo` | Deterministic from bitstream. |
| MaxFALL / MaxCLL | SEI 144 decode | Matches | `ffprobe -show_frames` (HDR metadata) | |
| AFD | ATSC A/53 user-data scan | Matches | `ffprobe`, BMD Decklink monitoring | |
| Captions present (608/708) | [`engine/content_analysis/captions.rs`](../src/engine/content_analysis/captions.rs) | Matches | `ffprobe -show_streams`, `ccextractor` | Presence + packet count only — not decoded text. |
| SCTE-35 cues / PIDs / last command | [`engine/content_analysis/scte35.rs`](../src/engine/content_analysis/scte35.rs) | Matches | dvbinspector, `scte35-decoder` | |
| SMPTE timecode (last + skips) | [`engine/content_analysis/timecode.rs`](../src/engine/content_analysis/timecode.rs) (pic_timing SEI, type 1) | Matches | `mediainfo`, `ffprobe -show_frames` | Skip-count comparison only valid against tools reading pic_timing. Not VITC/LTC. |
| MDI (approx) — NDF + MLR | [`engine/content_analysis/mdi.rs`](../src/engine/content_analysis/mdi.rs) | **Approximate** (signalled via `MdiStats.model = "approx-iat-spread"`) | IneoQuest IVMS, Telestream Inspector | Algorithm is intentionally simplified — see finding #2. |
| Analyser drops | `analyser_drops` counter (broadcast subscriber `Lagged` events) | Matches exactly | n/a | If non-zero, the analyser couldn't keep up with the broadcast channel — back off the tier rather than trusting the stats. |

## Per-flow card — Content Analysis (Audio Full)

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| Loudness M / S / I / LRA (LUFS) | [`audio_full.rs`](../src/engine/content_analysis/audio_full.rs) → `ebur128` crate (BS.1770-certified) | Matches within 0.1 LU | `ffmpeg -af ebur128`, Nugen MasterCheck, Dolby DPMS, `loudness-scanner` | AAC pre-decode via fdk-aac. Channel order assumes BS.1770 standard layout — non-standard PMT channel orders shift K-weighting (small effect). |
| True Peak (dBTP) | [`audio_full.rs`](../src/engine/content_analysis/audio_full.rs) (`update_window_true_peak` helper, queries `EbuR128::prev_true_peak`) | Matches | Nugen MasterCheck, ffmpeg `astats`, `ffmpeg -af ebur128=peak=true` | Polyphase oversampler (4× < 96 kHz, 2× < 192 kHz) per BS.1770. |
| Clip rate (clip/s) | [`audio_full.rs:368–369`](../src/engine/content_analysis/audio_full.rs) (mag ≥ 0.9975 ≈ −0.02 dBFS) | Matches policy; threshold is convention-only | `ffmpeg astats=metadata=1`, `ffprobe`, RX Audio Editor | Some meters use ≥ 0 dBFS, some −0.1 dBFS. Per-sample count, not held state. |
| Hard Mute | [`audio_full.rs`](../src/engine/content_analysis/audio_full.rs) — `HARD_MUTE_DURATION_MS = 40` ms, normalised to a sample count per stream's sample rate via `hard_mute_threshold_samples` | Matches policy | Most meters report mute as boolean state ≥ 1 s | Threshold is now in **time** (40 ms), normalised across 44.1 / 48 / 88.2 / 96 / 192 kHz. Previous fixed `2_000`-sample constant was 48 kHz-only. |
| Silent | [`audio_full.rs:426–440`](../src/engine/content_analysis/audio_full.rs) — M-LUFS ≤ −60 for ≥ 2 s | Matches policy | `ffmpeg -af silencedetect=n=-60dB:d=2` | When no decoder available (MP2 / AC-3 / E-AC-3 in TS — `codec_decoded: false`), falls back to bitrate < 1 kbps. The fallback diverges from R128-based detection. |
| Codec / sample rate / channels | ADTS header / RTP payload type | Matches | `ffprobe -show_streams` | |

## Per-flow card — Content Analysis (Video Full)

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| Decoded resolution / mean Y | [`engine/content_analysis/video_full.rs:282–339`](../src/engine/content_analysis/video_full.rs) (libavcodec decode under `block_in_place`, then pure-Rust Y-plane analysis) | Matches | `ffmpeg -vf cropdetect`, ImageMagick `identify` | Sampled at 1 Hz default (`video_full_hz`, clamped 0.0–30.0). |
| YUV-SAD freeze | per-pixel SAD vs previous Y plane | **Heuristic** | Telestream Vidchecker (proprietary), Tektronix Aurora | Useful for trend alarms — value is not portable across tools. |
| Blur (Laplacian variance) | 3×3 Laplacian on stride-4 Y sample | **Heuristic** | OmniTek OTM, Vidchecker | Lower = blurrier. |
| Blockiness | 8×8 boundary-gradient (Wang/Sheikh-style) | **Heuristic** | Vidchecker, Aurora | |
| Letterbox / Pillarbox | count near-black rows/cols (Y ≤ 20) | Matches | `ffmpeg -vf cropdetect=24:16:0` | |
| Colour bars / Slate | column-uniformity heuristic + freeze + mid-brightness | **Heuristic** | n/a (no industry standard) | |

## Per-flow card — Media Analysis

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| Protocol / payload format / FEC standard / Redundancy | string fields from input config | Matches | n/a | |
| TS Bitrate (`total_bitrate_bps`) | [`engine/media_analysis.rs`](../src/engine/media_analysis.rs) — PCR-derived nominal mux rate | Matches | `dvbinspector` "muxrate", `tsduck tsanalyze` | This is the **nominal** mux rate from PCR cadence — different from observed UDP throughput by null-packet padding. See box at top of doc. |
| Programs (count / number / PMT PID) | PSI parse | Matches | `dvbinspector`, `ffprobe`, `tsanalyze` | |
| Per-video-stream PID / codec / resolution / framerate / profile / level / bitrate | ES header + per-PID counter | Matches | `ffprobe -show_streams`, `mediainfo` | Per-PID bitrate uses the same `ThroughputEstimator`. |
| Per-audio-stream PID / codec / sample rate / channels / language / bitrate | same | Matches | `ffprobe -show_streams`, `mediainfo` | |

## Per-flow card — Trust-boundary metrics (PDV / IAT / packet rate)

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| IAT avg / min / max (µs) | input task wall-clock IAT samples | Matches | Wireshark (per-stream lifetime); we use per-input-task lifetime | Reset on input restart. |
| Jitter (PDV, µs) | RFC 3550 EWMA | Matches | Wireshark "Telephony → RTP → Stream Analysis" | |
| Pkts Filtered | counter on RP 2129 ingress filters | Matches exactly | n/a | |
| Packet Rate | `floor(packets / uptime)` | Matches | Wireshark IO graph (Packets/s) | UI-side derivation. |
| Seq Gaps | `flow.input.packets_lost` | Mislabelled | Wireshark RTP gaps (RTP) or `tsanalyze` (TS) | See pre-flagged issue #3. |
| BW Limit / Status | from input config | Matches | n/a | |

## Per-flow card — PCR Trust (per-output and flow rollup)

[`stats/pcr_trust.rs:126–230`](../src/stats/pcr_trust.rs) — fixed-size
rotating reservoir (4096 samples, ≈ 2.7 min at 25 Hz), exact percentiles via
linear interpolation on a sorted snapshot, modular subtraction in PCR space
(handles 33-bit wrap), 500 ms guard discards samples straddling
discontinuities (keyframe gaps, restarts, wrap).

| Field | Verdict | Reference tool | Notes |
|---|---|---|---|
| `pcr_trust.p50_us / p95_us / p99_us / max_us` | **Matches** (math is correct, well-tested in unit tests) | Custom: hardware-timestamping NIC + `tshark` PCR extraction + replay. No common off-the-shelf tool computes this exactly the same way. | Sample-skip > 500 ms is intentional — would skew toward 0 only on streams with PCR cadence > 500 ms (broken / non-broadcast). |
| `pcr_trust.window_p95_us` | Matches | same | Computed over last 256 samples (~10 s). |
| `pcr_trust.avg_us` | Matches | same | Cumulative drift / cumulative samples — lifetime average. |

## Per-flow card — PID-bus per-ES counters

[`engine/ts_es_analysis.rs`](../src/engine/ts_es_analysis.rs) — lightweight
per-`(input_id, source_pid)` task. Bitrate uses the same
`ThroughputEstimator` as input-side bitrate. CC and PCR-discontinuity logic
mirrors TR-101290 exactly.

| Field | Verdict | Reference tool |
|---|---|---|
| `per_es[].packets / bytes / bitrate_bps / cc_errors / pcr_disc` | Matches | `dvbinspector` per-PID view |

## Per-flow card — Outputs table

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| Packets / Bytes sent | atomic counter on output task | Matches exactly | Wireshark IO graph on egress | |
| Bitrate | `ThroughputEstimator` | Matches within ±1 s | Wireshark IO graph (1 s bin) | |
| Delay (avg µs / target frames) | output delay-buffer accumulator | Matches | n/a (own configured value) | |
| Dropped | counter incremented on `RecvError::Lagged` | Matches subscriber-side lag (not socket TX failures) | n/a | A slow output increments this; it is **not** an ESF/IP-layer drop count. |
| PCR p50 / p99 (µs) | `output.pcr_trust` (see PCR Trust section above) | Matches | as above | |

## Per-flow card — SRT details (per leg)

[`srt/connection.rs:588–664`](../src/srt/connection.rs) — direct passthrough
from `srt-protocol::SrtStats` (whichever backend is active). bilbycast does
not recompute these values.

| Field | Verdict | Reference tool | Notes |
|---|---|---|---|
| RTT avg/min/max | Matches | `srt-live-transmit -s 1`, libsrt CLI tools | Identical to any other libsrt-based peer; clock source is libsrt's internal timer. |
| Loss % | Matches | as above | |
| Retransmit packets | Matches | as above | |
| Bandwidth | Matches | as above | |
| Bonding (switch count, balance ratio) | Matches | `srt-live-transmit grp://` stats | Native libsrt socket-group bonding only. |

## Per-flow card — RIST stats

[`engine/input_rist.rs`](../src/engine/input_rist.rs) /
[`output_rist.rs`](../src/engine/output_rist.rs) — passthrough from
`rist-transport`, wire-verified against librist 0.2.11.

| Field | Verdict | Reference tool |
|---|---|---|
| RTT / loss / retransmit / bandwidth | Matches librist within wire-verified scope | librist `ristreceiver --status`, IneoQuest |

## ST 2110 sections

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| PTP lock state | [`engine/st2110/ptp.rs`](../src/engine/st2110/ptp.rs) — reads `/var/run/ptp4l` via Unix datagram | Matches PTP daemon truth | `pmc -u -b 0 'GET PORT_DATA_SET'`, NMOS IS-04 `/self` | Only as accurate as the host PTP daemon. |
| Red/Blue legs (rx/forwarded/dup/switches) | [`engine/st2110/redblue.rs`](../src/engine/st2110/redblue.rs) | Matches | n/a | |
| Essence flows (PT, IP:port, packet rate) | RFC 4566 SDP gen + counters | Matches | NMOS IS-04 `/senders`, SDP introspection | |

## Tunnels section

| Field | Source | Verdict | Reference tool | Notes |
|---|---|---|---|---|
| Push status (pending/pushed/failed) | manager-side reconciliation | Matches | n/a | |
| Bitrate / packets / bytes | tunnel forwarder counters | Matches | n/a | |

## Appear X sections

Direct passthrough from Appear X JSON-RPC via the gateway sidecar.

| Field | Verdict | Reference tool |
|---|---|---|
| Chassis / cards / firmware / IP I/O / alarms | **Matches Appear X console exactly** | Appear X web UI on the unit |

## Thumbnails

[`engine/thumbnail.rs`](../src/engine/thumbnail.rs) — in-process libavcodec
decode, JPEG 320x180, generated every 10 s.

| Field | Verdict | Notes |
|---|---|---|
| Image content | Matches | |
| Timestamp | **Wrong** | Manager remaps `timestamp` to `Utc::now()` on receipt ([`node_hub.rs:2179`](../../bilbycast-manager/crates/manager-server/src/ws/node_hub.rs)). UI shows manager-receive time, not edge-capture time. ~1 s skew typical, more if WS is congested. |

---

## Manager-side caveats

These apply to **every** value above. Confirmed in
[`bilbycast-manager/crates/manager-server/src/ws/node_hub.rs`](../../bilbycast-manager/crates/manager-server/src/ws/node_hub.rs)
and [`main.rs`](../../bilbycast-manager/crates/manager-server/src/main.rs):

- **Stats coalescing — 500 ms window** on browser broadcasts
  (`main.rs:763–779`). Two stats messages within 500 ms collapse to one;
  the UI never sees the intermediate sample. Negligible at the edge's 1 Hz
  default cadence.
- **5 s forced rebroadcast** even with no new stats. If the edge stops
  sending stats but stays connected, the UI shows stale values **with no
  obvious staleness indicator**. Use `health` cadence (15 s) to detect.
- **Event rate limit — 1000 events/min per node**, silently dropped
  (`node_hub.rs:2038`). A storm of `pcr_discontinuity` or
  `content_analysis_*` events on a bad stream will be truncated. The UI
  count will be lower than a `tcpdump`-of-the-WS would show.
- **No DB persistence for stats/health.** After a node disconnect, the
  UI shows the last cached value with no TTL. Always check the
  connection-state badge before trusting a stat.
- **Thumbnail timestamp remap** — see Thumbnails row above.

---

## Verification methodology

To audit any specific cell, generate a known reference signal, push it
through one bilbycast-edge node, and compare side-by-side.

### Setup

Boot a single edge from `testbed/configs/manual-edge-1.json` and push a
calibrated signal:

```bash
ffmpeg -re -f lavfi -i "testsrc2=size=1920x1080:rate=25" \
       -f lavfi -i "sine=frequency=1000:sample_rate=48000" \
       -c:v libx264 -g 50 -bf 0 -c:a aac -ar 48000 -ac 2 \
       -loudnorm I=-23:LRA=1:TP=-2 \
       -f mpegts udp://127.0.0.1:5000?pkt_size=1316
```

Capture the same UDP stream with `tcpdump -w capture.pcap port 5000` and
also save the raw TS with `ffmpeg -i udp://127.0.0.1:5000 -c copy capture.ts`
for the reference tools.

### Reference comparisons

| Manager UI metric | Reference command | Expected agreement |
|---|---|---|
| Bitrate (input/output) | `tshark -r capture.pcap -q -z io,stat,1` | ±0.5 % |
| TR-101290 counters | `tsduck tsanalyze < capture.ts` | exact |
| Loudness I/M/S/LRA | `ffmpeg -i capture.ts -af ebur128 -f null -` | ±0.1 LU (I), ±0.2 LU (LRA) |
| True Peak (dBTP) | `ffmpeg -i capture.ts -af ebur128=peak=true -f null -` | should agree within library rounding (post-fix) |
| Codec / resolution / framerate | `ffprobe -show_streams capture.ts` | exact |
| IAT / PDV | Wireshark "Telephony → RTP → Stream Analysis" | ±few µs after analyser settles |
| SRT stats | `srt-live-transmit -s 1 file://con?streamid=...` | exact (same library) |
| MDI | IneoQuest IVMS | **Disagreement expected — see issue #2** |
| Thumbnail timestamp | wall-clock screenshot | manager-time, not edge-time |

For each cell, record `expected ≈ measured ± Δ`, verdict, and deviation
cause. Track ongoing audit results in `testbed/STATUS_PAGE_TRUST_AUDIT.md`.

---

## Summary

For practical broadcast use, **the values on the node status page can be
trusted** for:

- Loudness compliance (I / M / S / LRA — uses BS.1770-certified `ebur128`
  crate)
- True Peak compliance (BS.1770 polyphase oversampler via
  `EbuR128::prev_true_peak`)
- TR-101290 quality monitoring (sync, CC, PAT/PMT, PCR)
- ffprobe-equivalent metadata (codec, resolution, framerate, channels, HDR
  signalling, captions, SCTE-35, timecode)
- SRT/RIST per-leg statistics (direct passthrough from libsrt/librist)
- PCR trust percentiles (algorithm verified, well-tested)
- CPU / RAM / uptime
- Bitrate (network-layer; ±1 s window)
- TS bitrate from PCR (nominal mux rate)
- Hard-mute (40 ms threshold, sample-rate normalised)

They remain **heuristic** for video freeze / blur / blockiness / bars /
slate — useful for trend alarms, not absolute reporting. No two
proprietary video-QC tools agree on these either.

The **MDI (approx)** field is intentionally a simplification of RFC 4445;
the wire shape now self-identifies as `model: "approx-iat-spread"` so
operators and downstream tools know to treat it as trending data, not
spec-conformant measurement.
