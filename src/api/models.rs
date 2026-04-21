// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::Serialize;

use crate::config::models::{AppConfig, FlowConfig, InputConfig, OutputConfig};
use crate::stats::models::{FlowStats, SrtLegStats};

/// Generic JSON response envelope used by all API endpoints.
///
/// Every successful response wraps its payload in this structure so clients can
/// uniformly check the `success` flag and access either `data` or `error`.
/// Error responses are handled separately by [`super::errors::ApiError`] which
/// produces a similar JSON shape without the `data` field.
#[derive(Serialize)]
pub struct ApiResponse<T: Serialize> {
    /// Indicates whether the request was successful. Always `true` for responses
    /// produced by [`ApiResponse::ok`].
    pub success: bool,
    /// The response payload. Present for successful responses; omitted (via
    /// `skip_serializing_if`) when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    /// Human-readable error message. Always `None` for successful responses;
    /// omitted from serialization when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    /// Constructs a successful API response wrapping the given `data` payload.
    ///
    /// Sets `success` to `true`, `data` to `Some(data)`, and `error` to `None`.
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }
}

/// Response body for `GET /api/v1/flows`, wrapped inside [`ApiResponse`].
#[derive(Serialize)]
pub struct FlowListResponse {
    /// List of summarized flow definitions from the current configuration.
    pub flows: Vec<FlowSummary>,
}

/// Lightweight summary of a single flow, used in list responses to avoid
/// transmitting the full input/output configuration for every flow.
#[derive(Serialize)]
pub struct FlowSummary {
    /// Unique identifier of the flow.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Whether the flow is marked as enabled in the configuration.
    pub enabled: bool,
    /// The type of the flow's input source: `"rtp"`, `"srt"`, or `"rtmp"`.
    pub input_type: String,
    /// Number of outputs configured for this flow.
    pub output_count: usize,
    /// Whether SMPTE 2022-7 redundancy is configured on the input.
    pub input_redundancy: bool,
    /// Whether any output has SMPTE 2022-7 redundancy configured.
    pub output_redundancy: bool,
}

impl FlowSummary {
    /// Build a flow summary by resolving the flow's input/output references
    /// from the top-level `AppConfig`. If a referenced input or output cannot
    /// be found, the summary degrades gracefully (input_type = "none", etc.).
    pub fn from_flow(flow: &FlowConfig, config: &AppConfig) -> Self {
        // Resolve the active input. A flow can now have multiple inputs, but
        // the summary reflects whichever one is currently active (the single
        // source publishing to the flow's broadcast channel).
        let resolved_input: Option<&InputConfig> = flow
            .input_ids
            .iter()
            .filter_map(|iid| config.inputs.iter().find(|i| i.id == *iid))
            .find(|def| def.active)
            .map(|def| &def.config);

        let (input_type, input_redundancy) = match resolved_input {
            Some(InputConfig::Rtp(_)) => ("rtp", false),
            Some(InputConfig::Udp(_)) => ("udp", false),
            Some(InputConfig::Srt(srt)) => ("srt", srt.redundancy.is_some()),
            Some(InputConfig::Rist(rist)) => ("rist", rist.redundancy.is_some()),
            Some(InputConfig::Rtmp(_)) => ("rtmp", false),
            Some(InputConfig::Rtsp(_)) => ("rtsp", false),
            Some(InputConfig::Webrtc(_)) => ("webrtc", false),
            Some(InputConfig::Whep(_)) => ("whep", false),
            Some(InputConfig::St2110_30(c)) => ("st2110_30", c.redundancy.is_some()),
            Some(InputConfig::St2110_31(c)) => ("st2110_31", c.redundancy.is_some()),
            Some(InputConfig::St2110_40(c)) => ("st2110_40", c.redundancy.is_some()),
            Some(InputConfig::St2110_20(c)) => ("st2110_20", c.redundancy.is_some()),
            Some(InputConfig::St2110_23(c)) => ("st2110_23", c.sub_streams.iter().any(|s| s.redundancy.is_some())),
            Some(InputConfig::RtpAudio(c)) => ("rtp_audio", c.redundancy.is_some()),
            // Bonded inputs use path-level redundancy (N>=2 paths implies
            // redundancy from the topology view). Flag as redundant when
            // more than one path is configured.
            Some(InputConfig::Bonded(c)) => ("bonded", c.paths.len() > 1),
            Some(InputConfig::TestPattern(_)) => ("test_pattern", false),
            None => ("none", false),
        };

        // Resolve outputs
        let resolved_outputs: Vec<&OutputConfig> = flow
            .output_ids
            .iter()
            .filter_map(|oid| config.outputs.iter().find(|o| o.id() == oid))
            .collect();

        let output_redundancy = resolved_outputs.iter().any(|o| match o {
            OutputConfig::Srt(srt) => srt.redundancy.is_some(),
            OutputConfig::St2110_30(c) | OutputConfig::St2110_31(c) => c.redundancy.is_some(),
            OutputConfig::St2110_40(c) => c.redundancy.is_some(),
            OutputConfig::St2110_20(c) => c.redundancy.is_some(),
            OutputConfig::St2110_23(c) => c.sub_streams.iter().any(|s| s.redundancy.is_some()),
            OutputConfig::RtpAudio(c) => c.redundancy.is_some(),
            _ => false,
        });

        Self {
            id: flow.id.clone(),
            name: flow.name.clone(),
            enabled: flow.enabled,
            input_type: input_type.to_string(),
            output_count: resolved_outputs.len(),
            input_redundancy,
            output_redundancy,
        }
    }
}

/// Response body for `GET /api/v1/stats`, wrapped inside [`ApiResponse`].
///
/// Combines system-wide metrics with per-flow statistics in a single response.
#[derive(Serialize)]
pub struct AllStatsResponse {
    /// Per-flow statistics. Includes live stats for running flows and zeroed
    /// placeholder entries for configured-but-idle flows.
    pub flows: Vec<FlowStats>,
    /// System-wide metrics (uptime, flow counts, version).
    pub system: SystemStats,
    /// Independent input inventory with live stats (when assigned to a running flow).
    pub inputs: Vec<InputStatusEntry>,
    /// Independent output inventory with live stats (when assigned to a running flow).
    pub outputs: Vec<OutputStatusEntry>,
}

/// Status entry for a single top-level input, with live stats when assigned to a flow.
#[derive(Serialize)]
pub struct InputStatusEntry {
    pub input_id: String,
    pub input_name: String,
    pub input_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_flow_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_flow_name: Option<String>,
    pub state: String,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub bitrate_bps: u64,
    pub packets_lost: u64,
    pub packets_recovered_fec: u64,
    pub redundancy_switches: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_stats: Option<SrtLegStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_leg2_stats: Option<SrtLegStats>,
}

/// Status entry for a single top-level output, with live stats when assigned to a flow.
#[derive(Serialize)]
pub struct OutputStatusEntry {
    pub output_id: String,
    pub output_name: String,
    pub output_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_flow_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_flow_name: Option<String>,
    pub state: String,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub bitrate_bps: u64,
    pub packets_dropped: u64,
    pub fec_packets_sent: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_stats: Option<SrtLegStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_leg2_stats: Option<SrtLegStats>,
}

/// System-wide metrics included in the stats and health responses.
#[derive(Serialize)]
pub struct SystemStats {
    /// Seconds elapsed since the application started.
    pub uptime_secs: u64,
    /// Total number of flows defined in the configuration (running + stopped).
    pub total_flows: usize,
    /// Number of flows currently running in the engine.
    pub active_flows: usize,
    /// Application version string from `Cargo.toml`.
    pub version: String,
}

/// Response body for `GET /health`.
///
/// Provides a quick snapshot of application liveness and high-level capacity.
/// Intended for consumption by load balancers and monitoring probes.
#[derive(Serialize)]
pub struct HealthResponse {
    /// Health status string. Currently always `"ok"` when the server is responsive.
    pub status: String,
    /// Application version string (e.g., `"0.1.0"`).
    pub version: String,
    /// Seconds elapsed since the application started.
    pub uptime_secs: u64,
    /// Number of flows currently running in the engine.
    pub active_flows: usize,
    /// Total number of flows defined in the configuration.
    pub total_flows: usize,
}
