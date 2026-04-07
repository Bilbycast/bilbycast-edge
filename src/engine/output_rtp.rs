// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::BytesMut;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtpOutputConfig;
use crate::fec::encoder::FecEncoder;
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::socket::create_udp_output;

use super::packet::RtpPacket;
use super::ts_program_filter::TsProgramFilter;

/// TS sync byte.
const TS_SYNC_BYTE: u8 = 0x47;

/// TS packet size.
const TS_PACKET_SIZE: usize = 188;

/// Number of TS packets per UDP datagram for raw TS output.
/// 7 × 188 = 1316 bytes — the standard for MPEG-TS over UDP.
const TS_PACKETS_PER_DATAGRAM: usize = 7;

/// Minimum consecutive sync bytes needed to confirm TS alignment.
const TS_SYNC_CONFIRM_COUNT: usize = 3;

/// Spawn a task that subscribes to the broadcast channel and sends
/// RTP packets out over a UDP socket. If FEC encode is configured,
/// FEC packets are also generated and sent. If redundancy is configured,
/// packets are duplicated to two independent UDP legs (SMPTE 2022-7).
pub fn spawn_rtp_output(
    config: RtpOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    tokio::spawn(async move {
        let result = if config.redundancy.is_some() {
            rtp_output_redundant_loop(&config, &mut rx, output_stats, cancel).await
        } else {
            rtp_output_loop(&config, &mut rx, output_stats, cancel).await
        };
        if let Err(e) = result {
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

    // Buffer for re-aligning raw TS data into 188-byte aligned datagrams.
    // SRT may deliver data in chunks that don't align to TS packet boundaries
    // (e.g., 1456-byte byte-stream chunks). This buffer accumulates incoming
    // data, finds the TS sync byte (0x47) boundary, and sends properly aligned
    // chunks (7 × 188 = 1316 bytes) — the standard for MPEG-TS over UDP.
    let mut ts_realign_buf = BytesMut::new();
    let ts_datagram_size = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
    let mut is_raw_ts_stream = false;
    let mut ts_sync_found = false;

    // Optional MPTS → SPTS program filter.
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "RTP output '{}': program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTP output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Apply program filter (if enabled). For RTP-wrapped
                        // packets the filter strips the RTP header, filters
                        // the TS payload, then re-wraps with the original
                        // RTP header. For raw TS the filtered bytes feed the
                        // re-alignment buffer below. If the filter eats the
                        // entire packet (no target-program TS in this datagram)
                        // we skip both the send AND the FEC update.
                        let packet = if let Some(ref mut filter) = program_filter {
                            match filter.filter_packet(&packet, &mut filter_scratch) {
                                Some(filtered) => RtpPacket {
                                    data: filtered,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                },
                                None => continue,
                            }
                        } else {
                            packet
                        };
                        if packet.is_raw_ts {
                            // Raw TS: buffer and re-align to 188-byte boundaries.
                            // SRT interop may deliver data in non-aligned chunks.
                            if !is_raw_ts_stream {
                                is_raw_ts_stream = true;
                                tracing::info!(
                                    "RTP output '{}': raw TS detected, enabling TS re-alignment ({} TS pkts/datagram)",
                                    config.id,
                                    TS_PACKETS_PER_DATAGRAM
                                );
                            }

                            ts_realign_buf.extend_from_slice(&packet.data);

                            // Find TS sync boundary on first data: scan for 0x47
                            // at 188-byte intervals to confirm alignment.
                            if !ts_sync_found {
                                let min_bytes = TS_SYNC_CONFIRM_COUNT * TS_PACKET_SIZE;
                                if ts_realign_buf.len() >= min_bytes {
                                    let mut found_offset = None;
                                    for offset in 0..TS_PACKET_SIZE {
                                        let all_sync = (0..TS_SYNC_CONFIRM_COUNT).all(|i| {
                                            let pos = offset + i * TS_PACKET_SIZE;
                                            pos < ts_realign_buf.len()
                                                && ts_realign_buf[pos] == TS_SYNC_BYTE
                                        });
                                        if all_sync {
                                            found_offset = Some(offset);
                                            break;
                                        }
                                    }

                                    if let Some(offset) = found_offset {
                                        if offset > 0 {
                                            tracing::info!(
                                                "RTP output '{}': TS sync found at offset {}, discarding {} leading bytes",
                                                config.id,
                                                offset,
                                                offset
                                            );
                                            let _ = ts_realign_buf.split_to(offset);
                                        } else {
                                            tracing::info!(
                                                "RTP output '{}': TS sync found at offset 0 (already aligned)",
                                                config.id
                                            );
                                        }
                                        ts_sync_found = true;
                                    } else {
                                        tracing::warn!(
                                            "RTP output '{}': no TS sync found in {} bytes, discarding",
                                            config.id,
                                            ts_realign_buf.len()
                                        );
                                        ts_realign_buf.clear();
                                    }
                                }
                            }

                            // Send complete TS-aligned datagrams
                            if ts_sync_found {
                                while ts_realign_buf.len() >= ts_datagram_size {
                                    let datagram = ts_realign_buf.split_to(ts_datagram_size);
                                    match socket.send_to(&datagram, dest).await {
                                        Ok(sent) => {
                                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                            stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                        }
                                        Err(e) => {
                                            tracing::warn!("RTP output '{}' send error: {e}", config.id);
                                        }
                                    }
                                }
                            }
                        } else {
                            // RTP-wrapped data: send as-is (already properly framed)
                            match socket.send_to(&packet.data, dest).await {
                                Ok(sent) => {
                                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                    stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    tracing::warn!("RTP output '{}' send error: {e}", config.id);
                                }
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

// ---------------------------------------------------------------------------
// SMPTE 2022-7 dual-leg RTP output
// ---------------------------------------------------------------------------

/// Dual-leg RTP output with SMPTE 2022-7 duplication.
///
/// Sends every packet to two independent UDP destinations. If FEC encoding
/// is enabled, FEC packets are also sent to both legs. Only leg 1 increments
/// `packets_sent`/`bytes_sent` to avoid double-counting.
async fn rtp_output_redundant_loop(
    config: &RtpOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Leg 1
    let (socket1, dest1) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
    )
    .await?;

    // Leg 2
    let leg2_dscp = redundancy.dscp.unwrap_or(config.dscp);
    let (socket2, dest2) = create_udp_output(
        &redundancy.dest_addr,
        redundancy.bind_addr.as_deref(),
        redundancy.interface_addr.as_deref(),
        leg2_dscp,
    )
    .await?;

    tracing::info!(
        "RTP output '{}' started with 2022-7 redundancy: leg1={} leg2={}",
        config.id,
        dest1,
        dest2
    );

    // FEC encoder (shared — same repair data sent to both legs)
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

    // Raw TS re-alignment buffer (shared for both legs)
    let mut ts_realign_buf = BytesMut::new();
    let ts_datagram_size = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
    let mut is_raw_ts_stream = false;
    let mut ts_sync_found = false;

    // Optional MPTS → SPTS program filter (shared by both legs).
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "RTP output '{}' (2022-7): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTP output '{}' (2022-7) stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Apply program filter (if enabled). Same semantics as
                        // the single-leg path: skip the entire packet if the
                        // filter eats it (no target-program TS in this datagram).
                        let packet = if let Some(ref mut filter) = program_filter {
                            match filter.filter_packet(&packet, &mut filter_scratch) {
                                Some(filtered) => RtpPacket {
                                    data: filtered,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                },
                                None => continue,
                            }
                        } else {
                            packet
                        };
                        if packet.is_raw_ts {
                            // Raw TS: buffer and re-align, send to both legs
                            if !is_raw_ts_stream {
                                is_raw_ts_stream = true;
                                tracing::info!(
                                    "RTP output '{}': raw TS detected, enabling TS re-alignment ({} TS pkts/datagram)",
                                    config.id,
                                    TS_PACKETS_PER_DATAGRAM
                                );
                            }

                            ts_realign_buf.extend_from_slice(&packet.data);

                            if !ts_sync_found {
                                let min_bytes = TS_SYNC_CONFIRM_COUNT * TS_PACKET_SIZE;
                                if ts_realign_buf.len() >= min_bytes {
                                    let mut found_offset = None;
                                    for offset in 0..TS_PACKET_SIZE {
                                        let all_sync = (0..TS_SYNC_CONFIRM_COUNT).all(|i| {
                                            let pos = offset + i * TS_PACKET_SIZE;
                                            pos < ts_realign_buf.len()
                                                && ts_realign_buf[pos] == TS_SYNC_BYTE
                                        });
                                        if all_sync {
                                            found_offset = Some(offset);
                                            break;
                                        }
                                    }

                                    if let Some(offset) = found_offset {
                                        if offset > 0 {
                                            let _ = ts_realign_buf.split_to(offset);
                                        }
                                        ts_sync_found = true;
                                    } else {
                                        ts_realign_buf.clear();
                                    }
                                }
                            }

                            if ts_sync_found {
                                while ts_realign_buf.len() >= ts_datagram_size {
                                    let datagram = ts_realign_buf.split_to(ts_datagram_size);
                                    // Send to both legs
                                    if let Ok(sent) = socket1.send_to(&datagram, dest1).await {
                                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                        stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                    }
                                    let _ = socket2.send_to(&datagram, dest2).await;
                                }
                            }
                        } else {
                            // RTP-wrapped: send to both legs
                            if let Ok(sent) = socket1.send_to(&packet.data, dest1).await {
                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                            }
                            let _ = socket2.send_to(&packet.data, dest2).await;
                        }

                        // FEC: generate and send to both legs
                        if let Some(ref mut encoder) = fec_encoder {
                            let rtp_header_len = 12;
                            let payload = if packet.data.len() > rtp_header_len {
                                &packet.data[rtp_header_len..]
                            } else {
                                &packet.data[..]
                            };

                            let fec_packets = encoder.process(packet.sequence_number, payload);
                            for fec_pkt in &fec_packets {
                                if let Ok(sent) = socket1.send_to(fec_pkt, dest1).await {
                                    stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                }
                                let _ = socket2.send_to(fec_pkt, dest2).await;
                            }
                            let sent = fec_sent_counter.load(Ordering::Relaxed);
                            stats.fec_packets_sent.store(sent, Ordering::Relaxed);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "RTP output '{}' (2022-7) lagged, dropped {n} packets",
                            config.id
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("RTP output '{}' (2022-7) broadcast channel closed", config.id);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
