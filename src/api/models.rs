// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use serde::Serialize;

use crate::config::models::FlowConfig;
use crate::stats::models::FlowStats;

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

impl From<&FlowConfig> for FlowSummary {
    fn from(flow: &FlowConfig) -> Self {
        use crate::config::models::{InputConfig, OutputConfig};

        let (input_type, input_redundancy) = match &flow.input {
            InputConfig::Rtp(_) => ("rtp", false),
            InputConfig::Udp(_) => ("udp", false),
            InputConfig::Srt(srt) => ("srt", srt.redundancy.is_some()),
            InputConfig::Rtmp(_) => ("rtmp", false),
            InputConfig::Rtsp(_) => ("rtsp", false),
            InputConfig::Webrtc(_) => ("webrtc", false),
            InputConfig::Whep(_) => ("whep", false),
        };
        let output_redundancy = flow.outputs.iter().any(|o| {
            matches!(o, OutputConfig::Srt(srt) if srt.redundancy.is_some())
        });
        Self {
            id: flow.id.clone(),
            name: flow.name.clone(),
            enabled: flow.enabled,
            input_type: input_type.to_string(),
            output_count: flow.outputs.len(),
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
