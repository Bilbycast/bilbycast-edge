//! MXL ancillary output dispatch shim (mirrors `output_st2110_40.rs`).

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::MxlAncOutputConfig;
use crate::engine::packet::RtpPacket;
use crate::manager::events::EventSender;
use crate::stats::collector::OutputStatsAccumulator;

use super::mxl::domain::MxlDomainManager;

#[allow(clippy::too_many_arguments)]
pub fn spawn_mxl_anc_output(
    config: MxlAncOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) -> JoinHandle<()> {
    super::mxl_io::spawn_mxl_anc_output(
        config,
        broadcast_tx,
        output_stats,
        cancel,
        event_sender,
        flow_id,
        domain_mgr,
    )
}
