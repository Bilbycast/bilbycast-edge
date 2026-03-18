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
    /// Initialising connections and resources; not yet forwarding packets.
    Starting,
    /// Actively receiving and forwarding media packets.
    Running,
    /// The flow encountered an unrecoverable error. The `String` contains a
    /// human-readable description of what went wrong.
    Error(String),
    /// The flow was explicitly stopped by the operator.
    Stopped,
}

/// Statistics for a flow's input leg.
#[derive(Debug, Clone, Serialize, Default)]
pub struct InputStats {
    /// Transport protocol type, e.g. `"srt"`, `"udp"`.
    pub input_type: String,
    /// Human-readable connection state, e.g. `"receiving"`, `"connecting"`.
    pub state: String,
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
    /// Transport protocol type, e.g. `"srt"`, `"udp"`.
    pub output_type: String,
    /// Human-readable connection state, e.g. `"active"`, `"connecting"`.
    pub state: String,
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
    /// Total packets lost (send + receive directions combined).
    pub pkt_loss_total: i64,
    /// Total packets retransmitted by the SRT ARQ mechanism.
    pub pkt_retransmit_total: i32,
    /// Milliseconds since the SRT socket was connected (socket uptime).
    pub uptime_ms: i64,
}
