use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::UdpInputConfig;
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::socket::bind_udp_input;
use crate::util::time::now_us;

use super::packet::{MAX_RTP_PACKET_SIZE, RtpPacket};

/// TS packet size used for synthetic timestamp calculation.
const TS_PACKET_SIZE: usize = 188;

/// Spawn a task that receives raw UDP datagrams and publishes them
/// to a broadcast channel for fan-out to outputs.
///
/// Unlike the RTP input, this accepts ALL datagrams without requiring
/// an RTP v2 header. Packets are forwarded with `is_raw_ts: true` and
/// synthetic sequence numbers. Suitable for receiving raw MPEG-TS over
/// UDP from sources like OBS, ffmpeg, or srt-live-transmit.
pub fn spawn_udp_input(
    config: UdpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = udp_input_loop(config, broadcast_tx, stats, cancel).await {
            tracing::error!("UDP input task exited with error: {e}");
        }
    })
}

async fn udp_input_loop(
    config: UdpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let socket = bind_udp_input(&config.bind_addr, config.interface_addr.as_deref()).await?;

    tracing::info!("UDP input started on {}", config.bind_addr);

    let mut buf = vec![0u8; MAX_RTP_PACKET_SIZE + 100];
    let mut seq_counter: u16 = 0;
    let mut ts_counter: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("UDP input on {} stopping (cancelled)", config.bind_addr);
                break;
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, _src)) => {
                        let data = &buf[..len];

                        // Generate synthetic sequence number
                        let seq = seq_counter;
                        seq_counter = seq_counter.wrapping_add(1);

                        // Approximate timestamp: assume 90kHz clock
                        let ts_pkts = (len / TS_PACKET_SIZE) as u32;
                        ts_counter = ts_counter.wrapping_add(ts_pkts.max(1) * 188 * 8);

                        stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        stats.input_bytes.fetch_add(len as u64, Ordering::Relaxed);

                        let packet = RtpPacket {
                            data: Bytes::copy_from_slice(data),
                            sequence_number: seq,
                            rtp_timestamp: ts_counter,
                            recv_time_us: now_us(),
                            is_raw_ts: true,
                        };

                        let _ = broadcast_tx.send(packet);
                    }
                    Err(e) => {
                        tracing::warn!("UDP input recv error: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}
