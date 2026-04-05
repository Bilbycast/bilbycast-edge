// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;

use anyhow::{Result, bail};
use dashmap::DashMap;

use crate::config::models::{FlowConfig, OutputConfig};
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::StatsCollector;

use super::flow::FlowRuntime;

/// Central coordinator for the lifecycle of all media flows in the system.
///
/// `FlowManager` owns a concurrent map of [`FlowRuntime`] instances keyed by
/// their unique flow ID. It is the single entry point that the REST API layer
/// calls to create, stop, inspect, or mutate running flows.
///
/// Internally, each flow is reference-counted (`Arc<FlowRuntime>`) so that
/// the manager can hand out references without holding a lock across async
/// boundaries. The underlying [`DashMap`] provides lock-free concurrent reads
/// and fine-grained per-shard write locks, making it safe to call methods from
/// multiple Tokio tasks simultaneously (e.g., concurrent REST requests).
///
/// # Thread Safety
///
/// All public methods are `&self` and can be called concurrently. Flow
/// creation and destruction are serialized per flow ID by the `DashMap` shard
/// lock, so duplicate flow IDs are detected atomically.
pub struct FlowManager {
    /// Active flow runtimes, keyed by flow_id.
    /// Uses `DashMap` for lock-free concurrent reads and fine-grained write locks.
    flows: DashMap<String, Arc<FlowRuntime>>,
    /// Global stats collector shared across all flows; each flow registers
    /// itself here on creation and unregisters on destruction.
    stats: Arc<StatsCollector>,
    /// Whether ffmpeg is available on this device (detected at startup).
    /// Used to gate optional thumbnail generation per flow.
    ffmpeg_available: bool,
    /// Event sender for forwarding operational events to the manager.
    event_sender: EventSender,
}

impl FlowManager {
    /// Create a new `FlowManager` with no active flows.
    ///
    /// The provided [`StatsCollector`] is shared with every flow that is
    /// subsequently created, allowing the stats subsystem to aggregate
    /// metrics across all flows.
    pub fn new(stats: Arc<StatsCollector>, ffmpeg_available: bool, event_sender: EventSender) -> Self {
        Self {
            flows: DashMap::new(),
            stats,
            ffmpeg_available,
            event_sender,
        }
    }

    /// Get a clone of the event sender for passing to sub-components.
    /// Retained for use by future sub-components that need to emit events.
    #[allow(dead_code)]
    pub fn event_sender(&self) -> &EventSender {
        &self.event_sender
    }

    /// Whether ffmpeg is available on this device.
    /// Retained as public accessor for thumbnail feature gating.
    #[allow(dead_code)]
    pub fn ffmpeg_available(&self) -> bool {
        self.ffmpeg_available
    }

    /// Return the number of flows that are currently active (running).
    ///
    /// This is a snapshot value; flows may start or stop concurrently.
    pub fn active_flow_count(&self) -> usize {
        self.flows.len()
    }

    /// Check if a flow is currently running
    pub fn is_running(&self, flow_id: &str) -> bool {
        self.flows.contains_key(flow_id)
    }

    /// Create and start a new media flow from the given configuration.
    ///
    /// This performs the full bring-up sequence:
    /// 1. Validates that no flow with the same ID is already running.
    /// 2. Delegates to [`FlowRuntime::start`] which spawns the input task,
    ///    creates the broadcast channel, and spawns all output tasks.
    /// 3. Inserts the resulting `FlowRuntime` into the active flows map.
    ///
    /// # Errors
    ///
    /// Returns an error if a flow with the same `config.id` is already running
    /// or if the underlying input/output tasks fail to initialize (e.g., socket
    /// bind failure, SRT connection error).
    pub async fn create_flow(&self, config: FlowConfig) -> Result<Arc<FlowRuntime>> {
        if self.flows.contains_key(&config.id) {
            bail!("Flow '{}' is already running", config.id);
        }

        let flow_id = config.id.clone();
        match FlowRuntime::start(config.clone(), &self.stats, self.ffmpeg_available, self.event_sender.clone()).await {
            Ok(runtime) => {
                let runtime = Arc::new(runtime);
                self.flows.insert(flow_id.clone(), runtime.clone());
                self.event_sender.emit_flow(
                    EventSeverity::Info,
                    "flow",
                    format!("Flow '{}' started", flow_id),
                    &flow_id,
                );
                Ok(runtime)
            }
            Err(e) => {
                self.event_sender.emit_flow(
                    EventSeverity::Critical,
                    "flow",
                    format!("Flow '{}' failed to start: {e}", flow_id),
                    &flow_id,
                );
                Err(e)
            }
        }
    }

    /// Stop and remove a running flow by its ID.
    ///
    /// Cancels the flow's cancellation token (which stops input and all
    /// output tasks), removes the flow from the active map, and unregisters
    /// its stats from the global collector.
    ///
    /// # Errors
    ///
    /// Returns an error if no flow with the given ID is currently running.
    pub async fn destroy_flow(&self, flow_id: &str) -> Result<()> {
        if let Some((_, runtime)) = self.flows.remove(flow_id) {
            runtime.stop().await;
            self.stats.unregister_flow(flow_id);
            self.event_sender.emit_flow(
                EventSeverity::Info,
                "flow",
                format!("Flow '{flow_id}' stopped"),
                flow_id,
            );
            Ok(())
        } else {
            bail!("Flow '{}' is not running", flow_id);
        }
    }

    /// Stop a running flow. This is an alias for [`destroy_flow`](Self::destroy_flow)
    /// and fully tears down the flow (cancels tasks, removes from map, unregisters stats).
    ///
    /// # Errors
    ///
    /// Returns an error if the flow is not currently running.
    pub async fn stop_flow(&self, flow_id: &str) -> Result<()> {
        self.destroy_flow(flow_id).await
    }

    /// Hot-add a new output to an already-running flow.
    ///
    /// The output subscribes to the flow's existing broadcast channel and
    /// begins receiving packets immediately. This enables adding destinations
    /// at runtime without restarting the flow or interrupting other outputs.
    ///
    /// # Errors
    ///
    /// Returns an error if the flow is not running or if the output fails to
    /// start (e.g., socket bind failure).
    pub async fn add_output(&self, flow_id: &str, output: OutputConfig) -> Result<()> {
        let output_id = output.id().to_string();
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        match runtime.add_output(output, &runtime.stats).await {
            Ok(()) => {
                self.event_sender.emit_flow(
                    EventSeverity::Info,
                    "flow",
                    format!("Output '{output_id}' added to flow '{flow_id}'"),
                    flow_id,
                );
                Ok(())
            }
            Err(e) => {
                self.event_sender.emit_flow(
                    EventSeverity::Warning,
                    "flow",
                    format!("Output '{output_id}' failed to start on flow '{flow_id}': {e}"),
                    flow_id,
                );
                Err(e)
            }
        }
    }

    /// Remove a single output from a running flow without affecting other outputs.
    ///
    /// Cancels the output's child cancellation token, which causes its task
    /// to exit, and unregisters the output's stats from the flow accumulator.
    ///
    /// # Errors
    ///
    /// Returns an error if the flow or output is not found.
    pub async fn remove_output(&self, flow_id: &str, output_id: &str) -> Result<()> {
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        runtime.remove_output(output_id).await?;
        self.event_sender.emit_flow(
            EventSeverity::Info,
            "flow",
            format!("Output '{output_id}' removed from flow '{flow_id}'"),
            flow_id,
        );
        Ok(())
    }

    /// Gracefully stop all running flows, typically called during application shutdown.
    ///
    /// Iterates over every active flow, cancels its token, and removes it from
    /// the map. Each flow's input and output tasks will observe the cancellation
    /// and exit. This method does not return until every flow has been signalled.
    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.flows.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            if let Some((_, runtime)) = self.flows.remove(&id) {
                runtime.stop().await;
            }
        }
        tracing::info!("All flows stopped");
    }

    /// Return a reference to the global [`StatsCollector`].
    ///
    /// Callers (e.g., the REST API stats endpoint) use this to read aggregated
    /// metrics for all flows without needing direct access to individual
    /// `FlowRuntime` instances.
    pub fn stats(&self) -> &Arc<StatsCollector> {
        &self.stats
    }
}
