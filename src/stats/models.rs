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
