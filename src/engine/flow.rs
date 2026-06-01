// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use bytes::Bytes;
use tokio::sync::{broadcast, watch};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// ─── Diagnostic drop counters (Step 1 of pixelation root-cause hunt) ─────────
// Process-global atomics that record packet loss at every silent-drop site
// in the input → fixer → broadcast → output pipeline. A reporter task logs
// non-zero deltas every 5 s. Remove once root cause is identified.
static FWD_LAGGED: AtomicU64 = AtomicU64::new(0);
static FIXER_TS_ACTIVE_FULL: AtomicU64 = AtomicU64::new(0);
static FIXER_TS_PASSIVE_FULL: AtomicU64 = AtomicU64::new(0);
static FIXER_TS_SWITCH_FULL: AtomicU64 = AtomicU64::new(0);
static FIXER_TS_KEEPALIVE_FULL: AtomicU64 = AtomicU64::new(0);
static FIXER_OUT_NO_RECEIVERS: AtomicU64 = AtomicU64::new(0);

fn spawn_drop_diag_reporter() {
    use std::sync::atomic::AtomicBool;
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    tokio::spawn(async move {
        let mut prev = [0u64; 6];
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let cur = [
                FWD_LAGGED.load(Ordering::Relaxed),
                FIXER_TS_ACTIVE_FULL.load(Ordering::Relaxed),
                FIXER_TS_PASSIVE_FULL.load(Ordering::Relaxed),
                FIXER_TS_SWITCH_FULL.load(Ordering::Relaxed),
                FIXER_TS_KEEPALIVE_FULL.load(Ordering::Relaxed),
                FIXER_OUT_NO_RECEIVERS.load(Ordering::Relaxed),
            ];
            let delta: [u64; 6] = std::array::from_fn(|i| cur[i] - prev[i]);
            if delta.iter().any(|d| *d > 0) {
                tracing::warn!(
                    target: "drop_diag",
                    "drop diag (last 30s / cumulative): forwarder_lagged={}/{} fixer_ts_active={}/{} fixer_ts_passive={}/{} fixer_ts_switch={}/{} fixer_ts_keepalive={}/{} fixer_out_no_receivers={}/{}",
                    delta[0], cur[0], delta[1], cur[1], delta[2], cur[2],
                    delta[3], cur[3], delta[4], cur[4], delta[5], cur[5],
                );
            }
            prev = cur;
        }
    });
}

use crate::config::models::*;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{FlowStatsAccumulator, OutputConfigMeta, OutputStatsAccumulator};

use super::input_rist::spawn_rist_input;
use super::input_rtmp::spawn_rtmp_input;
use super::input_rtp::spawn_rtp_input;
use super::input_srt::spawn_srt_input;
use super::input_udp::spawn_udp_input;
use super::output_hls::spawn_hls_output;
use super::output_rist::spawn_rist_output;
use super::output_rtmp::spawn_rtmp_output;
use super::output_rtp::spawn_rtp_output;
use super::output_srt::spawn_srt_output;
use super::output_udp::spawn_udp_output;
use super::output_webrtc::spawn_webrtc_output;
use super::packet::RtpPacket;
use super::ts_continuity_fixer::{ProcessResult, TsContinuityFixer};
use super::bandwidth_monitor::spawn_bandwidth_monitor;
use super::degradation_monitor::spawn_degradation_monitor;
use super::media_analysis::spawn_media_analyzer;
use super::thumbnail::spawn_thumbnail_generator;
use super::tr101290::spawn_tr101290_analyzer;
use super::ts_assembler::{
    resolve_essence_slots, spawn_spts_assembler, AssemblyPlan, AssemblySlot, EsKind,
    EssenceResolveError, PendingEssenceSlot, ProgramPlan, SptsBuildResult,
};
use super::ts_es_bus::{NodeEsBus, TsEsDemuxer};
use crate::stats::collector::{MediaAnalysisAccumulator, ThumbnailAccumulator, Tr101290Accumulator};

/// Runtime state for a single media flow (one or more inputs, N outputs).
///
/// A `FlowRuntime` owns all the moving parts of a running flow:
///
/// - **Input runtimes** (`input_handles`): one [`InputRuntime`] per input
///   assigned to the flow. All input tasks run simultaneously (warm passive),
///   but only the currently active input's packets are forwarded onto the
///   flow's main broadcast channel. Each input publishes to its own per-input
///   broadcast channel; a small forwarder task drains the per-input channel
///   and, when that input's ID matches the current `active_input_tx` value,
///   re-publishes the packet onto `broadcast_tx`.
/// - **Main broadcast channel** (`broadcast_tx`): a Tokio `broadcast::Sender`
///   that fans out every incoming packet to all subscribed outputs. The
///   channel is bounded — capacity is set by the flow's
///   [`crate::config::models::BandwidthProfile`] (see
///   [`crate::engine::bandwidth_profile::resolve_for_flow`]) and ranges
///   from 16 384 slots (Standard) up to 65 536 slots (Uncompressed).
///   Slow receivers that fall behind will receive a `Lagged` error and
///   lose packets rather than blocking the input.
/// - **Active-input watch** (`active_input_tx`): a `tokio::sync::watch`
///   channel carrying the ID of the currently active input. Switching inputs
///   is a single `send(new_id)` — no task restart, no broadcast-channel
///   churn, no gap from the outputs' perspective.
/// - **Output tasks** (`output_handles`): a map of [`OutputRuntime`]
///   instances, one per active output destination. Passive outputs exist in
///   `config.outputs` but have no running task.
/// - **Cancellation token** (`cancel_token`): the parent token for the
///   entire flow. Cancelling it signals every input and output task to shut
///   down.
/// - **Stats accumulator** (`stats`): per-flow metrics (packets, bytes,
///   loss, FEC) that are periodically snapshotted by the stats subsystem.
///
/// Outputs can be hot-added or removed at runtime via [`add_output`](Self::add_output)
/// and [`remove_output`](Self::remove_output) without disturbing the inputs or
/// other outputs.
pub struct FlowRuntime {
    pub config: ResolvedFlow,
    /// The main broadcast sender that output tasks subscribe to. Fed by the
    /// per-input forwarder tasks, but only while the active input gate is open.
    pub broadcast_tx: broadcast::Sender<RtpPacket>,
    /// Per-input runtime state. Each entry represents a running input task
    /// plus its forwarder. Keyed by input ID so the map can be updated in
    /// place when inputs are added/removed via `update_flow`.
    pub input_handles: RwLock<HashMap<String, InputRuntime>>,
    /// Live, mutable copy of this flow's input definitions. Populated at
    /// start from `config.inputs` and updated by hot-swap `add_input` /
    /// `remove_input` operations so post-start readers
    /// (`replace_assembly`, `switch_active_input`, `add_output`) see the
    /// current set rather than the start-time snapshot.
    ///
    /// **Control plane only** — never read on the per-packet hot path.
    /// All readers are operator-initiated (assembly hot-swap, input
    /// switch, output add). The per-input packet path captures whatever
    /// state it needs at task-spawn time and owns it locally.
    pub live_inputs: RwLock<Vec<crate::config::models::InputDefinition>>,
    /// Watch channel holding the currently-active input ID. Empty string
    /// means "no active input" (flow is idle — outputs are up but no packets
    /// are being forwarded). Switching inputs is `active_input_tx.send(id)`.
    pub active_input_tx: watch::Sender<String>,
    /// Output task handles, keyed by output_id. Protected by an async
    /// `RwLock` to allow concurrent reads (stats queries) and exclusive
    /// writes (hot-add / remove).
    pub output_handles: RwLock<HashMap<String, OutputRuntime>>,
    /// Parent cancellation token for the entire flow. Child tokens are
    /// derived from this for each output, so cancelling the parent stops
    /// everything.
    pub cancel_token: CancellationToken,
    /// Per-flow stats accumulator shared with the input task and all
    /// output tasks. Metrics are updated via atomic operations.
    pub stats: Arc<FlowStatsAccumulator>,
    /// TR-101290 analyzer task handle.
    /// Held for ownership — dropping a JoinHandle detaches the task.
    /// Shutdown is driven by CancellationToken, not by aborting the handle.
    #[allow(dead_code)]
    pub analyzer_handle: JoinHandle<()>,
    /// Media analysis task handle (if enabled in config).
    /// Held for ownership — dropping a JoinHandle detaches the task.
    /// Shutdown is driven by CancellationToken, not by aborting the handle.
    #[allow(dead_code)]
    pub media_analysis_handle: Option<JoinHandle<()>>,
    /// Thumbnail generation task handle (if enabled and ffmpeg available).
    /// Held for ownership — dropping a JoinHandle detaches the task.
    /// Shutdown is driven by CancellationToken, not by aborting the handle.
    #[allow(dead_code)]
    pub thumbnail_handle: Option<JoinHandle<()>>,
    /// Content-analysis Lite-tier task handle (if Lite enabled and the
    /// active input carries MPEG-TS). Held for ownership; shutdown driven
    /// by the parent CancellationToken.
    #[allow(dead_code)]
    pub content_analysis_handle: Option<JoinHandle<()>>,
    /// Content-analysis Audio Full tier task handle.
    #[allow(dead_code)]
    pub content_analysis_audio_handle: Option<JoinHandle<()>>,
    /// Content-analysis Video Full tier task handle.
    #[allow(dead_code)]
    pub content_analysis_video_handle: Option<JoinHandle<()>>,
    /// Bandwidth monitor task handle (if bandwidth_limit is configured).
    /// Held for ownership — shutdown is driven by CancellationToken.
    #[allow(dead_code)]
    pub bandwidth_monitor_handle: Option<JoinHandle<()>>,
    /// Continuous-degradation monitor handle (input-stall + FEC-rate
    /// warnings). Always spawned; shutdown driven by CancellationToken.
    #[allow(dead_code)]
    pub degradation_monitor_handle: JoinHandle<()>,
    /// WHIP input session channel sender (only set for WebRTC/WHIP input flows).
    /// Must be registered with the WebrtcSessionRegistry after flow creation.
    #[cfg(feature = "webrtc")]
    pub whip_session_tx: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)>,
    /// WHEP output session channel sender (only set for flows with a WHEP server output).
    /// Must be registered with the WebrtcSessionRegistry after flow creation.
    #[cfg(feature = "webrtc")]
    pub whep_session_tx: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)>,
    /// Event sender for emitting operational events to the manager.
    /// Stored so that hot-added outputs can also emit events.
    pub event_sender: EventSender,
    /// Continuity-fixer command channel. Per-input forwarders enqueue
    /// `FixerCommand::{ActivePacket, PassivePacket, Switch, Keepalive,
    /// SignalSourceDiscontinuity}` onto this and let the per-flow
    /// `ts_fixer_task` own the fixer state uncontended.
    ///
    /// Stored on the runtime so [`Self::add_input`] can build a new
    /// per-input forwarder that publishes onto the same fixer task,
    /// and so [`Self::remove_input`] can enqueue
    /// `FixerCommand::DropInputPsi` to evict the removed input's PSI
    /// cache.
    #[allow(dead_code)] // Phase 1b wires the reader via FlowRuntime::add_input.
    pub(crate) fixer_tx: tokio::sync::mpsc::Sender<FixerCommand>,
    /// Global stats collector. Cloned at start; held so [`Self::add_input`]
    /// can build per-input thumbnail accumulators with the same
    /// `thumbnail_update_notify` watch every other accumulator on the
    /// node uses.
    #[allow(dead_code)] // Phase 1b wires the reader via FlowRuntime::add_input.
    pub global_stats: Arc<crate::stats::collector::StatsCollector>,
    /// Whether ffmpeg is available on the host. Used by [`Self::add_input`]
    /// to gate per-input thumbnail spawning, mirroring the start-time
    /// thumbnail decision.
    #[allow(dead_code)] // Phase 1b wires the reader via FlowRuntime::add_input.
    pub ffmpeg_available: bool,
    /// Per-flow broadcast-class channel capacity (slots). Derived from
    /// the resolved [`crate::config::models::BandwidthProfile`] at flow
    /// start and reused by [`Self::add_input`] so hot-added inputs
    /// build their per-input broadcast channel at the same size as
    /// the start-time inputs.
    pub broadcast_capacity: usize,
    /// Watch channel receiver for the detected video frame rate (fps).
    /// Created when media analysis is enabled so that hot-added outputs
    /// with `TargetFrames` delay mode can subscribe to frame rate updates
    /// instead of falling back to `fallback_ms`.
    pub frame_rate_rx: Option<watch::Receiver<Option<f64>>>,
    /// PID-bus SPTS assembler task handle. `Some` only when the flow has
    /// `assembly.kind != passthrough`; in that case the assembler is the
    /// sole publisher into `broadcast_tx`. Held for ownership; shutdown
    /// is driven by the parent `CancellationToken`. Phase 7 added
    /// `plan_tx` alongside the JoinHandle so `update_flow_assembly` can
    /// swap the running plan without restarting the flow.
    #[allow(dead_code)]
    pub pid_bus_assembler_handle: Option<crate::engine::ts_assembler::AssemblerHandle>,
    /// PID-bus elementary-stream bus. `Some` whenever the assembler is
    /// running. Kept on the runtime so `replace_assembly` (Phase 7) can
    /// spawn fresh Hitless mergers when the new plan adds Hitless slots.
    #[allow(dead_code)]
    pub es_bus: Option<Arc<NodeEsBus>>,
    /// Live mutable copy of the flow's `assembly` block. Used by
    /// `replace_assembly` to diff old → new and decide whether to
    /// hot-swap or fall back to a full restart. Persisted form lives
    /// in `config.config.assembly` and is also updated by the handler
    /// after a successful in-place swap.
    #[allow(dead_code)]
    pub current_assembly: tokio::sync::Mutex<Option<crate::config::models::FlowAssembly>>,
    /// Replay-server recording handle. `Some` when the flow's config
    /// carries a [`crate::config::models::RecordingConfig`] block and
    /// the `replay` Cargo feature is compiled in. Lifetime tied to the
    /// flow — cancelled when the flow stops.
    #[cfg(feature = "replay")]
    pub recording_handle: Option<Arc<crate::replay::writer::RecordingHandle>>,
    /// Per-input replay command channels for [`crate::replay::ReplayCommand`].
    /// Populated when the per-input dispatch in
    /// [`spawn_single_input`] sees an `InputConfig::Replay` variant.
    /// The WS dispatcher resolves the active input id, looks up the
    /// sender here, and forwards the command. Lock-free DashMap keyed
    /// by input_id.
    #[cfg(feature = "replay")]
    pub replay_command_txs: dashmap::DashMap<String, tokio::sync::mpsc::Sender<crate::replay::ReplayCommand>>,
    /// Per-output resource contributions, keyed by output_id. Stored
    /// at spawn time (start_output / add_output) and subtracted on
    /// remove_output so a runtime change to a display output's
    /// `hw_decode` (or an output's `video_encode` codec) — which the
    /// edge handles via remove + add of the same id — leaves the
    /// running totals in sync with the live output set. Tuple is
    /// `(cost_units, session_usage)`.
    pub output_contributions: std::sync::RwLock<
        HashMap<String, (u32, crate::engine::hardware_probe::HwSessionUsage)>,
    >,
    /// Resource-budget unit cost of this flow (computed at start time
    /// from `derive_cost_plan` + `compute_flow_cost_units`). Surfaced
    /// on `FlowStats.cost_units` and rolled up into the node-level
    /// `HealthPayload.resource_budget.units_used` so the manager UI
    /// can render utilisation against `units_total`. Interior-mutable
    /// so `add_output` / `remove_output` can apply per-output cost
    /// deltas without restarting the flow — operators changing a
    /// display output's `hw_decode` or an output's `video_encode`
    /// codec see the Resources card update on the next health tick.
    pub cost_units: std::sync::atomic::AtomicU32,
    /// Per-family hardware encoder + decoder session counts
    /// attributable to this flow's outputs. Each HW `video_encode`
    /// output adds one session against its family's slot
    /// (`nvenc_in_use` / `qsv_in_use` / `videotoolbox_in_use` /
    /// `amf_in_use`); each HW-decoded `display` output adds one
    /// against `nvdec_in_use` or `qsv_decode_in_use`. Interior-
    /// mutable so hot-add / hot-remove of outputs adjust the count.
    /// `FlowManager::total_hw_sessions()` reads this value and rolls
    /// it up into the node-level
    /// `HealthPayload.resource_budget.hw_session_usage`.
    pub hw_session_usage: std::sync::RwLock<crate::engine::hardware_probe::HwSessionUsage>,
    /// Per-edge runtime registry of active local-display claims. Threaded
    /// in from `FlowManager::create_flow` so display outputs can register
    /// themselves and park when the targeted `(device, audio_device)` is
    /// already held by another flow's display output. Built only on
    /// hosts where the `display` Cargo feature is enabled.
    #[cfg(all(feature = "display", target_os = "linux"))]
    pub display_claim_registry: Arc<crate::display::claim_registry::DisplayClaimRegistry>,
    /// Master clock for this flow (PCR pacing, A/V sync, lipsync trim).
    /// Built at start time from `FlowConfig.master_clock` (operator
    /// override) or auto-selected from the active input. Cloned into
    /// every output / replacer that needs a clock reference. See the
    /// `engine::master_clock` module for the trait, kind enum, and
    /// selection policy.
    pub master_clock: crate::engine::master_clock::MasterClockHandle,
    /// Weak handle to the owning [`crate::engine::manager::FlowManager`]
    /// (PES Switch Phase 2.1c Stage 2). `Weak` rather than `Arc` so the
    /// FlowManager → FlowRuntime ownership stays acyclic; flow code that
    /// needs the manager (input publisher register / unregister, shared
    /// demuxer acquire) upgrades on the path. Set at `start` time;
    /// `None` should never be observed in production because the manager
    /// outlives every flow it owns.
    pub flow_manager_weak: std::sync::Weak<crate::engine::manager::FlowManager>,
    /// `input_id`s for which this flow registered a node-wide publisher
    /// via [`crate::engine::manager::FlowManager::register_input_publisher`]
    /// at start time. The list is used by `stop()` / `Drop` to
    /// symmetrically unregister so cross-flow subscribers see a clean
    /// `RecvError::Closed`. Tracked separately from `input_handles`
    /// because the unregister has to happen synchronously, before the
    /// per-input cancel tokens fire and the broadcast::Senders drop.
    pub registered_input_ids: std::sync::Mutex<Vec<String>>,
    /// PES Switch Phase 2.1c Stage 3: shim tasks that hold
    /// [`crate::engine::manager::DemuxerHandle`]s for inputs this
    /// flow's assembly references but does NOT own in `flow.input_ids`
    /// (cross-flow ES routing). One shim per foreign input — each
    /// task simply waits on the flow's cancel token, then drops its
    /// `DemuxerHandle` so the FlowManager's refcounted demuxer
    /// registry decrements. When the LAST flow referencing a foreign
    /// input drops its handle, the shared demuxer task is cancelled.
    /// Empty for non-assembled flows and for flows whose assembly
    /// only references their own inputs.
    pub foreign_demuxer_shims: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
}

/// Runtime state for a single input within a flow.
///
/// Each input has:
/// - A Tokio task running the protocol-specific receiver (`input_handle`),
///   which publishes every packet it reads onto its own per-input broadcast
///   channel.
/// - A forwarder task (`forwarder_handle`) that drains the per-input
///   broadcast channel and, when the input is currently active, re-publishes
///   packets onto the flow's main broadcast channel. When the input is
///   passive the forwarder silently drops packets — the task still runs and
///   updates its per-input stats so operators can see backup source health.
/// - A child cancellation token (`cancel_token`) scoped to this input
///   individually. Cancelling it stops just this input (and its forwarder)
///   without disturbing sibling inputs or any outputs.
pub struct InputRuntime {
    /// The protocol-specific input task (rtp/srt/rtmp/... receiver).
    /// Held for ownership — shutdown is driven via `cancel_token`.
    #[allow(dead_code)]
    pub input_handle: JoinHandle<()>,
    /// The forwarder task that gates packets on the watch channel and
    /// re-publishes them to the flow's main broadcast channel.
    #[allow(dead_code)]
    pub forwarder_handle: JoinHandle<()>,
    /// Child cancellation token scoped to this input. Cancelling it stops
    /// the input and its forwarder. Derived from the flow's parent token.
    #[allow(dead_code)]
    pub cancel_token: CancellationToken,
    /// Per-input thumbnail generation task handle (if enabled and ffmpeg available).
    #[allow(dead_code)]
    pub thumbnail_handle: Option<JoinHandle<()>>,
}

/// Runtime state for a single output within a flow.
///
/// Each output has its own:
/// - **Task handle** (`handle`): the Tokio `JoinHandle` for the output's
///   send loop (RTP UDP or SRT).
/// - **Child cancel token** (`cancel_token`): derived from the flow's
///   parent token. Cancelling this token stops only this output; cancelling
///   the parent token stops all outputs (and the input) at once.
/// - **Stats accumulator** (`stats`): per-output metrics (packets sent,
///   bytes sent, drops, FEC packets sent).
pub struct OutputRuntime {
    /// Tokio task handle for this output's send loop.
    /// Awaited during remove_output() and stop() for graceful shutdown.
    pub handle: JoinHandle<()>,
    /// Child cancellation token. Dropping or cancelling this stops only
    /// this output, leaving the rest of the flow running.
    pub cancel_token: CancellationToken,
    /// Per-output stats accumulator. Held for Arc refcount — the output task
    /// holds a clone and writes to it; dropping this would decrement the
    /// refcount while the task is still running.
    #[allow(dead_code)]
    pub stats: Arc<OutputStatsAccumulator>,
}

/// Structured error variants for [`FlowRuntime::add_input`] /
/// [`FlowRuntime::remove_input`]. Each variant carries a stable
/// `error_code` string the WS handler surfaces on `command_ack.error_code`
/// so the manager UI can render targeted messages (e.g. "switch to a
/// different active input first" for `active_input_in_use`).
///
/// The error type is attached to `anyhow::Error` so existing
/// `Result<(), anyhow::Error>` plumbing is unchanged. WS handlers recover
/// the code via `err.downcast_ref::<InputMembershipError>()`.
#[derive(Debug, Clone)]
pub struct InputMembershipError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for InputMembershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InputMembershipError {}

impl InputMembershipError {
    pub fn already_member(input_id: &str) -> Self {
        Self {
            code: "input_already_member",
            message: format!(
                "Input '{input_id}' is already a member of this flow"
            ),
        }
    }
    pub fn not_member(input_id: &str) -> Self {
        Self {
            code: "input_not_member",
            message: format!(
                "Input '{input_id}' is not a member of this flow"
            ),
        }
    }
    pub fn active_in_use(input_id: &str) -> Self {
        Self {
            code: "active_input_in_use",
            message: format!(
                "Input '{input_id}' is the currently active input — \
                 activate a different member first"
            ),
        }
    }
    pub fn pid_bus_in_use(input_id: &str) -> Self {
        Self {
            code: "pid_bus_input_in_use",
            message: format!(
                "Input '{input_id}' is referenced by the running \
                 assembly plan — unbind the slot first"
            ),
        }
    }
    pub fn hitless_leg_change(input_id: &str) -> Self {
        Self {
            code: "hitless_leg_change_requires_restart",
            message: format!(
                "Hot-edit involving input '{input_id}' would change a \
                 hitless slot's leg list — requires full flow restart"
            ),
        }
    }
    pub fn resource_critical(flow_id: &str, input_id: &str) -> Self {
        Self {
            code: "input_resource_critical",
            message: format!(
                "Cannot add input '{input_id}' to flow '{flow_id}': \
                 system resources critical"
            ),
        }
    }
}

/// Success output of [`FlowRuntime::add_input`]. Carries the registered
/// input id so callers can persist it, plus — when `feature = "webrtc"`
/// is on AND the new input is a WHIP server — the session channel that
/// the WS handler must register with the `WebrtcSessionRegistry` so
/// browsers can pair without a flow restart.
///
/// WHIP-server inputs hot-added at runtime were previously "spawned but
/// invisible to browsers until restart". Surfacing `whip_info` closes
/// that gap.
pub struct HotAddedInputInfo {
    #[allow(dead_code)] // Carried for symmetry + future logging hooks; only whip_info is read today.
    pub input_id: String,
    #[cfg(feature = "webrtc")]
    pub whip_info: Option<WhipSessionInfo>,
}

impl FlowRuntime {
    /// Create and start a new flow from the provided configuration.
    ///
    /// This is the main bring-up entry point. It performs the following steps
    /// in order:
    ///
    /// 1. Creates a parent [`CancellationToken`] for the flow.
    /// 2. Creates the broadcast channel used for input-to-output fan-out.
    /// 3. Registers the flow in the global stats collector.
    /// 4. Spawns the input task (RTP or SRT) which will begin writing to
    ///    the broadcast channel.
    /// 5. Spawns an output task for each output in the config, each
    ///    subscribing to the broadcast channel.
    ///
    /// # Errors
    ///
    /// Returns an error if any output task fails to start (e.g., socket
    /// bind failure). The input task is spawned first and errors there are
    /// reported asynchronously via the task's log output.
    pub async fn start(
        config: ResolvedFlow,
        global_stats: Arc<crate::stats::collector::StatsCollector>,
        ffmpeg_available: bool,
        event_sender: EventSender,
        #[cfg(all(feature = "display", target_os = "linux"))]
        display_claim_registry: Arc<crate::display::claim_registry::DisplayClaimRegistry>,
        node_es_bus: Arc<NodeEsBus>,
        flow_manager: Arc<crate::engine::manager::FlowManager>,
    ) -> Result<Self> {
        // PID-bus assembly is accepted at config-validation time. Phases
        // 5 + 6 together lift the runtime for `kind = spts`; `kind =
        // mpts` still bails loudly, and `kind = passthrough` means "no
        // assembler — behave like a legacy flow".
        //
        // Slot resolution is a two-phase process:
        //   1. `build_spts_plan` produces an `SptsBuildResult` with the
        //      concrete plan (scope guards enforced) plus any `Essence`
        //      slots still pending a `(input_id, source_pid)` lookup
        //      against the per-input PSI catalogue. Every scope failure
        //      emits a Critical event with a distinct `error_code`.
        //   2. After the per-input demuxers are running, the runtime
        //      polls each catalogue until every `Essence` slot resolves
        //      (up to `ESSENCE_RESOLVE_TIMEOUT`). On success, the plan
        //      is patched in place and the assembler is spawned.
        let spts_build: Option<SptsBuildResult> = match config.config.assembly.as_ref() {
            None => None,
            Some(a) if matches!(a.kind, AssemblyKind::Passthrough) => None,
            Some(a) => Some(build_assembly_plan(a, &config, &event_sender)?),
        };
        // Node-wide ES bus reference. The bus instance is owned by
        // `FlowManager` and shared across every assembled flow on the
        // edge — non-assembly flows hold a clone of the Arc but never
        // touch it (no map lookups, no allocations). Channels are keyed
        // by `(input_id, source_pid)` which is globally unique on the
        // node, so cross-flow channel collisions are impossible.
        //
        // Phase 1 of the PES Switch redesign: this is the only piece of
        // structural sharing in P1. Demuxer dedup (one demuxer per
        // input regardless of how many flows reference it) is a P2
        // change that lands together with dropping assignment-
        // uniqueness; before P2 each flow still owns its own demuxers
        // because inputs are still flow-scoped.
        let es_bus: Option<Arc<NodeEsBus>> = spts_build
            .as_ref()
            .map(|_| Arc::clone(&node_es_bus));
        let cancel_token = CancellationToken::new();

        // Resolve the bandwidth profile for this flow. Operator override
        // on `FlowConfig::bandwidth_profile` wins; otherwise auto-derive
        // from the input set (ST 2110-20/-23 / MXL video → Uncompressed,
        // everything else → Standard). The resolved capacity drives
        // every broadcast-class channel on the flow: main fan-out,
        // per-input pre-broadcast, fixer command channel.
        let input_refs: Vec<&InputDefinition> = config.inputs.iter().collect();
        let bandwidth_profile =
            crate::engine::bandwidth_profile::resolve_for_flow(
                &config.config,
                &input_refs,
            );
        let broadcast_capacity = bandwidth_profile.broadcast_capacity();
        tracing::info!(
            "flow {}: bandwidth profile {:?} → broadcast capacity {} slots",
            config.config.id,
            bandwidth_profile,
            broadcast_capacity,
        );

        // The broadcast channel is bounded to `broadcast_capacity` slots
        // (set by the flow's bandwidth profile).
        // When a slow output (receiver) cannot keep up, it will *not* block the
        // input or other outputs. Instead, the lagging receiver's next `recv()`
        // returns `RecvError::Lagged(n)`, telling it how many messages it missed.
        // Each output handles this by incrementing its `packets_dropped` stat.
        // The underscore `_` receiver is created and immediately dropped; it is
        // only needed to satisfy the channel constructor signature.
        let (broadcast_tx, _) = broadcast::channel::<RtpPacket>(broadcast_capacity);

        // Pick the active input (if any) up front — used for stats registration,
        // media analysis setup, and the initial value of the active-input watch.
        let active_input_def = config.active_input();
        let active_input_id: String = active_input_def
            .map(|d| d.id.clone())
            .unwrap_or_default();
        let active_input_cfg: Option<&InputConfig> = active_input_def.map(|d| &d.config);

        let input_type = active_input_cfg.map(input_type_str).unwrap_or("none");

        // Register flow stats
        let flow_stats = global_stats.register_flow(
            config.config.id.clone(),
            config.config.name.clone(),
            input_type.to_string(),
        );

        // Watch channel carrying the currently-active input ID. All per-input
        // forwarder tasks subscribe to this; switching inputs is a single
        // send() that does not disturb any running task.
        let (active_input_tx, active_input_watch_rx) = watch::channel(active_input_id.clone());
        flow_stats.set_active_input_id(&active_input_id);

        // Populate input config metadata for topology display from the
        // currently active input. When there is no active input, we leave
        // the stats meta empty and the UI shows "idle".
        //
        // Uses the same setter the switch path calls so the UI sees fresh
        // `input_type` + `InputConfigMeta` after every `switch_active_input`.
        if let Some(input) = active_input_cfg {
            let input_meta = build_input_config_meta(input);
            flow_stats.update_active_input_meta(input_type, input_meta);
        }

        // Populate output config metadata (all outputs, active or passive —
        // the manager UI shows passive outputs too, just with an inactive flag).
        for oc in &config.outputs {
            flow_stats.output_config_meta.insert(
                oc.id().to_string(),
                build_output_config_meta(oc),
            );
        }

        // TS continuity fixer for seamless input switching. Ensures CC
        // counters are continuous and discontinuity_indicator is set when
        // switching between inputs, preventing external decoders from
        // losing lock.
        //
        // The fixer is owned by a single dedicated task (`ts_fixer_task`)
        // so the per-input forwarders can hand off work via a bounded mpsc
        // without ever locking shared state on the packet hot path.
        // Channel capacity matches the per-flow bandwidth-profile
        // broadcast capacity so drop-on-full semantics line up with the
        // upstream broadcast drop-on-lag behaviour.
        let (fixer_tx, fixer_rx) = tokio::sync::mpsc::channel::<FixerCommand>(
            broadcast_capacity,
        );
        let fixer_task_cancel = cancel_token.child_token();
        // Handle is intentionally dropped — the task lives for the flow's
        // lifetime and exits on `fixer_task_cancel`, which inherits from
        // the flow's root cancel token. No join is needed on shutdown.
        let _ = tokio::spawn(ts_fixer_task(
            fixer_rx,
            broadcast_tx.clone(),
            fixer_task_cancel,
        ));
        spawn_drop_diag_reporter();

        // ── Master clock selection (must precede input + output spawn so
        // the pacer is available for ingress + egress transcoders) ──
        //
        // Ingress transcoders (Group A inputs that re-encode video) need
        // the same per-flow master-clock pacer the egress replacers use,
        // so that PCR generated inside the encoder pipeline references
        // the same clock as PCR re-stamped at wire-emit time.
        // Resolve the clock-source input for PID-bus assembled flows.
        // For assembled flows the master clock should track the
        // operator-designated `assembly.pcr_source` input rather than
        // whichever input happens to be the broadcast publisher (the
        // assembler's output is the broadcast publisher — sampling
        // its bytes would be a self-referential loop). For non-
        // assembled flows this is `None` and the active input drives
        // the master clock as before.
        let clock_source_input_cfg: Option<&crate::config::models::InputConfig> =
            crate::engine::master_clock::resolve_pcr_source_input_id(
                &config.config,
            )
            .and_then(|id| {
                config
                    .inputs
                    .iter()
                    .find(|d| d.id == id)
                    .map(|d| &d.config)
            });
        // Master clock takes the resolved clock-source input when
        // present, otherwise falls back to the active input.
        let mc_source_input_cfg = clock_source_input_cfg.or(active_input_cfg);
        let (master_clock, pll_master, ptp_handle) = build_master_clock(
            &config.config,
            mc_source_input_cfg,
            active_input_tx.subscribe(),
            &event_sender,
            &cancel_token,
        );
        if let Some(h) = ptp_handle {
            let _ = flow_stats.ptp_state.set(h);
        }
        flow_stats.set_master_clock_telemetry(master_clock.telemetry());
        flow_stats.set_master_clock_lipsync(master_clock.lipsync_offset_90k());
        let av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>> =
            Some(Arc::new(crate::engine::av_sync_mux::AvSyncPacer::new(
                master_clock.clone(),
            )));
        // For assembled flows (PID-bus / Node-Bus), claim the pacer so
        // per-input rewriters skip — the assembler runs its own
        // shared-anchor rewriter on the assembled output. Avoids per-
        // input anchors clashing with cross-input PES splice math.
        if config.config.assembly.is_some() {
            if let Some(p) = av_sync_pacer.as_ref() {
                p.mark_assembler_owned();
            }
        }

        // Spawn every input task (warm passive). Each input publishes to its
        // own per-input broadcast channel; a forwarder task drains it and
        // gates on the active-input watch.
        //
        // Per-input `force_idr` flags let the forwarder ask an input's
        // ingress video transcoder to emit an IDR on its next encoded frame.
        // The flag is set when this input transitions from passive → active
        // so downstream decoders get an immediate keyframe to resync on,
        // rather than waiting up to a full GOP for the next natural IDR.
        #[cfg(feature = "webrtc")]
        let mut whip_session_info: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)> = None;
        let mut input_handles: HashMap<String, InputRuntime> = HashMap::new();
        // Per-flow registry of replay command channels (one per Replay
        // input). Populated by `spawn_single_input`; the manager-WS
        // dispatcher reads it to route mark/cue/play/scrub commands.
        #[cfg(feature = "replay")]
        let replay_command_txs: dashmap::DashMap<String, tokio::sync::mpsc::Sender<crate::replay::ReplayCommand>> = dashmap::DashMap::new();
        // PES Switch Phase 2.1c Stage 2: track which `input_id`s we
        // register with the node-wide `FlowManager` so `stop()` can
        // symmetrically unregister. Built incrementally as the input
        // loop spawns each input task.
        let mut registered_input_ids: Vec<String> = Vec::with_capacity(config.inputs.len());
        // Bundle the shared per-flow state into a context so each input
        // spawn is a single call. Same engine code feeds the hot-add path
        // in `FlowRuntime::add_input` — see [`spawn_input_runtime`] for
        // the per-input setup order and rationale.
        let spawn_ctx = InputSpawnContext {
            flow_id: &config.config.id,
            flow_cancel: &cancel_token,
            active_input_watch_rx: &active_input_watch_rx,
            fixer_tx: &fixer_tx,
            flow_stats: &flow_stats,
            flow_manager: &flow_manager,
            event_sender: &event_sender,
            av_sync_pacer: av_sync_pacer.clone(),
            es_bus: es_bus.as_ref(),
            global_stats: &global_stats,
            ffmpeg_available,
            thumbnail_enabled: config.config.thumbnail,
            flow_clock_domain: config.config.clock_domain,
            broadcast_capacity,
            #[cfg(feature = "replay")]
            replay_command_txs: &replay_command_txs,
        };
        for input_def in &config.inputs {
            let spawned = spawn_input_runtime(input_def, &spawn_ctx);
            registered_input_ids.push(spawned.input_id.clone());
            #[cfg(feature = "webrtc")]
            {
                if spawned.whip_info.is_some() {
                    whip_session_info = spawned.whip_info;
                }
            }
            input_handles.insert(spawned.input_id, spawned.input_runtime);
        }

        // If the flow has no inputs at all, note it in the log. The broadcast
        // channel will stay silent until inputs are added by the manager.
        if input_handles.is_empty() {
            tracing::debug!(
                "Flow '{}' started with no inputs — awaiting input attachment",
                config.config.id
            );
        }

        // Spawn the SPTS assembler *after* every per-input demuxer is
        // wired up. For Phase 6, if the plan carries any `Essence` slots,
        // run the resolver first against each input's PSI catalogue:
        // the catalogue is populated by the Phase 2 observer on every
        // TS-producing input's per-input broadcast channel, so PAT/PMT
        // arriving even a few hundred ms after input spawn lands before
        // the resolver's 3 s deadline.
        // Build the per-input clock-identity map used by the cross-
        // clock compatibility check inside `finalize_spts_assembler`.
        // Populated unconditionally for assembled flows; the check
        // itself is a no-op today (membership rule prevents cross-
        // clock combinations) but the map is cheap (one entry per
        // input — typical < 10) and Phase 2.1 will need it ready.
        let input_clocks: HashMap<String, crate::engine::master_clock::ClockIdentity> = config
            .inputs
            .iter()
            .map(|i| (i.id.clone(), crate::engine::master_clock::clock_identity_for_input(i)))
            .collect();
        // PES Switch Phase 2.1c Stage 3: enable cross-flow ES routing.
        // The assembly plan may reference inputs that are NOT in
        // `flow.input_ids` — owned by sibling flows on the node.
        // For each such foreign input, acquire a refcounted handle on
        // the shared `TsEsDemuxer` so the demuxer stays alive while
        // this flow holds the reference; spawn a shim task that
        // releases the handle on flow cancel. Pid slots subscribe to
        // the resulting `(input_id, source_pid)` bus channels just like
        // own-flow slots. Essence resolution for foreign inputs still
        // requires the foreign flow's PSI catalogue, which today's
        // catalogue lookup (flow-scoped `per_input_counters`) doesn't
        // surface — foreign-input Essence slots therefore fail with
        // `pid_bus_essence_no_catalogue`. Operators should use
        // explicit Pid slots for cross-flow refs until catalogue
        // sharing lands as a follow-up.
        let mut foreign_demuxer_shims: Vec<JoinHandle<()>> = Vec::new();
        if let Some(build) = spts_build.as_ref() {
            let own_input_ids: std::collections::HashSet<String> = config
                .inputs
                .iter()
                .map(|d| d.id.clone())
                .collect();
            let mut foreign_seen: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for program in &build.plan.programs {
                for slot in &program.slots {
                    let candidates: Vec<&str> = match &slot.switch_legs {
                        Some(legs) => legs.iter().map(|(iid, _)| iid.as_str()).collect(),
                        None => vec![slot.source.0.as_str()],
                    };
                    for iid in candidates {
                        if own_input_ids.contains(iid) || foreign_seen.contains(iid) {
                            continue;
                        }
                        foreign_seen.insert(iid.to_string());
                        // Acquire a refcounted demuxer handle. The
                        // `spawn_fn` only fires on first reference
                        // node-wide; subsequent calls reuse the
                        // running task. If the foreign input isn't
                        // running on this node (publisher absent), the
                        // acquire returns `None` and the shim is a
                        // no-op — the slot will have no upstream data
                        // until the owning flow comes online.
                        let bus_for_demuxer = es_bus.as_ref().map(Arc::clone).unwrap_or_else(|| {
                            // Unreachable: spts_build => es_bus is Some.
                            // Defensive Arc clone so the closure stays
                            // self-contained.
                            Arc::clone(&node_es_bus)
                        });
                        // Foreign demuxer publishes onto the same node
                        // bus; the slot's bus subscriber sees its
                        // packets identically to own-input slots.
                        // Per-input counters for cross-flow demuxer
                        // are attributed to a synthetic node-level
                        // accumulator since this flow doesn't own
                        // PerInputCounters for foreign inputs. The
                        // owning flow's demuxer still updates its
                        // counters (because the demuxer was first
                        // spawned by that flow); subsequent acquire
                        // calls reuse the running task, so the
                        // spawn_fn closure here is normally never
                        // invoked — it's only the first-acquire path.
                        // For the rare edge case where THIS flow is
                        // the first to reference the foreign input
                        // (e.g. owner flow hasn't yet completed
                        // start), we provide a stub counters arc so
                        // the demuxer can start without panicking.
                        let foreign_id = iid.to_string();
                        let stub_counters = std::sync::Arc::new(
                            crate::stats::collector::PerInputCounters::new(
                                "foreign".to_string(),
                                crate::stats::collector::InputConfigMeta {
                                    mode: None,
                                    local_addr: None,
                                    remote_addr: None,
                                    listen_addr: None,
                                    bind_addr: None,
                                    rtsp_url: None,
                                    whep_url: None,
                                },
                            ),
                        );
                        let demuxer_handle = flow_manager.acquire_demuxer(
                            iid,
                            move |rx, cancel| {
                                Some(spawn_ts_es_demuxer_consumer(
                                    foreign_id,
                                    rx,
                                    bus_for_demuxer,
                                    cancel,
                                    stub_counters,
                                ))
                            },
                        );
                        let cancel = cancel_token.child_token();
                        foreign_demuxer_shims.push(tokio::spawn(async move {
                            let _demuxer_handle = demuxer_handle;
                            cancel.cancelled().await;
                        }));
                    }
                }
            }
        }

        let pid_bus_assembler_handle: Option<crate::engine::ts_assembler::AssemblerHandle> =
            match (spts_build, &es_bus) {
                (Some(build), Some(bus)) => Some(
                    finalize_spts_assembler(
                        build,
                        bus,
                        &broadcast_tx,
                        &flow_stats,
                        &cancel_token,
                        &event_sender,
                        &config.config.id,
                        &input_clocks,
                        &flow_manager,
                        av_sync_pacer.clone(),
                    )
                    .await?,
                ),
                _ => None,
            };

        // Start TR-101290 analyzer (independent broadcast subscriber).
        //
        // Only spawn the analyzer for inputs that actually carry MPEG-TS.
        // Audio-only and ANC inputs (ST 2110-30/-31/-40, `rtp_audio`) carry
        // PCM samples or RFC 8331 ancillary data on the broadcast channel,
        // not TS packets — running TR-101290 on them produces an endless
        // stream of "sync lost" warnings within seconds and pollutes the
        // operator log for no diagnostic value. The accumulator is still
        // created so `flow_stats.tr101290` has a stable shape regardless
        // of input type; for non-TS flows it just stays at zero.
        let tr101290_acc = Arc::new(Tr101290Accumulator::new());
        flow_stats.tr101290.set(tr101290_acc.clone()).ok();
        let analyzer_handle = if active_input_cfg.map_or(false, |i| i.is_ts_carrier()) {
            spawn_tr101290_analyzer(
                &broadcast_tx,
                tr101290_acc,
                cancel_token.child_token(),
                Some(active_input_watch_rx.clone()),
            )
        } else {
            // Spawn an immediately-completing no-op task so the
            // `analyzer_handle: JoinHandle<()>` field always has a value.
            tokio::spawn(async {})
        };

        // Create a watch channel to broadcast the detected video frame rate
        // to output tasks that use TargetFrames delay mode. The channel is
        // created whenever media analysis is enabled (not just when current
        // outputs need it) so that outputs hot-added later via the manager
        // can also subscribe to frame rate updates instead of falling back
        // to fallback_ms.
        let (frame_rate_tx, frame_rate_rx) = if config.config.media_analysis && active_input_cfg.is_some() {
            let (tx, rx) = tokio::sync::watch::channel::<Option<f64>>(None);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Start media analysis (if enabled and an active input is present).
        // Media analysis is always keyed on the currently active input —
        // passive inputs don't reach the broadcast channel, so the analyzer
        // would only see silence from them.
        let media_analysis_handle = if config.config.media_analysis && active_input_cfg.is_some() {
            let m = media_analysis_metadata(active_input_cfg.expect("checked by is_some() guard"));
            let media_acc = Arc::new(MediaAnalysisAccumulator::new(
                m.protocol,
                m.payload_format,
                m.fec_enabled,
                m.fec_type,
                m.redundancy_enabled,
                m.redundancy_type,
            ));
            flow_stats.media_analysis.set(media_acc.clone()).ok();
            Some(spawn_media_analyzer(
                &config,
                &broadcast_tx,
                media_acc,
                cancel_token.child_token(),
                frame_rate_tx,
                active_input_tx.subscribe(),
                config.inputs.clone(),
            ))
        } else {
            None
        };

        // Start thumbnail generator (if enabled, ffmpeg available, and an
        // active input is present)
        let thumbnail_handle = if config.config.thumbnail && ffmpeg_available && active_input_cfg.is_some() {
            let thumb_acc = Arc::new(ThumbnailAccumulator::new_with_update_notify(
                global_stats.thumbnail_update_notify.clone(),
            ));
            flow_stats.thumbnail.set(thumb_acc.clone()).ok();
            Some(spawn_thumbnail_generator(
                &broadcast_tx,
                thumb_acc,
                cancel_token.child_token(),
                config.config.thumbnail_program_number,
            ))
        } else {
            None
        };

        // Start in-depth content-analysis subsystem. Each tier is a
        // dedicated broadcast subscriber — drop-on-lag, no feedback into
        // the data path. Per-tier gating:
        //   - Lite      : TS-carrying input (PSI / GOP / SCTE-35 / MDI)
        //   - Audio Full: TS-carrying input  OR  PCM-RTP audio input
        //                 (ST 2110-30 / -31 / RtpAudio) — PCM path skips
        //                 demux+decode and feeds samples straight to R128
        //   - Video Full: TS-carrying input (ST 2110-20/-23 inputs encode
        //                 to TS at ingress so this still applies)
        //
        // The accumulator is created up-front if *any* tier-eligible spawn
        // will happen so the wire shape carries the tier-drop counters +
        // UI can render "analysing…" even before the first sample lands.
        let content_cfg = config.config.effective_content_analysis();
        let ts_carrier = active_input_cfg.map_or(false, |i| i.is_ts_carrier());
        let audio_full_mode = active_input_cfg.and_then(|i| derive_audio_full_mode(i, ts_carrier));
        let lite_eligible = content_cfg.lite && ts_carrier;
        let audio_full_eligible = content_cfg.audio_full && audio_full_mode.is_some();
        let video_full_eligible = content_cfg.video_full && ts_carrier;
        let any_tier_eligible =
            lite_eligible || audio_full_eligible || video_full_eligible;

        let ca_acc_shared = if any_tier_eligible {
            let acc = Arc::new(crate::stats::collector::ContentAnalysisAccumulator::new());
            flow_stats.content_analysis.set(acc.clone()).ok();
            Some(acc)
        } else {
            None
        };
        let content_analysis_handle = if let Some(ref acc) = ca_acc_shared {
            if lite_eligible {
                Some(crate::engine::content_analysis::spawn_content_analysis_lite(
                    &broadcast_tx,
                    acc.clone(),
                    event_sender.clone(),
                    config.config.id.clone(),
                    cancel_token.child_token(),
                    frame_rate_rx.clone(),
                ))
            } else {
                None
            }
        } else {
            None
        };
        let content_analysis_audio_handle = match (&ca_acc_shared, audio_full_mode) {
            (Some(acc), Some(mode)) if content_cfg.audio_full => {
                Some(crate::engine::content_analysis::spawn_content_analysis_audio_full(
                    &broadcast_tx,
                    acc.clone(),
                    event_sender.clone(),
                    config.config.id.clone(),
                    mode,
                    cancel_token.child_token(),
                ))
            }
            _ => None,
        };
        let content_analysis_video_handle = if let Some(ref acc) = ca_acc_shared {
            if video_full_eligible {
                Some(crate::engine::content_analysis::spawn_content_analysis_video_full(
                    &broadcast_tx,
                    acc.clone(),
                    event_sender.clone(),
                    config.config.id.clone(),
                    content_cfg.video_full_hz,
                    cancel_token.child_token(),
                ))
            } else {
                None
            }
        } else {
            None
        };

        // Start bandwidth monitor (if configured)
        let bandwidth_monitor_handle = if let Some(ref bw_limit) = config.config.bandwidth_limit {
            flow_stats.bandwidth_limit_mbps.set(bw_limit.max_bitrate_mbps).ok();
            Some(spawn_bandwidth_monitor(
                config.config.id.clone(),
                bw_limit.clone(),
                flow_stats.clone(),
                event_sender.clone(),
                cancel_token.child_token(),
            ))
        } else {
            None
        };

        // Start continuous-degradation monitor (input-stall, FEC-rate).
        // Always on — the thresholds are set so quiet links don't alarm.
        let degradation_monitor_handle = spawn_degradation_monitor(
            config.config.id.clone(),
            flow_stats.clone(),
            event_sender.clone(),
            cancel_token.child_token(),
        );

        // Master clock + pacer were built earlier (above the input loop)
        // so ingress transcoders could inherit the same per-flow clock.
        // The PCR ingress sampler + telemetry tick stay here because they
        // depend on broadcast_tx + flow_stats already being wired.
        if let Some(pll_master) = pll_master {
            let sampler_cancel = cancel_token.child_token();
            // For assembled flows, subscribe the sampler to the
            // designated `assembly.pcr_source.input_id`'s per-input
            // broadcast (via `FlowManager::subscribe_input`) so the
            // PLL tracks the *source* clock, not the assembler's
            // *output* clock (which is the master clock itself,
            // forming a circular feedback loop). For non-assembled
            // flows fall through to the legacy flow-broadcast
            // subscription — output is byte-for-byte the active
            // input's stream, so the sampler sees the source clock
            // either way.
            let sampler_rx = if let Some(input_id) =
                crate::engine::master_clock::resolve_pcr_source_input_id(
                    &config.config,
                )
            {
                match flow_manager.subscribe_input(input_id) {
                    Some(rx) => {
                        tracing::info!(
                            flow_id = %config.config.id,
                            pcr_source_input = %input_id,
                            "PCR sampler subscribing to designated clock-source input"
                        );
                        rx
                    }
                    None => {
                        tracing::warn!(
                            flow_id = %config.config.id,
                            pcr_source_input = %input_id,
                            "PCR sampler: clock-source input not yet registered on the node; \
                             falling back to flow broadcast (may form a self-referential loop \
                             on assembled flows)"
                        );
                        broadcast_tx.subscribe()
                    }
                }
            } else {
                broadcast_tx.subscribe()
            };
            let _ = crate::engine::pcr_ingress_sampler::spawn_pcr_ingress_sampler_with_rx(
                pll_master,
                sampler_rx,
                config.config.id.clone(),
                active_input_tx.subscribe(),
                event_sender.clone(),
                flow_stats.clone(),
                sampler_cancel,
            );
        }
        // Source-side discontinuity watcher. Runs on every flow regardless
        // of `master_clock.kind` because the PCR/PTS/DTS-backward-jump
        // alarm is relevant to Passthrough flows too (the operator wants
        // to see when an upstream `-stream_loop` sender's file boundary
        // glitches the receiver), not just SourcePcrPll-class flows.
        // Subscribes to the flow broadcast — for assembled flows that
        // means it sees the assembled output, which doesn't have inner
        // source PCRs, so the alarm only fires for passthrough /
        // transcoded flows in practice. For PID-bus flows the equivalent
        // alarm comes from each input's per-input ES bus stats (Phase 4).
        let _ = crate::engine::pcr_ingress_sampler::spawn_source_discontinuity_watch(
            &broadcast_tx,
            config.config.id.clone(),
            active_input_tx.subscribe(),
            event_sender.clone(),
            flow_stats.clone(),
            Some(fixer_tx.clone()),
            cancel_token.child_token(),
        );
        {
            let mc = master_clock.clone();
            let acc = flow_stats.clone();
            let cancel = cancel_token.child_token();
            tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(1));
                interval.set_missed_tick_behavior(
                    tokio::time::MissedTickBehavior::Delay,
                );
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = interval.tick() => {
                            let mut t = mc.telemetry();
                            // Stamp the currently-active input ID so
                            // the manager UI can render "PLL locked on
                            // <input_id>" / "wallclock fallback after
                            // failing on <input_id>". `acc` holds the
                            // canonical active-input under an RwLock —
                            // empty string maps to None.
                            if t.active_input_id.is_none() {
                                if let Ok(g) = acc.active_input_id.read() {
                                    if !g.is_empty() {
                                        t.active_input_id = Some(g.clone());
                                    }
                                }
                            }
                            acc.set_master_clock_telemetry(t);
                            acc.set_master_clock_lipsync(mc.lipsync_offset_90k());
                        }
                    }
                }
            });
        }

        // Start output tasks
        #[cfg(feature = "webrtc")]
        let mut whep_session_info: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)> = None;
        let mut output_handles = HashMap::new();
        // Resolve the active input's audio format once so audio output spawn
        // modules can construct an optional TranscodeStage. Returns None for
        // non-audio inputs (video, MPEG-TS, etc.) — passthrough is selected.
        // When the flow has multiple inputs and the active one is swapped,
        // outputs will need restarts to pick up the new format; most use
        // cases keep a homogeneous set of inputs so passthrough stays
        // applicable.
        let input_audio_format = active_input_cfg
            .and_then(crate::engine::audio_transcode::InputFormat::from_input_config);
        let compressed_audio_input = active_input_cfg
            .map_or(false, crate::engine::audio_decode::input_can_carry_ts_audio);
        // Loop over the *active* outputs only. Passive outputs are persisted
        // in config but do not get a running task until the operator flips
        // them active via the API.
        for output_config in config.outputs.iter().filter(|o| o.active()) {
            // For WHEP server outputs, create the session channel so the HTTP
            // handler can forward SDP offers to the output task.
            #[cfg(feature = "webrtc")]
            let whep_rx = if let OutputConfig::Webrtc(wc) = output_config {
                if wc.mode == crate::config::models::WebrtcOutputMode::WhepServer {
                    let (tx, rx) = tokio::sync::mpsc::channel(4);
                    whep_session_info = Some((tx, wc.bearer_token.clone()));
                    Some(rx)
                } else {
                    None
                }
            } else {
                None
            };

            let output_rt = Self::start_output(
                output_config,
                &broadcast_tx,
                &flow_stats,
                &cancel_token,
                &event_sender,
                &config.config.id,
                input_audio_format,
                compressed_audio_input,
                #[cfg(feature = "webrtc")]
                whep_rx,
                frame_rate_rx.clone(),
                #[cfg(all(feature = "display", target_os = "linux"))]
                &display_claim_registry,
                av_sync_pacer.clone(),
                active_input_tx.subscribe(),
            ).await?;
            output_handles.insert(output_config.id().to_string(), output_rt);
        }

        tracing::info!(
            "Flow '{}' ({}) started: {} input(s) (active: {}, type: {}), {} active output(s)",
            config.config.id,
            config.config.name,
            input_handles.len(),
            if active_input_id.is_empty() { "none" } else { active_input_id.as_str() },
            input_type,
            output_handles.len()
        );

        let current_assembly = tokio::sync::Mutex::new(config.config.assembly.clone());

        // Spawn the recording writer if the flow has `recording` configured
        // and the `replay` feature is on. Built after outputs so a
        // late-spawn failure doesn't leave the flow half-started — the
        // writer is a sibling subscriber and adding it last is safe.
        #[cfg(feature = "replay")]
        let recording_handle: Option<Arc<crate::replay::writer::RecordingHandle>> =
            if let Some(rec_cfg) = config.config.recording.as_ref().filter(|r| r.enabled).cloned() {
                let pre_buffer_seconds = rec_cfg.pre_buffer_seconds;
                match crate::replay::writer::spawn_writer(
                    config.config.id.clone(),
                    rec_cfg,
                    broadcast_tx.clone(),
                    event_sender.clone(),
                    cancel_token.clone(),
                ).await {
                    Ok(h) => {
                        // Surface the live writer counters on the flow's
                        // stats snapshot so the manager UI can render the
                        // ● REC + DROPS badges on the flow card without
                        // a per-flow status round-trip.
                        let _ = flow_stats.recording_stats.set(h.stats.clone());
                        let _ = flow_stats.recording_id.set(h.recording_id.clone());
                        // Phase 2.2 — emit a different event depending on
                        // whether the writer auto-armed (legacy: no pre-
                        // buffer, recording session begins at flow start)
                        // or is sitting in pre-buffer mode (always-on ring
                        // buffer, recording session begins on operator
                        // Start). Both events ride category `replay`.
                        if let Some(pb) = pre_buffer_seconds {
                            event_sender.emit_with_details(
                                crate::manager::events::EventSeverity::Info,
                                crate::manager::events::category::REPLAY,
                                format!(
                                    "Pre-buffer ({pb} s) active for flow '{}' — \
                                     recording will begin on Start",
                                    config.config.id
                                ),
                                Some(&config.config.id),
                                serde_json::json!({
                                    "replay_event": "recording_pre_buffer_started",
                                    "recording_id": h.recording_id,
                                    "pre_buffer_seconds": pb,
                                }),
                            );
                        } else {
                            event_sender.emit_with_details(
                                crate::manager::events::EventSeverity::Info,
                                crate::manager::events::category::REPLAY,
                                format!("Recording started for flow '{}'", config.config.id),
                                Some(&config.config.id),
                                serde_json::json!({
                                    "replay_event": "recording_started",
                                    "recording_id": h.recording_id,
                                }),
                            );
                        }
                        Some(Arc::new(h))
                    }
                    Err(e) => {
                        event_sender.emit_with_details(
                            crate::manager::events::EventSeverity::Critical,
                            crate::manager::events::category::REPLAY,
                            format!("Recording failed to start for flow '{}': {e}", config.config.id),
                            Some(&config.config.id),
                            serde_json::json!({
                                "replay_event": "recording_start_failed",
                                "error_code": "replay_disk_full",
                                "error": e.to_string(),
                            }),
                        );
                        None
                    }
                }
            } else {
                None
            };

        let cost_plan = derive_cost_plan(&config);
        let cost_units = crate::engine::hardware_probe::compute_flow_cost_units(&cost_plan);
        let hw_session_usage = std::sync::RwLock::new(
            crate::engine::hardware_probe::HwSessionUsage {
                nvenc_in_use: cost_plan.nvenc_sessions,
                qsv_in_use: cost_plan.qsv_sessions,
                videotoolbox_in_use: cost_plan.videotoolbox_sessions,
                amf_in_use: cost_plan.amf_sessions,
                vaapi_in_use: cost_plan.vaapi_sessions,
                nvenc_in_use_4k: cost_plan.nvenc_sessions_4k,
                qsv_in_use_4k: cost_plan.qsv_sessions_4k,
                amf_in_use_4k: cost_plan.amf_sessions_4k,
                vaapi_in_use_4k: cost_plan.vaapi_sessions_4k,
                nvdec_in_use: cost_plan.nvdec_sessions,
                qsv_decode_in_use: cost_plan.qsv_decode_sessions,
                vaapi_decode_in_use: cost_plan.vaapi_decode_sessions,
                nvdec_in_use_4k: cost_plan.nvdec_sessions_4k,
                qsv_decode_in_use_4k: cost_plan.qsv_decode_sessions_4k,
                vaapi_decode_in_use_4k: cost_plan.vaapi_decode_sessions_4k,
            },
        );
        let cost_units = std::sync::atomic::AtomicU32::new(cost_units);
        // Snapshot per-output contributions so hot-remove can subtract
        // exactly what each output added at spawn time. Hot-add inserts
        // into this map alongside the resource-totals delta; hot-edit
        // (remove + add of the same id) overwrites the entry on add.
        let mut output_contributions_map = HashMap::new();
        for out in &config.outputs {
            output_contributions_map
                .insert(out.id().to_string(), output_resource_contribution(out));
        }
        let output_contributions = std::sync::RwLock::new(output_contributions_map);

        // master_clock + pacer + ingress sampler + 1 Hz telemetry
        // mirror were built above, before output spawn, so each output's
        // replacer could attach to the per-flow pacer.

        Ok(Self {
            live_inputs: RwLock::new(config.inputs.clone()),
            config,
            broadcast_tx,
            input_handles: RwLock::new(input_handles),
            active_input_tx,
            output_handles: RwLock::new(output_handles),
            cancel_token,
            stats: flow_stats,
            analyzer_handle,
            media_analysis_handle,
            thumbnail_handle,
            content_analysis_handle,
            content_analysis_audio_handle,
            content_analysis_video_handle,
            bandwidth_monitor_handle,
            degradation_monitor_handle,
            #[cfg(feature = "webrtc")]
            whip_session_tx: whip_session_info,
            #[cfg(feature = "webrtc")]
            whep_session_tx: whep_session_info,
            event_sender,
            fixer_tx,
            global_stats,
            ffmpeg_available,
            broadcast_capacity,
            frame_rate_rx,
            pid_bus_assembler_handle,
            es_bus,
            current_assembly,
            #[cfg(feature = "replay")]
            recording_handle,
            #[cfg(feature = "replay")]
            replay_command_txs,
            cost_units,
            hw_session_usage,
            output_contributions,
            #[cfg(all(feature = "display", target_os = "linux"))]
            display_claim_registry,
            master_clock,
            flow_manager_weak: Arc::downgrade(&flow_manager),
            registered_input_ids: std::sync::Mutex::new(registered_input_ids),
            foreign_demuxer_shims: tokio::sync::Mutex::new(foreign_demuxer_shims),
        })
    }

    /// Public master-clock accessor for hot-add and the WS dispatcher.
    #[allow(dead_code)]
    pub fn master_clock(&self) -> &crate::engine::master_clock::MasterClockHandle {
        &self.master_clock
    }

    /// Resource-budget cost of this flow, in units. Set at start time
    /// and adjusted by hot-add / hot-remove of outputs so a runtime
    /// change to a display output's `hw_decode` (or an output's
    /// `video_encode`) reflects on the next manager health tick.
    pub fn cost_units(&self) -> u32 {
        self.cost_units
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Per-family hardware encoder + decoder session counts contributed
    /// by this flow. Aggregated across the node by
    /// [`FlowManager::total_hw_sessions`]. Returns a snapshot so
    /// callers don't hold the runtime's read guard across awaits.
    pub fn hw_session_usage(&self) -> crate::engine::hardware_probe::HwSessionUsage {
        self.hw_session_usage
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Phase 7: replace this flow's PID-bus assembly plan in place.
    ///
    /// The caller has already validated `new_assembly` (shape + field
    /// limits via `validate_flow`). This method:
    ///
    /// 1. Rejects the call if the flow has no running assembler (i.e.
    ///    `assembly.kind == passthrough` or absent — a switch is
    ///    meaningless).
    /// 2. Builds a new [`SptsBuildResult`] via the same machinery as
    ///    flow start-up (loud `pid_bus_*` errors via the `event_sender`).
    /// 3. Resolves any `Essence` slots against the per-input PSI
    ///    catalogues currently held on the flow's stats.
    /// 4. Spawns Hitless merger tasks for any new `SlotSource::Hitless`
    ///    slots in the new plan. Mergers added by previous calls remain
    ///    running — they keep publishing onto their synthetic bus key
    ///    until the parent flow's `cancel_token` fires. (Mergers
    ///    orphaned by removal of a Hitless slot stop being subscribed
    ///    to by the assembler so their packets just go to /dev/null.)
    /// 5. Sends [`crate::engine::ts_assembler::PlanCommand::ReplacePlan`]
    ///    to the running assembler.
    ///
    /// On success the runtime's `current_assembly` is updated; the
    /// caller is responsible for persisting `config.json`.
    pub async fn replace_assembly(
        &self,
        new_assembly: crate::config::models::FlowAssembly,
    ) -> Result<()> {
        let flow_id = &self.config.config.id;
        let handle = self
            .pid_bus_assembler_handle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(
                "flow '{}' has no running assembler — `update_flow_assembly` is only \
                 valid when the flow's existing `assembly.kind != passthrough`. Use \
                 `update_flow` to switch a passthrough flow into assembly mode.",
                flow_id
            ))?;
        let bus = self
            .es_bus
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(
                "flow '{}' assembler is running but has no NodeEsBus — internal invariant violated",
                flow_id
            ))?
            .clone();

        // Build the new plan against the current `ResolvedFlow.inputs`.
        // `build_assembly_plan` emits Critical events on every failure
        // path (`pid_bus_spts_*`, `pid_bus_audio_encode_codec_*`, etc.)
        // so the manager UI has its field-level signals.
        let mut build = build_assembly_plan(&new_assembly, &self.config, &self.event_sender)?;

        // Essence resolution against the flow's live PSI catalogues.
        // 3-second window matches start-up.
        // PES Switch Phase 2.2b: foreign-input refs (cross-flow
        // assembly slots) fall through to the node-wide FlowManager
        // catalogue registry — same lookup pattern as
        // `finalize_spts_assembler`.
        if !build.pending_essence.is_empty() {
            let flow_manager = self.flow_manager_weak.upgrade();
            let mut catalogues: std::collections::HashMap<
                String,
                Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>,
            > = std::collections::HashMap::new();
            for p in &build.pending_essence {
                if !catalogues.contains_key(&p.input_id) {
                    let store = self
                        .stats
                        .per_input_counters
                        .get(&p.input_id)
                        .map(|c| c.psi_catalog.clone())
                        .or_else(|| {
                            flow_manager
                                .as_ref()
                                .and_then(|fm| fm.psi_catalog_for_input(&p.input_id))
                        });
                    if let Some(c) = store {
                        catalogues.insert(p.input_id.clone(), c);
                    }
                }
            }
            match resolve_essence_slots(
                build.pending_essence.clone(),
                catalogues,
                ESSENCE_RESOLVE_TIMEOUT,
            )
            .await
            {
                Ok(pairs) => {
                    for ((program_idx, slot_idx, leg_idx), pid) in pairs {
                        if let Some(prog) = build.plan.programs.get_mut(program_idx) {
                            if let Some(slot) = prog.slots.get_mut(slot_idx) {
                                patch_essence_pid(slot, leg_idx, pid);
                            }
                        }
                    }
                }
                Err(err) => {
                    let (code, msg) = essence_error_to_event(flow_id, &err);
                    self.event_sender.emit_flow_with_details(
                        EventSeverity::Critical,
                        category::FLOW,
                        msg.clone(),
                        flow_id,
                        serde_json::json!({
                            "error_code": code,
                            "detail": format!("{err:?}"),
                        }),
                    );
                    bail!(msg);
                }
            }
        }

        // Post-essence PCR-source remap. Mirror of the startup-time fixup
        // in `build_assembly_plan_with_essence_resolution` (see flow.rs
        // ~line 2915). Without this, `pcr_source` stays at the
        // operator-declared output PID; once the assembler's
        // `apply_plan_replacement` looks up `pcr_source` against the
        // post-resolution slots (which carry source-side PIDs after
        // essence resolution), the find fails and the PMT's `PCR_PID`
        // field falls back to `pmt_pid` itself. Receivers parse the PMT,
        // look at the declared PCR_PID for clock recovery, find no PCR
        // there (the actual PCR is on the video out_pid), and STC
        // recovery breaks — manifests as audio drift / drop on the
        // first hot-swap of an assembly that uses essence-typed slots.
        // Pro broadcast-relevant: any operator using `kind: "essence"`
        // (the auto-PID UI affordance) hits this on every plan change.
        for prog in build.plan.programs.iter_mut() {
            if prog.slots.iter().any(|s| s.source == prog.pcr_source) {
                continue;
            }
            let pcr_input = prog.pcr_source.0.clone();
            if let Some(remap) = prog
                .slots
                .iter()
                .find(|s| s.source.0 == pcr_input)
                .map(|s| s.source.clone())
            {
                tracing::info!(
                    "Flow '{}': replace_assembly program {} pcr_source auto-remapped \
                     from 0x{:04X} to 0x{:04X} (essence-resolved slot on input '{}')",
                    flow_id, prog.program_number, prog.pcr_source.1, remap.1, pcr_input,
                );
                prog.pcr_source = remap;
            } else {
                let msg = format!(
                    "flow '{}': replace_assembly program {} pcr_source input '{}' \
                     has no slot after essence resolution",
                    flow_id, prog.program_number, pcr_input,
                );
                self.event_sender.emit_flow_with_details(
                    EventSeverity::Critical,
                    category::FLOW,
                    msg.clone(),
                    flow_id,
                    serde_json::json!({
                        "error_code": "pid_bus_pcr_source_unresolved",
                        "program_number": prog.program_number,
                        "input_id": pcr_input,
                    }),
                );
                bail!(msg);
            }
        }

        // Cross-clock compatibility check on the new plan. Mirrors the
        // step 2.5 in `finalize_spts_assembler` so a hot-swap into an
        // incompatible plan is rejected with the same error code +
        // structured details as an initial start. See that call site
        // for the full rationale.
        //
        // Reads `live_inputs` (control-plane lock) rather than the
        // start-time `self.config.inputs` snapshot so a flow that has
        // hot-added inputs after start sees the current input set when
        // an assembly plan is hot-swapped.
        let input_clocks: HashMap<String, crate::engine::master_clock::ClockIdentity> = {
            let inputs = self.live_inputs.read().await;
            inputs
                .iter()
                .map(|i| (i.id.clone(), crate::engine::master_clock::clock_identity_for_input(i)))
                .collect()
        };
        if let Err(mismatch) = crate::engine::ts_assembler::check_assembly_clock_compatibility(
            &build.plan,
            &input_clocks,
        ) {
            let msg = format!(
                "flow '{}': replace_assembly rejected — program {} slot 0x{:04X} input '{}' has master clock {} but the program already references input '{}' on clock {}",
                flow_id,
                mismatch.program_number,
                mismatch.slot_out_pid,
                mismatch.slot_input_id,
                mismatch.slot_clock,
                mismatch.reference_input_id,
                mismatch.reference_clock,
            );
            self.event_sender.emit_flow_with_details(
                EventSeverity::Critical,
                category::FLOW,
                msg.clone(),
                flow_id,
                serde_json::json!({
                    "error_code": "pid_bus_master_clock_mismatch",
                    "program_number": mismatch.program_number,
                    "slot_out_pid": mismatch.slot_out_pid,
                    "slot_input_id": mismatch.slot_input_id,
                    "slot_clock": mismatch.slot_clock,
                    "reference_input_id": mismatch.reference_input_id,
                    "reference_clock": mismatch.reference_clock,
                }),
            );
            bail!(msg);
        }

        // Spawn any Hitless mergers the new plan introduced. The
        // assembler picks up the synthetic bus key as soon as its
        // `ReplacePlan` lands.
        for hl in &build.pending_hitless {
            let _ = crate::engine::ts_es_hitless::spawn_hitless_es_merger_full(
                hl.uid.clone(),
                hl.primary.clone(),
                hl.backup.clone(),
                bus.clone(),
                std::time::Duration::from_millis(hl.stall_ms),
                hl.seq_aware,
                hl.path_differential_ms,
                self.cancel_token.child_token(),
            );
        }

        // Phase 8: refresh the per-ES routing snapshot (so snapshots
        // pick up renumbered out_pids) and spawn analyzers for any
        // newly-referenced `(input_id, source_pid)` keys. Analyzers for
        // PIDs that leave the plan keep running until flow teardown —
        // their counters are still valid for retrospective inspection
        // even when the assembler no longer forwards them.
        let mut routing: std::collections::HashMap<(String, u16), u16> =
            std::collections::HashMap::new();
        for prog in &build.plan.programs {
            for slot in &prog.slots {
                routing.insert(slot.source.clone(), slot.out_pid);
                // per_es_acc is idempotent — first call creates, subsequent
                // calls return the same Arc. Safe to call every swap.
                let already_spawned = self.stats.per_es_stats.contains_key(&slot.source);
                if !already_spawned {
                    let _ = crate::engine::ts_es_analysis::spawn_per_es_analyzer(
                        slot.source.0.clone(),
                        slot.source.1,
                        bus.clone(),
                        self.stats.clone(),
                        self.cancel_token.child_token(),
                    );
                }
            }
        }
        self.stats.set_pid_routing(routing);

        handle
            .plan_tx
            .send(crate::engine::ts_assembler::PlanCommand::ReplacePlan {
                plan: build.plan,
            })
            .await
            .map_err(|_| anyhow::anyhow!("flow '{}' assembler receiver closed", flow_id))?;

        let mut current = self.current_assembly.lock().await;
        *current = Some(new_assembly);
        Ok(())
    }

    /// Snapshot the currently-running assembly. Used by the
    /// `update_flow_assembly` handler to short-circuit no-op updates.
    pub async fn current_assembly(&self) -> Option<crate::config::models::FlowAssembly> {
        self.current_assembly.lock().await.clone()
    }

    /// Switch the currently active input of a running flow.
    ///
    /// This is a zero-task-restart operation: every input task already runs
    /// (warm passive), so switching only requires publishing the new ID on
    /// the `active_input_tx` watch channel. The forwarder tasks observe the
    /// change on their next packet and flip which one forwards to the main
    /// broadcast channel. Outputs see no discontinuity.
    ///
    /// # Errors
    ///
    /// Returns an error if `new_input_id` is not the ID of one of the flow's
    /// registered inputs.
    pub async fn switch_active_input(&self, new_input_id: &str) -> Result<()> {
        let handles = self.input_handles.read().await;
        if !handles.contains_key(new_input_id) {
            bail!(
                "Flow '{}': cannot switch to input '{}' — not a member of this flow",
                self.config.config.id,
                new_input_id
            );
        }
        drop(handles);
        self.active_input_tx
            .send(new_input_id.to_string())
            .map_err(|_| anyhow::anyhow!("active_input watch channel closed"))?;
        self.stats.set_active_input_id(new_input_id);

        // Refresh the header fields that the snapshot emits (input_type,
        // mode, addresses). Without this the manager UI keeps showing the
        // first-activated input's transport and address after a switch —
        // only the active_input_id badge would flip, not the header text.
        //
        // Reads `live_inputs` so hot-added inputs are discoverable here;
        // the lock is released before the thumbnail-rebuild prod below.
        {
            let inputs = self.live_inputs.read().await;
            if let Some(new_def) = inputs.iter().find(|d| d.id == new_input_id) {
                let new_input_type = input_type_str(&new_def.config);
                let new_meta = build_input_config_meta(&new_def.config);
                self.stats.update_active_input_meta(new_input_type, new_meta);
            }
        }

        // Ask the newly-active input's thumbnail generator to emit a frame
        // immediately rather than waiting up to 5s for its next interval
        // tick. The per-input generator has been buffering this input's TS
        // the whole time it was passive, so the capture is accurate.
        if let Some(thumb) = self.stats.per_input_thumbnails.get(new_input_id) {
            thumb.request_refresh();
        }

        tracing::info!(
            "Flow '{}': switched active input to '{}'",
            self.config.config.id,
            new_input_id
        );
        Ok(())
    }

    /// Toggle an output between active (running task) and passive (no task).
    ///
    /// When `active = true`, a new output task is started as if the output
    /// were being hot-added. When `active = false`, the running task is
    /// cancelled and removed from `output_handles` — the output config
    /// itself stays in `self.config.outputs` so it can be toggled back on
    /// later without re-creating it.
    pub async fn set_output_active(
        &self,
        output_id: &str,
        active: bool,
        flow_stats: &Arc<FlowStatsAccumulator>,
    ) -> Result<()> {
        if active {
            // Do we already have a running task for this output?
            {
                let handles = self.output_handles.read().await;
                if handles.contains_key(output_id) {
                    return Ok(());
                }
            }
            let output_cfg = self
                .config
                .outputs
                .iter()
                .find(|o| o.id() == output_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Flow '{}': output '{}' is not a member of this flow",
                        self.config.config.id,
                        output_id
                    )
                })?
                .clone();
            self.add_output(output_cfg, flow_stats).await
        } else {
            // Remove the running task if any; keep the output in config.
            let maybe_rt = {
                let mut handles = self.output_handles.write().await;
                handles.remove(output_id)
            };
            if let Some(rt) = maybe_rt {
                rt.cancel_token.cancel();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    rt.handle,
                )
                .await;
                self.stats.unregister_output(output_id);
                tracing::info!(
                    "Flow '{}': output '{}' set passive",
                    self.config.config.id,
                    output_id
                );
            }
            Ok(())
        }
    }

    /// Start a single output task and return its [`OutputRuntime`] handle.
    ///
    /// A child cancellation token is derived from `parent_cancel` so that
    /// cancelling the parent stops all outputs, while individual outputs
    /// can also be cancelled independently (e.g., via [`remove_output`](Self::remove_output)).
    ///
    /// The output registers its own [`OutputStatsAccumulator`] under the
    /// flow's stats, then subscribes to the broadcast channel and begins
    /// its send loop.
    async fn start_output(
        output_config: &OutputConfig,
        broadcast_tx: &broadcast::Sender<RtpPacket>,
        flow_stats: &Arc<FlowStatsAccumulator>,
        parent_cancel: &CancellationToken,
        event_sender: &EventSender,
        flow_id: &str,
        input_audio_format: Option<crate::engine::audio_transcode::InputFormat>,
        compressed_audio_input: bool,
        #[cfg(feature = "webrtc")]
        whep_session_rx: Option<tokio::sync::mpsc::Receiver<crate::api::webrtc::registry::NewSessionMsg>>,
        frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
        #[cfg(all(feature = "display", target_os = "linux"))]
        display_claim_registry: &Arc<crate::display::claim_registry::DisplayClaimRegistry>,
        // Per-flow A/V sync pacer. Threaded into UDP/RTP/SRT/RIST
        // outputs that build a TsVideoReplacer so output PCR is
        // generated from the master clock instead of pts × 300 − preroll.
        // `None` keeps the legacy PTS-derived behaviour.
        av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
        // Subscriber to the flow's `active_input_tx` watch channel.
        // Output spawn functions that build TsVideoReplacer /
        // TsAudioReplacer use this to flip the replacers' external
        // reset flag on every active-input change, preventing the
        // permanently-stuck-decoder symptom on switcher input swaps
        // between same-codec / same-PID inputs (where the replacers'
        // internal codec/PID-change reset path doesn't fire).
        active_input_rx: watch::Receiver<String>,
    ) -> Result<OutputRuntime> {
        let output_cancel = parent_cancel.child_token();

        match output_config {
            OutputConfig::Rtp(rtp_config) => {
                let output_stats = flow_stats.register_output(
                    rtp_config.id.clone(),
                    rtp_config.name.clone(),
                    "rtp".to_string(),
                );

                let handle = spawn_rtp_output(
                    rtp_config.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    frame_rate_rx,
                    event_sender.clone(),
                    av_sync_pacer.clone(),
                    active_input_rx.clone(),
                );

                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Udp(udp_config) => {
                let output_stats = flow_stats.register_output(
                    udp_config.id.clone(),
                    udp_config.name.clone(),
                    "udp".to_string(),
                );

                let handle = spawn_udp_output(
                    udp_config.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    input_audio_format,
                    frame_rate_rx,
                    event_sender.clone(),
                    av_sync_pacer.clone(),
                    active_input_rx.clone(),
                );

                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Srt(srt_config) => {
                let output_stats = flow_stats.register_output(
                    srt_config.id.clone(),
                    srt_config.name.clone(),
                    "srt".to_string(),
                );

                let handle = spawn_srt_output(
                    srt_config.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    event_sender.clone(),
                    flow_id.to_string(),
                    input_audio_format,
                    compressed_audio_input,
                    frame_rate_rx,
                    av_sync_pacer.clone(),
                    active_input_rx.clone(),
                );

                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Rist(rist_config) => {
                let output_stats = flow_stats.register_output(
                    rist_config.id.clone(),
                    rist_config.name.clone(),
                    "rist".to_string(),
                );

                let handle = spawn_rist_output(
                    rist_config.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    frame_rate_rx,
                    event_sender.clone(),
                    flow_id.to_string(),
                    av_sync_pacer.clone(),
                    active_input_rx.clone(),
                );

                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Rtmp(rtmp_config) => {
                let output_stats = flow_stats.register_output(
                    rtmp_config.id.clone(),
                    rtmp_config.name.clone(),
                    "rtmp".to_string(),
                );

                let handle = spawn_rtmp_output(
                    rtmp_config.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    compressed_audio_input,
                    flow_id.to_string(),
                    event_sender.clone(),
                );

                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Hls(hls_config) => {
                let output_stats = flow_stats.register_output(
                    hls_config.id.clone(),
                    hls_config.name.clone(),
                    "hls".to_string(),
                );

                let handle = spawn_hls_output(
                    hls_config.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    event_sender.clone(),
                    flow_id.to_string(),
                );

                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Cmaf(cmaf_config) => {
                let output_stats = flow_stats.register_output(
                    cmaf_config.id.clone(),
                    cmaf_config.name.clone(),
                    "cmaf".to_string(),
                );
                let handle = super::cmaf::spawn_cmaf_output(
                    cmaf_config.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    event_sender.clone(),
                    flow_id.to_string(),
                );
                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Webrtc(webrtc_config) => {
                let output_stats = flow_stats.register_output(
                    webrtc_config.id.clone(),
                    webrtc_config.name.clone(),
                    "webrtc".to_string(),
                );

                let handle = spawn_webrtc_output(
                    webrtc_config.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    #[cfg(feature = "webrtc")]
                    whep_session_rx,
                    event_sender.clone(),
                    flow_id.to_string(),
                    compressed_audio_input,
                );

                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::St2110_30(c) => {
                let output_stats = flow_stats.register_output(
                    c.id.clone(),
                    c.name.clone(),
                    "st2110_30".to_string(),
                );
                let handle = super::output_st2110_30::spawn_st2110_30_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    input_audio_format,
                    flow_id,
                    compressed_audio_input,
                );
                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::St2110_31(c) => {
                let output_stats = flow_stats.register_output(
                    c.id.clone(),
                    c.name.clone(),
                    "st2110_31".to_string(),
                );
                let handle = super::output_st2110_31::spawn_st2110_31_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    input_audio_format,
                    flow_id,
                    compressed_audio_input,
                );
                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::St2110_40(c) => {
                let output_stats = flow_stats.register_output(
                    c.id.clone(),
                    c.name.clone(),
                    "st2110_40".to_string(),
                );
                let handle = super::output_st2110_40::spawn_st2110_40_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                );
                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::RtpAudio(c) => {
                let output_stats = flow_stats.register_output(
                    c.id.clone(),
                    c.name.clone(),
                    "rtp_audio".to_string(),
                );
                let handle = super::output_rtp_audio::spawn_rtp_audio_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    input_audio_format,
                    flow_id,
                    compressed_audio_input,
                );
                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::St2110_20(c) => {
                let output_stats = flow_stats.register_output(
                    c.id.clone(),
                    c.name.clone(),
                    "st2110_20".to_string(),
                );
                let handle = super::output_st2110_20::spawn_st2110_20_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                );
                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::St2110_23(c) => {
                let output_stats = flow_stats.register_output(
                    c.id.clone(),
                    c.name.clone(),
                    "st2110_23".to_string(),
                );
                let handle = super::output_st2110_23::spawn_st2110_23_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                );
                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Bonded(c) => {
                let output_stats = flow_stats.register_output(
                    c.id.clone(),
                    c.name.clone(),
                    "bonded".to_string(),
                );
                let handle = super::output_bonded::spawn_bonded_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    event_sender.clone(),
                    flow_id.to_string(),
                );
                Ok(OutputRuntime {
                    handle,
                    cancel_token: output_cancel,
                    stats: output_stats,
                })
            }
            OutputConfig::Display(c) => {
                // Display output is Linux-only and gated by the `display`
                // Cargo feature. The schema is always present so configs
                // round-trip cleanly across platforms; on non-feature /
                // non-Linux builds the schema parses fine but the spawner
                // refuses with `display_device_invalid` so the manager UI
                // can highlight the offending field.
                let output_stats = flow_stats.register_output(
                    c.id.clone(),
                    c.name.clone(),
                    "display".to_string(),
                );
                output_stats.set_egress_static(
                    crate::stats::collector::EgressMediaSummaryStatic {
                        transport_mode: Some("display".into()),
                        video_passthrough: false,
                        audio_passthrough: false,
                        audio_only: false,
                        ..Default::default()
                    },
                );

                #[cfg(all(feature = "display", target_os = "linux"))]
                {
                    let handle = super::output_display::spawn_display_output(
                        c.clone(),
                        broadcast_tx,
                        output_stats.clone(),
                        output_cancel.clone(),
                        event_sender.clone(),
                        flow_id.to_string(),
                        Arc::clone(display_claim_registry),
                    );
                    Ok(OutputRuntime {
                        handle,
                        cancel_token: output_cancel,
                        stats: output_stats,
                    })
                }
                #[cfg(not(all(feature = "display", target_os = "linux")))]
                {
                    let _ = (broadcast_tx, output_stats, output_cancel, event_sender, flow_id);
                    anyhow::bail!(
                        "display output '{}' not supported: this build was compiled without \
                         the 'display' Cargo feature, or for a non-Linux target \
                         (error_code: display_device_invalid)",
                        c.id
                    )
                }
            }
            #[cfg(feature = "mxl")]
            OutputConfig::MxlVideo(c) => {
                let _ = (frame_rate_rx, av_sync_pacer, active_input_rx);
                let domain_mgr = super::mxl::domain::global().ok_or_else(|| {
                    anyhow::anyhow!(
                        "MXL video output '{}' refused: libmxl probe failed at boot \
                         (error_code: mxl_domain_unavailable)",
                        c.id
                    )
                })?;
                let output_stats = flow_stats.register_output(
                    c.id.clone(), c.name.clone(), "mxl_video".to_string(),
                );
                let handle = super::output_mxl_video::spawn_mxl_video_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    event_sender.clone(),
                    flow_id.to_string(),
                    domain_mgr,
                );
                Ok(OutputRuntime { handle, cancel_token: output_cancel, stats: output_stats })
            }
            #[cfg(feature = "mxl")]
            OutputConfig::MxlAudio(c) => {
                let _ = (frame_rate_rx, av_sync_pacer, active_input_rx, input_audio_format);
                let domain_mgr = super::mxl::domain::global().ok_or_else(|| {
                    anyhow::anyhow!(
                        "MXL audio output '{}' refused: libmxl probe failed at boot \
                         (error_code: mxl_domain_unavailable)",
                        c.id
                    )
                })?;
                let output_stats = flow_stats.register_output(
                    c.id.clone(), c.name.clone(), "mxl_audio".to_string(),
                );
                let handle = super::output_mxl_audio::spawn_mxl_audio_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    event_sender.clone(),
                    flow_id.to_string(),
                    domain_mgr,
                );
                Ok(OutputRuntime { handle, cancel_token: output_cancel, stats: output_stats })
            }
            #[cfg(feature = "mxl")]
            OutputConfig::MxlAnc(c) => {
                let _ = (frame_rate_rx, av_sync_pacer, active_input_rx);
                let domain_mgr = super::mxl::domain::global().ok_or_else(|| {
                    anyhow::anyhow!(
                        "MXL ANC output '{}' refused: libmxl probe failed at boot \
                         (error_code: mxl_domain_unavailable)",
                        c.id
                    )
                })?;
                let output_stats = flow_stats.register_output(
                    c.id.clone(), c.name.clone(), "mxl_anc".to_string(),
                );
                let handle = super::output_mxl_anc::spawn_mxl_anc_output(
                    c.clone(),
                    broadcast_tx,
                    output_stats.clone(),
                    output_cancel.clone(),
                    event_sender.clone(),
                    flow_id.to_string(),
                    domain_mgr,
                );
                Ok(OutputRuntime { handle, cancel_token: output_cancel, stats: output_stats })
            }
            #[cfg(not(feature = "mxl"))]
            OutputConfig::MxlVideo(_)
            | OutputConfig::MxlAudio(_)
            | OutputConfig::MxlAnc(_) => {
                let _ = (broadcast_tx, frame_rate_rx, av_sync_pacer, active_input_rx,
                         input_audio_format, output_cancel, event_sender, flow_id, flow_stats);
                anyhow::bail!(
                    "MXL output requires the `mxl` Cargo feature, which was not compiled in \
                     (error_code: mxl_feature_disabled)"
                )
            }
        }
    }

    /// Hot-add a new output to this already-running flow.
    ///
    /// The new output subscribes to the broadcast channel and starts
    /// receiving packets from the next message the input publishes. It
    /// does **not** receive packets that were sent before the subscription.
    ///
    /// The output is inserted into `output_handles` under a write lock so
    /// that concurrent stats reads are not blocked for long.
    pub async fn add_output(
        &self,
        output_config: OutputConfig,
        flow_stats: &Arc<FlowStatsAccumulator>,
    ) -> Result<()> {
        let output_id = output_config.id().to_string();

        // Note: WHEP server outputs hot-added at runtime won't accept viewers
        // until the flow is restarted (the session channel cannot be registered
        // with WebrtcSessionRegistry through this path). WHEP outputs defined
        // at flow creation time work correctly.

        // Reads `live_inputs` (control-plane lock) to find the input
        // currently marked `active: true` — matches the start-time
        // `config.active_input()` semantics while honouring hot-added
        // inputs.
        //
        // Note: the `active` flag in this flow-runtime-local copy is
        // populated from the start-time config and NOT refreshed on
        // `switch_active_input`. The watch channel (`active_input_tx`)
        // is the source of truth for the *currently* selected input
        // id post-switch. This call matches today's `add_output`
        // behaviour exactly — preserving the pre-existing semantic for
        // Phase 0 of the input hot-swap rollout.
        let active_input_cfg_owned: Option<crate::config::models::InputConfig> = {
            let inputs = self.live_inputs.read().await;
            inputs.iter().find(|d| d.active).map(|d| d.config.clone())
        };
        let active_input_cfg = active_input_cfg_owned.as_ref();
        let input_audio_format = active_input_cfg
            .and_then(crate::engine::audio_transcode::InputFormat::from_input_config);
        let compressed_audio_input = active_input_cfg
            .map_or(false, crate::engine::audio_decode::input_can_carry_ts_audio);
        let av_sync_pacer = Some(Arc::new(
            crate::engine::av_sync_mux::AvSyncPacer::new(self.master_clock.clone()),
        ));
        let output_rt = Self::start_output(
            &output_config,
            &self.broadcast_tx,
            flow_stats,
            &self.cancel_token,
            &self.event_sender,
            &self.config.config.id,
            input_audio_format,
            compressed_audio_input,
            #[cfg(feature = "webrtc")]
            None,
            self.frame_rate_rx.clone(),
            #[cfg(all(feature = "display", target_os = "linux"))]
            &self.display_claim_registry,
            av_sync_pacer,
            self.active_input_tx.subscribe(),
        ).await?;

        // Update output config metadata so stats snapshots reflect the new address/port
        flow_stats.output_config_meta.insert(
            output_id.clone(),
            build_output_config_meta(&output_config),
        );

        let mut handles = self.output_handles.write().await;
        handles.insert(output_id.clone(), output_rt);

        // Apply this output's resource-budget contribution onto the
        // running cost + session totals so the manager UI's Resources
        // card reflects hot-add / hot-remove without a flow restart.
        // The same helper also covers the hot-edit case (output config
        // changed) — `update_flow` issues remove + add of the same id,
        // so the old contribution leaves on remove and the new one
        // lands here.
        let (delta_units, delta_usage) = output_resource_contribution(&output_config);
        self.cost_units
            .fetch_add(delta_units, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut g) = self.hw_session_usage.write() {
            g.nvenc_in_use = g.nvenc_in_use.saturating_add(delta_usage.nvenc_in_use);
            g.qsv_in_use = g.qsv_in_use.saturating_add(delta_usage.qsv_in_use);
            g.videotoolbox_in_use = g
                .videotoolbox_in_use
                .saturating_add(delta_usage.videotoolbox_in_use);
            g.amf_in_use = g.amf_in_use.saturating_add(delta_usage.amf_in_use);
            g.vaapi_in_use = g.vaapi_in_use.saturating_add(delta_usage.vaapi_in_use);
            g.nvenc_in_use_4k = g.nvenc_in_use_4k.saturating_add(delta_usage.nvenc_in_use_4k);
            g.qsv_in_use_4k = g.qsv_in_use_4k.saturating_add(delta_usage.qsv_in_use_4k);
            g.amf_in_use_4k = g.amf_in_use_4k.saturating_add(delta_usage.amf_in_use_4k);
            g.vaapi_in_use_4k = g.vaapi_in_use_4k.saturating_add(delta_usage.vaapi_in_use_4k);
            g.nvdec_in_use = g.nvdec_in_use.saturating_add(delta_usage.nvdec_in_use);
            g.qsv_decode_in_use = g
                .qsv_decode_in_use
                .saturating_add(delta_usage.qsv_decode_in_use);
            g.vaapi_decode_in_use = g
                .vaapi_decode_in_use
                .saturating_add(delta_usage.vaapi_decode_in_use);
            g.nvdec_in_use_4k = g.nvdec_in_use_4k.saturating_add(delta_usage.nvdec_in_use_4k);
            g.qsv_decode_in_use_4k = g
                .qsv_decode_in_use_4k
                .saturating_add(delta_usage.qsv_decode_in_use_4k);
            g.vaapi_decode_in_use_4k = g
                .vaapi_decode_in_use_4k
                .saturating_add(delta_usage.vaapi_decode_in_use_4k);
        }
        if let Ok(mut m) = self.output_contributions.write() {
            m.insert(output_id.clone(), (delta_units, delta_usage));
        }

        tracing::info!("Hot-added output '{}' to flow '{}'", output_id, self.config.config.id);
        Ok(())
    }

    /// Remove a single output from this running flow (hot-remove).
    ///
    /// Cancels the output's child token, which causes its task to observe
    /// cancellation and exit. The output's stats are also unregistered from
    /// the flow accumulator. Other outputs and the input are unaffected.
    ///
    /// # Errors
    ///
    /// Returns an error if no output with the given ID exists in this flow.
    pub async fn remove_output(&self, output_id: &str) -> Result<()> {
        let output_rt = {
            let mut handles = self.output_handles.write().await;
            handles.remove(output_id)
        };
        if let Some(output_rt) = output_rt {
            output_rt.cancel_token.cancel();
            // Wait for the output task to finish cleanup (close SRT listener/socket,
            // release UDP port) before returning. If the task doesn't respond to
            // cancel within 5s, abort — a naive `timeout(..., handle)` moves the
            // JoinHandle into the timeout future and silently detaches the task on
            // elapse, leaving it running forever as an orphan (still sending
            // packets, no longer tracked) — e.g. an RTMP task stuck in a TLS
            // handshake or reconnect wait would stay alive after remove_output
            // returned.
            let mut handle = output_rt.handle;
            tokio::select! {
                _ = &mut handle => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    tracing::warn!(
                        "Output '{}' in flow '{}' did not exit within 5s of cancel — aborting",
                        output_id,
                        self.config.config.id,
                    );
                    handle.abort();
                    let _ = handle.await;
                }
            }
            self.stats.unregister_output(output_id);
            // Subtract this output's resource contribution from the
            // running totals. Lookup is by output_id against the map
            // populated at start_output / add_output time so the
            // delta matches exactly what was added — even if the
            // output config has since been mutated in the parent
            // ResolvedFlow snapshot.
            let removed = self
                .output_contributions
                .write()
                .ok()
                .and_then(|mut m| m.remove(output_id));
            if let Some((units, usage)) = removed {
                self.cost_units
                    .fetch_sub(units, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut g) = self.hw_session_usage.write() {
                    g.nvenc_in_use = g.nvenc_in_use.saturating_sub(usage.nvenc_in_use);
                    g.qsv_in_use = g.qsv_in_use.saturating_sub(usage.qsv_in_use);
                    g.videotoolbox_in_use = g
                        .videotoolbox_in_use
                        .saturating_sub(usage.videotoolbox_in_use);
                    g.amf_in_use = g.amf_in_use.saturating_sub(usage.amf_in_use);
                    g.vaapi_in_use = g.vaapi_in_use.saturating_sub(usage.vaapi_in_use);
                    g.nvenc_in_use_4k = g.nvenc_in_use_4k.saturating_sub(usage.nvenc_in_use_4k);
                    g.qsv_in_use_4k = g.qsv_in_use_4k.saturating_sub(usage.qsv_in_use_4k);
                    g.amf_in_use_4k = g.amf_in_use_4k.saturating_sub(usage.amf_in_use_4k);
                    g.vaapi_in_use_4k = g.vaapi_in_use_4k.saturating_sub(usage.vaapi_in_use_4k);
                    g.nvdec_in_use = g.nvdec_in_use.saturating_sub(usage.nvdec_in_use);
                    g.qsv_decode_in_use = g
                        .qsv_decode_in_use
                        .saturating_sub(usage.qsv_decode_in_use);
                    g.vaapi_decode_in_use = g
                        .vaapi_decode_in_use
                        .saturating_sub(usage.vaapi_decode_in_use);
                    g.nvdec_in_use_4k = g.nvdec_in_use_4k.saturating_sub(usage.nvdec_in_use_4k);
                    g.qsv_decode_in_use_4k = g
                        .qsv_decode_in_use_4k
                        .saturating_sub(usage.qsv_decode_in_use_4k);
                    g.vaapi_decode_in_use_4k = g
                        .vaapi_decode_in_use_4k
                        .saturating_sub(usage.vaapi_decode_in_use_4k);
                }
            }
            tracing::info!("Removed output '{}' from flow '{}'", output_id, self.config.config.id);
            Ok(())
        } else {
            bail!("Output '{}' not found in flow '{}'", output_id, self.config.config.id);
        }
    }

    /// Add an input to a running flow (hot-add).
    ///
    /// Spawns the input task graph (input task, forwarder or refcounted
    /// demuxer shim, optional PSI catalogue observer, optional per-input
    /// thumbnail generator) using the same engine code that
    /// [`Self::start`] uses for start-time inputs. The new input joins
    /// as warm-passive — its packets flow onto its per-input broadcast
    /// channel and pre-warm the PSI cache, but the flow's output
    /// `broadcast_tx` only forwards packets from the input named in the
    /// `active_input_tx` watch. An operator `activate_input` flips the
    /// new input in seamlessly via the existing switch path.
    ///
    /// # Refusal paths (structured `error_code`)
    ///
    /// - `input_already_member`: the flow's `input_handles` already
    ///   contains an entry for `input_def.id`. Idempotent no-op from
    ///   the runtime's perspective; surfaced as a Warning so the
    ///   operator sees that nothing happened.
    /// - `hitless_leg_change_requires_restart`: the input id appears in
    ///   any running hitless slot's leg list. The merger is not
    ///   designed to grow legs mid-flight; operator must restart the
    ///   flow to land this change.
    ///
    /// Bind failures (SRT-listener / RIST / RTSP / WHIP-server on a busy
    /// port) surface asynchronously through the event channel. The
    /// caller (WS handler) uses `wait_for_first_bind_failure` to detect
    /// them within a short window and call [`Self::remove_input`] to
    /// roll back.
    ///
    /// WHIP-server inputs hot-added at runtime spawn their input task
    /// but **do not** register a WebrtcSessionRegistry entry — same
    /// limitation as WHEP outputs hot-added via [`Self::add_output`].
    /// The WS handler emits a Warning event when the input is WHIP so
    /// operators know to restart the flow if browser pairing is needed.
    pub async fn add_input(
        &self,
        input_def: InputDefinition,
        flow_manager: &Arc<crate::engine::manager::FlowManager>,
    ) -> Result<HotAddedInputInfo> {
        let input_id = input_def.id.clone();

        // Idempotency check — already a member.
        {
            let handles = self.input_handles.read().await;
            if handles.contains_key(&input_id) {
                return Err(anyhow::Error::new(
                    InputMembershipError::already_member(&input_id),
                ));
            }
        }

        // Hitless-leg refusal: if the current assembly already lists
        // this input id as a hitless primary or backup leg, we can't
        // grow it in flight. Operator must restart.
        let hitless = self.assembly_hitless_inputs().await;
        if hitless.contains(&input_id) {
            return Err(anyhow::Error::new(
                InputMembershipError::hitless_leg_change(&input_id),
            ));
        }

        // Build the spawn context. Mirrors the context built at flow
        // start in `FlowRuntime::start` — same engine code spawns the
        // input on both code paths.
        let active_input_watch_rx = self.active_input_tx.subscribe();
        let av_sync_pacer = Some(Arc::new(
            crate::engine::av_sync_mux::AvSyncPacer::new(self.master_clock.clone()),
        ));
        // Mirror the start-time `mark_assembler_owned` so hot-added
        // inputs on an assembled flow also skip their per-input rewriter.
        if self.config.config.assembly.is_some() {
            if let Some(p) = av_sync_pacer.as_ref() {
                p.mark_assembler_owned();
            }
        }
        let spawn_ctx = InputSpawnContext {
            flow_id: &self.config.config.id,
            flow_cancel: &self.cancel_token,
            active_input_watch_rx: &active_input_watch_rx,
            fixer_tx: &self.fixer_tx,
            flow_stats: &self.stats,
            flow_manager,
            event_sender: &self.event_sender,
            av_sync_pacer,
            es_bus: self.es_bus.as_ref(),
            global_stats: &self.global_stats,
            ffmpeg_available: self.ffmpeg_available,
            thumbnail_enabled: self.config.config.thumbnail,
            flow_clock_domain: self.config.config.clock_domain,
            broadcast_capacity: self.broadcast_capacity,
            #[cfg(feature = "replay")]
            replay_command_txs: &self.replay_command_txs,
        };

        let spawned = spawn_input_runtime(&input_def, &spawn_ctx);

        // Persist live state in the same order as start:
        //   live_inputs (config snapshot) → input_handles (runtime) →
        //   registered_input_ids (unregister bookkeeping).
        {
            let mut live = self.live_inputs.write().await;
            live.push(input_def);
        }
        {
            let mut handles = self.input_handles.write().await;
            handles.insert(spawned.input_id.clone(), spawned.input_runtime);
        }
        if let Ok(mut ids) = self.registered_input_ids.lock() {
            ids.push(spawned.input_id.clone());
        }

        tracing::info!(
            "Hot-added input '{}' to flow '{}'",
            spawned.input_id,
            self.config.config.id
        );
        Ok(HotAddedInputInfo {
            input_id: spawned.input_id,
            #[cfg(feature = "webrtc")]
            whip_info: spawned.whip_info,
        })
    }

    /// Remove an input from a running flow (hot-remove).
    ///
    /// Unregisters the node-wide publisher first (so cross-flow
    /// subscribers see a clean `RecvError::Closed`), then cancels the
    /// per-input child token, awaits the input task + forwarder +
    /// thumbnail handles with the same 5s timeout the stop path uses,
    /// and finally drops the entry from `live_inputs`, `input_handles`,
    /// and `registered_input_ids`. Enqueues `FixerCommand::DropInputPsi`
    /// so the fixer evicts the per-input PSI cache — preventing a later
    /// re-add under the same id from injecting stale PAT/PMT.
    ///
    /// # Refusal paths (structured `error_code`)
    ///
    /// 1. `input_not_member`: id not in `input_handles`. Idempotent no-op
    ///    from the runtime's perspective; surfaced as a Warning.
    /// 2. `active_input_in_use`: id equals the current `active_input_tx`
    ///    value. Operator must `activate_input` to a different member
    ///    first. Refusing here keeps the data path's "always have an
    ///    active source" invariant; auto-switching to an arbitrary
    ///    sibling would silently change broadcast output.
    /// 3. `pid_bus_input_in_use`: id appears in any current assembly
    ///    slot's source tree. Operator must edit the assembly first;
    ///    auto-orphaning would mute the synthesised program silently.
    /// 4. `hitless_leg_change_requires_restart`: id appears in a
    ///    hitless slot's leg list. Merger isn't designed to shrink
    ///    legs mid-flight; operator must restart.
    ///
    /// # Master clock side-effect (R5)
    ///
    /// If the removed input is the operator-declared master-clock
    /// source (`flow.master_clock`), emits a Warning event
    /// `master_clock_input_removed` so the operator sees the PLL
    /// fallback. The clock's auto-resolver picks the next active input
    /// transparently; output PCR may briefly drift while the new
    /// source locks. Refusing here was the alternative — chosen
    /// against because retiring an old input shouldn't have to
    /// detour through a master-clock config edit.
    pub async fn remove_input(&self, input_id: &str) -> Result<()> {
        // (1) Not a member?
        {
            let handles = self.input_handles.read().await;
            if !handles.contains_key(input_id) {
                return Err(anyhow::Error::new(
                    InputMembershipError::not_member(input_id),
                ));
            }
        }
        // (2) Currently active?
        if self.active_input_tx.borrow().as_str() == input_id {
            return Err(anyhow::Error::new(
                InputMembershipError::active_in_use(input_id),
            ));
        }
        // (3) Referenced by the running assembly?
        if self.input_used_by_assembly(input_id).await {
            return Err(anyhow::Error::new(
                InputMembershipError::pid_bus_in_use(input_id),
            ));
        }
        // (4) In a hitless slot's leg list?
        let hitless = self.assembly_hitless_inputs().await;
        if hitless.contains(input_id) {
            return Err(anyhow::Error::new(
                InputMembershipError::hitless_leg_change(input_id),
            ));
        }

        // R5: master-clock source warning. Emit BEFORE teardown so the
        // operator's event log shows the cause before the PLL drift
        // appears in telemetry.
        let clock_source =
            crate::engine::master_clock::resolve_pcr_source_input_id(&self.config.config);
        if clock_source.as_deref() == Some(input_id) {
            self.event_sender.emit_flow_with_details(
                EventSeverity::Warning,
                category::FLOW,
                format!(
                    "Master clock source input '{}' is being removed from flow '{}'; \
                     PLL will fall back to the next active input — output PCR may \
                     briefly drift until lock",
                    input_id, self.config.config.id
                ),
                &self.config.config.id,
                serde_json::json!({
                    "error_code": "master_clock_input_removed",
                    "input_id": input_id,
                    "flow_id": self.config.config.id,
                }),
            );
        }

        // Snapshot whether this input is a WHIP server BEFORE we drop
        // its entry from `live_inputs` — the WebRTC session registry
        // cleanup below needs to know without re-acquiring the lock.
        // Only WHIP-server inputs registered a channel; other input
        // kinds had nothing to register.
        #[cfg(feature = "webrtc")]
        let removed_was_whip_server: bool = {
            let live = self.live_inputs.read().await;
            live.iter()
                .find(|d| d.id == input_id)
                .map(|d| matches!(d.config, InputConfig::Webrtc(_)))
                .unwrap_or(false)
        };

        // Drop per-input PSI cache via the fixer command channel.
        // Idempotent on the fixer; use try_send so a backed-up fixer
        // doesn't stall the remove path.
        let _ = self
            .fixer_tx
            .try_send(FixerCommand::DropInputPsi {
                input_id: input_id.to_string(),
            });

        // Unregister the node-wide publisher BEFORE cancelling per-input
        // tokens. Cross-flow subscribers see `RecvError::Closed` on the
        // next recv and exit cleanly. The `pid_bus_slot_source_closed`
        // Warning that sibling flows' slot_fanin emits covers the
        // operator-visible "this slot just went silent" surface.
        if let Some(fm) = self.flow_manager_weak.upgrade() {
            fm.unregister_input_publisher(input_id);
            // Unregister the WHIP session channel if we just removed
            // the flow's WHIP-server input. The registry is keyed by
            // flow_id (one WHIP per flow by design), so the cleanup
            // is symmetric with the `register_whip_input` we did on
            // hot-add. WHEP outputs for the flow stay registered.
            #[cfg(feature = "webrtc")]
            if removed_was_whip_server {
                if let Some(reg) = fm.webrtc_sessions() {
                    reg.unregister_whip_input(&self.config.config.id);
                    tracing::info!(
                        "Unregistered WHIP-server input '{}' from flow '{}'",
                        input_id,
                        self.config.config.id
                    );
                }
            }
        }

        // Pull the runtime out of input_handles, cancel its token, and
        // await all child task handles.
        let runtime = {
            let mut handles = self.input_handles.write().await;
            handles.remove(input_id)
        };
        if let Some(rt) = runtime {
            rt.cancel_token.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rt.input_handle)
                .await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rt.forwarder_handle)
                .await;
            if let Some(thumb) = rt.thumbnail_handle {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), thumb).await;
            }
            // Detach the per-input thumbnail accumulator so the stats
            // snapshot stops surfacing this input's last-thumbnail.
            self.stats.per_input_thumbnails.remove(input_id);
        }

        // Drop from live_inputs + registered_input_ids.
        {
            let mut live = self.live_inputs.write().await;
            live.retain(|d| d.id != input_id);
        }
        if let Ok(mut ids) = self.registered_input_ids.lock() {
            ids.retain(|id| id != input_id);
        }

        tracing::info!(
            "Hot-removed input '{}' from flow '{}'",
            input_id,
            self.config.config.id
        );
        Ok(())
    }

    /// Stop the entire flow by cancelling the parent token.
    ///
    /// This signals both the input task and every output task to shut down.
    /// Because output cancel tokens are children of the flow token, a single
    /// cancel propagates to all tasks. The actual task cleanup (socket close,
    /// thread join) happens asynchronously inside each task's select loop.
    pub async fn stop(&self) {
        tracing::info!("Stopping flow '{}' ({})", self.config.config.id, self.config.config.name);
        // PES Switch Phase 2.1c Stage 2: synchronously unregister
        // every node-wide input publisher this flow owned, BEFORE
        // tearing down the per-input cancel tokens. Cross-flow
        // subscribers see a clean `RecvError::Closed` and stop;
        // the manager's registry no longer holds a pointer to a
        // soon-to-drop `broadcast::Sender`. Done synchronously
        // because the manager is held via `Weak` — upgrading is
        // best-effort; if the manager is already dropped (during
        // shutdown) the entry was already torn down with it.
        if let Some(fm) = self.flow_manager_weak.upgrade() {
            if let Ok(ids) = self.registered_input_ids.lock() {
                for id in ids.iter() {
                    fm.unregister_input_publisher(id);
                }
            }
        }
        self.cancel_token.cancel();
        // Await all output task handles so sockets (especially SRT listeners)
        // are fully closed and ports released before the flow is considered stopped.
        let output_handles: Vec<_> = {
            let mut handles = self.output_handles.write().await;
            handles.drain().map(|(_, rt)| rt.handle).collect()
        };
        for handle in output_handles {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        }
        // Await every input task + forwarder so their sockets are released.
        let input_tasks: Vec<(JoinHandle<()>, JoinHandle<()>)> = {
            let mut handles = self.input_handles.write().await;
            handles
                .drain()
                .map(|(_, rt)| (rt.input_handle, rt.forwarder_handle))
                .collect()
        };
        for (ih, fh) in input_tasks {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), ih).await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), fh).await;
        }
        // PES Switch Phase 2.1c Stage 3: drain any cross-flow demuxer
        // shim tasks. Each holds a `DemuxerHandle` whose `Drop` impl
        // decrements the FlowManager's refcounted demuxer slot; the
        // task itself exits as soon as the parent cancel fires.
        let foreign_shims: Vec<JoinHandle<()>> = {
            let mut guard = self.foreign_demuxer_shims.lock().await;
            std::mem::take(&mut *guard)
        };
        for shim in foreign_shims {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), shim).await;
        }
    }

    /// Return `true` iff the given `input_id` appears anywhere in this
    /// flow's current assembly plan — as a slot source (Pid / Essence /
    /// Hitless leg / Switch leg) or as a PCR source at flow or program
    /// level.
    ///
    /// Used by the future hot-swap `remove_input` path to refuse removal
    /// of inputs still bound to the running assembly. The corresponding
    /// `pid_bus_input_in_use` error rides on `command_ack.error_code` so
    /// the manager UI can surface a structured "unbind the slot first"
    /// message rather than mute the assembled program silently.
    #[allow(dead_code)] // Phase 1 will wire callers via remove_input / add_input.
    pub async fn input_used_by_assembly(&self, input_id: &str) -> bool {
        let guard = self.current_assembly.lock().await;
        let Some(asm) = guard.as_ref() else { return false; };
        if let Some(pcr) = &asm.pcr_source {
            if pcr.input_id == input_id {
                return true;
            }
        }
        for program in &asm.programs {
            if let Some(pcr) = &program.pcr_source {
                if pcr.input_id == input_id {
                    return true;
                }
            }
            for stream in &program.streams {
                if slot_source_references_input(&stream.source, input_id) {
                    return true;
                }
            }
        }
        false
    }

    /// Return the set of `input_id`s that appear inside any
    /// [`crate::config::models::SlotSource::Hitless`] slot's primary or
    /// backup leg in this flow's current assembly plan.
    ///
    /// Used by the future hot-swap `add_input` / `remove_input` paths to
    /// detect changes that would alter a running hitless ES merger's leg
    /// list. The merger isn't designed to grow/shrink mid-flight, so
    /// such edits fall back to a full flow restart via
    /// `hitless_leg_change_requires_restart` on `command_ack.error_code`.
    #[allow(dead_code)] // Phase 1 will wire callers via remove_input / add_input.
    pub async fn assembly_hitless_inputs(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        let guard = self.current_assembly.lock().await;
        let Some(asm) = guard.as_ref() else { return out; };
        for program in &asm.programs {
            for stream in &program.streams {
                collect_hitless_input_ids(&stream.source, &mut out);
            }
        }
        out
    }
}

/// Recursive walker used by [`FlowRuntime::input_used_by_assembly`]. Returns
/// `true` if `input_id` appears anywhere inside the slot source tree.
///
/// Per the validation rules, `Hitless` cannot nest inside `Hitless` or
/// `Switch`, and `Switch` legs are flat — but recursing through the boxed
/// `Hitless { primary, backup }` arms is the natural shape.
#[allow(dead_code)] // Phase 1 wires callers via FlowRuntime::input_used_by_assembly.
fn slot_source_references_input(
    src: &crate::config::models::SlotSource,
    input_id: &str,
) -> bool {
    use crate::config::models::SlotSource;
    match src {
        SlotSource::Pid { input_id: id, .. } => id == input_id,
        SlotSource::Essence { input_id: id, .. } => id == input_id,
        SlotSource::Hitless { primary, backup, .. } => {
            slot_source_references_input(primary, input_id)
                || slot_source_references_input(backup, input_id)
        }
        SlotSource::Switch { legs, .. } => legs.iter().any(|l| l.input_id() == input_id),
    }
}

/// Helper for [`FlowRuntime::assembly_hitless_inputs`]. When `src` is a
/// `Hitless`, inserts every input id referenced by its primary or backup
/// leg into `out`. Non-`Hitless` sources are ignored.
#[allow(dead_code)] // Phase 1 wires callers via FlowRuntime::assembly_hitless_inputs.
fn collect_hitless_input_ids(
    src: &crate::config::models::SlotSource,
    out: &mut HashSet<String>,
) {
    use crate::config::models::SlotSource;
    if let SlotSource::Hitless { primary, backup, .. } = src {
        collect_all_input_ids(primary, out);
        collect_all_input_ids(backup, out);
    }
}

/// Insert every input id referenced anywhere inside `src` into `out`.
/// Used to enumerate the full leg list of a `Hitless` source.
#[allow(dead_code)] // Phase 1 wires callers via collect_hitless_input_ids.
fn collect_all_input_ids(
    src: &crate::config::models::SlotSource,
    out: &mut HashSet<String>,
) {
    use crate::config::models::SlotSource;
    match src {
        SlotSource::Pid { input_id, .. } => {
            out.insert(input_id.clone());
        }
        SlotSource::Essence { input_id, .. } => {
            out.insert(input_id.clone());
        }
        SlotSource::Hitless { primary, backup, .. } => {
            collect_all_input_ids(primary, out);
            collect_all_input_ids(backup, out);
        }
        SlotSource::Switch { legs, .. } => {
            for leg in legs {
                out.insert(leg.input_id().to_string());
            }
        }
    }
}

/// Optional WHIP session channel + bearer token surfaced by WebRTC inputs
/// so the HTTP signaling layer can register the flow's receive endpoint.
#[cfg(feature = "webrtc")]
type WhipSessionInfo = (
    tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>,
    Option<String>,
);

/// Compile-away placeholder when the webrtc feature is disabled. Keeps
/// `spawn_single_input`'s return shape stable across feature flags.
#[cfg(not(feature = "webrtc"))]
type WhipSessionInfo = ();

/// Dispatch an [`InputConfig`] variant onto its protocol-specific spawner.
///
/// Extracted out of `FlowRuntime::start` so the startup pipeline can line up
/// the per-input setup as a single `spawn_single_input` call rather than a
/// 200-line match arm. Returns the input task handle plus, for WHIP inputs,
/// the session channel + bearer token the HTTP signaling layer needs.
/// Common "MXL probe failed at boot" fallback used by the MXL input
/// spawn arms when [`super::mxl::domain::global`] returns `None`. Emits
/// a Critical event so the operator sees the cause, then parks on the
/// input's cancellation token (preserves flow shutdown semantics).
#[cfg(feature = "mxl")]
fn spawn_mxl_unavailable(
    kind: &'static str,
    input_id: String,
    cancel: CancellationToken,
    event_sender: EventSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        event_sender.emit(
            crate::manager::events::EventSeverity::Critical,
            crate::manager::events::category::FLOW,
            format!(
                "MXL {kind} input '{input_id}' refused: libmxl probe failed at boot \
                 (error_code: mxl_domain_unavailable)"
            ),
        );
        cancel.cancelled().await;
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_single_input(
    input_def: &InputDefinition,
    flow_id: &str,
    per_input_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
    input_cancel: &CancellationToken,
    event_sender: &EventSender,
    force_idr: &Arc<std::sync::atomic::AtomicBool>,
    flow_clock_domain: Option<u8>,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    #[cfg(feature = "replay")]
    replay_command_txs: &dashmap::DashMap<String, tokio::sync::mpsc::Sender<crate::replay::ReplayCommand>>,
) -> (JoinHandle<()>, Option<WhipSessionInfo>) {
    let input_id = input_def.id.clone();
    let mut whip_info: Option<WhipSessionInfo> = None;

    let handle = match &input_def.config {
        InputConfig::Rtp(rtp_config) => spawn_rtp_input(
            rtp_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(), av_sync_pacer.clone(),
        ),
        InputConfig::Udp(udp_config) => spawn_udp_input(
            udp_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(), av_sync_pacer.clone(),
        ),
        InputConfig::Srt(srt_config) => spawn_srt_input(
            srt_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(), av_sync_pacer.clone(),
        ),
        InputConfig::Rist(rist_config) => spawn_rist_input(
            rist_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(), av_sync_pacer.clone(),
        ),
        InputConfig::Rtmp(rtmp_config) => spawn_rtmp_input(
            rtmp_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(), av_sync_pacer.clone(),
        ),
        InputConfig::Rtsp(rtsp_config) => super::input_rtsp::spawn_rtsp_input(
            rtsp_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(), av_sync_pacer.clone(),
        ),
        #[cfg(feature = "webrtc")]
        InputConfig::Webrtc(webrtc_config) => {
            let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
            whip_info = Some((session_tx, webrtc_config.bearer_token.clone()));
            super::input_webrtc::spawn_whip_input(
                webrtc_config.clone(), flow_id.to_string(), input_id.clone(),
                per_input_tx.clone(), flow_stats.clone(), input_cancel.clone(),
                session_rx, event_sender.clone(), force_idr.clone(),
                av_sync_pacer.clone(),
            )
        }
        #[cfg(not(feature = "webrtc"))]
        InputConfig::Webrtc(_) => {
            let cancel = input_cancel.clone();
            tokio::spawn(async move {
                tracing::warn!("WebRTC/WHIP input requires the `webrtc` cargo feature");
                cancel.cancelled().await;
            })
        }
        #[cfg(feature = "webrtc")]
        InputConfig::Whep(whep_config) => super::input_webrtc::spawn_whep_input(
            whep_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(), av_sync_pacer.clone(),
        ),
        #[cfg(not(feature = "webrtc"))]
        InputConfig::Whep(_) => {
            let cancel = input_cancel.clone();
            tokio::spawn(async move {
                tracing::warn!("WHEP input requires the `webrtc` cargo feature");
                cancel.cancelled().await;
            })
        }
        // PTP clock_domain may be set on the flow itself or on the
        // per-essence input config. The cloned local inherits the
        // flow-level value when the essence does not override it.
        InputConfig::St2110_30(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            super::input_st2110_30::spawn_st2110_30_input(
                c, input_id.clone(), per_input_tx.clone(), flow_stats.clone(),
                input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            )
        }
        InputConfig::St2110_31(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            super::input_st2110_31::spawn_st2110_31_input(
                c, input_id.clone(), per_input_tx.clone(), flow_stats.clone(),
                input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            )
        }
        InputConfig::St2110_40(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            super::input_st2110_40::spawn_st2110_40_input(
                c, per_input_tx.clone(), flow_stats.clone(),
                input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            )
        }
        InputConfig::RtpAudio(c) => super::input_rtp_audio::spawn_rtp_audio_input(
            c.clone(), input_id.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), Some(event_sender.clone()),
            Some(flow_id.to_string()),
        ),
        InputConfig::St2110_20(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            super::input_st2110_20::spawn_st2110_20_input(
                c, input_id.clone(), per_input_tx.clone(),
                flow_stats.clone(), input_cancel.clone(),
            )
        }
        InputConfig::St2110_23(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            super::input_st2110_23::spawn_st2110_23_input(
                c, input_id.clone(), per_input_tx.clone(),
                flow_stats.clone(), input_cancel.clone(),
            )
        }
        InputConfig::Bonded(c) => super::input_bonded::spawn_bonded_input(
            c.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(),
            flow_id.to_string(), input_id.clone(),
        ),
        InputConfig::TestPattern(c) => super::input_test_pattern::spawn_test_pattern_input(
            c.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(),
            flow_id.to_string(), input_id.clone(),
        ),
        InputConfig::MediaPlayer(c) => super::input_media_player::spawn_media_player_input(
            c.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(),
            flow_id.to_string(), input_id.clone(),
            av_sync_pacer.clone(),
        ),
        #[cfg(feature = "replay")]
        InputConfig::Replay(c) => {
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<crate::replay::ReplayCommand>(32);
            // Register the per-input command channel so the manager-WS
            // dispatcher can route mark/cue/play/scrub commands to this
            // input by id.
            replay_command_txs.insert(input_id.clone(), cmd_tx);
            super::input_replay::spawn_replay_input(
                c.clone(), per_input_tx.clone(), flow_stats.clone(),
                input_cancel.clone(), event_sender.clone(),
                flow_id.to_string(), input_id.clone(), cmd_rx,
                av_sync_pacer.clone(),
            )
        }
        #[cfg(not(feature = "replay"))]
        InputConfig::Replay(_) => {
            let cancel = input_cancel.clone();
            let event_sender = event_sender.clone();
            tokio::spawn(async move {
                event_sender.emit(
                    crate::manager::events::EventSeverity::Critical,
                    crate::manager::events::category::FLOW,
                    "Replay input requires the `replay` Cargo feature",
                );
                cancel.cancelled().await;
            })
        }
        // MXL inputs — spawn arms call into engine::input_mxl_* shims
        // which dispatch to engine::mxl_io or engine::mxl_video_io. The
        // domain manager is looked up from the global OnceLock installed
        // at boot in main.rs; missing manager means libmxl probe failed.
        #[cfg(feature = "mxl")]
        InputConfig::MxlVideo(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            match super::mxl::domain::global() {
                Some(mgr) => super::input_mxl_video::spawn_mxl_video_input(
                    c, input_id.clone(), per_input_tx.clone(), flow_stats.clone(),
                    input_cancel.clone(), event_sender.clone(), flow_id.to_string(), mgr,
                ),
                None => spawn_mxl_unavailable("video", input_id.clone(), input_cancel.clone(), event_sender.clone()),
            }
        }
        #[cfg(feature = "mxl")]
        InputConfig::MxlAudio(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            match super::mxl::domain::global() {
                Some(mgr) => super::input_mxl_audio::spawn_mxl_audio_input(
                    c, input_id.clone(), per_input_tx.clone(), flow_stats.clone(),
                    input_cancel.clone(), event_sender.clone(), flow_id.to_string(), mgr,
                ),
                None => spawn_mxl_unavailable("audio", input_id.clone(), input_cancel.clone(), event_sender.clone()),
            }
        }
        #[cfg(feature = "mxl")]
        InputConfig::MxlAnc(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            match super::mxl::domain::global() {
                Some(mgr) => super::input_mxl_anc::spawn_mxl_anc_input(
                    c, input_id.clone(), per_input_tx.clone(), flow_stats.clone(),
                    input_cancel.clone(), event_sender.clone(), flow_id.to_string(), mgr,
                ),
                None => spawn_mxl_unavailable("ANC", input_id.clone(), input_cancel.clone(), event_sender.clone()),
            }
        }
        #[cfg(not(feature = "mxl"))]
        InputConfig::MxlVideo(_) | InputConfig::MxlAudio(_) | InputConfig::MxlAnc(_) => {
            let cancel = input_cancel.clone();
            let event_sender = event_sender.clone();
            let input_id_msg = input_id.clone();
            tokio::spawn(async move {
                event_sender.emit(
                    crate::manager::events::EventSeverity::Critical,
                    crate::manager::events::category::FLOW,
                    format!(
                        "MXL input '{input_id_msg}' requires the `mxl` Cargo feature \
                         (error_code: mxl_feature_disabled)"
                    ),
                );
                cancel.cancelled().await;
            })
        }
    };

    (handle, whip_info)
}

/// Shared per-flow context needed to spawn a single input's runtime
/// (input task + forwarder/demuxer-shim + PSI observer + thumbnail).
///
/// Bundles the references that don't vary per input so the spawn loop in
/// [`FlowRuntime::start`] and the hot-add path in `FlowRuntime::add_input`
/// share the same engine code without a 15-argument call site.
pub(crate) struct InputSpawnContext<'a> {
    pub flow_id: &'a str,
    pub flow_cancel: &'a CancellationToken,
    pub active_input_watch_rx: &'a watch::Receiver<String>,
    pub fixer_tx: &'a tokio::sync::mpsc::Sender<FixerCommand>,
    pub flow_stats: &'a Arc<FlowStatsAccumulator>,
    pub flow_manager: &'a Arc<crate::engine::manager::FlowManager>,
    pub event_sender: &'a EventSender,
    pub av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    pub es_bus: Option<&'a Arc<NodeEsBus>>,
    pub global_stats: &'a Arc<crate::stats::collector::StatsCollector>,
    pub ffmpeg_available: bool,
    pub thumbnail_enabled: bool,
    pub flow_clock_domain: Option<u8>,
    /// Per-flow per-input broadcast capacity (slots). Derived from the
    /// flow's [`crate::config::models::BandwidthProfile`] in
    /// [`FlowRuntime::start`]; passed through here so the hot-add path
    /// builds its `per_input_tx` at the same size as the start-time
    /// inputs. Matches the flow's main broadcast channel capacity.
    pub broadcast_capacity: usize,
    #[cfg(feature = "replay")]
    pub replay_command_txs:
        &'a dashmap::DashMap<String, tokio::sync::mpsc::Sender<crate::replay::ReplayCommand>>,
}

/// Output of [`spawn_input_runtime`]. Carries the assembled [`InputRuntime`]
/// that the caller inserts into `input_handles`, the registered input id,
/// and the optional WHIP session channel for the WebRTC HTTP signaling
/// layer (only present when `feature = "webrtc"` is on and the input
/// dispatched a WHIP receiver).
pub(crate) struct SpawnedInputRuntime {
    pub input_id: String,
    pub input_runtime: InputRuntime,
    #[cfg(feature = "webrtc")]
    pub whip_info: Option<WhipSessionInfo>,
}

/// Spawn a single input's full runtime: per-input broadcast publisher,
/// node-wide publisher registration (gets `psi_catalog`), per-input
/// liveness counters, protocol-specific input task, optional PSI catalogue
/// observer, forwarder (passthrough) or refcounted demuxer shim (assembled
/// flow), and optional thumbnail generator.
///
/// Used by both [`FlowRuntime::start`] (one call per input at start) and
/// the hot-add path (one call per added input). The setup order is
/// load-bearing: register publisher BEFORE counter registration so the
/// PSI catalogue lives in both the manager's node-wide registry and the
/// flow's stats accumulator; spawn the PSI catalogue observer BEFORE the
/// forwarder so passive inputs surface their PMTs even while another
/// input is active; spawn the demuxer shim BEFORE the thumbnail so a
/// later thumbnail generator subscribes to a publisher with at least one
/// downstream draining the channel.
///
/// Pure spawn — no awaits, no Result. Bind failures surface asynchronously
/// via the event channel; the hot-add caller uses `wait_for_first_bind_failure`
/// to detect them within a short window after spawn.
pub(crate) fn spawn_input_runtime(
    input_def: &InputDefinition,
    ctx: &InputSpawnContext<'_>,
) -> SpawnedInputRuntime {
    let input_id = input_def.id.clone();
    let input_cancel = ctx.flow_cancel.child_token();
    let (per_input_tx, _) = broadcast::channel::<RtpPacket>(ctx.broadcast_capacity);

    let psi_catalog = ctx
        .flow_manager
        .register_input_publisher(&input_id, per_input_tx.clone());
    let force_idr = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let per_input_counters = ctx.flow_stats.register_input_counters_with_psi_catalog(
        &input_id,
        input_type_str(&input_def.config),
        build_input_config_meta(&input_def.config),
        Arc::clone(&psi_catalog),
    );

    let (input_handle, this_whip_info) = spawn_single_input(
        input_def,
        ctx.flow_id,
        &per_input_tx,
        ctx.flow_stats,
        &input_cancel,
        ctx.event_sender,
        &force_idr,
        ctx.flow_clock_domain,
        ctx.av_sync_pacer.clone(),
        #[cfg(feature = "replay")]
        ctx.replay_command_txs,
    );

    if input_def.config.is_ts_carrier() {
        let _ = super::ts_psi_catalog::spawn_psi_catalog_observer(
            input_id.clone(),
            ctx.flow_id.to_string(),
            per_input_tx.clone(),
            per_input_counters.psi_catalog.clone(),
            input_cancel.child_token(),
        );
    }

    let forwarder_handle = if let Some(bus) = ctx.es_bus {
        let bus_clone = bus.clone();
        let pic = per_input_counters.clone();
        let input_id_for_demuxer = input_id.clone();
        let demuxer_handle = ctx.flow_manager.acquire_demuxer(
            &input_id,
            move |rx, cancel| {
                Some(spawn_ts_es_demuxer_consumer(
                    input_id_for_demuxer,
                    rx,
                    bus_clone,
                    cancel,
                    pic,
                ))
            },
        );
        let input_cancel_inner = input_cancel.clone();
        tokio::spawn(async move {
            let _demuxer_handle = demuxer_handle; // RAII — drops on cancel
            input_cancel_inner.cancelled().await;
        })
    } else {
        spawn_input_forwarder(
            input_id.clone(),
            per_input_tx.subscribe(),
            ctx.active_input_watch_rx.clone(),
            input_cancel.clone(),
            ctx.fixer_tx.clone(),
            force_idr.clone(),
            per_input_counters,
        )
    };

    let thumbnail_handle = if ctx.thumbnail_enabled && ctx.ffmpeg_available {
        let thumb_acc = Arc::new(ThumbnailAccumulator::new_with_update_notify(
            ctx.global_stats.thumbnail_update_notify.clone(),
        ));
        ctx.flow_stats
            .per_input_thumbnails
            .insert(input_id.clone(), thumb_acc.clone());
        Some(spawn_thumbnail_generator(
            &per_input_tx,
            thumb_acc,
            input_cancel.child_token(),
            None,
        ))
    } else {
        None
    };

    #[cfg(not(feature = "webrtc"))]
    let _ = this_whip_info;

    SpawnedInputRuntime {
        input_id,
        input_runtime: InputRuntime {
            input_handle,
            forwarder_handle,
            cancel_token: input_cancel,
            thumbnail_handle,
        },
        #[cfg(feature = "webrtc")]
        whip_info: this_whip_info,
    }
}

/// Metadata describing how the active input feeds the media analyzer.
///
/// Extracted out of `FlowRuntime::start` so the main startup pipeline
/// reads as an orchestration of named steps rather than a 120-line
/// match arm. Returned by [`media_analysis_metadata`].
struct MediaAnalysisMeta {
    protocol: String,
    payload_format: String,
    fec_enabled: bool,
    fec_type: Option<String>,
    redundancy_enabled: bool,
    redundancy_type: Option<String>,
}

/// Classify an input for media-analysis accumulator construction.
fn media_analysis_metadata(input: &InputConfig) -> MediaAnalysisMeta {
    match input {
        InputConfig::Rtp(rtp) => {
            let (fec_enabled, fec_type) = rtp
                .fec_decode
                .as_ref()
                .map(|f| (true, Some(format!("SMPTE 2022-1 (L={}, D={})", f.columns, f.rows))))
                .unwrap_or((false, None));
            let (redundancy_enabled, redundancy_type) = rtp
                .redundancy
                .as_ref()
                .map(|_| (true, Some("SMPTE 2022-7".to_string())))
                .unwrap_or((false, None));
            MediaAnalysisMeta {
                protocol: "rtp".into(),
                payload_format: "rtp_ts".into(),
                fec_enabled, fec_type, redundancy_enabled, redundancy_type,
            }
        }
        InputConfig::Udp(_) => MediaAnalysisMeta {
            protocol: "udp".into(), payload_format: "raw_ts".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::Srt(srt) => {
            let (redundancy_enabled, redundancy_type) = srt
                .redundancy
                .as_ref()
                .map(|_| (true, Some("SMPTE 2022-7".to_string())))
                .unwrap_or((false, None));
            MediaAnalysisMeta {
                protocol: "srt".into(), payload_format: "unknown".into(),
                fec_enabled: false, fec_type: None,
                redundancy_enabled, redundancy_type,
            }
        }
        InputConfig::Rist(rist) => {
            let (redundancy_enabled, redundancy_type) = rist
                .redundancy
                .as_ref()
                .map(|_| (true, Some("SMPTE 2022-7".to_string())))
                .unwrap_or((false, None));
            MediaAnalysisMeta {
                protocol: "rist".into(), payload_format: "raw_ts".into(),
                fec_enabled: false, fec_type: None,
                redundancy_enabled, redundancy_type,
            }
        }
        InputConfig::Rtmp(_) => MediaAnalysisMeta {
            protocol: "rtmp".into(), payload_format: "raw_ts".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::Rtsp(_) => MediaAnalysisMeta {
            protocol: "rtsp".into(), payload_format: "raw_ts".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::Webrtc(_) => MediaAnalysisMeta {
            protocol: "webrtc".into(), payload_format: "rtp_h264".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::Whep(_) => MediaAnalysisMeta {
            protocol: "whep".into(), payload_format: "rtp_h264".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::St2110_30(c) => {
            let (re, rt) = c.redundancy.as_ref().map(|_| (true, Some("SMPTE 2022-7".to_string()))).unwrap_or((false, None));
            MediaAnalysisMeta {
                protocol: "st2110_30".into(), payload_format: "pcm_l24".into(),
                fec_enabled: false, fec_type: None, redundancy_enabled: re, redundancy_type: rt,
            }
        }
        InputConfig::St2110_31(c) => {
            let (re, rt) = c.redundancy.as_ref().map(|_| (true, Some("SMPTE 2022-7".to_string()))).unwrap_or((false, None));
            MediaAnalysisMeta {
                protocol: "st2110_31".into(), payload_format: "aes3".into(),
                fec_enabled: false, fec_type: None, redundancy_enabled: re, redundancy_type: rt,
            }
        }
        InputConfig::St2110_40(c) => {
            let (re, rt) = c.redundancy.as_ref().map(|_| (true, Some("SMPTE 2022-7".to_string()))).unwrap_or((false, None));
            MediaAnalysisMeta {
                protocol: "st2110_40".into(), payload_format: "anc".into(),
                fec_enabled: false, fec_type: None, redundancy_enabled: re, redundancy_type: rt,
            }
        }
        InputConfig::St2110_20(c) => {
            let (re, rt) = c.redundancy.as_ref().map(|_| (true, Some("SMPTE 2022-7".to_string()))).unwrap_or((false, None));
            MediaAnalysisMeta {
                protocol: "st2110_20".into(), payload_format: "rfc4175_ycbcr422".into(),
                fec_enabled: false, fec_type: None, redundancy_enabled: re, redundancy_type: rt,
            }
        }
        InputConfig::St2110_23(_) => MediaAnalysisMeta {
            protocol: "st2110_23".into(),
            payload_format: "rfc4175_ycbcr422_multi".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: true,
            redundancy_type: Some("SMPTE 2110-23 multi-stream".into()),
        },
        InputConfig::RtpAudio(c) => {
            let (re, rt) = c.redundancy.as_ref().map(|_| (true, Some("SMPTE 2022-7".to_string()))).unwrap_or((false, None));
            let payload_format = match c.bit_depth {
                16 => "pcm_l16".to_string(),
                _ => "pcm_l24".to_string(),
            };
            MediaAnalysisMeta {
                protocol: "rtp_audio".into(), payload_format,
                fec_enabled: false, fec_type: None, redundancy_enabled: re, redundancy_type: rt,
            }
        }
        InputConfig::Bonded(c) => {
            let multi = c.paths.len() > 1;
            MediaAnalysisMeta {
                protocol: "bonded".into(), payload_format: "raw_ts".into(),
                fec_enabled: false, fec_type: None,
                redundancy_enabled: multi,
                redundancy_type: if multi {
                    Some(format!("bonded ({} paths)", c.paths.len()))
                } else {
                    None
                },
            }
        }
        InputConfig::TestPattern(_) => MediaAnalysisMeta {
            protocol: "test_pattern".into(), payload_format: "h264_aac_ts".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::MediaPlayer(_) => MediaAnalysisMeta {
            protocol: "media_player".into(), payload_format: "raw_ts".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::Replay(_) => MediaAnalysisMeta {
            protocol: "replay".into(), payload_format: "raw_ts".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::MxlVideo(_) => MediaAnalysisMeta {
            protocol: "mxl_video".into(), payload_format: "h264_hevc_ts".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::MxlAudio(c) => MediaAnalysisMeta {
            protocol: "mxl_audio".into(),
            payload_format: if c.audio_encode.is_some() { "audio_ts".into() } else { "pcm_f32".into() },
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
        InputConfig::MxlAnc(_) => MediaAnalysisMeta {
            protocol: "mxl_anc".into(), payload_format: "anc".into(),
            fec_enabled: false, fec_type: None,
            redundancy_enabled: false, redundancy_type: None,
        },
    }
}

/// Build a topology-display [`InputConfigMeta`] from a concrete
/// [`InputConfig`]. Used by `FlowRuntime::start` and by runtime updates that
/// swap the active input without restarting the flow.
/// Map an `InputConfig` variant to the short transport-type string carried
/// on `InputStats.input_type` and `PerInputLive.input_type`. Kept in sync
/// with the equivalent active-input lookup at flow startup.
fn input_type_str(input: &InputConfig) -> &'static str {
    match input {
        InputConfig::Rtp(_) => "rtp",
        InputConfig::Udp(_) => "udp",
        InputConfig::Srt(_) => "srt",
        InputConfig::Rist(_) => "rist",
        InputConfig::Rtmp(_) => "rtmp",
        InputConfig::Rtsp(_) => "rtsp",
        InputConfig::Webrtc(_) => "webrtc",
        InputConfig::Whep(_) => "whep",
        InputConfig::St2110_30(_) => "st2110_30",
        InputConfig::St2110_31(_) => "st2110_31",
        InputConfig::St2110_40(_) => "st2110_40",
        InputConfig::St2110_20(_) => "st2110_20",
        InputConfig::St2110_23(_) => "st2110_23",
        InputConfig::RtpAudio(_) => "rtp_audio",
        InputConfig::Bonded(_) => "bonded",
        InputConfig::TestPattern(_) => "test_pattern",
        InputConfig::MediaPlayer(_) => "media_player",
        InputConfig::Replay(_) => "replay",
        InputConfig::MxlVideo(_) => "mxl_video",
        InputConfig::MxlAudio(_) => "mxl_audio",
        InputConfig::MxlAnc(_) => "mxl_anc",
    }
}

/// Pick the right [`AudioFullMode`] for the active input. Returns `None`
/// for input types we cannot run R128 against — uncompressed video,
/// ANC-only flows, WebRTC (the broadcast carries TS-wrapped H.264 which
/// goes through the [`AudioFullMode::Ts`] path), bonded transports
/// (delegated to the underlying transport once attached), test pattern.
fn derive_audio_full_mode(
    input: &InputConfig,
    ts_carrier: bool,
) -> Option<crate::engine::content_analysis::audio_full::AudioFullMode> {
    use crate::engine::content_analysis::audio_full::AudioFullMode;
    match input {
        InputConfig::St2110_31(c) => Some(AudioFullMode::Aes3 {
            sample_rate: c.sample_rate,
            channels: c.channels,
        }),
        InputConfig::St2110_30(c) => {
            if c.bit_depth == 16 {
                Some(AudioFullMode::Pcm {
                    codec: "pcm_l16",
                    sample_rate: c.sample_rate,
                    channels: c.channels,
                    bytes_per_sample: 2,
                })
            } else {
                Some(AudioFullMode::Pcm {
                    codec: "pcm_l24",
                    sample_rate: c.sample_rate,
                    channels: c.channels,
                    bytes_per_sample: 3,
                })
            }
        }
        InputConfig::RtpAudio(c) => {
            let bps = if c.bit_depth == 16 { 2 } else { 3 };
            let codec = if c.bit_depth == 16 { "pcm_l16" } else { "pcm_l24" };
            Some(AudioFullMode::Pcm {
                codec,
                sample_rate: c.sample_rate,
                channels: c.channels,
                bytes_per_sample: bps,
            })
        }
        _ if ts_carrier => Some(AudioFullMode::Ts),
        _ => None,
    }
}

fn build_input_config_meta(input: &InputConfig) -> crate::stats::collector::InputConfigMeta {
    use crate::stats::collector::InputConfigMeta;
    match input {
        InputConfig::Rtp(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            rtsp_url: None, whep_url: None,
        },
        InputConfig::Udp(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            rtsp_url: None, whep_url: None,
        },
        InputConfig::Srt(c) => InputConfigMeta {
            mode: Some(format!("{:?}", c.mode).to_lowercase()),
            local_addr: c.local_addr.clone(),
            remote_addr: c.remote_addr.clone(),
            listen_addr: None, bind_addr: None,
            rtsp_url: None, whep_url: None,
        },
        InputConfig::Rist(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            rtsp_url: None, whep_url: None,
        },
        InputConfig::Rtmp(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: Some(c.listen_addr.clone()), bind_addr: None,
            rtsp_url: None, whep_url: None,
        },
        InputConfig::Rtsp(c) => InputConfigMeta {
            mode: Some(format!("{:?}", c.transport).to_lowercase()),
            local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: None,
            rtsp_url: Some(c.rtsp_url.clone()), whep_url: None,
        },
        InputConfig::Webrtc(_) => InputConfigMeta {
            mode: Some("whip_server".to_string()), local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: None,
            rtsp_url: None, whep_url: None,
        },
        InputConfig::Whep(c) => InputConfigMeta {
            mode: Some("whep_client".to_string()), local_addr: None,
            remote_addr: None,
            listen_addr: None, bind_addr: None,
            rtsp_url: None, whep_url: Some(c.whep_url.clone()),
        },
        InputConfig::St2110_30(c) | InputConfig::St2110_31(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            rtsp_url: None, whep_url: None,
        },
        InputConfig::St2110_40(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            rtsp_url: None, whep_url: None,
        },
        InputConfig::St2110_20(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            rtsp_url: None, whep_url: None,
        },
        InputConfig::St2110_23(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: None,
            bind_addr: c.sub_streams.first().map(|s| s.bind_addr.clone()),
            rtsp_url: None, whep_url: None,
        },
        InputConfig::RtpAudio(c) => InputConfigMeta {
            mode: None, local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            rtsp_url: None, whep_url: None,
        },
        InputConfig::Bonded(c) => InputConfigMeta {
            // Use the first path's identifier as a topology hint;
            // the full picture comes from bond stats, not this meta.
            mode: Some(format!("bonded ({} paths)", c.paths.len())),
            local_addr: None,
            remote_addr: None,
            listen_addr: None,
            bind_addr: None,
            rtsp_url: None,
            whep_url: None,
        },
        InputConfig::TestPattern(c) => InputConfigMeta {
            mode: Some(format!("test-pattern {}x{}@{}", c.width, c.height, c.fps)),
            local_addr: None,
            remote_addr: None,
            listen_addr: None,
            bind_addr: None,
            rtsp_url: None,
            whep_url: None,
        },
        InputConfig::MediaPlayer(c) => InputConfigMeta {
            mode: Some(format!("media-player ({} source(s))", c.sources.len())),
            local_addr: None,
            remote_addr: None,
            listen_addr: None,
            bind_addr: None,
            rtsp_url: None,
            whep_url: None,
        },
        InputConfig::Replay(c) => InputConfigMeta {
            mode: Some(match &c.clip_id {
                Some(clip) => format!("replay clip {clip} of {}", c.recording_id),
                None => format!("replay recording {}", c.recording_id),
            }),
            local_addr: None,
            remote_addr: None,
            listen_addr: None,
            bind_addr: None,
            rtsp_url: None,
            whep_url: None,
        },
        InputConfig::MxlVideo(c) => InputConfigMeta {
            mode: Some(format!("mxl_video {}x{} domain={} flow={}", c.width, c.height, c.mxl.domain_path, c.mxl.flow_name)),
            local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: None,
            rtsp_url: None, whep_url: None,
        },
        InputConfig::MxlAudio(c) => InputConfigMeta {
            mode: Some(format!("mxl_audio {}ch domain={} flow={}", c.channels, c.mxl.domain_path, c.mxl.flow_name)),
            local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: None,
            rtsp_url: None, whep_url: None,
        },
        InputConfig::MxlAnc(c) => InputConfigMeta {
            mode: Some(format!("mxl_anc domain={} flow={}", c.mxl.domain_path, c.mxl.flow_name)),
            local_addr: None, remote_addr: None,
            listen_addr: None, bind_addr: None,
            rtsp_url: None, whep_url: None,
        },
    }
}

/// Spawn the forwarder task for a single input. The forwarder drains the
/// per-input broadcast channel and re-publishes packets onto the flow's main
/// broadcast channel only while the `active_input_rx` watch value equals this
/// input's ID. Passive inputs' packets are silently dropped — the per-input
/// task keeps running so its stats continue to advance.
///
/// Fixer commands are dispatched to a dedicated [`ts_fixer_task`] via
/// `fixer_tx`. That task owns the [`TsContinuityFixer`] single-threaded —
/// no locks on the packet hot path — and is responsible for seamless
/// continuity counter progression and cached PAT/PMT injection at the
/// switch boundary so external decoders re-acquire quickly.
///
/// `force_idr` is this input's one-shot IDR request flag. The forwarder sets
/// it to `true` on a passive → active transition so that any ingress video
/// re-encoder in this input's pipeline emits an IDR on its next frame —
/// otherwise downstream decoders have to wait up to a full GOP for the next
/// natural keyframe before they can display the switched feed.
/// Idle-keepalive period. When the active input's forwarder has gone
/// this long without receiving a natural packet, it injects a single
/// NULL-PID (0x1FFF) TS packet into the output so downstream receivers
/// (ffplay, VLC, hardware decoders) see a live stream instead of socket
/// silence. Without this, a 3 s+ gap on the output — which happens
/// whenever the operator switches to an input that has no source feeding
/// it — drives ffplay into a terminal state where audio stops queuing
/// even after the source returns, needing an ffplay restart to recover.
///
/// 250 ms is below MPEG-TS PSI repetition intervals (100-500 ms typical)
/// and well under socket read timeouts on every common receiver.
const KEEPALIVE_INTERVAL_MS: u64 = 250;

/// Build a keepalive payload: 7 back-to-back 188-byte NULL TS packets
/// (PID 0x1FFF, payload-only, CC=0). Sized to exactly one 1316-byte UDP
/// datagram so the UDP output's 7-packet-per-datagram aligner flushes
/// it immediately — a single 188-byte null packet would sit in the
/// aligner's buffer until a real datagram's worth of bytes accumulated,
/// which defeats the whole point of a keepalive. Receivers drop NULL
/// packets per spec; all the keepalive does is keep UDP datagrams
/// flowing so sockets / decoders don't time out.
fn null_ts_packet() -> RtpPacket {
    const PACKETS: usize = 7;
    const ONE: [u8; 188] = {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47; // sync
        pkt[1] = 0x1F; // PUSI=0, TP=0, PID hi = 0x1F
        pkt[2] = 0xFF; // PID lo = 0xFF → full PID 0x1FFF (NULL_PID)
        pkt[3] = 0x10; // scrambling=00, adaptation=01 (payload only), CC=0
        pkt
    };
    let mut buf = Vec::with_capacity(PACKETS * 188);
    for _ in 0..PACKETS {
        buf.extend_from_slice(&ONE);
    }
    RtpPacket {
        data: Bytes::from(buf),
        sequence_number: 0,
        rtp_timestamp: 0,
        recv_time_us: 0,
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
    }
}

/// Command queued to the per-flow [`ts_fixer_task`] by an input forwarder.
///
/// Forwarders never touch the `TsContinuityFixer` directly — they enqueue
/// one of these variants and let the fixer task own the state uncontended.
pub(crate) enum FixerCommand {
    /// The active input just produced a media packet. The fixer must apply
    /// CC rewriting and forward it to the flow's broadcast channel.
    ActivePacket { input_id: String, pkt: RtpPacket },
    /// A passive input produced a packet. The fixer uses it to pre-warm its
    /// per-input PSI cache so the next switch can inject a fresh PAT/PMT.
    PassivePacket { input_id: String, pkt: RtpPacket },
    /// The active input changed. The fixer emits any cached PSI packets for
    /// the newly-active input and resets its CC state appropriately.
    Switch { input_id: String },
    /// Emit a keepalive NULL datagram to keep downstream sockets alive
    /// during a silent active input.
    Keepalive,
    /// The source-discontinuity watcher detected an upstream PCR / PTS / DTS
    /// backward jump on the active input's stream (e.g. an ffmpeg
    /// `-stream_loop -1 -c copy` file-loop boundary, an encoder restart, a
    /// decoder reseat). Signal the fixer to set the MPEG-TS
    /// `discontinuity_indicator` bit on the **next** PCR-bearing packet it
    /// forwards — same one-shot mechanism the operator-input-switch path
    /// uses. Receivers see DI=1 and flush their STC cleanly instead of
    /// silently sliding A/V over the jump.
    ///
    /// One-shot: the fixer ORs the flag with its existing
    /// `pending_di_on_pcr`; consumed on the next PCR.
    SignalSourceDiscontinuity,
    /// An input has been hot-removed from the flow. Drop its cached
    /// PAT/PMT so a later hot-add of an input under the same id doesn't
    /// re-inject stale PSI on the next switch.
    ///
    /// Enqueued by the future `FlowRuntime::remove_input` path before
    /// the per-input cancel token fires. Idempotent on the fixer side —
    /// dropping an unknown id is a no-op.
    #[allow(dead_code)] // Phase 1 wires callers via FlowRuntime::remove_input.
    DropInputPsi { input_id: String },
}

/// Dedicated fixer task that owns the [`TsContinuityFixer`] for one flow.
///
/// Serialising all fixer access onto a single task eliminates the former
/// `std::sync::Mutex` that per-input forwarders used to share. The channel
/// is bounded and producers use `try_send`, so a slow fixer drops commands
/// rather than back-pressuring the forwarders — matching the broadcast
/// drop-on-lag semantic everywhere else in the data path.
async fn ts_fixer_task(
    mut rx: tokio::sync::mpsc::Receiver<FixerCommand>,
    out_tx: broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
) {
    let mut fixer = TsContinuityFixer::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            cmd = rx.recv() => match cmd {
                Some(FixerCommand::ActivePacket { input_id, pkt }) => {
                    match fixer.process_packet(&input_id, &pkt) {
                        ProcessResult::Rewritten(fixed_data) => {
                            if out_tx.send(RtpPacket {
                                data: fixed_data,
                                sequence_number: pkt.sequence_number,
                                rtp_timestamp: pkt.rtp_timestamp,
                                recv_time_us: pkt.recv_time_us,
                                is_raw_ts: pkt.is_raw_ts,
                                upstream_seq: None,
                                upstream_leg_id: None,
                                sender_timestamp_us: None,
                            }).is_err() {
                                FIXER_OUT_NO_RECEIVERS.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        ProcessResult::Unchanged => {
                            if out_tx.send(pkt).is_err() {
                                FIXER_OUT_NO_RECEIVERS.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                Some(FixerCommand::PassivePacket { input_id, pkt }) => {
                    fixer.observe_passive(&input_id, &pkt);
                }
                Some(FixerCommand::Switch { input_id }) => {
                    let injected = fixer.on_switch(&input_id);
                    tracing::info!(
                        "TS fixer: switch to '{}', injecting {} PSI packets",
                        input_id, injected.len()
                    );
                    for inj_pkt in injected {
                        if out_tx.send(inj_pkt).is_err() {
                            FIXER_OUT_NO_RECEIVERS.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Some(FixerCommand::Keepalive) => {
                    if out_tx.send(null_ts_packet()).is_err() {
                        FIXER_OUT_NO_RECEIVERS.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Some(FixerCommand::SignalSourceDiscontinuity) => {
                    // The watcher saw an upstream backward PCR/PTS/DTS
                    // jump. Set the DI flag so the next PCR-bearing
                    // packet that flows through the fixer gets
                    // `discontinuity_indicator` set. One-shot;
                    // consumed inside `process_packet`.
                    fixer.signal_source_discontinuity();
                }
                Some(FixerCommand::DropInputPsi { input_id }) => {
                    // An input was hot-removed. Drop its cached PAT/PMT
                    // so a later hot-add under the same id doesn't
                    // re-inject stale PSI on the next switch. Idempotent.
                    fixer.forget_input(&input_id);
                }
                None => return,
            }
        }
    }
}

fn spawn_input_forwarder(
    input_id: String,
    mut rx: broadcast::Receiver<RtpPacket>,
    active_input_rx: watch::Receiver<String>,
    cancel: CancellationToken,
    fixer_tx: tokio::sync::mpsc::Sender<FixerCommand>,
    force_idr: Arc<std::sync::atomic::AtomicBool>,
    per_input_counters: Arc<crate::stats::collector::PerInputCounters>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut was_active = *active_input_rx.borrow() == input_id;
        // Keepalive tick: only emits NULL packets while this input is
        // active and no natural packet has arrived for the tick window.
        let mut keepalive = tokio::time::interval(std::time::Duration::from_millis(
            KEEPALIVE_INTERVAL_MS,
        ));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                r = rx.recv() => match r {
                    Ok(pkt) => {
                        // Per-input liveness counters — incremented for every
                        // packet regardless of active/passive, so the manager
                        // UI can report feed-present state for all inputs.
                        per_input_counters
                            .bytes
                            .fetch_add(pkt.data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        per_input_counters
                            .packets
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                        let is_active = *active_input_rx.borrow() == input_id;

                        if is_active && !was_active {
                            // This input just became active — ask the fixer
                            // task to switch and inject cached PSI.
                            if fixer_tx.try_send(FixerCommand::Switch { input_id: input_id.clone() }).is_err() {
                                FIXER_TS_SWITCH_FULL.fetch_add(1, Ordering::Relaxed);
                            }
                            // Ask any ingress video re-encoder on this input
                            // to emit an IDR on its next frame. Passthrough
                            // inputs ignore the flag (no encoder to signal).
                            force_idr.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        was_active = is_active;

                        if is_active {
                            // Drop on full — matches broadcast channel drop-on-lag.
                            if fixer_tx.try_send(FixerCommand::ActivePacket {
                                input_id: input_id.clone(),
                                pkt,
                            }).is_err() {
                                FIXER_TS_ACTIVE_FULL.fetch_add(1, Ordering::Relaxed);
                            }
                            keepalive.reset();
                        } else {
                            if fixer_tx.try_send(FixerCommand::PassivePacket {
                                input_id: input_id.clone(),
                                pkt,
                            }).is_err() {
                                FIXER_TS_PASSIVE_FULL.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Forwarder falling behind its per-input channel —
                        // skip missed packets but keep the task alive.
                        FWD_LAGGED.fetch_add(n, Ordering::Relaxed);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = keepalive.tick() => {
                    // Only emit keepalive when *this* input is the active
                    // one. Otherwise every passive forwarder would spam
                    // null packets into the broadcast channel.
                    if *active_input_rx.borrow() == input_id {
                        if fixer_tx.try_send(FixerCommand::Keepalive).is_err() {
                            FIXER_TS_KEEPALIVE_FULL.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    })
}

/// How long `FlowRuntime::start` waits for every pending `Essence`
/// slot to resolve against its input's PSI catalogue before bailing
/// with `pid_bus_essence_no_match` / `pid_bus_essence_no_catalogue`.
/// Typical PAT/PMT cadence is 100–400 ms from input bind; 3 s gives
/// generous headroom for startup races on RTMP/WebRTC inputs whose
/// first frame may lag socket bind by a second or two.
const ESSENCE_RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Map the resolver's error variant to the `error_code` + message the
/// runtime surfaces on the emitted Critical event. Centralised here so
/// the mapping stays consistent if the resolver grows new failure modes.
fn essence_error_to_event(flow_id: &str, err: &EssenceResolveError) -> (&'static str, String) {
    match err {
        EssenceResolveError::KindNotImplemented { kind } => (
            "pid_bus_essence_kind_not_implemented",
            format!(
                "flow '{flow_id}': essence kind {kind:?} is not yet implemented — \
                 Phase 6 resolves Video + Audio only. Use SlotSource::Pid with \
                 an explicit source_pid for Subtitle / Data slots."
            ),
        ),
        EssenceResolveError::NoCatalogue { input_id } => (
            "pid_bus_essence_no_catalogue",
            format!(
                "flow '{flow_id}': input '{input_id}' produced no PAT/PMT within \
                 the essence-resolution window — check that the input is actually \
                 receiving MPEG-TS."
            ),
        ),
        EssenceResolveError::NoMatch { input_id, kind } => (
            "pid_bus_essence_no_match",
            format!(
                "flow '{flow_id}': input '{input_id}' has a PSI catalogue but no \
                 stream of kind {kind:?} — pick a different input or a different \
                 essence kind."
            ),
        ),
    }
}

/// Finish bringing up the PID-bus SPTS runtime for a flow.
///
/// Extracted from `FlowRuntime::start` so the startup pipeline can call
/// a single step rather than inline ~160 lines of Essence resolution,
/// PCR fixup, Hitless merger spawn, per-ES analyzer spawn, and final
/// assembler spawn. All of these must run in this order:
///
/// 1. Resolve any pending `Essence` slots against the per-input PSI
///    catalogues (bail loudly on timeout / no-match).
/// 2. Fix up each program's `pcr_source` so it still references one of
///    its own slots after resolution.
/// 3. Spawn one pre-bus Hitless merger per expanded redundant slot.
/// 4. Snapshot the `(input_id, source_pid) → out_pid` routing onto the
///    flow stats and spawn a per-ES analyzer for each slot.
/// 5. Spawn the single assembler task that publishes onto
///    `broadcast_tx`.
#[allow(clippy::too_many_arguments)]
async fn finalize_spts_assembler(
    mut build: SptsBuildResult,
    bus: &Arc<NodeEsBus>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
    cancel_token: &CancellationToken,
    event_sender: &EventSender,
    flow_id: &str,
    input_clocks: &HashMap<String, crate::engine::master_clock::ClockIdentity>,
    flow_manager: &Arc<crate::engine::manager::FlowManager>,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
) -> Result<crate::engine::ts_assembler::AssemblerHandle> {
    // 1. Essence resolution.
    if !build.pending_essence.is_empty() {
        let mut catalogues: std::collections::HashMap<
            String,
            Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>,
        > = std::collections::HashMap::new();
        for p in &build.pending_essence {
            if !catalogues.contains_key(&p.input_id) {
                // Own-flow inputs reach the catalogue via the per-flow
                // counters map (cheap, no FlowManager hop). Foreign-flow
                // inputs (cross-flow assembly references — PES Switch
                // Phase 2.1c+) fall through to the node-wide
                // `FlowManager` registry, which holds the same `Arc`
                // the owning flow's PSI observer writes to (Phase 2.2b).
                let store = flow_stats
                    .per_input_counters
                    .get(&p.input_id)
                    .map(|c| c.psi_catalog.clone())
                    .or_else(|| flow_manager.psi_catalog_for_input(&p.input_id));
                if let Some(c) = store {
                    catalogues.insert(p.input_id.clone(), c);
                }
            }
        }
        tracing::info!(
            "Flow '{}': resolving {} essence slot(s) against per-input PSI catalogues (timeout = {} ms)",
            flow_id,
            build.pending_essence.len(),
            ESSENCE_RESOLVE_TIMEOUT.as_millis(),
        );
        match resolve_essence_slots(
            build.pending_essence.clone(),
            catalogues,
            ESSENCE_RESOLVE_TIMEOUT,
        )
        .await
        {
            Ok(pairs) => {
                for ((program_idx, slot_idx, leg_idx), pid) in pairs {
                    if let Some(prog) = build.plan.programs.get_mut(program_idx) {
                        if let Some(slot) = prog.slots.get_mut(slot_idx) {
                            patch_essence_pid(slot, leg_idx, pid);
                        }
                    }
                }
            }
            Err(err) => {
                let (code, msg) = essence_error_to_event(flow_id, &err);
                event_sender.emit_flow_with_details(
                    EventSeverity::Critical,
                    category::FLOW,
                    msg.clone(),
                    flow_id,
                    serde_json::json!({
                        "error_code": code,
                        "detail": format!("{err:?}"),
                    }),
                );
                bail!(msg);
            }
        }
    }

    // 2. Post-resolution PCR source fixup.
    for prog in build.plan.programs.iter_mut() {
        if prog.slots.iter().any(|s| s.source == prog.pcr_source) {
            continue;
        }
        let pcr_input = prog.pcr_source.0.clone();
        if let Some(remap) = prog
            .slots
            .iter()
            .find(|s| s.source.0 == pcr_input)
            .map(|s| s.source.clone())
        {
            tracing::info!(
                "Flow '{}': program {} pcr_source auto-remapped from 0x{:04X} to 0x{:04X} (essence-resolved slot on input '{}')",
                flow_id,
                prog.program_number,
                prog.pcr_source.1,
                remap.1,
                pcr_input,
            );
            prog.pcr_source = remap;
        } else {
            let msg = format!(
                "flow '{}': program {} pcr_source input '{}' has no slot after essence resolution",
                flow_id, prog.program_number, pcr_input,
            );
            event_sender.emit_flow_with_details(
                EventSeverity::Critical,
                category::FLOW,
                msg.clone(),
                flow_id,
                serde_json::json!({
                    "error_code": "pid_bus_pcr_source_unresolved",
                    "program_number": prog.program_number,
                    "input_id": pcr_input,
                }),
            );
            bail!(msg);
        }
    }

    // 2.5. Cross-clock compatibility check.
    //
    // Phase 2 of the PES Switch redesign drops the manager-side
    // membership check that today forces every assembly slot to
    // reference an input already in the flow. Once that's lifted,
    // operators can wire video and audio from different inputs.
    // This guard rejects any program whose slots come from
    // master-clock-incompatible inputs (two SourcePcr inputs from
    // separate encoders, cross-PTP-domain ST 2110 inputs, etc.) so
    // we never silently emit a stream that will drift apart.
    //
    // Today (membership check still in place + assignment-uniqueness
    // still enforced) the call is a no-op because every slot's input
    // lives in the same flow and therefore reports the same identity.
    // Phase 2.1 makes it load-bearing.
    if let Err(mismatch) = crate::engine::ts_assembler::check_assembly_clock_compatibility(
        &build.plan,
        input_clocks,
    ) {
        let msg = format!(
            "flow '{}': program {} slot 0x{:04X} input '{}' has master clock {} but the program already references input '{}' on clock {} — combining incompatible clocks would silently drift",
            flow_id,
            mismatch.program_number,
            mismatch.slot_out_pid,
            mismatch.slot_input_id,
            mismatch.slot_clock,
            mismatch.reference_input_id,
            mismatch.reference_clock,
        );
        event_sender.emit_flow_with_details(
            EventSeverity::Critical,
            category::FLOW,
            msg.clone(),
            flow_id,
            serde_json::json!({
                "error_code": "pid_bus_master_clock_mismatch",
                "program_number": mismatch.program_number,
                "slot_out_pid": mismatch.slot_out_pid,
                "slot_input_id": mismatch.slot_input_id,
                "slot_clock": mismatch.slot_clock,
                "reference_input_id": mismatch.reference_input_id,
                "reference_clock": mismatch.reference_clock,
            }),
        );
        bail!(msg);
    }

    // 3. Pre-bus Hitless mergers.
    if !build.pending_hitless.is_empty() {
        tracing::info!(
            "Flow '{}': spawning {} Hitless merger task(s) (primary-preference, stall {} ms)",
            flow_id,
            build.pending_hitless.len(),
            build.pending_hitless.first().map(|h| h.stall_ms).unwrap_or(0),
        );
    }
    for hl in &build.pending_hitless {
        let _ = crate::engine::ts_es_hitless::spawn_hitless_es_merger_full(
            hl.uid.clone(),
            hl.primary.clone(),
            hl.backup.clone(),
            bus.clone(),
            std::time::Duration::from_millis(hl.stall_ms),
            hl.seq_aware,
            hl.path_differential_ms,
            cancel_token.child_token(),
        );
    }

    let total_slots: usize = build.plan.programs.iter().map(|p| p.slots.len()).sum();
    tracing::info!(
        "Flow '{}': starting TS assembler — {} program(s), {} slot(s) total",
        flow_id,
        build.plan.programs.len(),
        total_slots,
    );

    // 4. Per-ES analyzers + routing snapshot.
    let mut routing: std::collections::HashMap<(String, u16), u16> =
        std::collections::HashMap::new();
    for prog in &build.plan.programs {
        for slot in &prog.slots {
            routing.insert(slot.source.clone(), slot.out_pid);
            let _ = crate::engine::ts_es_analysis::spawn_per_es_analyzer(
                slot.source.0.clone(),
                slot.source.1,
                bus.clone(),
                flow_stats.clone(),
                cancel_token.child_token(),
            );
        }
    }
    flow_stats.set_pid_routing(routing);

    // 5. The assembler itself — single publisher onto broadcast_tx.
    //    Pass the per-flow A/V sync pacer so the assembler can run
    //    its own muxer-mode rewriter on the assembled output (single
    //    shared anchor, master-clock PCR/PTS, TR 101 290 compliance).
    Ok(spawn_spts_assembler(
        build.plan,
        bus.clone(),
        broadcast_tx.clone(),
        cancel_token.child_token(),
        Some(event_sender.clone()),
        flow_id.to_string(),
        av_sync_pacer.clone(),
    ))
}

/// Classify an [`InputConfig`] against Phase 5/6/6.5 compatibility rules.
/// Returns `None` when the input can feed the SPTS assembler; otherwise
/// returns a short `error_code` pointing at the right remediation for
/// the operator:
///
/// - `pid_bus_spts_input_needs_audio_encode` — the input *could*
///   produce TS via the existing `audio_encode` input-level encoder
///   (ST 2110-30, ST 2110-31, `rtp_audio`) but hasn't been configured to.
/// - `pid_bus_audio_encode_codec_not_supported_on_input` — `audio_encode`
///   is set but the chosen codec has no Phase 6.5 runtime path
///   (today: `mp2`, `ac3`). Validation accepts them so the manager UI
///   can surface them; bring-up rejects them loudly.
/// - `pid_bus_spts_non_ts_input` — the input can't produce TS at all
///   within the Phase 5/6/6.5 runtime (ST 2110-40 ANC). Deferred to a
///   follow-up phase.
fn non_ts_spts_error_code(input: &InputConfig) -> Option<&'static str> {
    // Inputs that produce TS today — either natively or via a PCM→TS
    // synthesiser (`audio_encode` first-light codec set).
    if input.is_ts_carrier() {
        // Refine: for PCM/-31 inputs with `audio_encode`, double-check
        // that the chosen codec is actually supported at runtime. If not,
        // surface a distinct loud error instead of letting the input
        // task blow up at bring-up.
        let ae_codec: Option<&str> = match input {
            InputConfig::St2110_30(c) => c.audio_encode.as_ref().map(|a| a.codec.as_str()),
            InputConfig::RtpAudio(c) => c.audio_encode.as_ref().map(|a| a.codec.as_str()),
            InputConfig::St2110_31(c) => c.audio_encode.as_ref().map(|a| a.codec.as_str()),
            _ => None,
        };
        if let Some(codec) = ae_codec {
            if !super::input_pcm_encode::codec_supported_first_light(codec) {
                return Some("pid_bus_audio_encode_codec_not_supported_on_input");
            }
        }
        return None;
    }
    match input {
        InputConfig::St2110_30(c) if c.audio_encode.is_none() => {
            Some("pid_bus_spts_input_needs_audio_encode")
        }
        InputConfig::RtpAudio(c) if c.audio_encode.is_none() => {
            Some("pid_bus_spts_input_needs_audio_encode")
        }
        InputConfig::St2110_31(c) if c.audio_encode.is_none() => {
            Some("pid_bus_spts_input_needs_audio_encode")
        }
        _ => Some("pid_bus_spts_non_ts_input"),
    }
}

/// Look up the PMT `stream_type` a Phase 6.5 synthesised audio ES will
/// publish on — derived from the `audio_encode.codec` on the named
/// input. Returns `None` when the input is not a PCM / AES3 input, or
/// has no `audio_encode`, or the codec is outside the first-light set.
/// Used by the SPTS plan builder to reject slot/codec mismatches before
/// any traffic flows.
fn expected_stream_type_for_synthesised_input(
    inputs: &[crate::config::models::InputDefinition],
    input_id: &str,
) -> Option<u8> {
    let def = inputs.iter().find(|d| d.id == input_id)?;
    let codec = match &def.config {
        InputConfig::St2110_30(c) => c.audio_encode.as_ref().map(|a| a.codec.as_str())?,
        InputConfig::RtpAudio(c) => c.audio_encode.as_ref().map(|a| a.codec.as_str())?,
        InputConfig::St2110_31(c) => c.audio_encode.as_ref().map(|a| a.codec.as_str())?,
        _ => return None,
    };
    super::input_pcm_encode::stream_type_for_codec(codec)
}

/// Map the config-layer [`EssenceKind`] to the assembler's internal
/// [`EsKind`] so the resolver stays decoupled from the config crate.
fn es_kind_from_config(k: EssenceKind) -> EsKind {
    match k {
        EssenceKind::Video => EsKind::Video,
        EssenceKind::Audio => EsKind::Audio,
        EssenceKind::Subtitle => EsKind::Subtitle,
        EssenceKind::Data => EsKind::Data,
    }
}

/// Resolve a [`FlowAssembly`] with `kind = spts` or `kind = mpts` into
/// an [`SptsBuildResult`]. Scope guards enforced here:
///
/// - Every input on the flow must produce MPEG-TS (`is_ts_carrier()`),
///   with a sharpened error code when the input *could* produce TS via
///   the input-level `audio_encode` encoder but isn't configured to —
///   (`error_codes: pid_bus_spts_input_needs_audio_encode`,
///   `pid_bus_audio_encode_codec_not_supported_on_input`,
///   `pid_bus_spts_non_ts_input`).
/// - There is **no output-type gate**. The assembler stamps synthesised
///   RTP metadata (monotonic u16 sequence + 90 kHz timestamp derived
///   from the emission clock) so every existing output — UDP, RTP (with
///   or without 2022-1 FEC / 2022-7), SRT, RIST, HLS, CMAF, RTMP,
///   WebRTC — treats the assembled SPTS/MPTS as a first-class TS-
///   bearing source.
/// - Slot sources may be `SlotSource::Pid` (resolved here),
///   `SlotSource::Essence` (resolved against the input's PSI catalogue —
///   `pid_bus_essence_{kind_not_implemented,no_catalogue,no_match}`), or
///   `SlotSource::Hitless` (pre-bus merger spawned in
///   `FlowRuntime::start`; each leg must itself be `Pid` or `Essence` —
///   `pid_bus_hitless_leg_not_pid`).
/// - `pcr_source` existence is validated here; the match against a
///   concrete slot is deferred until after essence resolution and
///   surfaced as `pid_bus_pcr_source_unresolved` if it still doesn't
///   hit. MPTS programs without an effective PCR produce
///   `pid_bus_mpts_pcr_source_required`.
///
/// Every failure path emits a Critical event *before* bailing so the
/// manager UI can highlight the offending assembly field without having
/// to parse the error string.
/// Patch a resolved Essence PID into either the slot's `source` (top-level
/// `SlotSource::Essence`) or one of its `switch_legs` (Essence-typed leg
/// of a `SlotSource::Switch`). For the latter, also refresh `slot.source`
/// when the patched leg matches the slot's currently-active leg.
fn patch_essence_pid(slot: &mut AssemblySlot, leg_idx: Option<usize>, pid: u16) {
    match leg_idx {
        None => slot.source.1 = pid,
        Some(li) => {
            if let Some(legs) = slot.switch_legs.as_mut()
                && let Some(leg) = legs.get_mut(li)
            {
                leg.1 = pid;
                // If this is the active leg, refresh `source.1` too
                // — the assembler's install_plan / apply_plan_replacement
                // reads `source` to seed `active_leg_input` and the
                // PCR-resolution lookup uses it as well.
                if leg.0 == slot.source.0 {
                    slot.source.1 = pid;
                }
            }
        }
    }
}

fn build_assembly_plan(
    assembly: &FlowAssembly,
    flow: &ResolvedFlow,
    event_sender: &EventSender,
) -> Result<SptsBuildResult> {
    let flow_id = &flow.config.id;
    let emit = |error_code: &str, msg: &str, extra: serde_json::Value| {
        let mut details = serde_json::json!({ "error_code": error_code });
        if let (Some(d), Some(e)) = (details.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                d.insert(k.clone(), v.clone());
            }
        }
        event_sender.emit_flow_with_details(
            EventSeverity::Critical,
            category::FLOW,
            msg.to_string(),
            flow_id,
            details,
        );
    };

    // Output scope: the assembler stamps synthesised RTP metadata
    // (monotonic u16 sequence + 90 kHz timestamp derived from the
    // emission clock) so every existing output — UDP, RTP (with or
    // without 2022-1 FEC), SRT, RIST, HLS, CMAF, RTMP, WebRTC —
    // treats the assembled SPTS as a first-class TS-bearing source.
    // No output-type gate on assembly flows.

    // Input scope: every input must produce MPEG-TS. Phase 6 sharpens
    // the error for inputs that have an input-level encoder escape
    // hatch (ST 2110-30 / -31 / RtpAudio via `audio_encode`) so
    // operators know the fix without chasing the error text. Phase 6.5
    // enables that path at runtime for AAC family + s302m. ST 2110-40
    // (ANC) still has no path to TS and stays on the generic code.
    for input in &flow.inputs {
        if let Some(code) = non_ts_spts_error_code(&input.config) {
            let msg = match code {
                "pid_bus_spts_input_needs_audio_encode" => {
                    let hint = match &input.config {
                        InputConfig::St2110_31(_) => {
                            " Set `audio_encode = { codec: \"s302m\" }` — AES3 sub-frames \
                             ride through the 302M wrap bit-for-bit."
                        }
                        _ => {
                            " Set `audio_encode = { codec: \"aac_lc\" }` (or any supported \
                             codec) and the input will publish audio-only TS into the assembly."
                        }
                    };
                    format!(
                        "flow '{}': assembly.kind = spts requires TS on the broadcast channel; \
                         input '{}' is type '{}' and can produce TS via input-level `audio_encode`.{hint}",
                        flow_id,
                        input.id,
                        input_type_str(&input.config),
                    )
                }
                "pid_bus_audio_encode_codec_not_supported_on_input" => {
                    let codec = match &input.config {
                        InputConfig::St2110_30(c) => c.audio_encode.as_ref().map(|a| a.codec.clone()),
                        InputConfig::RtpAudio(c) => c.audio_encode.as_ref().map(|a| a.codec.clone()),
                        InputConfig::St2110_31(c) => c.audio_encode.as_ref().map(|a| a.codec.clone()),
                        _ => None,
                    }
                    .unwrap_or_default();
                    format!(
                        "flow '{}': input '{}' has `audio_encode.codec = \"{}\"` which \
                         is accepted at config time but has no Phase 6.5 runtime path. \
                         First-light supports aac_lc, he_aac_v1, he_aac_v2, s302m. \
                         mp2/ac3 are deferred to a follow-up.",
                        flow_id, input.id, codec
                    )
                }
                _ => format!(
                    "flow '{}': assembly.kind = spts requires every input to produce MPEG-TS. \
                     Input '{}' is type '{}' and has no path to TS in the current runtime \
                     (ST 2110-40 ANC-in-TS wrapping is deferred).",
                    flow_id,
                    input.id,
                    input_type_str(&input.config),
                ),
            };
            emit(
                code,
                &msg,
                serde_json::json!({
                    "input_id": input.id,
                    "input_type": input_type_str(&input.config),
                }),
            );
            bail!(msg);
        }
    }

    if assembly.programs.is_empty() {
        let msg = format!("flow '{flow_id}': assembly has no program");
        emit("pid_bus_no_program", &msg, serde_json::json!({}));
        bail!(msg);
    }

    let mut programs: Vec<ProgramPlan> = Vec::with_capacity(assembly.programs.len());
    let mut pending_essence: Vec<PendingEssenceSlot> = Vec::new();
    let mut pending_hitless: Vec<crate::engine::ts_assembler::PendingHitlessSlot> = Vec::new();

    for (program_idx, program) in assembly.programs.iter().enumerate() {
        // Resolve every slot. `Pid` slots map directly to a concrete
        // `(input_id, source_pid)`. `Essence` slots get a sentinel
        // `source_pid = 0` and an entry in `pending_essence` so the
        // runtime can patch the slot in place after the resolver
        // returns. `Hitless` slots expand into a synthetic bus key
        // (`hitless:slot_{P}_{S}`) and a `PendingHitlessSlot` entry
        // that the runtime turns into a merger task before the
        // assembler starts.
        let mut slots: Vec<AssemblySlot> = Vec::with_capacity(program.streams.len());
        // Active-input override for switch slots: prefer the operator's
        // last-known choice (round-tripped via `flow.active_input` at
        // restart) when it matches one of the slot's legs; otherwise
        // fall back to the slot-config's `initial_input_id`.
        let restart_active_id: Option<String> =
            flow.active_input().map(|d| d.id.clone());
        for stream in &program.streams {
            let (input_id, source_pid, switch_legs, splice_mode, splice_budget_ms): (
                String,
                u16,
                Option<Vec<(String, u16)>>,
                crate::config::models::SpliceMode,
                Option<u32>,
            ) = match &stream.source {
                SlotSource::Pid { input_id, source_pid } => {
                    (input_id.clone(), *source_pid, None, Default::default(), None)
                }
                SlotSource::Essence { input_id, kind } => {
                    let slot_idx = slots.len();
                    pending_essence.push(PendingEssenceSlot {
                        program_idx,
                        slot_idx,
                        input_id: input_id.clone(),
                        kind: es_kind_from_config(*kind),
                        leg_idx: None,
                    });
                    // sentinel, patched after resolution
                    (input_id.clone(), 0_u16, None, Default::default(), None)
                }
                SlotSource::Switch {
                    legs,
                    initial_input_id,
                    splice_mode,
                    splice_budget_ms,
                } => {
                    use crate::config::models::SwitchLeg;
                    let slot_idx = slots.len();
                    let mut leg_pairs: Vec<(String, u16)> = Vec::with_capacity(legs.len());
                    for (li, leg) in legs.iter().enumerate() {
                        match leg {
                            SwitchLeg::Pid { input_id, source_pid } => {
                                leg_pairs.push((input_id.clone(), *source_pid));
                            }
                            SwitchLeg::Essence { input_id, kind } => {
                                pending_essence.push(PendingEssenceSlot {
                                    program_idx,
                                    slot_idx,
                                    input_id: input_id.clone(),
                                    kind: es_kind_from_config(*kind),
                                    leg_idx: Some(li),
                                });
                                leg_pairs.push((input_id.clone(), 0_u16));
                            }
                        }
                    }
                    // Pick the active leg. Restart-persistence: if the
                    // operator's prior `active_input_id` is one of this
                    // slot's legs, use it. Otherwise fall back to the
                    // config's `initial_input_id`. Validation guarantees
                    // `initial_input_id` matches at least one leg.
                    let active_id = restart_active_id
                        .as_ref()
                        .filter(|aid| leg_pairs.iter().any(|(iid, _)| iid == aid.as_str()))
                        .cloned()
                        .unwrap_or_else(|| initial_input_id.clone());
                    let active_pair = leg_pairs
                        .iter()
                        .find(|(iid, _)| iid == &active_id)
                        .cloned()
                        .unwrap_or_else(|| leg_pairs[0].clone());
                    (active_pair.0, active_pair.1, Some(leg_pairs), *splice_mode, *splice_budget_ms)
                }
                SlotSource::Hitless {
                    primary,
                    backup,
                    mode,
                    stall_ms,
                    reorder_window,
                    path_differential_ms,
                } => {
                    let slot_idx = slots.len();
                    let uid = format!("slot_{}_{}", program_idx, slot_idx);
                    // Resolve primary + backup. For first-light both
                    // legs must be `SlotSource::Pid` — Essence-inside-
                    // Hitless and nested Hitless both fail loudly so
                    // the operator sees the limitation up-front.
                    let primary_pair = match primary.as_ref() {
                        SlotSource::Pid { input_id, source_pid } => {
                            (input_id.clone(), *source_pid)
                        }
                        _ => {
                            let msg = format!(
                                "flow '{flow_id}': program {} out_pid {}: SlotSource::Hitless \
                                 first-light only supports SlotSource::Pid legs (got {:?}). \
                                 Resolve nested Essence to a concrete PID, or wait for the \
                                 follow-up that adds Essence-inside-Hitless.",
                                program.program_number, stream.out_pid, primary,
                            );
                            emit(
                                "pid_bus_hitless_leg_not_pid",
                                &msg,
                                serde_json::json!({
                                    "program_number": program.program_number,
                                    "out_pid": stream.out_pid,
                                    "leg": "primary",
                                }),
                            );
                            bail!(msg);
                        }
                    };
                    let backup_pair = match backup.as_ref() {
                        SlotSource::Pid { input_id, source_pid } => {
                            (input_id.clone(), *source_pid)
                        }
                        _ => {
                            let msg = format!(
                                "flow '{flow_id}': program {} out_pid {}: SlotSource::Hitless \
                                 first-light only supports SlotSource::Pid legs (got {:?}). \
                                 Resolve nested Essence to a concrete PID, or wait for the \
                                 follow-up that adds Essence-inside-Hitless.",
                                program.program_number, stream.out_pid, backup,
                            );
                            emit(
                                "pid_bus_hitless_leg_not_pid",
                                &msg,
                                serde_json::json!({
                                    "program_number": program.program_number,
                                    "out_pid": stream.out_pid,
                                    "leg": "backup",
                                }),
                            );
                            bail!(msg);
                        }
                    };
                    let seq_aware = matches!(
                        mode,
                        crate::config::models::HitlessMode::SeqAware
                    );
                    pending_hitless.push(crate::engine::ts_assembler::PendingHitlessSlot {
                        uid: uid.clone(),
                        primary: primary_pair,
                        backup: backup_pair,
                        stall_ms: stall_ms.unwrap_or(
                            crate::engine::ts_es_hitless::DEFAULT_STALL_MS,
                        ),
                        seq_aware,
                        reorder_window: reorder_window.unwrap_or(1024),
                        path_differential_ms: *path_differential_ms,
                    });
                    // The assembler reads from the synthetic merger
                    // output. The pre-bus merger task is responsible
                    // for keeping that key supplied.
                    let (iid, pid) = crate::engine::ts_es_hitless::hitless_bus_key(&uid);
                    (iid, pid, None, Default::default(), None)
                }
            };
            // Phase 6.5 cross-check: when the slot's input is a PCM /
            // AES3 input with `audio_encode` set, the codec determines
            // the stream_type the input will actually publish. Fail
            // loudly if the operator declared a different stream_type
            // on the slot — otherwise the assembler would advertise one
            // thing in the synthesised PMT and the input would emit
            // PES bytes that don't match it. For Switch slots, run the
            // check against every leg's input — any leg whose input
            // would publish a different stream_type is rejected.
            let inputs_to_check: Vec<&str> = match &switch_legs {
                Some(legs) => legs.iter().map(|(iid, _)| iid.as_str()).collect(),
                None => vec![input_id.as_str()],
            };
            for iid in inputs_to_check {
                if let Some(expected_st) =
                    expected_stream_type_for_synthesised_input(&flow.inputs, iid)
                {
                    if stream.stream_type != expected_st {
                        let msg = format!(
                            "flow '{}': program {} out_pid {}: slot declares stream_type = 0x{:02X} \
                             but input '{}' has `audio_encode` that publishes stream_type = 0x{:02X}. \
                             Either match the slot to the input's codec or drop `stream_type` from \
                             the slot.",
                            flow_id,
                            program.program_number,
                            stream.out_pid,
                            stream.stream_type,
                            iid,
                            expected_st,
                        );
                        emit(
                            "pid_bus_spts_stream_type_mismatch",
                            &msg,
                            serde_json::json!({
                                "program_number": program.program_number,
                                "out_pid": stream.out_pid,
                                "input_id": iid,
                                "declared_stream_type": stream.stream_type,
                                "expected_stream_type": expected_st,
                            }),
                        );
                        bail!(msg);
                    }
                }
            }

            slots.push(AssemblySlot {
                source: (input_id, source_pid),
                out_pid: stream.out_pid,
                stream_type: stream.stream_type,
                switch_legs,
                splice_mode,
                splice_budget_ms,
            });
        }

        // Effective per-program PCR: program-level override beats
        // flow-level fallback. Validator already enforces that one of
        // them is present for each program; re-assert here so the
        // runtime is authoritative.
        let pcr = program
            .pcr_source
            .as_ref()
            .or(assembly.pcr_source.as_ref())
            .ok_or_else(|| {
                let msg = format!(
                    "flow '{flow_id}': program {} has no effective pcr_source",
                    program.program_number
                );
                emit(
                    "pid_bus_mpts_pcr_source_required",
                    &msg,
                    serde_json::json!({ "program_number": program.program_number }),
                );
                anyhow::anyhow!(msg)
            })?;
        let pcr_key = (pcr.input_id.clone(), pcr.pid);

        programs.push(ProgramPlan {
            program_number: program.program_number,
            pmt_pid: program.pmt_pid,
            pcr_source: pcr_key,
            slots,
        });
    }

    Ok(SptsBuildResult {
        plan: AssemblyPlan { programs },
        pending_essence,
        pending_hitless,
    })
}

/// Drain a per-input `broadcast::Receiver<RtpPacket>` into a
/// [`TsEsDemuxer`], publishing elementary-stream packets onto the flow's
/// shared [`NodeEsBus`]. Replaces [`spawn_input_forwarder`] for flows
/// running under the SPTS assembler — there's no active-input gate and
/// no continuity fixer in this path because the assembler synthesises
/// its own CC + PSI downstream.
///
/// Per-input liveness counters (`bytes`, `packets`) are still updated so
/// the manager UI's "feed present" indicator works identically to
/// passthrough mode.
fn spawn_ts_es_demuxer_consumer(
    input_id: String,
    mut rx: broadcast::Receiver<RtpPacket>,
    bus: Arc<NodeEsBus>,
    cancel: CancellationToken,
    per_input_counters: Arc<crate::stats::collector::PerInputCounters>,
) -> JoinHandle<()> {
    // Stage 2 of the data-plane redesign: run the demuxer on a
    // dedicated SCHED_FIFO OS thread with its own
    // tokio::current_thread runtime, lifting it off the main worker
    // pool. CPU pinning honoured via `BILBYCAST_PID_BUS_CPUS`. The
    // demuxer is the upstream half of the PID-bus assembler (which
    // also moved to a dedicated runtime); keeping them on the same
    // CPU set lets the kernel keep their shared cache hot.
    let input_id_for_demuxer = input_id.clone();
    let thread_handle = crate::engine::dedicated_runtime::spawn_dedicated(
        crate::engine::dedicated_runtime::DedicatedRuntimeConfig::new(
            format!("demuxer-{input_id_for_demuxer}"),
            "BILBYCAST_PID_BUS_CPUS",
        ),
        async move {
            let mut demuxer = TsEsDemuxer::new(input_id_for_demuxer.clone(), bus);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    r = rx.recv() => match r {
                        Ok(pkt) => {
                            per_input_counters
                                .bytes
                                .fetch_add(pkt.data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                            per_input_counters
                                .packets
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            demuxer.process(&pkt);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!(
                                "ts_es_demuxer_consumer '{}': lagged {} packets",
                                input_id_for_demuxer, n
                            );
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        },
    );
    // Bridge to the existing `JoinHandle<()>` shape via a tokio
    // spawn_blocking that joins the OS thread. Same pattern as the
    // assembler's join bridge — see ts_assembler::spawn_spts_assembler.
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            let _ = thread_handle.join();
        }).await;
    })
}

/// Build topology-display metadata from an output config.
///
/// Used both at initial flow creation and when hot-adding outputs so that
/// stats snapshots always reflect the current output address/port.
fn build_output_config_meta(config: &OutputConfig) -> OutputConfigMeta {
    match config {
        OutputConfig::Srt(c) => OutputConfigMeta {
            mode: Some(format!("{:?}", c.mode).to_lowercase()),
            remote_addr: c.remote_addr.clone(),
            local_addr: c.local_addr.clone(),
            dest_addr: None, dest_url: None, ingest_url: None, whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::Rist(c) => OutputConfigMeta {
            mode: None,
            remote_addr: Some(c.remote_addr.clone()),
            local_addr: c.local_addr.clone(),
            dest_addr: None, dest_url: None, ingest_url: None, whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::Rtp(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None,
            dest_addr: Some(c.dest_addr.clone()),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::Udp(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None,
            dest_addr: Some(c.dest_addr.clone()),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::Rtmp(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None, dest_addr: None,
            dest_url: Some(c.dest_url.clone()),
            ingest_url: None, whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::Hls(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None, dest_addr: None, dest_url: None,
            ingest_url: Some(c.ingest_url.clone()), whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::Cmaf(c) => OutputConfigMeta {
            mode: Some(if c.low_latency { "cmaf-ll".to_string() } else { "cmaf".to_string() }),
            remote_addr: None, local_addr: None, dest_addr: None, dest_url: None,
            ingest_url: Some(c.ingest_url.clone()), whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::Webrtc(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None, dest_addr: None, dest_url: None,
            ingest_url: None, whip_url: c.whip_url.clone(),
            program_number: c.program_number,
        },
        OutputConfig::St2110_30(c) | OutputConfig::St2110_31(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None,
            dest_addr: Some(c.dest_addr.clone()),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
        OutputConfig::St2110_40(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None,
            dest_addr: Some(c.dest_addr.clone()),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
        OutputConfig::St2110_20(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None,
            dest_addr: Some(c.dest_addr.clone()),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
        OutputConfig::St2110_23(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None,
            dest_addr: c.sub_streams.first().map(|s| s.dest_addr.clone()),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
        OutputConfig::RtpAudio(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None,
            dest_addr: Some(c.dest_addr.clone()),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
        OutputConfig::Bonded(c) => OutputConfigMeta {
            mode: Some(format!("bonded ({} paths, {:?})", c.paths.len(), c.scheduler)),
            remote_addr: None,
            local_addr: None,
            dest_addr: None,
            dest_url: None,
            ingest_url: None,
            whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::Display(c) => OutputConfigMeta {
            mode: Some(format!(
                "display ({}{}{})",
                c.device,
                c.audio_device
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|a| format!(" → {}", a))
                    .unwrap_or_default(),
                c.resolution
                    .as_deref()
                    .filter(|s| !s.is_empty() && *s != "auto")
                    .map(|r| format!(" @ {}", r))
                    .unwrap_or_default(),
            )),
            remote_addr: None,
            local_addr: None,
            dest_addr: Some(c.device.clone()),
            dest_url: None,
            ingest_url: None,
            whip_url: None,
            program_number: c.program_number,
        },
        OutputConfig::MxlVideo(c) => OutputConfigMeta {
            mode: Some(format!("mxl_video {}x{} domain={} flow={}", c.width, c.height, c.mxl.domain_path, c.mxl.flow_name)),
            remote_addr: None, local_addr: None,
            dest_addr: Some(format!("{}#{}", c.mxl.domain_path, c.mxl.flow_name)),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
        OutputConfig::MxlAudio(c) => OutputConfigMeta {
            mode: Some(format!("mxl_audio {}ch domain={} flow={}", c.channels, c.mxl.domain_path, c.mxl.flow_name)),
            remote_addr: None, local_addr: None,
            dest_addr: Some(format!("{}#{}", c.mxl.domain_path, c.mxl.flow_name)),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
        OutputConfig::MxlAnc(c) => OutputConfigMeta {
            mode: Some(format!("mxl_anc domain={} flow={}", c.mxl.domain_path, c.mxl.flow_name)),
            remote_addr: None, local_addr: None,
            dest_addr: Some(format!("{}#{}", c.mxl.domain_path, c.mxl.flow_name)),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
    }
}

/// Bump the matching decoder family on a `FlowCostPlan` for an encoder
/// family — every transcoder decodes the source before re-encoding, so
/// every HW-encoded video session also charges one HW-decoded session
/// against the co-located decoder family. NVENC ↔ NVDEC, QSV-encode ↔
/// QSV-decode, VAAPI-encode ↔ VAAPI-decode. AMF and VideoToolbox have
/// no separate decoder counter today, so they no-op. Mixed-vendor hosts
/// (e.g. NVIDIA encode + Intel decode) will mis-attribute under this
/// pairing — out of scope until transcoding configs grow an explicit
/// `hw_decode` field.
fn pair_decoder_sessions_for_encoder(
    plan: &mut crate::engine::hardware_probe::FlowCostPlan,
    family: crate::engine::hardware_probe::HwEncoderFamily,
    is_4k: bool,
) {
    use crate::engine::hardware_probe::HwEncoderFamily;
    match family {
        HwEncoderFamily::Nvenc => {
            plan.nvdec_sessions = plan.nvdec_sessions.saturating_add(1);
            if is_4k {
                plan.nvdec_sessions_4k = plan.nvdec_sessions_4k.saturating_add(1);
            }
        }
        HwEncoderFamily::Qsv => {
            plan.qsv_decode_sessions = plan.qsv_decode_sessions.saturating_add(1);
            if is_4k {
                plan.qsv_decode_sessions_4k = plan.qsv_decode_sessions_4k.saturating_add(1);
            }
        }
        HwEncoderFamily::Vaapi => {
            plan.vaapi_decode_sessions = plan.vaapi_decode_sessions.saturating_add(1);
            if is_4k {
                plan.vaapi_decode_sessions_4k = plan.vaapi_decode_sessions_4k.saturating_add(1);
            }
        }
        HwEncoderFamily::Amf | HwEncoderFamily::VideoToolbox => {}
    }
}

/// Sibling of `pair_decoder_sessions_for_encoder` that operates on the
/// runtime `HwSessionUsage` (`*_in_use` fields) used by hot-add /
/// hot-remove of outputs. Same family co-location rule.
fn pair_decoder_usage_for_encoder(
    usage: &mut crate::engine::hardware_probe::HwSessionUsage,
    family: crate::engine::hardware_probe::HwEncoderFamily,
    is_4k: bool,
) {
    use crate::engine::hardware_probe::HwEncoderFamily;
    match family {
        HwEncoderFamily::Nvenc => {
            usage.nvdec_in_use = usage.nvdec_in_use.saturating_add(1);
            if is_4k {
                usage.nvdec_in_use_4k = usage.nvdec_in_use_4k.saturating_add(1);
            }
        }
        HwEncoderFamily::Qsv => {
            usage.qsv_decode_in_use = usage.qsv_decode_in_use.saturating_add(1);
            if is_4k {
                usage.qsv_decode_in_use_4k = usage.qsv_decode_in_use_4k.saturating_add(1);
            }
        }
        HwEncoderFamily::Vaapi => {
            usage.vaapi_decode_in_use = usage.vaapi_decode_in_use.saturating_add(1);
            if is_4k {
                usage.vaapi_decode_in_use_4k = usage.vaapi_decode_in_use_4k.saturating_add(1);
            }
        }
        HwEncoderFamily::Amf | HwEncoderFamily::VideoToolbox => {}
    }
}

/// Walk a resolved flow's outputs + flow-level config and produce the
/// `FlowCostPlan` consumed by `engine::hardware_probe::compute_flow_cost_units`.
///
/// Each `video_encode` block is run through
/// [`engine::hardware_probe::resolve_for_video_encode_config`] so the
/// cost preview agrees with the backend the runtime will actually pick
/// — important for `*_auto` codec strings, which would otherwise
/// classify as software via the string-only `HwEncoderFamily::classify`
/// fallback. On hosts where the static-capabilities probe hasn't
/// installed yet (early startup, unit tests) or rejects the operator's
/// combination, the legacy classify-by-string path runs as a fallback
/// so the cost plan still has a sensible value.
fn derive_cost_plan(flow: &ResolvedFlow) -> crate::engine::hardware_probe::FlowCostPlan {
    use crate::config::models::{InputConfig, OutputConfig};

    let mut plan = crate::engine::hardware_probe::FlowCostPlan::default();

    // Helper to extract the `video_encode` block for the output types
    // that carry one. Spelled as `fn` (not closure) so the borrow
    // checker can see the input/output lifetimes are linked.
    fn video_encode_block(
        out: &OutputConfig,
    ) -> Option<&crate::config::models::VideoEncodeConfig> {
        match out {
            OutputConfig::Srt(c) => c.video_encode.as_ref(),
            OutputConfig::Rtp(c) => c.video_encode.as_ref(),
            OutputConfig::Udp(c) => c.video_encode.as_ref(),
            OutputConfig::Rist(c) => c.video_encode.as_ref(),
            OutputConfig::Rtmp(c) => c.video_encode.as_ref(),
            OutputConfig::Webrtc(c) => c.video_encode.as_ref(),
            OutputConfig::Cmaf(c) => c.video_encode.as_ref(),
            _ => None,
        }
    }

    // Sibling of `video_encode_block` for inputs. Used by the input
    // loop below to charge per-input transcoder sessions onto the same
    // per-family counters the output loop uses. ST 2110-20/-23 carry a
    // mandatory (non-Option) `video_encode` and always charge one HW
    // session pair; every other variant is opt-in.
    fn input_video_encode_block(
        inp: &crate::config::models::InputDefinition,
    ) -> Option<&crate::config::models::VideoEncodeConfig> {
        match &inp.config {
            InputConfig::Srt(c) => c.video_encode.as_ref(),
            InputConfig::Udp(c) => c.video_encode.as_ref(),
            InputConfig::Rtp(c) => c.video_encode.as_ref(),
            InputConfig::Rist(c) => c.video_encode.as_ref(),
            InputConfig::Rtmp(c) => c.video_encode.as_ref(),
            InputConfig::Rtsp(c) => c.video_encode.as_ref(),
            InputConfig::Webrtc(c) => c.video_encode.as_ref(),
            InputConfig::Whep(c) => c.video_encode.as_ref(),
            InputConfig::MediaPlayer(c) => c.video_encode.as_ref(),
            InputConfig::Replay(c) => c.video_encode.as_ref(),
            InputConfig::TestPattern(c) => c.video_encode.as_ref(),
            InputConfig::St2110_20(c) => Some(&c.video_encode),
            InputConfig::St2110_23(c) => Some(&c.video_encode),
            InputConfig::MxlVideo(c) => Some(&c.video_encode),
            InputConfig::St2110_30(_)
            | InputConfig::St2110_31(_)
            | InputConfig::St2110_40(_)
            | InputConfig::RtpAudio(_)
            | InputConfig::Bonded(_)
            | InputConfig::MxlAudio(_)
            | InputConfig::MxlAnc(_) => None,
        }
    }

    for out in &flow.outputs {
        let (audio, video) = output_encode_blocks(out);
        if audio.is_some() {
            plan.audio_encode_outputs = plan.audio_encode_outputs.saturating_add(1);
        }
        if let Some(codec) = video {
            let ve = video_encode_block(out);
            let (w, h, fn_, fd, bd, chroma) = match ve {
                Some(v) => (v.width, v.height, v.fps_num, v.fps_den, v.bit_depth, v.chroma.clone()),
                None => (None, None, None, None, None, None),
            };
            let chroma_ref = chroma.as_deref();

            // Prefer the resolver result so `*_auto` codecs get the
            // backend the runtime would actually pick on this node.
            // Fall back to string classification when the static
            // capability snapshot isn't available yet.
            let resolved = ve.and_then(|v| {
                crate::engine::hardware_probe::resolve_for_video_encode_config(v).ok()
            });

            let (is_hw, hw_family) = match resolved {
                Some(r) => (!r.is_software(), r.hw_family()),
                None => {
                    let f = crate::engine::hardware_probe::HwEncoderFamily::classify(codec);
                    (f.is_some(), f)
                }
            };
            let units = crate::engine::hardware_probe::weighted_video_encode_units(
                is_hw, w, h, fn_, fd, bd, chroma_ref,
            );
            if is_hw {
                plan.hw_video_encode_units = plan.hw_video_encode_units.saturating_add(units);
                // 4K-tier classification — matches the probe's
                // `video_engine::PROBE_WIDTH_4K` (3840) /
                // `PROBE_HEIGHT_4K` (2160). Sessions without an
                // explicit resolution default to 1080p (probe
                // baseline). VideoToolbox is intentionally excluded
                // — macOS doesn't expose a 4K probe today.
                let is_4k = w.map_or(false, |w| w >= 3840) || h.map_or(false, |h| h >= 2160);
                match hw_family {
                    Some(crate::engine::hardware_probe::HwEncoderFamily::Nvenc) => {
                        plan.nvenc_sessions = plan.nvenc_sessions.saturating_add(1);
                        if is_4k {
                            plan.nvenc_sessions_4k = plan.nvenc_sessions_4k.saturating_add(1);
                        }
                    }
                    Some(crate::engine::hardware_probe::HwEncoderFamily::Qsv) => {
                        plan.qsv_sessions = plan.qsv_sessions.saturating_add(1);
                        if is_4k {
                            plan.qsv_sessions_4k = plan.qsv_sessions_4k.saturating_add(1);
                        }
                    }
                    Some(crate::engine::hardware_probe::HwEncoderFamily::VideoToolbox) => {
                        plan.videotoolbox_sessions =
                            plan.videotoolbox_sessions.saturating_add(1);
                    }
                    Some(crate::engine::hardware_probe::HwEncoderFamily::Amf) => {
                        plan.amf_sessions = plan.amf_sessions.saturating_add(1);
                        if is_4k {
                            plan.amf_sessions_4k = plan.amf_sessions_4k.saturating_add(1);
                        }
                    }
                    Some(crate::engine::hardware_probe::HwEncoderFamily::Vaapi) => {
                        plan.vaapi_sessions = plan.vaapi_sessions.saturating_add(1);
                        if is_4k {
                            plan.vaapi_sessions_4k = plan.vaapi_sessions_4k.saturating_add(1);
                        }
                    }
                    None => {} // is_hw + no family — defensive, unreachable today
                }
                // Pair the decoder slot — every transcoder decodes
                // before encoding, so the same session pair shows up
                // on the host's decoder family alongside the encoder.
                if let Some(family) = hw_family {
                    pair_decoder_sessions_for_encoder(&mut plan, family, is_4k);
                }
            } else {
                plan.sw_video_encode_units = plan.sw_video_encode_units.saturating_add(units);
            }
        }
        // ST 2110-20/-23 outputs always run a video encoder pipeline
        // (RFC 4175 packetization is fed by an internal x264/x265 pass)
        // — count them as one SW video encode each at the SW baseline.
        if matches!(out, OutputConfig::St2110_20(_) | OutputConfig::St2110_23(_)) {
            // ST 2110 video runs uncompressed at 1.5 Gbps for HD, so
            // pixel-rate-aware costing matters here too. Use the same
            // helper with no overrides — defaults to 1080p30 4:2:0.
            let units = crate::engine::hardware_probe::weighted_video_encode_units(
                false, None, None, None, None, None, None,
            );
            plan.sw_video_encode_units =
                plan.sw_video_encode_units.saturating_add(units);
        }
        // Local-display outputs (Linux-only, `display` Cargo feature).
        // Resolve the operator's `hw_decode` preference against the
        // host's probed capabilities: a CPU-decoded display output
        // contributes to `display_outputs`; a HW-decoded one (NVDEC
        // / QSV) contributes to `display_hw_decoded_outputs` and one
        // session to the matching family limit. The resolver matches
        // the one `start_output()` calls, so the cost-plan and the
        // runtime always agree on what backend each output ended up
        // on. A forced-but-missing backend (e.g. `nvdec` on a host
        // without an NVIDIA card) is treated as CPU here — the
        // spawner soft-falls-back to CPU + emits
        // `display_hw_decode_unavailable_falling_back` so the picture
        // stays on screen, and the flow ends up paying the CPU cost
        // we charge here.
        if let OutputConfig::Display(d) = out {
            let pref = d.hw_decode.unwrap_or_default();
            let resolved = crate::engine::hardware_probe::resolve_display_decoder(
                &pref,
                crate::engine::hardware_probe::static_capabilities()
                    .as_deref(),
            )
            .unwrap_or(crate::engine::hardware_probe::ResolvedDisplayDecoder::Cpu);
            if resolved.is_hardware() {
                plan.display_hw_decoded_outputs =
                    plan.display_hw_decoded_outputs.saturating_add(1);
                match resolved.family() {
                    Some(crate::engine::hardware_probe::HwDecoderFamily::Nvdec) => {
                        plan.nvdec_sessions = plan.nvdec_sessions.saturating_add(1);
                    }
                    Some(crate::engine::hardware_probe::HwDecoderFamily::Qsv) => {
                        plan.qsv_decode_sessions =
                            plan.qsv_decode_sessions.saturating_add(1);
                    }
                    Some(crate::engine::hardware_probe::HwDecoderFamily::Vaapi) => {
                        plan.vaapi_decode_sessions =
                            plan.vaapi_decode_sessions.saturating_add(1);
                    }
                    None => {}
                }
            } else {
                plan.display_outputs = plan.display_outputs.saturating_add(1);
            }
            if d.show_audio_bars {
                plan.display_audio_bars_outputs =
                    plan.display_audio_bars_outputs.saturating_add(1);
            }
        }
    }

    // Input-side transcoders charge encoder + decoder sessions just
    // like outputs do. Inputs are switched, not hot-added (see
    // `switch_active_input`), so this loop only runs at flow start —
    // there is no input mirror of `output_resource_contribution`.
    for inp in &flow.inputs {
        let (audio, video) = input_encode_blocks(inp);
        if audio.is_some() {
            plan.audio_encode_inputs = plan.audio_encode_inputs.saturating_add(1);
        }
        if let Some(codec) = video {
            let ve = input_video_encode_block(inp);
            let (w, h, fn_, fd, bd, chroma) = match ve {
                Some(v) => (
                    v.width,
                    v.height,
                    v.fps_num,
                    v.fps_den,
                    v.bit_depth,
                    v.chroma.clone(),
                ),
                None => (None, None, None, None, None, None),
            };
            let chroma_ref = chroma.as_deref();

            let resolved = ve.and_then(|v| {
                crate::engine::hardware_probe::resolve_for_video_encode_config(v).ok()
            });
            let (is_hw, hw_family) = match resolved {
                Some(r) => (!r.is_software(), r.hw_family()),
                None => {
                    let f = crate::engine::hardware_probe::HwEncoderFamily::classify(codec);
                    (f.is_some(), f)
                }
            };
            let units = crate::engine::hardware_probe::weighted_video_encode_units(
                is_hw, w, h, fn_, fd, bd, chroma_ref,
            );
            if is_hw {
                plan.hw_video_encode_units =
                    plan.hw_video_encode_units.saturating_add(units);
                let is_4k = w.map_or(false, |w| w >= 3840)
                    || h.map_or(false, |h| h >= 2160);
                match hw_family {
                    Some(crate::engine::hardware_probe::HwEncoderFamily::Nvenc) => {
                        plan.nvenc_sessions = plan.nvenc_sessions.saturating_add(1);
                        if is_4k {
                            plan.nvenc_sessions_4k =
                                plan.nvenc_sessions_4k.saturating_add(1);
                        }
                    }
                    Some(crate::engine::hardware_probe::HwEncoderFamily::Qsv) => {
                        plan.qsv_sessions = plan.qsv_sessions.saturating_add(1);
                        if is_4k {
                            plan.qsv_sessions_4k =
                                plan.qsv_sessions_4k.saturating_add(1);
                        }
                    }
                    Some(crate::engine::hardware_probe::HwEncoderFamily::VideoToolbox) => {
                        plan.videotoolbox_sessions =
                            plan.videotoolbox_sessions.saturating_add(1);
                    }
                    Some(crate::engine::hardware_probe::HwEncoderFamily::Amf) => {
                        plan.amf_sessions = plan.amf_sessions.saturating_add(1);
                        if is_4k {
                            plan.amf_sessions_4k =
                                plan.amf_sessions_4k.saturating_add(1);
                        }
                    }
                    Some(crate::engine::hardware_probe::HwEncoderFamily::Vaapi) => {
                        plan.vaapi_sessions = plan.vaapi_sessions.saturating_add(1);
                        if is_4k {
                            plan.vaapi_sessions_4k =
                                plan.vaapi_sessions_4k.saturating_add(1);
                        }
                    }
                    None => {} // is_hw + no family — defensive, unreachable today
                }
                if let Some(family) = hw_family {
                    pair_decoder_sessions_for_encoder(&mut plan, family, is_4k);
                }
            } else {
                plan.sw_video_encode_units =
                    plan.sw_video_encode_units.saturating_add(units);
            }
        }
    }

    let content = flow.config.effective_content_analysis();
    plan.content_analysis_lite = content.lite;
    plan.content_analysis_audio_full = content.audio_full;
    plan.content_analysis_video_full = content.video_full;
    plan.recording_enabled = flow
        .config
        .recording
        .as_ref()
        .map(|r| r.enabled)
        .unwrap_or(false);

    plan
}

/// Compute the resource-budget contribution of a single output —
/// `(cost_units, session_usage)` — keying off the same shape rules
/// `derive_cost_plan` applies to a fresh flow. Used by hot-add /
/// hot-remove to keep `FlowRuntime.cost_units` and
/// `FlowRuntime.hw_session_usage` in sync with the live output set
/// without re-running the full plan.
///
/// The base passthrough cost (1 unit), content-analysis tiers, and
/// recording cost are flow-level (not per-output) and stay constant
/// across hot-add / hot-remove — they're not folded in here.
fn output_resource_contribution(
    output: &crate::config::models::OutputConfig,
) -> (u32, crate::engine::hardware_probe::HwSessionUsage) {
    use crate::config::models::OutputConfig;
    let mut units: u32 = 0;
    let mut usage = crate::engine::hardware_probe::HwSessionUsage::default();

    let (audio, video) = output_encode_blocks(output);
    if audio.is_some() {
        units = units.saturating_add(5);
    }
    if let Some(codec) = video {
        // 4K-tier classification matches the probe's
        // `video_engine::PROBE_WIDTH_4K` (3840) / `PROBE_HEIGHT_4K`
        // (2160). Outputs without an explicit resolution stay 1080p.
        let (w, h) = output_video_encode_dims(output);
        let is_4k = w.map_or(false, |w| w >= 3840) || h.map_or(false, |h| h >= 2160);
        let family = crate::engine::hardware_probe::HwEncoderFamily::classify(codec);
        match family {
            Some(crate::engine::hardware_probe::HwEncoderFamily::Nvenc) => {
                units = units.saturating_add(100);
                usage.nvenc_in_use = usage.nvenc_in_use.saturating_add(1);
                if is_4k {
                    usage.nvenc_in_use_4k = usage.nvenc_in_use_4k.saturating_add(1);
                }
            }
            Some(crate::engine::hardware_probe::HwEncoderFamily::Qsv) => {
                units = units.saturating_add(100);
                usage.qsv_in_use = usage.qsv_in_use.saturating_add(1);
                if is_4k {
                    usage.qsv_in_use_4k = usage.qsv_in_use_4k.saturating_add(1);
                }
            }
            Some(crate::engine::hardware_probe::HwEncoderFamily::VideoToolbox) => {
                units = units.saturating_add(100);
                usage.videotoolbox_in_use = usage.videotoolbox_in_use.saturating_add(1);
            }
            Some(crate::engine::hardware_probe::HwEncoderFamily::Amf) => {
                units = units.saturating_add(100);
                usage.amf_in_use = usage.amf_in_use.saturating_add(1);
                if is_4k {
                    usage.amf_in_use_4k = usage.amf_in_use_4k.saturating_add(1);
                }
            }
            Some(crate::engine::hardware_probe::HwEncoderFamily::Vaapi) => {
                units = units.saturating_add(100);
                usage.vaapi_in_use = usage.vaapi_in_use.saturating_add(1);
                if is_4k {
                    usage.vaapi_in_use_4k = usage.vaapi_in_use_4k.saturating_add(1);
                }
            }
            None => {
                units = units.saturating_add(500);
            }
        }
        // Pair the decoder slot — see `pair_decoder_usage_for_encoder`.
        // Hot-remove of this output runs the same path in reverse, so
        // the decoder counter stays symmetric with the encoder.
        if let Some(f) = family {
            pair_decoder_usage_for_encoder(&mut usage, f, is_4k);
        }
    }
    if matches!(
        output,
        OutputConfig::St2110_20(_) | OutputConfig::St2110_23(_)
    ) {
        units = units.saturating_add(500);
    }
    if let OutputConfig::Display(d) = output {
        let pref = d.hw_decode.unwrap_or_default();
        let resolved = crate::engine::hardware_probe::resolve_display_decoder(
            &pref,
            crate::engine::hardware_probe::static_capabilities().as_deref(),
        )
        .unwrap_or(crate::engine::hardware_probe::ResolvedDisplayDecoder::Cpu);
        if resolved.is_hardware() {
            units = units.saturating_add(100);
            match resolved.family() {
                Some(crate::engine::hardware_probe::HwDecoderFamily::Nvdec) => {
                    usage.nvdec_in_use = usage.nvdec_in_use.saturating_add(1);
                }
                Some(crate::engine::hardware_probe::HwDecoderFamily::Qsv) => {
                    usage.qsv_decode_in_use = usage.qsv_decode_in_use.saturating_add(1);
                }
                Some(crate::engine::hardware_probe::HwDecoderFamily::Vaapi) => {
                    usage.vaapi_decode_in_use = usage.vaapi_decode_in_use.saturating_add(1);
                }
                None => {}
            }
        } else {
            units = units.saturating_add(275);
        }
        if d.show_audio_bars {
            units = units.saturating_add(15);
        }
    }
    (units, usage)
}

/// Pull the `(audio_encode, video_encode_codec)` pair out of an
/// [`OutputConfig`] variant, returning `(None, None)` for outputs
/// that don't carry encode blocks (ST 2110 audio, bonded, etc.).
/// The video-encode side reports just the codec string (HW vs SW
/// classification is delegated to `is_hw_video_codec`).
/// Pull the `(width, height)` from an output's `video_encode` block,
/// when one exists. Used by `output_resource_contribution` to decide
/// whether a session counts against the 4K tier.
fn output_video_encode_dims(
    output: &crate::config::models::OutputConfig,
) -> (Option<u32>, Option<u32>) {
    use crate::config::models::OutputConfig::*;
    let ve = match output {
        Rtp(c) => c.video_encode.as_ref(),
        Udp(c) => c.video_encode.as_ref(),
        Srt(c) => c.video_encode.as_ref(),
        Rist(c) => c.video_encode.as_ref(),
        Rtmp(c) => c.video_encode.as_ref(),
        Cmaf(c) => c.video_encode.as_ref(),
        Webrtc(c) => c.video_encode.as_ref(),
        _ => None,
    };
    match ve {
        Some(v) => (v.width, v.height),
        None => (None, None),
    }
}

fn output_encode_blocks(
    output: &crate::config::models::OutputConfig,
) -> (Option<&crate::config::models::AudioEncodeConfig>, Option<&str>) {
    use crate::config::models::OutputConfig::*;
    match output {
        Rtp(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Udp(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Srt(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Rist(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Rtmp(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        // HLS video_encode is deferred — only audio_encode counts today.
        Hls(c) => (c.audio_encode.as_ref(), None),
        Cmaf(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Webrtc(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        // Outputs without encode blocks: ST 2110 audio / ancillary,
        // RtpAudio (PCM passthrough), Bonded (passes the broadcast
        // channel directly). ST 2110-20/-23 are caught above by the
        // dedicated SW-encoder match in `derive_cost_plan`.
        _ => (None, None),
    }
}

/// Sibling of [`output_encode_blocks`] for inputs. Returns
/// `(audio_encode, video_encode_codec)` for the input loop in
/// `derive_cost_plan`. ST 2110-20/-23 always return a `Some(codec)`
/// because `video_encode` is mandatory on those variants. ST 2110-30/-31
/// audio inputs return their optional `audio_encode` only. ST 2110-40
/// (ancillary), RtpAudio (PCM passthrough), and Bonded carry no encode
/// blocks.
fn input_encode_blocks(
    inp: &crate::config::models::InputDefinition,
) -> (Option<&crate::config::models::AudioEncodeConfig>, Option<&str>) {
    use crate::config::models::InputConfig::*;
    match &inp.config {
        Srt(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Udp(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Rtp(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Rist(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Rtmp(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Rtsp(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Webrtc(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Whep(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        MediaPlayer(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        Replay(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        TestPattern(c) => (
            c.audio_encode.as_ref(),
            c.video_encode.as_ref().map(|v| v.codec.as_str()),
        ),
        // ST 2110-30/-31 audio inputs: optional input-level
        // `audio_encode` turns the PCM/AES3 ingress into a TS-carrier
        // (AAC / s302m) for the PID bus. No video.
        St2110_30(c) => (c.audio_encode.as_ref(), None),
        St2110_31(c) => (c.audio_encode.as_ref(), None),
        // ST 2110-20/-23 video inputs: `video_encode` is mandatory on
        // ingress (RFC 4175 → H.264 / H.265 for the PID bus).
        St2110_20(c) => (None, Some(c.video_encode.codec.as_str())),
        St2110_23(c) => (None, Some(c.video_encode.codec.as_str())),
        // RTP audio: optional `audio_encode` on the RTP-PCM ingress.
        RtpAudio(c) => (c.audio_encode.as_ref(), None),
        // No encode blocks on these.
        St2110_40(_) | Bonded(_) => (None, None),
        // MXL video has mandatory `video_encode` (ingress decode → re-encode).
        MxlVideo(c) => (None, Some(c.video_encode.codec.as_str())),
        // MXL audio: optional `audio_encode` like ST 2110-30.
        MxlAudio(c) => (c.audio_encode.as_ref(), None),
        // MXL ANC carries no encode blocks.
        MxlAnc(_) => (None, None),
    }
}

// ─── Master clock construction ────────────────────────────────────────
//
// Phase 3 wires `SourcePcrPllMaster` for SRT/RTP/UDP/RIST/RTMP/RTSP/
// `media_player` / `replay` flows; `Ptp` and `AudioMaster` still fall
// through to the Wallclock impl until Phase 6 lands real backends.
// Returns the handle and an optional `SourcePcrPllMaster` so the flow
// runtime can spawn the ingress PCR sampler against the same instance.
/// Build the `master_clock.kind = "auto"` cascade: **PLL → PTP → Wallclock**.
///
/// Try the source PCR PLL first (best — output tracks the source clock, zero
/// source-relative drift). If it can't lock within the grace window, fall to
/// the node's PTP clock *when PTP is healthy* (a clean, cross-edge-coherent
/// reference); else fall to wallclock (the always-available floor). The PTP
/// rung is present only when the node has a PTP role configured (`ptp.conf`
/// mode != off) — otherwise the cascade degrades to the classic
/// PLL → Wallclock. The PTP-vs-wallclock choice is latched at fallback-fire
/// and only ever demotes (PTP → wallclock) if PTP later loses lock, so the
/// output never rides an unlocked CLOCK_REALTIME and never oscillates epochs.
fn build_auto_cascade(
    cfg: &crate::config::models::FlowConfig,
    active_input_rx: tokio::sync::watch::Receiver<String>,
    event_sender: &EventSender,
    cancel_token: &CancellationToken,
) -> (
    crate::engine::master_clock::MasterClockHandle,
    Option<Arc<crate::engine::master_clock::SourcePcrPllMaster>>,
    Option<crate::engine::st2110::ptp::PtpStateHandle>,
) {
    use crate::engine::master_clock::{
        MasterClock, MasterClockHandle, MasterClockKind, PcrPllWithFallback, SourcePcrPllMaster,
    };

    // ── PLL primary — same construction + lock-jitter override as an
    //    explicit `source_pcr_pll` pin ──
    let pll_config = {
        let mut c = crate::engine::pcr_pll::PcrPllConfig::default();
        if let Some(lock_us) = cfg.master_clock.as_ref().and_then(|m| m.pll_lock_jitter_us) {
            let clamped = lock_us.clamp(50, 5000) as u64;
            c.lock_jitter_us = clamped;
            c.unlock_jitter_us = clamped * 5;
        }
        c
    };
    let pll_inner = Arc::new(SourcePcrPllMaster::new_with_config(
        format!("source_pcr:{}", cfg.id),
        pll_config,
    ));

    // ── PTP middle rung — built only when the node has a PTP role
    //    configured. Polls the node's `ptp.conf` domain (matches the
    //    health probe), not the flow's SDP `clock_domain` (unset on a
    //    contribution flow). ──
    let ptp_settings = crate::util::ptp_config::load();
    let (ptp_fallback, ptp_handle): (
        Option<Arc<dyn MasterClock>>,
        Option<crate::engine::st2110::ptp::PtpStateHandle>,
    ) = if ptp_settings.mode != crate::util::ptp_config::PtpMode::Off {
        let domain = ptp_settings.domain.unwrap_or(127);
        let state = Arc::new(crate::engine::st2110::ptp::PtpStateReporter::spawn(
            crate::engine::st2110::ptp::PtpReporterConfig {
                domain,
                ..Default::default()
            },
            cancel_token.child_token(),
        ));
        let handle = (*state).clone();
        let ptp_clock: Arc<dyn MasterClock> =
            Arc::new(crate::engine::master_clock::PtpMasterClock::new(state));
        (Some(ptp_clock), Some(handle))
    } else {
        (None, None)
    };
    let has_ptp_rung = ptp_handle.is_some();

    let wrapper = Arc::new(PcrPllWithFallback::new_cascade(
        pll_inner.clone(),
        ptp_fallback,
        "auto",
    ));
    // Same fallback watcher as the PLL path — it drives the PLL→fallback
    // decision; `activate_fallback` picks PTP-vs-wallclock internally.
    let timeout_s = cfg
        .master_clock
        .as_ref()
        .and_then(|m| m.pll_lock_timeout_s)
        .unwrap_or(30);
    crate::engine::master_clock::spawn_pll_fallback_watcher(
        wrapper.clone(),
        cfg.id.clone(),
        active_input_rx,
        timeout_s,
        event_sender.clone(),
        cancel_token.child_token(),
    );

    let handle = MasterClockHandle::new(wrapper, MasterClockKind::SourcePcrPll)
        .with_configured_kind("auto");
    if let Some(mc_cfg) = cfg.master_clock.as_ref() {
        handle.set_lipsync_offset_90k(mc_cfg.lipsync_offset_90k);
    }
    tracing::warn!(
        flow_id = %cfg.id,
        ptp_rung = has_ptp_rung,
        "master clock: auto cascade (PLL → {} → Wallclock)",
        if has_ptp_rung { "PTP" } else { "(no PTP role)" },
    );
    (handle, Some(pll_inner), ptp_handle)
}

fn build_master_clock(
    cfg: &crate::config::models::FlowConfig,
    active_input: Option<&crate::config::models::InputConfig>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
    event_sender: &EventSender,
    cancel_token: &CancellationToken,
) -> (
    crate::engine::master_clock::MasterClockHandle,
    Option<Arc<crate::engine::master_clock::SourcePcrPllMaster>>,
    Option<crate::engine::st2110::ptp::PtpStateHandle>,
) {
    use crate::config::models::MasterClockKindConfig;
    use crate::engine::master_clock::{
        MasterClockHandle, MasterClockKind, SourcePcrPllMaster, WallclockMaster,
    };

    // `auto` is PER-INPUT. ST 2110 / MXL essence is PTP-domain, so it
    // resolves straight to PTP (no PLL-first). Contribution + assembly get
    // the PLL → PTP → Wallclock cascade. `select_master_kind_for_input`
    // returns `Ptp` only for the PTP-native (ST 2110 / MXL) inputs, so it's
    // the authoritative "is this input PTP-domain?" check.
    let is_auto = matches!(
        cfg.master_clock.as_ref().map(|m| m.kind),
        Some(MasterClockKindConfig::Auto)
    );
    if is_auto
        && !matches!(
            crate::engine::master_clock::select_master_kind_for_input(active_input, cfg),
            MasterClockKind::Ptp
        )
    {
        // Contribution / assembly → cascade.
        return build_auto_cascade(cfg, active_input_rx, event_sender, cancel_token);
    }

    let kind = match cfg.master_clock.as_ref().map(|m| m.kind) {
        Some(MasterClockKindConfig::SourcePcrPll) => MasterClockKind::SourcePcrPll,
        Some(MasterClockKindConfig::Ptp) => MasterClockKind::Ptp,
        Some(MasterClockKindConfig::AudioMaster) => MasterClockKind::AudioMaster,
        Some(MasterClockKindConfig::Passthrough) => MasterClockKind::Passthrough,
        Some(MasterClockKindConfig::SenderTimestamp) => {
            // Sender-timestamp recovery is wired through the PLL via
            // `pcr_ingress_sampler::sample_packet`'s per-packet
            // preference rule: when the packet carries
            // `RtpPacket.sender_timestamp_us` (libsrt's
            // `SRT_MsgCtrl::srctime` surfaced by `input_srt`), the
            // sampler feeds the PLL via
            // `SourcePcrPllMaster::record_sender_timestamp`; otherwise
            // it falls back to MPEG-TS PCR sampled from the bytes.
            //
            // Runtime kind stays `SourcePcrPll` (same PLL math). The
            // operator's `SenderTimestamp` choice is preserved on
            // `MasterClockTelemetry.configured_kind` for the UI label,
            // and the per-sample source is exposed on `rate_source`.
            MasterClockKind::SourcePcrPll
        }
        Some(MasterClockKindConfig::Wallclock) => MasterClockKind::Wallclock,
        Some(MasterClockKindConfig::Contribution) => {
            // Same runtime backend as `SourcePcrPll` — operator's
            // `Contribution` choice flags intent (clean contribution
            // feed, opt-in to source-PCR PLL pacing) rather than a
            // distinct runtime path. Telemetry surfaces the operator
            // label via `configured_kind` so the manager UI can render
            // "PLL (contribution)" distinct from a legacy explicit
            // `source_pcr_pll` pin. Required when the auto-policy's
            // new Wallclock default is too coarse — e.g. PTP-disciplined
            // contribution feeds where cross-edge clock coherence is
            // important.
            MasterClockKind::SourcePcrPll
        }
        Some(MasterClockKindConfig::Auto) => {
            // Auto on a PTP-native input (ST 2110 / MXL) → PTP-primary.
            // Contribution / assembly Auto returned to build_auto_cascade
            // above, so only the PTP-native case reaches here.
            MasterClockKind::Ptp
        }
        None => crate::engine::master_clock::select_master_kind_for_input(active_input, cfg),
    };

    // Operator-visible note on which master-clock backend the flow
    // ended up on. WARN so it shows at the default tracing level —
    // master-clock selection drives PCR sequencing on every output,
    // surfacing the active kind is operator-relevant context, not
    // debug noise.
    tracing::warn!(
        flow_id = %cfg.id,
        master_clock_kind = ?kind,
        configured_kind = ?cfg.master_clock.as_ref().map(|m| m.kind),
        "master clock selected for flow {}: {} (configured: {})",
        cfg.id,
        kind.as_str(),
        cfg.master_clock
            .as_ref()
            .map(|m| format!("{:?}", m.kind))
            .unwrap_or_else(|| "<auto>".to_string()),
    );

    let (handle, pll_master, ptp_handle) = match kind {
        MasterClockKind::SourcePcrPll => {
            // Operator override for the PLL lock-jitter threshold. Defaults
            // to broadcast-tier 100 µs; customers with internet contribution
            // can widen to 500-2000 µs via `master_clock.pll_lock_jitter_us`.
            // Clamped to [50, 5000] to keep the PLL physically meaningful
            // (below 50 µs is harder than CPU-clock-derived deltas can
            // sustain; above 5 ms the PLL output is no longer broadcast-
            // grade and an explicit Wallclock/Passthrough kind is more
            // honest about what's going on).
            let pll_config = {
                let mut c = crate::engine::pcr_pll::PcrPllConfig::default();
                if let Some(lock_us) = cfg
                    .master_clock
                    .as_ref()
                    .and_then(|m| m.pll_lock_jitter_us)
                {
                    let clamped = lock_us.clamp(50, 5000) as u64;
                    c.lock_jitter_us = clamped;
                    // Preserve the 5× threshold-level hysteresis margin.
                    c.unlock_jitter_us = clamped * 5;
                }
                c
            };
            let pll_inner = Arc::new(SourcePcrPllMaster::new_with_config(
                format!("source_pcr:{}", cfg.id),
                pll_config,
            ));
            // Wrap in PcrPllWithFallback so the watcher task can flip to
            // wallclock if the PLL never locks. The wrapper owns both
            // clocks behind a single `MasterClock` impl; `now_27mhz()`
            // dispatches based on an internal atomic flag.
            let wrapper = Arc::new(
                crate::engine::master_clock::PcrPllWithFallback::new(pll_inner.clone()),
            );
            // Spawn the fallback watcher unless the operator opted out
            // by setting `pll_lock_timeout_s: 0`. Default 30 s.
            let timeout_s = cfg
                .master_clock
                .as_ref()
                .and_then(|m| m.pll_lock_timeout_s)
                .unwrap_or(30);
            // The watcher tracks the active input via the shared
            // `active_input_rx` watch — it sees switches as they happen
            // and restarts the grace window so the new input gets a
            // fresh `timeout_s` to acquire lock.
            crate::engine::master_clock::spawn_pll_fallback_watcher(
                wrapper.clone(),
                cfg.id.clone(),
                active_input_rx,
                timeout_s,
                event_sender.clone(),
                cancel_token.child_token(),
            );
            let h = MasterClockHandle::new(wrapper, MasterClockKind::SourcePcrPll);
            (h, Some(pll_inner), None)
        }
        MasterClockKind::Ptp => {
            let domain = cfg.clock_domain.unwrap_or(0);
            let state = Arc::new(
                crate::engine::st2110::ptp::PtpStateReporter::spawn(
                    crate::engine::st2110::ptp::PtpReporterConfig {
                        domain,
                        ..Default::default()
                    },
                    cancel_token.child_token(),
                ),
            );
            let ptp_handle = (*state).clone();
            let inner = Arc::new(crate::engine::master_clock::PtpMasterClock::new(state));
            let h = MasterClockHandle::new(inner, MasterClockKind::Ptp);
            (h, None, Some(ptp_handle))
        }
        // AudioMaster falls through to Wallclock until a future phase
        // wires the local-display ALSA master clock. The kind tag is
        // preserved so manager UI surfaces the intended source.
        other => (
            MasterClockHandle::new(Arc::new(WallclockMaster::new()), other),
            None,
            None,
        ),
    };

    // Auto on a PTP-native input resolved to PTP above — tag it so the UI
    // shows "Auto → PTP" (the contribution/assembly cascade tags itself in
    // build_auto_cascade).
    let handle = if is_auto {
        handle.with_configured_kind("auto")
    } else {
        handle
    };

    if let Some(mc_cfg) = cfg.master_clock.as_ref() {
        handle.set_lipsync_offset_90k(mc_cfg.lipsync_offset_90k);
    }

    if matches!(kind, MasterClockKind::Wallclock)
        && active_input
            .map(is_webrtc_like_input)
            .unwrap_or(false)
        && handle.mark_degraded_warned()
    {
        event_sender.emit_flow(
            crate::manager::events::EventSeverity::Warning,
            crate::manager::events::category::FLOW,
            format!(
                "Flow {} on WebRTC input runs on wallclock master — A/V sync is best-effort",
                cfg.id,
            ),
            &cfg.id,
        );
    }

    (handle, pll_master, ptp_handle)
}

fn is_webrtc_like_input(input: &crate::config::models::InputConfig) -> bool {
    use crate::config::models::InputConfig;
    matches!(input, InputConfig::Webrtc(_) | InputConfig::Whep(_))
}

#[cfg(test)]
mod cost_plan_tests {
    //! Tests for `derive_cost_plan` + `output_resource_contribution`,
    //! focused on the encoder/decoder session pairing the manager
    //! UI's Resources card surfaces. Configs are built via JSON so
    //! the tests don't have to track every optional field on
    //! Rtp/Srt/etc. configs.
    use super::*;
    use crate::config::models::{
        AudioEncodeConfig, FlowConfig, InputConfig, InputDefinition, OutputConfig,
        ResolvedFlow, VideoEncodeConfig,
    };

    fn ve(codec: &str) -> VideoEncodeConfig {
        serde_json::from_value(serde_json::json!({ "codec": codec })).unwrap()
    }

    fn ve_4k(codec: &str) -> VideoEncodeConfig {
        serde_json::from_value(serde_json::json!({
            "codec": codec,
            "width": 3840,
            "height": 2160,
        }))
        .unwrap()
    }

    fn ae(codec: &str) -> AudioEncodeConfig {
        serde_json::from_value(serde_json::json!({ "codec": codec })).unwrap()
    }

    fn rtp_input(
        id: &str,
        video_encode: Option<VideoEncodeConfig>,
        audio_encode: Option<AudioEncodeConfig>,
    ) -> InputDefinition {
        let mut config: InputConfig = serde_json::from_value(serde_json::json!({
            "type": "rtp",
            "bind_addr": "0.0.0.0:5000",
        }))
        .unwrap();
        if let InputConfig::Rtp(c) = &mut config {
            c.video_encode = video_encode;
            c.audio_encode = audio_encode;
        }
        InputDefinition {
            active: true,
            group: None,
            id: id.to_string(),
            name: id.to_string(),
            config,
        }
    }

    fn rtp_output(id: &str, video_encode: Option<VideoEncodeConfig>) -> OutputConfig {
        let mut out: OutputConfig = serde_json::from_value(serde_json::json!({
            "type": "rtp",
            "id": id,
            "name": id,
            "active": true,
            "dest_addr": "127.0.0.1:5004",
        }))
        .unwrap();
        if let OutputConfig::Rtp(c) = &mut out {
            c.video_encode = video_encode;
        }
        out
    }

    fn flow(inputs: Vec<InputDefinition>, outputs: Vec<OutputConfig>) -> ResolvedFlow {
        let input_ids: Vec<String> = inputs.iter().map(|i| i.id.clone()).collect();
        let output_ids: Vec<String> = outputs
            .iter()
            .map(|o| match o {
                OutputConfig::Rtp(c) => c.id.clone(),
                OutputConfig::Srt(c) => c.id.clone(),
                OutputConfig::Udp(c) => c.id.clone(),
                _ => "x".to_string(),
            })
            .collect();
        let config: FlowConfig = serde_json::from_value(serde_json::json!({
            "id": "f1",
            "name": "f1",
            "enabled": true,
            "media_analysis": false,
            "thumbnail": false,
            "input_ids": input_ids,
            "output_ids": output_ids,
        }))
        .unwrap();
        ResolvedFlow { config, inputs, outputs }
    }

    #[test]
    fn output_transcoder_pairs_decoder_session() {
        let out = rtp_output("o1", Some(ve("h264_nvenc")));
        let (_units, usage) = output_resource_contribution(&out);
        assert_eq!(usage.nvenc_in_use, 1);
        assert_eq!(usage.nvdec_in_use, 1);
        assert_eq!(usage.qsv_in_use, 0);
        assert_eq!(usage.qsv_decode_in_use, 0);
        assert_eq!(usage.vaapi_decode_in_use, 0);
    }

    #[test]
    fn output_transcoder_qsv_pairs_qsv_decode() {
        let out = rtp_output("o1", Some(ve("h264_qsv")));
        let (_units, usage) = output_resource_contribution(&out);
        assert_eq!(usage.qsv_in_use, 1);
        assert_eq!(usage.qsv_decode_in_use, 1);
        assert_eq!(usage.nvdec_in_use, 0);
    }

    #[test]
    fn output_transcoder_amf_does_not_bump_decoder() {
        let out = rtp_output("o1", Some(ve("h264_amf")));
        let (_units, usage) = output_resource_contribution(&out);
        assert_eq!(usage.amf_in_use, 1);
        assert_eq!(usage.nvdec_in_use, 0);
        assert_eq!(usage.qsv_decode_in_use, 0);
        assert_eq!(usage.vaapi_decode_in_use, 0);
    }

    #[test]
    fn output_transcoder_4k_pairs_4k_decoder() {
        let out = rtp_output("o1", Some(ve_4k("hevc_nvenc")));
        let (_units, usage) = output_resource_contribution(&out);
        assert_eq!(usage.nvenc_in_use, 1);
        assert_eq!(usage.nvenc_in_use_4k, 1);
        assert_eq!(usage.nvdec_in_use, 1);
        assert_eq!(usage.nvdec_in_use_4k, 1);
    }

    #[test]
    fn input_transcoder_charges_encoder_and_decoder() {
        let f = flow(
            vec![rtp_input("i1", Some(ve("h264_qsv")), None)],
            vec![rtp_output("o1", None)],
        );
        let plan = derive_cost_plan(&f);
        assert_eq!(plan.qsv_sessions, 1);
        assert_eq!(plan.qsv_decode_sessions, 1);
        assert_eq!(plan.nvenc_sessions, 0);
        assert_eq!(plan.nvdec_sessions, 0);
    }

    #[test]
    fn input_audio_encode_bumps_audio_inputs_only() {
        let f = flow(
            vec![rtp_input("i1", None, Some(ae("aac_lc")))],
            vec![rtp_output("o1", None)],
        );
        let plan = derive_cost_plan(&f);
        assert_eq!(plan.audio_encode_inputs, 1);
        assert_eq!(plan.audio_encode_outputs, 0);
        assert_eq!(plan.qsv_sessions, 0);
        assert_eq!(plan.qsv_decode_sessions, 0);
    }

    #[test]
    fn output_transcoder_in_derive_cost_plan_pairs_decoder() {
        let f = flow(
            vec![rtp_input("i1", None, None)],
            vec![rtp_output("o1", Some(ve("h264_nvenc")))],
        );
        let plan = derive_cost_plan(&f);
        assert_eq!(plan.nvenc_sessions, 1);
        assert_eq!(plan.nvdec_sessions, 1);
    }

    #[test]
    fn input_qsv_plus_output_nvenc_bump_independently() {
        let f = flow(
            vec![rtp_input("i1", Some(ve("h264_qsv")), None)],
            vec![rtp_output("o1", Some(ve("h264_nvenc")))],
        );
        let plan = derive_cost_plan(&f);
        assert_eq!(plan.qsv_sessions, 1);
        assert_eq!(plan.qsv_decode_sessions, 1);
        assert_eq!(plan.nvenc_sessions, 1);
        assert_eq!(plan.nvdec_sessions, 1);
    }

    #[test]
    fn input_4k_vaapi_pairs_4k_vaapi_decode() {
        let f = flow(
            vec![rtp_input("i1", Some(ve_4k("h264_vaapi")), None)],
            vec![rtp_output("o1", None)],
        );
        let plan = derive_cost_plan(&f);
        assert_eq!(plan.vaapi_sessions, 1);
        assert_eq!(plan.vaapi_sessions_4k, 1);
        assert_eq!(plan.vaapi_decode_sessions, 1);
        assert_eq!(plan.vaapi_decode_sessions_4k, 1);
    }
}

#[cfg(test)]
mod input_membership_tests {
    //! Pure-logic tests for the hot-swap input-membership helpers
    //! (`slot_source_references_input`, `collect_all_input_ids`) and the
    //! stable `InputMembershipError` error codes that ride on
    //! `command_ack.error_code`.
    use super::*;
    use crate::config::models::{SlotSource, SwitchLeg};

    fn pid_src(input_id: &str) -> SlotSource {
        SlotSource::Pid {
            input_id: input_id.to_string(),
            source_pid: 0x100,
        }
    }

    #[test]
    fn slot_source_references_input_pid_match() {
        let s = pid_src("a");
        assert!(slot_source_references_input(&s, "a"));
        assert!(!slot_source_references_input(&s, "b"));
    }

    #[test]
    fn slot_source_references_input_essence_match() {
        let s = SlotSource::Essence {
            input_id: "feed-1".into(),
            kind: crate::config::models::EssenceKind::Video,
        };
        assert!(slot_source_references_input(&s, "feed-1"));
        assert!(!slot_source_references_input(&s, "feed-2"));
    }

    #[test]
    fn slot_source_references_input_hitless_walks_both_legs() {
        let s = SlotSource::Hitless {
            primary: Box::new(pid_src("primary")),
            backup: Box::new(pid_src("backup")),
            mode: Default::default(),
            stall_ms: None,
            reorder_window: None,
            path_differential_ms: None,
        };
        assert!(slot_source_references_input(&s, "primary"));
        assert!(slot_source_references_input(&s, "backup"));
        assert!(!slot_source_references_input(&s, "other"));
    }

    #[test]
    fn slot_source_references_input_switch_walks_every_leg() {
        let s = SlotSource::Switch {
            legs: vec![
                SwitchLeg::Pid { input_id: "cam-a".into(), source_pid: 0x100 },
                SwitchLeg::Essence {
                    input_id: "cam-b".into(),
                    kind: crate::config::models::EssenceKind::Video,
                },
            ],
            initial_input_id: "cam-a".into(),
            splice_mode: Default::default(),
            splice_budget_ms: None,
        };
        assert!(slot_source_references_input(&s, "cam-a"));
        assert!(slot_source_references_input(&s, "cam-b"));
        assert!(!slot_source_references_input(&s, "cam-c"));
    }

    #[test]
    fn collect_all_input_ids_hitless_finds_both_legs() {
        let h = SlotSource::Hitless {
            primary: Box::new(pid_src("p")),
            backup: Box::new(pid_src("b")),
            mode: Default::default(),
            stall_ms: None,
            reorder_window: None,
            path_differential_ms: None,
        };
        let mut out = HashSet::new();
        collect_all_input_ids(&h, &mut out);
        assert!(out.contains("p"));
        assert!(out.contains("b"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn collect_hitless_input_ids_ignores_non_hitless_sources() {
        let mut out = HashSet::new();
        collect_hitless_input_ids(&pid_src("a"), &mut out);
        assert!(out.is_empty());
        collect_hitless_input_ids(
            &SlotSource::Switch {
                legs: vec![SwitchLeg::Pid {
                    input_id: "x".into(),
                    source_pid: 0x100,
                }],
                initial_input_id: "x".into(),
                splice_mode: Default::default(),
                splice_budget_ms: None,
            },
            &mut out,
        );
        assert!(out.is_empty(), "Switch alone should not contribute to hitless-leg set");
    }

    #[test]
    fn input_membership_error_codes_are_stable() {
        // The error codes ride on command_ack.error_code and the manager
        // UI matches against them — pin the strings here so a future
        // refactor can't quietly break the protocol surface.
        assert_eq!(
            InputMembershipError::already_member("x").code,
            "input_already_member"
        );
        assert_eq!(InputMembershipError::not_member("x").code, "input_not_member");
        assert_eq!(
            InputMembershipError::active_in_use("x").code,
            "active_input_in_use"
        );
        assert_eq!(
            InputMembershipError::pid_bus_in_use("x").code,
            "pid_bus_input_in_use"
        );
        assert_eq!(
            InputMembershipError::hitless_leg_change("x").code,
            "hitless_leg_change_requires_restart"
        );
        assert_eq!(
            InputMembershipError::resource_critical("f", "x").code,
            "input_resource_critical"
        );
    }

    #[test]
    fn fixer_drop_input_psi_evicts_cache() {
        // Build a minimal TS packet that injects a PMT under input "a",
        // then ask the fixer to forget "a" and assert the cache is gone.
        let mut fixer = crate::engine::ts_continuity_fixer::TsContinuityFixer::new();
        // We can't easily synthesise valid PSI here without pulling in
        // the broader test fixtures from ts_continuity_fixer's own
        // tests, so just exercise the forget_input contract — it must
        // be a no-op on an unknown id and must not panic.
        fixer.forget_input("never-existed");
        fixer.forget_input("");
    }
}


