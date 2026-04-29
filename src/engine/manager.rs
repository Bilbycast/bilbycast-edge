// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Result, bail};
use dashmap::DashMap;

use crate::config::models::{OutputConfig, ResolvedFlow, ResourceLimitAction};
use crate::manager::events::{Event, EventSender, EventSeverity, category};
use crate::stats::collector::StatsCollector;

use super::flow::FlowRuntime;
use super::resource_monitor::SystemResourceState;

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
    /// System resource state — read for flow gating when resources are critical.
    resource_state: Arc<SystemResourceState>,
    /// Action to take when resources are critical. `None` or `Alarm` = no gating.
    resource_action: Option<ResourceLimitAction>,
}

impl FlowManager {
    /// Create a new `FlowManager` with no active flows.
    ///
    /// The provided [`StatsCollector`] is shared with every flow that is
    /// subsequently created, allowing the stats subsystem to aggregate
    /// metrics across all flows.
    pub fn new(
        stats: Arc<StatsCollector>,
        ffmpeg_available: bool,
        event_sender: EventSender,
        resource_state: Arc<SystemResourceState>,
        resource_action: Option<ResourceLimitAction>,
    ) -> Self {
        Self {
            flows: DashMap::new(),
            stats,
            ffmpeg_available,
            event_sender,
            resource_state,
            resource_action,
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

    /// Sum the per-flow `cost_units` budget consumption across every
    /// running flow. Each `FlowRuntime` carries the units it pays at
    /// start time (computed via `engine::hardware_probe::compute_flow_cost_units`).
    /// Surfaced on `HealthPayload.resource_budget.units_used` so the
    /// manager UI can render `units_used / units_total` percentage.
    pub fn total_cost_units(&self) -> u32 {
        self.flows.iter().map(|r| r.value().cost_units()).sum()
    }

    /// Check if a flow is currently running
    pub fn is_running(&self, flow_id: &str) -> bool {
        self.flows.contains_key(flow_id)
    }

    /// Look up a running flow's [`FlowRuntime`] by id.
    ///
    /// Returns `None` if the flow isn't currently active. Callers get
    /// an `Arc<FlowRuntime>` clone so they can read runtime handles
    /// (e.g. the PID-bus assembler's `plan_tx` for `update_flow_assembly`)
    /// without holding the DashMap shard lock.
    pub fn get_runtime(&self, flow_id: &str) -> Option<Arc<FlowRuntime>> {
        self.flows.get(flow_id).map(|r| r.value().clone())
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
    pub async fn create_flow(&self, config: ResolvedFlow) -> Result<Arc<FlowRuntime>> {
        // Gate flow creation when system resources are critical
        if self.resource_state.resources_critical.load(Ordering::Relaxed) {
            if matches!(self.resource_action, Some(ResourceLimitAction::GateFlows)) {
                self.event_sender.emit_flow_with_details(
                    EventSeverity::Warning,
                    category::SYSTEM_RESOURCES,
                    format!("Flow '{}' creation blocked: system resources critical", config.config.id),
                    &config.config.id,
                    serde_json::json!({"flow_id": config.config.id}),
                );
                bail!(
                    "Cannot start flow '{}': system resources critical (CPU or RAM threshold exceeded)",
                    config.config.id
                );
            }
        }

        if self.flows.contains_key(&config.config.id) {
            bail!("Flow '{}' is already running", config.config.id);
        }

        let flow_id = config.config.id.clone();
        match FlowRuntime::start(config.clone(), &self.stats, self.ffmpeg_available, self.event_sender.clone()).await {
            Ok(runtime) => {
                let runtime = Arc::new(runtime);
                self.flows.insert(flow_id.clone(), runtime.clone());
                self.event_sender.emit_flow(
                    EventSeverity::Info,
                    category::FLOW,
                    format!("Flow '{}' started", flow_id),
                    &flow_id,
                );
                Ok(runtime)
            }
            Err(e) => {
                self.event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::FLOW,
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
                category::FLOW,
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
                    category::FLOW,
                    format!("Output '{output_id}' added to flow '{flow_id}'"),
                    flow_id,
                );
                Ok(())
            }
            Err(e) => {
                self.event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::FLOW,
                    format!("Output '{output_id}' failed to start on flow '{flow_id}': {e}"),
                    flow_id,
                );
                Err(e)
            }
        }
    }

    /// Return the IDs of every output currently running on a flow, or `None`
    /// if the flow itself is not running.
    ///
    /// Reads from `FlowRuntime.output_handles` — this reflects **live runtime
    /// state**, not the config. `update_flow` / `update_config` reconciliation
    /// uses this as the authoritative "what's currently attached" set so
    /// orphan outputs (tasks that survived a previous teardown but are no
    /// longer referenced by the config) get torn down on the next update.
    pub async fn running_output_ids(&self, flow_id: &str) -> Option<Vec<String>> {
        let runtime = self.flows.get(flow_id)?;
        let handles = runtime.output_handles.read().await;
        Some(handles.keys().cloned().collect())
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
            category::FLOW,
            format!("Output '{output_id}' removed from flow '{flow_id}'"),
            flow_id,
        );
        Ok(())
    }

    /// Atomically switch the active input of a running flow.
    ///
    /// All inputs are already running (warm passive); the switch only
    /// updates the active-input watch channel, so no task restart happens
    /// and outputs see no discontinuity.
    ///
    /// # Errors
    ///
    /// Returns an error if the flow is not running or `new_input_id` is not
    /// a member of the flow.
    pub async fn switch_active_input(&self, flow_id: &str, new_input_id: &str) -> Result<()> {
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        let previous_input_id = runtime.active_input_tx.borrow().clone();
        runtime.switch_active_input(new_input_id).await?;
        self.event_sender.send(Event {
            severity: EventSeverity::Info,
            category: category::FLOW.to_string(),
            message: format!("Flow '{flow_id}': active input switched to '{new_input_id}'"),
            details: Some(serde_json::json!({
                "previous_input_id": previous_input_id,
            })),
            flow_id: Some(flow_id.to_string()),
            input_id: Some(new_input_id.to_string()),
            output_id: None,
        });
        Ok(())
    }

    /// Toggle an output active or passive on a running flow.
    ///
    /// Setting an output active spawns a fresh output task if one is not
    /// already running; setting it passive cancels and removes the running
    /// task while leaving the output config in place for a later re-activation.
    pub async fn set_output_active(
        &self,
        flow_id: &str,
        output_id: &str,
        active: bool,
    ) -> Result<()> {
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        runtime
            .set_output_active(output_id, active, &runtime.stats)
            .await?;
        self.event_sender.send(Event {
            severity: EventSeverity::Info,
            category: category::FLOW.to_string(),
            message: format!(
                "Flow '{flow_id}': output '{output_id}' set {}",
                if active { "active" } else { "passive" }
            ),
            details: Some(serde_json::json!({
                "active": active,
            })),
            flow_id: Some(flow_id.to_string()),
            input_id: None,
            output_id: Some(output_id.to_string()),
        });
        Ok(())
    }

    /// Atomically start every member flow of a flow group.
    ///
    /// Spawns each member's `FlowRuntime::start` in parallel via
    /// [`futures_util::future::join_all`]. If **any** member fails to start,
    /// every member that did successfully start is rolled back via
    /// `flow.stop()` and an `Err` is returned. This gives operators an
    /// all-or-nothing guarantee for groups that share a PTP domain or that
    /// must activate as a unit (audio + ANC, dual-feed talkback, etc.).
    ///
    /// **Note on barrier semantics:** "atomic" here means the *futures*
    /// complete in the same scheduler tick — i.e., the members start within
    /// roughly one tokio task wakeup of each other (typically << 1 ms on a
    /// quiet runtime). This is sufficient for IS-05 immediate activation; a
    /// stricter time-aligned start (using a `tokio::sync::Barrier` to gate
    /// the input task spawn until all members are bound) is a follow-up.
    pub async fn start_flow_group(
        &self,
        group_id: &str,
        members: Vec<ResolvedFlow>,
    ) -> Result<()> {
        if members.is_empty() {
            bail!("Flow group '{}' has no members to start", group_id);
        }
        // Avoid double-starting members that are already running.
        let already_running: Vec<&ResolvedFlow> = members
            .iter()
            .filter(|f| self.flows.contains_key(&f.config.id))
            .collect();
        if !already_running.is_empty() {
            bail!(
                "Cannot start flow group '{}': member flow(s) already running: {}",
                group_id,
                already_running
                    .iter()
                    .map(|f| f.config.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // Spawn all member starts in parallel.
        let starts = futures_util::future::join_all(
            members
                .iter()
                .cloned()
                .map(|cfg| async move { (cfg.config.id.clone(), self.create_flow(cfg).await) }),
        )
        .await;

        // Check for any failures.
        let mut started_ids: Vec<String> = Vec::new();
        let mut first_error: Option<(String, anyhow::Error)> = None;
        for (id, res) in starts {
            match res {
                Ok(_) => started_ids.push(id),
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some((id, e));
                    }
                }
            }
        }

        if let Some((failed_id, err)) = first_error {
            // Roll back: stop every member that DID start.
            tracing::error!(
                "Flow group '{}' start failed at member '{failed_id}': {err}; rolling back {} started members",
                group_id,
                started_ids.len()
            );
            for id in &started_ids {
                let _ = self.destroy_flow(id).await;
            }
            self.event_sender.emit(
                EventSeverity::Critical,
                category::FLOW_GROUP,
                format!("Flow group '{group_id}' start rolled back: member '{failed_id}' failed: {err}"),
            );
            bail!(
                "Flow group '{group_id}' start failed at member '{failed_id}': {err} (rolled back)"
            );
        }

        self.event_sender.emit(
            EventSeverity::Info,
            category::FLOW_GROUP,
            format!(
                "Flow group '{group_id}' started ({} members)",
                started_ids.len()
            ),
        );
        Ok(())
    }

    /// Stop every member flow of a flow group in parallel. Best-effort:
    /// individual member stop failures are logged but do not abort the
    /// remaining stops. The group is considered stopped once all member
    /// runtimes have exited.
    pub async fn stop_flow_group(&self, group_id: &str, member_ids: &[String]) -> Result<()> {
        if member_ids.is_empty() {
            return Ok(());
        }
        let stops = futures_util::future::join_all(
            member_ids
                .iter()
                .map(|id| async move { (id.clone(), self.destroy_flow(id).await) }),
        )
        .await;
        let mut failures: Vec<String> = Vec::new();
        for (id, res) in stops {
            if let Err(e) = res {
                tracing::warn!("Flow group '{group_id}' stop_flow_group: member '{id}' stop error: {e}");
                failures.push(id);
            }
        }
        self.event_sender.emit(
            EventSeverity::Info,
            category::FLOW_GROUP,
            format!(
                "Flow group '{group_id}' stopped ({} members; {} failures)",
                member_ids.len() - failures.len(),
                failures.len()
            ),
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
