// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use bytes::Bytes;
use tokio::sync::{broadcast, watch};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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
use super::packet::{BROADCAST_CHANNEL_CAPACITY, RtpPacket};
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
use super::ts_es_bus::{FlowEsBus, TsEsDemuxer};
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
///   channel is bounded to [`BROADCAST_CHANNEL_CAPACITY`] slots; slow
///   receivers that fall behind will receive a `Lagged` error and lose
///   packets rather than blocking the input.
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
    pub es_bus: Option<Arc<FlowEsBus>>,
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
    pub async fn start(config: ResolvedFlow, global_stats: &crate::stats::collector::StatsCollector, ffmpeg_available: bool, event_sender: EventSender) -> Result<Self> {
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
        // Shared ES bus for the SPTS runtime. Constructed only when a
        // plan is present; non-assembly flows pay zero cost (no Arc, no
        // DashMap). The bus lives as long as the flow, shared by every
        // per-input TS demuxer and the single assembler task.
        let es_bus: Option<Arc<FlowEsBus>> = spts_build
            .as_ref()
            .map(|_| Arc::new(FlowEsBus::new()));
        let cancel_token = CancellationToken::new();
        // The broadcast channel is bounded to BROADCAST_CHANNEL_CAPACITY slots.
        // When a slow output (receiver) cannot keep up, it will *not* block the
        // input or other outputs. Instead, the lagging receiver's next `recv()`
        // returns `RecvError::Lagged(n)`, telling it how many messages it missed.
        // Each output handles this by incrementing its `packets_dropped` stat.
        // The underscore `_` receiver is created and immediately dropped; it is
        // only needed to satisfy the channel constructor signature.
        let (broadcast_tx, _) = broadcast::channel::<RtpPacket>(BROADCAST_CHANNEL_CAPACITY);

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
        // Channel capacity matches the per-flow `BROADCAST_CHANNEL_CAPACITY`
        // so drop-on-full semantics line up with the upstream broadcast
        // drop-on-lag behaviour.
        let (fixer_tx, fixer_rx) = tokio::sync::mpsc::channel::<FixerCommand>(
            BROADCAST_CHANNEL_CAPACITY,
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
        for input_def in &config.inputs {
            let input_id = input_def.id.clone();
            let input_cancel = cancel_token.child_token();
            let (per_input_tx, _) = broadcast::channel::<RtpPacket>(BROADCAST_CHANNEL_CAPACITY);
            let force_idr = Arc::new(std::sync::atomic::AtomicBool::new(false));

            // Register per-input liveness counters. The forwarder increments
            // these on every packet arriving from this input's broadcast
            // channel so the manager UI can surface NO SIGNAL / feed-present
            // state for passive inputs too. The `InputConfigMeta` attached
            // here is what the snapshot path lifts into `PerInputLive` so
            // the manager topology view can draw upstream links for every
            // input of an assembled flow — not only the single active one.
            let per_input_counters = flow_stats.register_input_counters(
                &input_id,
                input_type_str(&input_def.config),
                build_input_config_meta(&input_def.config),
            );

            let (input_handle, this_whip_info) = spawn_single_input(
                input_def,
                &config.config.id,
                &per_input_tx,
                &flow_stats,
                &input_cancel,
                &event_sender,
                &force_idr,
                config.config.clock_domain,
                #[cfg(feature = "replay")]
                &replay_command_txs,
            );

            // Spawn the forwarder: drains the per-input channel and forwards
            // onto the main broadcast channel iff this input's ID matches the
            // current value in the active-input watch. The shared continuity
            // fixer ensures CC counters are seamless across input switches.
            // Spawn per-input PSI catalogue observer for TS-bearing inputs.
            // Runs independently of flow activation so passive inputs still
            // surface their programs and PIDs to the manager / UI. Must happen
            // *before* the forwarder consumes `per_input_counters`.
            if input_def.config.is_ts_carrier() {
                let _ = super::ts_psi_catalog::spawn_psi_catalog_observer(
                    input_id.clone(),
                    config.config.id.clone(),
                    per_input_tx.clone(),
                    per_input_counters.psi_catalog.clone(),
                    input_cancel.child_token(),
                );
            }

            // In SPTS assembly mode, the per-input forwarder is replaced
            // by a TS-ES demuxer consumer that publishes elementary
            // streams onto the shared [`FlowEsBus`]. The assembler (one
            // per flow, spawned below) pulls ES from the bus, rewrites
            // PIDs, synthesises PAT/PMT, and is the sole publisher onto
            // `broadcast_tx`. The active-input watch is unused in this
            // mode — every input contributes ES simultaneously.
            let forwarder_handle = if let Some(ref bus) = es_bus {
                spawn_ts_es_demuxer_consumer(
                    input_id.clone(),
                    per_input_tx.subscribe(),
                    bus.clone(),
                    input_cancel.clone(),
                    per_input_counters,
                )
            } else {
                spawn_input_forwarder(
                    input_id.clone(),
                    per_input_tx.subscribe(),
                    active_input_watch_rx.clone(),
                    input_cancel.clone(),
                    fixer_tx.clone(),
                    force_idr.clone(),
                    per_input_counters,
                )
            };

            // Spawn per-input thumbnail generator (subscribes to the input's
            // own broadcast channel so it captures this source's video even
            // when the input is passive / not currently active).
            let thumbnail_handle = if config.config.thumbnail && ffmpeg_available {
                let thumb_acc = Arc::new(ThumbnailAccumulator::new_with_update_notify(
                    global_stats.thumbnail_update_notify.clone(),
                ));
                flow_stats.per_input_thumbnails.insert(input_id.clone(), thumb_acc.clone());
                Some(spawn_thumbnail_generator(
                    &per_input_tx,
                    thumb_acc,
                    input_cancel.child_token(),
                    None, // no program filter for per-input thumbnails (pre-filter stream)
                ))
            } else {
                None
            };

            #[cfg(feature = "webrtc")]
            {
                if this_whip_info.is_some() {
                    whip_session_info = this_whip_info;
                }
            }
            #[cfg(not(feature = "webrtc"))]
            let _ = this_whip_info;

            input_handles.insert(
                input_id,
                InputRuntime {
                    input_handle,
                    forwarder_handle,
                    cancel_token: input_cancel,
                    thumbnail_handle,
                },
            );
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

        Ok(Self {
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
            frame_rate_rx,
            pid_bus_assembler_handle,
            es_bus,
            current_assembly,
            #[cfg(feature = "replay")]
            recording_handle,
            #[cfg(feature = "replay")]
            replay_command_txs,
        })
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
                "flow '{}' assembler is running but has no FlowEsBus — internal invariant violated",
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
        if !build.pending_essence.is_empty() {
            let mut catalogues: std::collections::HashMap<
                String,
                Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>,
            > = std::collections::HashMap::new();
            for p in &build.pending_essence {
                if !catalogues.contains_key(&p.input_id) {
                    if let Some(c) = self.stats.per_input_counters.get(&p.input_id) {
                        catalogues.insert(p.input_id.clone(), c.psi_catalog.clone());
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
                    for ((program_idx, slot_idx), pid) in pairs {
                        if let Some(prog) = build.plan.programs.get_mut(program_idx) {
                            if let Some(slot) = prog.slots.get_mut(slot_idx) {
                                slot.source.1 = pid;
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

        // Spawn any Hitless mergers the new plan introduced. The
        // assembler picks up the synthetic bus key as soon as its
        // `ReplacePlan` lands.
        for hl in &build.pending_hitless {
            let _ = crate::engine::ts_es_hitless::spawn_hitless_es_merger(
                hl.uid.clone(),
                hl.primary.clone(),
                hl.backup.clone(),
                bus.clone(),
                std::time::Duration::from_millis(hl.stall_ms),
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
        if let Some(new_def) = self
            .config
            .inputs
            .iter()
            .find(|d| d.id == new_input_id)
        {
            let new_input_type = input_type_str(&new_def.config);
            let new_meta = build_input_config_meta(&new_def.config);
            self.stats.update_active_input_meta(new_input_type, new_meta);
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

        let active_input_cfg = self.config.active_input().map(|d| &d.config);
        let input_audio_format = active_input_cfg
            .and_then(crate::engine::audio_transcode::InputFormat::from_input_config);
        let compressed_audio_input = active_input_cfg
            .map_or(false, crate::engine::audio_decode::input_can_carry_ts_audio);
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
        ).await?;

        // Update output config metadata so stats snapshots reflect the new address/port
        flow_stats.output_config_meta.insert(
            output_id.clone(),
            build_output_config_meta(&output_config),
        );

        let mut handles = self.output_handles.write().await;
        handles.insert(output_id.clone(), output_rt);

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
            tracing::info!("Removed output '{}' from flow '{}'", output_id, self.config.config.id);
            Ok(())
        } else {
            bail!("Output '{}' not found in flow '{}'", output_id, self.config.config.id);
        }
    }

    /// Stop the entire flow by cancelling the parent token.
    ///
    /// This signals both the input task and every output task to shut down.
    /// Because output cancel tokens are children of the flow token, a single
    /// cancel propagates to all tasks. The actual task cleanup (socket close,
    /// thread join) happens asynchronously inside each task's select loop.
    pub async fn stop(&self) {
        tracing::info!("Stopping flow '{}' ({})", self.config.config.id, self.config.config.name);
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
    #[cfg(feature = "replay")]
    replay_command_txs: &dashmap::DashMap<String, tokio::sync::mpsc::Sender<crate::replay::ReplayCommand>>,
) -> (JoinHandle<()>, Option<WhipSessionInfo>) {
    let input_id = input_def.id.clone();
    let mut whip_info: Option<WhipSessionInfo> = None;

    let handle = match &input_def.config {
        InputConfig::Rtp(rtp_config) => spawn_rtp_input(
            rtp_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(),
        ),
        InputConfig::Udp(udp_config) => spawn_udp_input(
            udp_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(),
        ),
        InputConfig::Srt(srt_config) => spawn_srt_input(
            srt_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(),
        ),
        InputConfig::Rist(rist_config) => spawn_rist_input(
            rist_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(),
        ),
        InputConfig::Rtmp(rtmp_config) => spawn_rtmp_input(
            rtmp_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(),
        ),
        InputConfig::Rtsp(rtsp_config) => super::input_rtsp::spawn_rtsp_input(
            rtsp_config.clone(), per_input_tx.clone(), flow_stats.clone(),
            input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            input_id.clone(), force_idr.clone(),
        ),
        #[cfg(feature = "webrtc")]
        InputConfig::Webrtc(webrtc_config) => {
            let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
            whip_info = Some((session_tx, webrtc_config.bearer_token.clone()));
            super::input_webrtc::spawn_whip_input(
                webrtc_config.clone(), flow_id.to_string(), input_id.clone(),
                per_input_tx.clone(), flow_stats.clone(), input_cancel.clone(),
                session_rx, event_sender.clone(), force_idr.clone(),
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
            input_id.clone(), force_idr.clone(),
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
                c, per_input_tx.clone(), flow_stats.clone(),
                input_cancel.clone(), event_sender.clone(), flow_id.to_string(),
            )
        }
        InputConfig::St2110_31(c) => {
            let mut c = c.clone();
            c.clock_domain = c.clock_domain.or(flow_clock_domain);
            super::input_st2110_31::spawn_st2110_31_input(
                c, per_input_tx.clone(), flow_stats.clone(),
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
            c.clone(), per_input_tx.clone(), flow_stats.clone(),
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
    };

    (handle, whip_info)
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
    }
}

/// Command queued to the per-flow [`ts_fixer_task`] by an input forwarder.
///
/// Forwarders never touch the `TsContinuityFixer` directly — they enqueue
/// one of these variants and let the fixer task own the state uncontended.
enum FixerCommand {
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
                            let _ = out_tx.send(RtpPacket {
                                data: fixed_data,
                                sequence_number: pkt.sequence_number,
                                rtp_timestamp: pkt.rtp_timestamp,
                                recv_time_us: pkt.recv_time_us,
                                is_raw_ts: pkt.is_raw_ts,
                            });
                        }
                        ProcessResult::Unchanged => {
                            let _ = out_tx.send(pkt);
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
                        let _ = out_tx.send(inj_pkt);
                    }
                }
                Some(FixerCommand::Keepalive) => {
                    let _ = out_tx.send(null_ts_packet());
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
                            let _ = fixer_tx.try_send(FixerCommand::Switch { input_id: input_id.clone() });
                            // Ask any ingress video re-encoder on this input
                            // to emit an IDR on its next frame. Passthrough
                            // inputs ignore the flag (no encoder to signal).
                            force_idr.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        was_active = is_active;

                        if is_active {
                            // Drop on full — matches broadcast channel drop-on-lag.
                            let _ = fixer_tx.try_send(FixerCommand::ActivePacket {
                                input_id: input_id.clone(),
                                pkt,
                            });
                            keepalive.reset();
                        } else {
                            let _ = fixer_tx.try_send(FixerCommand::PassivePacket {
                                input_id: input_id.clone(),
                                pkt,
                            });
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Forwarder falling behind its per-input channel —
                        // skip missed packets but keep the task alive.
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = keepalive.tick() => {
                    // Only emit keepalive when *this* input is the active
                    // one. Otherwise every passive forwarder would spam
                    // null packets into the broadcast channel.
                    if *active_input_rx.borrow() == input_id {
                        let _ = fixer_tx.try_send(FixerCommand::Keepalive);
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
    bus: &Arc<FlowEsBus>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
    cancel_token: &CancellationToken,
    event_sender: &EventSender,
    flow_id: &str,
) -> Result<crate::engine::ts_assembler::AssemblerHandle> {
    // 1. Essence resolution.
    if !build.pending_essence.is_empty() {
        let mut catalogues: std::collections::HashMap<
            String,
            Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>,
        > = std::collections::HashMap::new();
        for p in &build.pending_essence {
            if !catalogues.contains_key(&p.input_id) {
                if let Some(c) = flow_stats.per_input_counters.get(&p.input_id) {
                    catalogues.insert(p.input_id.clone(), c.psi_catalog.clone());
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
                for ((program_idx, slot_idx), pid) in pairs {
                    if let Some(prog) = build.plan.programs.get_mut(program_idx) {
                        if let Some(slot) = prog.slots.get_mut(slot_idx) {
                            slot.source.1 = pid;
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
        let _ = crate::engine::ts_es_hitless::spawn_hitless_es_merger(
            hl.uid.clone(),
            hl.primary.clone(),
            hl.backup.clone(),
            bus.clone(),
            std::time::Duration::from_millis(hl.stall_ms),
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
    Ok(spawn_spts_assembler(
        build.plan,
        bus.clone(),
        broadcast_tx.clone(),
        cancel_token.child_token(),
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
        for stream in &program.streams {
            let (input_id, source_pid) = match &stream.source {
                SlotSource::Pid { input_id, source_pid } => (input_id.clone(), *source_pid),
                SlotSource::Essence { input_id, kind } => {
                    let slot_idx = slots.len();
                    pending_essence.push(PendingEssenceSlot {
                        program_idx,
                        slot_idx,
                        input_id: input_id.clone(),
                        kind: es_kind_from_config(*kind),
                    });
                    (input_id.clone(), 0_u16) // sentinel, patched after resolution
                }
                SlotSource::Hitless { primary, backup } => {
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
                    pending_hitless.push(crate::engine::ts_assembler::PendingHitlessSlot {
                        uid: uid.clone(),
                        primary: primary_pair,
                        backup: backup_pair,
                        stall_ms: crate::engine::ts_es_hitless::DEFAULT_STALL_MS,
                    });
                    // The assembler reads from the synthetic merger
                    // output. The pre-bus merger task is responsible
                    // for keeping that key supplied.
                    crate::engine::ts_es_hitless::hitless_bus_key(&uid)
                }
            };
            // Phase 6.5 cross-check: when the slot's input is a PCM /
            // AES3 input with `audio_encode` set, the codec determines
            // the stream_type the input will actually publish. Fail
            // loudly if the operator declared a different stream_type
            // on the slot — otherwise the assembler would advertise one
            // thing in the synthesised PMT and the input would emit
            // PES bytes that don't match it.
            if let Some(expected_st) =
                expected_stream_type_for_synthesised_input(&flow.inputs, &input_id)
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
                        input_id,
                        expected_st,
                    );
                    emit(
                        "pid_bus_spts_stream_type_mismatch",
                        &msg,
                        serde_json::json!({
                            "program_number": program.program_number,
                            "out_pid": stream.out_pid,
                            "input_id": input_id,
                            "declared_stream_type": stream.stream_type,
                            "expected_stream_type": expected_st,
                        }),
                    );
                    bail!(msg);
                }
            }

            slots.push(AssemblySlot {
                source: (input_id, source_pid),
                out_pid: stream.out_pid,
                stream_type: stream.stream_type,
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
/// shared [`FlowEsBus`]. Replaces [`spawn_input_forwarder`] for flows
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
    bus: Arc<FlowEsBus>,
    cancel: CancellationToken,
    per_input_counters: Arc<crate::stats::collector::PerInputCounters>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut demuxer = TsEsDemuxer::new(input_id.clone(), bus);
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
                        // Slow consumer — drop on the floor. PID-bus
                        // subscribers are independent per-PID channels
                        // with their own Lagged handling; no correlation
                        // state to reset at the demuxer level.
                        tracing::debug!(
                            "ts_es_demuxer_consumer '{}': lagged {} packets",
                            input_id, n
                        );
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
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
    }
}
