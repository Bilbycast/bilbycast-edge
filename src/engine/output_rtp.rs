use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtpOutputConfig;
use crate::fec::encoder::FecEncoder;
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::socket::create_udp_output;

use super::packet::RtpPacket;

/// Spawn a task that subscribes to the broadcast channel and sends
/// RTP packets out over a UDP socket. If FEC encode is configured,
/// FEC packets are also generated and sent.
pub fn spawn_rtp_output(
    config: RtpOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    tokio::spawn(async move {
        if let Err(e) = rtp_output_loop(&config, &mut rx, output_stats, cancel).await {
            tracing::error!("RTP output '{}' exited with error: {e}", config.id);
        }
    })
}

/// Core send loop for a single RTP output.
///
/// Subscribes to the flow's broadcast channel and, for every received
/// [`RtpPacket`], sends it as a UDP datagram to the configured destination
/// address. If FEC encoding is enabled, additional FEC repair packets are
/// generated and sent after each media packet.
///
/// **Broadcast lag handling**: if this output cannot keep up with the input
/// rate (e.g., due to transient network stalls), the broadcast receiver
/// will report `Lagged(n)`. The output counts those dropped packets in
/// `stats.packets_dropped` and continues — it never blocks the input or
/// other outputs.
///
/// The loop exits when the cancellation token fires or the broadcast
/// channel is closed (input task exited).
async fn rtp_output_loop(
    config: &RtpOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let (socket, dest) =
        create_udp_output(&config.dest_addr, config.bind_addr.as_deref(), config.interface_addr.as_deref(), config.dscp).await?;

    tracing::info!(
        "RTP output '{}' started -> {}",
        config.id,
        dest
    );

    // Optional FEC encoder (SMPTE 2022-1).
    //
    // Like the input-side FEC decoder, we use a shared `Arc<AtomicU64>`
    // counter so the encoder can increment it internally and we periodically
    // copy the value back to the stats accumulator without borrow conflicts.
    let fec_sent_counter = Arc::new(AtomicU64::new(0));
    let mut fec_encoder = config.fec_encode.as_ref().map(|fec_config| {
        tracing::info!(
            "FEC encode enabled for output '{}': L={} D={}",
            config.id,
            fec_config.columns,
            fec_config.rows
        );
        FecEncoder::new(
            fec_config.columns,
            fec_config.rows,
            fec_sent_counter.clone(),
        )
    });

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTP output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Send the media packet
                        match socket.send_to(&packet.data, dest).await {
                            Ok(sent) => {
                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                            }
                            Err(e) => {
                                tracing::warn!("RTP output '{}' send error: {e}", config.id);
                            }
                        }

                        // Generate and send FEC packets if enabled.
                        //
                        // The FEC encoder accumulates media payloads in an
                        // L x D matrix (columns x rows). FEC repair packets
                        // are emitted at two points:
                        //   - A **column FEC packet** is emitted every L
                        //     media packets (one column is full).
                        //   - A **row FEC packet** is emitted every D
                        //     media packets (one row is full).
                        //   - When the full L x D matrix is complete, the
                        //     final column and row packets are both emitted.
                        //
                        // The returned `fec_packets` vec is empty for most
                        // calls and contains 1-2 packets when a column or
                        // row boundary is reached.
                        if let Some(ref mut encoder) = fec_encoder {
                            let rtp_header_len = 12;
                            let payload = if packet.data.len() > rtp_header_len {
                                &packet.data[rtp_header_len..]
                            } else {
                                &packet.data[..]
                            };

                            let fec_packets = encoder.process(packet.sequence_number, payload);
                            for fec_pkt in fec_packets {
                                match socket.send_to(&fec_pkt, dest).await {
                                    Ok(sent) => {
                                        stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "RTP output '{}' FEC send error: {e}",
                                            config.id
                                        );
                                    }
                                }
                            }
                            // Sync FEC sent count back to stats accumulator
                            let sent = fec_sent_counter.load(Ordering::Relaxed);
                            stats.fec_packets_sent.store(sent, Ordering::Relaxed);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "RTP output '{}' lagged, dropped {n} packets",
                            config.id
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("RTP output '{}' broadcast channel closed", config.id);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
