use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::stats::models::FlowStats;

use super::errors::ApiError;
use super::models::{AllStatsResponse, ApiResponse, HealthResponse, SystemStats};
use super::server::AppState;

/// `GET /api/v1/stats` -- Retrieve aggregated system and per-flow statistics.
///
/// Returns an [`AllStatsResponse`] containing:
/// - **System stats**: uptime, total configured flows, active (running) flows, and the
///   application version.
/// - **Per-flow stats**: live statistics from every running flow (packets, bytes, bitrate,
///   loss, FEC, SRT metrics). Flows that are configured but not currently running are
///   included as idle entries with default/zeroed counters, ensuring the response always
///   covers every flow in the configuration.
///
/// # Errors
///
/// This handler is infallible under normal operation.
pub async fn all_stats(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<AllStatsResponse>>, ApiError> {
    let config = state.config.read().await;
    let uptime = state.start_time.elapsed().as_secs();

    // Get real stats from running flows, with fallback for non-running flows
    let mut flow_stats = state.flow_manager.stats().all_snapshots();

    // Include configured-but-not-running flows as idle
    for flow_cfg in &config.flows {
        if !flow_stats.iter().any(|s| s.flow_id == flow_cfg.id) {
            flow_stats.push(FlowStats {
                flow_id: flow_cfg.id.clone(),
                flow_name: flow_cfg.name.clone(),
                ..Default::default()
            });
        }
    }

    let response = AllStatsResponse {
        system: SystemStats {
            uptime_secs: uptime,
            total_flows: config.flows.len(),
            active_flows: state.flow_manager.active_flow_count(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        flows: flow_stats,
    };

    Ok(Json(ApiResponse::ok(response)))
}

/// `GET /api/v1/stats/{flow_id}` -- Retrieve statistics for a single flow.
///
/// First attempts to return live statistics from the engine for the requested flow.
/// If the flow is not currently running (but exists in the configuration), falls back
/// to returning a [`FlowStats`] populated only with the flow's ID and name from the
/// config, with all counters at their default (zero) values. This ensures the endpoint
/// always returns useful information for any configured flow.
///
/// # Errors
///
/// - [`ApiError::NotFound`] (404) if no flow with the given `flow_id` exists in either
///   the engine or the configuration.
pub async fn flow_stats(
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<ApiResponse<FlowStats>>, ApiError> {
    // First check if we have live stats from the engine
    if let Some(stats) = state.flow_manager.stats().flow_snapshot(&flow_id) {
        return Ok(Json(ApiResponse::ok(stats)));
    }

    // Fall back to config-only info
    let config = state.config.read().await;
    let flow = config
        .flows
        .iter()
        .find(|f| f.id == flow_id)
        .ok_or_else(|| ApiError::NotFound(format!("Flow '{flow_id}' not found")))?;

    let stats = FlowStats {
        flow_id: flow.id.clone(),
        flow_name: flow.name.clone(),
        ..Default::default()
    };

    Ok(Json(ApiResponse::ok(stats)))
}

/// `GET /health` -- Lightweight health check endpoint.
///
/// Returns a [`HealthResponse`] JSON object with the following fields:
/// - `status`: always `"ok"` if the server is responsive.
/// - `version`: the SRTEdge application version (from `CARGO_PKG_VERSION`).
/// - `uptime_secs`: seconds since the application started.
/// - `active_flows`: number of flows currently running in the engine.
/// - `total_flows`: total number of flows defined in the configuration.
///
/// This endpoint is suitable for use with load balancers, orchestrators, and
/// monitoring systems that need a simple availability check.
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let config = state.config.read().await;
    let uptime = state.start_time.elapsed().as_secs();
    let active = state.flow_manager.active_flow_count();

    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: uptime,
        active_flows: active,
        total_flows: config.flows.len(),
    })
}

/// `GET /metrics` -- Prometheus-compatible metrics endpoint.
///
/// Returns metrics in the Prometheus text exposition format (`text/plain; version=0.0.4`).
/// Metrics are organized into the following categories:
///
/// **Application-level gauges:**
/// - `srtedge_info` -- application version label
/// - `srtedge_uptime_seconds` -- time since startup in seconds
/// - `srtedge_flows_total` -- total number of configured flows
/// - `srtedge_flows_active` -- number of currently running flows
///
/// **Per-flow input counters/gauges** (labeled by `flow_id`):
/// - `srtedge_flow_input_packets_total` -- total RTP packets received
/// - `srtedge_flow_input_bytes_total` -- total bytes received
/// - `srtedge_flow_input_bitrate_bps` -- current input bitrate (bits/sec)
/// - `srtedge_flow_input_packets_lost` -- total packets lost (sequence gaps)
/// - `srtedge_flow_input_fec_recovered_total` -- packets recovered via FEC
/// - `srtedge_flow_input_redundancy_switches_total` -- SMPTE 2022-7 leg switches
///
/// **Per-output counters/gauges** (labeled by `flow_id` + `output_id`):
/// - `srtedge_flow_output_packets_total` -- total packets sent
/// - `srtedge_flow_output_bytes_total` -- total bytes sent
/// - `srtedge_flow_output_bitrate_bps` -- current output bitrate (bits/sec)
/// - `srtedge_flow_output_packets_dropped` -- packets dropped due to lag
/// - `srtedge_flow_output_fec_sent_total` -- FEC packets sent
///
/// **SRT-specific gauges** (labeled by `flow_id`, optionally `output_id` and `leg`):
/// - `srtedge_srt_rtt_ms` -- SRT round-trip time in milliseconds
/// - `srtedge_srt_loss_total` -- SRT total packet loss counter
///
/// Only metrics for currently running flows are emitted; idle flows are excluded.
pub async fn prometheus_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let uptime = state.start_time.elapsed().as_secs();
    let flow_snapshots = state.flow_manager.stats().all_snapshots();

    let mut output = String::new();

    // App info
    output.push_str("# HELP srtedge_info Application info\n");
    output.push_str("# TYPE srtedge_info gauge\n");
    output.push_str(&format!(
        "srtedge_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));

    output.push_str("# HELP srtedge_uptime_seconds Application uptime\n");
    output.push_str("# TYPE srtedge_uptime_seconds gauge\n");
    output.push_str(&format!("srtedge_uptime_seconds {uptime}\n"));

    output.push_str("# HELP srtedge_flows_total Total configured flows\n");
    output.push_str("# TYPE srtedge_flows_total gauge\n");
    output.push_str(&format!("srtedge_flows_total {}\n", config.flows.len()));

    output.push_str("# HELP srtedge_flows_active Currently running flows\n");
    output.push_str("# TYPE srtedge_flows_active gauge\n");
    output.push_str(&format!(
        "srtedge_flows_active {}\n",
        state.flow_manager.active_flow_count()
    ));

    // Per-flow input metrics
    if !flow_snapshots.is_empty() {
        output.push_str("\n# HELP srtedge_flow_input_packets_total Total RTP packets received\n");
        output.push_str("# TYPE srtedge_flow_input_packets_total counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "srtedge_flow_input_packets_total{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.packets_received
            ));
        }

        output.push_str("\n# HELP srtedge_flow_input_bytes_total Total bytes received\n");
        output.push_str("# TYPE srtedge_flow_input_bytes_total counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "srtedge_flow_input_bytes_total{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.bytes_received
            ));
        }

        output.push_str("\n# HELP srtedge_flow_input_bitrate_bps Input bitrate in bits per second\n");
        output.push_str("# TYPE srtedge_flow_input_bitrate_bps gauge\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "srtedge_flow_input_bitrate_bps{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.bitrate_bps
            ));
        }

        output.push_str("\n# HELP srtedge_flow_input_packets_lost Total packets lost (sequence gaps)\n");
        output.push_str("# TYPE srtedge_flow_input_packets_lost counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "srtedge_flow_input_packets_lost{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.packets_lost
            ));
        }

        output.push_str("\n# HELP srtedge_flow_input_fec_recovered_total Packets recovered via FEC\n");
        output.push_str("# TYPE srtedge_flow_input_fec_recovered_total counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "srtedge_flow_input_fec_recovered_total{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.packets_recovered_fec
            ));
        }

        output.push_str("\n# HELP srtedge_flow_input_redundancy_switches_total Redundancy leg switches\n");
        output.push_str("# TYPE srtedge_flow_input_redundancy_switches_total counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "srtedge_flow_input_redundancy_switches_total{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.redundancy_switches
            ));
        }

        // Per-output metrics
        output.push_str("\n# HELP srtedge_flow_output_packets_total Total packets sent per output\n");
        output.push_str("# TYPE srtedge_flow_output_packets_total counter\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "srtedge_flow_output_packets_total{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.packets_sent
                ));
            }
        }

        output.push_str("\n# HELP srtedge_flow_output_bytes_total Total bytes sent per output\n");
        output.push_str("# TYPE srtedge_flow_output_bytes_total counter\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "srtedge_flow_output_bytes_total{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.bytes_sent
                ));
            }
        }

        output.push_str("\n# HELP srtedge_flow_output_bitrate_bps Output bitrate in bits per second\n");
        output.push_str("# TYPE srtedge_flow_output_bitrate_bps gauge\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "srtedge_flow_output_bitrate_bps{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.bitrate_bps
                ));
            }
        }

        output.push_str("\n# HELP srtedge_flow_output_packets_dropped Packets dropped due to lag\n");
        output.push_str("# TYPE srtedge_flow_output_packets_dropped counter\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "srtedge_flow_output_packets_dropped{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.packets_dropped
                ));
            }
        }

        output.push_str("\n# HELP srtedge_flow_output_fec_sent_total FEC packets sent per output\n");
        output.push_str("# TYPE srtedge_flow_output_fec_sent_total counter\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "srtedge_flow_output_fec_sent_total{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.fec_packets_sent
                ));
            }
        }

        // SRT-specific metrics (when available)
        output.push_str("\n# HELP srtedge_srt_rtt_ms SRT round-trip time in milliseconds\n");
        output.push_str("# TYPE srtedge_srt_rtt_ms gauge\n");
        for fs in &flow_snapshots {
            if let Some(ref srt) = fs.input.srt_stats {
                output.push_str(&format!(
                    "srtedge_srt_rtt_ms{{flow_id=\"{}\",leg=\"input\"}} {:.1}\n",
                    fs.flow_id, srt.rtt_ms
                ));
            }
            if let Some(ref srt) = fs.input.srt_leg2_stats {
                output.push_str(&format!(
                    "srtedge_srt_rtt_ms{{flow_id=\"{}\",leg=\"input_leg2\"}} {:.1}\n",
                    fs.flow_id, srt.rtt_ms
                ));
            }
            for os in &fs.outputs {
                if let Some(ref srt) = os.srt_stats {
                    output.push_str(&format!(
                        "srtedge_srt_rtt_ms{{flow_id=\"{}\",output_id=\"{}\",leg=\"leg1\"}} {:.1}\n",
                        fs.flow_id, os.output_id, srt.rtt_ms
                    ));
                }
                if let Some(ref srt) = os.srt_leg2_stats {
                    output.push_str(&format!(
                        "srtedge_srt_rtt_ms{{flow_id=\"{}\",output_id=\"{}\",leg=\"leg2\"}} {:.1}\n",
                        fs.flow_id, os.output_id, srt.rtt_ms
                    ));
                }
            }
        }

        output.push_str("\n# HELP srtedge_srt_loss_total SRT total packet loss\n");
        output.push_str("# TYPE srtedge_srt_loss_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref srt) = fs.input.srt_stats {
                output.push_str(&format!(
                    "srtedge_srt_loss_total{{flow_id=\"{}\",leg=\"input\"}} {}\n",
                    fs.flow_id, srt.pkt_loss_total
                ));
            }
            for os in &fs.outputs {
                if let Some(ref srt) = os.srt_stats {
                    output.push_str(&format!(
                        "srtedge_srt_loss_total{{flow_id=\"{}\",output_id=\"{}\",leg=\"leg1\"}} {}\n",
                        fs.flow_id, os.output_id, srt.pkt_loss_total
                    ));
                }
            }
        }
    }

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        output,
    )
}
