use std::sync::Arc;

use srt_protocol::config::SocketStatus;
use srt_transport::SrtSocket;

use crate::stats::models::SrtLegStats;

/// Poll SRT statistics from a connected socket and convert to our internal
/// [`SrtLegStats`] model.
///
/// Retrieves performance statistics from the SRT socket and extracts:
///
/// - **RTT** (`ms_rtt`) -- smoothed round-trip time in milliseconds.
/// - **Send rate** (`mbps_send_rate`) -- estimated send throughput in Mbps.
/// - **Receive rate** (`mbps_recv_rate`) -- estimated receive throughput in Mbps.
/// - **Packet loss** -- combined send + receive loss totals.
/// - **Retransmits** (`pkt_retrans_total`) -- total ARQ retransmissions.
/// - **Uptime** (`ms_timestamp`) -- milliseconds since the socket connected.
///
/// Returns `None` if the socket is in a broken or closed state.
pub async fn poll_srt_stats(socket: &Arc<SrtSocket>) -> Option<SrtLegStats> {
    let status = socket.status();
    match status {
        SocketStatus::Broken | SocketStatus::Closed => return None,
        _ => {}
    }

    let stats = socket.stats().await;

    Some(SrtLegStats {
        state: format!("{:?}", status).to_lowercase(),
        rtt_ms: stats.ms_rtt,
        send_rate_mbps: stats.mbps_send_rate,
        recv_rate_mbps: stats.mbps_recv_rate,
        pkt_loss_total: stats.pkt_rcv_loss_total as i64 + stats.pkt_snd_loss_total as i64,
        pkt_retransmit_total: stats.pkt_retrans_total,
        uptime_ms: stats.ms_timestamp,
    })
}
