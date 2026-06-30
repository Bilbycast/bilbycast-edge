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
use bytes::Bytes;
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::config::TunnelDirection;
use super::crypto::TunnelCipher;
use super::protocol::{
    self, encode_udp_control, try_decode_udp_control, RelayDirection, UdpRelayControl,
    TUNNEL_PROTOCOL_VERSION,
};
use super::relay_client::RelayTunnelState;
use super::udp_forwarder::{self, DatagramLink, PlainUdpLink, UdpForwarderStats};
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
    /// Optional NIC pin (SO_BINDTODEVICE) for the outbound relay socket —
    /// same mechanism as a bonded UDP leg. `None` → kernel default route.
    pub interface: Option<String>,
    /// Optional source address (`ip` or `ip/prefix`) the outbound relay
    /// socket binds to. On its own pins the egress source IP; in gateway
    /// mode it also keys the policy rule.
    pub source: Option<String>,
    /// Optional gateway-mode next-hop. When set (requires `source` +
    /// `interface`) the edge programs a `from <source>` policy route via
    /// the gateway before the socket binds, so this tunnel egresses out a
    /// specific uplink on a shared NIC.
    pub gateway: Option<String>,
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

/// Bind a UDP socket of the same address family as `remote` and `connect`
/// it so it only sends to / receives from the relay/peer.
///
/// Optionally NIC-pinned exactly like a bonded UDP leg: `interface` applies
/// `setsockopt(SO_BINDTODEVICE)` BEFORE bind; `source` binds the egress
/// source IP (port 0). With both `None` this is byte-for-byte the previous
/// ephemeral `0.0.0.0:0` / `[::]:0` behaviour. Construction mirrors
/// `util::socket::create_udp_output`.
async fn connect_socket(
    remote: SocketAddr,
    interface: Option<&str>,
    source: Option<&str>,
) -> Result<Arc<UdpSocket>> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};

    let domain = if remote.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;
    socket.set_reuse_address(true)?;

    // NIC pin must precede bind. Prefer SO_BINDTODEVICE, fall back to the
    // unprivileged IP_UNICAST_IF hint when the edge lacks CAP_NET_RAW (the
    // normal case) — identical to how a bonded UDP leg pins its uplink.
    if let Some(name) = interface {
        crate::util::socket::apply_nic_pin(&socket, name, remote.is_ipv6())?;
    }

    // Bind the source IP (ephemeral port) when set, else the family wildcard.
    let bind: SocketAddr = match source {
        Some(src) => {
            let net = crate::engine::bond_routing::SourceNet::parse(src)?;
            SocketAddr::new(net.addr, 0)
        }
        None => {
            if remote.is_ipv6() {
                "[::]:0".parse().unwrap()
            } else {
                "0.0.0.0:0".parse().unwrap()
            }
        }
    };
    socket.bind(&SockAddr::from(bind))?;
    // UDP connect performs no handshake — returns immediately on a
    // non-blocking socket — so it just latches the default peer.
    socket.connect(&SockAddr::from(remote))?;

    let std_sock: std::net::UdpSocket = socket.into();
    let sock = UdpSocket::from_std(std_sock)?;
    Ok(Arc::new(sock))
}

/// Resolve a relay/peer `host:port` to all candidate socket addresses,
/// ordered **IPv4 first**.
///
/// `tokio::net::lookup_host` (getaddrinfo) returns the AAAA record before the A
/// record on a dual-stack host (RFC 6724 source-address ordering). But our
/// relay binds its UDP rendezvous on IPv4 (`0.0.0.0`) and a bond leg pinned to
/// an IPv4-only uplink (a cellular modem, say) has *no* IPv6 route at all — so
/// dialing the AAAA first is either unreachable (`ENETUNREACH` at `connect`) or
/// silently unanswered. Ordering IPv4 first and letting the caller try each
/// candidate in turn fixes both; an IPv6-only relay (no A record) still works
/// because IPv6 is then the only candidate.
async fn resolve_candidates(addr: &str) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = match lookup_host(addr).await {
        Ok(it) => it.collect(),
        Err(_) => Vec::new(),
    };
    // Stable sort: IPv4 (`is_ipv6() == false`) sorts before IPv6 (`true`).
    addrs.sort_by_key(|a| a.is_ipv6());
    addrs
}

/// Connect to the first reachable candidate, trying each address family in
/// turn. `family_offset` rotates the starting candidate (driven by the caller's
/// relay-rotation counter) so a leg that keeps failing on the preferred family
/// eventually attempts the other one. Returns the connected socket and the
/// address it latched, or `None` if every candidate failed to bind/connect.
async fn connect_any(
    candidates: &[SocketAddr],
    family_offset: usize,
    interface: Option<&str>,
    source: Option<&str>,
    tunnel_id: &Uuid,
) -> Option<(SocketAddr, Arc<UdpSocket>)> {
    let n = candidates.len();
    if n == 0 {
        return None;
    }
    for k in 0..n {
        let cand = candidates[family_offset.wrapping_add(k) % n];
        match connect_socket(cand, interface, source).await {
            Ok(sock) => return Some((cand, sock)),
            Err(e) => tracing::warn!(
                %tunnel_id,
                "native-UDP relay: bind/connect to {cand} failed: {e} — trying next address"
            ),
        }
    }
    None
}

/// Best-effort gateway-mode policy-route programming for a native-UDP
/// tunnel. Mirrors the bonded-output path: ensure the source address is on
/// the NIC, install `default via <gateway>` in a private table, and a
/// `from <source>` rule. The tunnel socket binds `source` so its packets
/// match the rule. Returns `true` if a route was programmed (caller must
/// tear it down on exit). On error logs a warning and returns `false` — the
/// host may already have policy routing, so we never hard-fail the tunnel.
///
/// `path_id` is always `0` for a tunnel (one leg per tunnel); the
/// `(tunnel_id, 0)` key namespaces it within `BondRouteManager`.
async fn program_tunnel_gateway(
    tunnel_id: &Uuid,
    interface: &str,
    source: &str,
    gateway: &str,
) -> bool {
    let gw_ip: std::net::IpAddr = match gateway.parse() {
        Ok(ip) => ip,
        Err(_) => return false, // validation already rejected this
    };
    let src_net = match crate::engine::bond_routing::SourceNet::parse(source) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let mgr = match crate::engine::bond_routing::BondRouteManager::global().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                %tunnel_id,
                "native-UDP tunnel: gateway routing unavailable ({e}) — continuing without \
                 policy route (host may already route {source} via {gateway})"
            );
            return false;
        }
    };
    if let Err(e) = mgr
        .program(&tunnel_id.to_string(), 0u8, interface, src_net, gw_ip)
        .await
    {
        tracing::warn!(
            %tunnel_id,
            "native-UDP tunnel: failed to program gateway route ({source} via {gateway} dev \
             {interface}): {e} — continuing (host may already route it)"
        );
        return false;
    }
    tracing::info!(
        %tunnel_id,
        "native-UDP tunnel: programmed gateway route {source} via {gateway} dev {interface}"
    );
    true
}

/// Tear down a tunnel's gateway-mode policy route. Best-effort, idempotent.
async fn teardown_tunnel_gateway(tunnel_id: &Uuid) {
    if let Ok(mgr) = crate::engine::bond_routing::BondRouteManager::global().await {
        mgr.teardown(&tunnel_id.to_string(), 0u8).await;
    }
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

    // Gateway-mode policy routing (best-effort) BEFORE any socket binds the
    // source IP. Tracked so we tear it down on every exit path below.
    let gateway_programmed = match (
        params.gateway.as_deref(),
        params.source.as_deref(),
        params.interface.as_deref(),
    ) {
        (Some(gw), Some(src), Some(iface)) => {
            program_tunnel_gateway(&params.tunnel_id, iface, src, gw).await
        }
        _ => false,
    };

    let mut idx = 0usize;
    let outcome: Result<()> = 'outer: loop {
        if cancel.is_cancelled() {
            break 'outer Ok(());
        }
        let relay_addr = params.relay_addrs[idx % params.relay_addrs.len()].clone();
        let _ = active_idx_tx.send(idx % params.relay_addrs.len());
        state_tx.send_replace(RelayTunnelState::Connecting);

        // Resolve all candidate addresses (IPv4 first) and connect to the first
        // reachable one, falling back across address families.
        let candidates = resolve_candidates(&relay_addr).await;
        if candidates.is_empty() {
            tracing::warn!(tunnel_id = %params.tunnel_id, "native-UDP relay: cannot resolve '{relay_addr}'");
            rotate_after(&cancel, &mut idx).await;
            continue;
        }
        let (resolved, sock) = match connect_any(
            &candidates,
            idx,
            params.interface.as_deref(),
            params.source.as_deref(),
            &params.tunnel_id,
        )
        .await
        {
            Some(pair) => pair,
            None => {
                tracing::warn!(tunnel_id = %params.tunnel_id, "native-UDP relay: all addresses for '{relay_addr}' failed to connect");
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
            break 'outer Ok(());
        }
        if let Err(e) = fwd {
            tracing::debug!(tunnel_id = %params.tunnel_id, "native-UDP forwarder exited: {e}");
        }
        rotate_after(&cancel, &mut idx).await;
    };

    if gateway_programmed {
        teardown_tunnel_gateway(&params.tunnel_id).await;
    }
    outcome
}

/// Advance the relay index and sleep the retry delay (cancel-aware).
async fn rotate_after(cancel: &CancellationToken, idx: &mut usize) {
    *idx = idx.wrapping_add(1);
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(RETRY_DELAY) => {}
    }
}

/// Run a native plain-UDP **relay bond leg** entirely in-process: connect
/// outbound to the relay, Register/keepalive, and bridge the bond's framed
/// datagrams ↔ the relay socket over `from_bond` / `to_bond` channels — with
/// NO `127.0.0.1` loopback hop and NO `TunnelManager` loopback tunnel.
///
/// This shares the relay-socket lifecycle (resolve → connect → Register /
/// keepalive → liveness-watchdog → failover rotation → gateway routing) with
/// [`run_native_relay_tunnel`]; only the forwarder differs. Instead of
/// `udp_forwarder::run_egress`/`run_ingress` (which own a loopback UDP socket),
/// the in-process pump:
///   - drains `from_bond` (datagrams the bond scheduler emitted on this leg),
///     conditionally tunnel-AEAD-encrypts, prepends the 16-byte `tunnel_id`
///     ([`protocol::encode_udp_datagram`]) and writes the relay socket —
///     **byte-identical** to `run_egress`, so `bilbycast-relay` is unchanged;
///   - reads the relay socket ([`PlainUdpLink::recv_datagram`], which peels
///     control acks), decodes the prefix, conditionally decrypts, and
///     `to_bond.try_send`s the inner datagram to the bond receiver.
///
/// Both directions are pumped regardless of `direction` (a bonded leg carries
/// media one way and the NACK/keepalive back-channel the other); `direction`
/// only sets the `Register` direction so the relay pairs the two halves.
///
/// **Conditional AEAD** (`cipher`): `Some` ONLY when the bond is UNKEYED (the
/// leg's `tunnel_encryption_key` is the single layer the bridge applies);
/// `None` when the bond's own `0xBD` key is the single layer (the attached path
/// seals/opens it). This decision MUST match the peer edge's leg — a mismatch
/// is a total blackout, surfaced via `decrypt_errors` (the key-skew watchdog).
///
/// **Relay rotation**: the bond leg channels (`from_bond` / `to_bond`) PERSIST
/// across a relay failover — only the socket + Register task are rebuilt; the
/// bond leg is never torn down for a relay rotation. Returns only on
/// cancellation or when the bond drops its end (leg removed).
#[allow(clippy::too_many_arguments)]
pub async fn run_native_relay_leg_inproc(
    params: NativeRelayParams,
    cancel: CancellationToken,
    stats: Arc<UdpForwarderStats>,
    cipher: Option<Arc<TunnelCipher>>,
    to_bond: mpsc::Sender<Bytes>,
    mut from_bond: mpsc::Receiver<Bytes>,
) -> Result<()> {
    if params.relay_addrs.is_empty() {
        anyhow::bail!("native-UDP relay bond leg requires at least one relay address");
    }

    let bind_token = params.tunnel_bind_secret.as_deref().map(|secret| {
        super::auth::compute_bind_token(
            &params.tunnel_id.to_string(),
            direction_str(params.direction),
            secret,
        )
    });

    // Gateway-mode policy routing (best-effort) BEFORE any socket binds the
    // source IP. Tracked so it's torn down on every exit path below. Unlike a
    // UDP leg (sender-only gateway), BOTH ends of a relay leg dial out, so
    // gateway mode is valid on either direction.
    let gateway_programmed = match (
        params.gateway.as_deref(),
        params.source.as_deref(),
        params.interface.as_deref(),
    ) {
        (Some(gw), Some(src), Some(iface)) => {
            program_tunnel_gateway(&params.tunnel_id, iface, src, gw).await
        }
        _ => false,
    };

    let mut idx = 0usize;
    let outcome: Result<()> = 'outer: loop {
        if cancel.is_cancelled() {
            break 'outer Ok(());
        }
        let relay_addr = params.relay_addrs[idx % params.relay_addrs.len()].clone();

        let candidates = resolve_candidates(&relay_addr).await;
        if candidates.is_empty() {
            tracing::warn!(tunnel_id = %params.tunnel_id, "native-UDP relay bond leg: cannot resolve '{relay_addr}'");
            rotate_after(&cancel, &mut idx).await;
            continue;
        }
        let (resolved, sock) = match connect_any(
            &candidates,
            idx,
            params.interface.as_deref(),
            params.source.as_deref(),
            &params.tunnel_id,
        )
        .await
        {
            Some(pair) => pair,
            None => {
                tracing::warn!(tunnel_id = %params.tunnel_id, "native-UDP relay bond leg: all addresses for '{relay_addr}' failed to connect");
                rotate_after(&cancel, &mut idx).await;
                continue;
            }
        };

        tracing::info!(
            tunnel_id = %params.tunnel_id,
            relay = %resolved,
            direction = %params.direction,
            encrypted = cipher.is_some(),
            "native-UDP relay bond leg connecting"
        );

        let link = PlainUdpLink::new(sock.clone());
        let last_ack = link.last_ack_ms.clone();
        // Seed liveness to "now" — PlainUdpLink::new leaves last_ack at 0, which
        // the watchdog would read as silent-forever and rotate instantly.
        last_ack.store(udp_forwarder::now_epoch_ms(), Ordering::Relaxed);

        let attempt_cancel = cancel.child_token();

        // Register / keepalive + liveness watchdog (mirrors run_native_relay_tunnel,
        // minus the RelayTunnelState watch — bond leg health is the bond's own job).
        {
            let reg_sock = sock.clone();
            let reg_cancel = attempt_cancel.clone();
            let tunnel_id = params.tunnel_id;
            let direction = relay_direction(params.direction);
            let bind_token = bind_token.clone();
            let last_ack = last_ack.clone();
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
                let _ = reg_sock.send(&register).await;
                let mut tick = tokio::time::interval(KEEPALIVE_INTERVAL);
                tick.tick().await; // consume the immediate first tick
                loop {
                    tokio::select! {
                        _ = reg_cancel.cancelled() => return,
                        _ = tick.tick() => {
                            let _ = reg_sock.send(&register).await;
                            let since = udp_forwarder::now_epoch_ms()
                                .saturating_sub(last_ack.load(Ordering::Relaxed));
                            if since > RELAY_DEAD_TIMEOUT_MS {
                                tracing::warn!(%tunnel_id, "native-UDP relay bond leg silent for {since} ms — rotating");
                                reg_cancel.cancel();
                                return;
                            }
                        }
                    }
                }
            });
        }

        // In-process pump until the attempt is cancelled (relay dead / outer
        // shutdown), the link errors, or the bond drops its end (leg removed).
        loop {
            tokio::select! {
                _ = attempt_cancel.cancelled() => break,
                // Bond → relay. `from_bond` yields an already-bond-framed datagram
                // (0xBD-sealed when the bond is keyed, else plaintext 0xBC). The
                // `await` here is the DRAIN side, not a backpressure injection on
                // the bond sender (the bond `try_send`'d into the path channel and
                // drops on full), so it is safe — never stalls the bond.
                maybe = from_bond.recv() => {
                    let Some(frame) = maybe else {
                        // Bond dropped its sender → this leg is gone for good.
                        break 'outer Ok(());
                    };
                    let payload = if let Some(c) = &cipher {
                        match c.encrypt(frame.as_ref()) {
                            Ok(p) => p,
                            Err(e) => {
                                stats.send_errors.fetch_add(1, Ordering::Relaxed);
                                tracing::debug!(tunnel_id = %params.tunnel_id, "relay bond leg encrypt error: {e}");
                                continue;
                            }
                        }
                    } else {
                        frame.to_vec()
                    };
                    // Byte-identical framing to udp_forwarder::run_egress so the
                    // relay forwards it verbatim with zero changes.
                    let datagram = protocol::encode_udp_datagram(&params.tunnel_id, &payload);
                    match link.send_datagram(Bytes::from(datagram)) {
                        Ok(()) => {
                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                            stats.bytes_sent.fetch_add(frame.len() as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            stats.send_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::debug!(tunnel_id = %params.tunnel_id, "relay bond leg send error: {e}");
                        }
                    }
                }
                // Relay → bond.
                res = link.recv_datagram() => {
                    let datagram = match res {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::debug!(tunnel_id = %params.tunnel_id, "relay bond leg recv error: {e}");
                            break;
                        }
                    };
                    if let Some((_tid, enc)) = protocol::decode_udp_datagram(&datagram) {
                        let payload = if let Some(c) = &cipher {
                            match c.decrypt(enc) {
                                Ok(p) => p,
                                Err(_) => {
                                    stats.decrypt_errors.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                            }
                        } else {
                            enc.to_vec()
                        };
                        stats.packets_received.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_received.fetch_add(payload.len() as u64, Ordering::Relaxed);
                        // Drop-on-full — the bond owns recovery (cross-leg ARQ/FEC).
                        // NEVER await: the attached-path recv loop drains this and
                        // a full channel is leg loss like any other.
                        let _ = to_bond.try_send(Bytes::from(payload));
                    }
                }
            }
        }

        if cancel.is_cancelled() {
            break 'outer Ok(());
        }
        rotate_after(&cancel, &mut idx).await;
    };

    if gateway_programmed {
        teardown_tunnel_gateway(&params.tunnel_id).await;
    }
    outcome
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
    interface: Option<String>,
    source: Option<String>,
    gateway: Option<String>,
    tunnel_psk: String,
    state_tx: watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    stats: Arc<UdpForwarderStats>,
    cipher: Option<Arc<TunnelCipher>>,
) -> Result<()> {
    match direction {
        TunnelDirection::Egress => {
            // Gateway-mode policy routing (best-effort) before the source bind.
            // Tracked so it's torn down on every exit path of the egress run.
            let gateway_programmed = match (
                gateway.as_deref(),
                source.as_deref(),
                interface.as_deref(),
            ) {
                (Some(gw), Some(src), Some(iface)) => {
                    program_tunnel_gateway(&tunnel_id, iface, src, gw).await
                }
                _ => false,
            };

            let result: Result<()> = async {
                let peer = peer_addr.ok_or_else(|| anyhow::anyhow!("peer_addr required for direct egress"))?;
                let candidates = resolve_candidates(&peer).await;
                let (_resolved, sock) = connect_any(
                    &candidates,
                    0,
                    interface.as_deref(),
                    source.as_deref(),
                    &tunnel_id,
                )
                .await
                .ok_or_else(|| anyhow::anyhow!("cannot resolve/connect peer '{peer}'"))?;
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
            .await;

            if gateway_programmed {
                teardown_tunnel_gateway(&tunnel_id).await;
            }
            result
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
                        Some(c) => match c.decrypt(enc) {
                            Ok(p) => p,
                            Err(_) => {
                                stats.decrypt_errors.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        },
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
