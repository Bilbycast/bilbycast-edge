//! MXL audio input dispatch shim (mirrors `input_st2110_30.rs`).

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::MxlAudioInputConfig;
use crate::engine::packet::RtpPacket;
use crate::manager::events::EventSender;
use crate::stats::collector::FlowStatsAccumulator;

use super::mxl::domain::MxlDomainManager;

#[allow(clippy::too_many_arguments)]
pub fn spawn_mxl_audio_input(
    config: MxlAudioInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) -> JoinHandle<()> {
    super::mxl_io::spawn_mxl_audio_input(
        config,
        input_id,
        broadcast_tx,
        flow_stats,
        cancel,
        event_sender,
        flow_id,
        domain_mgr,
    )
}
