// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native plain-UDP tunnel client (no QUIC) for SRT/RIST over relay or direct.
//!
//! This is the edge counterpart of the relay's [`bilbycast-relay` `udp_relay`]
//! data plane. It rides the SAME loopback-forwarder contract as the QUIC path
//! (`udp_forwarder::run_egress`/`run_ingress`) — the SRT/RIST endpoint still
//! binds a local loopback port — but swaps the carrier from a `quinn::Connection`
//! ([`udp_forwarder::QuicLink`]) to a plain connected `UdpSocket`
//! ([`udp_forwarder::PlainUdpLink`]). No QUIC overhead, no second congestion
//! controller fighting SRT/RIST ARQ.
//!
//! **Relay mode:** both edges connect outbound to the relay's UDP listener
//! (firewall traversal), each periodically sending an authenticated
//! `Register` control datagram (HMAC bind token, same registry as the QUIC
//! path). The relay latches each edge's source address and forwards media
//! verbatim between them.
//!
//! **Direct mode:** the caller (egress) connects to the listener's public UDP
//! port and registers with a PSK token; the listener latches the caller's source
//! address and bridges. (For NAT-on-both-sides without a public port, use SRT's
//! native Rendezvous mode at the SRT layer, or relay mode.)
//!
//! **RIST note:** RIST uses an even RTP port + odd RTCP port. A native-RIST
//! service is provisioned as a *pair* of these single-port tunnels (one per
//! port) by the manager, so RTCP/NACK retransmission traverses correctly with
//! zero RIST-specific forwarding code here.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::config::TunnelDirection;
use super::crypto::TunnelCipher;
use super::protocol::{
    self, encode_udp_control, try_decode_udp_control, RelayDirection, UdpRelayControl,
    TUNNEL_PROTOCOL_VERSION,
};
use super::relay_client::RelayTunnelState;
use super::udp_forwarder::{self, PlainUdpLink, UdpForwarderStats};
use crate::manager::events::EventSender;

/// How often the edge re-sends a `Register` (keepalive) to maintain the relay's
/// source-address latch + NAT binding. Must be well under the relay's idle
/// timeout (30 s).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// No control ack for this long on the current relay → consider it dead and
/// rotate to the next `relay_addrs` entry (sized like the QUIC ~25 s window so
/// cellular/satellite handovers don't flap).
const RELAY_DEAD_TIMEOUT_MS: u64 = 25_000;

/// Brief delay between failover attempts.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Parameters for a native plain-UDP relay tunnel.
pub struct NativeRelayParams {
    pub tunnel_id: Uuid,
    pub relay_addrs: Vec<String>,
    pub direction: TunnelDirection,
    pub tunnel_bind_secret: Option<String>,
}

fn relay_direction(d: TunnelDirection) -> RelayDirection {
    match d {
        TunnelDirection::Ingress => RelayDirection::Ingress,
        TunnelDirection::Egress => RelayDirection::Egress,
    }
}

fn direction_str(d: TunnelDirection) -> &'static str {
    match d {
        TunnelDirection::Ingress => "ingress",
        TunnelDirection::Egress => "egress",
    }
}

/// Bind an ephemeral UDP socket of the same address family as `remote` and
/// `connect` it so it only sends to / receives from the relay/peer.
async fn connect_socket(remote: SocketAddr) -> Result<Arc<UdpSocket>> {
    let bind: SocketAddr = if remote.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let sock = UdpSocket::bind(bind).await?;
    sock.connect(remote).await?;
    Ok(Arc::new(sock))
}

/// Run a native plain-UDP **relay** tunnel: connect outbound to the relay,
/// register/keepalive, and bridge the local loopback port over plain UDP.
/// Loops over `relay_addrs` for failover; returns only on cancellation.
#[allow(clippy::too_many_arguments)]
pub async fn run_native_relay_tunnel(
    params: NativeRelayParams,
    local_addr: SocketAddr,
    state_tx: watch::Sender<RelayTunnelState>,
    active_idx_tx: watch::Sender<usize>,
    cancel: CancellationToken,
    stats: Arc<UdpForwarderStats>,
    cipher: Option<Arc<TunnelCipher>>,
    _event_sender: EventSender,
) -> Result<()> {
    if params.relay_addrs.is_empty() {
        anyhow::bail!("native-UDP relay tunnel requires at least one relay address");
    }

    let bind_token = params.tunnel_bind_secret.as_deref().map(|secret| {
        super::auth::compute_bind_token(
            &params.tunnel_id.to_string(),
            direction_str(params.direction),
            secret,
        )
    });

    let mut idx = 0usize;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let relay_addr = params.relay_addrs[idx % params.relay_addrs.len()].clone();
        let _ = active_idx_tx.send(idx % params.relay_addrs.len());
        state_tx.send_replace(RelayTunnelState::Connecting);

        // Resolve + connect.
        let resolved = match lookup_host(&relay_addr).await.ok().and_then(|mut it| it.next()) {
            Some(a) => a,
            None => {
                tracing::warn!(tunnel_id = %params.tunnel_id, "native-UDP relay: cannot resolve '{relay_addr}'");
                rotate_after(&cancel, &mut idx).await;
                continue;
            }
        };
        let sock = match connect_socket(resolved).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(tunnel_id = %params.tunnel_id, "native-UDP relay: bind/connect to {resolved} failed: {e}");
                rotate_after(&cancel, &mut idx).await;
                continue;
            }
        };

        tracing::info!(
            tunnel_id = %params.tunnel_id,
            relay = %resolved,
            direction = %params.direction,
            "native-UDP relay tunnel connecting"
        );

        let link = PlainUdpLink::new(sock.clone());
        let ready = link.ready.clone();
        let last_ack = link.last_ack_ms.clone();

        // Reset liveness for this attempt.
        ready.store(false, Ordering::Relaxed);
        last_ack.store(udp_forwarder::now_epoch_ms(), Ordering::Relaxed);

        let attempt_cancel = cancel.child_token();

        // Register / keepalive + liveness watchdog task.
        {
            let reg_sock = sock.clone();
            let reg_cancel = attempt_cancel.clone();
            let tunnel_id = params.tunnel_id;
            let direction = relay_direction(params.direction);
            let bind_token = bind_token.clone();
            let ready = ready.clone();
            let last_ack = last_ack.clone();
            let state_tx = state_tx.clone();
            tokio::spawn(async move {
                let register = match encode_udp_control(&UdpRelayControl::Register {
                    tunnel_id,
                    direction,
                    bind_token,
                    protocol_version: TUNNEL_PROTOCOL_VERSION,
                }) {
                    Ok(b) => b,
                    Err(_) => return,
                };
                // Send one immediately, then on each tick.
                let _ = reg_sock.send(&register).await;
                let mut tick = tokio::time::interval(KEEPALIVE_INTERVAL);
                tick.tick().await; // consume the immediate first tick
                loop {
                    tokio::select! {
                        _ = reg_cancel.cancelled() => return,
                        _ = tick.tick() => {
                            let _ = reg_sock.send(&register).await;
                            // Promote to Ready once the relay acks both sides latched.
                            if ready.load(Ordering::Relaxed) {
                                state_tx.send_if_modified(|s| {
                                    if *s != RelayTunnelState::Ready { *s = RelayTunnelState::Ready; true } else { false }
                                });
                            }
                            // Liveness: no ack within the dead window → rotate relay.
                            let since = udp_forwarder::now_epoch_ms()
                                .saturating_sub(last_ack.load(Ordering::Relaxed));
                            if since > RELAY_DEAD_TIMEOUT_MS {
                                tracing::warn!(%tunnel_id, "native-UDP relay silent for {since} ms — rotating");
                                reg_cancel.cancel();
                                return;
                            }
                        }
                    }
                }
            });
        }

        // Run the loopback forwarder over the plain-UDP link until the attempt
        // is cancelled (relay dead / outer shutdown) or the forwarder errors.
        let fwd = match params.direction {
            // Edge Egress = source side: bind local_addr, listen for the SRT/RIST
            // caller, send INTO the tunnel.
            TunnelDirection::Egress => {
                udp_forwarder::run_egress(
                    params.tunnel_id,
                    local_addr,
                    link,
                    stats.clone(),
                    attempt_cancel.clone(),
                    cipher.clone(),
                )
                .await
            }
            // Edge Ingress = destination side: receive from the tunnel, forward
            // to the local loopback (SRT/RIST listener).
            TunnelDirection::Ingress => {
                udp_forwarder::run_ingress(
                    params.tunnel_id,
                    local_addr,
                    link,
                    stats.clone(),
                    attempt_cancel.clone(),
                    cipher.clone(),
                )
                .await
            }
        };

        state_tx.send_replace(RelayTunnelState::Down);
        if cancel.is_cancelled() {
            return Ok(());
        }
        if let Err(e) = fwd {
            tracing::debug!(tunnel_id = %params.tunnel_id, "native-UDP forwarder exited: {e}");
        }
        rotate_after(&cancel, &mut idx).await;
    }
}

/// Advance the relay index and sleep the retry delay (cancel-aware).
async fn rotate_after(cancel: &CancellationToken, idx: &mut usize) {
    *idx = idx.wrapping_add(1);
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(RETRY_DELAY) => {}
    }
}

// ── Direct mode (peer-to-peer, no relay) ──

/// Run a native plain-UDP **direct** tunnel.
///
/// - Egress (caller): connect to the listener's public `peer_addr`, register
///   with a PSK token, bridge loopback → peer.
/// - Ingress (listener): bind the public `listen_addr`, await an authenticated
///   register, latch the caller's source addr, bridge tunnel → loopback.
#[allow(clippy::too_many_arguments)]
pub async fn run_native_direct_tunnel(
    tunnel_id: Uuid,
    direction: TunnelDirection,
    local_addr: SocketAddr,
    peer_addr: Option<String>,
    direct_listen_addr: Option<String>,
    tunnel_psk: String,
    state_tx: watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    stats: Arc<UdpForwarderStats>,
    cipher: Option<Arc<TunnelCipher>>,
) -> Result<()> {
    match direction {
        TunnelDirection::Egress => {
            let peer = peer_addr.ok_or_else(|| anyhow::anyhow!("peer_addr required for direct egress"))?;
            let resolved = lookup_host(&peer)
                .await?
                .next()
                .ok_or_else(|| anyhow::anyhow!("cannot resolve peer '{peer}'"))?;
            let sock = connect_socket(resolved).await?;
            let link = PlainUdpLink::new(sock.clone());

            // PSK-authenticated register/keepalive toward the listener.
            let token = super::auth::generate_token(&tunnel_id.to_string(), &tunnel_psk);
            let attempt_cancel = cancel.child_token();
            {
                let reg_sock = sock.clone();
                let reg_cancel = attempt_cancel.clone();
                tokio::spawn(async move {
                    let register = match encode_udp_control(&UdpRelayControl::Register {
                        tunnel_id,
                        direction: RelayDirection::Egress,
                        bind_token: Some(token),
                        protocol_version: TUNNEL_PROTOCOL_VERSION,
                    }) {
                        Ok(b) => b,
                        Err(_) => return,
                    };
                    let _ = reg_sock.send(&register).await;
                    let mut tick = tokio::time::interval(KEEPALIVE_INTERVAL);
                    tick.tick().await;
                    loop {
                        tokio::select! {
                            _ = reg_cancel.cancelled() => return,
                            _ = tick.tick() => { let _ = reg_sock.send(&register).await; }
                        }
                    }
                });
            }
            state_tx.send_replace(RelayTunnelState::Ready);
            udp_forwarder::run_egress(tunnel_id, local_addr, link, stats, attempt_cancel, cipher).await
        }
        TunnelDirection::Ingress => {
            let listen = direct_listen_addr
                .ok_or_else(|| anyhow::anyhow!("direct_listen_addr required for direct ingress"))?;
            let listen_sa: SocketAddr = listen
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid direct_listen_addr '{listen}': {e}"))?;
            run_native_direct_listener(
                tunnel_id, listen_sa, local_addr, tunnel_psk, state_tx, cancel, stats, cipher,
            )
            .await
        }
    }
}

/// Direct-mode listener: bind the public UDP port, authenticate the caller's
/// register (PSK), latch its source address, and bridge tunnel ↔ loopback.
#[allow(clippy::too_many_arguments)]
async fn run_native_direct_listener(
    tunnel_id: Uuid,
    listen_addr: SocketAddr,
    forward_addr: SocketAddr,
    tunnel_psk: String,
    state_tx: watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    stats: Arc<UdpForwarderStats>,
    cipher: Option<Arc<TunnelCipher>>,
) -> Result<()> {
    let public = UdpSocket::bind(listen_addr).await.map_err(|e| {
        crate::util::port_error::annotate_bind_error(e, listen_addr, "native-UDP direct listener")
    })?;
    let public = Arc::new(public);
    // Ephemeral socket to deliver to / receive return traffic from loopback.
    let loop_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    state_tx.send_replace(RelayTunnelState::Connecting);
    tracing::info!(%tunnel_id, listen = %listen_addr, "native-UDP direct listener started");

    let caller = Arc::new(super::udp_forwarder::AtomicPeerAddr::new());
    // Separate buffers: the two select! arms each hold a distinct &mut borrow.
    let mut buf_pub = vec![0u8; 2048];
    let mut buf_loop = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            // Public → loopback (media) or control (register).
            r = public.recv_from(&mut buf_pub) => {
                let (n, from) = r?;
                let data = &buf_pub[..n];
                if let Some(UdpRelayControl::Register { bind_token, .. }) = try_decode_udp_control(data) {
                    let ok = bind_token
                        .as_deref()
                        .and_then(|t| super::auth::verify_token(t, &tunnel_psk))
                        .map(|id| id == tunnel_id.to_string())
                        .unwrap_or(false);
                    if ok {
                        caller.store(from);
                        state_tx.send_replace(RelayTunnelState::Ready);
                        // Ack so the caller confirms reachability.
                        if let Ok(ack) = encode_udp_control(&UdpRelayControl::Ack { tunnel_id, ready: true }) {
                            let _ = public.send_to(&ack, from).await;
                        }
                    } else {
                        tracing::warn!(%tunnel_id, "native-UDP direct: rejected register from {from} (bad PSK)");
                    }
                    continue;
                }
                // Media: only accept from the latched caller.
                if caller.load() != Some(from) { continue; }
                if let Some((_id, enc)) = protocol::decode_udp_datagram(data) {
                    let payload = match &cipher {
                        Some(c) => match c.decrypt(enc) { Ok(p) => p, Err(_) => continue },
                        None => enc.to_vec(),
                    };
                    let _ = loop_sock.send_to(&payload, forward_addr).await;
                    stats.packets_received.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_received.fetch_add(payload.len() as u64, Ordering::Relaxed);
                }
            }
            // Loopback return → public (back to the caller).
            r = loop_sock.recv_from(&mut buf_loop) => {
                let (n, _from) = r?;
                let Some(dest) = caller.load() else { continue };
                let payload = match &cipher {
                    Some(c) => match c.encrypt(&buf_loop[..n]) { Ok(p) => p, Err(_) => { stats.send_errors.fetch_add(1, Ordering::Relaxed); continue } },
                    None => buf_loop[..n].to_vec(),
                };
                let datagram = protocol::encode_udp_datagram(&tunnel_id, &payload);
                let _ = public.send_to(&datagram, dest).await;
                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                stats.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
    }
}
