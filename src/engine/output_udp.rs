use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::BytesMut;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::UdpOutputConfig;
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::socket::create_udp_output;

use super::packet::RtpPacket;

/// TS sync byte.
const TS_SYNC_BYTE: u8 = 0x47;

/// TS packet size.
const TS_PACKET_SIZE: usize = 188;

/// Number of TS packets per UDP datagram.
/// 7 × 188 = 1316 bytes — the standard for MPEG-TS over UDP.
const TS_PACKETS_PER_DATAGRAM: usize = 7;

/// Minimum consecutive sync bytes needed to confirm TS alignment.
const TS_SYNC_CONFIRM_COUNT: usize = 3;

/// Minimum RTP header size (version 2, no CSRC, no extensions).
const RTP_HEADER_MIN_SIZE: usize = 12;

/// Spawn a task that subscribes to the broadcast channel and sends
/// raw MPEG-TS datagrams (no RTP headers) over a UDP socket.
///
/// If the input stream is RTP-wrapped (`is_raw_ts == false`), the 12-byte
/// RTP header is stripped before buffering. The TS data is re-aligned into
/// standard 7×188-byte datagrams before sending.
pub fn spawn_udp_output(
    config: UdpOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    tokio::spawn(async move {
        if let Err(e) = udp_output_loop(&config, &mut rx, output_stats, cancel).await {
            tracing::error!("UDP output '{}' exited with error: {e}", config.id);
        }
    })
}

async fn udp_output_loop(
    config: &UdpOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let (socket, dest) =
        create_udp_output(&config.dest_addr, config.bind_addr.as_deref(), config.interface_addr.as_deref(), config.dscp).await?;

    tracing::info!("UDP output '{}' started -> {}", config.id, dest);

    // Buffer for re-aligning TS data into 188-byte aligned datagrams.
    let mut ts_buf = BytesMut::new();
    let ts_datagram_size = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
    let mut ts_sync_found = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("UDP output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Extract raw TS payload: strip RTP header if present
                        let ts_data = if packet.is_raw_ts {
                            &packet.data[..]
                        } else if packet.data.len() > RTP_HEADER_MIN_SIZE {
                            &packet.data[RTP_HEADER_MIN_SIZE..]
                        } else {
                            continue;
                        };

                        ts_buf.extend_from_slice(ts_data);

                        // Find TS sync boundary on first data
                        if !ts_sync_found {
                            let min_bytes = TS_SYNC_CONFIRM_COUNT * TS_PACKET_SIZE;
                            if ts_buf.len() >= min_bytes {
                                let mut found_offset = None;
                                for offset in 0..TS_PACKET_SIZE {
                                    let all_sync = (0..TS_SYNC_CONFIRM_COUNT).all(|i| {
                                        let pos = offset + i * TS_PACKET_SIZE;
                                        pos < ts_buf.len() && ts_buf[pos] == TS_SYNC_BYTE
                                    });
                                    if all_sync {
                                        found_offset = Some(offset);
                                        break;
                                    }
                                }

                                if let Some(offset) = found_offset {
                                    if offset > 0 {
                                        tracing::info!(
                                            "UDP output '{}': TS sync found at offset {}, discarding {} leading bytes",
                                            config.id, offset, offset
                                        );
                                        let _ = ts_buf.split_to(offset);
                                    } else {
                                        tracing::info!(
                                            "UDP output '{}': TS sync found (already aligned)",
                                            config.id
                                        );
                                    }
                                    ts_sync_found = true;
                                } else {
                                    tracing::warn!(
                                        "UDP output '{}': no TS sync found in {} bytes, discarding",
                                        config.id, ts_buf.len()
                                    );
                                    ts_buf.clear();
                                }
                            }
                        }

                        // Send complete TS-aligned datagrams
                        if ts_sync_found {
                            while ts_buf.len() >= ts_datagram_size {
                                let datagram = ts_buf.split_to(ts_datagram_size);
                                match socket.send_to(&datagram, dest).await {
                                    Ok(sent) => {
                                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                        stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        tracing::warn!("UDP output '{}' send error: {e}", config.id);
                                    }
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!("UDP output '{}' lagged, dropped {n} packets", config.id);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("UDP output '{}' broadcast channel closed", config.id);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
