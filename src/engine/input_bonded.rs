// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
    BondSocket, BondSocketConfig, FecParams, PathConfig as BondPathTxCfg,
    PathTransport as BondPathTxTransport, QuicRole as BondQuicRoleTx, QuicTlsMode as BondQuicTlsTx,
    RistRole as BondRistRoleTx,
};

use crate::config::models::{
    BondPathTransportConfig, BondQuicRole, BondQuicTls, BondRistRole, BondedInputConfig,
};
use crate::manager::events::{
    BondEventScope, EventSender, EventSeverity, category, run_bond_event_forwarder,
};
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
        // Register this input's legs as "in use" (held for the task's
        // life) so a per-leg capacity probe won't saturate a link this
        // flow is receiving on. See engine::bond_leg_probe.
        let _active_legs = super::bond_leg_probe::register_active_legs(
            &flow_id,
            super::bond_leg_probe::leg_keys_for_paths(&config.paths),
        );

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
                // Receiver legs bind a listening port (inbound only) — there is
                // no meaningful egress interface, so leave it `None` (the
                // cellular signal join is a sender-side concern). See the
                // BondPathStatsHandle.interface contract.
                path_handles.push(crate::stats::collector::BondPathStatsHandle::new(
                    p.id,
                    p.name.clone(),
                    bond_transport_label(&p.transport),
                    ps,
                ));
            }
        }
        stats.set_input_bond_stats(crate::stats::collector::BondStatsHandle {
            flow_id: config.bond_flow_id,
            role: crate::stats::collector::BondStatsRole::Receiver,
            scheduler: String::new(), // receivers don't schedule
            conn_stats: socket.stats(),
            paths: path_handles,
            // Sender-side counter; stays 0 on the receiver.
            oversize_payloads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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

        // Forward bonding lifecycle events (per-path alive/dead +
        // bond-aggregate transitions) to the manager event feed.
        tokio::spawn(run_bond_event_forwarder(
            socket.subscribe_events(),
            event_sender.clone(),
            flow_id.clone(),
            BondEventScope::Input,
            input_id.clone(),
            cancel.clone(),
        ));

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
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
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
    if let Some(ms) = cfg.hold_max_ms {
        out.hold_max = Some(Duration::from_millis(ms as u64));
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
    if let Some(hex_key) = &cfg.encryption_key {
        out.encryption_key = Some(decode_bond_key(hex_key)?);
    }
    if let Some(f) = &cfg.fec {
        out.fec = Some(FecParams {
            columns: f.columns,
            rows: f.rows,
        });
    }
    // Per-leg FEC: each path that carries its own `fec` runs an independent
    // decoder over only that leg's packets (validation rejects mixing this
    // with the combined `fec` above). A non-empty map selects per-leg mode.
    for p in &cfg.paths {
        if let Some(f) = &p.fec {
            out.per_path_fec.insert(
                p.id,
                FecParams {
                    columns: f.columns,
                    rows: f.rows,
                },
            );
        }
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
        BondPathTransportConfig::Udp {
            bind,
            remote,
            interface,
            // gateway/source are sender-side only (validation rejects
            // them on a receiver); ignore on the receive path.
            gateway: _,
            source: _,
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
            bind,
            interface,
        } => BondPathTxTransport::Quic {
            role: match role {
                BondQuicRole::Client => BondQuicRoleTx::Client,
                BondQuicRole::Server => BondQuicRoleTx::Server,
            },
            addr: parse_sockaddr(addr)?,
            server_name: server_name.clone(),
            tls: translate_tls(tls)?,
            bind: bind.as_deref().map(parse_sockaddr).transpose()?,
            interface: interface.clone(),
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
    // Fast path: a literal `ip:port`.
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(addr);
    }
    // Fall back to DNS for a `host:port` remote so an operator can point a
    // bond leg at a hostname (e.g. `home.example.com:7400`) instead of a
    // bare IP literal that has to be hand-edited every time the address
    // changes. Resolved once here at flow-build time — an IP change after
    // that needs a flow restart, same as any resolve-at-connect transport.
    // This runs at flow start, never on the data path. Prefer an IPv4
    // record (bond cellular / CGNAT legs are v4) and fall back to the
    // first record of any family.
    use std::net::ToSocketAddrs;
    let resolved: Vec<SocketAddr> = s
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("invalid or unresolvable socket address '{s}': {e}"))?
        .collect();
    resolved
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| resolved.first())
        .copied()
        .ok_or_else(|| anyhow::anyhow!("socket address '{s}' resolved to no records"))
}

/// Decode a 64-hex-char bond AEAD key into its 32 raw bytes. Shared by
/// the bonded input (receiver) and output (sender) so both ends derive
/// the same key bytes from the operator's hex string.
pub(crate) fn decode_bond_key(hex: &str) -> anyhow::Result<Vec<u8>> {
    let hex = hex.trim();
    if hex.len() != 64 {
        anyhow::bail!(
            "bond encryption_key must be 64 hex chars (32 bytes), got {}",
            hex.len()
        );
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("invalid hex in bond encryption_key at {i}: {e}"))
        })
        .collect()
}

/// String label for the path transport, used in stats + Prometheus.
pub(crate) fn bond_transport_label(t: &BondPathTransportConfig) -> String {
    match t {
        BondPathTransportConfig::Udp { .. } => "udp".to_string(),
        BondPathTransportConfig::Rist { .. } => "rist".to_string(),
        BondPathTransportConfig::Quic { .. } => "quic".to_string(),
    }
}

/// The kernel netdev an interface-mode UDP leg egresses on, for the bond-leg
/// stats join (cellular signal strip). `None` for gateway-mode legs (the
/// gateway IP + policy route do the steering, not a NIC pin) and for
/// QUIC / RIST legs.
pub(crate) fn bond_path_interface(t: &BondPathTransportConfig) -> Option<String> {
    match t {
        BondPathTransportConfig::Udp {
            interface, gateway, ..
        } if gateway.is_none() => interface.clone(),
        _ => None,
    }
}

#[cfg(test)]
mod bonded_input_tests {
    use super::*;

    #[test]
    fn bond_transport_label_names_each_transport() {
        let udp: crate::config::models::BondPathTransportConfig =
            serde_json::from_str(r#"{ "type": "udp", "bind": "0.0.0.0:7400" }"#).unwrap();
        assert_eq!(bond_transport_label(&udp).as_str(), "udp");
    }

    #[test]
    fn parse_sockaddr_takes_literal_then_resolves_hostnames() {
        // Literal ip:port — the fast path, byte-exact.
        let lit = parse_sockaddr("203.0.113.7:7400").unwrap();
        assert_eq!(lit, "203.0.113.7:7400".parse::<SocketAddr>().unwrap());
        // Hostname:port resolves (localhost is in /etc/hosts → no network),
        // preferring the IPv4 record.
        let host = parse_sockaddr("localhost:7400").unwrap();
        assert_eq!(host.port(), 7400);
        assert!(host.ip().is_loopback());
        // A bare host with no port is rejected.
        assert!(parse_sockaddr("localhost").is_err());
    }

    #[test]
    fn build_receiver_cfg_maps_paths_and_flow_id() {
        let cfg: crate::config::models::BondedInputConfig = serde_json::from_str(
            r#"{ "bond_flow_id": 6016,
                 "paths": [
                   { "id": 0, "name": "a", "transport": { "type": "udp", "bind": "0.0.0.0:7400" } },
                   { "id": 1, "name": "b", "transport": { "type": "udp", "bind": "0.0.0.0:7401" } }
                 ] }"#,
        )
        .unwrap();
        let sock = build_receiver_cfg(&cfg).expect("receiver cfg builds");
        assert_eq!(sock.flow_id, 6016);
        assert_eq!(sock.paths.len(), 2);
    }
}
