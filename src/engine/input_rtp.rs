use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtpInputConfig;
use crate::fec::decoder::FecDecoder;
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::rtp_parse::{is_likely_rtp, parse_rtp_sequence_number, parse_rtp_timestamp};
use crate::util::socket::bind_udp_input;
use crate::util::time::now_us;

use super::packet::{MAX_RTP_PACKET_SIZE, RtpPacket};

/// Spawn a task that receives RTP packets from a UDP socket and
/// publishes them to a broadcast channel for fan-out to outputs.
/// If FEC decode is configured, lost packets may be recovered.
pub fn spawn_rtp_input(
    config: RtpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = rtp_input_loop(config, broadcast_tx, stats, cancel).await {
            tracing::error!("RTP input task exited with error: {e}");
        }
    })
}

/// Core receive loop for RTP input.
///
/// Binds a UDP socket to `config.bind_addr` (with optional multicast join via
/// `config.interface_addr`), then enters a select loop that:
///
/// - Reads datagrams from the socket.
/// - Filters non-RTP packets (e.g., stray RTCP).
/// - Tracks sequence numbers and counts gaps as packet loss.
/// - Optionally passes packets through a [`FecDecoder`] pipeline that can
///   recover lost media packets from FEC (SMPTE 2022-1) redundancy data.
/// - Publishes each resulting [`RtpPacket`] to the broadcast channel.
///
/// The loop exits when the cancellation token fires.
async fn rtp_input_loop(
    config: RtpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let socket = bind_udp_input(&config.bind_addr, config.interface_addr.as_deref()).await?;

    tracing::info!("RTP input started on {}", config.bind_addr);

    // Optional FEC decoder.
    //
    // The FEC decoder needs to report how many packets it has recovered,
    // but it lives behind a `&mut` reference inside the select loop where
    // we also need `&stats`. To avoid borrow conflicts, we use a shared
    // `Arc<AtomicU64>` counter: the decoder increments it internally on
    // each recovery, and we periodically copy its value into the stats
    // accumulator with a simple `load` + `store` (no lock required).
    let fec_recovered_counter = Arc::new(AtomicU64::new(0));
    let mut fec_decoder = config.fec_decode.as_ref().map(|fec_config| {
        tracing::info!(
            "FEC decode enabled: L={} D={}",
            fec_config.columns,
            fec_config.rows
        );
        FecDecoder::new(
            fec_config.columns,
            fec_config.rows,
            fec_recovered_counter.clone(),
        )
    });

    let mut buf = vec![0u8; MAX_RTP_PACKET_SIZE + 100]; // extra headroom
    let mut last_seq: Option<u16> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTP input on {} stopping (cancelled)", config.bind_addr);
                break;
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, _src)) => {
                        let data = &buf[..len];

                        if !is_likely_rtp(data) {
                            continue;
                        }

                        let seq = parse_rtp_sequence_number(data).unwrap_or(0);
                        let ts = parse_rtp_timestamp(data).unwrap_or(0);

                        // Detect sequence gaps for loss counting.
                        // RTP sequence numbers are 16-bit and wrap around, so
                        // we use wrapping arithmetic. Gaps larger than 1000 are
                        // likely caused by a source restart or massive reorder
                        // rather than actual loss, so we ignore them to avoid
                        // wildly inflating the loss counter.
                        if let Some(prev) = last_seq {
                            let expected = prev.wrapping_add(1);
                            if seq != expected {
                                let gap = seq.wrapping_sub(expected) as u64;
                                if gap > 0 && gap < 1000 {
                                    stats.input_loss.fetch_add(gap, Ordering::Relaxed);
                                }
                            }
                        }
                        last_seq = Some(seq);

                        // Update stats
                        stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        stats.input_bytes.fetch_add(len as u64, Ordering::Relaxed);

                        let bytes_data = Bytes::copy_from_slice(data);

                        if let Some(ref mut decoder) = fec_decoder {
                            // FEC decode pipeline:
                            //
                            // `process_media` accepts every incoming packet (both
                            // media and FEC). Internally it distinguishes them by
                            // payload type:
                            //   - Media packets are stored in a column/row matrix
                            //     and returned immediately for forwarding.
                            //   - FEC packets are consumed by the XOR recovery
                            //     engine. If a gap can be filled, the recovered
                            //     media packet(s) are also returned in the vec.
                            //
                            // The returned vec therefore contains the original
                            // media packet plus any packets recovered by FEC.
                            let packets = decoder.process_media(seq, ts, &bytes_data);
                            for pkt in packets {
                                let _ = broadcast_tx.send(pkt);
                            }
                            // Sync the FEC recovered counter back to the stats
                            // accumulator (atomic load + store, no lock needed).
                            let recovered = fec_recovered_counter.load(Ordering::Relaxed);
                            stats.fec_recovered.store(recovered, Ordering::Relaxed);
                        } else {
                            // No FEC — forward the raw media packet directly.
                            let packet = RtpPacket {
                                data: bytes_data,
                                sequence_number: seq,
                                rtp_timestamp: ts,
                                recv_time_us: now_us(),
                            };
                            let _ = broadcast_tx.send(packet);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("RTP input recv error: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}
