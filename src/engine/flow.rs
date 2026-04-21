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
use crate::manager::events::EventSender;
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

        // Shared TS continuity fixer for seamless input switching. Ensures
        // CC counters are continuous and discontinuity_indicator is set when
        // switching between inputs, preventing external decoders from losing lock.
        let continuity_fixer = Arc::new(std::sync::Mutex::new(TsContinuityFixer::new()));

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
        for input_def in &config.inputs {
            let input_id = input_def.id.clone();
            let input_cancel = cancel_token.child_token();
            let (per_input_tx, _) = broadcast::channel::<RtpPacket>(BROADCAST_CHANNEL_CAPACITY);
            let force_idr = Arc::new(std::sync::atomic::AtomicBool::new(false));

            // Register per-input liveness counters. The forwarder increments
            // these on every packet arriving from this input's broadcast
            // channel so the manager UI can surface NO SIGNAL / feed-present
            // state for passive inputs too.
            let per_input_counters = flow_stats
                .register_input_counters(&input_id, input_type_str(&input_def.config));

            #[cfg(feature = "webrtc")]
            let mut this_whip_info: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)> = None;

            let input_handle = match &input_def.config {
                InputConfig::Rtp(rtp_config) => {
                    spawn_rtp_input(
                        rtp_config.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                        input_id.clone(),
                        force_idr.clone(),
                    )
                }
                InputConfig::Udp(udp_config) => {
                    spawn_udp_input(
                        udp_config.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                        input_id.clone(),
                        force_idr.clone(),
                    )
                }
                InputConfig::Srt(srt_config) => {
                    spawn_srt_input(
                        srt_config.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                        input_id.clone(),
                        force_idr.clone(),
                    )
                }
                InputConfig::Rist(rist_config) => {
                    spawn_rist_input(
                        rist_config.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                        input_id.clone(),
                        force_idr.clone(),
                    )
                }
                InputConfig::Rtmp(rtmp_config) => {
                    spawn_rtmp_input(
                        rtmp_config.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                        input_id.clone(),
                        force_idr.clone(),
                    )
                }
                InputConfig::Rtsp(rtsp_config) => {
                    super::input_rtsp::spawn_rtsp_input(
                        rtsp_config.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                        input_id.clone(),
                        force_idr.clone(),
                    )
                }
                #[cfg(feature = "webrtc")]
                InputConfig::Webrtc(webrtc_config) => {
                    let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
                    this_whip_info = Some((session_tx, webrtc_config.bearer_token.clone()));
                    super::input_webrtc::spawn_whip_input(
                        webrtc_config.clone(),
                        config.config.id.clone(),
                        input_id.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        session_rx,
                        event_sender.clone(),
                        force_idr.clone(),
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
                InputConfig::Whep(whep_config) => {
                    super::input_webrtc::spawn_whep_input(
                        whep_config.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                        input_id.clone(),
                        force_idr.clone(),
                    )
                }
                #[cfg(not(feature = "webrtc"))]
                InputConfig::Whep(_) => {
                    let cancel = input_cancel.clone();
                    tokio::spawn(async move {
                        tracing::warn!("WHEP input requires the `webrtc` cargo feature");
                        cancel.cancelled().await;
                    })
                }
                // The PTP clock_domain may be set on the flow itself or on the
                // per-essence input config. The cloned local inherits the
                // flow-level value when the essence does not override it.
                InputConfig::St2110_30(c) => {
                    let mut c = c.clone();
                    c.clock_domain = c.clock_domain.or(config.config.clock_domain);
                    super::input_st2110_30::spawn_st2110_30_input(
                        c,
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                    )
                }
                InputConfig::St2110_31(c) => {
                    let mut c = c.clone();
                    c.clock_domain = c.clock_domain.or(config.config.clock_domain);
                    super::input_st2110_31::spawn_st2110_31_input(
                        c,
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                    )
                }
                InputConfig::St2110_40(c) => {
                    let mut c = c.clone();
                    c.clock_domain = c.clock_domain.or(config.config.clock_domain);
                    super::input_st2110_40::spawn_st2110_40_input(
                        c,
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                        event_sender.clone(),
                        config.config.id.clone(),
                    )
                }
                InputConfig::RtpAudio(c) => super::input_rtp_audio::spawn_rtp_audio_input(
                    c.clone(),
                    per_input_tx.clone(),
                    flow_stats.clone(),
                    input_cancel.clone(),
                    Some(event_sender.clone()),
                    Some(config.config.id.clone()),
                ),
                InputConfig::St2110_20(c) => {
                    let mut c = c.clone();
                    c.clock_domain = c.clock_domain.or(config.config.clock_domain);
                    super::input_st2110_20::spawn_st2110_20_input(
                        c,
                        input_id.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                    )
                }
                InputConfig::St2110_23(c) => {
                    let mut c = c.clone();
                    c.clock_domain = c.clock_domain.or(config.config.clock_domain);
                    super::input_st2110_23::spawn_st2110_23_input(
                        c,
                        input_id.clone(),
                        per_input_tx.clone(),
                        flow_stats.clone(),
                        input_cancel.clone(),
                    )
                }
                InputConfig::Bonded(c) => super::input_bonded::spawn_bonded_input(
                    c.clone(),
                    per_input_tx.clone(),
                    flow_stats.clone(),
                    input_cancel.clone(),
                    event_sender.clone(),
                    config.config.id.clone(),
                    input_id.clone(),
                ),
                InputConfig::TestPattern(c) => super::input_test_pattern::spawn_test_pattern_input(
                    c.clone(),
                    per_input_tx.clone(),
                    flow_stats.clone(),
                    input_cancel.clone(),
                    event_sender.clone(),
                    config.config.id.clone(),
                    input_id.clone(),
                ),
            };

            // Spawn the forwarder: drains the per-input channel and forwards
            // onto the main broadcast channel iff this input's ID matches the
            // current value in the active-input watch. The shared continuity
            // fixer ensures CC counters are seamless across input switches.
            let forwarder_handle = spawn_input_forwarder(
                input_id.clone(),
                per_input_tx.subscribe(),
                broadcast_tx.clone(),
                active_input_watch_rx.clone(),
                input_cancel.clone(),
                continuity_fixer.clone(),
                force_idr.clone(),
                per_input_counters,
            );

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
            let (protocol, payload_format, fec_enabled, fec_type, redundancy_enabled, redundancy_type) =
                match active_input_cfg.unwrap() {
                    InputConfig::Rtp(rtp) => {
                        let (fec_en, fec_ty) = if let Some(fec) = &rtp.fec_decode {
                            (true, Some(format!("SMPTE 2022-1 (L={}, D={})", fec.columns, fec.rows)))
                        } else {
                            (false, None)
                        };
                        let (red_en, red_ty) = if rtp.redundancy.is_some() {
                            (true, Some("SMPTE 2022-7".to_string()))
                        } else {
                            (false, None)
                        };
                        ("rtp".to_string(), "rtp_ts".to_string(), fec_en, fec_ty, red_en, red_ty)
                    }
                    InputConfig::Udp(_) => {
                        ("udp".to_string(), "raw_ts".to_string(), false, None, false, None)
                    }
                    InputConfig::Srt(srt) => {
                        let (red_en, red_ty) = if srt.redundancy.is_some() {
                            (true, Some("SMPTE 2022-7".to_string()))
                        } else {
                            (false, None)
                        };
                        ("srt".to_string(), "unknown".to_string(), false, None, red_en, red_ty)
                    }
                    InputConfig::Rist(rist) => {
                        let (red_en, red_ty) = if rist.redundancy.is_some() {
                            (true, Some("SMPTE 2022-7".to_string()))
                        } else {
                            (false, None)
                        };
                        ("rist".to_string(), "raw_ts".to_string(), false, None, red_en, red_ty)
                    }
                    InputConfig::Rtmp(_) => {
                        ("rtmp".to_string(), "raw_ts".to_string(), false, None, false, None)
                    }
                    InputConfig::Rtsp(_) => {
                        ("rtsp".to_string(), "raw_ts".to_string(), false, None, false, None)
                    }
                    InputConfig::Webrtc(_) => {
                        ("webrtc".to_string(), "rtp_h264".to_string(), false, None, false, None)
                    }
                    InputConfig::Whep(_) => {
                        ("whep".to_string(), "rtp_h264".to_string(), false, None, false, None)
                    }
                    InputConfig::St2110_30(c) => {
                        let (red_en, red_ty) = if c.redundancy.is_some() {
                            (true, Some("SMPTE 2022-7".to_string()))
                        } else {
                            (false, None)
                        };
                        ("st2110_30".to_string(), "pcm_l24".to_string(), false, None, red_en, red_ty)
                    }
                    InputConfig::St2110_31(c) => {
                        let (red_en, red_ty) = if c.redundancy.is_some() {
                            (true, Some("SMPTE 2022-7".to_string()))
                        } else {
                            (false, None)
                        };
                        ("st2110_31".to_string(), "aes3".to_string(), false, None, red_en, red_ty)
                    }
                    InputConfig::St2110_40(c) => {
                        let (red_en, red_ty) = if c.redundancy.is_some() {
                            (true, Some("SMPTE 2022-7".to_string()))
                        } else {
                            (false, None)
                        };
                        ("st2110_40".to_string(), "anc".to_string(), false, None, red_en, red_ty)
                    }
                    InputConfig::St2110_20(c) => {
                        let (red_en, red_ty) = if c.redundancy.is_some() {
                            (true, Some("SMPTE 2022-7".to_string()))
                        } else {
                            (false, None)
                        };
                        ("st2110_20".to_string(), "rfc4175_ycbcr422".to_string(), false, None, red_en, red_ty)
                    }
                    InputConfig::St2110_23(_c) => {
                        ("st2110_23".to_string(), "rfc4175_ycbcr422_multi".to_string(), false, None, true, Some("SMPTE 2110-23 multi-stream".to_string()))
                    }
                    InputConfig::RtpAudio(c) => {
                        let (red_en, red_ty) = if c.redundancy.is_some() {
                            (true, Some("SMPTE 2022-7".to_string()))
                        } else {
                            (false, None)
                        };
                        let payload_fmt = match c.bit_depth {
                            16 => "pcm_l16".to_string(),
                            _ => "pcm_l24".to_string(),
                        };
                        ("rtp_audio".to_string(), payload_fmt, false, None, red_en, red_ty)
                    }
                    InputConfig::Bonded(c) => {
                        // Bonded inputs aggregate N paths — surface the
                        // path count as the "redundancy" signal the UI
                        // uses for multi-path flows.
                        let multi = c.paths.len() > 1;
                        (
                            "bonded".to_string(),
                            "raw_ts".to_string(),
                            false,
                            None,
                            multi,
                            if multi {
                                Some(format!("bonded ({} paths)", c.paths.len()))
                            } else {
                                None
                            },
                        )
                    }
                    InputConfig::TestPattern(_) => (
                        "test_pattern".to_string(),
                        "h264_aac_ts".to_string(),
                        false,
                        None,
                        false,
                        None,
                    ),
                };
            let media_acc = Arc::new(MediaAnalysisAccumulator::new(
                protocol,
                payload_format,
                fec_enabled,
                fec_type,
                redundancy_enabled,
                redundancy_type,
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
            bandwidth_monitor_handle,
            degradation_monitor_handle,
            #[cfg(feature = "webrtc")]
            whip_session_tx: whip_session_info,
            #[cfg(feature = "webrtc")]
            whep_session_tx: whep_session_info,
            event_sender,
            frame_rate_rx,
        })
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
    }
}

/// Spawn the forwarder task for a single input. The forwarder drains the
/// per-input broadcast channel and re-publishes packets onto the flow's main
/// broadcast channel only while the `active_input_rx` watch value equals this
/// input's ID. Passive inputs' packets are silently dropped — the per-input
/// task keeps running so its stats continue to advance.
///
/// The shared `continuity_fixer` ensures seamless TS continuity counter
/// progression when switching between inputs, and injects cached PAT/PMT
/// packets at the switch boundary so external decoders can re-acquire quickly.
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

fn spawn_input_forwarder(
    input_id: String,
    mut rx: broadcast::Receiver<RtpPacket>,
    out_tx: broadcast::Sender<RtpPacket>,
    active_input_rx: watch::Receiver<String>,
    cancel: CancellationToken,
    continuity_fixer: Arc<std::sync::Mutex<TsContinuityFixer>>,
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
                            // This input just became active — trigger switch
                            // to fix CC counters and inject cached PSI.
                            let injected = continuity_fixer.lock().unwrap().on_switch(&input_id);
                            tracing::info!(
                                "Input forwarder '{}': switch detected, injecting {} PSI packets",
                                input_id, injected.len()
                            );
                            for inj_pkt in injected {
                                let _ = out_tx.send(inj_pkt);
                            }
                            // Ask any ingress video re-encoder on this input
                            // to emit an IDR on its next frame. Passthrough
                            // inputs ignore the flag (no encoder to signal).
                            force_idr.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        was_active = is_active;

                        if is_active {
                            let mut fixer = continuity_fixer.lock().unwrap();
                            match fixer.process_packet(&input_id, &pkt) {
                                ProcessResult::Rewritten(fixed_data) => {
                                    drop(fixer);
                                    let _ = out_tx.send(RtpPacket {
                                        data: fixed_data,
                                        sequence_number: pkt.sequence_number,
                                        rtp_timestamp: pkt.rtp_timestamp,
                                        recv_time_us: pkt.recv_time_us,
                                        is_raw_ts: pkt.is_raw_ts,
                                    });
                                }
                                ProcessResult::Unchanged => {
                                    // Hot path: no rewrite needed, forward unchanged.
                                    drop(fixer);
                                    let _ = out_tx.send(pkt);
                                }
                            }
                            // A real packet just went out — push the next
                            // keepalive tick out one full interval.
                            keepalive.reset();
                        } else {
                            // Passive — observe PSI for cache pre-warming so
                            // PAT/PMT is ready for injection on switch.
                            continuity_fixer.lock().unwrap().observe_passive(&input_id, &pkt);
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
                        // Emit a 7-packet null payload so downstream UDP
                        // sockets see steady datagrams during a dead-
                        // input period (prevents socket timeouts on
                        // receivers that treat silence as EOF).
                        let _ = out_tx.send(null_ts_packet());
                    }
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
