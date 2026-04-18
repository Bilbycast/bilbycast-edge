// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::stats::models::FlowStats;

use super::errors::ApiError;
use super::models::{
    AllStatsResponse, ApiResponse, HealthResponse, InputStatusEntry, OutputStatusEntry,
    SystemStats,
};
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
/// Gathers all stats from state. Used by both the API and the monitor dashboard.
pub async fn gather_all_stats(state: &AppState) -> AllStatsResponse {
    let config = state.config.read().await;
    let uptime = state.start_time.elapsed().as_secs();

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

    // Build independent input inventory with live stats
    let inputs: Vec<InputStatusEntry> = config
        .inputs
        .iter()
        .map(|inp_def| {
            // Find the flow that references this input (inputs are still
            // exclusive to one flow, but a flow can hold multiple inputs).
            let assigned_flow = config
                .flows
                .iter()
                .find(|f| f.input_ids.iter().any(|id| id == &inp_def.id));

            // If assigned, pull live stats from the flow snapshot
            let live = assigned_flow.and_then(|fc| {
                flow_stats.iter().find(|fs| fs.flow_id == fc.id)
            });

            // Check standby listener status for unassigned inputs
            let standby = state.standby_listeners.as_ref()
                .and_then(|sl| sl.get_status(&inp_def.id));

            InputStatusEntry {
                input_id: inp_def.id.clone(),
                input_name: inp_def.name.clone(),
                input_type: inp_def.config.type_name().to_string(),
                assigned_flow_id: assigned_flow.map(|f| f.id.clone()),
                assigned_flow_name: assigned_flow.map(|f| f.name.clone()),
                state: live
                    .map(|fs| fs.input.state.clone())
                    .or_else(|| standby.as_ref().map(|s| s.state.clone()))
                    .unwrap_or_else(|| {
                        if assigned_flow.is_some() { "Idle".to_string() } else { "Unassigned".to_string() }
                    }),
                packets_received: live.map(|fs| fs.input.packets_received).unwrap_or(0),
                bytes_received: live.map(|fs| fs.input.bytes_received).unwrap_or(0),
                bitrate_bps: live.map(|fs| fs.input.bitrate_bps).unwrap_or(0),
                packets_lost: live.map(|fs| fs.input.packets_lost).unwrap_or(0),
                packets_recovered_fec: live.map(|fs| fs.input.packets_recovered_fec).unwrap_or(0),
                redundancy_switches: live.map(|fs| fs.input.redundancy_switches).unwrap_or(0),
                srt_stats: live.and_then(|fs| fs.input.srt_stats.clone()),
                srt_leg2_stats: live.and_then(|fs| fs.input.srt_leg2_stats.clone()),
            }
        })
        .collect();

    // Build independent output inventory with live stats
    let outputs: Vec<OutputStatusEntry> = config
        .outputs
        .iter()
        .map(|out_cfg| {
            let out_id = out_cfg.id();
            let assigned_flow = config
                .flows
                .iter()
                .find(|f| f.output_ids.iter().any(|oid| oid == out_id));

            // Find the matching OutputStats within the flow snapshot
            let live_output = assigned_flow.and_then(|fc| {
                flow_stats
                    .iter()
                    .find(|fs| fs.flow_id == fc.id)
                    .and_then(|fs| fs.outputs.iter().find(|os| os.output_id == out_id))
            });

            OutputStatusEntry {
                output_id: out_id.to_string(),
                output_name: out_cfg.name().to_string(),
                output_type: out_cfg.type_name().to_string(),
                assigned_flow_id: assigned_flow.map(|f| f.id.clone()),
                assigned_flow_name: assigned_flow.map(|f| f.name.clone()),
                state: live_output
                    .map(|os| os.state.clone())
                    .unwrap_or_else(|| {
                        if assigned_flow.is_some() { "Idle".to_string() } else { "Unassigned".to_string() }
                    }),
                packets_sent: live_output.map(|os| os.packets_sent).unwrap_or(0),
                bytes_sent: live_output.map(|os| os.bytes_sent).unwrap_or(0),
                bitrate_bps: live_output.map(|os| os.bitrate_bps).unwrap_or(0),
                packets_dropped: live_output.map(|os| os.packets_dropped).unwrap_or(0),
                fec_packets_sent: live_output.map(|os| os.fec_packets_sent).unwrap_or(0),
                srt_stats: live_output.and_then(|os| os.srt_stats.clone()),
                srt_leg2_stats: live_output.and_then(|os| os.srt_leg2_stats.clone()),
            }
        })
        .collect();

    AllStatsResponse {
        system: SystemStats {
            uptime_secs: uptime,
            total_flows: config.flows.len(),
            active_flows: state.flow_manager.active_flow_count(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        flows: flow_stats,
        inputs,
        outputs,
    }
}

pub async fn all_stats(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<AllStatsResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(gather_all_stats(&state).await)))
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
/// - `version`: the BilbyCast Edge application version (from `CARGO_PKG_VERSION`).
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
/// - `bilbycast_edge_info` -- application version label
/// - `bilbycast_edge_uptime_seconds` -- time since startup in seconds
/// - `bilbycast_edge_flows_total` -- total number of configured flows
/// - `bilbycast_edge_flows_active` -- number of currently running flows
///
/// **Per-flow input counters/gauges** (labeled by `flow_id`):
/// - `bilbycast_edge_flow_input_packets_total` -- total RTP packets received
/// - `bilbycast_edge_flow_input_bytes_total` -- total bytes received
/// - `bilbycast_edge_flow_input_bitrate_bps` -- current input bitrate (bits/sec)
/// - `bilbycast_edge_flow_input_packets_lost` -- total packets lost (sequence gaps)
/// - `bilbycast_edge_flow_input_fec_recovered_total` -- packets recovered via FEC
/// - `bilbycast_edge_flow_input_redundancy_switches_total` -- SMPTE 2022-7 leg switches
///
/// **Per-output counters/gauges** (labeled by `flow_id` + `output_id`):
/// - `bilbycast_edge_flow_output_packets_total` -- total packets sent
/// - `bilbycast_edge_flow_output_bytes_total` -- total bytes sent
/// - `bilbycast_edge_flow_output_bitrate_bps` -- current output bitrate (bits/sec)
/// - `bilbycast_edge_flow_output_packets_dropped` -- packets dropped due to lag
/// - `bilbycast_edge_flow_output_fec_sent_total` -- FEC packets sent
/// - `bilbycast_edge_flow_output_latency_us` -- end-to-end output latency (min/avg/max)
/// - `bilbycast_edge_flow_output_latency_frames` -- end-to-end output latency in video frames
///
/// **SRT-specific gauges** (labeled by `flow_id`, optionally `output_id` and `leg`):
/// - `bilbycast_edge_srt_rtt_ms` -- SRT round-trip time in milliseconds
/// - `bilbycast_edge_srt_loss_total` -- SRT total packet loss counter
///
/// Only metrics for currently running flows are emitted; idle flows are excluded.
pub async fn prometheus_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let uptime = state.start_time.elapsed().as_secs();
    let flow_snapshots = state.flow_manager.stats().all_snapshots();

    let mut output = String::new();

    // App info
    output.push_str("# HELP bilbycast_edge_info Application info\n");
    output.push_str("# TYPE bilbycast_edge_info gauge\n");
    output.push_str(&format!(
        "bilbycast_edge_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));

    output.push_str("# HELP bilbycast_edge_uptime_seconds Application uptime\n");
    output.push_str("# TYPE bilbycast_edge_uptime_seconds gauge\n");
    output.push_str(&format!("bilbycast_edge_uptime_seconds {uptime}\n"));

    output.push_str("# HELP bilbycast_edge_flows_total Total configured flows\n");
    output.push_str("# TYPE bilbycast_edge_flows_total gauge\n");
    output.push_str(&format!("bilbycast_edge_flows_total {}\n", config.flows.len()));

    output.push_str("# HELP bilbycast_edge_flows_active Currently running flows\n");
    output.push_str("# TYPE bilbycast_edge_flows_active gauge\n");
    output.push_str(&format!(
        "bilbycast_edge_flows_active {}\n",
        state.flow_manager.active_flow_count()
    ));

    // System resource metrics
    {
        use std::sync::atomic::Ordering;
        let rs = &state.resource_state;
        let cpu = rs.cpu_percent_f64();
        let ram = rs.ram_percent_f64();
        let ram_used = rs.ram_used_mb.load(Ordering::Relaxed) * 1024 * 1024;
        let ram_total = rs.ram_total_mb.load(Ordering::Relaxed) * 1024 * 1024;
        let critical = if rs.resources_critical.load(Ordering::Relaxed) { 1 } else { 0 };

        output.push_str("\n# HELP bilbycast_edge_system_cpu_percent System CPU usage percentage\n");
        output.push_str("# TYPE bilbycast_edge_system_cpu_percent gauge\n");
        output.push_str(&format!("bilbycast_edge_system_cpu_percent {cpu:.1}\n"));

        output.push_str("# HELP bilbycast_edge_system_ram_percent System RAM usage percentage\n");
        output.push_str("# TYPE bilbycast_edge_system_ram_percent gauge\n");
        output.push_str(&format!("bilbycast_edge_system_ram_percent {ram:.1}\n"));

        output.push_str("# HELP bilbycast_edge_system_ram_used_bytes System RAM used in bytes\n");
        output.push_str("# TYPE bilbycast_edge_system_ram_used_bytes gauge\n");
        output.push_str(&format!("bilbycast_edge_system_ram_used_bytes {ram_used}\n"));

        output.push_str("# HELP bilbycast_edge_system_ram_total_bytes System RAM total in bytes\n");
        output.push_str("# TYPE bilbycast_edge_system_ram_total_bytes gauge\n");
        output.push_str(&format!("bilbycast_edge_system_ram_total_bytes {ram_total}\n"));

        output.push_str("# HELP bilbycast_edge_system_resources_critical Whether resources are in critical state\n");
        output.push_str("# TYPE bilbycast_edge_system_resources_critical gauge\n");
        output.push_str(&format!("bilbycast_edge_system_resources_critical {critical}\n"));
    }

    // Per-flow input metrics
    if !flow_snapshots.is_empty() {
        output.push_str("\n# HELP bilbycast_edge_flow_input_packets_total Total RTP packets received\n");
        output.push_str("# TYPE bilbycast_edge_flow_input_packets_total counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "bilbycast_edge_flow_input_packets_total{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.packets_received
            ));
        }

        output.push_str("\n# HELP bilbycast_edge_flow_input_bytes_total Total bytes received\n");
        output.push_str("# TYPE bilbycast_edge_flow_input_bytes_total counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "bilbycast_edge_flow_input_bytes_total{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.bytes_received
            ));
        }

        output.push_str("\n# HELP bilbycast_edge_flow_input_bitrate_bps Input bitrate in bits per second\n");
        output.push_str("# TYPE bilbycast_edge_flow_input_bitrate_bps gauge\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "bilbycast_edge_flow_input_bitrate_bps{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.bitrate_bps
            ));
        }

        output.push_str("\n# HELP bilbycast_edge_flow_input_packets_lost Total packets lost (sequence gaps)\n");
        output.push_str("# TYPE bilbycast_edge_flow_input_packets_lost counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "bilbycast_edge_flow_input_packets_lost{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.packets_lost
            ));
        }

        output.push_str("\n# HELP bilbycast_edge_flow_input_fec_recovered_total Packets recovered via FEC\n");
        output.push_str("# TYPE bilbycast_edge_flow_input_fec_recovered_total counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "bilbycast_edge_flow_input_fec_recovered_total{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.packets_recovered_fec
            ));
        }

        output.push_str("\n# HELP bilbycast_edge_flow_input_redundancy_switches_total Redundancy leg switches\n");
        output.push_str("# TYPE bilbycast_edge_flow_input_redundancy_switches_total counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "bilbycast_edge_flow_input_redundancy_switches_total{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.redundancy_switches
            ));
        }

        // RP 2129 metrics
        output.push_str("\n# HELP bilbycast_edge_flow_input_packets_filtered Packets dropped by ingress filters\n");
        output.push_str("# TYPE bilbycast_edge_flow_input_packets_filtered counter\n");
        for fs in &flow_snapshots {
            output.push_str(&format!(
                "bilbycast_edge_flow_input_packets_filtered{{flow_id=\"{}\"}} {}\n",
                fs.flow_id, fs.input.packets_filtered
            ));
        }

        output.push_str("\n# HELP bilbycast_edge_flow_pdv_jitter_us Packet delivery variation (jitter) in microseconds\n");
        output.push_str("# TYPE bilbycast_edge_flow_pdv_jitter_us gauge\n");
        for fs in &flow_snapshots {
            if let Some(jitter) = fs.pdv_jitter_us {
                output.push_str(&format!(
                    "bilbycast_edge_flow_pdv_jitter_us{{flow_id=\"{}\"}} {:.1}\n",
                    fs.flow_id, jitter
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_flow_iat_avg_us Average inter-arrival time in microseconds\n");
        output.push_str("# TYPE bilbycast_edge_flow_iat_avg_us gauge\n");
        for fs in &flow_snapshots {
            if let Some(ref iat) = fs.iat {
                output.push_str(&format!(
                    "bilbycast_edge_flow_iat_avg_us{{flow_id=\"{}\"}} {:.1}\n",
                    fs.flow_id, iat.avg_us
                ));
            }
        }

        // Per-output metrics
        output.push_str("\n# HELP bilbycast_edge_flow_output_packets_total Total packets sent per output\n");
        output.push_str("# TYPE bilbycast_edge_flow_output_packets_total counter\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "bilbycast_edge_flow_output_packets_total{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.packets_sent
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_flow_output_bytes_total Total bytes sent per output\n");
        output.push_str("# TYPE bilbycast_edge_flow_output_bytes_total counter\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "bilbycast_edge_flow_output_bytes_total{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.bytes_sent
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_flow_output_bitrate_bps Output bitrate in bits per second\n");
        output.push_str("# TYPE bilbycast_edge_flow_output_bitrate_bps gauge\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "bilbycast_edge_flow_output_bitrate_bps{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.bitrate_bps
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_flow_output_packets_dropped Packets dropped due to lag\n");
        output.push_str("# TYPE bilbycast_edge_flow_output_packets_dropped counter\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "bilbycast_edge_flow_output_packets_dropped{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.packets_dropped
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_flow_output_fec_sent_total FEC packets sent per output\n");
        output.push_str("# TYPE bilbycast_edge_flow_output_fec_sent_total counter\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                output.push_str(&format!(
                    "bilbycast_edge_flow_output_fec_sent_total{{flow_id=\"{}\",output_id=\"{}\"}} {}\n",
                    fs.flow_id, os.output_id, os.fec_packets_sent
                ));
            }
        }

        // End-to-end latency per output
        output.push_str("\n# HELP bilbycast_edge_flow_output_latency_us End-to-end output latency in microseconds\n");
        output.push_str("# TYPE bilbycast_edge_flow_output_latency_us gauge\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                if let Some(ref lat) = os.latency {
                    output.push_str(&format!(
                        "bilbycast_edge_flow_output_latency_us{{flow_id=\"{}\",output_id=\"{}\",stat=\"min\"}} {}\n",
                        fs.flow_id, os.output_id, lat.min_us
                    ));
                    output.push_str(&format!(
                        "bilbycast_edge_flow_output_latency_us{{flow_id=\"{}\",output_id=\"{}\",stat=\"avg\"}} {}\n",
                        fs.flow_id, os.output_id, lat.avg_us
                    ));
                    output.push_str(&format!(
                        "bilbycast_edge_flow_output_latency_us{{flow_id=\"{}\",output_id=\"{}\",stat=\"max\"}} {}\n",
                        fs.flow_id, os.output_id, lat.max_us
                    ));
                }
            }
        }

        output.push_str("\n# HELP bilbycast_edge_flow_output_latency_frames End-to-end output latency in video frames\n");
        output.push_str("# TYPE bilbycast_edge_flow_output_latency_frames gauge\n");
        for fs in &flow_snapshots {
            for os in &fs.outputs {
                if let Some(ref lat) = os.latency {
                    if let Some(frames) = lat.latency_frames {
                        output.push_str(&format!(
                            "bilbycast_edge_flow_output_latency_frames{{flow_id=\"{}\",output_id=\"{}\"}} {:.2}\n",
                            fs.flow_id, os.output_id, frames
                        ));
                    }
                }
            }
        }

        // SRT-specific metrics (when available)
        output.push_str("\n# HELP bilbycast_edge_srt_rtt_ms SRT round-trip time in milliseconds\n");
        output.push_str("# TYPE bilbycast_edge_srt_rtt_ms gauge\n");
        for fs in &flow_snapshots {
            if let Some(ref srt) = fs.input.srt_stats {
                output.push_str(&format!(
                    "bilbycast_edge_srt_rtt_ms{{flow_id=\"{}\",leg=\"input\"}} {:.1}\n",
                    fs.flow_id, srt.rtt_ms
                ));
            }
            if let Some(ref srt) = fs.input.srt_leg2_stats {
                output.push_str(&format!(
                    "bilbycast_edge_srt_rtt_ms{{flow_id=\"{}\",leg=\"input_leg2\"}} {:.1}\n",
                    fs.flow_id, srt.rtt_ms
                ));
            }
            for os in &fs.outputs {
                if let Some(ref srt) = os.srt_stats {
                    output.push_str(&format!(
                        "bilbycast_edge_srt_rtt_ms{{flow_id=\"{}\",output_id=\"{}\",leg=\"leg1\"}} {:.1}\n",
                        fs.flow_id, os.output_id, srt.rtt_ms
                    ));
                }
                if let Some(ref srt) = os.srt_leg2_stats {
                    output.push_str(&format!(
                        "bilbycast_edge_srt_rtt_ms{{flow_id=\"{}\",output_id=\"{}\",leg=\"leg2\"}} {:.1}\n",
                        fs.flow_id, os.output_id, srt.rtt_ms
                    ));
                }
            }
        }

        // RIST-specific metrics (when available)
        output.push_str("\n# HELP bilbycast_edge_rist_rtt_ms RIST round-trip time in milliseconds\n");
        output.push_str("# TYPE bilbycast_edge_rist_rtt_ms gauge\n");
        output.push_str("# HELP bilbycast_edge_rist_nack_sent_total RIST NACK messages sent by receiver\n");
        output.push_str("# TYPE bilbycast_edge_rist_nack_sent_total counter\n");
        output.push_str("# HELP bilbycast_edge_rist_nack_received_total RIST NACK messages received by sender\n");
        output.push_str("# TYPE bilbycast_edge_rist_nack_received_total counter\n");
        output.push_str("# HELP bilbycast_edge_rist_retransmit_total RIST packets retransmitted by sender\n");
        output.push_str("# TYPE bilbycast_edge_rist_retransmit_total counter\n");
        output.push_str("# HELP bilbycast_edge_rist_packets_lost_total RIST packets not recovered by ARQ\n");
        output.push_str("# TYPE bilbycast_edge_rist_packets_lost_total counter\n");
        output.push_str("# HELP bilbycast_edge_rist_packets_recovered_total RIST packets recovered via retransmit\n");
        output.push_str("# TYPE bilbycast_edge_rist_packets_recovered_total counter\n");
        for fs in &flow_snapshots {
            let push_leg = |out: &mut String, leg_label: &str, rist: &crate::stats::models::RistLegStats, owner_labels: &str| {
                out.push_str(&format!(
                    "bilbycast_edge_rist_rtt_ms{{{},leg=\"{}\"}} {:.1}\n",
                    owner_labels, leg_label, rist.rtt_ms
                ));
                out.push_str(&format!(
                    "bilbycast_edge_rist_nack_sent_total{{{},leg=\"{}\"}} {}\n",
                    owner_labels, leg_label, rist.nack_sent_total
                ));
                out.push_str(&format!(
                    "bilbycast_edge_rist_nack_received_total{{{},leg=\"{}\"}} {}\n",
                    owner_labels, leg_label, rist.nack_received_total
                ));
                out.push_str(&format!(
                    "bilbycast_edge_rist_retransmit_total{{{},leg=\"{}\"}} {}\n",
                    owner_labels, leg_label, rist.pkt_retransmit_total
                ));
                out.push_str(&format!(
                    "bilbycast_edge_rist_packets_lost_total{{{},leg=\"{}\"}} {}\n",
                    owner_labels, leg_label, rist.packets_lost
                ));
                out.push_str(&format!(
                    "bilbycast_edge_rist_packets_recovered_total{{{},leg=\"{}\"}} {}\n",
                    owner_labels, leg_label, rist.packets_recovered
                ));
            };
            let input_labels = format!("flow_id=\"{}\",leg_role=\"input\"", fs.flow_id);
            if let Some(ref rist) = fs.input.rist_stats {
                push_leg(&mut output, "leg1", rist, &input_labels);
            }
            if let Some(ref rist) = fs.input.rist_leg2_stats {
                push_leg(&mut output, "leg2", rist, &input_labels);
            }
            for os in &fs.outputs {
                let output_labels = format!(
                    "flow_id=\"{}\",output_id=\"{}\",leg_role=\"output\"",
                    fs.flow_id, os.output_id
                );
                if let Some(ref rist) = os.rist_stats {
                    push_leg(&mut output, "leg1", rist, &output_labels);
                }
                if let Some(ref rist) = os.rist_leg2_stats {
                    push_leg(&mut output, "leg2", rist, &output_labels);
                }
            }
        }

        // TR-101290 metrics
        output.push_str("\n# HELP bilbycast_edge_tr101290_ts_packets_total TS packets analyzed\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_ts_packets_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_ts_packets_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.ts_packets_analyzed
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_sync_byte_errors_total Sync byte errors\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_sync_byte_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_sync_byte_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.sync_byte_errors
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_cc_errors_total Continuity counter errors\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_cc_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_cc_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.cc_errors
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_pat_errors_total PAT timeout errors\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_pat_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_pat_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.pat_errors
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_pmt_errors_total PMT timeout errors\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_pmt_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_pmt_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.pmt_errors
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_pid_errors_total PID errors (ES PIDs missing)\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_pid_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_pid_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.pid_errors
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_tei_errors_total Transport error indicator errors\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_tei_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_tei_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.tei_errors
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_crc_errors_total CRC-32 errors on PAT/PMT sections\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_crc_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_crc_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.crc_errors
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_pcr_discontinuity_errors_total PCR discontinuity errors\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_pcr_discontinuity_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_pcr_discontinuity_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.pcr_discontinuity_errors
                ));
            }
        }

        output.push_str("\n# HELP bilbycast_edge_tr101290_pcr_accuracy_errors_total PCR accuracy errors\n");
        output.push_str("# TYPE bilbycast_edge_tr101290_pcr_accuracy_errors_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref tr) = fs.tr101290 {
                output.push_str(&format!(
                    "bilbycast_edge_tr101290_pcr_accuracy_errors_total{{flow_id=\"{}\"}} {}\n",
                    fs.flow_id, tr.pcr_accuracy_errors
                ));
            }
        }

        // Media analysis metrics
        for fs in &flow_snapshots {
            if let Some(ref ma) = fs.media_analysis {
                for v in &ma.video_streams {
                    output.push_str(&format!(
                        "bilbycast_edge_media_video_info{{flow_id=\"{}\",pid=\"0x{:04X}\",codec=\"{}\",resolution=\"{}\",profile=\"{}\",level=\"{}\"}} 1\n",
                        fs.flow_id,
                        v.pid,
                        v.codec,
                        v.resolution.as_deref().unwrap_or("unknown"),
                        v.profile.as_deref().unwrap_or("unknown"),
                        v.level.as_deref().unwrap_or("unknown"),
                    ));
                    if let Some(fps) = v.frame_rate {
                        output.push_str(&format!(
                            "bilbycast_edge_media_video_framerate{{flow_id=\"{}\",pid=\"0x{:04X}\"}} {:.2}\n",
                            fs.flow_id, v.pid, fps
                        ));
                    }
                    if v.bitrate_bps > 0 {
                        output.push_str(&format!(
                            "bilbycast_edge_media_pid_bitrate_bps{{flow_id=\"{}\",pid=\"0x{:04X}\",type=\"video\"}} {}\n",
                            fs.flow_id, v.pid, v.bitrate_bps
                        ));
                    }
                }
                for a in &ma.audio_streams {
                    output.push_str(&format!(
                        "bilbycast_edge_media_audio_info{{flow_id=\"{}\",pid=\"0x{:04X}\",codec=\"{}\",sample_rate=\"{}\",channels=\"{}\"{}}} 1\n",
                        fs.flow_id,
                        a.pid,
                        a.codec,
                        a.sample_rate_hz.map(|r| r.to_string()).unwrap_or_else(|| "unknown".to_string()),
                        a.channels.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string()),
                        a.language.as_ref().map(|l| format!(",language=\"{}\"", l)).unwrap_or_default(),
                    ));
                    if a.bitrate_bps > 0 {
                        output.push_str(&format!(
                            "bilbycast_edge_media_pid_bitrate_bps{{flow_id=\"{}\",pid=\"0x{:04X}\",type=\"audio\"}} {}\n",
                            fs.flow_id, a.pid, a.bitrate_bps
                        ));
                    }
                }
                if ma.total_bitrate_bps > 0 {
                    output.push_str(&format!(
                        "bilbycast_edge_media_total_bitrate_bps{{flow_id=\"{}\"}} {}\n",
                        fs.flow_id, ma.total_bitrate_bps
                    ));
                }
            }
        }

        output.push_str("\n# HELP bilbycast_edge_srt_loss_total SRT total packet loss\n");
        output.push_str("# TYPE bilbycast_edge_srt_loss_total counter\n");
        for fs in &flow_snapshots {
            if let Some(ref srt) = fs.input.srt_stats {
                output.push_str(&format!(
                    "bilbycast_edge_srt_loss_total{{flow_id=\"{}\",leg=\"input\"}} {}\n",
                    fs.flow_id, srt.pkt_loss_total
                ));
            }
            for os in &fs.outputs {
                if let Some(ref srt) = os.srt_stats {
                    output.push_str(&format!(
                        "bilbycast_edge_srt_loss_total{{flow_id=\"{}\",output_id=\"{}\",leg=\"leg1\"}} {}\n",
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
