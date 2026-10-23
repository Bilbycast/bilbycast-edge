// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What keeps the manager link from stranding a node.
//!
//! A node's link to the manager could stop for good with nothing logged: the
//! manager showed it offline, its media kept flowing, and only restarting the
//! edge brought it back (bilbycast-edge#180). Every way in was an `.await`
//! with no end in the one task that does all the reconnecting, or a
//! reconnect that authenticated with credentials the manager no longer held.
//! This module holds the pieces that close those ways, kept apart from the
//! session loop in `client.rs` so each can be tested on its own:
//!
//! * **The dial** ([`dial_manager`]) — resolve, connect and upgrade, each
//!   stage bounded by what is left of one deadline, with the addresses raced
//!   so one that drops SYNs cannot hold the dial on its own.
//! * **The handshake** ([`handshake_backstop`]) — a deadline over the whole
//!   pre-session exchange, disarmed the moment the auth reply is in hand.
//! * **The live session** — a reader that runs beside the session's writes
//!   ([`read_inbound`]), a watchdog on the manager's silence learned from its
//!   pings ([`InboundCadence`], [`manager_silence_watchdog`]), and a bound on
//!   every write ([`send_bounded`]).
//! * **The credentials** ([`LinkCredentials`]) — what each attempt
//!   authenticates with, carried from one connection to the next.
//!
//! bilbycast-relay and the gateway SDK keep the same model with the same
//! constants, so an operator sees one behaviour whichever node it is.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, Bytes, Message, Utf8Bytes};

use super::ManagerConfig;

/// Hard deadline on the whole dial: DNS, TCP, TLS and the HTTP upgrade.
///
/// `tokio_tungstenite::connect_async*` has no deadline of its own. A peer that
/// completes TCP and TLS and then never returns the 101 parks the future for
/// ever, and because the reconnect loop is a single task that one `.await`
/// ends all future reconnection: the node goes offline until the process is
/// restarted, with no error logged. Two production edges did exactly that,
/// 20 hours apart, and stayed dark for 36 and 29 hours while their media kept
/// flowing — see bilbycast-edge#180.
pub(super) const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one TCP attempt runs on its own before the next address is
/// started beside it (RFC 8305's "connection attempt delay"). An attempt that
/// fails starts the next at once.
const CONNECT_ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// A dial slower than this is logged even when it eventually succeeds, so the
/// condition is visible before it becomes an outage.
const SLOW_DIAL_WARN: Duration = Duration::from_secs(5);

/// Deadline on writing the auth frame. A stalled peer stops draining long
/// before it closes, so this write can block just as the dial can.
pub(super) const AUTH_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline on the manager's answer to the auth frame.
pub(super) const AUTH_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline on the whole pre-session handshake: dial, auth write and auth
/// reply. Longer than those three together (30 + 10 + 10 s), so it never cuts
/// off an attempt they would still have let through.
///
/// The per-step timeouts cover the stalls we have actually observed. This one
/// covers the ones we have not: it is armed before the attempt and disarmed
/// the moment a well-formed auth reply is in hand, so any pre-auth step that
/// blocks for a reason nobody anticipated still ends in a retry rather than
/// in a node that never comes back. Generous on purpose — it is a backstop,
/// not the primary control.
pub(super) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Inbound silence never tolerated for less than this, however fast the
/// manager pings. It is only a floor: the real limit scales with the cadence
/// the manager is actually using (see [`InboundCadence::silence_limit`]).
const SILENCE_FLOOR: Duration = Duration::from_secs(90);

/// How many ping intervals of silence end a session. Four, so one lost ping
/// cannot do it on its own.
const SILENCE_PINGS: u32 = 4;

/// How often the watchdog looks. Coarse on purpose: the limit is measured in
/// minutes, and a check is a clock read.
const SILENCE_CHECK: Duration = Duration::from_secs(5);

/// The manager's largest `ws_keepalive_interval_secs`. A gap between two
/// pings longer than this is a manager that stalled, not its cadence, so no
/// gap is counted as longer.
const MAX_KEEPALIVE: Duration = Duration::from_secs(300);

/// How many of the most recent ping gaps the cadence is judged on.
const CADENCE_GAPS: usize = 8;

/// A single write to the manager that has not completed in this long ends
/// the session: the manager has stopped reading.
///
/// Independent of the ping cadence on purpose. It is what covers the window
/// before the first ping of every session, and the whole of a session with a
/// manager that never pings at all — in both, the silence watchdog is
/// unarmed. A quiet but healthy session has no write pending, so it cannot
/// trip it, and a manager that is merely slow to drain completes each write
/// well inside five minutes.
pub(super) const WRITE_STALL_LIMIT: Duration = Duration::from_secs(300);

/// Depth of the queue the reader hands frames to the session loop on. Pings
/// never enter it, so only a manager that has sent this many commands the
/// loop has not yet picked up can hold the reader back.
pub(super) const INBOUND_QUEUE: usize = 256;

/// How many secrets the edge keeps besides the one it presents.
///
/// Each is one refused authentication at worst, and the manager locks a node
/// out after five refusals in 60 s (`NodeAuthLimiter`). Four fallbacks means
/// the right secret is reached on the fifth attempt at the latest, before the
/// lockout would refuse it too.
const MAX_FALLBACK_SECRETS: usize = 4;

// ───────────────────────────────────────────────────────
// Why a connection ended
// ───────────────────────────────────────────────────────

/// What kind of failure ended a connection attempt or a session.
///
/// Each kind has its own `error_code` and its own event message. The manager
/// coalesces events on their message and keeps the first occurrence's
/// details, so one message for every cause would show the first cause
/// against every later one; one message per kind keeps the kinds apart on
/// the Events page, each with a count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkLoss {
    /// The connection settings cannot be used as they stand: a plaintext
    /// URL, self-signed mode without its guard, an unparseable URL.
    Settings,
    /// The dial failed outright: DNS, a refused TCP connect, TLS, or the
    /// HTTP upgrade answered with an error.
    DialFailed,
    /// The dial ran out of time.
    DialTimeout,
    /// The auth frame could not be written, or no answer came, in time.
    HandshakeTimeout,
    /// The manager refused the credentials.
    AuthRejected,
    /// The manager answered with something that is not an auth reply.
    Protocol,
    /// The connection broke: a WebSocket error, or a close during auth.
    ConnectionError,
    /// Nothing heard from the manager past the limit its pings set.
    ManagerSilent,
    /// A write to the manager stopped completing.
    WriteStalled,
}

impl LinkLoss {
    /// The stable `details.error_code` of the "Manager connection lost" event.
    pub(super) fn error_code(self) -> &'static str {
        match self {
            Self::Settings => "manager_link_settings",
            Self::DialFailed => "manager_dial_failed",
            Self::DialTimeout => "manager_dial_timeout",
            Self::HandshakeTimeout => "manager_handshake_timeout",
            Self::AuthRejected => "manager_auth_rejected",
            Self::Protocol => "manager_protocol_error",
            Self::ConnectionError => "manager_connection_error",
            Self::ManagerSilent => "manager_silent",
            Self::WriteStalled => "manager_write_stalled",
        }
    }

    /// The event's message: constant per kind, for the coalescing reason
    /// above. The specific reason rides in `details.reason`.
    pub(super) fn event_message(self) -> &'static str {
        match self {
            Self::Settings => "Manager connection lost: connection settings refused",
            Self::DialFailed => "Manager connection lost: could not reach the manager",
            Self::DialTimeout => "Manager connection lost: dial timed out",
            Self::HandshakeTimeout => "Manager connection lost: handshake timed out",
            Self::AuthRejected => "Manager connection lost: authentication refused",
            Self::Protocol => "Manager connection lost: unexpected reply",
            Self::ConnectionError => "Manager connection lost: connection error",
            Self::ManagerSilent => "Manager connection lost: manager went silent",
            Self::WriteStalled => "Manager connection lost: manager stopped reading",
        }
    }
}

/// A failed connection attempt or session: its kind, and what happened.
#[derive(Debug)]
pub(super) struct LinkError {
    pub(super) loss: LinkLoss,
    pub(super) reason: String,
}

impl LinkError {
    pub(super) fn new(loss: LinkLoss, reason: impl Into<String>) -> Self {
        Self { loss, reason: reason.into() }
    }
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// How a live session stopped.
#[derive(Debug)]
pub(super) enum SessionEnd {
    /// The connection closed or failed under a write: an ordinary end, after
    /// which the reconnect loop dials again without raising an event.
    Closed,
    /// The session failed, for the reason given.
    Lost(LinkError),
}

impl From<LinkError> for SessionEnd {
    fn from(e: LinkError) -> Self {
        Self::Lost(e)
    }
}

/// A manager URL as it may be shown in an event: scheme, host, port and path,
/// with any userinfo, query and fragment dropped. A URL may carry a
/// credential for whatever fronts the manager, and an event is readable by
/// everyone the node is shared with.
pub(super) fn shown_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => "(unparseable URL)".to_string(),
    }
}

// ───────────────────────────────────────────────────────
// The dial
// ───────────────────────────────────────────────────────

/// The stream every dial produces.
pub(super) type ManagerWsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Where a manager URL points: the host as written (for messages) and the
/// socket addresses it names.
struct DialTarget {
    host: String,
    port: u16,
    /// Set when the host is an IP literal, which needs no lookup.
    literal: Option<std::net::IpAddr>,
}

impl DialTarget {
    fn parse(url: &str) -> Result<Self, LinkError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| LinkError::new(LinkLoss::Settings, format!("Manager URL cannot be parsed: {e}")))?;
        let port = parsed.port_or_known_default().ok_or_else(|| {
            LinkError::new(
                LinkLoss::Settings,
                format!("Manager URL scheme {:?} has no known port", parsed.scheme()),
            )
        })?;
        let (host, literal) = match parsed.host() {
            Some(url::Host::Domain(name)) => (name.to_string(), None),
            Some(url::Host::Ipv4(ip)) => (ip.to_string(), Some(ip.into())),
            // `host_str` keeps the brackets an IPv6 literal is written with,
            // and a lookup would refuse them.
            Some(url::Host::Ipv6(ip)) => (ip.to_string(), Some(ip.into())),
            None => {
                return Err(LinkError::new(LinkLoss::Settings, "Manager URL has no host"));
            }
        };
        Ok(Self { host, port, literal })
    }
}

/// Dial the manager at `url`, giving up after `deadline`.
///
/// Three stages, each bounded by what is left of the one deadline, and each
/// named by the error it ends in, so a log line points at the step that
/// failed rather than at the dial as a whole:
///
/// 1. **DNS** — `tokio::net::lookup_host`; an IP literal needs none.
/// 2. **TCP** — the addresses raced Happy-Eyeballs style
///    ([`connect_first`]). tokio's own `TcpStream::connect(host)` tries them
///    strictly one after another with no deadline on any one of them, so a
///    first address that silently drops SYNs (a broken IPv6 path to a
///    dual-stack manager, a dead A record) held it for the kernel's SYN
///    timeout, ~127 s — and a deadline shorter than that meant the second
///    address was never tried at all. Neither a longer deadline nor sorting
///    IPv4 first would fix that: the handshake backstop would cut off the
///    first, and the second only swaps which family's failure strands the
///    node.
/// 3. **TLS + upgrade** — `client_async_tls_with_config` on the connected
///    stream. That is the function `connect_async*` calls once it has a
///    socket, so SNI, the certificate pinning and the self-signed verifier
///    behave exactly as they did; passing `None` as the connector is what
///    `connect_async` does. Nagle stays on, as `connect_async` leaves it.
pub(super) async fn dial_manager(
    url: &str,
    connector: Option<tokio_tungstenite::Connector>,
    deadline: Duration,
) -> Result<ManagerWsStream, LinkError> {
    let started = Instant::now();
    let give_up = started + deadline;
    let target = DialTarget::parse(url)?;
    let host = target.host.as_str();

    let addrs: Vec<SocketAddr> = match target.literal {
        Some(ip) => vec![SocketAddr::new(ip, target.port)],
        None => match tokio::time::timeout_at(give_up, tokio::net::lookup_host((host, target.port))).await {
            Ok(Ok(found)) => found.collect(),
            Ok(Err(e)) => {
                return Err(LinkError::new(
                    LinkLoss::DialFailed,
                    format!("manager DNS lookup for {host} failed: {e}"),
                ));
            }
            Err(_) => {
                return Err(LinkError::new(
                    LinkLoss::DialTimeout,
                    format!("manager DNS lookup for {host} timed out after {}s", deadline.as_secs()),
                ));
            }
        },
    };
    if addrs.is_empty() {
        return Err(LinkError::new(
            LinkLoss::DialFailed,
            format!("manager DNS lookup for {host} returned no addresses"),
        ));
    }

    let ordered = happy_eyeballs_order(&addrs);
    let counted = match ordered.len() {
        1 => "1 address".to_string(),
        n => format!("{n} addresses"),
    };
    let race = connect_first(&ordered, tokio::net::TcpStream::connect);
    let (winner, tcp) = match tokio::time::timeout_at(give_up, race).await {
        Ok(Ok(won)) => won,
        Ok(Err(e)) => {
            return Err(LinkError::new(
                LinkLoss::DialFailed,
                format!("TCP connect to {host} ({counted}) failed: {e}"),
            ));
        }
        Err(_) => {
            return Err(LinkError::new(
                LinkLoss::DialTimeout,
                format!("TCP connect to {host} timed out after {}s", deadline.as_secs()),
            ));
        }
    };
    // Attempts start in order, so every address ahead of the winner was
    // tried first and either failed or was still waiting when it won. Said
    // once per dial, so a broken path shows up in the journal instead of as
    // an unexplained quarter-second per connect.
    let passed_over = ordered.iter().take_while(|a| **a != winner).collect::<Vec<_>>();
    if passed_over.is_empty() {
        tracing::debug!("Manager TCP connect to {host} via {winner}");
    } else {
        tracing::warn!(
            "Manager TCP connect to {host} went via {winner}; {passed_over:?} did not connect first — \
             check the path to those addresses"
        );
    }

    let upgrade = tokio_tungstenite::client_async_tls_with_config(url, tcp, None, connector);
    let (stream, _response) = match tokio::time::timeout_at(give_up, upgrade).await {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => {
            return Err(LinkError::new(LinkLoss::DialFailed, format!("WebSocket connect failed: {e}")));
        }
        Err(_) => {
            return Err(LinkError::new(
                LinkLoss::DialTimeout,
                format!("TLS/WebSocket upgrade with {host} timed out after {}s", deadline.as_secs()),
            ));
        }
    };

    let took = started.elapsed();
    if took >= SLOW_DIAL_WARN {
        tracing::warn!(
            "Manager dial took {:.1}s — slow handshake, watch for a stall",
            took.as_secs_f64()
        );
    }
    Ok(stream)
}

/// Order `addrs` for [`connect_first`]: the families interleaved, starting
/// with the family of the first address the resolver returned (RFC 8305
/// §4). The resolver's own order within each family is kept.
fn happy_eyeballs_order(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else {
        return Vec::new();
    };
    let (mut lead, mut other): (VecDeque<SocketAddr>, VecDeque<SocketAddr>) =
        addrs.iter().partition(|a| a.is_ipv6() == first.is_ipv6());
    let mut ordered = Vec::with_capacity(addrs.len());
    while !lead.is_empty() || !other.is_empty() {
        ordered.extend(lead.pop_front());
        ordered.extend(other.pop_front());
    }
    ordered
}

/// Connect to the first of `ordered` that answers.
///
/// The first attempt starts at once; each next one starts
/// [`CONNECT_ATTEMPT_DELAY`] after the one before, or at once when an
/// attempt fails, and every attempt started keeps running beside the later
/// ones. The first to connect wins and the rest are dropped, which closes
/// them. All failed: the last error.
///
/// No attempt has a deadline of its own: the caller bounds the whole race,
/// and a short per-address cut would only hurt lossy links, where the third
/// or fourth SYN is the one that lands. `connect` is a parameter so the
/// tests can stand in an address that never answers.
async fn connect_first<C, F, T>(ordered: &[SocketAddr], mut connect: C) -> io::Result<(SocketAddr, T)>
where
    C: FnMut(SocketAddr) -> F,
    F: Future<Output = io::Result<T>>,
{
    let mut queue = ordered.iter().copied().peekable();
    let mut racing = FuturesUnordered::new();
    let mut start = |addr: SocketAddr| {
        let attempt = connect(addr);
        async move { (addr, attempt.await) }
    };
    let mut last_error = None;
    if let Some(addr) = queue.next() {
        racing.push(start(addr));
    }
    loop {
        tokio::select! {
            Some((addr, result)) = racing.next() => match result {
                Ok(stream) => return Ok((addr, stream)),
                Err(e) => {
                    last_error = Some(e);
                    if let Some(next) = queue.next() {
                        racing.push(start(next));
                    }
                }
            },
            () = tokio::time::sleep(CONNECT_ATTEMPT_DELAY), if queue.peek().is_some() => {
                if let Some(next) = queue.next() {
                    racing.push(start(next));
                }
            }
            // Nothing racing and nothing left to start: every address failed.
            else => {
                return Err(last_error.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no addresses to connect to")
                }));
            }
        }
    }
}

// ───────────────────────────────────────────────────────
// The handshake
// ───────────────────────────────────────────────────────

/// Resolves when an attempt has failed to authenticate in time.
///
/// If `authenticated` fires first the attempt got through, so this future
/// pends for ever and the `select!` it sits in can only be resolved by the
/// session itself ending. A dropped sender (the attempt returned early with
/// an error) also counts as "no longer waiting": the attempt's own error is
/// the better message, so this still pends and lets that one surface.
pub(super) async fn handshake_backstop(authenticated: oneshot::Receiver<()>, deadline: Duration) {
    if tokio::time::timeout(deadline, authenticated).await.is_ok() {
        std::future::pending::<()>().await;
    }
}

// ───────────────────────────────────────────────────────
// The live session
// ───────────────────────────────────────────────────────

/// What the live session has heard from the manager, and how often it pings.
///
/// The dial deadline (bilbycast-edge#180) covers a session that never starts.
/// This covers one that dies after it started: nothing in the session loop
/// noticed a manager that had gone quiet, and a peer that stops draining
/// parks a write on a full socket buffer exactly as the dial parked on a
/// missing 101 — the node offline in the manager, nothing logged, media still
/// flowing.
///
/// The manager pings every node on its `ws_keepalive_interval_secs` setting,
/// anywhere from 1 to [`MAX_KEEPALIVE`], read once per connection — so the
/// cadence is fixed for the life of a session, and the edge learns it from
/// the pings rather than assuming 15 s. The rules:
///
/// * **Unarmed until the first ping.** A manager that predates the keepalive
///   never pings, and treating its quiet as death would drop a healthy
///   session every few minutes. (A manager that never pings is still bounded,
///   by [`WRITE_STALL_LIMIT`].)
/// * **Armed on the first ping**, taking the time since the session started
///   as the first gap. That over-estimates the interval — the manager's first
///   ping comes one interval after its own post-auth setup — which is the
///   safe direction.
/// * **The median of the last [`CADENCE_GAPS`] gaps**, each clamped to
///   [`MAX_KEEPALIVE`]. Not the widest gap: a running maximum only ever
///   grows, so one 85 s manager stall turned a 90 s limit into 340 s for the
///   rest of the session, and the next stall pushed it further. A median
///   ignores the odd long gap, and the odd short one too.
#[derive(Debug, Clone)]
pub(super) struct InboundCadence {
    started: Instant,
    last_frame: Instant,
    last_ping: Option<Instant>,
    gaps: VecDeque<Duration>,
}

impl InboundCadence {
    pub(super) fn new(now: Instant) -> Self {
        Self { started: now, last_frame: now, last_ping: None, gaps: VecDeque::with_capacity(CADENCE_GAPS) }
    }

    /// Any frame at all from the manager proves the session is alive.
    pub(super) fn on_frame(&mut self, now: Instant) {
        self.last_frame = now;
    }

    /// A ping also teaches the cadence.
    pub(super) fn on_ping(&mut self, now: Instant) {
        self.on_frame(now);
        let since = self.last_ping.unwrap_or(self.started);
        if self.gaps.len() == CADENCE_GAPS {
            self.gaps.pop_front();
        }
        self.gaps.push_back(now.saturating_duration_since(since).min(MAX_KEEPALIVE));
        self.last_ping = Some(now);
    }

    /// The silence that ends the session, or `None` while no ping has come —
    /// the watchdog is unarmed until then.
    pub(super) fn silence_limit(&self) -> Option<Duration> {
        let mut sorted: Vec<Duration> = self.gaps.iter().copied().collect();
        sorted.sort_unstable();
        // The upper median of an even count: of two middle gaps, the longer
        // one, the safe direction again.
        let median = *sorted.get(sorted.len() / 2)?;
        Some(SILENCE_FLOOR.max(median.saturating_mul(SILENCE_PINGS)))
    }

    /// How long the manager has been silent, if that is past the limit.
    pub(super) fn dead_for(&self, now: Instant) -> Option<(Duration, Duration)> {
        let limit = self.silence_limit()?;
        let silent = now.saturating_duration_since(self.last_frame);
        (silent > limit).then_some((silent, limit))
    }
}

fn lock_cadence(cadence: &Mutex<InboundCadence>) -> MutexGuard<'_, InboundCadence> {
    cadence.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Record a frame from the manager: a ping teaches the cadence, anything else
/// only proves the manager is there.
pub(super) fn note_inbound(cadence: &Mutex<InboundCadence>, frame: &Message, now: Instant) {
    let mut heard = lock_cadence(cadence);
    if matches!(frame, Message::Ping(_)) {
        heard.on_ping(now);
    } else {
        heard.on_frame(now);
    }
}

/// A frame the reader hands the session loop.
#[derive(Debug)]
pub(super) enum Inbound {
    Text(Utf8Bytes),
    Binary(Bytes),
    /// A close frame, or the end of the stream.
    Closed,
    Failed(tungstenite::Error),
}

/// Read the manager's frames for the life of the session, stamping each on
/// the cadence as it arrives, and hand the session loop what it acts on.
///
/// It runs beside the session loop rather than as one of its `select!`
/// branches, which is the point. The loop does its writes inside its
/// branches, so a loop held up by its own slow writes — a burst of
/// thumbnails on a starved uplink — left the manager's pings unread in the
/// kernel buffer, and the watchdog took a live but busy manager for a dead
/// one: the session flapped. Out here a frame is stamped when it arrives,
/// whatever the loop is doing.
///
/// Pings and pongs stop here. tungstenite queues the answer to a ping itself
/// and writes it on the next read, which is this loop's very next turn, so
/// the session loop no longer answers them (it would be a second pong).
/// Text, binary, the close and an error go on, in order; after a close or an
/// error the reader stops reading and waits to be dropped with the session.
pub(super) async fn read_inbound<R>(
    mut read: R,
    cadence: &Mutex<InboundCadence>,
    forward: mpsc::Sender<Inbound>,
) -> Infallible
where
    R: Stream<Item = Result<Message, tungstenite::Error>> + Unpin,
{
    loop {
        let next = match read.next().await {
            Some(Ok(frame)) => {
                note_inbound(cadence, &frame, Instant::now());
                match frame {
                    Message::Text(text) => Inbound::Text(text),
                    Message::Binary(bytes) => Inbound::Binary(bytes),
                    Message::Close(_) => Inbound::Closed,
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                }
            }
            Some(Err(e)) => Inbound::Failed(e),
            None => Inbound::Closed,
        };
        let last = matches!(next, Inbound::Closed | Inbound::Failed(_));
        if forward.send(next).await.is_err() || last {
            break;
        }
    }
    std::future::pending().await
}

/// Resolves once the manager has been silent past its limit.
///
/// Raced against the session rather than run as one of its loop's `select!`
/// branches: a write that never completes holds the loop inside one branch,
/// where no other branch can run. Out here the timer still fires, and winning
/// the race drops the session future — and the stuck write with it.
pub(super) async fn manager_silence_watchdog(cadence: &Mutex<InboundCadence>) -> (Duration, Duration) {
    let mut check = tokio::time::interval(SILENCE_CHECK);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        check.tick().await;
        if let Some(dead) = lock_cadence(cadence).dead_for(Instant::now()) {
            return dead;
        }
    }
}

/// Run the session loop, its reader and the silence watchdog together, until
/// the session ends or the watchdog calls the manager dead.
///
/// The reader never finishes on its own (after a close it waits), so the
/// session's own end is what reports a close or an error, in the order the
/// frames came. A close — or a write that failed under the connection — is an
/// ordinary end and comes back `Ok`.
pub(super) async fn race_session<S, R>(session: S, reader: R, cadence: &Mutex<InboundCadence>) -> Result<(), LinkError>
where
    S: Future<Output = Result<(), SessionEnd>>,
    R: Future<Output = Infallible>,
{
    tokio::select! {
        ended = session => match ended {
            Ok(()) | Err(SessionEnd::Closed) => Ok(()),
            Err(SessionEnd::Lost(e)) => Err(e),
        },
        never = reader => match never {},
        (silent, limit) = manager_silence_watchdog(cadence) => Err(LinkError::new(
            LinkLoss::ManagerSilent,
            format!(
                "Nothing heard from the manager for {}s (limit {}s from its ping cadence) \
                 — treating the session as dead and reconnecting",
                silent.as_secs(),
                limit.as_secs()
            ),
        )),
    }
}

/// Write one frame to the manager, giving up after [`WRITE_STALL_LIMIT`].
///
/// Every write the session makes goes through here, the first health frame
/// after auth included. A write that fails (the connection broke under it)
/// ends the session as a close does; one that cannot complete for the limit
/// ends it as a loss.
pub(super) async fn send_bounded<S>(sink: &mut S, frame: Message) -> Result<(), SessionEnd>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    match tokio::time::timeout(WRITE_STALL_LIMIT, sink.send(frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::info!("A write to the manager failed ({e}); ending the session");
            Err(SessionEnd::Closed)
        }
        Err(_) => Err(SessionEnd::Lost(LinkError::new(
            LinkLoss::WriteStalled,
            format!(
                "A write to the manager has not completed in {}s — the manager has stopped \
                 reading; reconnecting",
                WRITE_STALL_LIMIT.as_secs()
            ),
        ))),
    }
}

// ───────────────────────────────────────────────────────
// The credentials
// ───────────────────────────────────────────────────────

/// What the reconnect loop authenticates with, carried from one connection to
/// the next and shared with the command handlers.
///
/// The connection that learns of a change — a registration, a rotated secret,
/// which of several candidate secrets the manager accepted — updates this in
/// place, so the change survives however that connection ends. The edge used
/// to authenticate every attempt from the `ManagerConfig` it started with,
/// replaced only from a `Registered` value the session returned on a clean
/// end. So a successful `rotate_secret` left the next reconnect presenting
/// the secret the manager had just replaced — refused at every attempt, and
/// locked out, until a restart re-read `secrets.json` — and a session that
/// registered and then ended in an error (a connection reset, the usual way a
/// WebSocket ends, or the silence watchdog) dropped the credentials and
/// presented the consumed registration token until a restart.
///
/// The running `AppConfig` is never read back into this. It is what the
/// files hold and what a restart presents, and it has not always held the
/// secret: a `PUT /api/v1/config` of the masked `GET` body used to empty its
/// `node_secret`, which read back here would have had the next reconnect
/// present no secret at all.
///
/// bilbycast-relay keeps the same model (its `LinkCredentials`).
#[derive(Debug)]
pub(super) struct LinkCredentials {
    /// The connection settings. `node_secret` is the secret presented at the
    /// next authentication.
    pub(super) config: ManagerConfig,
    /// Secrets the manager may hold instead of `config.node_secret`, the most
    /// likely first. Empty except between a rotation and the first
    /// authentication that shows which secret the manager stored.
    fallback_secrets: Vec<String>,
}

/// The credentials as the reconnect loop and the command handlers share them.
/// A plain mutex: it is taken once per connection attempt and once per
/// credential change, never on a media path and never across an `.await`.
pub(super) type SharedCredentials = Arc<Mutex<LinkCredentials>>;

pub(super) fn lock_credentials(creds: &SharedCredentials) -> MutexGuard<'_, LinkCredentials> {
    creds.lock().unwrap_or_else(PoisonError::into_inner)
}

impl LinkCredentials {
    pub(super) fn new(config: ManagerConfig) -> Self {
        Self { config, fallback_secrets: Vec::new() }
    }

    pub(super) fn shared(config: ManagerConfig) -> SharedCredentials {
        Arc::new(Mutex::new(Self::new(config)))
    }

    /// The manager registered the edge and issued these credentials.
    pub(super) fn registered(&mut self, node_id: String, node_secret: String) {
        self.config.registration_token = None;
        self.config.node_id = Some(node_id);
        self.config.node_secret = Some(node_secret);
        self.fallback_secrets.clear();
    }

    /// The edge saved `new_secret` and is about to answer the `rotate_secret`
    /// that carried it.
    ///
    /// The edge cannot know whether the manager will store it. The manager
    /// stores a rotated secret only once it reads a successful
    /// acknowledgement, so it keeps the old one when the acknowledgement is
    /// lost with the connection, arrives after the manager's 10 s wait, or its
    /// database write fails. An edge that simply switched to the new secret
    /// would be refused at its next reconnect in each of those cases, and
    /// keep being refused; one that kept the old one is refused whenever the
    /// rotation did land. So it keeps both, presents the likelier one first
    /// and falls back to the other ([`Self::rejected`]) until an
    /// authentication settles it ([`Self::accepted`]).
    ///
    /// Until the acknowledgement is known to have left, the likelier one is
    /// the old secret, since the manager cannot have read it yet: the new one
    /// waits as a fallback. [`Self::rotation_acknowledged`] puts it first once
    /// the acknowledgement is written. Two steps, because the
    /// acknowledgement leaves through the session loop and a reconnect can
    /// start before the handler hears whether it did — with the new secret
    /// already a candidate, that reconnect still reaches it.
    pub(super) fn rotating(&mut self, new_secret: String) {
        // Room for it comes from the last fallback, the least likely: the
        // oldest, the manager's only if every rotation since was lost, or a
        // rotation whose acknowledgement never left. Pushed first and cut
        // after, it was the new secret that went whenever four were held.
        self.fallback_secrets.truncate(MAX_FALLBACK_SECRETS - 1);
        self.fallback_secrets.push(new_secret);
    }

    /// The acknowledgement of the rotation to `new_secret` was written: the
    /// manager has most likely stored it, so present it first, with the
    /// secret it replaced as the first fallback.
    pub(super) fn rotation_acknowledged(&mut self, new_secret: &str) {
        if self.config.node_secret.as_deref() == Some(new_secret) {
            return;
        }
        self.fallback_secrets.retain(|s| s != new_secret);
        if let Some(previous) = self.config.node_secret.replace(new_secret.to_string()) {
            self.fallback_secrets.insert(0, previous);
        }
        self.fallback_secrets.truncate(MAX_FALLBACK_SECRETS);
    }

    /// The manager refused `config.node_secret`. Present the next candidate at
    /// the next attempt.
    ///
    /// The refused secret goes to the back rather than away. A refusal is not
    /// always a verdict on the secret: the manager also answers `auth_error`
    /// while a node is locked out and when its database read fails.
    pub(super) fn rejected(&mut self) {
        if self.fallback_secrets.is_empty() {
            return;
        }
        let next = self.fallback_secrets.remove(0);
        if let Some(refused) = self.config.node_secret.replace(next) {
            self.fallback_secrets.push(refused);
        }
    }

    /// The manager accepted `config.node_secret`. Returns whether this settled
    /// which secret it holds, in which case the caller saves it: the files
    /// hold the secret presented first after the rotation, and the manager
    /// may have accepted another.
    pub(super) fn accepted(&mut self) -> bool {
        let settled = !self.fallback_secrets.is_empty();
        self.fallback_secrets.clear();
        settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn manager_config(secret: Option<&str>) -> ManagerConfig {
        ManagerConfig {
            enabled: true,
            urls: vec!["wss://manager.example:8443/ws/node".into()],
            accept_self_signed_cert: false,
            cert_fingerprint: None,
            registration_token: None,
            node_id: Some("node-1".into()),
            node_secret: secret.map(str::to_string),
        }
    }

    // ── The dial ───────────────────────────────────────────────────

    /// The families alternate, the resolver's first family leads, and each
    /// family keeps the resolver's order.
    #[test]
    fn the_families_are_interleaved_from_the_first_one_returned() {
        let v6a = addr("[2001:db8::1]:443");
        let v6b = addr("[2001:db8::2]:443");
        let v4a = addr("192.0.2.1:443");
        let v4b = addr("192.0.2.2:443");
        assert_eq!(happy_eyeballs_order(&[v6a, v6b, v4a, v4b]), [v6a, v4a, v6b, v4b]);
        assert_eq!(happy_eyeballs_order(&[v4a, v6a, v4b]), [v4a, v6a, v4b]);
        assert_eq!(happy_eyeballs_order(&[v4a, v4b]), [v4a, v4b]);
        assert!(happy_eyeballs_order(&[]).is_empty());
    }

    /// The shape behind the fix: a first address that never answers (a path
    /// that drops SYNs) and a second that is live. tokio's own connect would
    /// sit on the first for the kernel's SYN timeout; the race reaches the
    /// second a quarter of a second later.
    #[tokio::test]
    async fn a_dead_first_address_does_not_hold_the_dial() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        let dead = addr("192.0.2.1:9");
        tokio::spawn(async move {
            let _held = listener.accept().await;
            std::future::pending::<()>().await
        });

        let started = std::time::Instant::now();
        let (winner, _stream) = connect_first(&[dead, live], |a: SocketAddr| async move {
            if a == dead {
                std::future::pending::<io::Result<tokio::net::TcpStream>>().await
            } else {
                tokio::net::TcpStream::connect(a).await
            }
        })
        .await
        .expect("the live address connects");
        assert_eq!(winner, live);
        assert!(started.elapsed() < secs(2), "took {:?}", started.elapsed());
    }

    /// The same through the real connect, with TEST-NET-1 as the dead
    /// address: it either drops the SYN or is unreachable at once, and the
    /// live listener wins either way.
    #[tokio::test]
    async fn a_dead_first_address_does_not_hold_a_real_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _held = listener.accept().await;
            std::future::pending::<()>().await
        });

        let started = std::time::Instant::now();
        let (winner, _stream) =
            tokio::time::timeout(secs(10), connect_first(&[addr("192.0.2.1:9"), live], tokio::net::TcpStream::connect))
                .await
                .expect("the race finished")
                .expect("the live address connects");
        assert_eq!(winner, live);
        assert!(started.elapsed() < secs(2), "took {:?}", started.elapsed());
    }

    /// Attempts are staggered, not started all at once: the second starts a
    /// connection-attempt delay after the first.
    #[tokio::test(start_paused = true)]
    async fn the_next_attempt_waits_out_the_attempt_delay() {
        let (a, b) = (addr("192.0.2.1:443"), addr("192.0.2.2:443"));
        let started = Instant::now();
        let started_at = Mutex::new(Vec::new());
        let (winner, ()) = connect_first(&[a, b], |x: SocketAddr| {
            started_at.lock().unwrap().push((x, started.elapsed()));
            async move {
                if x == a { std::future::pending::<io::Result<()>>().await } else { Ok(()) }
            }
        })
        .await
        .unwrap();
        assert_eq!(winner, b);
        assert_eq!(*started_at.lock().unwrap(), [(a, Duration::ZERO), (b, CONNECT_ATTEMPT_DELAY)]);
    }

    /// An attempt that fails starts the next at once, without waiting out the
    /// delay — a refused or unreachable address costs nothing.
    #[tokio::test(start_paused = true)]
    async fn a_failed_attempt_starts_the_next_at_once() {
        let (a, b) = (addr("192.0.2.1:443"), addr("192.0.2.2:443"));
        let started = Instant::now();
        let (winner, ()) = connect_first(&[a, b], |x: SocketAddr| async move {
            if x == a { Err(io::Error::from(io::ErrorKind::ConnectionRefused)) } else { Ok(()) }
        })
        .await
        .unwrap();
        assert_eq!(winner, b);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    /// Every address failing reports the last failure, not a timeout.
    #[tokio::test(start_paused = true)]
    async fn every_address_failing_reports_the_last_error() {
        let (a, b) = (addr("192.0.2.1:443"), addr("192.0.2.2:443"));
        let err = connect_first(&[a, b], |x: SocketAddr| async move {
            Err::<(), _>(io::Error::new(io::ErrorKind::ConnectionRefused, format!("refused by {x}")))
        })
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), format!("refused by {b}"));
    }

    /// The exact shape of the stall behind bilbycast-edge#180: the peer
    /// completes TCP, reads the upgrade request and never answers it. `ws://`
    /// puts the stall precisely at the upgrade; over `wss://` the TLS
    /// handshake would come first.
    #[tokio::test]
    async fn a_peer_that_never_answers_the_upgrade_is_given_up_on() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (read_request, request_read) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0, "the dial hung up before finishing its request");
                request.extend_from_slice(&buf[..n]);
            }
            let _ = read_request.send(());
            // Hold the socket open and never send the 101.
            std::future::pending::<()>().await
        });

        let started = std::time::Instant::now();
        let err = dial_manager(&format!("ws://{addr}/ws/node"), None, secs(1))
            .await
            .expect_err("a peer that never answers the upgrade must not hang for ever");
        request_read.await.expect("the upgrade request reached the peer");
        assert_eq!(err.loss, LinkLoss::DialTimeout);
        assert!(err.reason.contains("TLS/WebSocket upgrade") && err.reason.contains("timed out"), "{err}");
        assert!(started.elapsed() < secs(5), "dial took {:?}", started.elapsed());
    }

    /// A peer that accepts TCP and says nothing at all stalls the TLS
    /// handshake, before any upgrade request is sent. The same deadline
    /// covers it.
    #[tokio::test]
    async fn a_peer_that_never_answers_the_tls_handshake_is_given_up_on() {
        // `main` installs the process-wide rustls provider; a test process has
        // none, and the `wss://` dial below needs one to build its config.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Hold the accepted socket open and never write a byte.
            let _held = listener.accept().await;
            std::future::pending::<()>().await
        });

        let started = std::time::Instant::now();
        let err = dial_manager(&format!("wss://{addr}/ws/node"), None, secs(1))
            .await
            .expect_err("a peer that never answers must not hang for ever");
        assert_eq!(err.loss, LinkLoss::DialTimeout);
        assert!(err.reason.contains("timed out"), "{err}");
        assert!(started.elapsed() < secs(5), "dial took {:?}", started.elapsed());
    }

    /// A refused connection must still come back as an ordinary error naming
    /// the TCP stage, not as a timeout: the retry path and the operator both
    /// depend on telling the two apart.
    #[tokio::test]
    async fn a_refused_dial_reports_the_connection_error() {
        // Bind and drop, so the port is almost certainly closed.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let err = dial_manager(&format!("wss://{addr}/ws/node"), None, secs(10))
            .await
            .expect_err("a closed port cannot connect");
        assert_eq!(err.loss, LinkLoss::DialFailed);
        assert!(err.reason.starts_with("TCP connect to 127.0.0.1 (1 address) failed"), "{err}");
    }

    /// An IPv6 literal is written in brackets, which a lookup would refuse.
    #[test]
    fn an_ipv6_literal_needs_no_lookup() {
        let target = DialTarget::parse("wss://[::1]:8443/ws/node").unwrap();
        assert_eq!(target.literal, Some("::1".parse().unwrap()));
        assert_eq!(target.port, 8443);
        let target = DialTarget::parse("wss://manager.example/ws/node").unwrap();
        assert_eq!((target.host.as_str(), target.port, target.literal), ("manager.example", 443, None));
    }

    /// A credential in the URL never reaches an event.
    #[test]
    fn a_shown_url_drops_its_credentials() {
        assert_eq!(
            shown_url("wss://user:pass@manager.example:8443/ws/node?token=abc#x"),
            "wss://manager.example:8443/ws/node"
        );
    }

    // ── The handshake ──────────────────────────────────────────────

    /// Once a session authenticates it may run for days, so the backstop must
    /// never fire behind it.
    #[tokio::test]
    async fn the_backstop_stays_quiet_once_authenticated() {
        let (tx, rx) = oneshot::channel();
        tx.send(()).unwrap();
        let fired =
            tokio::time::timeout(Duration::from_millis(250), handshake_backstop(rx, Duration::from_millis(50))).await;
        assert!(fired.is_err(), "the backstop fired on an authenticated session");
    }

    /// And it must fire when authentication never arrives.
    #[tokio::test]
    async fn the_backstop_fires_when_auth_never_arrives() {
        let (_tx, rx) = oneshot::channel::<()>();
        tokio::time::timeout(secs(2), handshake_backstop(rx, Duration::from_millis(50)))
            .await
            .expect("the backstop should have fired when auth never completed");
    }

    // ── The cadence ────────────────────────────────────────────────

    /// A manager that has never pinged has told us nothing about its cadence,
    /// and one that predates the keepalive never will. The watchdog must not
    /// fire, however long the quiet.
    #[test]
    fn the_watchdog_is_unarmed_until_the_first_ping() {
        let t0 = Instant::now();
        let c = InboundCadence::new(t0);
        assert_eq!(c.silence_limit(), None);
        assert_eq!(c.dead_for(t0 + secs(3600)), None);
    }

    /// One ping arms it, taking the time since the session started as the
    /// first gap — here the manager's 15 s keepalive plus its own setup.
    #[test]
    fn the_first_ping_arms_the_watchdog() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(17));
        assert_eq!(c.silence_limit(), Some(SILENCE_FLOOR));
        assert_eq!(c.dead_for(t0 + secs(17) + secs(89)), None);
        assert!(c.dead_for(t0 + secs(17) + secs(91)).is_some());
    }

    /// The keepalive is configurable to 300 s. A fixed limit would drop every
    /// healthy session on such a manager; a learned one does not.
    #[test]
    fn a_slow_cadence_scales_the_limit() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(300));
        c.on_ping(t0 + secs(600));
        assert_eq!(c.silence_limit(), Some(secs(1200)));
        assert_eq!(c.dead_for(t0 + secs(600) + secs(1199)), None);
    }

    /// One manager stall does not move the limit. With the widest gap kept, a
    /// single 85 s stall at a 15 s cadence raised the limit to 340 s for the
    /// rest of the session.
    #[test]
    fn the_median_ignores_one_long_gap() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        let mut t = t0;
        for gap in [15, 15, 15, 85, 15, 15] {
            t += secs(gap);
            c.on_ping(t);
        }
        assert_eq!(c.silence_limit(), Some(SILENCE_FLOOR));
    }

    /// Pings that arrive back to back do not pull the limit down: below the
    /// floor at any cadence, and not off a slow cadence's real interval.
    #[test]
    fn back_to_back_pings_do_not_shrink_the_limit() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        for _ in 0..CADENCE_GAPS {
            c.on_ping(t0);
        }
        assert_eq!(c.silence_limit(), Some(SILENCE_FLOOR), "no gap at all is held to the floor");

        let mut c = InboundCadence::new(t0);
        let mut t = t0;
        for _ in 0..5 {
            t += secs(300);
            c.on_ping(t);
        }
        for _ in 0..3 {
            c.on_ping(t);
        }
        assert_eq!(c.silence_limit(), Some(secs(1200)));
    }

    /// No gap counts as longer than the manager's largest keepalive: a
    /// longer one is a stall, not a cadence.
    #[test]
    fn a_gap_is_clamped_to_the_largest_keepalive() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(5000));
        assert_eq!(c.silence_limit(), Some(MAX_KEEPALIVE * SILENCE_PINGS));
    }

    /// Any frame keeps the session alive, not only pings — a manager busy
    /// sending commands is plainly there.
    #[test]
    fn any_frame_resets_the_silence() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(15));
        c.on_frame(t0 + secs(80));
        assert_eq!(c.dead_for(t0 + secs(160)), None);
        assert!(c.dead_for(t0 + secs(171)).is_some());
    }

    /// Only a ping teaches the cadence. A pong or a command proves the
    /// manager is there, but says nothing about how often it pings.
    #[test]
    fn only_a_ping_teaches_the_cadence() {
        let t0 = Instant::now();
        let cadence = Mutex::new(InboundCadence::new(t0));
        note_inbound(&cadence, &Message::Pong(Bytes::new()), t0 + secs(1));
        note_inbound(&cadence, &Message::Text("{}".into()), t0 + secs(2));
        assert_eq!(cadence.lock().unwrap().silence_limit(), None);
        assert_eq!(cadence.lock().unwrap().last_frame, t0 + secs(2));
        note_inbound(&cadence, &Message::Ping(Bytes::new()), t0 + secs(15));
        assert!(cadence.lock().unwrap().silence_limit().is_some());
    }

    // ── The watchdog and the race ──────────────────────────────────

    /// The watchdog resolves on a manager already silent past its limit.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_fires_on_a_silent_manager() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(15));
        let cadence = Mutex::new(c);
        let (silent, limit) = manager_silence_watchdog(&cadence).await;
        assert_eq!(limit, SILENCE_FLOOR);
        assert!(silent > limit);
        assert!(Instant::now() <= t0 + secs(15) + SILENCE_FLOOR + SILENCE_CHECK);
    }

    /// And it stays quiet for as long as frames keep arriving, across many
    /// checks: a session can last for days.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_stays_quiet_while_the_manager_talks() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(15));
        let cadence = Mutex::new(c);
        let talking = async {
            for _ in 0..40 {
                tokio::time::sleep(secs(15)).await;
                note_inbound(&cadence, &Message::Ping(Bytes::new()), Instant::now());
            }
        };
        tokio::select! {
            () = talking => {}
            _ = manager_silence_watchdog(&cadence) => panic!("the watchdog fired on a live session"),
        }
    }

    /// The race is what drops a session stuck in a write: the session never
    /// finishes, and the watchdog ends it anyway.
    #[tokio::test(start_paused = true)]
    async fn the_race_drops_a_stuck_session_on_silence() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(15));
        let cadence = Mutex::new(c);
        let err = race_session(std::future::pending(), std::future::pending(), &cadence).await.unwrap_err();
        assert_eq!(err.loss, LinkLoss::ManagerSilent);
        assert!(err.reason.contains("Nothing heard"), "{err}");
    }

    /// A session that ends on its own ends the race with it: a close is an
    /// ordinary end, a loss is reported as itself.
    #[tokio::test(start_paused = true)]
    async fn the_race_reports_how_the_session_ended() {
        let cadence = Mutex::new(InboundCadence::new(Instant::now()));
        let closed = race_session(async { Err::<(), _>(SessionEnd::Closed) }, std::future::pending(), &cadence).await;
        assert!(closed.is_ok());
        let lost = race_session(
            async { Err::<(), _>(SessionEnd::Lost(LinkError::new(LinkLoss::ConnectionError, "reset"))) },
            std::future::pending(),
            &cadence,
        )
        .await
        .unwrap_err();
        assert_eq!(lost.loss, LinkLoss::ConnectionError);
    }

    /// The reader stamps every frame, keeps the pings to itself and hands on
    /// the rest in order, ending at the close.
    #[tokio::test(start_paused = true)]
    async fn the_reader_stamps_every_frame_and_forwards_what_the_loop_acts_on() {
        let frames: Vec<Result<Message, tungstenite::Error>> = vec![
            Ok(Message::Ping(Bytes::new())),
            Ok(Message::Text("one".into())),
            Ok(Message::Pong(Bytes::new())),
            Ok(Message::Binary(Bytes::from_static(b"two"))),
            Ok(Message::Close(None)),
            Ok(Message::Text("after the close".into())),
        ];
        let frames = futures_util::stream::iter(frames);
        let cadence = Mutex::new(InboundCadence::new(Instant::now()));
        let (tx, mut rx) = mpsc::channel(INBOUND_QUEUE);
        let reader = read_inbound(frames, &cadence, tx);
        let forwarded = async {
            let mut got = Vec::new();
            while let Some(frame) = rx.recv().await {
                let last = matches!(frame, Inbound::Closed);
                got.push(frame);
                if last {
                    break;
                }
            }
            got
        };
        let got = tokio::select! {
            never = reader => match never {},
            got = forwarded => got,
        };
        assert!(
            matches!(&got[..], [Inbound::Text(t), Inbound::Binary(b), Inbound::Closed] if t.as_str() == "one" && &b[..] == b"two"),
            "{got:?}"
        );
        assert!(cadence.lock().unwrap().silence_limit().is_some(), "the ping armed the watchdog");
    }

    /// A sink whose writes never complete, or fail at once.
    enum TestSink {
        Stalled,
        Broken,
    }

    impl Sink<Message> for TestSink {
        type Error = tungstenite::Error;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            match *self {
                TestSink::Stalled => std::task::Poll::Pending,
                TestSink::Broken => std::task::Poll::Ready(Err(tungstenite::Error::ConnectionClosed)),
            }
        }
        fn start_send(self: std::pin::Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            self.poll_ready(cx)
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A write that never completes ends the session after the stall limit,
    /// with nothing needed from the cadence — the window before the first
    /// ping, and a manager that never pings, are covered by this alone.
    #[tokio::test(start_paused = true)]
    async fn a_write_that_never_completes_ends_the_session() {
        let started = Instant::now();
        match send_bounded(&mut TestSink::Stalled, Message::Text("{}".into())).await {
            Err(SessionEnd::Lost(e)) => assert_eq!(e.loss, LinkLoss::WriteStalled),
            other => panic!("expected a write stall, got {other:?}"),
        }
        assert_eq!(started.elapsed(), WRITE_STALL_LIMIT);
    }

    /// A write that fails under a broken connection is an ordinary end.
    #[tokio::test]
    async fn a_failed_write_ends_the_session_as_a_close_does() {
        let ended = send_bounded(&mut TestSink::Broken, Message::Text("{}".into())).await;
        assert!(matches!(ended, Err(SessionEnd::Closed)), "{ended:?}");
    }

    // ── The credentials ────────────────────────────────────────────

    /// Two rotations, both acknowledged, the second never stored: the manager
    /// holds the first. Every secret issued since the last accepted one stays
    /// a candidate, newest first, and a refusal moves one to the back rather
    /// than dropping it.
    #[test]
    fn every_secret_since_the_last_accepted_one_is_tried_newest_first() {
        let mut creds = LinkCredentials::new(manager_config(Some("s0")));
        for s in ["s1", "s2"] {
            creds.rotating(s.into());
            creds.rotation_acknowledged(s);
        }

        let mut presented = Vec::new();
        for _ in 0..4 {
            presented.push(creds.config.node_secret.clone().unwrap());
            creds.rejected();
        }
        assert_eq!(presented, ["s2", "s1", "s0", "s2"]);

        assert!(creds.accepted(), "accepting settles which secret the manager holds");
        assert!(creds.fallback_secrets.is_empty());
        assert!(!creds.accepted(), "nothing was open the second time");
    }

    /// Until its acknowledgement is known to have left, a rotation keeps the
    /// old secret first and the new one as a fallback, so a reconnect that
    /// starts in between still reaches whichever the manager holds.
    #[test]
    fn an_unacknowledged_rotation_keeps_the_old_secret_first() {
        let mut creds = LinkCredentials::new(manager_config(Some("old")));
        creds.rotating("new".into());
        assert_eq!(creds.config.node_secret.as_deref(), Some("old"));
        creds.rejected();
        assert_eq!(creds.config.node_secret.as_deref(), Some("new"));
    }

    /// The manager locks a node out after five refusals in 60 s. However many
    /// rotations go unconfirmed, the edge reaches the last candidate on its
    /// fifth attempt at the latest.
    #[test]
    fn the_candidates_stay_inside_the_managers_lockout() {
        let mut creds = LinkCredentials::new(manager_config(Some("s0")));
        for i in 1..=10 {
            let s = format!("s{i}");
            creds.rotating(s.clone());
            creds.rotation_acknowledged(&s);
        }
        assert_eq!(creds.config.node_secret.as_deref(), Some("s10"));
        assert_eq!(creds.fallback_secrets, ["s9", "s8", "s7", "s6"]);
        let candidates = 1 + creds.fallback_secrets.len();
        assert!(candidates <= 5, "{candidates} candidates would run into the lockout");

        // Unacknowledged rotations stay inside it too, and each is still a
        // candidate when it is made.
        for i in 11..=20 {
            let s = format!("s{i}");
            creds.rotating(s.clone());
            assert_eq!(creds.fallback_secrets.last(), Some(&s), "{:?}", creds.fallback_secrets);
            assert!(creds.fallback_secrets.len() <= MAX_FALLBACK_SECRETS);
        }
        assert_eq!(creds.fallback_secrets, ["s9", "s8", "s7", "s20"]);
    }

    /// A registration replaces whatever was there, fallbacks included.
    #[test]
    fn a_registration_settles_the_credentials() {
        let mut creds = LinkCredentials::new(ManagerConfig {
            registration_token: Some("one-time".into()),
            node_id: None,
            ..manager_config(None)
        });
        creds.rotating("stray".into());
        creds.registered("node-9".into(), "issued".into());
        assert_eq!(creds.config.registration_token, None);
        assert_eq!(creds.config.node_id.as_deref(), Some("node-9"));
        assert_eq!(creds.config.node_secret.as_deref(), Some("issued"));
        assert!(!creds.accepted(), "nothing left to settle");
    }

    /// Each kind of loss has its own code and its own message, so the
    /// manager's coalescing keeps them apart.
    #[test]
    fn every_kind_of_loss_is_told_apart() {
        let all = [
            LinkLoss::Settings,
            LinkLoss::DialFailed,
            LinkLoss::DialTimeout,
            LinkLoss::HandshakeTimeout,
            LinkLoss::AuthRejected,
            LinkLoss::Protocol,
            LinkLoss::ConnectionError,
            LinkLoss::ManagerSilent,
            LinkLoss::WriteStalled,
        ];
        let codes: std::collections::HashSet<_> = all.iter().map(|l| l.error_code()).collect();
        let messages: std::collections::HashSet<_> = all.iter().map(|l| l.event_message()).collect();
        assert_eq!(codes.len(), all.len());
        assert_eq!(messages.len(), all.len());
    }
}
