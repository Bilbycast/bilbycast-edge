// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::*;
use crate::manager::events::EventSender;
use crate::stats::collector::{FlowStatsAccumulator, OutputConfigMeta, OutputStatsAccumulator};

use super::input_rtmp::spawn_rtmp_input;
use super::input_rtp::spawn_rtp_input;
use super::input_srt::spawn_srt_input;
use super::input_udp::spawn_udp_input;
use super::output_hls::spawn_hls_output;
use super::output_rtmp::spawn_rtmp_output;
use super::output_rtp::spawn_rtp_output;
use super::output_srt::spawn_srt_output;
use super::output_udp::spawn_udp_output;
use super::output_webrtc::spawn_webrtc_output;
use super::packet::{BROADCAST_CHANNEL_CAPACITY, RtpPacket};
use super::bandwidth_monitor::spawn_bandwidth_monitor;
use super::media_analysis::spawn_media_analyzer;
use super::thumbnail::spawn_thumbnail_generator;
use super::tr101290::spawn_tr101290_analyzer;
use crate::stats::collector::{MediaAnalysisAccumulator, ThumbnailAccumulator, Tr101290Accumulator};

/// Runtime state for a single media flow (one input, N outputs).
///
/// A `FlowRuntime` owns all the moving parts of a running flow:
///
/// - **Input task** (`input_handle`): a Tokio task that reads packets from
///   an RTP or SRT source and publishes them into the broadcast channel.
/// - **Broadcast channel** (`broadcast_tx`): a Tokio `broadcast::Sender`
///   that fans out every incoming packet to all subscribed outputs. The
///   channel is bounded to [`BROADCAST_CHANNEL_CAPACITY`] slots; slow
///   receivers that fall behind will receive a `Lagged` error and lose
///   packets rather than blocking the input.
/// - **Output tasks** (`output_handles`): a map of [`OutputRuntime`]
///   instances, one per configured output destination.
/// - **Cancellation token** (`cancel_token`): the parent token for the
///   entire flow. Cancelling it signals both the input and every output
///   task to shut down.
/// - **Stats accumulator** (`stats`): per-flow metrics (packets, bytes,
///   loss, FEC) that are periodically snapshotted by the stats subsystem.
///
/// Outputs can be hot-added or removed at runtime via [`add_output`](Self::add_output)
/// and [`remove_output`](Self::remove_output) without disturbing the input or
/// other outputs.
pub struct FlowRuntime {
    pub config: FlowConfig,
    /// The broadcast sender that the input writes into. Every output
    /// subscribes to this sender to receive a copy of each packet.
    pub broadcast_tx: broadcast::Sender<RtpPacket>,
    /// Tokio task handle for the input (RTP or SRT receiver).
    /// Held for ownership — dropping a JoinHandle detaches the task.
    /// Shutdown is driven by CancellationToken, not by aborting the handle.
    #[allow(dead_code)]
    pub input_handle: JoinHandle<()>,
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
    pub async fn start(config: FlowConfig, global_stats: &crate::stats::collector::StatsCollector, ffmpeg_available: bool, event_sender: EventSender) -> Result<Self> {
        let cancel_token = CancellationToken::new();
        // The broadcast channel is bounded to BROADCAST_CHANNEL_CAPACITY slots.
        // When a slow output (receiver) cannot keep up, it will *not* block the
        // input or other outputs. Instead, the lagging receiver's next `recv()`
        // returns `RecvError::Lagged(n)`, telling it how many messages it missed.
        // Each output handles this by incrementing its `packets_dropped` stat.
        // The underscore `_` receiver is created and immediately dropped; it is
        // only needed to satisfy the channel constructor signature.
        let (broadcast_tx, _) = broadcast::channel::<RtpPacket>(BROADCAST_CHANNEL_CAPACITY);

        let input_type = match &config.input {
            Some(InputConfig::Rtp(_)) => "rtp",
            Some(InputConfig::Udp(_)) => "udp",
            Some(InputConfig::Srt(_)) => "srt",
            Some(InputConfig::Rtmp(_)) => "rtmp",
            Some(InputConfig::Rtsp(_)) => "rtsp",
            Some(InputConfig::Webrtc(_)) => "webrtc",
            Some(InputConfig::Whep(_)) => "whep",
            Some(InputConfig::St2110_30(_)) => "st2110_30",
            Some(InputConfig::St2110_31(_)) => "st2110_31",
            Some(InputConfig::St2110_40(_)) => "st2110_40",
            Some(InputConfig::RtpAudio(_)) => "rtp_audio",
            None => "none",
        };

        // Register flow stats
        let flow_stats = global_stats.register_flow(
            config.id.clone(),
            config.name.clone(),
            input_type.to_string(),
        );

        // Populate input config metadata for topology display
        use crate::stats::collector::InputConfigMeta;
        if let Some(ref input) = config.input {
            let input_meta = match input {
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
                    local_addr: Some(c.local_addr.clone()),
                    remote_addr: c.remote_addr.clone(),
                    listen_addr: None, bind_addr: None,
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
                InputConfig::RtpAudio(c) => InputConfigMeta {
                    mode: None, local_addr: None, remote_addr: None,
                    listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
                    rtsp_url: None, whep_url: None,
                },
            };
            let _ = flow_stats.input_config_meta.set(input_meta);
        }

        // Populate output config metadata
        for oc in &config.outputs {
            flow_stats.output_config_meta.insert(
                oc.id().to_string(),
                build_output_config_meta(oc),
            );
        }

        // Start input task (if present — flows with no input are standalone
        // output definitions that receive data when the Flow Router connects
        // an input at runtime).
        #[cfg(feature = "webrtc")]
        let mut whip_session_info: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)> = None;
        let input_handle = if let Some(ref input) = config.input {
            match input {
                InputConfig::Rtp(rtp_config) => {
                    spawn_rtp_input(
                        rtp_config.clone(),
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                    )
                }
                InputConfig::Udp(udp_config) => {
                    spawn_udp_input(
                        udp_config.clone(),
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                    )
                }
                InputConfig::Srt(srt_config) => {
                    spawn_srt_input(
                        srt_config.clone(),
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                        event_sender.clone(),
                        config.id.clone(),
                    )
                }
                InputConfig::Rtmp(rtmp_config) => {
                    spawn_rtmp_input(
                        rtmp_config.clone(),
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                        event_sender.clone(),
                        config.id.clone(),
                    )
                }
                InputConfig::Rtsp(rtsp_config) => {
                    super::input_rtsp::spawn_rtsp_input(
                        rtsp_config.clone(),
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                        event_sender.clone(),
                        config.id.clone(),
                    )
                }
                #[cfg(feature = "webrtc")]
                InputConfig::Webrtc(webrtc_config) => {
                    // Create channel for WHIP API handler → input task communication
                    let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
                    // Store session_tx and bearer_token for registration with WebrtcSessionRegistry
                    whip_session_info = Some((session_tx, webrtc_config.bearer_token.clone()));
                    super::input_webrtc::spawn_whip_input(
                        webrtc_config.clone(),
                        config.id.clone(),
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                        session_rx,
                        event_sender.clone(),
                    )
                }
                #[cfg(not(feature = "webrtc"))]
                InputConfig::Webrtc(_) => {
                    let cancel = cancel_token.child_token();
                    tokio::spawn(async move {
                        tracing::warn!("WebRTC/WHIP input requires the `webrtc` cargo feature");
                        cancel.cancelled().await;
                    })
                }
                #[cfg(feature = "webrtc")]
                InputConfig::Whep(whep_config) => {
                    super::input_webrtc::spawn_whep_input(
                        whep_config.clone(),
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                        event_sender.clone(),
                        config.id.clone(),
                    )
                }
                #[cfg(not(feature = "webrtc"))]
                InputConfig::Whep(_) => {
                    let cancel = cancel_token.child_token();
                    tokio::spawn(async move {
                        tracing::warn!("WHEP input requires the `webrtc` cargo feature");
                        cancel.cancelled().await;
                    })
                }
                // The PTP clock_domain may be set on the flow itself or on the
                // per-essence input config. The cloned local inherits the
                // flow-level value when the essence does not override it; the
                // stored InputConfig is untouched so update_flow's diff stays
                // accurate.
                InputConfig::St2110_30(c) => {
                    let mut c = c.clone();
                    c.clock_domain = c.clock_domain.or(config.clock_domain);
                    super::input_st2110_30::spawn_st2110_30_input(
                        c,
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                        event_sender.clone(),
                        config.id.clone(),
                    )
                }
                InputConfig::St2110_31(c) => {
                    let mut c = c.clone();
                    c.clock_domain = c.clock_domain.or(config.clock_domain);
                    super::input_st2110_31::spawn_st2110_31_input(
                        c,
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                        event_sender.clone(),
                        config.id.clone(),
                    )
                }
                InputConfig::St2110_40(c) => {
                    let mut c = c.clone();
                    c.clock_domain = c.clock_domain.or(config.clock_domain);
                    super::input_st2110_40::spawn_st2110_40_input(
                        c,
                        broadcast_tx.clone(),
                        flow_stats.clone(),
                        cancel_token.child_token(),
                        event_sender.clone(),
                        config.id.clone(),
                    )
                }
                InputConfig::RtpAudio(c) => super::input_rtp_audio::spawn_rtp_audio_input(
                    c.clone(),
                    broadcast_tx.clone(),
                    flow_stats.clone(),
                    cancel_token.child_token(),
                    Some(event_sender.clone()),
                    Some(config.id.clone()),
                ),
            }
        } else {
            // No input — spawn a no-op task that waits for cancellation.
            // The broadcast channel exists but is silent until the Flow Router
            // connects an input.
            let cancel = cancel_token.child_token();
            tokio::spawn(async move { cancel.cancelled().await })
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
        let analyzer_handle = if config.input.as_ref().map_or(false, |i| i.is_ts_carrier()) {
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

        // Check if any output uses TargetFrames delay mode, which requires
        // the detected video frame rate from media analysis. If so, create a
        // watch channel to broadcast the frame rate to those output tasks.
        let needs_frame_rate = config.outputs.iter().any(|o| {
            let delay = match o {
                OutputConfig::Rtp(c) => c.delay.as_ref(),
                OutputConfig::Udp(c) => c.delay.as_ref(),
                OutputConfig::Srt(c) => c.delay.as_ref(),
                _ => None,
            };
            matches!(delay, Some(OutputDelay::TargetFrames { .. }))
        });
        let (frame_rate_tx, frame_rate_rx) = if needs_frame_rate {
            let (tx, rx) = tokio::sync::watch::channel::<Option<f64>>(None);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Start media analysis (if enabled and input is present)
        let media_analysis_handle = if config.media_analysis && config.input.is_some() {
            let (protocol, payload_format, fec_enabled, fec_type, redundancy_enabled, redundancy_type) =
                match config.input.as_ref().unwrap() {
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
            ))
        } else {
            None
        };

        // Start thumbnail generator (if enabled, ffmpeg available, and input is present)
        let thumbnail_handle = if config.thumbnail && ffmpeg_available && config.input.is_some() {
            let thumb_acc = Arc::new(ThumbnailAccumulator::new());
            flow_stats.thumbnail.set(thumb_acc.clone()).ok();
            Some(spawn_thumbnail_generator(
                &broadcast_tx,
                thumb_acc,
                cancel_token.child_token(),
                config.thumbnail_program_number,
            ))
        } else {
            None
        };

        // Start bandwidth monitor (if configured)
        let bandwidth_monitor_handle = if let Some(ref bw_limit) = config.bandwidth_limit {
            flow_stats.bandwidth_limit_mbps.set(bw_limit.max_bitrate_mbps).ok();
            Some(spawn_bandwidth_monitor(
                config.id.clone(),
                bw_limit.clone(),
                flow_stats.clone(),
                event_sender.clone(),
                cancel_token.child_token(),
            ))
        } else {
            None
        };

        // Start output tasks
        #[cfg(feature = "webrtc")]
        let mut whep_session_info: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)> = None;
        let mut output_handles = HashMap::new();
        // Resolve the upstream input audio format once so audio output spawn
        // modules can construct an optional TranscodeStage. Returns None for
        // non-audio inputs (video, MPEG-TS, etc.) — passthrough is selected.
        let input_audio_format = config.input.as_ref()
            .and_then(crate::engine::audio_transcode::InputFormat::from_input_config);
        // Resolve once whether the upstream input is a TS-bearing source that
        // could carry compressed audio (AAC). When true, audio output spawn
        // modules will run an in-line TsDemuxer + AacDecoder ahead of the
        // transcode chain so AAC contribution audio can land into PCM
        // outputs (ST 2110-30/-31, rtp_audio, SMPTE 302M).
        let compressed_audio_input = config.input.as_ref()
            .map_or(false, crate::engine::audio_decode::input_can_carry_ts_audio);
        for output_config in &config.outputs {
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
                &config.id,
                input_audio_format,
                compressed_audio_input,
                #[cfg(feature = "webrtc")]
                whep_rx,
                frame_rate_rx.clone(),
            ).await?;
            output_handles.insert(output_config.id().to_string(), output_rt);
        }

        tracing::info!(
            "Flow '{}' ({}) started: {} input, {} output(s)",
            config.id,
            config.name,
            input_type,
            output_handles.len()
        );

        Ok(Self {
            config,
            broadcast_tx,
            input_handle,
            output_handles: RwLock::new(output_handles),
            cancel_token,
            stats: flow_stats,
            analyzer_handle,
            media_analysis_handle,
            thumbnail_handle,
            bandwidth_monitor_handle,
            #[cfg(feature = "webrtc")]
            whip_session_tx: whip_session_info,
            #[cfg(feature = "webrtc")]
            whep_session_tx: whep_session_info,
            event_sender,
        })
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

        let input_audio_format = self.config.input.as_ref()
            .and_then(crate::engine::audio_transcode::InputFormat::from_input_config);
        let compressed_audio_input = self.config.input.as_ref()
            .map_or(false, crate::engine::audio_decode::input_can_carry_ts_audio);
        let output_rt = Self::start_output(
            &output_config,
            &self.broadcast_tx,
            flow_stats,
            &self.cancel_token,
            &self.event_sender,
            &self.config.id,
            input_audio_format,
            compressed_audio_input,
            #[cfg(feature = "webrtc")]
            None,
            // Hot-added outputs with TargetFrames delay won't get a frame rate
            // channel — they'll fall back to fallback_ms. Full TargetFrames
            // support requires a flow restart to wire up the watch channel.
            None,
        ).await?;

        // Update output config metadata so stats snapshots reflect the new address/port
        flow_stats.output_config_meta.insert(
            output_id.clone(),
            build_output_config_meta(&output_config),
        );

        let mut handles = self.output_handles.write().await;
        handles.insert(output_id.clone(), output_rt);

        tracing::info!("Hot-added output '{}' to flow '{}'", output_id, self.config.id);
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
            // release UDP port) before returning. This ensures the port is free for
            // reuse when a new output is created on the same address.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                output_rt.handle,
            ).await;
            self.stats.unregister_output(output_id);
            tracing::info!("Removed output '{}' from flow '{}'", output_id, self.config.id);
            Ok(())
        } else {
            bail!("Output '{}' not found in flow '{}'", output_id, self.config.id);
        }
    }

    /// Stop the entire flow by cancelling the parent token.
    ///
    /// This signals both the input task and every output task to shut down.
    /// Because output cancel tokens are children of the flow token, a single
    /// cancel propagates to all tasks. The actual task cleanup (socket close,
    /// thread join) happens asynchronously inside each task's select loop.
    pub async fn stop(&self) {
        tracing::info!("Stopping flow '{}' ({})", self.config.id, self.config.name);
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
    }
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
            local_addr: Some(c.local_addr.clone()),
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
        OutputConfig::RtpAudio(c) => OutputConfigMeta {
            mode: None, remote_addr: None, local_addr: None,
            dest_addr: Some(c.dest_addr.clone()),
            dest_url: None, ingest_url: None, whip_url: None,
            program_number: None,
        },
    }
}
