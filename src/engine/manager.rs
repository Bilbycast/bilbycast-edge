// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

use std::sync::Arc;

use anyhow::{Result, bail};
use dashmap::DashMap;

use crate::config::models::{FlowConfig, OutputConfig};
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
}

impl FlowManager {
    /// Create a new `FlowManager` with no active flows.
    ///
    /// The provided [`StatsCollector`] is shared with every flow that is
    /// subsequently created, allowing the stats subsystem to aggregate
    /// metrics across all flows.
    pub fn new(stats: Arc<StatsCollector>) -> Self {
        Self {
            flows: DashMap::new(),
            stats,
        }
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

        let runtime = FlowRuntime::start(config.clone(), &self.stats).await?;
        let runtime = Arc::new(runtime);
        self.flows.insert(config.id.clone(), runtime.clone());
        Ok(runtime)
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
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        runtime.add_output(output, &runtime.stats).await
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
        runtime.remove_output(output_id).await
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
