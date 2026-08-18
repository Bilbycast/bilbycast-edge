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

use std::net::{IpAddr, SocketAddr};
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
use crate::manager::events::{category, EventSender, EventSeverity};

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

/// A direct-mode listener's latched caller may only move to a DIFFERENT source
/// IP once the tunnel has been silent this long.
///
/// This is the edge half of the relay's slot hold-down
/// (`bilbycast-relay::udp_relay::SLOT_TAKEOVER_GRACE_MS`) and deliberately
/// carries the SAME value: the two ends run the same rendezvous protocol with
/// the same `KEEPALIVE_INTERVAL` (5 s), so 12 s is two missed keepalives on
/// both. A flowing session cannot be stolen, while a genuine WAN-IP change
/// (carrier handover, DHCP move) recovers within one relay-dead window.
/// Invariant: `KEEPALIVE_INTERVAL` (5 s) < this < `RELAY_DEAD_TIMEOUT_MS` (25 s).
///
/// **It protects an ENCRYPTED direct tunnel only.** Only media the AEAD has
/// actually opened arms it — [`SlotHoldDown::note_media`] is called under
/// `cipher.is_some()`. Encryption is optional on this path
/// (`manager::start_native_direct_tunnel`: "Direct mode: encryption optional"),
/// and with no key configured framing alone is no evidence whatever: the tunnel
/// id travels in the cleartext register token, so any datagram beginning with
/// those 16 bytes would arm the hold-down and hand whoever won the idle-slot
/// race a lock for free. An unencrypted tunnel can offer no proof, so it gets
/// no protection and keeps the previous last-writer-wins behaviour.
///
/// **Cost on a genuine carrier handover.** The caller's connected socket errors
/// when its IP changes, `run_native_direct_tunnel` returns `Err`, and the
/// manager respawns with backoff — by which time the grace has usually elapsed.
/// Worst case this adds ≤ 12 s to a handover blackout, ≤ 17 s counting the next
/// keepalive.
const SLOT_TAKEOVER_GRACE_MS: u64 = 12_000;

/// A slot must not be held against a persistently-present challenger forever.
///
/// Without this ceiling the hold-down turns a ≤ 5 s hijack into a PERMANENT
/// one. An idle slot is deliberately last-writer-wins (`last_media_ms == 0`),
/// so a peer holding the replayable token registers first while the tunnel is
/// idle, is acked, sends its own media, and arms the stamp — and from then on
/// every `Register` from the real caller's address is refused while its media is
/// dropped by the accept gate. Nothing escapes that state on its own: the
/// listener loops until cancel or a socket error, the manager's respawn loop
/// only re-enters on `Err`, and the squatter's traffic keeps
/// `packets_received` climbing so a traffic-based watchdog reads the tunnel as
/// healthy.
///
/// So a challenger that keeps presenting a valid token from one IP for this
/// long takes the slot. Damage is then bounded in BOTH directions — a hijack
/// costs the real caller at most this long, a hold costs a challenger at most
/// this long — and for a live contribution feed that beats an unbounded,
/// unrecoverable blackout in one direction. It sits well above
/// [`SLOT_TAKEOVER_GRACE_MS`] so an ordinary quiet period never reaches it.
const SLOT_CONTESTED_CEILING_MS: u64 = 60_000;

/// Monotonic milliseconds, never zero.
///
/// The hold-down MUST NOT use wall time: an NTP/PTP step (routine on a
/// broadcast host — `chrony` settling, `phc2sys` starting) would either expire
/// every hold-down at once or freeze them for the length of the step. This is
/// also deliberately NOT `util::time::now_us()`, which returns 0 until
/// `init_epoch()` has run and would then read as "media arrived at time 0".
///
/// The `+ 1` mirrors the relay: 0 is the "has never carried media" sentinel, so
/// a datagram forwarded in the first millisecond must not stamp the slot with a
/// value that reads as unprotected.
fn mono_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Is a latched caller protected from being moved to a different source IP?
///
/// Pure so the boundary is unit-testable without a 12 s sleep or a clock seam.
/// `last_media_ms == 0` means the tunnel has never carried media, and such a
/// slot is deliberately unprotected — see [`register_may_latch`].
fn slot_is_protected(last_media_ms: u64, now_ms: u64) -> bool {
    last_media_ms != 0 && now_ms.saturating_sub(last_media_ms) < SLOT_TAKEOVER_GRACE_MS
}

/// Is `src` an address a real peer could have reached us from, and that we can
/// therefore safely latch as this tunnel's caller?
///
/// Ported from the relay's `udp_relay::is_latchable_source`, and for exactly its
/// reason: **the latch turns a source address into a *send* target.** Every byte
/// the local application returns is sent there (`public.send_to(&datagram,
/// dest)` on the loopback-return arm), so the slot must never hold an address no
/// host owns. Source port 0 is legal UDP, is not filtered as a martian, and
/// `recv_from` reports it verbatim — one spoofed `Register` from `ip:0` would
/// latch it and turn the entire return path (SRT handshake / ACK / NAK) into a
/// stream of `EINVAL`s swallowed by `let _ =`. Unspecified, multicast and
/// broadcast are worse still: a black hole, or a free fan-out of somebody
/// else's contribution feed onto a segment.
///
/// Deliberately permissive about **loopback and private ranges**: the testbed
/// and these tests bind `127.0.0.1` / `127.0.0.2`, and real deployments run
/// direct tunnels between RFC1918 / ULA addresses inside an operator network.
/// Rejecting either would break working configurations, which is a worse bug
/// than the one this closes.
fn is_latchable_source(src: SocketAddr) -> bool {
    if src.port() == 0 {
        return false;
    }
    match src.ip() {
        IpAddr::V4(v4) => !(v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast()),
        IpAddr::V6(v6) => !(v6.is_unspecified() || v6.is_multicast()),
    }
}

/// May a `Register` whose PSK token has already verified move the direct-mode
/// listener's latched caller to `from`?
///
/// The listener's caller latch was unconditional last-writer-wins, and the PSK
/// token it trusts is a *static* `HMAC(tunnel_id, psk)` — no nonce, no expiry —
/// sent in the clear in every 5-second keepalive. Anyone who observes one
/// Register can therefore replay it forever from any address and take the
/// tunnel: the real caller's media is then dropped (it no longer matches the
/// latch) and every byte the listener returns — SRT handshake/ACK/NAK, or media
/// on a reversed leg — is delivered to the replayer instead. This is the same
/// hijack the relay closed on its side; the two ends now agree in spirit.
///
/// Rules, in the order they matter:
/// - **Same IP always moves.** A NAT port rebind, a caller restart on the same
///   host and a re-provisioned socket all keep the IP, and refusing them would
///   drop real sessions.
/// - **A slot that has never carried media stays last-writer-wins**, so first
///   contact is byte-identical to the previous behaviour, and pre-claiming an
///   idle tunnel buys an attacker no protection.
/// - **A different IP may take over once the incumbent has gone quiet** for
///   [`SLOT_TAKEOVER_GRACE_MS`], which is what a genuine WAN-IP change looks
///   like (the media stops the moment the address changes).
///
/// Only *media* refreshes `last_media_ms`, never a `Register` — protection is
/// earned by carrying traffic, exactly as on the relay.
///
/// **Residual risk: same-IP is trusted unconditionally, and that is not merely
/// a NAT-rebind accommodation.** Behind CGNAT, a shared corporate NAT or a
/// shared datacenter the attacker *presents the caller's IP*, so it can move the
/// latch at will and this predicate never fires. Combined with the hold-down it
/// could then keep the slot for a full [`SLOT_CONTESTED_CEILING_MS`] at a time.
/// The relay makes the same trade for the same reason (refusing a same-IP move
/// drops real sessions), so it is inherited rather than introduced — but it is
/// the reason this hold-down is a mitigation, not a fix. The fix is a
/// non-replayable register token, which is a wire change both edges must take
/// together.
fn register_may_latch(
    current: Option<SocketAddr>,
    from: SocketAddr,
    last_media_ms: u64,
    now_ms: u64,
) -> bool {
    match current {
        Some(cur) if cur.ip() != from.ip() => !slot_is_protected(last_media_ms, now_ms),
        _ => true,
    }
}

/// Why a `Register` whose PSK token already verified was nonetheless refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusedLatch {
    /// The source address is not one a real peer could own — see
    /// [`is_latchable_source`].
    UnlatchableSource,
    /// The tunnel is carrying media from a different IP and the hold-down has
    /// not yet reached [`SLOT_CONTESTED_CEILING_MS`].
    SlotHeld,
}

impl RefusedLatch {
    /// Stable machine-readable discriminator for the manager event.
    fn code(self) -> &'static str {
        match self {
            Self::UnlatchableSource => "unlatchable_source",
            Self::SlotHeld => "slot_held",
        }
    }

    /// Operator-facing explanation, shared by the log line and the event.
    fn explain(self) -> &'static str {
        match self {
            Self::UnlatchableSource => {
                "its source address cannot belong to a real peer (port 0, unspecified, \
                 multicast or broadcast), and latching it would send the tunnel's return \
                 traffic nowhere"
            }
            Self::SlotHeld => {
                "the tunnel is carrying media from a different address; a replayed PSK \
                 token cannot move a live peer"
            }
        }
    }
}

/// What to do with a `Register` whose PSK token has already verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LatchDecision {
    /// Move (or set) the caller latch to this address.
    Accept,
    /// Refuse, without acking. `report` is `Some(suppressed_since_last)` at most
    /// once per [`SLOT_TAKEOVER_GRACE_MS`] — see [`SlotHoldDown::throttle`].
    Refuse {
        reason: RefusedLatch,
        report: Option<u64>,
    },
}

/// A challenger currently being refused: which IP, when it first was, and when
/// it last was.
///
/// `last_ms` is what makes [`SLOT_CONTESTED_CEILING_MS`] a *persistence budget*
/// rather than a wall-clock alarm. Without it, one `Register` now and one a
/// minute later would take a live, flowing tunnel on two packets and no
/// sustained presence — the ceiling's whole cost to an attacker is that it has
/// to keep showing up.
#[derive(Debug, Clone, Copy)]
struct Contest {
    ip: IpAddr,
    first_ms: u64,
    last_ms: u64,
}

/// Caller-latch hold-down state for one direct-mode listener.
///
/// Owned by the listener task and touched only from its `public` `select!` arm,
/// so a plain value is correct and free — no lock, no atomic, no allocation, and
/// nothing new awaited on the media path.
struct SlotHoldDown {
    grace_ms: u64,
    ceiling_ms: u64,
    /// Monotonic ms at which the latched caller last delivered media that was
    /// both accepted and authenticated. `0` = never.
    last_media_ms: u64,
    /// The challenger being refused, so one that never gives up reaches
    /// `ceiling_ms`. Cleared when that same challenger is finally let in, so the
    /// peer it displaced must itself contend for a full ceiling to get the slot
    /// back and neither side can be locked out.
    contender: Option<Contest>,
    /// Monotonic ms of the last refusal reported (`0` = none) and how many
    /// refusals have been suppressed since.
    last_report_ms: u64,
    suppressed: u64,
}

impl SlotHoldDown {
    fn new(grace_ms: u64, ceiling_ms: u64) -> Self {
        Self {
            grace_ms,
            ceiling_ms,
            last_media_ms: 0,
            contender: None,
            last_report_ms: 0,
            suppressed: 0,
        }
    }

    /// The latched caller delivered media that is genuinely ours. ONLY this arms
    /// the hold-down — never a `Register` — so protection is earned by carrying
    /// traffic and pre-claiming an idle tunnel buys nothing.
    fn note_media(&mut self, now_ms: u64) {
        self.last_media_ms = now_ms;
    }

    /// At most one refusal report per `grace_ms`, counting the rest.
    ///
    /// Deliberately ONE global slot rather than a per-challenger-IP table: a
    /// refusal is driven entirely by an attacker, who picks both the rate and
    /// the source address, so a per-IP table lets a spoofed-source flood emit
    /// one interpolated `WARN` and one manager event *per packet* on a broadcast
    /// host writing to journald. Before the hold-down existed, an accepted
    /// register logged nothing at all, so this path is pure new attacker-driven
    /// volume and has to be bounded rather than merely keyed. A genuine second
    /// challenger loses at most one grace window of warning latency, and the
    /// suppressed count says how much was hidden.
    fn throttle(&mut self, now_ms: u64) -> Option<u64> {
        if self.last_report_ms != 0 && now_ms.saturating_sub(self.last_report_ms) < self.grace_ms {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.last_report_ms = now_ms;
        Some(std::mem::take(&mut self.suppressed))
    }

    /// Accept `from` onto the latch, clearing the contest **only when `from` is
    /// the challenger that was contending**.
    ///
    /// Clearing on every accept looks tidier and is wrong: the incumbent
    /// re-sends its own `Register` on `KEEPALIVE_INTERVAL` (5 s) for the life of
    /// the tunnel — `run_native_direct_tunnel`'s egress arm spawns a task that
    /// does nothing else — and those are same-IP, so they are accepted. Letting
    /// one reset an unrelated challenger's clock means a squatter resets the real
    /// caller's ceiling twelve times per ceiling by behaving completely normally,
    /// the ceiling is never reached, and the permanent unrecoverable blackout
    /// [`SLOT_CONTESTED_CEILING_MS`] exists to bound is back in full.
    fn accept(&mut self, from: SocketAddr) -> LatchDecision {
        if self.contender.is_some_and(|c| c.ip == from.ip()) {
            self.contender = None;
        }
        LatchDecision::Accept
    }

    /// Offer a `Register` whose PSK token has already verified.
    fn offer(
        &mut self,
        current: Option<SocketAddr>,
        from: SocketAddr,
        now_ms: u64,
    ) -> LatchDecision {
        if !is_latchable_source(from) {
            // Such an address is unlatchable at every point in time, so this
            // deliberately does NOT feed the contested ceiling — otherwise a
            // spoofer registering from `ip:0` for a minute would be handed the
            // tunnel's return path, which is the very outcome the check exists
            // to prevent.
            return LatchDecision::Refuse {
                reason: RefusedLatch::UnlatchableSource,
                report: self.throttle(now_ms),
            };
        }
        if register_may_latch(current, from, self.last_media_ms, now_ms) {
            return self.accept(from);
        }
        // A contest continues only while the challenger keeps showing up. A gap
        // longer than the grace (well over two keepalives) means it stopped, so
        // it starts again from zero — the ceiling is a budget of sustained
        // presence, not an alarm clock started by the first packet ever seen.
        let since = match self.contender {
            Some(c) if c.ip == from.ip() && now_ms.saturating_sub(c.last_ms) <= self.grace_ms => {
                self.contender = Some(Contest {
                    last_ms: now_ms,
                    ..c
                });
                c.first_ms
            }
            _ => {
                self.contender = Some(Contest {
                    ip: from.ip(),
                    first_ms: now_ms,
                    last_ms: now_ms,
                });
                now_ms
            }
        };
        if now_ms.saturating_sub(since) >= self.ceiling_ms {
            // Ceiling reached: the incumbent has held the slot against a
            // continuously-present peer for a minute. Yield rather than leave
            // the tunnel unrecoverable — see [`SLOT_CONTESTED_CEILING_MS`].
            return self.accept(from);
        }
        LatchDecision::Refuse {
            reason: RefusedLatch::SlotHeld,
            report: self.throttle(now_ms),
        }
    }
}

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
                    let Some((datagram_id, enc)) = protocol::decode_udp_datagram(&datagram) else { continue };
                    // Same refusal as `udp_forwarder::run_{egress,ingress}`, same
                    // safety argument: this leg is one tunnel with one id at both
                    // ends (the register token binds it), and the relay forwards
                    // the framing verbatim while routing on that prefix alone with
                    // no per-datagram bind auth — so the receiving edge is the only
                    // place a re-addressed datagram can be refused.
                    if datagram_id != params.tunnel_id {
                        stats.framing_errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
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
    event_sender: EventSender,
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
                tunnel_id,
                listen_sa,
                local_addr,
                tunnel_psk,
                state_tx,
                cancel,
                stats,
                cipher,
                event_sender,
                SlotHoldDown::new(SLOT_TAKEOVER_GRACE_MS, SLOT_CONTESTED_CEILING_MS),
            )
            .await
        }
    }
}

/// Direct-mode listener: bind the public UDP port, authenticate the caller's
/// register (PSK), latch its source address, and bridge tunnel ↔ loopback.
///
/// `hold_down` carries the caller-latch policy. Production passes
/// [`SlotHoldDown::new`] with [`SLOT_TAKEOVER_GRACE_MS`] /
/// [`SLOT_CONTESTED_CEILING_MS`]; it is a parameter rather than a hard-coded
/// constant so the wire tests can exercise the real listener against a
/// sub-second ceiling instead of asserting on the helper in isolation — the
/// distinction that matters, because a helper test passes whether or not the
/// listener consults the helper.
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
    event_sender: EventSender,
    mut hold_down: SlotHoldDown,
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
                    if !ok {
                        tracing::warn!(%tunnel_id, "native-UDP direct: rejected register from {from} (bad PSK)");
                        continue;
                    }
                    // Hold-down: a replayed token must not move a live tunnel to
                    // a different source IP, and no token may latch an address a
                    // real peer cannot own. See `SlotHoldDown::offer`.
                    let held = caller.load();
                    if let LatchDecision::Refuse { reason, report } =
                        hold_down.offer(held, from, mono_ms())
                    {
                        // Deliberately no Ack — the sender is either replaying a
                        // harvested token or is a stale duplicate caller, and an
                        // Ack would confirm to an attacker that the tunnel is
                        // real. Mirrors the relay's `RejectedSlotHeld` path.
                        //
                        // Reported at most once per grace window (see
                        // `SlotHoldDown::throttle`): the refusal is entirely
                        // attacker-driven, so it must not become a log or event
                        // amplifier on a broadcast host.
                        if let Some(suppressed) = report {
                            tracing::warn!(
                                %tunnel_id,
                                held = ?held,
                                suppressed,
                                "native-UDP direct: refused register from {from} — {}",
                                reason.explain()
                            );
                            // The refusal is otherwise invisible: the tunnel
                            // state stays `Ready` and `packets_received` keeps
                            // climbing on the incumbent's traffic, so nothing on
                            // any manager surface would show a caller being
                            // locked out.
                            event_sender.emit_with_details(
                                EventSeverity::Warning,
                                category::TUNNEL,
                                format!(
                                    "Direct tunnel listener refused a register from {from}: {}",
                                    reason.explain()
                                ),
                                None,
                                serde_json::json!({
                                    "error_code": "tunnel_register_refused",
                                    "tunnel_id": tunnel_id.to_string(),
                                    "reason": reason.code(),
                                    "refused_addr": from.to_string(),
                                    "held_addr": held.map(|a| a.to_string()),
                                    "suppressed": suppressed,
                                }),
                            );
                        }
                        continue;
                    }
                    caller.store(from);
                    state_tx.send_replace(RelayTunnelState::Ready);
                    // Ack so the caller confirms reachability.
                    if let Ok(ack) = encode_udp_control(&UdpRelayControl::Ack { tunnel_id, ready: true }) {
                        let _ = public.send_to(&ack, from).await;
                    }
                    continue;
                }
                // Media: only accept from the latched caller.
                if caller.load() != Some(from) { continue; }
                let Some((datagram_id, enc)) = protocol::decode_udp_datagram(data) else { continue };
                // This socket serves exactly one tunnel. Be honest about what
                // the check is worth: it is NOT authentication. A CONFORMING
                // peer can never send a foreign prefix — the register handshake
                // already binds the id (`verify_token(..) == tunnel_id` above)
                // and `udp_forwarder::run_egress` frames with that same id — and
                // an attacker that reaches this line already matches the latched
                // caller's exact `SocketAddr`, so it could simply write the
                // correct 16 bytes (the prefix rides OUTSIDE the AEAD; see
                // `crypto.rs`, the AAD is empty).
                //
                // What it actually closes is narrower and real: on an
                // UNENCRYPTED direct tunnel (`tunnel_encryption_key` is optional
                // in direct mode) a mis-framed datagram — or a nil-prefixed one
                // that failed to parse as control and fell through to here — is
                // no longer blind-forwarded into the local application.
                if datagram_id != tunnel_id {
                    stats.framing_errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
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
                // The tunnel has now carried media from the latched caller: arm
                // the hold-down — but ONLY when the AEAD actually opened it.
                //
                // With no key configured, framing is not evidence of anything:
                // the tunnel id travels in the cleartext register token, so any
                // datagram starting with those 16 bytes reaches this line, and
                // arming on it would hand whoever won the idle-slot race a lock
                // on the tunnel for free. Leaving the stamp at 0 restores exactly
                // the pre-hold-down last-writer-wins semantics for that
                // deployment shape. See `SLOT_TAKEOVER_GRACE_MS`.
                if cipher.is_some() {
                    hold_down.note_media(mono_ms());
                }
                let _ = loop_sock.send_to(&payload, forward_addr).await;
                stats.packets_received.fetch_add(1, Ordering::Relaxed);
                stats.bytes_received.fetch_add(payload.len() as u64, Ordering::Relaxed);
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

#[cfg(test)]
mod direct_listener_holddown_tests {
    use super::*;

    fn addr(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse::<std::net::IpAddr>().unwrap(), port)
    }

    fn hold_down() -> SlotHoldDown {
        SlotHoldDown::new(SLOT_TAKEOVER_GRACE_MS, SLOT_CONTESTED_CEILING_MS)
    }

    /// THE HIJACK, at the predicate. The direct-mode listener trusts a *static*
    /// `HMAC(tunnel_id, psk)` token that is re-sent in the clear every 5 s, so
    /// it is replayable forever. A live tunnel's caller must not move to a
    /// different IP on the strength of one replayed `Register`: that both
    /// blackholes the real feed and hands the return traffic to the replayer.
    ///
    /// **This test does not prove the hijack is closed** — it exercises the pure
    /// predicate in isolation and therefore passes whether or not the listener
    /// consults it. `direct_listener_wire_tests::
    /// replayed_register_from_another_ip_cannot_steal_a_live_tunnel` carries
    /// that proof, on real sockets. The tests here pin the boundary arithmetic,
    /// which is the part a 12 s sleep has no business testing.
    #[test]
    fn predicate_refuses_a_different_ip_while_media_flows() {
        let now = 1_000_000u64;
        assert!(
            !register_may_latch(
                Some(addr("198.51.100.10", 5000)),
                addr("203.0.113.7", 5000),
                now,
                now
            ),
            "a replayed token must not steal a tunnel that is carrying media"
        );
    }

    /// A NAT port rebind keeps the IP and must still re-latch — it is normal
    /// and frequent, and refusing it would drop real sessions.
    #[test]
    fn same_ip_port_rebind_still_relatches() {
        let now = 1_000_000u64;
        assert!(register_may_latch(
            Some(addr("198.51.100.10", 5000)),
            addr("198.51.100.10", 41234),
            now,
            now
        ));
    }

    /// First contact, and any tunnel that has never carried media, stay
    /// last-writer-wins — byte-identical to the behaviour before the hold-down,
    /// so re-provisioning a tunnel to a new peer is unaffected and pre-claiming
    /// an idle tunnel buys an attacker no protection.
    #[test]
    fn first_contact_and_idle_slots_are_last_writer_wins() {
        let now = 1_000_000u64;
        assert!(
            register_may_latch(None, addr("203.0.113.7", 5000), 0, now),
            "nothing latched yet"
        );
        assert!(
            register_may_latch(
                Some(addr("198.51.100.10", 5000)),
                addr("203.0.113.7", 5000),
                0,
                now
            ),
            "latched but has never carried media"
        );
    }

    /// A genuine WAN-IP change (carrier handover, DHCP move) must recover once
    /// the incumbent has gone quiet — the media stops the instant the address
    /// changes, so the grace always elapses. Exercised on the pure predicate:
    /// a 12 s sleep has no place in a unit test.
    #[test]
    fn takeover_allowed_once_the_incumbent_goes_quiet() {
        let now = 1_000_000u64;
        assert!(slot_is_protected(now, now), "just carried media");
        assert!(
            slot_is_protected(now - (SLOT_TAKEOVER_GRACE_MS - 1), now),
            "still inside the grace window"
        );
        assert!(
            !slot_is_protected(now - SLOT_TAKEOVER_GRACE_MS, now),
            "grace elapsed — a real carrier handover must be able to take over"
        );
        assert!(
            register_may_latch(
                Some(addr("198.51.100.10", 5000)),
                addr("203.0.113.7", 5000),
                now - SLOT_TAKEOVER_GRACE_MS,
                now
            ),
            "handover must recover, or the tunnel is stranded until it restarts"
        );
        // A stamp in the future (a clock oddity we cannot rule out) saturates
        // the subtraction to 0 and therefore protects the slot. That is the safe
        // direction — it expires as the monotonic clock advances past it, rather
        // than leaving a live tunnel stealable.
        assert!(slot_is_protected(now + 5_000, now));
    }

    /// The grace must sit between the register cadence and the relay-dead
    /// window, or the hold-down either blocks live keepalives or outlives the
    /// attempt it is protecting.
    #[test]
    fn grace_window_is_bounded_by_the_keepalive_and_dead_timeout() {
        assert!(
            SLOT_TAKEOVER_GRACE_MS > KEEPALIVE_INTERVAL.as_millis() as u64,
            "must exceed the register cadence"
        );
        // Both sides are consts, so this is worth catching at compile time
        // rather than on a test run.
        const {
            assert!(
                SLOT_TAKEOVER_GRACE_MS < RELAY_DEAD_TIMEOUT_MS,
                "the hold-down must expire inside the dead-relay window"
            )
        };
    }

    /// `mono_ms` must never return the `0` sentinel, or media forwarded in the
    /// process's first millisecond stamps the tunnel as never-having-carried-any
    /// — the exact hole that let an attacker through on the relay side.
    #[test]
    fn mono_ms_never_returns_the_never_carried_sentinel() {
        assert!(mono_ms() > 0);
        assert!(!slot_is_protected(0, mono_ms()), "0 means never");
        assert!(slot_is_protected(mono_ms(), mono_ms()));
    }

    /// The ceiling must sit clear of the grace, or an ordinary quiet period
    /// would reach it and the hold-down would yield for no reason.
    #[test]
    fn contested_ceiling_is_well_above_the_grace() {
        const {
            assert!(
                SLOT_CONTESTED_CEILING_MS > SLOT_TAKEOVER_GRACE_MS,
                "a slot must be holdable for at least one grace window"
            )
        };
        // And it must exceed the relay-dead window too, so a legitimate peer's
        // own failover has run its course before the ceiling ever fires.
        const {
            assert!(SLOT_CONTESTED_CEILING_MS > RELAY_DEAD_TIMEOUT_MS)
        };
    }

    /// THE OTHER HALF OF THE HIJACK. A hold-down with no ceiling converts a
    /// ≤ 5 s theft into a permanent, unrecoverable one: the attacker wins the
    /// idle-slot race, arms the stamp with its own media, and every later
    /// `Register` from the real caller is refused for the life of the process.
    /// A challenger that keeps presenting a valid token must eventually win.
    #[test]
    fn a_persistent_challenger_takes_a_permanently_held_slot() {
        let mut hd = hold_down();
        let squatter = addr("203.0.113.7", 5000);
        let real = addr("198.51.100.10", 5000);
        let mut now = 1_000_000u64;

        // The squatter wins the idle slot and keeps its media flowing.
        assert_eq!(hd.offer(None, squatter, now), LatchDecision::Accept);
        hd.note_media(now);

        // Every refusal inside the ceiling keeps the slot with the squatter,
        // even though the squatter re-arms the stamp on every tick.
        while now < 1_000_000 + SLOT_CONTESTED_CEILING_MS {
            now += 1_000;
            hd.note_media(now);
            assert!(
                matches!(
                    hd.offer(Some(squatter), real, now),
                    LatchDecision::Refuse { reason: RefusedLatch::SlotHeld, .. }
                ),
                "the hold-down must still be holding at +{}ms",
                now - 1_000_000
            );
        }

        // One tick past the ceiling the real caller gets in, despite the stamp
        // being as fresh as it has ever been.
        now += 1_000;
        hd.note_media(now);
        assert_eq!(
            hd.offer(Some(squatter), real, now),
            LatchDecision::Accept,
            "a slot held against a continuously-present peer for the ceiling must yield"
        );
    }

    /// AND THE INCUMBENT MUST NOT BE ABLE TO RESET THE CLOCK BY BREATHING.
    ///
    /// Every conforming caller re-sends its `Register` on `KEEPALIVE_INTERVAL`
    /// (5 s) forever — `run_native_direct_tunnel`'s egress arm spawns a task
    /// that does exactly that. Those registers are same-IP, so they are
    /// accepted. If an accept cleared an unrelated challenger's contest, the
    /// squatter would reset the real caller's ceiling clock twelve times per
    /// ceiling simply by behaving like a normal peer, the ceiling would never be
    /// reached, and [`SLOT_CONTESTED_CEILING_MS`] would be decorative — the
    /// permanent, unrecoverable blackout it exists to bound would be back.
    #[test]
    fn an_incumbent_keepalive_does_not_reset_a_challengers_ceiling() {
        let mut hd = hold_down();
        let squatter = addr("203.0.113.7", 5000);
        let real = addr("198.51.100.10", 5000);
        let mut now = 1_000_000u64;

        assert_eq!(hd.offer(None, squatter, now), LatchDecision::Accept);
        hd.note_media(now);

        // The squatter keepalives every 5 s exactly as a real caller does, while
        // the real caller contends every second.
        while now < 1_000_000 + SLOT_CONTESTED_CEILING_MS {
            now += 1_000;
            hd.note_media(now);
            if now.is_multiple_of(5_000) {
                assert_eq!(
                    hd.offer(Some(squatter), squatter, now),
                    LatchDecision::Accept,
                    "the incumbent's own keepalive must still be accepted"
                );
            }
            assert!(
                matches!(
                    hd.offer(Some(squatter), real, now),
                    LatchDecision::Refuse { reason: RefusedLatch::SlotHeld, .. }
                ),
                "the hold-down must still be holding at +{}ms",
                now - 1_000_000
            );
        }

        now += 1_000;
        hd.note_media(now);
        assert_eq!(
            hd.offer(Some(squatter), squatter, now),
            LatchDecision::Accept
        );
        assert_eq!(
            hd.offer(Some(squatter), real, now),
            LatchDecision::Accept,
            "the incumbent's keepalives must not hold the ceiling off indefinitely"
        );
    }

    /// The ceiling is a persistence budget, not a wall-clock alarm.
    ///
    /// It must measure how long a challenger has been *continuously* present,
    /// not how long ago it was first seen — otherwise one register now and one
    /// register a minute later takes a live, flowing tunnel with two packets and
    /// no sustained presence at all.
    #[test]
    fn a_challenger_that_goes_away_must_start_its_ceiling_again() {
        let mut hd = hold_down();
        let incumbent = addr("198.51.100.10", 5000);
        let attacker = addr("203.0.113.7", 5000);
        let start = 1_000_000u64;

        hd.note_media(start);
        assert!(matches!(
            hd.offer(Some(incumbent), attacker, start),
            LatchDecision::Refuse { .. }
        ));

        // Silence for well over a ceiling, then a single register.
        let late = start + SLOT_CONTESTED_CEILING_MS + 1;
        hd.note_media(late);
        assert!(
            matches!(
                hd.offer(Some(incumbent), attacker, late),
                LatchDecision::Refuse { reason: RefusedLatch::SlotHeld, .. }
            ),
            "two packets a ceiling apart is not continuous presence"
        );
    }

    /// The ceiling is per-challenger: a different IP arriving mid-contest
    /// restarts the clock, so a rotating flood cannot accumulate credit toward
    /// taking a live tunnel.
    #[test]
    fn the_ceiling_clock_restarts_when_the_challenger_changes() {
        let mut hd = hold_down();
        let incumbent = addr("198.51.100.10", 5000);
        let now = 1_000_000u64;
        hd.note_media(now);

        // Challenger A contends for almost the whole ceiling...
        let a = addr("203.0.113.7", 5000);
        hd.offer(Some(incumbent), a, now);
        hd.note_media(now + SLOT_CONTESTED_CEILING_MS - 1);
        // ...then B arrives and must start from zero, not inherit A's credit.
        let b = addr("203.0.113.8", 5000);
        assert!(matches!(
            hd.offer(Some(incumbent), b, now + SLOT_CONTESTED_CEILING_MS - 1),
            LatchDecision::Refuse { .. }
        ));
        hd.note_media(now + SLOT_CONTESTED_CEILING_MS + 1);
        assert!(
            matches!(
                hd.offer(Some(incumbent), b, now + SLOT_CONTESTED_CEILING_MS + 1),
                LatchDecision::Refuse { .. }
            ),
            "B must serve its own ceiling, not inherit A's"
        );
    }

    /// An accept clears the contest, so the peer that just lost the slot has to
    /// serve a full ceiling of its own to get it back. Without this the two
    /// would trade the tunnel every tick once the first ceiling elapsed.
    #[test]
    fn an_accept_clears_the_contest() {
        let mut hd = hold_down();
        let a = addr("198.51.100.10", 5000);
        let b = addr("203.0.113.7", 5000);
        let now = 1_000_000u64;

        hd.note_media(now);
        hd.offer(Some(a), b, now);
        assert!(hd.contender.is_some());
        // A quiet incumbent lets B in on the ordinary grace path.
        assert_eq!(
            hd.offer(Some(a), b, now + SLOT_TAKEOVER_GRACE_MS),
            LatchDecision::Accept
        );
        assert!(hd.contender.is_none(), "an accept must clear the contest");
    }

    /// `is_latchable_source` refuses the addresses the latch must never turn
    /// into a send target. Port 0 is the reachable one — it is legal UDP, is not
    /// filtered as a martian, and `recv_from` reports it verbatim, so one
    /// spoofed register would make every `send_to` on the return path fail
    /// `EINVAL` into a `let _ =`.
    #[test]
    fn unlatchable_sources_are_refused() {
        assert!(!is_latchable_source(addr("198.51.100.10", 0)), "port 0");
        assert!(!is_latchable_source(addr("0.0.0.0", 5000)), "unspecified v4");
        assert!(!is_latchable_source(addr("239.1.2.3", 5000)), "multicast v4");
        assert!(
            !is_latchable_source(addr("255.255.255.255", 5000)),
            "broadcast v4"
        );
        assert!(!is_latchable_source(addr("::", 5000)), "unspecified v6");
        assert!(!is_latchable_source(addr("ff02::1", 5000)), "multicast v6");

        // Permissive where it must be, or the testbed and every RFC1918
        // deployment break.
        assert!(is_latchable_source(addr("127.0.0.1", 5000)));
        assert!(is_latchable_source(addr("127.0.0.2", 5000)));
        assert!(is_latchable_source(addr("10.1.2.3", 5000)));
        assert!(is_latchable_source(addr("fd00::1", 5000)));
        assert!(is_latchable_source(addr("203.0.113.7", 5000)));
    }

    /// A martian source is refused on an idle slot too — where the hold-down
    /// itself is deliberately last-writer-wins — and never accrues toward the
    /// contested ceiling, or a spoofer sending from `ip:0` for a minute would be
    /// handed the tunnel's return path.
    #[test]
    fn a_martian_source_never_reaches_the_ceiling() {
        let mut hd = hold_down();
        let martian = addr("198.51.100.10", 0);
        let mut now = 1_000_000u64;
        for _ in 0..(SLOT_CONTESTED_CEILING_MS / 1_000 + 5) {
            assert!(
                matches!(
                    hd.offer(None, martian, now),
                    LatchDecision::Refuse { reason: RefusedLatch::UnlatchableSource, .. }
                ),
                "an unlatchable source must be refused forever, ceiling or not"
            );
            now += 1_000;
        }
        assert!(hd.contender.is_none());
    }

    /// The refusal path is attacker-driven: it must not become a log or event
    /// amplifier. One report per grace window, with the rest counted.
    #[test]
    fn refusal_reports_are_rate_limited_and_count_what_they_hide() {
        let mut hd = hold_down();
        let incumbent = addr("198.51.100.10", 5000);
        let attacker = addr("203.0.113.7", 5000);
        let start = 1_000_000u64;
        hd.note_media(start);

        let first = hd.offer(Some(incumbent), attacker, start);
        assert_eq!(
            first,
            LatchDecision::Refuse {
                reason: RefusedLatch::SlotHeld,
                report: Some(0)
            },
            "the first refusal is always reported"
        );

        // A flood inside the window reports nothing further.
        let mut now = start;
        for _ in 0..100 {
            now += 10;
            hd.note_media(now);
            assert_eq!(
                hd.offer(Some(incumbent), attacker, now),
                LatchDecision::Refuse {
                    reason: RefusedLatch::SlotHeld,
                    report: None
                }
            );
        }

        // The next window reports once, saying how much was hidden.
        now = start + SLOT_TAKEOVER_GRACE_MS;
        hd.note_media(now);
        assert_eq!(
            hd.offer(Some(incumbent), attacker, now),
            LatchDecision::Refuse {
                reason: RefusedLatch::SlotHeld,
                report: Some(100)
            }
        );
    }
}

/// End-to-end coverage of the direct-mode listener over real sockets.
///
/// This module carries the actual proof. `direct_listener_holddown_tests` pins
/// the boundary arithmetic of the pure helpers, but a helper test passes whether
/// or not the listener consults the helper — only these drive the real listener
/// task over real UDP.
///
/// Linux-only because a hijack is by definition a *different source IP*, and
/// `127.0.0.2` is a usable loopback address there with no configuration (macOS
/// needs an explicit `ifconfig lo0 alias`).
#[cfg(all(test, target_os = "linux"))]
mod direct_listener_wire_tests {
    use super::*;
    use crate::manager::events::{event_channel, Event};
    use tokio::time::timeout;

    const PSK: &str = "0123456789abcdef0123456789abcdef";
    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SETTLE: Duration = Duration::from_millis(400);
    const PATIENCE: Duration = Duration::from_secs(2);

    /// Claim an ephemeral UDP port and release it, so the listener under test
    /// can bind a port we know. It binds it microseconds later; a loss of the
    /// race surfaces as a loud bind error, never as a silent pass.
    fn reserve_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .expect("probe bind")
            .local_addr()
            .expect("probe addr")
            .port()
    }

    fn register_bytes(tunnel_id: Uuid) -> Vec<u8> {
        let token = super::super::auth::generate_token(&tunnel_id.to_string(), PSK);
        encode_udp_control(&UdpRelayControl::Register {
            tunnel_id,
            direction: RelayDirection::Egress,
            bind_token: Some(token),
            protocol_version: TUNNEL_PROTOCOL_VERSION,
        })
        .expect("encode register")
    }

    fn production_hold_down() -> SlotHoldDown {
        SlotHoldDown::new(SLOT_TAKEOVER_GRACE_MS, SLOT_CONTESTED_CEILING_MS)
    }

    /// A running direct-mode listener plus the "local application" socket it
    /// forwards into and the event stream it reports refusals on.
    struct Harness {
        tunnel_id: Uuid,
        listen_addr: SocketAddr,
        app: UdpSocket,
        cipher: Option<Arc<TunnelCipher>>,
        events: tokio::sync::mpsc::Receiver<Event>,
        cancel: CancellationToken,
        listener: tokio::task::JoinHandle<Result<()>>,
    }

    impl Harness {
        /// The shape the hold-down actually protects: an ENCRYPTED direct
        /// tunnel. Only AEAD-opened media arms the hold-down, so an unencrypted
        /// tunnel is deliberately left at last-writer-wins — covered separately
        /// by `an_unencrypted_direct_tunnel_is_not_locked_by_unauthenticated_media`.
        async fn start() -> Self {
            Self::start_with(true, production_hold_down()).await
        }

        async fn start_with(encrypted: bool, hold_down: SlotHoldDown) -> Self {
            let tunnel_id = Uuid::new_v4();
            // Bound first, so its address is known without a race.
            let app = UdpSocket::bind("127.0.0.1:0").await.expect("app bind");
            let forward_addr = app.local_addr().expect("app addr");
            let listen_addr: SocketAddr =
                format!("127.0.0.1:{}", reserve_port()).parse().expect("listen addr");
            let cipher = encrypted
                .then(|| Arc::new(TunnelCipher::new(KEY).expect("test cipher")));
            let (state_tx, _state_rx) = watch::channel(RelayTunnelState::Down);
            let (event_tx, events) = event_channel();
            let cancel = CancellationToken::new();
            let listener = tokio::spawn(run_native_direct_listener(
                tunnel_id,
                listen_addr,
                forward_addr,
                PSK.to_string(),
                state_tx,
                cancel.clone(),
                Arc::new(UdpForwarderStats::default()),
                cipher.clone(),
                event_tx,
                hold_down,
            ));
            Self { tunnel_id, listen_addr, app, cipher, events, cancel, listener }
        }

        /// Frame `body` exactly as a conforming peer's `udp_forwarder::run_egress`
        /// would: encrypt when the tunnel is keyed, then prefix the tunnel id.
        fn frame(&self, body: &[u8]) -> Vec<u8> {
            self.frame_for(self.tunnel_id, body)
        }

        fn frame_for(&self, id: Uuid, body: &[u8]) -> Vec<u8> {
            let payload = match &self.cipher {
                Some(c) => c.encrypt(body).expect("encrypt"),
                None => body.to_vec(),
            };
            protocol::encode_udp_datagram(&id, &payload)
        }

        /// Undo `frame` on a datagram the listener returned.
        fn unframe(&self, datagram: &[u8]) -> (Uuid, Vec<u8>) {
            let (id, payload) = protocol::decode_udp_datagram(datagram).expect("framed");
            let body = match &self.cipher {
                Some(c) => c.decrypt(payload).expect("decrypt"),
                None => payload.to_vec(),
            };
            (id, body)
        }

        /// Register from `sock` and assert it was acked (i.e. it now owns the
        /// caller latch).
        async fn register_ok(&self, sock: &UdpSocket) {
            sock.send_to(&register_bytes(self.tunnel_id), self.listen_addr)
                .await
                .expect("send register");
            let mut buf = [0u8; 512];
            let n = timeout(PATIENCE, sock.recv(&mut buf))
                .await
                .expect("register must be acked")
                .expect("ack recv");
            assert!(
                matches!(try_decode_udp_control(&buf[..n]), Some(UdpRelayControl::Ack { .. })),
                "an accepted register must be acked"
            );
        }

        /// Send one media datagram and assert the app received exactly it.
        /// Returns the listener's loopback source address (the return path).
        async fn expect_forwarded(&self, sock: &UdpSocket, body: &[u8]) -> SocketAddr {
            sock.send_to(&self.frame(body), self.listen_addr)
                .await
                .expect("send media");
            let mut buf = [0u8; 512];
            let (n, src) = timeout(PATIENCE, self.app.recv_from(&mut buf))
                .await
                .expect("media must reach the app")
                .expect("app recv");
            assert_eq!(&buf[..n], body);
            src
        }

        async fn shutdown(self) {
            self.cancel.cancel();
            let _ = timeout(PATIENCE, self.listener).await;
        }
    }

    /// THE HIJACK, on the wire. The listener trusts a *static*
    /// `HMAC(tunnel_id, psk)` token that is re-sent in the clear every 5 s, so
    /// an observer can replay it verbatim from anywhere, forever. While media is
    /// flowing, that replay must be refused, un-acked, and must change nothing:
    /// the real caller keeps delivering, the attacker cannot inject, and the
    /// return traffic — the actual payoff — still goes to the real caller.
    #[tokio::test]
    async fn replayed_register_from_another_ip_cannot_steal_a_live_tunnel() {
        let h = Harness::start().await;

        let caller = UdpSocket::bind("127.0.0.1:0").await.expect("caller bind");
        h.register_ok(&caller).await;
        // Media is what arms the hold-down.
        let return_path = h.expect_forwarded(&caller, b"live-feed-1").await;

        // ── The attack: the caller's own register, replayed from another IP. ──
        let attacker = UdpSocket::bind("127.0.0.2:0").await.expect("attacker bind");
        attacker
            .send_to(&register_bytes(h.tunnel_id), h.listen_addr)
            .await
            .expect("replay register");
        let mut buf = [0u8; 512];
        assert!(
            timeout(SETTLE, attacker.recv(&mut buf)).await.is_err(),
            "a refused register must not be acked — an ack confirms the tunnel id is real"
        );

        // The attacker never became the caller, so its media is still dropped.
        attacker
            .send_to(&h.frame(b"attacker-feed"), h.listen_addr)
            .await
            .expect("send attacker media");
        assert!(
            timeout(SETTLE, h.app.recv_from(&mut buf)).await.is_err(),
            "the attacker must not be able to inject into a live tunnel"
        );

        // The real caller is untouched.
        h.expect_forwarded(&caller, b"live-feed-2").await;

        // And the return path (SRT handshake/ACK/NAK, or media on a reversed
        // leg) still reaches the real caller rather than the replayer.
        h.app
            .send_to(b"return-traffic", return_path)
            .await
            .expect("app return send");
        let n = timeout(PATIENCE, caller.recv(&mut buf))
            .await
            .expect("return traffic must reach the real caller, not the attacker")
            .expect("caller recv return");
        let (id, payload) = h.unframe(&buf[..n]);
        assert_eq!(id, h.tunnel_id);
        assert_eq!(payload, b"return-traffic");

        h.shutdown().await;
    }

    /// THE OTHER HALF OF THE HIJACK, on the wire: the hold-down must not turn a
    /// ≤ 5 s theft into a permanent one.
    ///
    /// An idle slot is deliberately last-writer-wins, so a peer holding the
    /// replayable token registers FIRST while the tunnel is idle and then keeps
    /// its own media flowing — refreshing the stamp for as long as it likes.
    /// Without a ceiling the real caller's every later register is refused for
    /// the life of the process, its media is dropped by the accept gate, and
    /// nothing recovers: the listener loops until cancel or a socket error, and
    /// the squatter's traffic keeps `packets_received` climbing so a
    /// traffic-based watchdog reads the tunnel as healthy. Unbounded blackout on
    /// a contribution feed is strictly worse than the bounded theft it replaced.
    ///
    /// Runs against a sub-second ceiling so the test costs milliseconds; the
    /// production value and its ordering against the grace are asserted in
    /// `direct_listener_holddown_tests::contested_ceiling_is_well_above_the_grace`.
    #[tokio::test]
    async fn contested_ceiling_returns_a_hijacked_tunnel_to_the_real_caller() {
        let ceiling = Duration::from_millis(600);
        let h = Harness::start_with(
            true,
            SlotHoldDown::new(SLOT_TAKEOVER_GRACE_MS, ceiling.as_millis() as u64),
        )
        .await;

        // The squatter wins the idle-slot race with the harvested token...
        let squatter = UdpSocket::bind("127.0.0.2:0").await.expect("squatter bind");
        h.register_ok(&squatter).await;
        // ...and replays one captured ciphertext, which arms the hold-down.
        // (There is no anti-replay at this layer — the nonce travels in the
        // datagram — so the same passive attacker that harvested the token
        // harvested this too.)
        h.expect_forwarded(&squatter, b"squat").await;

        // The real caller is now locked out for as long as the squatter holds.
        let caller = UdpSocket::bind("127.0.0.1:0").await.expect("caller bind");
        caller
            .send_to(&register_bytes(h.tunnel_id), h.listen_addr)
            .await
            .expect("send register");
        let mut buf = [0u8; 512];
        assert!(
            timeout(Duration::from_millis(200), caller.recv(&mut buf))
                .await
                .is_err(),
            "the hold-down must refuse while the incumbent is inside the grace"
        );

        // But it must not be permanent. Keep presenting the valid token; the
        // ceiling has to yield the slot back.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the ceiling never yielded — the hold-down has turned a bounded hijack \
                 into a permanent, unrecoverable one"
            );
            // The squatter behaves like any conforming caller and re-sends its
            // own `Register` — the egress arm of `run_native_direct_tunnel`
            // spawns a task that does this every `KEEPALIVE_INTERVAL` forever.
            // Those same-IP registers are accepted, and accepting one must not
            // reset the real caller's ceiling clock, or a squatter defeats the
            // ceiling by doing nothing more than behaving normally.
            squatter
                .send_to(&register_bytes(h.tunnel_id), h.listen_addr)
                .await
                .expect("squatter keepalive");
            caller
                .send_to(&register_bytes(h.tunnel_id), h.listen_addr)
                .await
                .expect("send register");
            if let Ok(Ok(n)) = timeout(Duration::from_millis(100), caller.recv(&mut buf)).await {
                assert!(
                    matches!(
                        try_decode_udp_control(&buf[..n]),
                        Some(UdpRelayControl::Ack { .. })
                    ),
                    "the yield must be a real ack"
                );
                break;
            }
        }

        // And the real feed flows again.
        h.expect_forwarded(&caller, b"real-feed").await;
        h.shutdown().await;
    }

    /// An UNENCRYPTED direct tunnel must keep the pre-hold-down last-writer-wins
    /// behaviour.
    ///
    /// `tunnel_encryption_key` is optional in direct mode
    /// (`manager::start_native_direct_tunnel`), and with no key nothing
    /// authenticates media: any datagram beginning with the tunnel id — which
    /// travels in the cleartext register token — passes framing. Arming the
    /// hold-down on that would hand whoever wins the idle-slot race a lock on
    /// the tunnel with no key at all, for a whole contested ceiling at a time.
    /// A tunnel that can offer no proof gets no protection.
    #[tokio::test]
    async fn an_unencrypted_direct_tunnel_is_not_locked_by_unauthenticated_media() {
        let h = Harness::start_with(false, production_hold_down()).await;

        let squatter = UdpSocket::bind("127.0.0.2:0").await.expect("squatter bind");
        h.register_ok(&squatter).await;
        h.expect_forwarded(&squatter, b"unauthenticated").await;

        // The real caller re-latches immediately, exactly as it did before the
        // hold-down existed.
        let caller = UdpSocket::bind("127.0.0.1:0").await.expect("caller bind");
        h.register_ok(&caller).await;
        h.expect_forwarded(&caller, b"real").await;

        h.shutdown().await;
    }

    /// A refusal must reach the operator, and must not be an amplifier.
    ///
    /// Left as a `tracing::warn!` alone it is a black hole: the tunnel state
    /// stays `Ready` (it was set when the incumbent latched) and
    /// `packets_received` keeps climbing, so no manager surface shows a caller
    /// being locked out. Unthrottled it is worse than the silence it replaced —
    /// an accepted register used to log nothing at all, so every line here is
    /// new volume an attacker chooses the rate of.
    #[tokio::test]
    async fn a_refused_register_is_reported_once_per_window() {
        let mut h = Harness::start().await;

        let caller = UdpSocket::bind("127.0.0.1:0").await.expect("caller bind");
        h.register_ok(&caller).await;
        h.expect_forwarded(&caller, b"live").await;

        let attacker = UdpSocket::bind("127.0.0.2:0").await.expect("attacker bind");
        for _ in 0..8 {
            attacker
                .send_to(&register_bytes(h.tunnel_id), h.listen_addr)
                .await
                .expect("replay register");
        }

        let ev = timeout(PATIENCE, h.events.recv())
            .await
            .expect("a refusal must be operator-visible")
            .expect("event channel open");
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(ev.category, category::TUNNEL);
        let details = ev.details.expect("structured details");
        assert_eq!(details["error_code"], "tunnel_register_refused");
        assert_eq!(details["reason"], RefusedLatch::SlotHeld.code());
        assert_eq!(details["tunnel_id"], h.tunnel_id.to_string());
        assert_eq!(
            details["refused_addr"],
            attacker.local_addr().expect("attacker addr").to_string()
        );
        assert_eq!(
            details["held_addr"],
            caller.local_addr().expect("caller addr").to_string()
        );

        // Eight refusals inside one grace window produce exactly one event.
        tokio::time::sleep(SETTLE).await;
        assert!(
            h.events.try_recv().is_err(),
            "refusals must be rate-limited — an attacker picks the rate"
        );

        h.shutdown().await;
    }

    /// A same-IP port rebind (NAT rebind, caller socket rotation) must still
    /// re-latch mid-stream — refusing it would drop real sessions, which is the
    /// way this hardening could plausibly break production.
    #[tokio::test]
    async fn same_ip_rebind_relatches_mid_stream() {
        let h = Harness::start().await;

        let caller = UdpSocket::bind("127.0.0.1:0").await.expect("caller bind");
        h.register_ok(&caller).await;
        h.expect_forwarded(&caller, b"a").await;

        // Same IP, brand-new port, while the hold-down is armed.
        let rebound = UdpSocket::bind("127.0.0.1:0").await.expect("rebind");
        h.register_ok(&rebound).await;
        h.expect_forwarded(&rebound, b"b").await;

        h.shutdown().await;
    }

    /// A datagram framed for a DIFFERENT tunnel id must be dropped rather than
    /// decrypted and forwarded here. The prefix rides outside the AEAD, so the
    /// receiving edge is the only place a cross-tunnel replay can be refused.
    #[tokio::test]
    async fn datagram_framed_for_another_tunnel_is_dropped() {
        let h = Harness::start().await;
        let other_tunnel = Uuid::new_v4();

        let caller = UdpSocket::bind("127.0.0.1:0").await.expect("caller bind");
        h.register_ok(&caller).await;

        caller
            .send_to(&h.frame_for(other_tunnel, b"wrong-tunnel"), h.listen_addr)
            .await
            .expect("send foreign datagram");
        let mut buf = [0u8; 512];
        assert!(
            timeout(SETTLE, h.app.recv_from(&mut buf)).await.is_err(),
            "a datagram addressed to another tunnel must not be forwarded"
        );

        // A correctly-addressed datagram still flows, so this is a filter and
        // not a blanket refusal.
        h.expect_forwarded(&caller, b"ours").await;

        h.shutdown().await;
    }
}
