// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Bonded output.
//!
//! Subscribes to the flow's broadcast channel, frames each outbound
//! payload into the bond protocol, and transmits across N paths via
//! [`bonding_transport::BondSocket::sender`]. The scheduler decides
//! which path (or paths, for duplication) carries each packet.
//!
//! ## Scheduler selection
//!
//! - [`BondSchedulerKind::RoundRobin`] — built-in
//!   `RoundRobinScheduler`.
//! - [`BondSchedulerKind::WeightedRtt`] — built-in
//!   `WeightedRttScheduler`.
//! - [`BondSchedulerKind::MediaAware`] — edge-owned
//!   [`super::bonded_scheduler::MediaAwareScheduler`]. Walks the TS
//!   stream, finds H.264 / HEVC NAL boundaries, and tags SPS / PPS /
//!   IDR packets as `Priority::Critical` so the inner scheduler
//!   duplicates them across the two lowest-RTT paths.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use bonding_transport::{
    BondSocket, BondSocketConfig, PacketHints, PathConfig as BondPathTxCfg,
    PathTransport as BondPathTxTransport, RoundRobinScheduler, WeightedRttScheduler,
};

use crate::config::models::{BondedOutputConfig, BondSchedulerKind};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;

use super::input_bonded::{parse_sockaddr, translate_tls};
use super::packet::RtpPacket;
use super::ts_program_filter::TsProgramFilter;

const RTP_HEADER_MIN_SIZE: usize = 12;

/// Spawn a bonded output task.
pub fn spawn_bonded_output(
    config: BondedOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();
    tokio::spawn(async move {
        if let Err(e) =
            bonded_output_loop(&config, &mut rx, output_stats, cancel, &event_sender, &flow_id).await
        {
            tracing::error!("bonded output '{}' exited with error: {e}", config.id);
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::MEDIA,
                format!("bonded output '{}' exited with error: {e}", config.id),
                &flow_id,
            );
        }
    })
}

async fn bonded_output_loop(
    config: &BondedOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    let socket_cfg = build_sender_cfg(config)?;
    let path_ids: Vec<u8> = socket_cfg.paths.iter().map(|p| p.id).collect();

    // Pick scheduler. MediaAware wraps WeightedRtt and adds NAL
    // awareness on top; see engine::bonded_scheduler.
    let socket = match config.scheduler {
        BondSchedulerKind::RoundRobin => {
            BondSocket::sender(socket_cfg, RoundRobinScheduler::new(path_ids.clone()))
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
        BondSchedulerKind::WeightedRtt => {
            BondSocket::sender(socket_cfg, WeightedRttScheduler::new(path_ids.clone()))
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
        BondSchedulerKind::MediaAware => {
            // MediaAware scheduler only influences the *hints* we pass
            // to send(); the inner scheduler is WeightedRtt so
            // `Critical`-priority packets duplicate across the two
            // lowest-RTT paths.
            BondSocket::sender(socket_cfg, WeightedRttScheduler::new(path_ids.clone()))
                .await
                .map_err(|e| anyhow::anyhow!("bonded sender setup: {e}"))?
        }
    };

    // Register per-path + aggregate bond stats on the accumulator so
    // `OutputStats.bond_stats` carries path-level RTT, loss,
    // retransmit, keepalive state at snapshot time.
    let scheduler_label = match config.scheduler {
        BondSchedulerKind::RoundRobin => "round_robin",
        BondSchedulerKind::WeightedRtt => "weighted_rtt",
        BondSchedulerKind::MediaAware => "media_aware",
    }
    .to_string();
    let mut path_handles: Vec<crate::stats::collector::BondPathStatsHandle> =
        Vec::with_capacity(config.paths.len());
    for p in &config.paths {
        if let Some(ps) = socket.path_stats(p.id) {
            path_handles.push(crate::stats::collector::BondPathStatsHandle {
                id: p.id,
                name: p.name.clone(),
                transport: super::input_bonded::bond_transport_label(&p.transport),
                stats: ps,
            });
        }
    }
    stats.set_bond_stats(crate::stats::collector::BondStatsHandle {
        flow_id: config.bond_flow_id,
        role: crate::stats::collector::BondStatsRole::Sender,
        scheduler: scheduler_label,
        conn_stats: socket.stats(),
        paths: path_handles,
    });

    tracing::info!(
        "bonded output '{}' up: {} path(s), scheduler={:?}",
        config.id,
        path_ids.len(),
        config.scheduler
    );
    events.emit_flow(
        EventSeverity::Info,
        category::MEDIA,
        format!(
            "bonded output '{}' up — {} path(s)",
            config.id,
            path_ids.len()
        ),
        flow_id,
    );

    // Optional media-aware scheduler state.
    let mut media_sched = match config.scheduler {
        BondSchedulerKind::MediaAware => {
            Some(super::bonded_scheduler::MediaAwareScheduler::new())
        }
        _ => None,
    };

    let mut program_filter = config.program_number.map(TsProgramFilter::new);
    let mut filter_scratch: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("bonded output '{}' stopping (cancelled)", config.id);
                socket.close();
                return Ok(());
            }
            result = rx.recv() => match result {
                Ok(packet) => {
                    let ts_bytes: &[u8] = if packet.is_raw_ts {
                        &packet.data[..]
                    } else if packet.data.len() > RTP_HEADER_MIN_SIZE {
                        &packet.data[RTP_HEADER_MIN_SIZE..]
                    } else {
                        continue;
                    };

                    let filtered: &[u8] = if let Some(ref mut pf) = program_filter {
                        filter_scratch.clear();
                        pf.filter_into(ts_bytes, &mut filter_scratch);
                        if filter_scratch.is_empty() {
                            continue;
                        }
                        &filter_scratch
                    } else {
                        ts_bytes
                    };

                    // Decide priority / marker from media-aware
                    // scheduler (or fall back to defaults).
                    let hints = match media_sched.as_mut() {
                        Some(s) => s.hints_for(filtered),
                        None => PacketHints::default(),
                    };

                    // Emit via bond socket. We clone into a fresh Bytes
                    // so the bond layer owns its own refcount; if the
                    // caller holds `packet` elsewhere the copy is
                    // amortised with the scheduler's own send.
                    let payload = bytes::Bytes::copy_from_slice(filtered);
                    if let Err(e) = socket.send(payload, hints).await {
                        tracing::warn!("bonded output '{}' send error: {e}", config.id);
                    } else {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats
                            .bytes_sent
                            .fetch_add(filtered.len() as u64, Ordering::Relaxed);
                        stats.record_latency(packet.recv_time_us);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    tracing::warn!("bonded output '{}' lagged, dropped {n} packets", config.id);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("bonded output '{}' broadcast channel closed", config.id);
                    return Ok(());
                }
            },
        }
    }
}

fn build_sender_cfg(cfg: &BondedOutputConfig) -> anyhow::Result<BondSocketConfig> {
    let mut out = BondSocketConfig {
        flow_id: cfg.bond_flow_id,
        paths: Vec::with_capacity(cfg.paths.len()),
        ..Default::default()
    };
    if let Some(ms) = cfg.keepalive_ms {
        out.keepalive_interval = std::time::Duration::from_millis(ms as u64);
    }
    if let Some(cap) = cfg.retransmit_capacity {
        out.retransmit_capacity = cap;
    }
    for p in &cfg.paths {
        out.paths.push(BondPathTxCfg {
            id: p.id,
            name: p.name.clone(),
            weight_hint: p.weight_hint,
            transport: translate_transport_for_sender(&p.transport)?,
        });
    }
    Ok(out)
}

fn translate_transport_for_sender(
    t: &crate::config::models::BondPathTransportConfig,
) -> anyhow::Result<BondPathTxTransport> {
    use crate::config::models::{BondPathTransportConfig, BondQuicRole, BondRistRole};
    use bonding_transport::{QuicRole as BondQuicRoleTx, RistRole as BondRistRoleTx};
    Ok(match t {
        BondPathTransportConfig::Udp {
            bind,
            remote,
            interface,
        } => BondPathTxTransport::Udp {
            bind: bind.as_deref().map(parse_sockaddr).transpose()?,
            remote: remote.as_deref().map(parse_sockaddr).transpose()?,
            interface: interface.clone(),
        },
        BondPathTransportConfig::Rist {
            role,
            remote,
            local_bind,
            buffer_ms,
        } => BondPathTxTransport::Rist {
            role: match role {
                BondRistRole::Sender => BondRistRoleTx::Sender,
                BondRistRole::Receiver => BondRistRoleTx::Receiver,
            },
            remote: remote.as_deref().map(parse_sockaddr).transpose()?,
            local_bind: local_bind.as_deref().map(parse_sockaddr).transpose()?,
            buffer_ms: *buffer_ms,
        },
        BondPathTransportConfig::Quic {
            role,
            addr,
            server_name,
            tls,
        } => BondPathTxTransport::Quic {
            role: match role {
                BondQuicRole::Client => BondQuicRoleTx::Client,
                BondQuicRole::Server => BondQuicRoleTx::Server,
            },
            addr: parse_sockaddr(addr)?,
            server_name: server_name.clone(),
            tls: translate_tls(tls)?,
        },
    })
}
