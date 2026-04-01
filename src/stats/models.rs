// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

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
    /// Statistics for the flow's input leg.
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
    /// Number of times the active input leg switched between leg 1 and leg 2.
    pub redundancy_switches: u64,
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

    // ── Priority 2 (important) ──

    /// TS packets with the Transport Error Indicator bit set.
    pub tei_errors: u64,
    /// PCR jumps exceeding 100 ms or going backwards.
    pub pcr_discontinuity_errors: u64,
    /// PCR jitter relative to wall clock exceeding 500 ns.
    pub pcr_accuracy_errors: u64,

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
    /// Detected video elementary streams.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub video_streams: Vec<VideoStreamInfo>,
    /// Detected audio elementary streams.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_streams: Vec<AudioStreamInfo>,
    /// Aggregate TS bitrate in bits per second.
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
