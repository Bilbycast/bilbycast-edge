// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! RIST Simple Profile (TR-06-1:2020) input.
//!
//! Binds a dual-port UDP channel (RTP on even port P, RTCP on P+1) via
//! [`rist_transport::RistSocket::receiver`], consumes the decoded RTP
//! payload stream, and publishes each datagram onto the flow's per-input
//! broadcast channel. RIST's NACK-based retransmission and jitter buffer
//! happen inside the `RistSocket`, so by the time bytes reach this module
//! they are already in delivery order.
//!
//! Payload handling mirrors the UDP input: we treat every datagram as raw
//! MPEG-TS (`is_raw_ts: true`) and synthesise sequence numbers / RTP
//! timestamps for the downstream 2022-7 merger and stats counters.
//!
//! Optional SMPTE 2022-7 redundancy: two `RistSocket::receiver` instances
//! bound on separate even ports feed a shared [`HitlessMerger`] using
//! synthetic per-leg sequence counters.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use rist_transport::{RistSocket, RistSocketConfig};

use crate::config::models::RistInputConfig;
use crate::manager::events::{EventSender, EventSeverity};
use crate::redundancy::merger::{ActiveLeg, HitlessMerger};
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::time::now_us;

use super::packet::RtpPacket;

const TS_PACKET_SIZE: usize = 188;

/// Spawn a RIST input task. Produces `RtpPacket { is_raw_ts: true, .. }`.
pub fn spawn_rist_input(
    config: RistInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = if config.redundancy.is_some() {
            rist_input_redundant_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id)
                .await
        } else {
            rist_input_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id).await
        };
        if let Err(e) = result {
            tracing::error!("RIST input task exited with error: {e}");
            event_sender.emit_flow(
                EventSeverity::Critical,
                "rist",
                format!("RIST input lost: {e}"),
                &flow_id,
            );
        }
    })
}

fn socket_config_for(config: &RistInputConfig, local_addr: SocketAddr) -> RistSocketConfig {
    let mut sc = RistSocketConfig::default();
    sc.local_addr = local_addr;
    if let Some(ms) = config.buffer_ms {
        sc.buffer_size = Duration::from_millis(ms as u64);
    }
    if let Some(r) = config.max_nack_retries {
        sc.max_nack_retries = r;
    }
    if let Some(ms) = config.rtcp_interval_ms {
        sc.rtcp_interval = Duration::from_millis(ms as u64);
    }
    sc.cname = config.cname.clone();
    sc
}

async fn rist_input_loop(
    config: RistInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    let bind: SocketAddr = config.bind_addr.parse()?;
    let sc = socket_config_for(&config, bind);

    let mut socket = RistSocket::receiver(sc).await.map_err(|e| {
        events.emit_flow(
            EventSeverity::Critical,
            "rist",
            format!("RIST input bind failed: {e}"),
            flow_id,
        );
        anyhow::anyhow!("RIST receiver bind failed: {e}")
    })?;

    tracing::info!("RIST input started on {}", bind);
    events.emit_flow(
        EventSeverity::Info,
        "rist",
        format!("RIST input listening on {}", bind),
        flow_id,
    );

    let mut seq_counter: u16 = 0;
    let mut ts_counter: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RIST input on {} stopping (cancelled)", bind);
                socket.close();
                return Ok(());
            }
            recv = socket.recv() => {
                match recv {
                    Some(data) => {
                        publish(
                            &data,
                            &broadcast_tx,
                            &stats,
                            &mut seq_counter,
                            &mut ts_counter,
                        );
                    }
                    None => {
                        // Socket closed — exit cleanly.
                        tracing::info!("RIST input recv channel closed on {}", bind);
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn publish(
    data: &Bytes,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: &Arc<FlowStatsAccumulator>,
    seq_counter: &mut u16,
    ts_counter: &mut u32,
) {
    let seq = *seq_counter;
    *seq_counter = seq_counter.wrapping_add(1);
    let ts_pkts = (data.len() / TS_PACKET_SIZE) as u32;
    *ts_counter = ts_counter.wrapping_add(ts_pkts.max(1) * 188 * 8);

    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let packet = RtpPacket {
        data: data.clone(),
        sequence_number: seq,
        rtp_timestamp: *ts_counter,
        recv_time_us: now_us(),
        is_raw_ts: true,
    };

    let _ = broadcast_tx.send(packet);
}

async fn rist_input_redundant_loop(
    config: RistInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    let leg1_addr: SocketAddr = config.bind_addr.parse()?;
    let leg2_addr: SocketAddr = redundancy.bind_addr.parse()?;

    let mut socket_leg1 = RistSocket::receiver(socket_config_for(&config, leg1_addr))
        .await
        .map_err(|e| anyhow::anyhow!("RIST input leg 1 bind failed: {e}"))?;
    let mut socket_leg2 = RistSocket::receiver(socket_config_for(&config, leg2_addr))
        .await
        .map_err(|e| anyhow::anyhow!("RIST input leg 2 bind failed: {e}"))?;

    tracing::info!(
        "RIST input started with 2022-7 redundancy: leg1={} leg2={}",
        leg1_addr,
        leg2_addr
    );
    events.emit_flow(
        EventSeverity::Info,
        "rist",
        format!(
            "RIST input listening (2022-7): leg1={} leg2={}",
            leg1_addr, leg2_addr
        ),
        flow_id,
    );

    let mut merger = HitlessMerger::new();
    let mut seq_counter_leg1: u16 = 0;
    let mut seq_counter_leg2: u16 = 0;
    let mut ts_counter: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RIST redundant input stopping (cancelled)");
                socket_leg1.close();
                socket_leg2.close();
                return Ok(());
            }
            recv = socket_leg1.recv() => match recv {
                Some(data) => handle_redundant_leg(
                    &data, ActiveLeg::Leg1, &mut merger,
                    &mut seq_counter_leg1, &mut ts_counter,
                    &broadcast_tx, &stats,
                ),
                None => {
                    tracing::warn!("RIST input leg 1 closed");
                    return Ok(());
                }
            },
            recv = socket_leg2.recv() => match recv {
                Some(data) => handle_redundant_leg(
                    &data, ActiveLeg::Leg2, &mut merger,
                    &mut seq_counter_leg2, &mut ts_counter,
                    &broadcast_tx, &stats,
                ),
                None => {
                    tracing::warn!("RIST input leg 2 closed");
                    return Ok(());
                }
            },
        }
    }
}

fn handle_redundant_leg(
    data: &Bytes,
    leg: ActiveLeg,
    merger: &mut HitlessMerger,
    leg_seq: &mut u16,
    ts_counter: &mut u32,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: &Arc<FlowStatsAccumulator>,
) {
    let seq = *leg_seq;
    *leg_seq = leg_seq.wrapping_add(1);

    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

    if merger.try_merge(seq, leg).is_none() {
        return;
    }

    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let ts_pkts = (data.len() / TS_PACKET_SIZE) as u32;
    *ts_counter = ts_counter.wrapping_add(ts_pkts.max(1) * 188 * 8);

    let packet = RtpPacket {
        data: data.clone(),
        sequence_number: seq,
        rtp_timestamp: *ts_counter,
        recv_time_us: now_us(),
        is_raw_ts: true,
    };

    let _ = broadcast_tx.send(packet);
}
