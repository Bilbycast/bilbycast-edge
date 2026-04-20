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
}

/// Liveness snapshot for a single input leg within a flow. Shipped inside
/// `FlowStats.inputs_live`; carries just enough state for the manager UI to
/// decide between NO SIGNAL, IDLE, and RECEIVING for every configured input.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PerInputLive {
    pub input_id: String,
    pub input_type: String,
    pub state: String,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub bitrate_bps: u64,
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
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
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
    /// Human-readable connection state, e.g. `"receiving"`, `"connecting"`.
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
}

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

    /// `true` when all Priority 1 error counters are zero.
    pub priority1_ok: bool,
    /// `true` when all Priority 2 error counters are zero.
    pub priority2_ok: bool,

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
    pub reassembly_overflow: u64,

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
    pub loss_fraction: f64,
    pub throughput_bps: u64,
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
}
