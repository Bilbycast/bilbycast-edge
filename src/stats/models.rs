// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::Serialize;

/// Per-flow statistics, aggregating one input and zero or more outputs.
#[derive(Debug, Clone, Serialize, Default)]
pub struct FlowStats {
    /// Unique identifier for this flow (matches the config key).
    pub flow_id: String,
    /// Human-readable display name for the flow.
    pub flow_name: String,
    /// Current lifecycle state of the flow.
    pub state: FlowState,
    /// ID of the currently active input, if any. When a flow has multiple
    /// inputs configured, this identifies which one is currently publishing
    /// to the broadcast channel. `None` when the flow has no inputs or is
    /// idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_input_id: Option<String>,
    /// Statistics for the flow's active input leg.
    pub input: InputStats,
    /// Statistics for each configured output, one entry per output.
    pub outputs: Vec<OutputStats>,
    /// Wall-clock seconds since the flow was started.
    pub uptime_secs: u64,
    /// TR-101290 transport stream analysis (present when flow is running).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tr101290: Option<Tr101290Stats>,
    /// Overall flow health derived from all metrics (RP 2129 M6).
    pub health: FlowHealth,
    /// RTP inter-arrival time metrics in microseconds (RP 2129 U2/M2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iat: Option<IatStats>,
    /// Packet delivery variation / jitter in microseconds (RP 2129 U2/M3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdv_jitter_us: Option<f64>,
    /// Media content analysis (codec, resolution, frame rate, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_analysis: Option<MediaAnalysisStats>,
    /// Thumbnail generation statistics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<ThumbnailStats>,
    /// Whether the flow's input bitrate currently exceeds the configured bandwidth limit.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bandwidth_exceeded: bool,
    /// Whether the flow is currently blocked (packets dropped) due to bandwidth limit enforcement.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bandwidth_blocked: bool,
    /// Configured bandwidth limit in Mbps (for dashboard display). Absent if no limit configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bandwidth_limit_mbps: Option<f64>,
    /// PTP clock state for this flow. Populated by ST 2110 flows whose
    /// `clock_domain` is set; absent for non-ST-2110 flows. Backward-compatible
    /// addition — old manager builds ignore unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptp_state: Option<PtpStateStats>,
    /// Per-leg counters for SMPTE 2022-7 Red/Blue dual-network operation.
    /// Present only when the flow's input has `redundancy` set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_legs: Option<NetworkLegsStats>,
    /// Per-essence breakdown when this flow is part of a multi-essence
    /// ST 2110 flow group. Reserved for the flow-group runtime in step 5/6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub essence_flows: Option<Vec<EssenceFlowStats>>,
    /// Per-input liveness snapshot. One entry per input configured on the
    /// flow — lets the manager UI render per-input "NO SIGNAL" / feed-present
    /// state for every input, including passive / non-switched inputs. Absent
    /// on flows without registered inputs so old manager builds see an
    /// unchanged JSON shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs_live: Option<Vec<PerInputLive>>,
    /// Per-elementary-stream counters from the PID bus. Populated only for
    /// flows with an active assembly (passthrough flows rely on
    /// `media_analysis.program_bitrates` instead). One entry per
    /// `(input_id, source_pid)` pair observed on the bus; when an assembler
    /// is running, each entry additionally carries `out_pid` so operators
    /// can pivot their trust signals off the egress PID. PID-bus Phase 8
    /// addition; old manager builds ignore unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_es: Option<Vec<PerEsStats>>,
    /// Flow-wide PCR accuracy rollup — percentiles computed over the union
    /// of every output's PCR trust reservoir. Gives dashboards a single
    /// summary number even when the flow has multiple outputs. Absent when
    /// no output has yet collected enough samples. PID-bus Phase 8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_trust_flow: Option<PcrTrustStats>,
    /// Flow-wide A/V mux-interleave rollup — worst-case (max absolute)
    /// p95 across all outputs. Absent when no output has collected
    /// enough samples. Hard-renamed from `av_sync_flow` 2026-06-06.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av_interleave_flow: Option<AvInterleaveStats>,
    /// Edge-added A/V skew (exact lip-sync error introduced by this
    /// edge's PTS-touching stages) for the ACTIVE input's path. See
    /// [`AvSkewStats`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av_skew: Option<AvSkewStats>,
    /// In-depth content-analysis snapshot. Populated when the flow has
    /// `content_analysis.lite | audio_full | video_full` enabled. Each
    /// sub-field is independently optional so a partial selection (e.g.
    /// Lite-only) round-trips with a minimal JSON payload. Backward-
    /// compatible addition; old manager builds ignore unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_analysis: Option<ContentAnalysisStats>,
    /// Replay-server recording snapshot. Populated only when the flow
    /// has a `recording` block configured and the `replay` Cargo
    /// feature is compiled in. Surfaces the live counters
    /// ([`RecordingSnapshot`] = `armed`, `current_pts_90khz`,
    /// `segments_written`, `bytes_written`, `packets_dropped`,
    /// `segments_pruned`, `index_entries`) so the manager UI can show
    /// a "● REC" / "DROPS" badge on the flow card without polling a
    /// separate endpoint. Backward-compatible addition; old manager
    /// builds ignore unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingSnapshot>,
    /// Master-clock telemetry: which clock the flow is paced against,
    /// whether it is locked, and recovered jitter / rate offset for PLL
    /// masters. Populated for every running flow (every flow has a master
    /// clock — Wallclock is the last-resort default). Backward-compatible
    /// addition; old manager builds ignore unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_clock: Option<MasterClockStats>,
    /// Per-slot liveness rollup for assembled (PID-bus) flows: total
    /// slot count plus the subset currently stalled (no ES packet for
    /// ≥ 5 s, with a 10 s grace after flow start). Published at 1 Hz by
    /// the running TS assembler. Absent on passthrough flows so the
    /// JSON shape is unchanged for them. Backward-compatible addition;
    /// old manager builds ignore unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assembly_health: Option<AssemblyHealth>,
    /// Structured explanation of the current `health` value: one entry per
    /// triggered condition, most-severe first, each carrying a stable
    /// `code`, its own `severity`, and an operator-facing `detail` string.
    /// Empty when the flow is `Healthy`. `health` equals the maximum
    /// `severity` across these reasons, so the badge and its explanation
    /// stay consistent. Lets the manager surface *why* a flow is degraded
    /// without a second round-trip to the event log. Backward-compatible
    /// addition; old manager builds ignore unknown fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health_reasons: Vec<HealthReason>,
}

/// Assembly-slot liveness rollup surfaced on [`FlowStats`] for assembled
/// flows. A "slot" here is one configured `(program, out_pid)` entry of
/// the assembly plan; for Switch slots, only the currently-active leg
/// counts as data. Stall detection lives in `engine::ts_assembler` —
/// this is the wire shape only.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AssemblyHealth {
    /// Number of slots in the running plan, across every program.
    pub total_slots: u32,
    /// Number of entries in `stalled_slots`.
    pub stalled_slot_count: u32,
    /// One entry per currently-stalled slot. Empty when every slot is
    /// receiving ES data (the healthy steady state).
    pub stalled_slots: Vec<StalledSlotStats>,
}

/// One stalled assembly slot within [`AssemblyHealth`].
#[derive(Debug, Clone, Serialize, Default)]
pub struct StalledSlotStats {
    /// MPEG-TS program number the slot belongs to.
    pub program_number: u16,
    /// Egress PID the slot rewrites onto.
    pub out_pid: u16,
    /// Source input feeding the slot. For Switch slots this is the
    /// currently-active leg.
    pub input_id: String,
    /// Source PID on `input_id` the slot subscribes to (post
    /// `pid_overrides`).
    pub source_pid: u16,
    /// Seconds since the slot last forwarded an ES packet. For slots
    /// that have never received data, seconds since the slot was
    /// registered (flow start or hot-swap).
    pub seconds_since_data: u64,
}

/// Master-clock telemetry surfaced on `FlowStats`. Mirror of
/// `engine::master_clock::MasterClockTelemetry` placed in the wire model
/// so the stats crate doesn't have to depend on the engine crate.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MasterClockStats {
    /// Tagged kind: "source_pcr_pll" / "ptp" / "audio_master" / "wallclock".
    /// When `fallback_active`, this reflects the **actual** driving
    /// clock (e.g. "wallclock"); `configured_kind` carries what the
    /// operator originally requested.
    pub kind: String,
    /// True when the clock is converged enough for broadcast-grade emit.
    pub locked: bool,
    /// Recovered rate vs. local CPU clock, in ppm. Only meaningful for
    /// PLL-style masters; `0.0` for Wallclock + Ptp.
    pub rate_offset_ppm: f64,
    /// Recent jitter in microseconds (p99 over last 1 s window).
    pub jitter_us: u64,
    /// Operator-set lipsync trim in 90 kHz ticks. Bounded ±18 000.
    pub lipsync_offset_90k: i64,
    /// What the operator (or auto-policy) configured. Set when a
    /// fallback has fired so the UI can render "PCR PLL → Wallclock
    /// (fallback)". `None` for normal operation. Additive on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_kind: Option<String>,
    /// `true` when a fallback has activated. Driven by the PLL
    /// fallback watcher in `engine::master_clock`. Additive — older
    /// managers ignore it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fallback_active: bool,
    /// Human-readable reason the fallback fired
    /// ("no_pcr_observed" / "insufficient_samples" / "jitter_too_high").
    /// Set only when `fallback_active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// ID of the input currently feeding the clock. For
    /// `source_pcr_pll` this is the active input whose PCR samples the
    /// PLL is tracking; the field refreshes on operator-driven input
    /// switches without re-creating the master. `None` for clock kinds
    /// that don't take their rate from a single ingress stream (`ptp`,
    /// bare `wallclock`). Lets the manager UI render "PLL locked on
    /// <input_id>" / "wallclock fallback after failing on <input_id>".
    /// Additive — older managers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_input_id: Option<String>,
}

#[inline]
fn is_false(b: &bool) -> bool { !*b }

/// Live atomic-counter snapshot of a flow's recording writer.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RecordingSnapshot {
    pub armed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    pub current_pts_90khz: u64,
    pub segments_written: u64,
    pub bytes_written: u64,
    pub segments_pruned: u64,
    pub packets_dropped: u64,
    pub index_entries: u64,
    /// Wall-clock Unix milliseconds of the most recent successful TS
    /// append on this writer. Manager-side stall detection compares
    /// this against `now()` to flag a `recording_stalled` Critical when
    /// `armed` is true but writes have stopped advancing while the WS
    /// connection is healthy. `0` until the first byte lands.
    #[serde(default)]
    pub last_write_unix_ms: u64,
    /// Phase 2 — writer-state discriminator: `"idle"`, `"pre_buffer"`,
    /// or `"armed"`. `armed` alone can't distinguish pre-roll from a
    /// stopped recorder, so the manager UI keys its `Pre-roll` /
    /// `● PRE-ROLL` chips off this field. Forward-compatible: legacy
    /// edge builds omit it, and the manager falls back to
    /// `armed`-derived state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// In-depth content-analysis snapshot. Mirrors the tier shape of
/// [`crate::config::models::ContentAnalysisConfig`] — each sub-block is
/// only present when the corresponding tier is enabled and has produced
/// at least one sample.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ContentAnalysisStats {
    /// Lite (compressed-domain) results. Always present when `lite=true`,
    /// even before the first PSI table arrives, so the manager UI can
    /// render an "analysing…" state distinguishable from "tier off".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lite: Option<ContentAnalysisLiteStats>,
    /// Audio Full results — Phase 2 placeholder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_full: Option<serde_json::Value>,
    /// Video Full results — Phase 3 placeholder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_full: Option<serde_json::Value>,
}

/// Lite content-analysis results. Compressed-domain only — no decode.
///
/// Every sub-field is independently `Option`-typed so an input that doesn't
/// supply a particular signal (e.g. captions on a feed without SEI user-data)
/// produces `None` rather than a zeroed-out struct that suggests the absence
/// is itself the measurement.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ContentAnalysisLiteStats {
    /// GOP / frame-type cadence detected from the active input's video PES.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gop: Option<GopStats>,
    /// Container / codec signalling pulled out of SPS / VUI / SEI / TS
    /// user-data. AR, colour primaries, transfer characteristics, range,
    /// HDR static metadata (MaxFALL / MaxCLL / mastering display), and AFD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signalling: Option<SignallingStats>,
    /// SMPTE 12M-1 / -2 timecode observed on the active video PID
    /// (H.264 / H.265 `pic_timing` SEI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timecode: Option<TimecodeStats>,
    /// Closed-caption presence (CEA-608 / 708 in `user_data_registered_itu_t_t35`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captions: Option<CaptionsStats>,
    /// SCTE-35 splice-information presence on any PMT-listed PID with
    /// stream_type 0x86.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scte35: Option<Scte35Stats>,
    /// Media Delivery Index (RFC 4445) computed from input-side packet
    /// timing — measured per-input on every transport, including the
    /// post-recovered TS stream out of SRT / RIST.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdi: Option<MdiStats>,
    /// Whether the analyser is currently keeping up with the broadcast
    /// channel. Increments on `RecvError::Lagged` from the flow broadcast
    /// channel and is informational only — alarms are not raised because
    /// the data path is unaffected.
    pub analyser_drops: u64,
}

/// GOP structure observed from the video PES. Counters are lifetime totals;
/// the cadence fields are smoothed over the most recent window.
#[derive(Debug, Clone, Serialize, Default)]
pub struct GopStats {
    /// PID of the analysed video stream. `None` until a PMT has been seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_pid: Option<u16>,
    /// Codec family (`"h264"`, `"h265"`, `"mpeg2"`, `"other"`).
    pub codec: String,
    /// Frame count by NAL/slice type since flow start.
    pub idr_count: u64,
    pub i_count: u64,
    pub p_count: u64,
    pub b_count: u64,
    /// Mean distance (in frames) between successive IDR / I-frames.
    /// `None` until at least two IDR / I-frames have been observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idr_interval_frames: Option<f32>,
    /// Whether the most recently completed GOP was closed
    /// (no B-frame references across the IDR boundary). H.264-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_gop: Option<bool>,
}

/// Container / codec signalling pulled from SPS / VUI / SEI / TS user-data.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SignallingStats {
    /// Display aspect ratio derived from `sample_aspect_ratio_idc` (or
    /// extended SAR) and decoded width / height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    /// Colour primaries (`"bt709"`, `"bt2020"`, `"bt601"`, `"bt470bg"`,
    /// `"smpte240m"`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour_primaries: Option<String>,
    /// Transfer characteristics (`"bt709"`, `"smpte2084"`, `"arib-std-b67"`,
    /// `"linear"`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer_characteristics: Option<String>,
    /// Matrix coefficients (`"bt709"`, `"bt2020-ncl"`, `"bt2020-cl"`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matrix_coefficients: Option<String>,
    /// Pixel range — `"limited"` (TV) / `"full"` (PC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_range: Option<String>,
    /// HDR transfer family (`"sdr"`, `"hdr10"`, `"hlg"`, `"unknown"`).
    pub hdr: String,
    /// Maximum content light level in cd/m² (HDR10 SEI 144).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cll: Option<u32>,
    /// Maximum frame-average light level in cd/m² (HDR10 SEI 144).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fall: Option<u32>,
    /// Active Format Description (CEA-708 / SMPTE 2016-1) most recently
    /// observed in TS user-data PES. `None` when no AFD descriptor has
    /// arrived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub afd: Option<u8>,
}

/// SMPTE 12M timecode observed on the video PES.
#[derive(Debug, Clone, Serialize, Default)]
pub struct TimecodeStats {
    /// Whether timecode has been observed at any point since flow start.
    pub seen: bool,
    /// Most recent timecode in `HH:MM:SS:FF` (or `;FF` when drop-frame).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// Whether the cadence has been monotonic since the first sample
    /// (no skip-backwards). Resets to `true` on any non-monotonic step
    /// to avoid a single bad sample latching the alarm forever.
    pub monotonic: bool,
    /// Number of non-monotonic timecode steps observed since flow start.
    pub non_monotonic_count: u64,
}

/// CEA-608 / 708 closed caption presence detection.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CaptionsStats {
    /// `true` if at least one caption packet has been observed in the
    /// last 5 seconds. Drives the `content_analysis_caption_lost` event
    /// when it transitions from true → false on a flow that previously
    /// had captions.
    pub present: bool,
    /// Lifetime count of caption packets carried in SEI user-data.
    pub packet_count: u64,
    /// Service variants seen so far (`"cea-608"`, `"cea-708"`).
    pub services: Vec<String>,
}

/// SCTE-35 splice-information presence detection.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Scte35Stats {
    /// PIDs carrying stream_type 0x86 (SCTE-35) per the most recent PMT.
    pub pids: Vec<u16>,
    /// Cumulative count of `splice_info_section`s observed.
    pub cue_count: u64,
    /// Most recent splice command type (`"splice_null"`, `"splice_insert"`,
    /// `"time_signal"`, `"bandwidth_reservation"`, `"private_command"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_command: Option<String>,
    /// Most recent cue's PTS, when present (`splice_insert` /
    /// `time_signal`). 90 kHz ticks; convert to seconds with `/ 90000`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pts: Option<u64>,
}

/// Media Delivery Index, approximated. Inspired by RFC 4445 but **not** a
/// strict implementation — see the `model` discriminator and the module
/// docs in `engine::content_analysis::mdi` for the exact algorithm.
///
/// `delay_factor_ms` is computed as `peak_iat − mean_iat` over the current
/// window (a buffer-depth proxy), not the per-RFC-4445 VB-overflow model.
/// `loss_rate_pps` is derived from MPEG-TS continuity-counter
/// discontinuities per second on the post-recovered stream — one CC gap
/// can hide N missing packets, and the field under-reports loss that
/// ARQ/FEC silently recovered upstream.
///
/// Suitable for **trending and alarming**; do **not** report these values
/// against a strict RFC 4445 spec-conformance bar — IneoQuest IVMS,
/// Telestream Inspector, and Bridge Technologies VB will report different
/// numbers for the same stream.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MdiStats {
    /// `MDI = NDF:MLR`, formatted for display (`"4.2:0"`).
    pub mdi: String,
    /// Algorithm discriminator. Currently always `"approx-iat-spread"` to
    /// signal that this is **not** the RFC 4445 VB-overflow model.
    /// Consumers comparing against another MDI probe should treat the
    /// numbers as approximate unless this field changes.
    pub model: &'static str,
    /// NDF in ms — `peak_iat − mean_iat` over the most recent window.
    pub delay_factor_ms: f32,
    /// MLR in events/s — TS continuity-counter discontinuities per second
    /// on the post-recovered stream. This is **not** packets-lost-per-second
    /// in the RFC 4445 sense (one CC gap can represent N packets).
    pub loss_rate_pps: f32,
    /// Number of windows where `delay_factor_ms` exceeded the configured
    /// alarm threshold (50 ms by default — the boundary between `OK` and
    /// `Warning` in most Bridge / Tek probes).
    pub windows_above_threshold: u64,
}

/// Per-elementary-stream counters collected on the PID bus. One entry per
/// `(input_id, source_pid)` channel the flow's `NodeEsBus` currently
/// tracks. The assembler annotates entries it is actively forwarding with
/// `out_pid`; unreferenced bus keys (e.g. PIDs present on an input but not
/// used by the current plan) show up with `out_pid = None` so operators
/// can still inspect them.
///
/// Counters are lifetime totals; bitrate is the rolling 1 Hz estimate
/// from the same `ThroughputEstimator` used for the flow-level counters.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PerEsStats {
    /// Flow-local input ID the ES is pulled from.
    pub input_id: String,
    /// Source-side PID on that input.
    pub source_pid: u16,
    /// Egress PID after the assembler's PID-remap, when an assembler is
    /// running and this bus key is referenced by the current plan.
    /// `None` when the flow is passthrough or the PID is observed but
    /// not routed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_pid: Option<u16>,
    /// PMT `stream_type` most recently seen for this PID. `0` when the
    /// bus has not yet observed a PMT mapping (packets published before
    /// the first PAT/PMT round-trip).
    pub stream_type: u8,
    /// High-level essence kind derived from `stream_type` when known
    /// (`"video"`, `"audio"`, `"subtitle"`, `"data"`). Empty string when
    /// the stream_type is not yet resolved.
    pub kind: String,
    /// Total TS packets observed on this PID.
    pub packets: u64,
    /// Total bytes observed on this PID (always 188 × packets — included
    /// for downstream ease).
    pub bytes: u64,
    /// Rolling 1 Hz bitrate estimate in bits-per-second.
    pub bitrate_bps: u64,
    /// Cumulative continuity-counter errors detected on this PID.
    pub cc_errors: u64,
    /// Cumulative PCR discontinuity events (PCR jumped backwards or
    /// advanced more than the configured max interval). Only populated
    /// for PIDs that carry PCR.
    pub pcr_discontinuity_errors: u64,
}

/// Liveness snapshot for a single input leg within a flow. Shipped inside
/// `FlowStats.inputs_live`; carries just enough state for the manager UI to
/// decide between NO SIGNAL, IDLE, and RECEIVING for every configured input.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PerInputLive {
    pub input_id: String,
    pub input_type: String,
    /// Same state vocabulary as [`InputStats::state`], including
    /// `"connect_failed"` for caller-mode inputs whose consecutive
    /// connect failures crossed the threshold.
    pub state: String,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub bitrate_bps: u64,
    /// SRT mode: `"caller"`, `"listener"`, `"rendezvous"`. Mirrors
    /// `InputStats.mode` for topology display of assembled flows where
    /// every input is concurrently active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtsp_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whep_url: Option<String>,
    /// Lightweight PAT/PMT catalogue observed on this input. `None` on
    /// non-TS inputs (RTMP / WebRTC / RTP-ES / ST 2110-30/-40) or when
    /// no PSI has arrived yet. Drives the manager UI's per-input "Programs
    /// & PIDs" panel and is the metadata source for the PID-bus assembler
    /// landing in Phase 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psi_catalog: Option<crate::engine::ts_psi_catalog::PsiCatalog>,
    /// Monotonic update counter from the per-input PSI catalogue store.
    /// Advances every time the observer accepts a fresh PAT or PMT;
    /// consumers (manager UI, WS clients) can diff this against the last
    /// value they saw to skip re-rendering unchanged `psi_catalog`
    /// payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psi_catalog_tick: Option<u64>,
    /// Active alarm on this input's *own* thumbnail accumulator: `"black"`,
    /// `"frozen"`, `"no_signal"`, or `None`. Each input gets a dedicated
    /// thumbnail generator subscribed to its pre-fixer broadcast channel,
    /// so the alarm reflects what *this* source is actually delivering —
    /// independent of which input is currently switched onto the flow's
    /// program bus. Lets the manager UI badge per-input cards correctly
    /// instead of painting the flow-level alarm onto every sibling card.
    /// Absent on flows that don't generate per-input thumbnails (e.g.
    /// thumbnail disabled) so older manager builds see an unchanged shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_alarm: Option<String>,
    /// Full per-input thumbnail counters (`total_captured`, `capture_errors`,
    /// `has_thumbnail`, `last_error`). Lets operators see *whether each
    /// per-input generator is actually capturing* — a passive leg that
    /// stops decoding produces a stale frame and a flat `total_captured`,
    /// which the alarm field alone cannot distinguish from a healthy
    /// silent-but-decoded source. Absent on flows that don't generate
    /// per-input thumbnails so older manager builds see an unchanged
    /// shape (the existing `thumbnail_alarm` field remains for backwards
    /// compatibility).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<ThumbnailStats>,
}

/// PTP state snapshot reported up to the manager.
///
/// Mirrors `engine::st2110::ptp::PtpState` but uses `Option` for fields that
/// are not yet known so the JSON shape stays stable when ptp4l is unreachable
/// (the common case at startup).
#[derive(Debug, Clone, Serialize, Default)]
pub struct PtpStateStats {
    /// `"locked"`, `"holdover"`, `"unlocked"`, or `"unavailable"`.
    pub lock_state: String,
    /// PTP domain (0..=127) the reporter is monitoring.
    pub domain: Option<u8>,
    /// Grandmaster identity as a hex string ("xx:xx:xx:xx:xx:xx:xx:xx").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grandmaster_id: Option<String>,
    /// Offset from master in nanoseconds (negative = local clock is behind).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_ns: Option<i64>,
    /// Mean path delay in nanoseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_path_delay_ns: Option<i64>,
    /// Steps removed from the grandmaster (0 if directly connected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps_removed: Option<u16>,
    /// Unix epoch milliseconds of the most recent successful update from
    /// `ptp4l`. `None` when no update has happened yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_update_ms: Option<u64>,
}

/// Per-leg counters for a SMPTE 2022-7 dual-network input.
///
/// Sourced from `engine::st2110::redblue::RedBlueStats::snapshot()`. Both
/// legs are exposed even when one is currently silent so operators can see
/// the imbalance.
#[derive(Debug, Clone, Serialize, Default)]
pub struct NetworkLegsStats {
    pub red: LegCounters,
    pub blue: LegCounters,
    /// Total times the merger flipped the active leg.
    pub leg_switches: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LegCounters {
    pub packets_received: u64,
    pub bytes_received: u64,
    pub packets_forwarded: u64,
    pub packets_duplicate: u64,
}

/// Per-essence stats for a single member of a flow group.
///
/// Used by the manager UI to render multi-essence ST 2110 flow groups.
/// Today only the flow_id and essence_type are surfaced — packet/byte counts
/// continue to live on the per-flow snapshot. The struct is reserved here so
/// that future additions can land without bumping the WS protocol version.
#[derive(Debug, Clone, Serialize, Default)]
pub struct EssenceFlowStats {
    pub flow_id: String,
    /// `"st2110_30"`, `"st2110_31"`, `"st2110_40"`, future `"st2110_22"`,
    /// `"st2110_20"`.
    pub essence_type: String,
    /// Optional human-readable label (channel order, ANC stream description).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Inter-arrival time statistics (microseconds).
#[derive(Debug, Clone, Serialize, Default)]
pub struct IatStats {
    /// Minimum IAT observed in the last reporting window.
    pub min_us: f64,
    /// Maximum IAT observed in the last reporting window.
    pub max_us: f64,
    /// Average IAT observed in the last reporting window.
    pub avg_us: f64,
}

/// Flow health/alarm state derived from monitoring metrics (RP 2129 M6).
///
/// Variant order is severity order — `derive_flow_health` relies on the
/// derived `Ord` (`Healthy < Warning < Error < Critical`) to roll the
/// per-reason severities up into the overall badge via `Iterator::max`.
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum FlowHealth {
    /// No errors, bitrate > 0, stream healthy.
    #[default]
    Healthy,
    /// Minor issues: PCR accuracy errors or low-level CC errors.
    Warning,
    /// Significant issues: sync loss, PAT/PMT timeout, or high packet loss.
    Error,
    /// Sustained failures: input disconnected or zero bitrate for extended period.
    Critical,
}

/// One structured explanation of *why* a flow is at its current
/// [`FlowHealth`]. Surfaced as `FlowStats.health_reasons` so the manager UI
/// can let an operator click the health badge and see the specific
/// condition(s) driving it instead of just the coarse colour. The overall
/// `FlowStats.health` equals the maximum `severity` across these reasons
/// (or `Healthy` when the list is empty), so the badge and its explanation
/// can never disagree.
#[derive(Debug, Clone, Serialize)]
pub struct HealthReason {
    /// Stable machine-readable identifier for the condition, suitable for
    /// keying UI tooltips and for correlating with the matching `error_code`
    /// on the event stream (e.g. `"bandwidth_exceeded"`,
    /// `"tr101290_p1"`, `"assembly_slot_stalled"`, `"packet_loss"`).
    pub code: String,
    /// Severity this single condition contributes to the overall badge —
    /// one of `Warning` / `Error` / `Critical` (never `Healthy`).
    pub severity: FlowHealth,
    /// Human-readable, operator-facing description, carrying live numbers
    /// where available (e.g. "Ingest 24.1 Mbps exceeds the 20 Mbps limit").
    pub detail: String,
}

/// Lifecycle state of a media flow.
#[derive(Debug, Clone, Serialize, Default)]
pub enum FlowState {
    /// Configured but not yet running (initial/default state).
    #[default]
    Idle,
    /// Actively receiving and forwarding media packets.
    Running,
}

/// Statistics for a flow's input leg.
#[derive(Debug, Clone, Serialize, Default)]
pub struct InputStats {
    /// Transport protocol type, e.g. `"srt"`, `"udp"`, `"rtmp"`.
    pub input_type: String,
    /// Human-readable connection state: `"receiving"`, `"idle"`,
    /// `"waiting"`, or `"connect_failed"` (caller-mode connection has
    /// failed `CONNECT_FAILED_THRESHOLD` consecutive connect attempts;
    /// resets on a successful connect).
    pub state: String,
    /// SRT mode: "caller", "listener", or "rendezvous" (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Local bind address from config (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
    /// Remote address from config — SRT caller/rendezvous destination (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    /// RTMP listen address (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_addr: Option<String>,
    /// RTP bind address (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    /// RTSP source URL (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtsp_url: Option<String>,
    /// WHEP source URL (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whep_url: Option<String>,
    /// Total RTP packets received on this input.
    pub packets_received: u64,
    /// Total bytes received (RTP payload + header).
    pub bytes_received: u64,
    /// Estimated receive bitrate in bits per second.
    pub bitrate_bps: u64,
    /// Number of RTP packets detected as lost (sequence gaps).
    pub packets_lost: u64,
    /// Packets dropped by ingress filters (source IP, payload type, rate limit).
    pub packets_filtered: u64,
    /// Number of lost packets successfully recovered via SMPTE 2022-1 FEC.
    pub packets_recovered_fec: u64,
    /// SRT-level statistics for the primary input leg (if SRT transport).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_stats: Option<SrtLegStats>,
    /// SRT-level statistics for the redundancy (second) input leg (if SMPTE 2022-7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_leg2_stats: Option<SrtLegStats>,
    /// RIST-level statistics for the primary input leg (if RIST transport).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rist_stats: Option<RistLegStats>,
    /// RIST-level statistics for the redundancy (second) input leg (if SMPTE 2022-7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rist_leg2_stats: Option<RistLegStats>,
    /// Bonding-level statistics for this input, including per-path detail.
    /// Populated only for bonded inputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bond_stats: Option<BondLegStats>,
    /// Native libsrt SRT bonding (socket-group) per-member stats.
    /// Populated only when the SRT input has a `bonding` block and the
    /// libsrt backend is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_bonding_stats: Option<SrtBondingStats>,
    /// Number of times the active input leg switched between leg 1 and leg 2.
    pub redundancy_switches: u64,
    /// Cumulative count of source-side timing-field discontinuities
    /// observed in the ingress stream (PCR / PTS / DTS jumps > 500 ms,
    /// rolled up). Drives the manager UI's "Source discontinuities"
    /// chip on the flow card so the operator sees a chronic upstream
    /// `-stream_loop`-style sender at a glance, before the events
    /// page surfaces the underlying `source_pcr_discontinuity` /
    /// `source_pts_discontinuity` / `source_dts_discontinuity` event
    /// trail. Additive — older managers ignore it.
    #[serde(default)]
    pub source_discontinuities: u64,
    /// True SMPTE 2022-7 buffered-merger stats. Present only when the
    /// input was started with `path_differential_ms` set on the
    /// redundancy config (industry-standard buffered mode). Absent on
    /// the legacy stateless dedup path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffered_hitless: Option<BufferedHitlessSnapshot>,
    /// Ingress de-jitter: cumulative packets shed by this input's
    /// release-rate servo residence cap (raw UDP / RTP only). Non-zero
    /// means a burst / source-rate offset exceeded the ±5 % servo
    /// authority and the buffer was trimmed to bound input latency; the
    /// receiver re-clocks from PCR. Backward-compatible additive field.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub ingress_dejitter_shed: u64,
    /// Ingress de-jitter: current de-jitter buffer occupancy (packets).
    /// The servo holds this near the configured `ingress_dejitter_ms` of
    /// content. Absent on inputs without a de-jitter buffer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_buffer_depth: Option<u64>,
    /// Per-input PCM transcode stage statistics. Present only when the input
    /// runs an `engine::audio_transcode::TranscodeStage` (i.e. the input config
    /// has a `transcode` block). Absent on passthrough inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode_stats: Option<TranscodeStatsSnapshot>,
    /// Per-input audio decode stage statistics. Present when the input runs
    /// an AAC decoder as part of its ingress transcode pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_decode_stats: Option<DecodeStatsSnapshot>,
    /// Per-input audio encode stage statistics. Present when the input runs
    /// `engine::audio_encode::AudioEncoder` as part of ingress normalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode_stats: Option<EncodeStatsSnapshot>,
    /// Per-input video encode stage statistics. Present when the input runs
    /// `video-engine::VideoEncoder` (ST 2110-20/-23 RFC 4175 ingress, or an
    /// `engine::ts_video_replace::TsVideoReplacer` fed by the InputTranscoder
    /// composer on Group A inputs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode_stats: Option<VideoEncodeStatsSnapshot>,
    /// Per-input video decode stage statistics. Present when an input runs a
    /// `video_engine::VideoDecoder` (Group A composer paths that decode
    /// before re-encoding). Absent on raw-uncompressed inputs (ST 2110-20/-23)
    /// and on passthrough ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_decode_stats: Option<VideoDecodeStatsSnapshot>,
    /// Compact, snapshot-time description of the media arriving on this input:
    /// pipeline stages traversed, resolved codecs, and high-level format.
    /// Mirrors the per-output [`EgressMediaSummary`] so the manager UI can
    /// render the ingress pipeline with the same renderer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_summary: Option<EgressMediaSummary>,
}

/// Statistics for a single output leg of a flow.
#[derive(Debug, Clone, Serialize, Default)]
pub struct OutputStats {
    /// Unique identifier for this output (matches the config key).
    pub output_id: String,
    /// Human-readable display name for the output.
    pub output_name: String,
    /// Transport protocol type, e.g. `"srt"`, `"udp"`, `"rtmp"`.
    pub output_type: String,
    /// Human-readable connection state, e.g. `"active"`, `"connecting"`.
    pub state: String,
    /// SRT mode (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Remote address — SRT caller destination or RTP dest_addr (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    /// Destination address — RTP dest_addr (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest_addr: Option<String>,
    /// RTMP destination URL (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest_url: Option<String>,
    /// HLS ingest URL (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingest_url: Option<String>,
    /// WHIP URL for WebRTC (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whip_url: Option<String>,
    /// Local address — SRT listener bind address (for topology display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
    /// Configured MPTS program filter for this output (mirrors `program_number`
    /// in the output config). `null` means passthrough for TS-native outputs
    /// (UDP/RTP/SRT/HLS) or "auto, lowest program_number in PAT" for re-muxing
    /// outputs (RTMP/WebRTC). Surfaced so the manager status view can show at
    /// a glance which MPTS program each output is locked to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Total RTP packets successfully sent on this output.
    pub packets_sent: u64,
    /// Total bytes sent (RTP payload + header).
    pub bytes_sent: u64,
    /// Estimated send bitrate in bits per second.
    pub bitrate_bps: u64,
    /// Number of packets dropped because the send channel was full.
    pub packets_dropped: u64,
    /// Number of SMPTE 2022-1 FEC packets generated and sent for this output.
    pub fec_packets_sent: u64,
    /// SRT-level statistics for the primary output leg (if SRT transport).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_stats: Option<SrtLegStats>,
    /// SRT-level statistics for the redundancy (second) output leg (if SMPTE 2022-7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_leg2_stats: Option<SrtLegStats>,
    /// RIST-level statistics for the primary output leg (if RIST transport).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rist_stats: Option<RistLegStats>,
    /// RIST-level statistics for the redundancy (second) output leg (if SMPTE 2022-7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rist_leg2_stats: Option<RistLegStats>,
    /// Bonding-level statistics for this output, including per-path
    /// detail. Populated only for bonded outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bond_stats: Option<BondLegStats>,
    /// Native libsrt SRT bonding (socket-group) per-member stats.
    /// Populated only when the SRT output has a `bonding` block and the
    /// libsrt backend is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_bonding_stats: Option<SrtBondingStats>,
    /// Per-output PCM transcode stage statistics. Present only when the
    /// output runs an `engine::audio_transcode::TranscodeStage` (i.e., the
    /// output config has a `transcode` block AND the upstream input is an
    /// audio essence). Absent otherwise (passthrough outputs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcode_stats: Option<TranscodeStatsSnapshot>,
    /// Per-output AAC decode stage statistics. Present only when the output
    /// runs an `engine::audio_decode::AacDecoder` to turn compressed audio
    /// (AAC-LC in MPEG-TS) into PCM for downstream PCM-only outputs or for a
    /// PCM→encoder chain. Absent on pass-through outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_decode_stats: Option<DecodeStatsSnapshot>,
    /// Per-output audio encode stage statistics. Present only when the
    /// output runs an `engine::audio_encode::AudioEncoder` (ffmpeg sidecar)
    /// to produce a compressed codec (AAC / HE-AAC / Opus / MP2 / AC-3) from
    /// upstream PCM. Absent on pass-through outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_encode_stats: Option<EncodeStatsSnapshot>,
    /// Per-output video encode stage statistics. Present only when the output
    /// runs an `engine::ts_video_replace::TsVideoReplacer` (i.e. the output
    /// config has a `video_encode` block). Absent on pass-through video outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode_stats: Option<VideoEncodeStatsSnapshot>,
    /// Per-output video decode stage statistics. Present when the output runs
    /// a `video_engine::VideoDecoder` — either inside
    /// `engine::ts_video_replace::TsVideoReplacer` (true transcode, paired
    /// with `video_encode_stats`) or as the terminal decoder feeding
    /// `engine::output_display` (decode-only, no encode counterpart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_decode_stats: Option<VideoDecodeStatsSnapshot>,
    /// End-to-end latency from input receive to output send.
    /// Present only when the output has actively sent packets in the last
    /// reporting window (1 second). Backward-compatible addition — old manager
    /// builds ignore unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<OutputLatencyStats>,
    /// Compact, snapshot-time description of the egress media for this output:
    /// pipeline stages traversed, output codecs, and high-level format. Built
    /// by the per-flow stats path from the output's static config + any
    /// active encode/decode/transcode stages + the cached input
    /// `MediaAnalysis`. Backward-compatible addition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_summary: Option<EgressMediaSummary>,
    /// Per-output PCR accuracy trust metric. Present only when the output
    /// has forwarded ≥ 2 PCR-bearing TS packets in the flow's lifetime —
    /// audio-only, non-TS, and freshly-started outputs omit this field.
    /// PID-bus Phase 8 addition; old managers ignore unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_trust: Option<PcrTrustStats>,
    /// Per-output A/V mux-interleave metric. Present only when the output
    /// has forwarded both video and audio PES PTS values. Absent on
    /// audio-only, video-only, non-TS, and freshly-started outputs.
    /// Hard-renamed from `av_sync` 2026-06-06.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av_interleave: Option<AvInterleaveStats>,
    /// Output-local edge-added A/V skew from output-level transcode
    /// stages (`audio_encode` / `video_encode` on this output). Absent
    /// when the output transcodes nothing — the flow-level
    /// `FlowStats.av_skew` then covers the whole path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av_skew: Option<AvSkewStats>,
    /// Per-output local-display stats. Populated only when the output
    /// type is `display`; absent on every network-egress output.
    /// Backward-compatible additive field — old managers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_stats: Option<DisplayStats>,
    /// The active wire-pacing release tier for this output, set once at
    /// output startup. One of `so_txtime`, `clock_nanosleep_fifo`,
    /// `clock_nanosleep`, `unpaced`. Absent on outputs that don't own
    /// a UDP socket directly (SRT, RIST, RTMP, HLS, CMAF, WebRTC).
    /// Backward-compatible additive field — old managers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_pacing_tier: Option<String>,
    /// The egress pacing mode this output actually runs, resolved at
    /// spawn. Bare mode ("forward" / "pcr" / "servo") for an explicit
    /// operator value; "auto (pcr)" / "auto (forward)" when the config
    /// field was unset and the engine resolved it (pcr iff the flow had
    /// a bonded input at spawn). Lets an operator see which mode an
    /// auto output landed on after an upgrade or an input-set change.
    /// Only present on UDP/RTP-family outputs. Backward-compatible
    /// additive field — old managers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_pacing_effective: Option<String>,
    /// Number of datagrams the kernel rejected as late (target tx time
    /// landed in the past) on the SO_TXTIME release path. Always 0 on
    /// the userspace-sleep release paths. Backward-compatible additive
    /// field — old managers ignore it.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub wire_pacing_late: u64,
    /// CPU index the wire-emit thread was pinned to at spawn (via
    /// `pthread_setaffinity_np`, configured per-process by
    /// `BILBYCAST_WIRE_EMIT_CPUS`). `None` means not pinned — the
    /// kernel scheduler floats the thread, which can cause contention
    /// with Tokio workers under load. Backward-compatible additive
    /// field — old managers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_pacing_pinned_cpu: Option<u32>,
    /// Egress de-jitter: datagrams shed by the residence cap (compressed /
    /// `Lossless` outputs). The bounded-latency safety net — non-zero means
    /// the release-rate servo hit its ±authority ceiling (a burst or rate
    /// offset bigger than the servo can absorb) and the buffer was trimmed to
    /// stay inside the receiver T-STD; the receiver re-clocked from PCR. The
    /// companion residence is on `latency.{avg,max}_us`. Always 0 on ST 2110
    /// and protocol-paced outputs (SRT/RIST/RTMP/HLS/CMAF/WebRTC).
    /// Backward-compatible additive field — old managers ignore it.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub egress_shed: u64,
    /// Egress de-jitter: current wire-emit queue depth (datagrams in flight
    /// between the broadcast subscriber and the wire). The servo holds this
    /// near its setpoint (~the configured `egress_buffer_ms` of content); a
    /// sustained climb is the early signature of the latency runaway the
    /// servo + shed exist to prevent. Absent on outputs without a dedicated
    /// wire emitter. Backward-compatible additive field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_emit_depth: Option<u64>,
}

#[inline]
fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

/// Per-output statistics for the local-display (`display`) output type.
/// Populated by `engine::output_display` and read by the manager UI's
/// flow-card "Display" sub-panel. All fields are best-effort snapshots —
/// the lock-free atomic counters under the hood are sampled per stats
/// cycle.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DisplayStats {
    /// Frames page-flipped to the connector since the output started.
    pub frames_displayed: u64,
    /// Frames the display task dropped because they arrived more than
    /// one frame-period behind the audio clock.
    pub frames_dropped_late: u64,
    /// Frames the display task held for an extra vsync because the
    /// next decoded frame's PTS was too far ahead of the audio clock.
    pub frames_repeated: u64,
    /// ALSA xrun (`EPIPE`) count over the output's lifetime.
    pub audio_underruns: u64,
    /// True when the audio decode stage delivered blocks at some point
    /// and the counter has since frozen for ≥ 5 s while the output runs —
    /// audio-only death (dropped audio PID, silent demux). Distinct from
    /// `audio_underruns`, which only counts inside an ALSA write attempt
    /// and stays 0 when no audio reaches the writer at all. Always false
    /// for sources that never carried audio.
    #[serde(default)]
    pub audio_stalled: bool,
    /// EMA of the signed video-vs-audio offset (positive = video late
    /// relative to the audio master clock), in milliseconds.
    pub av_sync_offset_ms: i32,
    /// Negotiated KMS mode resolution, e.g. `"1920x1080"`.
    pub current_resolution: String,
    /// Negotiated KMS refresh rate in Hz.
    pub current_refresh_hz: u32,
    /// Current pixel format on the wire to the connector
    /// (v1 always `"XRGB8888"`).
    pub pixel_format: String,
    /// Decoder backend, `"sw"` in v1; `"vaapi"` / `"nvdec"` in v2.
    pub decoder_kind: String,
    /// Source video codec (`"h264"` / `"hevc"`).
    pub video_codec: String,
    /// Source audio codec (`"aac"` / `"mp2"` / `"ac3"` / `"eac3"` /
    /// `"opus"` / `"none"` when audio is muted).
    pub audio_codec: String,

    // ── HW-decode lifecycle (Section 1) ──
    /// Cumulative `send_packet` failures into the active video decoder.
    /// Sustained growth on a HW backend triggers the
    /// `display_hw_decode_runtime_failed` Warning + a HW→CPU demotion.
    /// Additive field — older managers ignore it via `serde(default)`.
    #[serde(default)]
    pub send_packet_errors: u64,
    /// Number of HW→CPU demotions on this output. `0` is the
    /// steady-state happy path; `1+` means the operator's chosen HW
    /// backend stopped working and we silently fell back. Manager UI
    /// renders e.g. "qsv (1 demotion)" so the operator knows to switch
    /// the dropdown to `cpu` permanently or wait for a driver fix.
    #[serde(default)]
    pub decoder_demotions: u64,
    /// Decoded frames produced since the active decoder was last
    /// opened. Resets on every HW→CPU demotion and on every codec
    /// switch. Used by the manager UI to badge "warming up" until the
    /// first frame lands.
    #[serde(default)]
    pub frames_received_since_open: u64,

    // ── Reolink + S4 4K diagnostics (Section 5) ──
    /// PTS jumps detected by the demux loop's `pts_jump` heuristic.
    /// Spikes on Reolink point at SRT-FEC repair producing out-of-order
    /// PTS that trip the 1 s flush threshold.
    #[serde(default)]
    pub pts_jumps_observed: u64,
    /// Video access units skipped while waiting for the first keyframe
    /// after a decoder flush (startup, operator switch, broadcast
    /// Lagged, pts_jump). Pre-IDR slices are guaranteed `send_packet`
    /// rejections on a flushed decoder, so the demux loop sheds them
    /// instead of feeding them. Bounded by one GOP per switch in the
    /// healthy case; sustained steady-state growth means the flush
    /// triggers are firing spuriously. Additive field — older managers
    /// ignore it.
    #[serde(default)]
    pub aus_skipped_awaiting_keyframe: u64,
    /// Decoded frames whose `frame.pts()` was `None`, forcing the
    /// display loop to fall back to the most recent input PTS. Growth
    /// implicates the B-frame display-PTS plumbing.
    #[serde(default)]
    pub frame_pts_fallbacks: u64,
    /// Frames the display path dropped because the decoder produced a
    /// pixel format we have no `*_planes` accessor for.
    #[serde(default)]
    pub frames_dropped_unsupported_pixfmt: u64,
    /// Times the broadcast subscriber returned `Lagged(n)`. One per
    /// Lagged event, regardless of `n`. Sustained growth means the
    /// demux + decode + display pipeline can't keep up with the input
    /// rate.
    #[serde(default)]
    pub subscriber_lag_events: u64,
    /// Frames the demux+scale child dropped because the bounded mpsc
    /// to the display task was full. Distinguishes "blit/present is
    /// the bottleneck" from `frames_dropped_late` (frame arrived too
    /// late to show) and `subscriber_lag_events` (decode is slow).
    #[serde(default)]
    pub frames_dropped_mpsc_full: u64,
    /// Decoded frames dropped on display arrival because a switch /
    /// Lagged / pts_jump bumped the shared `frame_gen` after they were
    /// produced. Counts the queued pre-switch frames the display loop
    /// shed in microseconds (vs. paying full blit + vsync time) so the
    /// new stream's first frame renders within one frame period of the
    /// switch. Sustained growth in steady state would indicate
    /// spurious flush triggers and is worth investigating.
    #[serde(default)]
    pub frames_dropped_stale_gen: u64,
    /// Audio blocks the demux+decode child dropped because the bounded
    /// `atx` mpsc was full when `try_send` ran. Audio counterpart to
    /// `frames_dropped_mpsc_full`. Steady growth points at the
    /// "audio mpsc full → silent drop → audio_pts lags real time →
    /// av_sync_offset_ms drifts positive" failure mode that's invisible
    /// from `audio_underruns` alone. See `audio_dropped_mpsc_full` on
    /// `DisplayStatsCounters` for the exact mechanism.
    #[serde(default)]
    pub audio_dropped_mpsc_full: u64,
    /// `true` when the audio-bars + stream-info-header KMS overlay
    /// plane was successfully enabled at display-task startup. False
    /// means the per-frame rasterise can't reach the panel — the
    /// composition layer never gets a plane to draw onto.
    #[serde(default)]
    pub bars_overlay_enabled: bool,
    /// Number of `MeterPublisher::publish` calls — each one hands a
    /// fresh per-PID levels snapshot to the display loop. Stays at 0
    /// when the audio-meter task is alive but never decodes audio
    /// (broadcast lagged, decoder open failed, source stream type
    /// misclassified). Read against `bars_rasterise_skipped_empty`.
    #[serde(default)]
    pub meter_publishes: u64,
    /// Largest single `blit_and_present` duration since startup, in µs.
    /// `kms.present()` blocks one vblank (~16 700 µs at 60 Hz) so this
    /// is one-vblank-floored; values past ~33 000 µs mean the per-frame
    /// work is missing vblank slots.
    #[serde(default)]
    pub blit_us_max: u64,
    /// Average `blit_and_present` duration since startup, in µs.
    /// Computed on the fly by the snapshot path from the running sum
    /// + count atomics on the counter struct.
    #[serde(default)]
    pub blit_us_avg: u64,
    /// Largest single decode-AU duration since startup, in µs. Wraps
    /// the synchronous libavcodec `send_packet` + reorder-buffer
    /// drain + plane-copy out of the decoder's lifetime. Values past
    /// the source frame period (40 ms at 25 fps) on motion-heavy
    /// segments explain why audio momentarily outruns video.
    #[serde(default)]
    pub decode_us_max: u64,
    /// Average decode-AU duration since startup, in µs.
    #[serde(default)]
    pub decode_us_avg: u64,
}

/// PCR accuracy trust metric — percentiles of `|observed_Δ − expected_Δ|`
/// for consecutive PCR-bearing TS packets. Measured at egress so it
/// captures scheduling jitter introduced by the assembler + the output
/// transport (SRT backpressure, UDP socket scheduling, hitless dedup
/// stalls). Units are microseconds.
///
/// A healthy assembler doing byte-for-byte PCR forwarding should see
/// p95 well under 1 ms and p99 under a few ms on a CPU-idle host. The
/// testbed probe asserts p95 < 5 ms and p99 < 20 ms — generous bounds
/// that still catch real scheduling pathologies.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PcrTrustStats {
    /// Number of drift samples currently held in the rotating reservoir.
    /// Caps at the reservoir size (currently 4096). Use this + the sample
    /// rate to estimate how far back the reported percentiles look.
    pub samples: u64,
    /// Cumulative PCR pairs observed over the flow's lifetime (can exceed
    /// `samples` once the reservoir has rolled over).
    pub cumulative_samples: u64,
    /// Average absolute drift over the cumulative lifetime, in µs.
    pub avg_us: u64,
    /// 50th-percentile absolute drift across the reservoir, in µs.
    pub p50_us: u64,
    /// 95th-percentile absolute drift across the reservoir, in µs.
    pub p95_us: u64,
    /// 99th-percentile absolute drift across the reservoir, in µs.
    pub p99_us: u64,
    /// Largest absolute drift observed in the reservoir, in µs.
    pub max_us: u64,
    /// Number of samples in the short "recent" window (≤ 256). Gives the
    /// UI a signal for "is this spike current or baseline".
    pub window_samples: u64,
    /// 95th-percentile drift across the recent window, in µs. Compare
    /// against `p95_us` to see whether recent behaviour diverges from
    /// the longer-window baseline.
    pub window_p95_us: u64,
}

/// Egress A/V **mux-interleave** metric — signed offset between the most
/// recent video PES PTS and audio PES PTS observed in the byte stream at
/// egress. Positive = video muxed ahead of audio (the normal broadcast
/// T-STD geometry). Units are milliseconds.
///
/// **This is NOT lip-sync** — receivers pair A/V by PTS regardless of
/// interleave. It bounds the RECEIVER BUFFERING REQUIREMENT: a player
/// must buffer at least this much to pair late-muxed audio with video.
/// Sustained values beyond a consumer player's caching (~1 s for VLC)
/// starve its audio queue. Edge-added lip-sync error is the separate
/// [`AvSkewStats`]. Percentiles are over the ~4096-sample rolling
/// reservoir (≈1 min); `ewma_ms` replaces the old lifetime average whose
/// post-switch convergence read as "drift". Hard-renamed from
/// `AvSyncStats` / wire key `av_sync` 2026-06-06.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AvInterleaveStats {
    pub samples: u64,
    pub cumulative_samples: u64,
    /// Signed EWMA (~4-6 s time constant) of video−audio PTS at egress.
    pub ewma_ms: i64,
    pub p50_abs_ms: i64,
    pub p95_abs_ms: i64,
    pub p99_abs_ms: i64,
    pub max_abs_ms: i64,
    pub window_samples: u64,
    pub window_p95_abs_ms: i64,
    pub video_pid: u16,
    pub audio_pid: u16,
}

/// Edge-added A/V skew — the lip-sync error THIS EDGE introduces, exact,
/// derived from the PTS-rewrite stages' own (output − source) PTS deltas
/// rather than estimated from the mux. `skew_ms > 0` ⇒ audio is presented
/// LATER than video relative to the source's own A/V alignment (audio
/// lags). Includes the operator's deliberate lipsync trim, reported
/// separately in `lipsync_trim_ms` so dashboards can distinguish
/// configured offset from defect.
///
/// `mode == "passthrough"`: no PTS-modifying stage is active on the
/// path — the source's A/V relationship is preserved bit-exactly and the
/// skew is 0 by construction. `mode == "measured"`: at least one stage
/// (PTS rewriter trim, audio/video transcode) is active and the value is
/// computed from stage anchor state. EBU R37 thresholds apply to THIS
/// metric: |skew| > 20 ms warning, > 40 ms error.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AvSkewStats {
    pub skew_ms: i64,
    /// Worst |skew_ms| observed since flow start / input switch.
    pub worst_abs_ms: i64,
    /// The operator-configured lipsync trim portion included in `skew_ms`.
    pub lipsync_trim_ms: i64,
    /// "passthrough" | "measured"
    pub mode: String,
}

/// Per-output end-to-end latency statistics for the last reporting window.
///
/// Measured from the input packet's `recv_time_us` (monotonic receive time)
/// to the moment the output successfully sends the packet. The window resets
/// on each 1-second stats snapshot.
#[derive(Debug, Clone, Serialize, Default)]
pub struct OutputLatencyStats {
    /// Minimum end-to-end latency in microseconds during the window.
    pub min_us: u64,
    /// Average end-to-end latency in microseconds during the window.
    pub avg_us: u64,
    /// Maximum end-to-end latency in microseconds during the window.
    pub max_us: u64,
    /// Average latency expressed in video frames. Present only when the
    /// flow's media analysis has detected a video frame rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_frames: Option<f64>,
}

/// Per-output transcoder snapshot. Mirrors `engine::audio_transcode::TranscodeStats`
/// at point-in-time, suitable for the manager UI and Prometheus surfacing.
#[derive(Debug, Clone, Serialize, Default)]
pub struct TranscodeStatsSnapshot {
    /// Input RTP packets seen by the transcoder.
    pub input_packets: u64,
    /// Output RTP packets emitted (may differ from input due to packet-time
    /// or sample-rate differences between input and output).
    pub output_packets: u64,
    /// Packets dropped inside the transcoder (decode error, resampler
    /// failure, malformed RTP). Distinct from the broadcast-channel lag
    /// drops counted in `packets_dropped`.
    pub dropped: u64,
    /// Times the transcoder reset its internal state due to an upstream
    /// format change.
    pub format_resets: u64,
    /// Most recent end-to-end transcode latency, in microseconds. Measured
    /// from the input packet's `recv_time_us` to emission time.
    pub last_latency_us: u64,
}

/// Per-output audio decode snapshot. Mirrors
/// `engine::audio_decode::DecodeStats` at point-in-time plus a small set of
/// steady-state descriptors so the manager UI can label the stage without
/// having to cross-reference the output config.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DecodeStatsSnapshot {
    /// Compressed audio frames fed into the decoder.
    pub input_frames: u64,
    /// PCM frame blocks emitted (one per successfully decoded input frame).
    pub output_blocks: u64,
    /// Frames that failed to decode (corrupt input, symphonia error).
    pub decode_errors: u64,
    /// Frames dropped because the decoder had not yet seen its init config.
    pub dropped_uninit: u64,
    /// Wire identifier of the input codec the decoder is handling. Always
    /// `"AAC-LC"` in Phase A.
    pub input_codec: String,
    /// Output PCM sample rate in Hz.
    pub output_sample_rate_hz: u32,
    /// Output PCM channel count (1 or 2).
    pub output_channels: u8,
}

/// Per-output audio encode snapshot. Mirrors
/// `engine::audio_encode::EncodeStats` at point-in-time plus the resolved
/// target codec / sample rate / channel count / bitrate so the manager UI
/// can display what the encoder is actually producing.
#[derive(Debug, Clone, Serialize, Default)]
pub struct EncodeStatsSnapshot {
    /// PCM frames that were accepted into the bounded input channel feeding
    /// ffmpeg stdin.
    pub pcm_frames_submitted: u64,
    /// PCM frames dropped because the bounded input channel was full (slow
    /// or restarting ffmpeg). These are distinct from the generic output
    /// `packets_dropped` counter.
    pub pcm_frames_dropped: u64,
    /// Encoded codec frames successfully framed out of ffmpeg stdout.
    pub encoded_frames_out: u64,
    /// Number of times the ffmpeg subprocess supervisor restarted the
    /// encoder (e.g. after a non-zero exit or spawn failure).
    pub supervisor_restarts: u64,
    /// Wire identifier of the target codec, e.g. `"aac_lc"`, `"opus"`.
    pub output_codec: String,
    /// Resolved target sample rate in Hz.
    pub target_sample_rate_hz: u32,
    /// Resolved target channel count.
    pub target_channels: u8,
    /// Resolved target bitrate in kbps.
    pub target_bitrate_kbps: u32,
    /// Source audio PID the `TsAudioReplacer` is currently locked onto
    /// (PMT-discovered or operator-pinned via
    /// `audio_encode.source_audio_pid`). `0` means the PMT hasn't been
    /// seen yet OR the output uses the subprocess `AudioEncoder` (no
    /// `TsAudioReplacer` stats handle). Surfaces on the UI as
    /// `(from PID 0x0101)` on the audio-transcode badge.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub source_pid: u16,
    /// Source audio stream_type byte (`0x0F` AAC, `0x03/0x04` MPEG-1/2,
    /// `0x81` AC-3, `0x06` private). `0` = unknown.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub source_stream_type: u8,
}

/// Per-output video encode snapshot. Mirrors `engine::ts_video_replace::VideoEncodeStats`
/// at point-in-time plus the resolved input/output codec and target frame
/// geometry so the manager UI can label the stage without cross-referencing
/// the output config.
#[derive(Debug, Clone, Serialize, Default)]
pub struct VideoEncodeStatsSnapshot {
    /// Compressed video frames fed into the decoder.
    pub input_frames: u64,
    /// Encoded video frames emitted by the encoder.
    pub output_frames: u64,
    /// Frames dropped inside the replacer (decode error, encoder backpressure,
    /// supervisor restart). Distinct from the broadcast-channel `packets_dropped`.
    pub dropped_frames: u64,
    /// Wire identifier of the input codec (e.g. `"h264"`, `"hevc"`).
    pub input_codec: String,
    /// Wire identifier of the target codec.
    pub output_codec: String,
    /// Target frame width in pixels (0 if not yet known).
    pub output_width: u32,
    /// Target frame height in pixels.
    pub output_height: u32,
    /// Target frame rate (0.0 if not yet known).
    pub output_fps: f32,
    /// Target bitrate in kbps.
    pub output_bitrate_kbps: u32,
    /// Encoder backend label (`"x264"`, `"x265"`, `"nvenc"`).
    pub encoder_backend: String,
    /// Most recent end-to-end frame latency through the replacer, in microseconds.
    pub last_latency_us: u64,
    /// Number of times the encoder supervisor restarted the backend.
    pub supervisor_restarts: u64,
    /// Source video PID the replacer is currently transcoding (PMT-discovered
    /// or operator-pinned via `video_encode.source_video_pid`). `0` means
    /// the PMT hasn't been seen yet on this run, so no PID lock yet.
    /// Surfaces on the manager UI's transcode badge so operators can
    /// verify which video stream the transcoder picked at a glance.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub source_pid: u16,
    /// Source video stream_type byte (e.g. `0x1B` H.264, `0x24` H.265).
    /// `0` means unknown.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub source_stream_type: u8,
}

/// Per-input or per-output video decode snapshot. Mirrors
/// `engine::ts_video_replace::VideoDecodeStats` (and the analogous counter
/// inside `engine::output_display`) so the manager UI can tell true
/// transcoding (`video_decode` + `video_encode` on the same leg) apart
/// from encode-only (ST 2110-20 ingress) and decode-only (Display output).
#[derive(Debug, Clone, Serialize, Default)]
pub struct VideoDecodeStatsSnapshot {
    /// Compressed video frames fed into the decoder.
    pub input_frames: u64,
    /// Raw video frames successfully emitted.
    pub output_frames: u64,
    /// Frames that failed to decode.
    pub decode_errors: u64,
    /// Wire identifier of the source codec (e.g. `"h264"`, `"hevc"`).
    pub input_codec: String,
    /// Decoded frame width in pixels (0 if not yet known).
    pub output_width: u32,
    /// Decoded frame height in pixels.
    pub output_height: u32,
    /// Decoded frame rate (0.0 if not yet known).
    pub output_fps: f32,
}

#[allow(dead_code)]
fn is_zero_u16(v: &u16) -> bool { *v == 0 }
#[allow(dead_code)]
fn is_zero_u8(v: &u8) -> bool { *v == 0 }

/// Snapshot-time description of the egress (or ingress) media for a single
/// output or input. Reused for both legs of a flow — when populated as
/// [`InputStats::ingress_summary`] it describes the transcoded media entering
/// the flow's broadcast channel.
///
/// Composed from three sources:
/// - The output's static config (program filter, transport mode, audio/video
///   encode descriptors).
/// - The flow's cached input [`MediaAnalysis`] (used for whatever passes through
///   unchanged — codec, resolution, frame rate, audio sample rate / channels).
/// - Live encode / decode / transcode stats handles when active.
///
/// `pipeline` is an ordered list of tags identifying the stages a packet
/// traverses on its way out: `"passthrough"`, `"program_filter"`,
/// `"audio_decode"`, `"audio_encode"`, `"audio_transcode_pcm"`, `"audio_302m"`,
/// `"video_encode"`, `"ts_audio_replace"`, `"ts_video_replace"`. Receivers should
/// treat unknown tags as opaque and render them in display order.
///
/// All fields are optional and backward-compatible additions — older manager
/// builds simply ignore the block.
#[derive(Debug, Clone, Serialize, Default)]
pub struct EgressMediaSummary {
    /// Codec of the video essence on this output (e.g. `"h264"`, `"hevc"`,
    /// `"jpeg_xs"`). `None` for audio-only outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    /// Encoded video resolution as `"WIDTHxHEIGHT"` (e.g. `"1920x1080"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_resolution: Option<String>,
    /// Encoded video frame rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_fps: Option<f32>,
    /// Video bitrate in kbps when the output is actively re-encoding video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_bitrate_kbps: Option<u32>,
    /// Codec of the audio essence on this output (e.g. `"aac_lc"`, `"opus"`,
    /// `"mp2"`, `"ac3"`, `"s302m"`, `"l24"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    /// Audio sample rate in Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_sample_rate_hz: Option<u32>,
    /// Audio channel count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_channels: Option<u8>,
    /// Audio bitrate in kbps when the output is actively re-encoding audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_bitrate_kbps: Option<u32>,
    /// MPTS program filter applied by this output (`None` = passthrough or auto).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// High-level transport descriptor (`"ts"`, `"rtp"`, `"audio_302m"`,
    /// `"st2110-30"`, `"st2110-31"`, `"st2110-40"`, `"flv"`, `"hls"`,
    /// `"webrtc"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_mode: Option<String>,
    /// Ordered pipeline stage tags. Empty when the output is pure passthrough.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pipeline: Vec<String>,
}

/// TR-101290 transport stream analysis statistics for a single flow.
///
/// Contains error counts for Priority 1 (critical) and Priority 2 (important)
/// checks as defined by ETSI TR 101 290. Summary flags `priority1_ok` and
/// `priority2_ok` indicate whether any errors have been detected since the
/// flow started.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Tr101290Stats {
    /// Total MPEG-TS packets inspected by the analyzer.
    pub ts_packets_analyzed: u64,
    /// Number of PAT sections received.
    pub pat_count: u64,
    /// Number of PMT sections received.
    pub pmt_count: u64,

    // ── Priority 1 (critical) ──

    /// Number of times sync was lost (≥5 consecutive missing 0x47 sync bytes).
    pub sync_loss_count: u64,
    /// Individual TS packets where the sync byte was not 0x47.
    pub sync_byte_errors: u64,
    /// Continuity counter discontinuities (CC not incrementing by 1 mod 16).
    pub cc_errors: u64,
    /// PAT (PID 0x0000) not received within the required 500 ms interval.
    pub pat_errors: u64,
    /// A PMT referenced by the PAT was not received within 500 ms.
    pub pmt_errors: u64,
    /// Elementary stream PIDs referenced in PMT but not seen within 500 ms.
    pub pid_errors: u64,

    // ── Priority 2 (important) ──

    /// TS packets with the Transport Error Indicator bit set.
    pub tei_errors: u64,
    /// CRC-32 verification failures on PAT/PMT sections.
    pub crc_errors: u64,
    /// PCR jumps exceeding 100 ms or going backwards.
    pub pcr_discontinuity_errors: u64,
    /// PCR jitter relative to wall clock exceeding 500 ns.
    pub pcr_accuracy_errors: u64,

    // ── Windowed counters (reset each snapshot, "errors since last report") ──

    /// CC errors in the last reporting window.
    pub window_cc_errors: u64,
    /// PAT timeout errors in the last reporting window.
    pub window_pat_errors: u64,
    /// PMT timeout errors in the last reporting window.
    pub window_pmt_errors: u64,
    /// PID errors in the last reporting window.
    pub window_pid_errors: u64,
    /// TEI errors in the last reporting window.
    pub window_tei_errors: u64,
    /// CRC errors in the last reporting window.
    pub window_crc_errors: u64,
    /// PCR discontinuity errors in the last reporting window.
    pub window_pcr_discontinuity_errors: u64,
    /// PCR accuracy errors in the last reporting window.
    pub window_pcr_accuracy_errors: u64,

    // ── Summary ──

    /// `true` when all Priority 1 error counters are zero AND no
    /// PMT-referenced continuous-media PID is currently missing
    /// (`missing_continuous_pids == 0` — P1 PID_error is a state, so the
    /// flag holds false for the duration of the outage, not just the
    /// snapshot the latch fired in).
    pub priority1_ok: bool,
    /// GAUGE: PMT-referenced continuous-media PIDs (video/audio,
    /// descriptor-classified 0x06) currently absent from the stream.
    /// Sparse essences (teletext / subtitles / SCTE-35) never count.
    #[serde(default)]
    pub missing_continuous_pids: u64,
    /// `true` when all Priority 2 error counters are zero.
    pub priority2_ok: bool,
    /// `true` when all Priority 3 error counters (P2-extended + P3) are
    /// zero. `None` on edges that don't ship the `tr101290_full`
    /// capability so the manager UI can fall back to today's rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority3_ok: Option<bool>,

    // ── Priority 2 extended (PTS / CAT / PCR repetition split) ──
    /// PES PIDs that stopped emitting PTS within 700 ms (TR 101 290 §5.2.1).
    #[serde(default)]
    pub pts_errors: u64,
    /// CAT (PID 0x0001) referenced or observed, then absent for > 500 ms.
    #[serde(default)]
    pub cat_errors: u64,
    /// PCR repetition: a PCR-bearing PID stopped emitting PCR within
    /// 100 ms (split from the legacy `pcr_discontinuity_errors`, which
    /// now strictly counts unrecoverable PCR jumps).
    #[serde(default)]
    pub pcr_repetition_errors: u64,

    // ── Priority 3 (application-specific) ──
    #[serde(default)]
    pub nit_errors: u64,
    #[serde(default)]
    pub si_repetition_errors: u64,
    #[serde(default)]
    pub unreferenced_pid_errors: u64,
    #[serde(default)]
    pub sdt_errors: u64,
    #[serde(default)]
    pub eit_errors: u64,
    #[serde(default)]
    pub rst_errors: u64,
    #[serde(default)]
    pub tdt_errors: u64,

    // ── Windowed P2-extended / P3 ──
    #[serde(default)]
    pub window_pts_errors: u64,
    #[serde(default)]
    pub window_cat_errors: u64,
    #[serde(default)]
    pub window_pcr_repetition_errors: u64,
    #[serde(default)]
    pub window_nit_errors: u64,
    #[serde(default)]
    pub window_si_repetition_errors: u64,
    #[serde(default)]
    pub window_unreferenced_pid_errors: u64,
    #[serde(default)]
    pub window_sdt_errors: u64,
    #[serde(default)]
    pub window_eit_errors: u64,
    #[serde(default)]
    pub window_rst_errors: u64,
    #[serde(default)]
    pub window_tdt_errors: u64,

    // ── VSF TR-07 (JPEG XS over MPEG-2 TS per SMPTE ST 2022-2) ──

    /// Whether the stream is TR-07 compliant (JPEG XS detected in PMT).
    pub tr07_compliant: bool,
    /// PID of the JPEG XS elementary stream, if detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jpeg_xs_pid: Option<u16>,
}

/// SRT connection-level statistics for a single SRT socket/leg.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SrtLegStats {
    /// Human-readable SRT socket state, e.g. `"connected"`, `"broken"`.
    pub state: String,
    /// Smoothed round-trip time in milliseconds.
    pub rtt_ms: f64,
    /// Estimated send rate in megabits per second.
    pub send_rate_mbps: f64,
    /// Estimated receive rate in megabits per second.
    pub recv_rate_mbps: f64,
    /// Estimated link bandwidth in megabits per second.
    pub bandwidth_mbps: f64,
    /// Maximum configured bandwidth in megabits per second.
    pub max_bw_mbps: f64,

    // ── Cumulative counters ──

    /// Total packets sent (including retransmissions).
    pub pkt_sent_total: i64,
    /// Total packets received.
    pub pkt_recv_total: i64,
    /// Total packets lost (send + receive directions combined).
    pub pkt_loss_total: i64,
    /// Total send-side lost packets.
    pub pkt_send_loss_total: i32,
    /// Total receive-side lost packets.
    pub pkt_recv_loss_total: i32,
    /// Total packets retransmitted by the SRT ARQ mechanism (sender side).
    pub pkt_retransmit_total: i32,
    /// Total retransmitted packets received (receiver side ARQ metric).
    pub pkt_recv_retransmit_total: i32,
    /// Total packets dropped at receiver (too late for TSBPD delivery).
    pub pkt_recv_drop_total: i32,
    /// Total packets dropped at sender (too late to send).
    pub pkt_send_drop_total: i32,
    /// Total undecryptable packets (encryption mismatch).
    pub pkt_recv_undecrypt_total: i32,
    /// Total bytes sent.
    pub byte_sent_total: u64,
    /// Total bytes received.
    pub byte_recv_total: u64,
    /// Total bytes retransmitted.
    pub byte_retrans_total: u64,
    /// Total bytes dropped at receiver.
    pub byte_recv_drop_total: u64,
    /// Total bytes lost (receiver side).
    pub byte_recv_loss_total: u64,
    /// Total bytes dropped at sender (too late to send).
    pub byte_send_drop_total: u64,
    /// Total undecrypted bytes (encryption mismatch).
    pub byte_recv_undecrypt_total: u64,
    /// Total unique data packets sent (excluding retransmissions).
    pub pkt_sent_unique_total: i64,
    /// Total unique data packets received (excluding retransmissions).
    pub pkt_recv_unique_total: i64,
    /// Total unique bytes sent (excluding retransmissions).
    pub byte_sent_unique_total: u64,
    /// Total unique bytes received (excluding retransmissions).
    pub byte_recv_unique_total: u64,

    // ── ACK/NAK counters ──

    /// Total ACK packets sent.
    pub pkt_sent_ack_total: i32,
    /// Total ACK packets received.
    pub pkt_recv_ack_total: i32,
    /// Total NAK packets sent.
    pub pkt_sent_nak_total: i32,
    /// Total NAK packets received.
    pub pkt_recv_nak_total: i32,

    // ── Flow control / buffer state ──

    /// Flow window size in packets.
    pub pkt_flow_window: i32,
    /// Congestion window size in packets.
    pub pkt_congestion_window: i32,
    /// Packets currently in flight.
    pub pkt_flight_size: i32,
    /// Available send buffer in bytes.
    pub byte_avail_send_buf: i32,
    /// Available receive buffer in bytes.
    pub byte_avail_recv_buf: i32,
    /// Send buffer latency in milliseconds.
    pub ms_send_buf: i32,
    /// Receive buffer latency in milliseconds.
    pub ms_recv_buf: i32,
    /// Negotiated sender TSBPD delay in milliseconds.
    pub ms_send_tsbpd_delay: i32,
    /// Negotiated receiver TSBPD delay in milliseconds.
    pub ms_recv_tsbpd_delay: i32,

    // ── Buffer occupancy ──

    /// Unacknowledged packets in sender buffer.
    pub pkt_send_buf: i32,
    /// Unacknowledged bytes in sender buffer.
    pub byte_send_buf: i32,
    /// Undelivered packets in receiver buffer.
    pub pkt_recv_buf: i32,
    /// Undelivered bytes in receiver buffer.
    pub byte_recv_buf: i32,

    // ── Pacing ──

    /// Packet sending period in microseconds (congestion control pacing).
    pub us_pkt_send_period: f64,

    // ── Reorder / belated ──

    /// Packet reorder distance.
    pub pkt_reorder_distance: i32,
    /// Packet reorder tolerance.
    pub pkt_reorder_tolerance: i32,
    /// Packets received but too late for TSBPD delivery (ARQ failed to recover in time).
    pub pkt_recv_belated: i64,
    /// Average lateness of belated packets in milliseconds.
    pub pkt_recv_avg_belated_time: f64,

    // ── FEC (packet filter) statistics ──

    /// FEC packets sent (overhead, total).
    pub pkt_send_filter_extra_total: i32,
    /// FEC packets received (total).
    pub pkt_recv_filter_extra_total: i32,
    /// Packets recovered by FEC (total).
    pub pkt_recv_filter_supply_total: i32,
    /// Unrecoverable FEC losses (total).
    pub pkt_recv_filter_loss_total: i32,
    /// FEC packets sent (overhead, interval).
    pub pkt_send_filter_extra: i32,
    /// Packets recovered by FEC (interval).
    pub pkt_recv_filter_supply: i32,
    /// Unrecoverable FEC losses (interval).
    pub pkt_recv_filter_loss: i32,

    /// Milliseconds since the SRT socket was connected (socket uptime).
    pub uptime_ms: i64,
}

/// One member link inside a native libsrt SRT bonding group.
///
/// Populated by the SRT bonding stats poller from
/// `srt_transport::SrtGroup::member_stats()`. Each entry corresponds to one
/// SRT path (one NIC / one peer) participating in the group.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SrtBondingMemberStats {
    /// "ip:port" of the peer this member is connected to.
    pub endpoint: String,
    /// Underlying SRT socket status — `"connected"`, `"broken"`, etc.
    pub socket_status: String,
    /// Group-level member status — `"running"` (active), `"idle"` (standby
    /// backup), `"pending"` (negotiating), `"broken"`.
    pub member_status: String,
    /// Backup-mode priority; lower is preferred. 0 in broadcast mode.
    pub weight: u16,
    /// Full SRT stats for this member link.
    pub stats: SrtLegStats,
}

/// Aggregate + per-member stats for a native libsrt SRT bonding
/// (socket-group) input or output.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SrtBondingStats {
    /// Bonding mode string — `"broadcast"` or `"backup"`.
    pub mode: String,
    /// Aggregate group-level stats (what libsrt reports on the group
    /// socket itself).
    pub aggregate: SrtLegStats,
    /// One entry per member link.
    pub members: Vec<SrtBondingMemberStats>,
}

/// RIST Simple Profile connection-level statistics for a single RIST
/// socket/leg. Populated from [`rist_transport::RistConnStats`] by
/// [`engine::input_rist`] / [`engine::output_rist`] at stats-snapshot time.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RistLegStats {
    /// Human-readable RIST state, e.g. `"receiving"`, `"sending"`, `"idle"`.
    pub state: String,
    /// Socket role: `"sender"` or `"receiver"`.
    pub role: String,
    /// Smoothed round-trip time in milliseconds.
    pub rtt_ms: f64,
    /// Interarrival jitter (RFC 3550 A.8) in microseconds.
    pub jitter_us: u64,

    // ── Sender-side counters ──
    /// Total RTP packets transmitted (including retransmissions).
    pub packets_sent: u64,
    /// Total bytes transmitted.
    pub bytes_sent: u64,
    /// Total packets retransmitted in response to NACKs.
    pub pkt_retransmit_total: u64,
    /// Total packets requested via NACKs received from the receiver.
    pub nack_received_total: u64,

    // ── Receiver-side counters ──
    /// Total RTP packets received (unique + retransmits).
    pub packets_received: u64,
    /// Total bytes received.
    pub bytes_received: u64,
    /// Total packets NOT recovered by ARQ — dropped before delivery.
    pub packets_lost: u64,
    /// Total lost packets subsequently recovered via retransmit.
    pub packets_recovered: u64,
    /// Total NACK feedback messages sent to the peer.
    pub nack_sent_total: u64,
    /// Total duplicate packets ignored at the reorder buffer.
    pub duplicates: u64,
    /// Total packets dropped because the application consumer was lagging
    /// or the packet arrived outside the reorder window.
    pub reorder_drops: u64,
    /// Total RTP data packets received with the RIST retransmit flag set
    /// (SSRC LSB=1). Authoritative count of ARQ deliveries — distinct
    /// from `packets_recovered`, which also includes natural
    /// out-of-order arrivals that happen to fill a gap.
    pub retransmits_received: u64,
}

/// Bonded flow connection-level statistics. Populated from
/// [`bonding_protocol::stats::BondConnStats`] and the per-path
/// [`bonding_protocol::stats::PathStats`] by
/// [`engine::input_bonded`] / [`engine::output_bonded`] at
/// stats-snapshot time.
///
/// One `BondLegStats` per bonded input or output — the `paths` Vec
/// carries per-path detail so the manager UI can show each leg's
/// RTT, loss, throughput, and liveness independently of the
/// aggregate.
#[derive(Debug, Clone, Serialize, Default)]
pub struct BondLegStats {
    /// Overall bond state — `"up"` when at least one path is alive
    /// and traffic is flowing, `"idle"` when no paths have carried
    /// traffic, `"degraded"` when one or more paths have been
    /// declared dead.
    pub state: String,
    /// Bond-protocol flow identifier matched between sender and
    /// receiver. Opaque to the UI — useful for log correlation.
    pub flow_id: u32,
    /// `"sender"` or `"receiver"` — which side of the bond this
    /// stats entry describes.
    pub role: String,
    /// Scheduler kind configured on this output (sender side only:
    /// `"round_robin"`, `"weighted_rtt"`, `"media_aware"`). Empty
    /// string on the receiver side.
    pub scheduler: String,
    /// Aggregate bond bandwidth (bits/sec) — sum of the per-path
    /// realized throughput in the bond's active direction (bytes_sent
    /// for a sender, bytes_received for a receiver). Sampled at 1 Hz
    /// off the byte counters; `0` until the first interval elapses.
    /// Older edges omit this (serde default `0`).
    #[serde(default)]
    pub throughput_bps: u64,
    /// Aggregate proactive **FEC repair** bandwidth across all legs
    /// (bits/sec) — sum of the per-leg `fec_throughput_bps`. Redundancy
    /// overhead on top of `throughput_bps`. Sender side only; 0 on the
    /// receiver side and on older edges (serde default).
    #[serde(default)]
    pub fec_throughput_bps: u64,
    /// Aggregate **total** wire bandwidth across all legs (bits/sec) —
    /// sum of the per-leg `wire_throughput_bps` (media + retransmits +
    /// duplicates + FEC + AEAD envelope). The honest gross wire load of
    /// the whole bond. Sender side only; 0 on the receiver side and on
    /// older edges (serde default).
    #[serde(default)]
    pub wire_throughput_bps: u64,
    /// Receiver side only: the adaptive hold servo's **current**
    /// reorder/recovery budget in milliseconds (floor `hold_ms`,
    /// ceiling `hold_max_ms`; fixed at `hold_ms` when no ceiling is
    /// configured). `None` on the sender side and on older edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u64>,
    /// Receiver side: times the bond dropped its reassembly anchor
    /// because the sender restarted with a new session epoch. Older
    /// edges omit this (serde default `0`).
    #[serde(default)]
    pub session_resets: u64,
    /// Sender side: payloads that exceeded the per-datagram MTU budget
    /// (sent anyway — the bond layer neither drops nor fragments; see
    /// the `bond_payload_exceeds_mtu` event). Older edges omit this.
    #[serde(default)]
    pub oversize_payloads: u64,

    // ── Aggregate sender-side counters ──
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_retransmitted: u64,
    /// Packets intentionally duplicated by the scheduler (e.g.
    /// `Critical`-priority IDR frames sent on two paths).
    pub packets_duplicated: u64,
    /// Packets the scheduler couldn't dispatch because no path was
    /// alive — failed-hard bonds register here.
    pub packets_dropped_no_path: u64,

    // ── Aggregate receiver-side counters ──
    pub packets_received: u64,
    pub bytes_received: u64,
    pub packets_delivered: u64,
    pub gaps_recovered: u64,
    pub gaps_lost: u64,
    pub duplicates_received: u64,
    /// Datagrams that arrived too late to use (already-delivered/aged-out
    /// `bond_seq`, or out of the ring window) — a leg contributing copies
    /// too late to help, NOT reassembly-ring exhaustion. Formerly
    /// `reassembly_overflow`; aliased for back-compat on ingest.
    #[serde(alias = "reassembly_overflow")]
    pub late_stale_drops: u64,

    /// Per-path detail. One entry per path registered at socket
    /// creation. Order matches the config.
    pub paths: Vec<BondPathLegStats>,
}

/// Per-path counters for a bonded flow.
#[derive(Debug, Clone, Serialize, Default)]
pub struct BondPathLegStats {
    pub id: u8,
    pub name: String,
    /// `"udp"`, `"quic"`, or `"rist"`.
    pub transport: String,
    /// `"alive"` or `"dead"`.
    pub state: String,
    pub rtt_ms: f64,
    pub jitter_us: u64,
    /// Per-leg equalization: receiver-measured relative one-way delay (µs) —
    /// how much later this leg's packets land than the fastest eligible leg.
    /// 0 when equalization is off / this leg is the fastest / un-measured.
    #[serde(default)]
    pub relative_owd_us: u64,
    pub loss_fraction: f64,
    /// Rate the sender **put on** this link (bits/sec, off the byte
    /// counter for the bond's active direction).
    pub throughput_bps: u64,
    /// Rate the receiver reports actually **arriving** on this link
    /// (bits/sec, windowed, from the v2 keepalive byte feedback). The
    /// gap between `throughput_bps` and this is loss / saturation. The
    /// adaptive scheduler fills each link toward this discovered
    /// capacity. Sender side only; 0 on the receiver side and on legacy
    /// edges (serde default).
    #[serde(default)]
    pub delivered_bps: u64,
    /// Proactive **FEC repair** bandwidth this edge adds on the leg
    /// (bits/sec) — combined or per-leg XOR / Reed-Solomon. This is pure
    /// redundancy *on top of* `throughput_bps`: it is deliberately NOT
    /// part of the media byte counter (folding it in would read as loss
    /// to the congestion controller). 0 when FEC is off, on the receiver
    /// side, and on legacy edges (serde default).
    #[serde(default)]
    pub fec_throughput_bps: u64,
    /// **Total** bandwidth actually on this leg's wire (bits/sec) in the
    /// send direction: media + retransmits + duplicates + FEC repair,
    /// each including its bond header and (on an encrypted UDP leg) the
    /// 29-byte AEAD envelope. The honest wire load — `throughput_bps`
    /// plus the FEC + encryption overhead the media counter omits;
    /// excludes only the OS-level UDP/QUIC/IP framing below the bond.
    /// Sender side only; 0 on the receiver side and on legacy edges.
    #[serde(default)]
    pub wire_throughput_bps: u64,
    /// The adaptive scheduler's **discovered usable capacity** for this
    /// link (bits/sec) — what the congestion controller believes the
    /// link can carry and fills it toward. 0 for the non-adaptive
    /// policies, the receiver side, and legacy edges (serde default).
    #[serde(default)]
    pub capacity_bps: u64,
    pub queue_depth: u64,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub nacks_sent: u64,
    pub nacks_received: u64,
    pub retransmits_sent: u64,
    pub retransmits_received: u64,
    pub keepalives_sent: u64,
    pub keepalives_received: u64,
    /// Socket rebuilds performed on this leg by the interface watcher
    /// (UDP paths only — interface churn / send-error runs). Older
    /// edges omit this (serde default `0`).
    #[serde(default)]
    pub rebuilds: u64,
    /// Packets recovered by THIS leg's per-leg FEC (XOR or Reed-Solomon).
    /// 0 when the leg has no per-leg FEC. Distinct from the bond-aggregate
    /// `gaps_recovered` — lets the operator see each leg's proactive FEC
    /// at work. Older edges omit this (serde default `0`).
    #[serde(default)]
    pub fec_recovered: u64,
    /// How this leg's egress is pinned to its link:
    /// `"gateway"` (edge-programmed policy route via a router),
    /// `"so_bindtodevice"` (hard NIC bind, needs CAP_NET_RAW),
    /// `"ip_unicast_if"` (unprivileged NIC egress hint),
    /// `"ip_bound_if"` (Apple/BSD bind), or `"none"` (kernel default route).
    #[serde(default)]
    pub binding: String,
    /// Kernel netdev this leg egresses on (interface-mode UDP legs only).
    /// Lets the manager UI join the leg to its `network_interfaces[].cellular`
    /// radio state and draw a signal strip on the leg row. `None` for
    /// gateway-mode legs (the gateway IP + policy route do the steering), for
    /// the receiver side, for QUIC/RIST legs, and on older edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
}

// ── Media Analysis ────────────────────────────────────────────────────────

/// Media content analysis statistics for a single flow.
///
/// Reports detected codecs, resolution, frame rate, audio format, and
/// transport-level features (FEC, redundancy) derived from MPEG-TS PSI
/// tables and elementary stream headers.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MediaAnalysisStats {
    /// Transport protocol: "rtp", "srt", "rtmp".
    pub protocol: String,
    /// Payload format: "rtp_ts" or "raw_ts".
    pub payload_format: String,
    /// FEC information (if configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fec: Option<FecInfo>,
    /// Redundancy information (if configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedundancyInfo>,
    /// Number of MPEG-TS programs detected in PAT.
    pub program_count: u16,
    /// Per-program elementary stream breakdown (one entry per PMT).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<ProgramInfo>,
    /// Detected video elementary streams (flat union across all programs,
    /// kept for backwards compatibility with older manager UIs).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub video_streams: Vec<VideoStreamInfo>,
    /// Detected audio elementary streams (flat union across all programs,
    /// kept for backwards compatibility with older manager UIs).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_streams: Vec<AudioStreamInfo>,
    /// Aggregate TS bitrate in bits per second.
    pub total_bitrate_bps: u64,
}

/// Detected MPEG-TS program (one PMT) and its elementary streams.
#[derive(Debug, Clone, Serialize)]
pub struct ProgramInfo {
    /// MPEG-TS program_number from the PAT.
    pub program_number: u16,
    /// PID of this program's PMT.
    pub pmt_pid: u16,
    /// Video elementary streams belonging to this program.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub video_streams: Vec<VideoStreamInfo>,
    /// Audio elementary streams belonging to this program.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_streams: Vec<AudioStreamInfo>,
    /// Sum of bitrates of all elementary streams in this program (bits/sec).
    pub total_bitrate_bps: u64,
}

/// FEC configuration information.
#[derive(Debug, Clone, Serialize)]
pub struct FecInfo {
    /// FEC standard, e.g. "SMPTE 2022-1".
    pub standard: String,
    /// Column count (L parameter).
    pub columns: u8,
    /// Row count (D parameter).
    pub rows: u8,
}

/// True SMPTE 2022-7 buffered-merger snapshot. Surfaces the
/// path-differential measurement, per-leg health, and the loss
/// breakdown the manager UI uses to render asymmetric-path metrics.
#[derive(Debug, Clone, Serialize, Default)]
pub struct BufferedHitlessSnapshot {
    /// Configured skew-accommodation buffer in ms (the constant
    /// emission latency).
    pub max_path_diff_ms: u32,
    /// Currently-observed path differential between the two legs in
    /// microseconds (last sample). Useful as a live debug widget.
    pub current_path_diff_us: u64,
    /// Largest path differential observed since flow start. Operators
    /// use this to size `path_differential_ms` — set it ≥ this value
    /// with headroom.
    pub max_observed_path_diff_us: u64,
    /// Total packets emitted by the merger.
    pub packets_emitted: u64,
    /// Packets where both legs delivered (the canonical 2022-7
    /// happy path — every emitted packet had hitless coverage).
    pub packets_via_both_legs: u64,
    /// Packets emitted where only leg 1 delivered. Loss on leg 2.
    pub packets_via_leg1_only: u64,
    /// Packets emitted where only leg 2 delivered. Loss on leg 1.
    pub packets_via_leg2_only: u64,
    /// Duplicate packets the merger correctly dropped.
    pub dups_dropped: u64,
    /// Sequence-number gaps where neither leg ever delivered.
    pub gap_lost: u64,
    /// Packets that arrived after their hold window had expired (the
    /// path differential was misconfigured or the leg latency
    /// momentarily spiked beyond `max_path_diff`).
    pub late_dropped: u64,
    /// Packets dropped because the buffer reached its overflow cap.
    /// Sustained non-zero values indicate a misconfigured
    /// `path_differential_ms` (set too high).
    pub buffer_overflow_dropped: u64,
    /// Number of times the merger reset its bitmap on a cross-window
    /// forward jump (publisher restart / new ISN).
    pub stream_resets: u64,
    /// Failovers between legs at emission time.
    pub failovers: u64,
    /// Live buffer depth (packets in flight).
    pub current_buffer_depth: u64,
    /// SSRC mismatch counter — increments when the two legs report
    /// different RTP SSRCs (a misconfiguration; 2022-7 mandates
    /// byte-identical streams including SSRC).
    pub ssrc_mismatch: u64,
    /// Per-leg packet receive counts.
    pub leg1_packets_received: u64,
    pub leg2_packets_received: u64,
    /// Per-leg health flag.
    /// 0 = unknown, 1 = up, 2 = degraded, 3 = down.
    pub leg1_health: u8,
    pub leg2_health: u8,
}

/// Redundancy configuration information.
#[derive(Debug, Clone, Serialize)]
pub struct RedundancyInfo {
    /// Redundancy standard, e.g. "SMPTE 2022-7".
    pub standard: String,
}

/// Detected video elementary stream information.
#[derive(Debug, Clone, Serialize)]
pub struct VideoStreamInfo {
    /// MPEG-TS PID of this video elementary stream.
    pub pid: u16,
    /// Human-readable codec name, e.g. "H.264/AVC", "H.265/HEVC", "JPEG XS".
    pub codec: String,
    /// MPEG-TS stream_type value from PMT.
    pub stream_type: u8,
    /// Detected resolution, e.g. "1920x1080".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Detected frame rate in frames per second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    /// Codec profile, e.g. "High", "Main".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Codec level, e.g. "4.0", "5.1".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// Estimated bitrate for this PID in bits per second.
    pub bitrate_bps: u64,
}

/// Detected audio elementary stream information.
#[derive(Debug, Clone, Serialize)]
pub struct AudioStreamInfo {
    /// MPEG-TS PID of this audio elementary stream.
    pub pid: u16,
    /// Human-readable codec name, e.g. "AAC-LC", "AC-3", "E-AC-3".
    pub codec: String,
    /// MPEG-TS stream_type value from PMT.
    pub stream_type: u8,
    /// Sample rate in Hz, e.g. 48000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<u32>,
    /// Number of audio channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
    /// ISO 639 language code, e.g. "eng", "fra".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Estimated bitrate for this PID in bits per second.
    pub bitrate_bps: u64,
}

// ── Thumbnail Generation ─────────────────────────────────────────────────

/// Thumbnail generation statistics for a single flow.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ThumbnailStats {
    /// Whether thumbnail generation is active for this flow.
    pub enabled: bool,
    /// Total thumbnails successfully captured since flow start.
    pub total_captured: u64,
    /// Total capture errors (ffmpeg failures, timeouts) since flow start.
    pub capture_errors: u64,
    /// Whether a thumbnail is currently available.
    pub has_thumbnail: bool,
    /// Active thumbnail alarm: `"black"` if the frame is all-dark,
    /// `"frozen"` if the same frame has been captured for 3+ consecutive
    /// intervals (30 s+). Absent when no alarm is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alarm: Option<String>,
    /// Most recent capture-failure reason — verbatim error string from
    /// the decode path (`"no video frames buffered"`, `"in-process
    /// thumbnail decode failed: ..."`, `"thumbnail task panicked: ..."`).
    /// Replaced on every error and cleared on the next successful capture
    /// so the snapshot reflects the *current* failure state, not the
    /// historical one. Lets operators distinguish "decoder rejecting the
    /// AU" from "no buffered AUs to decode" without enabling debug logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}
