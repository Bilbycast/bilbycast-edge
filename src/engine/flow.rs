use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::*;
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};

use super::input_rtp::spawn_rtp_input;
use super::input_srt::spawn_srt_input;
use super::output_hls::spawn_hls_output;
use super::output_rtmp::spawn_rtmp_output;
use super::output_rtp::spawn_rtp_output;
use super::output_srt::spawn_srt_output;
use super::output_webrtc::spawn_webrtc_output;
use super::packet::{BROADCAST_CHANNEL_CAPACITY, RtpPacket};
use super::tr101290::spawn_tr101290_analyzer;
use crate::stats::collector::Tr101290Accumulator;

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
    pub analyzer_handle: JoinHandle<()>,
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
    pub config: OutputConfig,
    /// Tokio task handle for this output's send loop.
    pub handle: JoinHandle<()>,
    /// Child cancellation token. Dropping or cancelling this stops only
    /// this output, leaving the rest of the flow running.
    pub cancel_token: CancellationToken,
    /// Per-output stats accumulator shared with the output task.
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
            InputConfig::Srt(_) => "srt",
        };

        // Register flow stats
        let flow_stats = global_stats.register_flow(
            config.id.clone(),
            config.name.clone(),
            input_type.to_string(),
        );

        // Start input task
        let input_handle = match &config.input {
            InputConfig::Rtp(rtp_config) => {
                spawn_rtp_input(
                    rtp_config.clone(),
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
        };

        // Start TR-101290 analyzer (independent broadcast subscriber)
        let tr101290_acc = Arc::new(Tr101290Accumulator::new());
        flow_stats.tr101290.set(tr101290_acc.clone()).ok();
        let analyzer_handle = spawn_tr101290_analyzer(
            &broadcast_tx,
            tr101290_acc,
            cancel_token.child_token(),
        );

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
                    config: output_config.clone(),
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
                    config: output_config.clone(),
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
                    config: output_config.clone(),
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
                    config: output_config.clone(),
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
                    config: output_config.clone(),
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
