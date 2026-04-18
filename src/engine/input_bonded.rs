// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Bonded input.
//!
//! Binds N bond paths (UDP / QUIC / RIST) via
//! [`bonding_transport::BondSocket::receiver`], reassembles the
//! bond-sequenced payloads in order, and publishes each delivered
//! datagram as `RtpPacket { is_raw_ts: true, .. }` onto the flow's
//! per-input broadcast channel. Bonding-layer NACK recovery, keepalive
//! liveness, and per-path stats are handled inside the `BondSocket`.
//!
//! Downstream consumers see a single ordered stream of payloads —
//! exactly like the UDP and RIST inputs. No per-packet overhead
//! beyond the existing broadcast-channel hop.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use bonding_transport::{
    BondSocket, BondSocketConfig, PathConfig as BondPathTxCfg,
    PathTransport as BondPathTxTransport, QuicRole as BondQuicRoleTx, QuicTlsMode as BondQuicTlsTx,
    RistRole as BondRistRoleTx,
};

use crate::config::models::{
    BondPathTransportConfig, BondQuicRole, BondQuicTls, BondRistRole, BondedInputConfig,
};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::time::now_us;

use super::packet::RtpPacket;

const TS_PACKET_SIZE: usize = 188;

/// Spawn a bonded input task. Produces `RtpPacket { is_raw_ts: true, .. }`.
pub fn spawn_bonded_input(
    config: BondedInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let socket_cfg = match build_receiver_cfg(&config) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("bonded input '{}': config translation failed: {}", input_id, e);
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::MEDIA,
                    format!("bonded input config invalid: {e}"),
                    &flow_id,
                );
                return;
            }
        };

        let socket = match BondSocket::receiver(socket_cfg).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("bonded input '{}': bind failed: {}", input_id, e);
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::MEDIA,
                    format!("bonded input bind failed: {e}"),
                    &flow_id,
                );
                return;
            }
        };

        // Collect per-path stats handles so the accumulator snapshot
        // path can surface `BondLegStats.paths[…].rtt_ms`, loss,
        // keepalive liveness, etc. The operator-facing `name` and
        // `transport` labels come from the config so the UI doesn't
        // need to re-read it.
        let mut path_handles: Vec<crate::stats::collector::BondPathStatsHandle> =
            Vec::with_capacity(config.paths.len());
        for p in &config.paths {
            if let Some(ps) = socket.path_stats(p.id) {
                path_handles.push(crate::stats::collector::BondPathStatsHandle {
                    id: p.id,
                    name: p.name.clone(),
                    transport: bond_transport_label(&p.transport),
                    stats: ps,
                });
            }
        }
        stats.set_input_bond_stats(crate::stats::collector::BondStatsHandle {
            flow_id: config.bond_flow_id,
            role: crate::stats::collector::BondStatsRole::Receiver,
            scheduler: String::new(), // receivers don't schedule
            conn_stats: socket.stats(),
            paths: path_handles,
        });

        tracing::info!(
            "bonded input '{}' up — {} path(s), bond_flow_id={}",
            input_id,
            config.paths.len(),
            config.bond_flow_id
        );
        event_sender.emit_flow(
            EventSeverity::Info,
            category::MEDIA,
            format!(
                "bonded input listening: {} path(s), bond_flow_id={}",
                config.paths.len(),
                config.bond_flow_id
            ),
            &flow_id,
        );

        let mut seq_counter: u16 = 0;
        let mut ts_counter: u32 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("bonded input '{}' stopping (cancelled)", input_id);
                    socket.close();
                    return;
                }
                recv = socket.recv() => match recv {
                    Some(payload) => {
                        publish(
                            &payload,
                            &broadcast_tx,
                            &stats,
                            &mut seq_counter,
                            &mut ts_counter,
                        );
                    }
                    None => {
                        tracing::info!("bonded input '{}' recv channel closed", input_id);
                        return;
                    }
                },
            }
        }
    })
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

/// Translate an edge `BondedInputConfig` into a
/// `bonding_transport::BondSocketConfig` for the receiver side.
pub(crate) fn build_receiver_cfg(cfg: &BondedInputConfig) -> anyhow::Result<BondSocketConfig> {
    let mut out = BondSocketConfig {
        flow_id: cfg.bond_flow_id,
        paths: Vec::with_capacity(cfg.paths.len()),
        ..Default::default()
    };
    if let Some(ms) = cfg.hold_ms {
        out.hold_time = Duration::from_millis(ms as u64);
    }
    if let Some(ms) = cfg.keepalive_ms {
        out.keepalive_interval = Duration::from_millis(ms as u64);
    }
    if let Some(ms) = cfg.nack_delay_ms {
        out.nack_delay = Duration::from_millis(ms as u64);
    }
    if let Some(n) = cfg.max_nack_retries {
        out.max_nack_retries = n;
    }
    for p in &cfg.paths {
        out.paths.push(BondPathTxCfg {
            id: p.id,
            name: p.name.clone(),
            weight_hint: p.weight_hint,
            transport: translate_transport_for_receiver(&p.transport)?,
        });
    }
    Ok(out)
}

fn translate_transport_for_receiver(
    t: &BondPathTransportConfig,
) -> anyhow::Result<BondPathTxTransport> {
    Ok(match t {
        BondPathTransportConfig::Udp { bind, remote } => BondPathTxTransport::Udp {
            bind: bind.as_deref().map(parse_sockaddr).transpose()?,
            remote: remote.as_deref().map(parse_sockaddr).transpose()?,
        },
        BondPathTransportConfig::Rist {
            role,
            remote,
            local_bind,
            buffer_ms,
        } => {
            let role = match role {
                BondRistRole::Sender => BondRistRoleTx::Sender,
                BondRistRole::Receiver => BondRistRoleTx::Receiver,
            };
            BondPathTxTransport::Rist {
                role,
                remote: remote.as_deref().map(parse_sockaddr).transpose()?,
                local_bind: local_bind.as_deref().map(parse_sockaddr).transpose()?,
                buffer_ms: *buffer_ms,
            }
        }
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

pub(crate) fn translate_tls(tls: &BondQuicTls) -> anyhow::Result<BondQuicTlsTx> {
    Ok(match tls {
        BondQuicTls::SelfSigned => BondQuicTlsTx::SelfSigned,
        BondQuicTls::Pem {
            cert_chain_path,
            private_key_path,
            client_trust_root_path,
        } => {
            let cert_chain = std::fs::read(cert_chain_path)
                .map_err(|e| anyhow::anyhow!("read cert chain '{cert_chain_path}': {e}"))?;
            let private_key = std::fs::read(private_key_path)
                .map_err(|e| anyhow::anyhow!("read private key '{private_key_path}': {e}"))?;
            let client_trust_root = match client_trust_root_path {
                Some(p) => Some(
                    std::fs::read(p).map_err(|e| anyhow::anyhow!("read trust root '{p}': {e}"))?,
                ),
                None => None,
            };
            BondQuicTlsTx::Pem {
                cert_chain,
                private_key,
                client_trust_root,
            }
        }
    })
}

pub(crate) fn parse_sockaddr(s: &str) -> anyhow::Result<SocketAddr> {
    s.parse::<SocketAddr>()
        .map_err(|e| anyhow::anyhow!("invalid socket address '{s}': {e}"))
}

/// String label for the path transport, used in stats + Prometheus.
pub(crate) fn bond_transport_label(t: &BondPathTransportConfig) -> String {
    match t {
        BondPathTransportConfig::Udp { .. } => "udp".to_string(),
        BondPathTransportConfig::Rist { .. } => "rist".to_string(),
        BondPathTransportConfig::Quic { .. } => "quic".to_string(),
    }
}
