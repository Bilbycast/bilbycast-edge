use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use super::models::*;

/// Per-output atomic counters for a single output leg.
///
/// Tracks `packets_sent`, `bytes_sent`, `packets_dropped`, and
/// `fec_packets_sent` as lock-free [`AtomicU64`] values. The hot
/// data-plane path increments these with `Relaxed` ordering; the
/// stats snapshot path reads them to produce an [`OutputStats`].
pub struct OutputStatsAccumulator {
    pub output_id: String,
    pub output_name: String,
    pub output_type: String,
    pub packets_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub fec_packets_sent: AtomicU64,
}

impl OutputStatsAccumulator {
    /// Create a new accumulator with all counters initialised to zero.
    pub fn new(output_id: String, output_name: String, output_type: String) -> Self {
        Self {
            output_id,
            output_name,
            output_type,
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            packets_dropped: AtomicU64::new(0),
            fec_packets_sent: AtomicU64::new(0),
        }
    }

    /// Take a point-in-time snapshot of all atomic counters and return an
    /// [`OutputStats`] value suitable for JSON serialisation.
    pub fn snapshot(&self) -> OutputStats {
        OutputStats {
            output_id: self.output_id.clone(),
            output_name: self.output_name.clone(),
            output_type: self.output_type.clone(),
            state: "active".to_string(),
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bitrate_bps: 0, // TODO: calculate from throughput estimator
            packets_dropped: self.packets_dropped.load(Ordering::Relaxed),
            fec_packets_sent: self.fec_packets_sent.load(Ordering::Relaxed),
            srt_stats: None,
            srt_leg2_stats: None,
        }
    }
}

/// Per-flow atomic counters for a single media flow (one input, N outputs).
///
/// Holds input-side counters (`input_packets`, `input_bytes`, `input_loss`,
/// `fec_recovered`, `redundancy_switches`) as lock-free [`AtomicU64`] values,
/// plus a [`DashMap`] of per-output [`OutputStatsAccumulator`] instances keyed
/// by output ID. This allows each output's hot path to update its own counters
/// independently without contention on a shared lock.
pub struct FlowStatsAccumulator {
    pub flow_id: String,
    pub flow_name: String,
    pub input_type: String,
    pub started_at: Instant,
    // Input counters
    pub input_packets: AtomicU64,
    pub input_bytes: AtomicU64,
    pub input_loss: AtomicU64,
    pub fec_recovered: AtomicU64,
    pub redundancy_switches: AtomicU64,
    // Per-output stats
    pub output_stats: DashMap<String, Arc<OutputStatsAccumulator>>,
}

impl FlowStatsAccumulator {
    /// Create a new accumulator with all counters initialised to zero.
    ///
    /// Records `Instant::now()` as the flow start time for uptime calculation.
    pub fn new(flow_id: String, flow_name: String, input_type: String) -> Self {
        Self {
            flow_id,
            flow_name,
            input_type,
            started_at: Instant::now(),
            input_packets: AtomicU64::new(0),
            input_bytes: AtomicU64::new(0),
            input_loss: AtomicU64::new(0),
            fec_recovered: AtomicU64::new(0),
            redundancy_switches: AtomicU64::new(0),
            output_stats: DashMap::new(),
        }
    }

    /// Register a new output for this flow and return a shared reference to its
    /// [`OutputStatsAccumulator`]. The accumulator is inserted into the internal
    /// `DashMap` keyed by `output_id`.
    pub fn register_output(&self, output_id: String, output_name: String, output_type: String) -> Arc<OutputStatsAccumulator> {
        let acc = Arc::new(OutputStatsAccumulator::new(output_id.clone(), output_name, output_type));
        self.output_stats.insert(output_id, acc.clone());
        acc
    }

    /// Remove an output's accumulator from this flow.
    pub fn unregister_output(&self, output_id: &str) {
        self.output_stats.remove(output_id);
    }

    /// Take a point-in-time snapshot of all input counters and every registered
    /// output's counters, assembling them into a [`FlowStats`] value.
    pub fn snapshot(&self) -> FlowStats {
        let outputs: Vec<OutputStats> = self
            .output_stats
            .iter()
            .map(|entry| entry.value().snapshot())
            .collect();

        FlowStats {
            flow_id: self.flow_id.clone(),
            flow_name: self.flow_name.clone(),
            state: FlowState::Running,
            input: InputStats {
                input_type: self.input_type.clone(),
                state: "receiving".to_string(),
                packets_received: self.input_packets.load(Ordering::Relaxed),
                bytes_received: self.input_bytes.load(Ordering::Relaxed),
                bitrate_bps: 0, // TODO
                packets_lost: self.input_loss.load(Ordering::Relaxed),
                packets_recovered_fec: self.fec_recovered.load(Ordering::Relaxed),
                srt_stats: None,
                srt_leg2_stats: None,
                redundancy_switches: self.redundancy_switches.load(Ordering::Relaxed),
            },
            outputs,
            uptime_secs: self.started_at.elapsed().as_secs(),
        }
    }
}

/// Global statistics registry that holds all flow stats accumulators.
///
/// Backed by a [`DashMap<String, Arc<FlowStatsAccumulator>>`], keyed by
/// flow ID. Engine tasks register/unregister flows at start-up and
/// shutdown. The REST API reads snapshots via [`Self::all_snapshots`] or
/// [`Self::flow_snapshot`] without blocking the data plane.
pub struct StatsCollector {
    pub flow_stats: DashMap<String, Arc<FlowStatsAccumulator>>,
    pub start_time: Instant,
}

impl StatsCollector {
    /// Create an empty stats collector. Records `Instant::now()` as the
    /// global start time (used for system-level uptime reporting).
    pub fn new() -> Self {
        Self {
            flow_stats: DashMap::new(),
            start_time: Instant::now(),
        }
    }

    /// Register a new flow and return a shared reference to its
    /// [`FlowStatsAccumulator`]. Inserts the accumulator into the global
    /// `DashMap` keyed by `flow_id`.
    pub fn register_flow(&self, flow_id: String, flow_name: String, input_type: String) -> Arc<FlowStatsAccumulator> {
        let acc = Arc::new(FlowStatsAccumulator::new(flow_id.clone(), flow_name, input_type));
        self.flow_stats.insert(flow_id, acc.clone());
        acc
    }

    /// Remove a flow's accumulator from the global registry.
    pub fn unregister_flow(&self, flow_id: &str) {
        self.flow_stats.remove(flow_id);
    }

    /// Snapshot every registered flow and return a `Vec` of [`FlowStats`].
    pub fn all_snapshots(&self) -> Vec<FlowStats> {
        self.flow_stats
            .iter()
            .map(|entry| entry.value().snapshot())
            .collect()
    }

    /// Snapshot a single flow by ID. Returns `None` if the flow is not registered.
    pub fn flow_snapshot(&self, flow_id: &str) -> Option<FlowStats> {
        self.flow_stats.get(flow_id).map(|entry| entry.snapshot())
    }
}
