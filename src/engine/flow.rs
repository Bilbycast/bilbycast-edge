// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::*;
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};

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
use super::media_analysis::spawn_media_analyzer;
use super::tr101290::spawn_tr101290_analyzer;
use crate::stats::collector::{MediaAnalysisAccumulator, Tr101290Accumulator};

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
    /// Stored to keep the JoinHandle alive; shutdown uses CancellationToken.
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
    /// Stored to keep the JoinHandle alive; shutdown uses CancellationToken.
    #[allow(dead_code)]
    pub analyzer_handle: JoinHandle<()>,
    /// Media analysis task handle (if enabled in config).
    /// Stored to keep the JoinHandle alive; shutdown uses CancellationToken.
    #[allow(dead_code)]
    pub media_analysis_handle: Option<JoinHandle<()>>,
    /// WHIP input session channel sender (only set for WebRTC/WHIP input flows).
    /// Must be registered with the WebrtcSessionRegistry after flow creation.
    #[cfg(feature = "webrtc")]
    pub whip_session_tx: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)>,
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
    /// Stored to keep the JoinHandle alive; shutdown uses CancellationToken.
    #[allow(dead_code)]
    pub handle: JoinHandle<()>,
    /// Child cancellation token. Dropping or cancelling this stops only
    /// this output, leaving the rest of the flow running.
    pub cancel_token: CancellationToken,
    /// Per-output stats accumulator. The Arc is shared with the output task;
    /// dropping this field would decrement the refcount while the task still writes to it.
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
    pub async fn start(config: FlowConfig, global_stats: &crate::stats::collector::StatsCollector) -> Result<Self> {
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
            InputConfig::Rtp(_) => "rtp",
            InputConfig::Udp(_) => "udp",
            InputConfig::Srt(_) => "srt",
            InputConfig::Rtmp(_) => "rtmp",
            InputConfig::Rtsp(_) => "rtsp",
            InputConfig::Webrtc(_) => "webrtc",
            InputConfig::Whep(_) => "whep",
        };

        // Register flow stats
        let flow_stats = global_stats.register_flow(
            config.id.clone(),
            config.name.clone(),
            input_type.to_string(),
        );

        // Populate input config metadata for topology display
        use crate::stats::collector::{InputConfigMeta, OutputConfigMeta};
        let input_meta = match &config.input {
            InputConfig::Rtp(c) => InputConfigMeta {
                mode: None, local_addr: None, remote_addr: None,
                listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            },
            InputConfig::Udp(c) => InputConfigMeta {
                mode: None, local_addr: None, remote_addr: None,
                listen_addr: None, bind_addr: Some(c.bind_addr.clone()),
            },
            InputConfig::Srt(c) => InputConfigMeta {
                mode: Some(format!("{:?}", c.mode).to_lowercase()),
                local_addr: Some(c.local_addr.clone()),
                remote_addr: c.remote_addr.clone(),
                listen_addr: None, bind_addr: None,
            },
            InputConfig::Rtmp(c) => InputConfigMeta {
                mode: None, local_addr: None, remote_addr: None,
                listen_addr: Some(c.listen_addr.clone()), bind_addr: None,
            },
            InputConfig::Rtsp(c) => InputConfigMeta {
                mode: Some(format!("{:?}", c.transport).to_lowercase()),
                local_addr: None, remote_addr: Some(c.rtsp_url.clone()),
                listen_addr: None, bind_addr: None,
            },
            InputConfig::Webrtc(_) => InputConfigMeta {
                mode: Some("whip_server".to_string()), local_addr: None, remote_addr: None,
                listen_addr: None, bind_addr: None,
            },
            InputConfig::Whep(c) => InputConfigMeta {
                mode: Some("whep_client".to_string()), local_addr: None,
                remote_addr: Some(c.whep_url.clone()),
                listen_addr: None, bind_addr: None,
            },
        };
        let _ = flow_stats.input_config_meta.set(input_meta);

        // Populate output config metadata
        for oc in &config.outputs {
            let meta = match oc {
                OutputConfig::Srt(c) => OutputConfigMeta {
                    mode: Some(format!("{:?}", c.mode).to_lowercase()),
                    remote_addr: c.remote_addr.clone(),
                    local_addr: Some(c.local_addr.clone()),
                    dest_addr: None, dest_url: None, ingest_url: None, whip_url: None,
                },
                OutputConfig::Rtp(c) => OutputConfigMeta {
                    mode: None, remote_addr: None, local_addr: None,
                    dest_addr: Some(c.dest_addr.clone()),
                    dest_url: None, ingest_url: None, whip_url: None,
                },
                OutputConfig::Udp(c) => OutputConfigMeta {
                    mode: None, remote_addr: None, local_addr: None,
                    dest_addr: Some(c.dest_addr.clone()),
                    dest_url: None, ingest_url: None, whip_url: None,
                },
                OutputConfig::Rtmp(c) => OutputConfigMeta {
                    mode: None, remote_addr: None, local_addr: None, dest_addr: None,
                    dest_url: Some(c.dest_url.clone()),
                    ingest_url: None, whip_url: None,
                },
                OutputConfig::Hls(c) => OutputConfigMeta {
                    mode: None, remote_addr: None, local_addr: None, dest_addr: None, dest_url: None,
                    ingest_url: Some(c.ingest_url.clone()), whip_url: None,
                },
                OutputConfig::Webrtc(c) => OutputConfigMeta {
                    mode: None, remote_addr: None, local_addr: None, dest_addr: None, dest_url: None,
                    ingest_url: None, whip_url: c.whip_url.clone(),
                },
            };
            flow_stats.output_config_meta.insert(oc.id().to_string(), meta);
        }

        // Start input task
        #[cfg(feature = "webrtc")]
        let mut whip_session_info: Option<(tokio::sync::mpsc::Sender<crate::api::webrtc::registry::NewSessionMsg>, Option<String>)> = None;
        let input_handle = match &config.input {
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
                )
            }
            InputConfig::Rtmp(rtmp_config) => {
                spawn_rtmp_input(
                    rtmp_config.clone(),
                    broadcast_tx.clone(),
                    flow_stats.clone(),
                    cancel_token.child_token(),
                )
            }
            InputConfig::Rtsp(rtsp_config) => {
                super::input_rtsp::spawn_rtsp_input(
                    rtsp_config.clone(),
                    broadcast_tx.clone(),
                    flow_stats.clone(),
                    cancel_token.child_token(),
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
        };

        // Start TR-101290 analyzer (independent broadcast subscriber)
        let tr101290_acc = Arc::new(Tr101290Accumulator::new());
        flow_stats.tr101290.set(tr101290_acc.clone()).ok();
        let analyzer_handle = spawn_tr101290_analyzer(
            &broadcast_tx,
            tr101290_acc,
            cancel_token.child_token(),
        );

        // Start media analysis (if enabled)
        let media_analysis_handle = if config.media_analysis {
            let (protocol, payload_format, fec_enabled, fec_type, redundancy_enabled, redundancy_type) =
                match &config.input {
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
            ))
        } else {
            None
        };

        // Start output tasks
        let mut output_handles = HashMap::new();
        for output_config in &config.outputs {
            let output_rt = Self::start_output(
                output_config,
                &broadcast_tx,
                &flow_stats,
                &cancel_token,
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
            #[cfg(feature = "webrtc")]
            whip_session_tx: whip_session_info,
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

        let output_rt = Self::start_output(
            &output_config,
            &self.broadcast_tx,
            flow_stats,
            &self.cancel_token,
        ).await?;

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
        let mut handles = self.output_handles.write().await;
        if let Some(output_rt) = handles.remove(output_id) {
            output_rt.cancel_token.cancel();
            // Clean up stats for this output
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
    }
}
