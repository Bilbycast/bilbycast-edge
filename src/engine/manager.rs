// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use dashmap::DashMap;

use crate::config::models::{OutputConfig, ResolvedFlow, ResourceLimitAction};
use crate::manager::events::{Event, EventSender, EventSeverity, category};
use crate::stats::collector::StatsCollector;

use super::flow::FlowRuntime;
use super::packet::RtpPacket;
use super::resource_monitor::SystemResourceState;
use super::ts_es_bus::NodeEsBus;
use super::ts_psi_catalog::PsiCatalogStore;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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
    /// Static hardware capabilities — read for HW-encoder oversubscription
    /// checks at flow start. `None` on builds where the probe didn't run
    /// (legacy startup paths / tests). Soft warning only, never blocks
    /// flow creation.
    static_caps: Option<Arc<crate::engine::hardware_probe::StaticCapabilities>>,
    /// Per-edge runtime registry that tracks which `(device, audio_device)`
    /// physical display output is currently held. When two flows target
    /// the same pair, the second parks here until the holder releases.
    /// Only built into the FlowManager on hosts where the `display`
    /// feature is enabled (Linux + the `display` Cargo feature).
    #[cfg(all(feature = "display", target_os = "linux"))]
    display_claim_registry: Arc<crate::display::claim_registry::DisplayClaimRegistry>,
    /// WebRTC session registry. Held so flow lifecycle (destroy + WHIP
    /// hot-remove) can clean up the per-flow signalling channels +
    /// session sessions. `None` on builds without the `webrtc` feature.
    /// Optional even with the feature on because the registry is
    /// constructed at the HTTP server layer; not all entry points wire
    /// it through (e.g. `tests` and `examples` that exercise the engine
    /// in isolation).
    #[cfg(feature = "webrtc")]
    webrtc_sessions:
        Option<Arc<crate::api::webrtc::registry::WebrtcSessionRegistry>>,
    /// Node-wide elementary-stream bus. Shared `Arc` across every
    /// assembled flow on the edge — passthrough flows never touch it.
    /// Channels are keyed by `(input_id, source_pid)`; input IDs are
    /// globally unique on the node so the key shape already covers the
    /// node-wide scope. Owned here so that — once assignment uniqueness
    /// is lifted in Phase 2 of the PES Switch redesign — multiple flows
    /// can subscribe to the same input's ES channels without spawning
    /// duplicate demuxers.
    node_es_bus: Arc<NodeEsBus>,
    /// Per-input node-wide broadcast publishers (PES Switch Phase 2.1).
    /// One entry per running input on the node, keyed by `input_id`
    /// (globally unique). The owning flow's input task publishes onto
    /// the [`NodeInputPublisher::sender`]; any consumer on the node —
    /// the flow's own forwarder, a sibling flow's assembly slot, or a
    /// shared `TsEsDemuxer` — subscribes via [`Self::subscribe_input`].
    ///
    /// Lifecycle: registered by `FlowRuntime::start` for every input
    /// it owns; unregistered on flow stop. Cross-flow references go
    /// inactive when the owning flow stops (subscribers see
    /// `RecvError::Closed` on the next `recv()`) — graceful degrade
    /// that mirrors the operator-facing "this input is offline"
    /// state, not a panic.
    input_publishers: DashMap<String, Arc<NodeInputPublisher>>,
    /// Refcounted per-input `TsEsDemuxer` registry (PES Switch
    /// Phase 2.1). When a flow's assembly references `input_id`, it
    /// calls [`Self::acquire_demuxer`] which returns a
    /// [`DemuxerHandle`]: spawns the demuxer task on first reference,
    /// increments refcount on subsequent references. Drop on
    /// `DemuxerHandle` decrements; the task exits when the count
    /// reaches zero. This is the single point that prevents the
    /// duplicate-demuxer / duplicate-bus-publish bug when two flows
    /// both reference the same input on the node.
    demuxers: DashMap<String, Arc<DemuxerSlot>>,
}

/// One node-wide publisher entry for a running input. Held by every
/// consumer that wants this input's bytes via [`FlowManager::subscribe_input`].
pub struct NodeInputPublisher {
    /// Stable per-input handle so consumers can `.subscribe()` for a
    /// fresh `broadcast::Receiver`. Cloned freely; the underlying
    /// channel is bounded so a slow consumer drops, never blocks.
    pub sender: broadcast::Sender<RtpPacket>,
    /// Per-input PSI (PAT/PMT) catalogue store. Created here so the
    /// catalogue can be reached node-wide via [`FlowManager::psi_catalog_for_input`]
    /// for cross-flow Essence-slot resolution (PES Switch Phase 2.2b).
    /// The owning flow's PSI observer task writes to this store; both
    /// the owning flow's [`crate::stats::collector::PerInputCounters`]
    /// and any foreign flow's assembler resolver hold an `Arc` clone
    /// and read the same snapshot. Non-TS inputs spawn no observer
    /// (gated on `InputConfig::is_ts_carrier`); the store stays empty.
    pub psi_catalog: Arc<PsiCatalogStore>,
}

/// One node-wide refcounted `TsEsDemuxer` task. The owning
/// [`FlowManager`] keeps an `Arc<DemuxerSlot>` in [`FlowManager::demuxers`];
/// each [`DemuxerHandle`] handed to a flow holds its own clone of the
/// `Arc`, and `Drop::drop` on the handle decrements `refcount`. When
/// the count hits zero the slot's `cancel` token is fired and the
/// demuxer task wakes up + exits.
pub struct DemuxerSlot {
    /// Demuxer task handle. Kept for completeness; in production the
    /// task watches `cancel` rather than being explicitly joined.
    #[allow(dead_code)]
    join: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Cancellation token. Fired when `refcount` reaches zero so the
    /// demuxer task exits cleanly without leaking subscribers.
    cancel: CancellationToken,
    /// Live subscriber count. Atomically incremented when a
    /// [`DemuxerHandle`] is created; decremented in `Drop::drop`.
    /// When this transitions from `1` to `0`, the slot fires
    /// `cancel` to terminate the underlying task. Tracked
    /// independently of `Arc::strong_count` because the slot itself
    /// is kept alive in the `FlowManager::demuxers` map while any
    /// consumer holds a handle.
    refcount: AtomicUsize,
    /// Input ID this demuxer is fed by. Persisted on the slot for
    /// the cleanup path on `release_demuxer`.
    input_id: String,
}

/// RAII handle returned by [`FlowManager::acquire_demuxer`]. Dropping
/// it decrements the slot's refcount; when the last handle drops, the
/// underlying demuxer task is cancelled and removed from
/// [`FlowManager::demuxers`].
pub struct DemuxerHandle {
    slot: Arc<DemuxerSlot>,
    manager: std::sync::Weak<FlowManager>,
}

impl Drop for DemuxerHandle {
    fn drop(&mut self) {
        // `fetch_sub` returns the previous value — `1` means we just
        // decremented to zero and own the cleanup.
        let prev = self.slot.refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.slot.cancel.cancel();
            if let Some(manager) = self.manager.upgrade() {
                // Remove the slot from the map so a subsequent
                // `acquire_demuxer` for the same input spawns a fresh
                // demuxer rather than reusing a cancelled one.
                manager.demuxers.remove(&self.slot.input_id);
            }
        }
    }
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
        static_caps: Option<Arc<crate::engine::hardware_probe::StaticCapabilities>>,
        #[cfg(all(feature = "display", target_os = "linux"))]
        display_claim_registry: Arc<crate::display::claim_registry::DisplayClaimRegistry>,
        #[cfg(feature = "webrtc")]
        webrtc_sessions: Option<
            Arc<crate::api::webrtc::registry::WebrtcSessionRegistry>,
        >,
    ) -> Self {
        Self {
            flows: DashMap::new(),
            stats,
            ffmpeg_available,
            event_sender,
            resource_state,
            resource_action,
            static_caps,
            #[cfg(all(feature = "display", target_os = "linux"))]
            display_claim_registry,
            #[cfg(feature = "webrtc")]
            webrtc_sessions,
            node_es_bus: Arc::new(NodeEsBus::new()),
            input_publishers: DashMap::new(),
            demuxers: DashMap::new(),
        }
    }

    /// Accessor for the WebRTC session registry (if wired). Used by
    /// `FlowRuntime::remove_input` to unregister WHIP-server inputs
    /// without going through the WS-layer plumbing.
    #[cfg(feature = "webrtc")]
    pub fn webrtc_sessions(
        &self,
    ) -> Option<&Arc<crate::api::webrtc::registry::WebrtcSessionRegistry>> {
        self.webrtc_sessions.as_ref()
    }

    /// Get a clone of the node-wide elementary-stream bus handle. Used
    /// by `FlowRuntime::start` so assembled flows share one bus across
    /// the node; passthrough flows never call this. Phase 2 of the PES
    /// Switch redesign will use this accessor from non-create_flow
    /// paths (e.g. cross-flow assembly editor + node-wide demuxer
    /// registry); today only `create_flow` references the field.
    #[allow(dead_code)]
    pub fn node_es_bus(&self) -> Arc<NodeEsBus> {
        Arc::clone(&self.node_es_bus)
    }

    /// Register the broadcast publisher for an input that's about to
    /// start running on this node. Called by `FlowRuntime::start` once
    /// it has created the input's per-input broadcast channel; gives
    /// node-wide consumers (cross-flow assembly slots, the shared
    /// `TsEsDemuxer` registry, foreign-flow Essence resolvers) a way
    /// to attach without re-creating the channel.
    ///
    /// Returns the per-input [`PsiCatalogStore`] so the caller can pass
    /// the same `Arc` to its `PerInputCounters` (so the per-flow
    /// snapshot path keeps using the catalogue) and to its PSI observer
    /// task. Reusing an existing entry's catalogue lets the input
    /// publisher be re-registered without losing PSI history; in
    /// today's single-owner reality this path is taken only after a
    /// flow restart on the same input.
    ///
    /// Idempotent on the publisher slot: re-registering an `input_id`
    /// replaces the previous broadcast `Sender` so cross-flow
    /// consumers attaching afterwards see the new owner's bytes.
    #[allow(dead_code)]
    pub fn register_input_publisher(
        &self,
        input_id: &str,
        sender: broadcast::Sender<RtpPacket>,
    ) -> Arc<PsiCatalogStore> {
        // `entry` keeps it atomic against concurrent register calls for
        // the same input id (e.g. a flow stop+restart racing).
        let entry = self
            .input_publishers
            .entry(input_id.to_string())
            .and_modify(|existing| {
                let psi = Arc::clone(&existing.psi_catalog);
                *existing = Arc::new(NodeInputPublisher {
                    sender: sender.clone(),
                    psi_catalog: psi,
                });
            })
            .or_insert_with(|| {
                Arc::new(NodeInputPublisher {
                    sender: sender.clone(),
                    psi_catalog: Arc::new(PsiCatalogStore::new()),
                })
            });
        Arc::clone(&entry.value().psi_catalog)
    }

    /// Unregister an input's publisher when the owning flow stops.
    /// Subscribers see `RecvError::Closed` on the next `recv()` and
    /// reconnect (or exit, depending on their drop policy). Idempotent.
    #[allow(dead_code)]
    pub fn unregister_input_publisher(&self, input_id: &str) {
        self.input_publishers.remove(input_id);
    }

    /// Subscribe to a node-wide input's broadcast channel. Returns
    /// `None` when the input isn't running on this node — caller
    /// should treat that as "input offline" rather than a fatal error.
    #[allow(dead_code)]
    pub fn subscribe_input(&self, input_id: &str) -> Option<broadcast::Receiver<RtpPacket>> {
        self.input_publishers.get(input_id).map(|e| e.value().sender.subscribe())
    }

    /// Look up the per-input PSI catalogue for cross-flow Essence-slot
    /// resolution (PES Switch Phase 2.2b). Returns the same `Arc` the
    /// owning flow holds via its `PerInputCounters`. Returns `None`
    /// when the input isn't running on this node — caller treats that
    /// as the same "no catalogue" state as a TS-bearing input that
    /// hasn't yet emitted a PAT/PMT (resolver bails with
    /// `pid_bus_essence_no_catalogue`).
    #[allow(dead_code)]
    pub fn psi_catalog_for_input(&self, input_id: &str) -> Option<Arc<PsiCatalogStore>> {
        self.input_publishers
            .get(input_id)
            .map(|e| Arc::clone(&e.value().psi_catalog))
    }

    /// Acquire a refcounted handle to the node-wide `TsEsDemuxer` for
    /// `input_id`. The first caller spawns the demuxer task via
    /// `spawn_fn`; subsequent callers reuse it. The handle's `Drop`
    /// decrements the refcount; when the last handle drops, the
    /// demuxer task is cancelled and removed from the registry.
    ///
    /// Returns `None` when the demuxer can't be spawned — typically
    /// because the input isn't running (`spawn_fn` returns `None`).
    ///
    /// `spawn_fn` receives the slot's cancel token + the input's
    /// publisher subscriber so the demuxer task can hook up its
    /// upstream + cancellation in one shot. Today's caller is
    /// `flow.rs::finalize_spts_assembler` — the function lives here
    /// so the refcounting + lifecycle stay opaque to flow code.
    #[allow(dead_code)]
    pub fn acquire_demuxer<F>(self: &Arc<Self>, input_id: &str, spawn_fn: F) -> Option<DemuxerHandle>
    where
        F: FnOnce(broadcast::Receiver<RtpPacket>, CancellationToken) -> Option<JoinHandle<()>>,
    {
        // Fast path: existing slot. Use `entry` API to avoid a
        // get-then-insert race when two flows acquire simultaneously.
        let slot = self
            .demuxers
            .entry(input_id.to_string())
            .or_try_insert_with(|| -> Result<Arc<DemuxerSlot>, ()> {
                let rx = self.subscribe_input(input_id).ok_or(())?;
                let cancel = CancellationToken::new();
                let join = spawn_fn(rx, cancel.clone()).ok_or(())?;
                Ok(Arc::new(DemuxerSlot {
                    join: tokio::sync::Mutex::new(Some(join)),
                    cancel,
                    refcount: AtomicUsize::new(0),
                    input_id: input_id.to_string(),
                }))
            })
            .ok()?
            .value()
            .clone();
        // Bump the refcount BEFORE returning the handle so concurrent
        // Drop of a sibling handle never falsely cancels the task.
        slot.refcount.fetch_add(1, Ordering::AcqRel);
        Some(DemuxerHandle {
            slot,
            manager: Arc::downgrade(self),
        })
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

    /// Sum the per-flow hardware-encoder session usage across every
    /// running flow, returning the per-family `HwSessionUsage` block
    /// the manager attaches to `HealthPayload.resource_budget`.
    /// Pairs with the static `hw_session_limits` probe so the manager
    /// UI can render `nvenc_in_use / nvenc_max_sessions` chips and
    /// alarm when usage exceeds capacity.
    pub fn total_hw_sessions(&self) -> crate::engine::hardware_probe::HwSessionUsage {
        let mut acc = crate::engine::hardware_probe::HwSessionUsage::default();
        for entry in self.flows.iter() {
            let f = entry.value().hw_session_usage();
            acc.nvenc_in_use = acc.nvenc_in_use.saturating_add(f.nvenc_in_use);
            acc.qsv_in_use = acc.qsv_in_use.saturating_add(f.qsv_in_use);
            acc.videotoolbox_in_use = acc
                .videotoolbox_in_use
                .saturating_add(f.videotoolbox_in_use);
            acc.amf_in_use = acc.amf_in_use.saturating_add(f.amf_in_use);
            acc.vaapi_in_use = acc.vaapi_in_use.saturating_add(f.vaapi_in_use);
            acc.nvenc_in_use_4k = acc.nvenc_in_use_4k.saturating_add(f.nvenc_in_use_4k);
            acc.qsv_in_use_4k = acc.qsv_in_use_4k.saturating_add(f.qsv_in_use_4k);
            acc.amf_in_use_4k = acc.amf_in_use_4k.saturating_add(f.amf_in_use_4k);
            acc.vaapi_in_use_4k = acc.vaapi_in_use_4k.saturating_add(f.vaapi_in_use_4k);
            acc.nvdec_in_use = acc.nvdec_in_use.saturating_add(f.nvdec_in_use);
            acc.qsv_decode_in_use = acc.qsv_decode_in_use.saturating_add(f.qsv_decode_in_use);
            acc.vaapi_decode_in_use = acc
                .vaapi_decode_in_use
                .saturating_add(f.vaapi_decode_in_use);
            acc.nvdec_in_use_4k = acc.nvdec_in_use_4k.saturating_add(f.nvdec_in_use_4k);
            acc.qsv_decode_in_use_4k = acc
                .qsv_decode_in_use_4k
                .saturating_add(f.qsv_decode_in_use_4k);
            acc.vaapi_decode_in_use_4k = acc
                .vaapi_decode_in_use_4k
                .saturating_add(f.vaapi_decode_in_use_4k);
        }
        acc
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

    /// Snapshot every running flow's `(flow_id, recording_id)` pair for
    /// flows that currently hold a [`crate::replay::writer::RecordingHandle`].
    /// Used by the `list_recordings` dispatcher to decorate orphan-vs-armed
    /// rows and by `delete_recording` to refuse a delete on an armed
    /// recording without scanning the manager DB. The DashMap is iterated
    /// once and the result is materialised before returning, so callers
    /// don't pin shard locks while walking the disk.
    #[cfg(feature = "replay")]
    pub fn flows_with_recording(&self) -> Vec<(String, String)> {
        self.flows
            .iter()
            .filter_map(|r| {
                let flow_id = r.key().clone();
                r.value()
                    .recording_handle
                    .as_ref()
                    .map(|h| (flow_id, h.recording_id.clone()))
            })
            .collect()
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
    pub async fn create_flow(self: &Arc<Self>, config: ResolvedFlow) -> Result<Arc<FlowRuntime>> {
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
        match FlowRuntime::start(
            config.clone(),
            Arc::clone(&self.stats),
            self.ffmpeg_available,
            self.event_sender.clone(),
            #[cfg(all(feature = "display", target_os = "linux"))]
            Arc::clone(&self.display_claim_registry),
            Arc::clone(&self.node_es_bus),
            Arc::clone(self),
        ).await {
            Ok(runtime) => {
                let runtime = Arc::new(runtime);
                self.flows.insert(flow_id.clone(), runtime.clone());
                self.event_sender.emit_flow(
                    EventSeverity::Info,
                    category::FLOW,
                    format!("Flow '{}' started", flow_id),
                    &flow_id,
                );
                // HW-encoder oversubscribe check — soft warning only.
                // Runs after the flow joins `self.flows` so
                // `total_hw_sessions()` includes its contribution. We
                // never block the create on this; the alarm just
                // tells the operator to re-plan their HW assignments.
                self.emit_hw_oversubscribe_warnings(&flow_id);
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

    /// Compare the live per-family HW encoder session usage against the
    /// startup-probed limits, emitting one Warning event per family
    /// that exceeds capacity. Each event carries
    /// `error_code: hw_encoder_oversubscribed` plus structured
    /// `details` (family / in_use / max_sessions) so the manager UI
    /// can highlight the right family without parsing free-text.
    /// No-op when no static capabilities snapshot was attached or when
    /// no family was probed.
    fn emit_hw_oversubscribe_warnings(&self, flow_id: &str) {
        let Some(caps) = self.static_caps.as_ref() else {
            return;
        };
        let usage = self.total_hw_sessions();

        // Encoder side — H.264 / HEVC transcodes on flow outputs. Same
        // shape as before; `error_code: hw_encoder_oversubscribed`.
        let enc = &caps.hw_encoder_session_limits;
        if !enc.is_empty() {
            let checks: [(Option<u32>, u32, &'static str); 4] = [
                (enc.nvenc_max_sessions, usage.nvenc_in_use, "nvenc"),
                (enc.qsv_max_sessions, usage.qsv_in_use, "qsv"),
                (enc.amf_max_sessions, usage.amf_in_use, "amf"),
                (enc.vaapi_max_sessions, usage.vaapi_in_use, "vaapi"),
            ];
            for (max, in_use, family) in checks {
                if let Some(max) = max {
                    if in_use > max {
                        let msg = format!(
                            "Flow '{flow_id}' caused {family} encoder oversubscription: \
                            {in_use} sessions in use, {max} probed at startup. \
                            Reduce HW transcodes or restart on a host with more capacity."
                        );
                        self.event_sender.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::SYSTEM_RESOURCES,
                            msg,
                            flow_id,
                            serde_json::json!({
                                "error_code": "hw_encoder_oversubscribed",
                                "family": family,
                                "role": "encoder",
                                "in_use": in_use,
                                "max_sessions": max,
                                "flow_id": flow_id,
                            }),
                        );
                    }
                }
            }
        }

        // Decoder side — currently only HW-decoded `display` outputs
        // pay against these limits. Same Warning-level surface so the
        // manager can render a single chip with `family + role`
        // discriminators.
        let dec = &caps.hw_decoder_session_limits;
        if !dec.is_empty() {
            let dec_checks: [(Option<u32>, u32, &'static str); 3] = [
                (dec.nvdec_max_sessions, usage.nvdec_in_use, "nvdec"),
                (dec.qsv_max_sessions, usage.qsv_decode_in_use, "qsv"),
                (dec.vaapi_max_sessions, usage.vaapi_decode_in_use, "vaapi"),
            ];
            for (max, in_use, family) in dec_checks {
                if let Some(max) = max {
                    if in_use > max {
                        let msg = format!(
                            "Flow '{flow_id}' caused {family} decoder oversubscription: \
                            {in_use} sessions in use, {max} probed at startup. \
                            Drop a HW-decoded display output or restart on a host with more capacity."
                        );
                        self.event_sender.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::SYSTEM_RESOURCES,
                            msg,
                            flow_id,
                            serde_json::json!({
                                "error_code": "hw_decoder_oversubscribed",
                                "family": family,
                                "role": "decoder",
                                "in_use": in_use,
                                "max_sessions": max,
                                "flow_id": flow_id,
                            }),
                        );
                    }
                }
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
            // Clear the flow's WebRTC session entries (WHIP input channel,
            // WHEP output channel, bearer tokens, in-flight sessions). Long-
            // standing TODO in `webrtc.rs::unregister_flow` — without this,
            // a browser pairing against the now-dead flow id hits a closed
            // `mpsc::Sender` instead of a clean "no WHIP for this flow".
            #[cfg(feature = "webrtc")]
            if let Some(reg) = self.webrtc_sessions.as_ref() {
                reg.unregister_flow(flow_id);
            }
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

    /// Return the IDs of every input currently attached to a running flow,
    /// or `None` if the flow itself is not running.
    ///
    /// Reads from `FlowRuntime.input_handles` — this reflects **live runtime
    /// state**, not the config. The future `update_flow` / `update_config`
    /// input-set diff uses this as the authoritative "what's currently
    /// attached" set so a hot-add / hot-remove dispatch correctly identifies
    /// the deltas against the running flow.
    #[allow(dead_code)] // Phase 1 wires callers via the update_flow / update_config diff.
    pub async fn running_input_ids(&self, flow_id: &str) -> Option<Vec<String>> {
        let runtime = self.flows.get(flow_id)?;
        let handles = runtime.input_handles.read().await;
        Some(handles.keys().cloned().collect())
    }

    /// Return the set of input ids that appear inside any `Hitless` slot
    /// (primary or backup) of the flow's current assembly plan, or
    /// `None` if the flow is not running.
    ///
    /// Used by the Phase 2 `update_flow` / `update_config` diff to decide
    /// whether an input-set change would alter a running hitless ES
    /// merger's leg list. The merger isn't designed to grow/shrink legs
    /// mid-flight, so such edits trigger a full flow restart instead of
    /// going through the surgical `add_input` / `remove_input` paths.
    pub async fn flow_hitless_inputs(
        &self,
        flow_id: &str,
    ) -> Option<std::collections::HashSet<String>> {
        let runtime = self.flows.get(flow_id)?;
        Some(runtime.assembly_hitless_inputs().await)
    }

    /// Add a single input to a running flow without disturbing other
    /// inputs or any outputs (hot-add).
    ///
    /// Thin wrapper over [`FlowRuntime::add_input`]. Looks up the running
    /// flow, delegates the spawn, emits an Info event on success, and
    /// propagates the structured `error_code` on failure via the
    /// `InputMembershipError` payload attached to the returned
    /// `anyhow::Error` — WS handlers downcast to recover the code.
    ///
    /// # Errors
    ///
    /// - `Flow '...' is not running` if the flow id is unknown.
    /// - The runtime's `InputMembershipError` variants
    ///   (`input_already_member`, `hitless_leg_change_requires_restart`)
    ///   propagated verbatim.
    /// - Asynchronous bind failures (`port_conflict`, `bind_failed`) do
    ///   NOT surface here; the WS handler uses
    ///   `wait_for_first_bind_failure` to detect them within
    ///   `FIRST_BIND_WAIT` and call [`Self::remove_input`] for rollback.
    pub async fn add_input(
        self: &Arc<Self>,
        flow_id: &str,
        input_def: crate::config::models::InputDefinition,
    ) -> Result<crate::engine::flow::HotAddedInputInfo> {
        let input_id = input_def.id.clone();
        // Resource-critical gate — mirrors `create_flow`. Refuses with a
        // structured `input_resource_critical` `error_code` so the manager
        // UI shows the same banner it uses for the create path. Other
        // resource modes (alarm-only) fall through.
        if self.resource_state.resources_critical.load(Ordering::Relaxed)
            && matches!(self.resource_action, Some(ResourceLimitAction::GateFlows))
        {
            self.event_sender.emit_flow_with_details(
                EventSeverity::Warning,
                category::SYSTEM_RESOURCES,
                format!(
                    "Hot-add of input '{input_id}' to flow '{flow_id}' blocked: \
                     system resources critical"
                ),
                flow_id,
                serde_json::json!({
                    "error_code": "input_resource_critical",
                    "flow_id": flow_id,
                    "input_id": input_id,
                }),
            );
            return Err(anyhow::Error::new(
                crate::engine::flow::InputMembershipError::resource_critical(
                    flow_id, &input_id,
                ),
            ));
        }
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        match runtime.add_input(input_def, self).await {
            Ok(info) => {
                self.event_sender.emit_flow(
                    EventSeverity::Info,
                    category::FLOW,
                    format!("Input '{input_id}' added to flow '{flow_id}'"),
                    flow_id,
                );
                Ok(info)
            }
            Err(e) => {
                self.event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::FLOW,
                    format!("Input '{input_id}' failed to add to flow '{flow_id}': {e}"),
                    flow_id,
                );
                Err(e)
            }
        }
    }

    /// Remove a single input from a running flow without disturbing other
    /// inputs or outputs (hot-remove).
    ///
    /// Thin wrapper over [`FlowRuntime::remove_input`]. Cancels the
    /// input's child cancellation token, awaits its task handles, drops
    /// the entry from the runtime's live state. The fixer's per-input
    /// PSI cache is evicted via a `FixerCommand::DropInputPsi` message
    /// so a later re-add under the same id doesn't carry stale PAT/PMT.
    ///
    /// # Errors
    ///
    /// - `Flow '...' is not running` if the flow id is unknown.
    /// - The runtime's `InputMembershipError` variants
    ///   (`input_not_member`, `active_input_in_use`,
    ///   `pid_bus_input_in_use`, `hitless_leg_change_requires_restart`)
    ///   propagated verbatim.
    pub async fn remove_input(&self, flow_id: &str, input_id: &str) -> Result<()> {
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        runtime.remove_input(input_id).await?;
        self.event_sender.emit_flow(
            EventSeverity::Info,
            category::FLOW,
            format!("Input '{input_id}' removed from flow '{flow_id}'"),
            flow_id,
        );
        Ok(())
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
    /// Apply a new thumbnail capture cadence to a running flow's generators
    /// (flow-level + per-input) live, without a restart. Errors only if the
    /// flow isn't currently running.
    pub fn apply_thumbnail_interval(
        &self,
        flow_id: &str,
        new_interval_secs: Option<u32>,
    ) -> Result<()> {
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        runtime.apply_thumbnail_interval(new_interval_secs);
        Ok(())
    }

    pub async fn switch_active_input(
        &self,
        flow_id: &str,
        new_input_id: &str,
        splice_mode_override: Option<crate::config::models::SpliceMode>,
    ) -> Result<()> {
        let runtime = self
            .flows
            .get(flow_id)
            .ok_or_else(|| anyhow::anyhow!("Flow '{}' is not running", flow_id))?;
        let previous_input_id = runtime.active_input_tx.borrow().clone();
        runtime.switch_active_input(new_input_id).await?;
        // PID-bus / Flow Assembly: when the flow has a running assembler,
        // additionally signal the switch via PlanCommand. The assembler
        // walks every Switch slot whose leg list contains `new_input_id`,
        // flips that slot's active-leg pointer, bumps the owning program's
        // PMT version, and arms DI=1 on the next PCR for the slot's
        // out_pid. Slots without a matching leg are silently skipped.
        //
        // `splice_mode_override` lets the caller bypass the slot's
        // config-time `splice_mode` for *this one switch* — useful for
        // ad-hoc operator overrides without reconfiguring the flow.
        // The assembler honours the override on a per-call basis; the
        // config-time value remains the slot's default.
        //
        // Channel try_send is fine here — channel depth 16 is plenty for
        // operator-paced switches; if full, the operator can re-Take.
        if let Some(handle) = runtime.pid_bus_assembler_handle.as_ref() {
            let _ = handle.plan_tx.try_send(
                crate::engine::ts_assembler::PlanCommand::SwitchActiveInput {
                    new_input_id: new_input_id.to_string(),
                    splice_mode_override,
                },
            );
        } else if let Some(mode) = splice_mode_override {
            // Passthrough flow: there is no assembler to honour a
            // PES-aligned / forced-PMT-bump splice. The TsContinuityFixer
            // does a plain seamless cut. Surface a Warning so the operator
            // knows the per-switch override had no effect rather than
            // swallowing it silently — PES Switch is an assembled-flow
            // (SPTS / MPTS / PID-bus) feature only.
            let mode_str = match mode {
                crate::config::models::SpliceMode::PmtBump => "pmt_bump",
                crate::config::models::SpliceMode::PesAligned => "pes_aligned",
            };
            self.event_sender.send(Event {
                severity: EventSeverity::Warning,
                category: category::FLOW.to_string(),
                message: format!(
                    "Flow '{flow_id}': splice override '{mode_str}' ignored — flow is passthrough \
                     (no PID-bus assembler); used the default seamless cut"
                ),
                details: Some(serde_json::json!({
                    "error_code": "splice_override_ignored",
                    "requested_mode": mode_str,
                    "reason": "flow_not_assembled",
                    "new_input_id": new_input_id,
                })),
                flow_id: Some(flow_id.to_string()),
                input_id: Some(new_input_id.to_string()),
                output_id: None,
            });
        }
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
        self: &Arc<Self>,
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
                .map(|cfg| {
                    let this = Arc::clone(self);
                    async move { (cfg.config.id.clone(), this.create_flow(cfg).await) }
                }),
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

#[cfg(test)]
mod tests {
    //! PES Switch Phase 2.1c tests: NodeInputPublisher + refcounted
    //! DemuxerSlot infrastructure. These tests verify the lifecycle
    //! semantics in isolation — the wiring into FlowRuntime lands in
    //! Stage 2 with its own integration tests.

    use super::*;
    use crate::stats::collector::StatsCollector;
    use std::sync::atomic::Ordering;

    fn make_fm() -> Arc<FlowManager> {
        let stats = Arc::new(StatsCollector::new());
        let (event_sender, _rx) = crate::manager::events::event_channel();
        let resource_state = Arc::new(crate::engine::resource_monitor::SystemResourceState::new());
        Arc::new(FlowManager::new(
            stats,
            false,
            event_sender,
            resource_state,
            None,
            None,
            #[cfg(all(feature = "display", target_os = "linux"))]
            crate::display::claim_registry::DisplayClaimRegistry::new(),
            #[cfg(feature = "webrtc")]
            None,
        ))
    }

    #[tokio::test]
    async fn input_publisher_round_trip() {
        let fm = make_fm();
        let (tx, _rx_drop) = broadcast::channel::<RtpPacket>(8);
        fm.register_input_publisher("in-a", tx.clone());
        let mut rx = fm.subscribe_input("in-a").expect("publisher registered");
        // Publish via the cloned sender; subscriber should see it.
        let pkt = RtpPacket {
            data: bytes::Bytes::from_static(&[0u8; 8]),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
            is_raw_ts: true,
        };
        tx.send(pkt).expect("send ok");
        let got = rx.recv().await.expect("recv ok");
        assert_eq!(got.data.len(), 8);
        // Unregister, subscribe should return None now.
        fm.unregister_input_publisher("in-a");
        assert!(fm.subscribe_input("in-a").is_none());
    }

    #[tokio::test]
    async fn subscribe_input_returns_none_when_absent() {
        let fm = make_fm();
        assert!(fm.subscribe_input("ghost").is_none());
    }

    #[tokio::test]
    async fn acquire_demuxer_refcount_lifecycle() {
        let fm = make_fm();
        let (tx, _) = broadcast::channel::<RtpPacket>(8);
        fm.register_input_publisher("in-a", tx);
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let cancel_seen = Arc::new(AtomicUsize::new(0));
        let spawn_fn = |spawn_count: Arc<AtomicUsize>, cancel_seen: Arc<AtomicUsize>| {
            move |_rx: broadcast::Receiver<RtpPacket>, cancel: CancellationToken| -> Option<JoinHandle<()>> {
                spawn_count.fetch_add(1, Ordering::Relaxed);
                let seen = cancel_seen.clone();
                Some(tokio::spawn(async move {
                    cancel.cancelled().await;
                    seen.fetch_add(1, Ordering::Relaxed);
                }))
            }
        };
        // First acquire spawns; refcount = 1.
        let h1 = fm
            .acquire_demuxer("in-a", spawn_fn(spawn_count.clone(), cancel_seen.clone()))
            .expect("first acquire");
        assert_eq!(spawn_count.load(Ordering::Relaxed), 1);
        // Second acquire reuses; refcount = 2.
        let h2 = fm
            .acquire_demuxer("in-a", spawn_fn(spawn_count.clone(), cancel_seen.clone()))
            .expect("second acquire reuses");
        assert_eq!(
            spawn_count.load(Ordering::Relaxed),
            1,
            "demuxer must be spawned exactly once"
        );
        // Drop one handle; task must still be running (refcount 2 → 1).
        drop(h1);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(cancel_seen.load(Ordering::Relaxed), 0, "task lives while refcount > 0");
        // Drop the last handle; cancel should fire.
        drop(h2);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            cancel_seen.load(Ordering::Relaxed),
            1,
            "task must be cancelled when last handle drops"
        );
        // Re-acquire after cleanup → spawns a fresh demuxer.
        let _h3 = fm
            .acquire_demuxer("in-a", spawn_fn(spawn_count.clone(), cancel_seen.clone()))
            .expect("re-acquire spawns afresh");
        assert_eq!(
            spawn_count.load(Ordering::Relaxed),
            2,
            "after cleanup, next acquire spawns a new demuxer"
        );
    }

    #[tokio::test]
    async fn acquire_demuxer_returns_none_when_no_publisher() {
        let fm = make_fm();
        let result = fm.acquire_demuxer("ghost", |_rx, _cancel| {
            panic!("spawn_fn must not be called when no publisher exists");
        });
        assert!(result.is_none());
    }

    /// PES Switch Phase 2.2b: registering an input publisher returns a
    /// catalogue Arc the owning flow can pass through to its
    /// `PerInputCounters`. The same catalogue is reachable
    /// node-wide via `psi_catalog_for_input` so foreign-flow Essence
    /// resolvers can read it.
    #[tokio::test]
    async fn psi_catalog_shared_via_register_input_publisher() {
        let fm = make_fm();
        let (tx, _rx_drop) = broadcast::channel::<RtpPacket>(8);
        let cat_owner = fm.register_input_publisher("in-a", tx);
        // Foreign-side lookup returns the SAME Arc.
        let cat_foreign = fm.psi_catalog_for_input("in-a").expect("registered");
        assert!(
            Arc::ptr_eq(&cat_owner, &cat_foreign),
            "owner + foreign lookup must share the same PsiCatalogStore Arc"
        );
        // Unknown input id returns None.
        assert!(fm.psi_catalog_for_input("ghost").is_none());
    }

    /// PES Switch Phase 2.2b: re-registering the same input id (e.g.
    /// after a flow restart on the same input) preserves the catalogue
    /// Arc so any foreign reader holding it continues to see updates
    /// from the new publisher's observer task. The broadcast `Sender`
    /// is replaced; the catalogue is not.
    #[tokio::test]
    async fn psi_catalog_preserved_across_re_register() {
        let fm = make_fm();
        let (tx1, _rx1_drop) = broadcast::channel::<RtpPacket>(8);
        let cat1 = fm.register_input_publisher("in-a", tx1);
        let (tx2, _rx2_drop) = broadcast::channel::<RtpPacket>(8);
        let cat2 = fm.register_input_publisher("in-a", tx2);
        assert!(
            Arc::ptr_eq(&cat1, &cat2),
            "re-registering an input id must keep the same PsiCatalogStore"
        );
    }

    /// PES Switch Phase 2.2b: unregistering drops the catalogue entry
    /// from the registry, so a subsequent lookup returns None and a
    /// fresh registration creates a new store.
    #[tokio::test]
    async fn psi_catalog_dropped_on_unregister() {
        let fm = make_fm();
        let (tx1, _rx1_drop) = broadcast::channel::<RtpPacket>(8);
        let cat1 = fm.register_input_publisher("in-a", tx1);
        fm.unregister_input_publisher("in-a");
        assert!(fm.psi_catalog_for_input("in-a").is_none());
        let (tx2, _rx2_drop) = broadcast::channel::<RtpPacket>(8);
        let cat2 = fm.register_input_publisher("in-a", tx2);
        assert!(
            !Arc::ptr_eq(&cat1, &cat2),
            "fresh registration after unregister must allocate a new store"
        );
    }
}
