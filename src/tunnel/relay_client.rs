// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Relay client — manages the QUIC connection to a bilbycast-relay server.
//!
//! The relay is stateless — no authentication needed. The edge connects and
//! immediately binds tunnels by UUID. All tunnel data is encrypted end-to-end
//! between edge nodes; the relay only sees ciphertext.

use std::time::Duration;

use anyhow::Result;
use quinn::Connection;
use tokio::net::lookup_host;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::config::{TunnelDirection, TunnelProtocol};
use super::protocol::{
    self, EdgeMessage, ParsedMessage, RelayDirection, RelayMessage, RelayProtocol, ALPN_RELAY,
    TUNNEL_PROTOCOL_VERSION,
};
use super::quic;
use crate::manager::events::{EventSender, EventSeverity, category};

/// How long a tunnel waits for its peer before it looks for it elsewhere. One
/// with two relays binds the other as well ([`other_relay`]), which is how two
/// edges meet that have lost different paths. One with a single relay lets go of
/// it and starts over.
const WAITING_TIMEOUT: Duration = Duration::from_secs(10);

/// How long both sides have to be on the other relay before a tunnel moves
/// there. Both edges may be trying each other's relay at that moment, and would
/// go past each other, and again, if each went to where the other has just been.
/// A tunnel that waits on its backup goes to its primary at once, if that is
/// near ([`OtherRelay::goes`]).
const SETTLE: Duration = Duration::from_secs(1);

/// How long we wait for the QUIC handshake to a relay to complete before
/// stepping to the next relay. Bounded so a dead primary cannot stall failover
/// behind Quinn's default idle timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// Marker substring used to distinguish a waiting-timeout failure from a real
/// connect/bind error. The failover loop checks for this to avoid logging a
/// hard error when the peer of a tunnel with one relay is simply not there.
const WAITING_TIMEOUT_MSG: &str = "waiting-timeout-stepping-forward";

/// How often we ping the relay on the control stream of a tunnel that is up.
const PING_INTERVAL: Duration = Duration::from_secs(15);

/// How long the message on a stream the relay opened may take to arrive. The
/// streams are read in turn, so one that stalls holds the next up for as long.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long we wait before binding a side again that the relay has removed.
/// A tunnel that is being deleted has its `delete_tunnel` on the way, and one
/// removed again and again is not bound any faster than [`REBIND_MAX`]: the
/// wait doubles up to it, and starts over once a bind has stood that long.
const REBIND_MIN: Duration = Duration::from_secs(1);
const REBIND_MAX: Duration = Duration::from_secs(60);

/// How long the relay may take to answer that bind. It may have a peer to
/// notify first, which it gives 5 s.
const REBIND_ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

/// How much of a relay's reason is kept, in characters. It goes into the log
/// and into events.
const MAX_REASON: usize = 200;

/// How many changes of a tunnel's fortune are reported at once, and then one
/// in how long: a peer that comes and goes raises no more events than this.
const REPORT_BURST: u32 = 4;
const REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// How long a tunnel that is up on its backup stays there before it tries the
/// primary, how long after a try that came to nothing it tries again, and how
/// often it asks while it waits there whether the primary is still near enough.
const FAILBACK_INTERVAL: Duration = Duration::from_secs(60);

/// How long a relay may take to have read that we leave it.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a tunnel with two relays binds its side again while it waits for
/// its peer ([`RelayLink::bind_again`]).
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// The waits above that a test has no time for.
#[derive(Debug, Clone)]
pub struct Timing {
    pub connect: Duration,
    pub waiting: Duration,
    pub settle: Duration,
    pub failback: Duration,
    pub refresh: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            waiting: WAITING_TIMEOUT,
            settle: SETTLE,
            failback: FAILBACK_INTERVAL,
            refresh: REFRESH_INTERVAL,
        }
    }
}

/// State of the relay tunnel connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayTunnelState {
    Connecting,
    Waiting,
    Ready,
    Down,
}

impl std::fmt::Display for RelayTunnelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connecting => write!(f, "connecting"),
            Self::Waiting => write!(f, "waiting"),
            Self::Ready => write!(f, "ready"),
            Self::Down => write!(f, "down"),
        }
    }
}

/// Parameters for establishing a relay tunnel.
///
/// `relay_addrs` is an ordered list (primary first, optional backup second).
/// The failover loop in [`connect_with_retry`] walks this list and publishes
/// the index of the currently-active relay on `active_idx_tx`.
pub struct RelayTunnelParams {
    pub tunnel_id: Uuid,
    /// Ordered list of relay addresses (`host:port` strings — IP literals
    /// or DNS names). Must contain at least one entry. A second entry
    /// enables automatic primary↔backup failover. DNS resolution happens
    /// at connect time via `tokio::net::lookup_host`, so an IP change
    /// behind a hostname is picked up on the next connect attempt
    /// without a tunnel reconfig.
    pub relay_addrs: Vec<String>,
    pub direction: TunnelDirection,
    pub protocol: TunnelProtocol,
    /// Optional manager node_id to identify this edge to the relay.
    pub edge_id: Option<String>,
    /// Optional bind secret for relay tunnel authentication.
    pub tunnel_bind_secret: Option<String>,
    /// Maximum RTT increase (ms) tolerated when failing back from backup to
    /// primary ([`other_relay`]).
    pub failback_rtt_gate_ms: u32,
    pub timing: Timing,
}

/// The relay's reason for refusing our bind.
#[derive(Debug)]
struct BindRejected(String);

impl std::fmt::Display for BindRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BindRejected {}

/// A message from the relay, by where it came.
enum Received {
    /// On the control stream: what answers a message of ours.
    Control(RelayMessage),
    /// On a stream of its own: what has become of the tunnel since.
    Notice(RelayMessage),
    /// The control stream has ended, and with it the relay's side of the
    /// connection.
    Ended(anyhow::Error),
}

/// What the relay says of our tunnel.
#[derive(Debug)]
enum News {
    /// Both sides are bound.
    Ready,
    /// Ours is bound, the other is not yet.
    Waiting,
    /// The other side is gone. Ours is still bound.
    PeerGone(String),
    /// Our side was removed.
    Removed(String),
    /// Our bind was refused.
    Refused(String),
    /// Nothing, and our side may be bound no more ([`RelayLink::bind_again`]).
    Stale,
}

impl News {
    /// A `TunnelDown` that is the answer to a bind.
    fn refused(reason: &str) -> Self {
        Self::Refused(reason.chars().take(MAX_REASON).collect())
    }

    /// A `TunnelDown` that is not. One with a reason we do not know is taken to
    /// be about our own side: binding it again does no harm if it was not.
    fn down(reason: &str) -> Self {
        match reason {
            protocol::PEER_DISCONNECTED | protocol::PEER_UNBOUND => Self::PeerGone(reason.into()),
            _ => Self::Removed(reason.chars().take(MAX_REASON).collect()),
        }
    }
}

/// What others have of a [`RelayLink`].
#[derive(Clone)]
pub struct LinkHandle {
    conn: Connection,
    /// Has the link bind our side again ([`RelayLink::bind_again`]). One that is up
    /// does so only if told `true`.
    again: mpsc::Sender<bool>,
}

/// A connection to a relay, with our side of one tunnel bound on it.
pub struct RelayLink {
    handle: LinkHandle,
    again: mpsc::Receiver<bool>,
    control: quinn::SendStream,
    tunnel_id: Uuid,
    bind: EdgeMessage,
    received: mpsc::Receiver<Received>,
    /// Read the relay's streams into `received`, and end with the link.
    _readers: JoinSet<()>,
    /// When we last bound, and whether that has not been answered yet.
    bound_at: Instant,
    unanswered: bool,
    /// The last notice read ahead of that answer. The relay sends a notice
    /// about a side it has bound, so the notice is the later of the two.
    early: Option<News>,
    /// What the relay's news has come to.
    state: RelayTunnelState,
    /// The peer has been there.
    has_met: bool,
    /// How often we bind again while it is not, and when next.
    refresh_interval: Option<Duration>,
    refresh: Option<Instant>,
}

impl RelayLink {
    /// Connect to a relay.
    ///
    /// `relay_addr` is a `host:port` string. It is resolved inside the
    /// [`CONNECT_TIMEOUT`] window, so a slow resolver cannot hold the caller and
    /// a DNS change is picked up on every attempt without a config reload. The
    /// first returned `SocketAddr` is used — multi-record round-robin is not
    /// implemented at this layer (operators wanting that can list multiple
    /// addresses in `relay_addrs`).
    async fn dial(params: &RelayTunnelParams, relay_addr: &str) -> Result<Self> {
        let connect = async {
            let endpoint = quic::make_client_endpoint(ALPN_RELAY)?;
            let resolved = lookup_host(relay_addr)
                .await
                .map_err(|e| anyhow::anyhow!("resolve relay '{relay_addr}': {e}"))?
                .next()
                .ok_or_else(|| anyhow::anyhow!("relay '{relay_addr}' resolved to no addresses"))?;
            let conn = quic::connect(&endpoint, resolved, "relay").await?;
            Ok::<_, anyhow::Error>((resolved, conn))
        };
        let within = params.timing.connect;
        let (resolved, conn) = timeout(within, connect)
            .await
            .map_err(|_| anyhow::anyhow!("connect to relay {relay_addr} timed out after {within:?}"))??;
        tracing::info!(
            tunnel_id = %params.tunnel_id,
            relay = %relay_addr,
            resolved = %resolved,
            "Connected to relay"
        );
        Self::open(conn, params).await
    }

    /// Open the control stream, and introduce ourselves on it.
    async fn open(conn: Connection, params: &RelayTunnelParams) -> Result<Self> {
        let (mut control, control_recv) = conn.open_bi().await?;

        // Send protocol version handshake (new relays respond with HelloAck; old relays ignore it)
        protocol::write_message(
            &mut control,
            &EdgeMessage::Hello {
                protocol_version: TUNNEL_PROTOCOL_VERSION,
                software_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await?;

        // Identify this edge to the relay (enables topology visualization in the manager)
        if let Some(ref id) = params.edge_id {
            let identify = EdgeMessage::Identify { edge_id: id.clone() };
            protocol::write_message(&mut control, &identify).await?;
        }

        let (received_tx, received) = mpsc::channel(16);
        let mut readers = JoinSet::new();
        readers.spawn(read_control(control_recv, received_tx.clone()));
        readers.spawn(read_notices(conn.clone(), received_tx));
        let (again_tx, again) = mpsc::channel(4);
        Ok(Self {
            handle: LinkHandle { conn, again: again_tx },
            again,
            control,
            tunnel_id: params.tunnel_id,
            bind: bind_message(params),
            received,
            _readers: readers,
            bound_at: Instant::now(),
            unanswered: false,
            early: None,
            state: RelayTunnelState::Connecting,
            has_met: false,
            refresh_interval: (params.relay_addrs.len() > 1).then_some(params.timing.refresh),
            refresh: None,
        })
    }

    /// Bind our side of the tunnel.
    async fn bind(&mut self) -> Result<()> {
        self.bound_at = Instant::now();
        self.unanswered = true;
        protocol::write_message(&mut self.control, &self.bind).await
    }

    async fn unbind(&mut self) -> Result<()> {
        let unbind = EdgeMessage::TunnelUnbind { tunnel_id: self.tunnel_id };
        protocol::write_message(&mut self.control, &unbind).await
    }

    /// Bind our side again, in case another bind of it has taken it away: that of
    /// a cloned edge, or our own on the other relay if the two are one relay's
    /// addresses. The relay says nothing of it, nor when the other has gone and
    /// nobody has the side.
    ///
    /// Relays up to v0.20.0 count the binds of a connection, less its unbinds, and
    /// refuse too many. Nothing is lost by unbinding first while the peer is away.
    async fn bind_again(&mut self) -> Result<()> {
        if self.state == RelayTunnelState::Waiting {
            self.unbind().await?;
        }
        self.bind().await
    }

    /// The connection, for what the tunnel carries.
    pub fn conn(&self) -> &Connection {
        &self.handle.conn
    }

    /// What [`other_relay`] needs of the link the tunnel is on.
    pub fn handle(&self) -> &LinkHandle {
        &self.handle
    }

    /// Leave the relay, for the link that the tunnel has moved to.
    pub fn leave_for(self, other: &Self, after: Duration) {
        let later = self.bound_at > other.bound_at;
        self.let_go(other.handle.clone(), after, later);
    }

    /// Leave the relay, and then have the link that the tunnel is on bind again. If
    /// that is up, it is to only if our bind is the `later` and may have taken the
    /// side from it. It has not if it was answered, and not with a `TunnelReady`,
    /// nor has any come since: that is another relay.
    fn let_go(self, active: LinkHandle, after: Duration, later: bool) {
        let if_up = self.unanswered || later && self.has_met;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                self.leave(after).await;
                let _ = active.again.try_send(if_up);
            });
        }
    }

    /// Let go of our side, so that the relay does not list it any more, and of
    /// the connection. The streams that are open on it keep it until they end.
    async fn leave(mut self, after: Duration) {
        tokio::time::sleep(after).await;
        let left = async {
            self.unbind().await?;
            // A connection that is closed delivers nothing more.
            self.control.finish()?;
            self.control.stopped().await?;
            Ok::<_, anyhow::Error>(())
        };
        if !matches!(timeout(LEAVE_TIMEOUT, left).await, Ok(Ok(()))) {
            tracing::debug!(tunnel_id = %self.tunnel_id, "The relay was not told that we leave it");
        }
    }

    /// The next news of the tunnel, or why there will be none. It loses nothing
    /// if it is dropped before it is done.
    async fn news(&mut self) -> Result<News> {
        loop {
            if !self.unanswered
                && let Some(news) = self.early.take()
            {
                return Ok(self.taken_in(news));
            }
            let refresh = self.refresh;
            let stale = async move {
                match refresh {
                    Some(at) => sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let news = tokio::select! {
                biased;
                received = self.received.recv() => match received {
                    Some(Received::Control(msg)) => self.answer(msg),
                    Some(Received::Notice(msg)) => match self.notice(msg) {
                        Some(news) if self.unanswered => {
                            self.early = Some(news);
                            None
                        }
                        news => news,
                    },
                    Some(Received::Ended(e)) => return Err(e),
                    None => anyhow::bail!("relay streams are no longer read"),
                },
                Some(if_up) = self.again.recv() => {
                    (if_up || self.state != RelayTunnelState::Ready).then_some(News::Stale)
                }
                _ = stale => Some(News::Stale),
            };
            if let Some(news) = news {
                return Ok(self.taken_in(news));
            }
        }
    }

    fn taken_in(&mut self, news: News) -> News {
        self.state = match news {
            News::Ready => RelayTunnelState::Ready,
            News::Waiting | News::PeerGone(_) => RelayTunnelState::Waiting,
            News::Removed(_) | News::Refused(_) => RelayTunnelState::Down,
            News::Stale => self.state,
        };
        self.has_met |= self.state == RelayTunnelState::Ready;
        self.refresh = match self.state {
            RelayTunnelState::Waiting => self.refresh_interval.map(|wait| Instant::now() + wait),
            _ => None,
        };
        news
    }

    /// What a message on the control stream says of our tunnel.
    fn answer(&mut self, msg: RelayMessage) -> Option<News> {
        match msg {
            RelayMessage::TunnelReady { tunnel_id } if tunnel_id == self.tunnel_id => {
                self.unanswered = false;
                Some(News::Ready)
            }
            RelayMessage::TunnelWaiting { tunnel_id } if tunnel_id == self.tunnel_id => {
                self.unanswered = false;
                Some(News::Waiting)
            }
            RelayMessage::TunnelDown { tunnel_id, reason } if tunnel_id == self.tunnel_id => {
                match std::mem::take(&mut self.unanswered) {
                    true => Some(News::refused(&reason)),
                    false => Some(News::down(&reason)),
                }
            }
            RelayMessage::HelloAck { protocol_version, software_version } => {
                tracing::info!(
                    tunnel_id = %self.tunnel_id,
                    "Relay hello_ack: protocol v{protocol_version}, software {software_version}"
                );
                if protocol_version != TUNNEL_PROTOCOL_VERSION {
                    tracing::warn!(
                        "Tunnel protocol version mismatch: edge={TUNNEL_PROTOCOL_VERSION}, relay={protocol_version}"
                    );
                }
                None
            }
            RelayMessage::Pong => None,
            other => self.not_ours(other),
        }
    }

    /// What a message on a stream of its own says of our tunnel.
    fn notice(&self, msg: RelayMessage) -> Option<News> {
        match msg {
            RelayMessage::TunnelReady { tunnel_id } if tunnel_id == self.tunnel_id => {
                Some(News::Ready)
            }
            RelayMessage::TunnelDown { tunnel_id, reason } if tunnel_id == self.tunnel_id => {
                Some(News::down(&reason))
            }
            other => self.not_ours(other),
        }
    }

    fn not_ours(&self, msg: RelayMessage) -> Option<News> {
        tracing::debug!(tunnel_id = %self.tunnel_id, "Relay message: {msg:?}");
        None
    }

    /// See to the bind of a tunnel that is up: report what becomes of the
    /// tunnel, and bind our side again if it is removed. If that is refused, or
    /// the relay is heard no more, the connection is closed, which ends the
    /// forwarder on it, and this returns; the caller then connects anew.
    pub async fn hold(
        &mut self,
        relay_addr: &str,
        state_tx: &watch::Sender<RelayTunnelState>,
        event_sender: &EventSender,
    ) {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        let mut reports = Reports::new(Instant::now());
        let mut due: Option<Due> = None;
        let mut delay = RebindDelay(REBIND_MIN);
        let mut bound_at = Instant::now();
        let why = loop {
            let timer = async {
                match due {
                    Some(Due::Bind(at) | Due::Answer(at)) => sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                news = self.news() => match news {
                    // Not about a side of ours: it is removed, and not bound
                    // again yet. The relay may have had several things to say.
                    Ok(_) if matches!(due, Some(Due::Bind(_))) => {}
                    Ok(News::Stale) => if let Err(e) = self.bind_again().await {
                        break format!("bind not sent: {e}");
                    },
                    Ok(news @ (News::Ready | News::Waiting)) => {
                        if due.take().is_some() {
                            bound_at = Instant::now();
                        }
                        if let News::Ready = news {
                            tracing::info!(tunnel_id = %self.tunnel_id, "Tunnel ready");
                            state_tx.send_replace(RelayTunnelState::Ready);
                            reports.trouble = None;
                        } else if *state_tx.borrow() != RelayTunnelState::Waiting {
                            tracing::info!(tunnel_id = %self.tunnel_id, "Waiting for peer to bind");
                            state_tx.send_replace(RelayTunnelState::Waiting);
                        }
                    }
                    Ok(News::PeerGone(reason)) => {
                        tracing::info!(tunnel_id = %self.tunnel_id, reason = %reason, "Tunnel peer gone");
                        state_tx.send_replace(RelayTunnelState::Waiting);
                        reports.trouble = Some(Trouble::PeerGone(reason));
                    }
                    Ok(News::Removed(reason)) => {
                        let wait = delay.next(bound_at.elapsed());
                        tracing::warn!(
                            tunnel_id = %self.tunnel_id, reason = %reason,
                            "Tunnel removed on relay, binding again in {wait:?}"
                        );
                        due = Some(Due::Bind(Instant::now() + wait));
                        state_tx.send_replace(RelayTunnelState::Down);
                        reports.trouble = Some(Trouble::Removed(reason));
                    }
                    Ok(News::Refused(reason)) => break format!("bind refused: {reason}"),
                    Err(e) => break format!("control stream ended: {e}"),
                },
                _ = timer => {
                    if let Some(Due::Answer(_)) = due {
                        break "the relay did not answer our bind".to_string();
                    }
                    if let Err(e) = self.bind().await {
                        break format!("bind not sent: {e}");
                    }
                    due = Some(Due::Answer(Instant::now() + REBIND_ANSWER_TIMEOUT));
                }
                _ = ping.tick() => {
                    if let Err(e) = protocol::write_message(&mut self.control, &EdgeMessage::Ping).await {
                        break format!("ping not sent: {e}");
                    }
                }
            }
            if let Some(report) = reports.due(Instant::now()) {
                let (severity, message, details) = trouble_event(report, relay_addr);
                event_sender.emit_flow_with_details(
                    severity,
                    category::TUNNEL,
                    message,
                    &self.tunnel_id.to_string(),
                    details,
                );
            }
        };
        tracing::warn!(tunnel_id = %self.tunnel_id, relay = %relay_addr, "Leaving the relay connection: {why}");
        self.conn().close(0u32.into(), b"");
    }
}

/// Read the control stream until it ends.
async fn read_control(mut control: quinn::RecvStream, received: mpsc::Sender<Received>) {
    loop {
        let msg = match protocol::read_message_resilient(&mut control).await {
            Ok(ParsedMessage::Known(msg)) => Received::Control(msg),
            Ok(ParsedMessage::Unknown { msg_type }) => {
                tracing::debug!("Unknown relay message type '{msg_type}', ignoring");
                continue;
            }
            Err(e) => {
                let _ = received.send(Received::Ended(e)).await;
                return;
            }
        };
        if received.send(msg).await.is_err() {
            return;
        }
    }
}

/// Read the streams the relay opens, for as long as the connection lasts. Each
/// carries one message, and they are taken in the order they were opened. Left
/// unread they would also use up what we allow the relay of them.
async fn read_notices(conn: Connection, received: mpsc::Sender<Received>) {
    while let Ok(mut stream) = conn.accept_uni().await {
        let read = protocol::read_message_resilient(&mut stream);
        match timeout(NOTICE_TIMEOUT, read).await {
            Ok(Ok(ParsedMessage::Known(msg))) => {
                if received.send(Received::Notice(msg)).await.is_err() {
                    return;
                }
            }
            Ok(Ok(ParsedMessage::Unknown { msg_type })) => {
                tracing::debug!("Unknown relay notice type '{msg_type}', ignoring");
            }
            // The relay opens one before it looks whether there is still
            // anything to say.
            Ok(Err(e)) => tracing::debug!("Relay notice without a message: {e}"),
            Err(_) => tracing::debug!("Relay notice did not arrive in {NOTICE_TIMEOUT:?}"),
        }
    }
}

/// What there is to come of a side of ours that was removed.
#[derive(Clone, Copy)]
enum Due {
    /// Our bind of it again.
    Bind(Instant),
    /// The relay's answer to that, by then.
    Answer(Instant),
}

/// The wait before a removed side is bound again: see [`REBIND_MIN`].
struct RebindDelay(Duration);

impl RebindDelay {
    /// The next wait, after a bind that stood for `stood`.
    fn next(&mut self, stood: Duration) -> Duration {
        if stood >= REBIND_MAX {
            self.0 = REBIND_MIN;
        }
        let wait = self.0;
        self.0 = (wait * 2).min(REBIND_MAX);
        wait
    }
}

/// What has befallen a tunnel since it came up.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Trouble {
    PeerGone(String),
    Removed(String),
}

/// The event that reports `report`: the trouble a tunnel is in, or with `None`
/// that it is out of it. Shared by both carriers.
pub(super) fn trouble_event(
    report: Option<Trouble>,
    relay_addr: &str,
) -> (EventSeverity, String, serde_json::Value) {
    match report {
        Some(Trouble::PeerGone(reason)) => (
            EventSeverity::Warning,
            format!("Tunnel peer disconnected: {reason}"),
            serde_json::json!({ "reason": reason, "relay_addr": relay_addr }),
        ),
        Some(Trouble::Removed(reason)) => (
            EventSeverity::Warning,
            format!("Tunnel removed on relay: {reason}"),
            serde_json::json!({ "reason": reason, "relay_addr": relay_addr }),
        ),
        None => (
            EventSeverity::Info,
            "Tunnel ready again".to_string(),
            serde_json::json!({ "relay_addr": relay_addr }),
        ),
    }
}

/// Which changes of a tunnel's fortune are reported, and when. One that comes
/// too soon after others waits, and what is reported then is what it has come
/// to: the last report is always the one that is true.
pub(super) struct Reports {
    pub(super) trouble: Option<Trouble>,
    reported: Option<Trouble>,
    left: u32,
    refilled: Instant,
}

impl Reports {
    pub(super) fn new(now: Instant) -> Self {
        Self { trouble: None, reported: None, left: REPORT_BURST, refilled: now }
    }

    /// The trouble to report now, or that there is none any more.
    pub(super) fn due(&mut self, now: Instant) -> Option<Option<Trouble>> {
        let earned = (now.duration_since(self.refilled).as_secs() / REPORT_INTERVAL.as_secs()) as u32;
        self.refilled += REPORT_INTERVAL * earned;
        self.left = self.left.saturating_add(earned).min(REPORT_BURST);
        if self.trouble == self.reported || self.left == 0 {
            return None;
        }
        self.left -= 1;
        self.reported = self.trouble.clone();
        Some(self.trouble.clone())
    }
}

/// The message that binds our side of the tunnel (with optional bind token for
/// relay authentication).
fn bind_message(params: &RelayTunnelParams) -> EdgeMessage {
    let (direction, direction_str) = match params.direction {
        TunnelDirection::Ingress => (RelayDirection::Ingress, "ingress"),
        TunnelDirection::Egress => (RelayDirection::Egress, "egress"),
    };
    let protocol = match params.protocol {
        TunnelProtocol::Tcp => RelayProtocol::Tcp,
        TunnelProtocol::Udp => RelayProtocol::Udp,
    };
    // Compute bind token if bind secret is available
    let bind_token = params.tunnel_bind_secret.as_deref().map(|secret| {
        super::auth::compute_bind_token(&params.tunnel_id.to_string(), direction_str, secret)
    });
    EdgeMessage::TunnelBind { tunnel_id: params.tunnel_id, direction, protocol, bind_token }
}

/// Which relay is tried next while the tunnel is bound on none, and when.
pub(super) struct Walk {
    relays: usize,
    began: usize,
    pub(super) at: usize,
    backoff: Duration,
}

impl Walk {
    pub(super) fn new(relays: usize, began: usize) -> Self {
        Self { relays, began, at: began, backoff: Duration::from_secs(1) }
    }

    /// The relay it is at came to nothing: go to the next. Back where it began
    /// it has tried them all, and waits before it goes on, for this long.
    pub(super) fn step(&mut self) -> Option<Duration> {
        self.at = (self.at + 1) % self.relays;
        if self.at != self.began {
            return None;
        }
        let wait = self.backoff;
        self.backoff = (wait * 2).min(Duration::from_secs(60));
        Some(wait)
    }
}

/// When a tunnel that is on one of its two relays tries the other.
pub(super) struct OtherRelay {
    on_backup: bool,
    timing: Timing,
    /// The tunnel's state on the relay it is on, and since when.
    state: RelayTunnelState,
    since: Instant,
    /// When it came there, or when the last try of the other came to nothing.
    idle: Instant,
    /// How long that puts off the next try, if it waits for its peer.
    pause: Duration,
    delay: RebindDelay,
    /// The peer has been left on a primary that is too far.
    gave_way: bool,
}

impl OtherRelay {
    pub(super) fn new(on_backup: bool, timing: &Timing, now: Instant) -> Self {
        Self {
            on_backup,
            timing: timing.clone(),
            state: RelayTunnelState::Connecting,
            since: now,
            idle: now,
            pause: Duration::ZERO,
            delay: RebindDelay(REBIND_MIN),
            gave_way: false,
        }
    }

    pub(super) fn set(&mut self, state: RelayTunnelState, now: Instant) {
        if state == self.state {
            return;
        }
        // A peer that was there for less than it settles has only looked.
        if self.state == RelayTunnelState::Ready && now >= self.since + 2 * self.timing.settle {
            self.pause = Duration::ZERO;
            self.delay = RebindDelay(REBIND_MIN);
            self.gave_way = false;
        }
        (self.state, self.since) = (state, now);
    }

    /// When the other relay is tried next, if it is.
    pub(super) fn due(&self) -> Option<Instant> {
        match self.state {
            RelayTunnelState::Waiting => {
                Some((self.since + self.timing.waiting).max(self.idle + self.pause))
            }
            RelayTunnelState::Ready if self.on_backup => Some(self.idle + self.timing.failback),
            _ => None,
        }
    }

    /// Whether our side is to be bound on the other relay. A tunnel that is up
    /// on its primary has nothing to look for, and one that is up on its backup
    /// does not go back to a primary that is too far. But it may be up only for
    /// as long as the peer tries the backup, and stays on the primary if it finds
    /// us there.
    pub(super) fn wanted(&self, now: Instant, is_near: impl FnOnce() -> bool) -> bool {
        if self.state != RelayTunnelState::Ready {
            return true;
        }
        self.on_backup && (now < self.since + self.timing.settle || is_near())
    }

    /// See [`SETTLE`].
    pub(super) fn settle(&self) -> Duration {
        match self.on_backup && self.state != RelayTunnelState::Ready {
            true => Duration::ZERO,
            false => self.timing.settle,
        }
    }

    /// Both sides are on the other relay: whether the tunnel goes there. One that
    /// waits on its backup does not go to a primary that is too far until the peer
    /// has had the time to come to the backup. A peer that starts looks for us on
    /// the primary first, and on the backup only if we are not there.
    pub(super) fn goes(&mut self, now: Instant, is_near: impl FnOnce() -> bool) -> bool {
        let waits = self.on_backup && self.state != RelayTunnelState::Ready;
        if !waits || self.gave_way || is_near() {
            return true;
        }
        self.gave_way = true;
        // What it waits, and what it may take to connect.
        (self.idle, self.pause) = (now, self.timing.waiting + self.timing.connect);
        false
    }

    /// The peer is not on the other relay.
    pub(super) fn alone(&mut self) {
        self.gave_way = false;
    }

    /// The try came to nothing.
    pub(super) fn failed(&mut self, now: Instant) {
        self.idle = now;
        self.pause = self.delay.next(Duration::ZERO);
    }
}

/// The relays that have refused our bind since we began. That is an alarm once,
/// and a refusal may go on for as long as nobody sees to it.
struct Refusals(Vec<bool>);

impl Refusals {
    fn none(params: &RelayTunnelParams) -> Self {
        Self(vec![false; params.relay_addrs.len()])
    }

    fn report(&mut self, params: &RelayTunnelParams, idx: usize, e: &BindRejected, events: &EventSender) {
        let relay_addr = &params.relay_addrs[idx];
        tracing::warn!(
            tunnel_id = %params.tunnel_id,
            relay = %relay_addr,
            "Tunnel bind rejected by relay: {e}"
        );
        if !std::mem::replace(&mut self.0[idx], true) {
            events.emit_flow_with_details(
                EventSeverity::Critical,
                category::TUNNEL,
                format!("Tunnel bind rejected by relay: {e}"),
                &params.tunnel_id.to_string(),
                serde_json::json!({ "relay_addr": relay_addr }),
            );
        }
    }
}

/// A second connection, that the tunnel may move to. It is let go of if it does
/// not.
struct Second {
    link: Option<RelayLink>,
    /// The one the tunnel is on.
    active: LinkHandle,
}

impl Drop for Second {
    fn drop(&mut self) {
        if let Some(link) = self.link.take() {
            link.let_go(self.active.clone(), Duration::ZERO, true);
        }
    }
}

/// The other relay of a tunnel that is on relay `idx`, once the tunnel is to
/// move there. With one relay there is none.
///
/// Our side is bound there too, on a connection of its own, and the tunnel moves
/// when the relay says that both sides are there, but not at once to a primary
/// that is too far ([`OtherRelay::goes`]). What it is on stays as it is until then:
/// nothing is interrupted, and nothing moves while the peer cannot come too. That
/// goes on for as long as the tunnel waits for its peer, which may have lost its
/// path to the relay the tunnel is on, and, if the primary is near enough, while
/// the tunnel is up on its backup. The peer needs to know nothing of it: an older
/// one binds where it happens to look.
pub async fn other_relay(
    params: &RelayTunnelParams,
    idx: usize,
    active: &LinkHandle,
    state: watch::Receiver<RelayTunnelState>,
    events: &EventSender,
) -> (usize, RelayLink) {
    other_relay_refusing(params, idx, active, state, &mut Refusals::none(params), events).await
}

async fn other_relay_refusing(
    params: &RelayTunnelParams,
    idx: usize,
    active: &LinkHandle,
    mut state: watch::Receiver<RelayTunnelState>,
    refusals: &mut Refusals,
    events: &EventSender,
) -> (usize, RelayLink) {
    let to = (idx + 1) % params.relay_addrs.len();
    if to == idx {
        return std::future::pending().await;
    }
    let mut rule = OtherRelay::new(idx != 0, &params.timing, Instant::now());
    loop {
        rule.set(*state.borrow_and_update(), Instant::now());
        let due = async {
            match rule.due() {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = due => {}
            changed = state.changed() => match changed {
                Ok(()) => continue,
                Err(_) => return std::future::pending().await,
            },
        }
        match try_other(params, to, active, &mut state, &mut rule).await {
            Ok(Some(link)) => return (to, link),
            Ok(None) => tracing::info!(
                tunnel_id = %params.tunnel_id,
                relay = %params.relay_addrs[to],
                "The peer is on a primary that is too far, leaving it to come to the backup"
            ),
            Err(e) => {
                tracing::debug!(
                    tunnel_id = %params.tunnel_id,
                    relay = %params.relay_addrs[to],
                    "Not moving to the other relay: {e}"
                );
                if let Some(e) = e.downcast_ref() {
                    refusals.report(params, to, e, events);
                }
                rule.failed(Instant::now());
            }
        }
    }
}

/// Bind our side on relay `to` and wait there until the tunnel is to move to it.
/// `None` if the peer is there and it is not to ([`OtherRelay::goes`]).
async fn try_other(
    params: &RelayTunnelParams,
    to: usize,
    active: &LinkHandle,
    state: &mut watch::Receiver<RelayTunnelState>,
    rule: &mut OtherRelay,
) -> Result<Option<RelayLink>> {
    let link = RelayLink::dial(params, &params.relay_addrs[to]).await?;
    let conn = link.conn().clone();
    let is_near = || {
        let gate = Duration::from_millis(params.failback_rtt_gate_ms.into());
        let (rtt, active_rtt) = (conn.rtt(), active.conn.rtt());
        let near = rtt <= active_rtt.saturating_add(gate);
        if !near {
            tracing::info!(
                tunnel_id = %params.tunnel_id,
                "Not failing back: primary_rtt={rtt:?}, active_rtt={active_rtt:?}, gate={gate:?}"
            );
        }
        near
    };
    rule.set(*state.borrow_and_update(), Instant::now());
    if !rule.wanted(Instant::now(), is_near) {
        anyhow::bail!("it is not wanted, or too far");
    }
    let mut second = Second { link: Some(link), active: active.clone() };
    let link = second.link.as_mut().expect("just put there");
    link.bind().await?;
    let every = params.timing.failback;
    let mut check = tokio::time::interval_at(Instant::now() + every, every);
    let mut both_since: Option<Instant> = None;
    loop {
        let settled = async {
            match both_since {
                Some(since) => sleep_until(since + rule.settle()).await,
                None => std::future::pending().await,
            }
        };
        // It has settled only if there is nothing else to say.
        tokio::select! {
            biased;
            changed = state.changed() => {
                changed?;
                rule.set(*state.borrow_and_update(), Instant::now());
                check.reset_after(params.timing.settle);
            }
            news = link.news() => {
                let why = match news {
                    Ok(News::Ready) => {
                        both_since.get_or_insert(Instant::now());
                        continue;
                    }
                    Ok(News::Waiting | News::PeerGone(_)) => {
                        both_since = None;
                        rule.alone();
                        continue;
                    }
                    Ok(News::Stale) => {
                        link.bind_again().await?;
                        continue;
                    }
                    Ok(News::Removed(reason)) => anyhow::anyhow!("tunnel removed on relay: {reason}"),
                    Ok(News::Refused(reason)) => {
                        // There is no bind to let go of.
                        second.link = None;
                        return Err(BindRejected(reason).into());
                    }
                    Err(e) => e,
                };
                return Err(why);
            }
            _ = check.tick() => {}
            _ = settled => {
                return Ok(if rule.goes(Instant::now(), is_near) { second.link.take() } else { None });
            }
        }
        if !rule.wanted(Instant::now(), is_near) {
            anyhow::bail!("it is not wanted any more, or too far");
        }
    }
}

/// Bind the tunnel on relay `idx` and wait until both sides are there, on this
/// relay or on the other.
///
/// A tunnel with two relays stays bound on those it has reached for as long as
/// it waits. With one, it bails after [`WAITING_TIMEOUT`] with an error
/// containing [`WAITING_TIMEOUT_MSG`], so that the caller starts over without
/// treating it as a hard failure.
async fn meet(
    params: &RelayTunnelParams,
    idx: usize,
    state_tx: &watch::Sender<RelayTunnelState>,
    refusals: &mut Refusals,
    events: &EventSender,
) -> Result<(usize, RelayLink)> {
    let relay_addr: &str = &params.relay_addrs[idx];
    state_tx.send_replace(RelayTunnelState::Connecting);
    let mut link = RelayLink::dial(params, relay_addr).await?;
    link.bind().await?;

    // Wait for tunnel to become ready. The relay says so in answer to our bind
    // if we are the second edge to bind, and in a notice if the peer binds
    // after us.
    let active = link.handle().clone();
    let other = other_relay_refusing(params, idx, &active, state_tx.subscribe(), refusals, events);
    tokio::pin!(other);
    let mut deadline: Option<Instant> = None;
    loop {
        let waited_out = async {
            match deadline {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        // The relay it is on before the other.
        tokio::select! {
            biased;
            news = link.news() => match news? {
                News::Ready => return Ok((idx, link)),
                // As it did, after we have bound again.
                News::Waiting if *state_tx.borrow() == RelayTunnelState::Waiting => {}
                News::Waiting => {
                    state_tx.send_replace(RelayTunnelState::Waiting);
                    if params.relay_addrs.len() == 1 {
                        deadline.get_or_insert(Instant::now() + params.timing.waiting);
                    }
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        "Waiting for peer to bind"
                    );
                }
                // It came and went before we were told that it came.
                News::PeerGone(_) => {}
                News::Stale => link.bind_again().await?,
                News::Removed(reason) => anyhow::bail!("tunnel removed on relay: {reason}"),
                News::Refused(reason) => return Err(BindRejected(reason).into()),
            },
            found = &mut other => {
                link.leave_for(&found.1, Duration::ZERO);
                return Ok(found);
            }
            _ = waited_out => {
                // Peer hasn't bound on this relay within the window. Unbind
                // politely so the relay drops its pending state, then bail.
                let _ = link.unbind().await;
                tracing::info!(
                    tunnel_id = %params.tunnel_id,
                    relay = %relay_addr,
                    "Peer hasn't bound on this relay within {:?}, starting over",
                    params.timing.waiting
                );
                anyhow::bail!("{WAITING_TIMEOUT_MSG}: peer not bound on {relay_addr}");
            }
        }
    }
}

/// Report that the tunnel is up on relay `to`, and was on relay `from` before.
pub fn report_up(params: &RelayTunnelParams, from: usize, to: usize, events: &EventSender) {
    report_moved(&params.tunnel_id, &params.relay_addrs, from, to, serde_json::json!({}), events);
}

/// [`report_up`] for any carrier: `extra` goes into the failover event's
/// details beside what QUIC puts there.
pub(super) fn report_moved(
    tunnel_id: &Uuid,
    relay_addrs: &[String],
    from: usize,
    to: usize,
    extra: serde_json::Value,
    events: &EventSender,
) {
    let tunnel_id = tunnel_id.to_string();
    let (from_addr, to_addr) = (&relay_addrs[from], &relay_addrs[to]);
    events.emit_flow_with_details(
        EventSeverity::Info,
        category::TUNNEL,
        "Tunnel connected to relay",
        &tunnel_id,
        serde_json::json!({ "relay_addr": to_addr }),
    );
    if from == to {
        return;
    }
    let which = if to == 0 { "primary" } else { "backup" };
    tracing::warn!(
        tunnel_id = %tunnel_id,
        "Tunnel failed over from {from_addr} to {to_addr} (idx {from} → {to})"
    );
    let mut details = serde_json::json!({
        "from_relay_addr": from_addr,
        "to_relay_addr": to_addr,
        "from_idx": from,
        "to_idx": to,
    });
    if let (Some(base), serde_json::Value::Object(extra)) = (details.as_object_mut(), extra) {
        base.extend(extra);
    }
    events.emit_flow_with_details(
        EventSeverity::Warning,
        category::TUNNEL,
        format!("Tunnel failed over to {which} relay {to_addr}"),
        &tunnel_id,
        details,
    );
}

/// Report that a native-UDP tunnel goes back to its primary relay: the sibling
/// of the QUIC carrier's report in `manager.rs`, with the RTTs it measures from
/// its Registers (`null` while it has too few) and its `transport`.
pub(super) fn report_failing_back(
    tunnel_id: &Uuid,
    primary_relay: &str,
    primary_rtt: Option<Duration>,
    active_rtt: Option<Duration>,
    gate_ms: u32,
    transport: &str,
    events: &EventSender,
) {
    events.emit_flow_with_details(
        EventSeverity::Info,
        category::TUNNEL,
        format!("Tunnel failing back to primary relay {primary_relay}"),
        &tunnel_id.to_string(),
        serde_json::json!({
            "primary_relay_addr": primary_relay,
            "primary_rtt_ms": primary_rtt.map(|d| d.as_millis()),
            "active_rtt_ms": active_rtt.map(|d| d.as_millis()),
            "gate_ms": gate_ms,
            "transport": transport,
        }),
    );
}

/// Connect to a relay with ordered failover + exponential backoff, and bind the
/// tunnel there, until both sides are ([`meet`]).
///
/// Walks `params.relay_addrs` starting from `*active_idx`, stepping forward on
/// each failure. After trying every relay in the list once (a full cycle), it
/// sleeps with exponential backoff before retrying.
///
/// `active_idx` is owned by the caller so that reconnect cycles are sticky: when
/// the forwarder drops and we reconnect, we resume on the same relay we were
/// using (and only step forward on sustained failure).
///
/// `active_idx_tx` publishes the currently-active relay index to observers
/// (stats snapshot).
pub async fn connect_with_retry(
    params: &RelayTunnelParams,
    active_idx: &mut usize,
    active_idx_tx: &watch::Sender<usize>,
    state_tx: &watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    event_sender: EventSender,
) -> Result<RelayLink> {
    if params.relay_addrs.is_empty() {
        anyhow::bail!("relay_addrs is empty — at least one relay is required");
    }

    let n = params.relay_addrs.len();
    let mut walk = Walk::new(n, if *active_idx < n { *active_idx } else { 0 });
    let mut refusals = Refusals::none(params);

    loop {
        let relay_addr: &str = &params.relay_addrs[walk.at];
        let met = tokio::select! {
            met = meet(params, walk.at, state_tx, &mut refusals, &event_sender) => met,
            _ = cancel.cancelled() => anyhow::bail!("cancelled during connect"),
        };
        match met {
            Ok((idx, link)) => {
                *active_idx = idx;
                active_idx_tx.send_replace(idx);
                state_tx.send_replace(RelayTunnelState::Ready);
                tracing::info!(
                    tunnel_id = %params.tunnel_id,
                    direction = %params.direction,
                    "Tunnel ready"
                );
                report_up(params, walk.began, idx, &event_sender);
                return Ok(link);
            }
            Err(e) => {
                let err_msg = e.to_string();
                let is_waiting_timeout = err_msg.contains(WAITING_TIMEOUT_MSG);

                if is_waiting_timeout {
                    // Expected, not a hard failure.
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        relay = %relay_addr,
                        "Peer not there (waiting timeout)"
                    );
                } else if let Some(e) = e.downcast_ref() {
                    refusals.report(params, walk.at, e, &event_sender);
                } else {
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning,
                        category::TUNNEL,
                        format!("Tunnel connection to relay failed: {e}"),
                        &params.tunnel_id.to_string(),
                        serde_json::json!({ "relay_addr": relay_addr.to_string() }),
                    );
                    tracing::warn!(
                        tunnel_id = %params.tunnel_id,
                        relay = %relay_addr,
                        "Relay connection failed: {e}"
                    );
                }

                // Step to the next relay in the list. If that brings us back
                // to where we began we've tried every relay once this cycle —
                // sleep with exponential backoff before the next round.
                let pause = walk.step();
                active_idx_tx.send_replace(walk.at);
                if let Some(backoff) = pause {
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        "All {n} relays failed this cycle, sleeping {backoff:?} before retry"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => anyhow::bail!("cancelled during retry backoff"),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) mod stand_in {
    //! A relay that says what the test tells it to, over the real protocol.
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::net::UdpSocket;

    use super::*;
    use crate::tunnel::protocol::{read_message, write_message};

    pub(crate) const WAIT: Duration = Duration::from_secs(20);

    pub(crate) async fn soon<T>(what: &str, work: impl Future<Output = T>) -> T {
        timeout(WAIT, work).await.unwrap_or_else(|_| panic!("{what} did not happen"))
    }

    pub(crate) struct StandIn {
        endpoint: quinn::Endpoint,
        pub(crate) addr: String,
    }

    /// One edge's connection, as the relay has it.
    pub(crate) struct Session {
        pub(crate) conn: Connection,
        pub(crate) send: quinn::SendStream,
        pub(crate) recv: quinn::RecvStream,
    }

    impl StandIn {
        pub(crate) fn start() -> Self {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let any = "127.0.0.1:0".parse().unwrap();
            let endpoint = quic::make_server_endpoint(any, ALPN_RELAY, None, None).unwrap();
            let addr = endpoint.local_addr().unwrap().to_string();
            Self { endpoint, addr }
        }

        /// The next connection to come.
        pub(crate) async fn accept(&self) -> Connection {
            let incoming = soon("a connection", self.endpoint.accept()).await.unwrap();
            soon("its handshake", incoming.into_future()).await.unwrap()
        }

        /// The same, with its control stream.
        pub(crate) async fn session(&self) -> Session {
            let conn = self.accept().await;
            let (send, recv) = soon("its control stream", conn.accept_bi()).await.unwrap();
            Session { conn, send, recv }
        }

        /// [`Self::session`], read up to its bind.
        pub(crate) async fn binding(&self) -> (Session, EdgeMessage) {
            let mut session = self.session().await;
            let bind = session.next().await;
            (session, bind)
        }

        /// The next connection to come, which the relay ends before any bind.
        pub(crate) async fn turns_away(&self) {
            self.session().await.conn.close(0u32.into(), b"");
        }

        /// Whether a connection comes within `time`.
        pub(crate) async fn is_dialled(&self, time: Duration) -> bool {
            timeout(time, self.endpoint.accept()).await.is_ok()
        }
    }

    impl Session {
        /// The next message.
        pub(crate) async fn raw(&mut self) -> EdgeMessage {
            soon("a message", read_message(&mut self.recv)).await.unwrap()
        }

        /// The next message that is not part of the greeting, nor a ping.
        pub(crate) async fn next(&mut self) -> EdgeMessage {
            let next = async {
                loop {
                    match self.raw().await {
                        EdgeMessage::Hello { .. } | EdgeMessage::Identify { .. } | EdgeMessage::Ping => {}
                        msg => return msg,
                    }
                }
            };
            soon("a message", next).await
        }

        /// Whether the edge says anything within `time`, a ping aside.
        pub(crate) async fn is_silent(&mut self, time: Duration) -> bool {
            timeout(time, self.next()).await.is_err()
        }

        pub(crate) async fn answer(&mut self, msg: RelayMessage) {
            write_message(&mut self.send, &msg).await.unwrap();
        }

        pub(crate) async fn notify(&self, msg: RelayMessage) {
            let mut stream = soon("room for a notice", self.conn.open_uni()).await.unwrap();
            write_message(&mut stream, &msg).await.unwrap();
            stream.finish().unwrap();
        }

        pub(crate) async fn closed(&self) {
            soon("the edge's leaving", self.conn.closed()).await;
        }

        /// The edge lets go of its side, and then of the connection.
        pub(crate) async fn is_left(&mut self) {
            assert!(matches!(self.next().await, EdgeMessage::TunnelUnbind { .. }));
            self.closed().await;
        }
    }

    /// A way to a relay, that takes a while.
    pub(crate) struct Far {
        /// In milliseconds, each way, or [`CUT`].
        pub(crate) delay: Arc<AtomicU64>,
        _way: JoinSet<()>,
    }

    /// The delay of a way on which nothing comes through.
    pub(crate) const CUT: u64 = u64::MAX;

    impl Far {
        /// From now on the relay's address is that of the way to it.
        pub(crate) async fn to(relay: &mut StandIn, delay: u64) -> Self {
            let here = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let addr = here.local_addr().unwrap().to_string();
            let relay = std::mem::replace(&mut relay.addr, addr);
            let delay = Arc::new(AtomicU64::new(delay));
            let wait = delay.clone();
            let mut way = JoinSet::new();
            way.spawn(async move {
                let mut back = JoinSet::new();
                let mut edges: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
                loop {
                    let (data, edge) = next_datagram(&here).await;
                    if let Some(there) = edges.get(&edge) {
                        later(&wait, there.clone(), data, None);
                        continue;
                    }
                    let there = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                    there.connect(&relay).await.unwrap();
                    edges.insert(edge, there.clone());
                    later(&wait, there.clone(), data, None);
                    let (here, wait) = (here.clone(), wait.clone());
                    back.spawn(async move {
                        loop {
                            let (data, _) = next_datagram(&there).await;
                            later(&wait, here.clone(), data, Some(edge));
                        }
                    });
                }
            });
            Self { delay, _way: way }
        }
    }

    async fn next_datagram(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
        let mut buf = vec![0; 2048];
        loop {
            // An error is of a datagram that was sent.
            if let Ok((n, from)) = socket.recv_from(&mut buf).await {
                buf.truncate(n);
                return (buf, from);
            }
        }
    }

    fn later(wait: &AtomicU64, by: Arc<UdpSocket>, data: Vec<u8>, to: Option<SocketAddr>) {
        let wait = match wait.load(Ordering::Relaxed) {
            CUT => return,
            wait => Duration::from_millis(wait),
        };
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = match to {
                Some(to) => by.send_to(&data, to).await,
                None => by.send(&data).await,
            };
        });
    }

    pub(crate) fn ready(tunnel_id: Uuid) -> RelayMessage {
        RelayMessage::TunnelReady { tunnel_id }
    }

    pub(crate) fn waiting(tunnel_id: Uuid) -> RelayMessage {
        RelayMessage::TunnelWaiting { tunnel_id }
    }

    pub(crate) fn down(tunnel_id: Uuid, reason: &str) -> RelayMessage {
        RelayMessage::TunnelDown { tunnel_id, reason: reason.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::*;
    use super::*;
    use crate::manager::events::{Event, event_channel};
    use RelayTunnelState::{Connecting, Down, Ready, Waiting};

    const REFUSED: &str = "bind authentication failed";

    /// One tunnel's connection to a stand-in relay, and what becomes of it.
    struct Tunnel {
        id: Uuid,
        relay: StandIn,
        state: watch::Receiver<RelayTunnelState>,
        events: mpsc::Receiver<Event>,
        cancel: CancellationToken,
        connect: Option<tokio::task::JoinHandle<Result<Connection>>>,
    }

    impl Tunnel {
        fn start() -> Self {
            Self::start_with(|params| params)
        }

        fn start_with(adjust: impl FnOnce(RelayTunnelParams) -> RelayTunnelParams) -> Self {
            let relay = StandIn::start();
            let params = adjust(RelayTunnelParams {
                tunnel_id: Uuid::new_v4(),
                relay_addrs: vec![relay.addr.clone()],
                direction: TunnelDirection::Ingress,
                protocol: TunnelProtocol::Udp,
                edge_id: Some("edge-1".into()),
                tunnel_bind_secret: Some("ab".repeat(32)),
                failback_rtt_gate_ms: 50,
                timing: Default::default(),
            });
            let id = params.tunnel_id;
            let (state_tx, state) = watch::channel(Connecting);
            let (event_tx, events) = event_channel();
            let cancel = CancellationToken::new();
            let token = cancel.clone();
            let connect = tokio::spawn(async move {
                let (active_tx, _active) = watch::channel(0);
                let (cancel, events) = (token.clone(), event_tx.clone());
                let mut link =
                    connect_with_retry(&params, &mut 0, &active_tx, &state_tx, cancel, events).await?;
                let conn = link.conn().clone();
                tokio::spawn(async move {
                    let relay_addr = &params.relay_addrs[0];
                    tokio::select! {
                        _ = link.hold(relay_addr, &state_tx, &event_tx) => {}
                        _ = token.cancelled() => {}
                    }
                });
                Ok(conn)
            });
            Self { id, relay, state, events, cancel, connect: Some(connect) }
        }

        /// Wait for a state.
        async fn becomes(&mut self, want: RelayTunnelState) {
            soon("a change of state", async {
                while *self.state.borrow_and_update() != want {
                    self.state.changed().await.unwrap();
                }
            })
            .await;
        }

        /// Bound, and the peer with it.
        async fn up(&mut self) -> (Session, Connection) {
            let (mut session, bind) = self.relay.binding().await;
            self.assert_binds(&bind);
            session.answer(ready(self.id)).await;
            self.becomes(Ready).await;
            let conn = soon("the connection", self.connect.take().unwrap()).await.unwrap().unwrap();
            assert_eq!(self.told().await, "Tunnel connected to relay");
            (session, conn)
        }

        fn assert_binds(&self, bind: &EdgeMessage) {
            let token =
                crate::tunnel::auth::compute_bind_token(&self.id.to_string(), "ingress", &"ab".repeat(32));
            match bind {
                EdgeMessage::TunnelBind { tunnel_id, direction, protocol, bind_token } => {
                    assert_eq!(*tunnel_id, self.id);
                    assert_eq!(*direction, RelayDirection::Ingress);
                    assert_eq!(*protocol, RelayProtocol::Udp);
                    assert_eq!(bind_token.as_deref(), Some(&token[..]));
                }
                other => panic!("{other:?}"),
            }
        }

        async fn event(&mut self) -> Event {
            let event = soon("an event", self.events.recv()).await.unwrap();
            assert_eq!(event.category, category::TUNNEL);
            assert_eq!(event.flow_id, Some(self.id.to_string()));
            event
        }

        /// The message of the next event.
        async fn told(&mut self) -> String {
            self.event().await.message
        }

        /// No event is raised within `time`.
        async fn is_quiet(&mut self, time: Duration) {
            if let Ok(event) = timeout(time, self.events.recv()).await {
                panic!("{event:?}");
            }
        }
    }

    const A_MOMENT: Duration = Duration::from_millis(300);

    /// The greeting, as every relay release reads it.
    #[tokio::test]
    async fn the_edge_greets_the_relay_and_binds() {
        let mut t = Tunnel::start();
        let mut session = t.relay.session().await;
        let mut said = Vec::new();
        for _ in 0..3 {
            said.push(session.raw().await);
        }
        assert!(
            matches!(
                &said[..],
                [
                    EdgeMessage::Hello { protocol_version: TUNNEL_PROTOCOL_VERSION, .. },
                    EdgeMessage::Identify { edge_id },
                    EdgeMessage::TunnelBind { .. },
                ] if edge_id == "edge-1"
            ),
            "{said:?}"
        );
        t.assert_binds(&said[2]);
        // Before and after the answer, what is not about the tunnel is let by.
        for msg in [
            RelayMessage::HelloAck { protocol_version: 99, software_version: "x".into() },
            RelayMessage::Pong,
            ready(Uuid::new_v4()),
            waiting(Uuid::new_v4()),
            down(Uuid::new_v4(), REFUSED),
        ] {
            session.answer(msg).await;
        }
        let unknown = serde_json::json!({ "type": "some_day" });
        protocol::write_message(&mut session.send, &unknown).await.unwrap();
        tokio::time::sleep(A_MOMENT).await;
        assert_eq!(*t.state.borrow(), Connecting);
        session.answer(waiting(t.id)).await;
        t.becomes(Waiting).await;
        session.notify(ready(Uuid::new_v4())).await;
        session.notify(down(Uuid::new_v4(), "tunnel closed by manager")).await;
        tokio::time::sleep(A_MOMENT).await;
        assert_eq!(*t.state.borrow(), Waiting);
        session.notify(ready(t.id)).await;
        t.becomes(Ready).await;
        soon("the connection", t.connect.take().unwrap()).await.unwrap().unwrap();
        let event = t.event().await;
        assert_eq!((event.severity, &event.message[..]), (EventSeverity::Info, "Tunnel connected to relay"));
        assert_eq!(event.details.unwrap()["relay_addr"], t.relay.addr);
        assert!(session.is_silent(A_MOMENT).await);
    }

    #[test]
    fn the_bind_is_for_the_side_the_tunnel_is_configured_with() {
        use {TunnelDirection as D, TunnelProtocol as P};
        let secret = "ab".repeat(32);
        for (direction, protocol, token, want) in [
            (D::Ingress, P::Tcp, Some("ingress"), ("ingress", "tcp")),
            (D::Ingress, P::Udp, None, ("ingress", "udp")),
            (D::Egress, P::Tcp, None, ("egress", "tcp")),
            (D::Egress, P::Udp, Some("egress"), ("egress", "udp")),
        ] {
            let id = Uuid::new_v4();
            let token = token.map(|side| {
                crate::tunnel::auth::compute_bind_token(&id.to_string(), side, &secret)
            });
            let bind = bind_message(&RelayTunnelParams {
                tunnel_id: id,
                relay_addrs: Vec::new(),
                direction,
                protocol,
                edge_id: None,
                tunnel_bind_secret: token.is_some().then(|| secret.clone()),
                failback_rtt_gate_ms: 50,
                timing: Default::default(),
            });
            let mut on_the_wire = serde_json::json!({
                "type": "tunnel_bind", "tunnel_id": id, "direction": want.0, "protocol": want.1,
            });
            if let Some(token) = token {
                on_the_wire["bind_token"] = token.into();
            }
            assert_eq!(serde_json::to_value(bind).unwrap(), on_the_wire);
        }
    }

    /// The peer goes, and comes back, whatever the relay calls its going. The
    /// connection and the bind stay as they are: nothing is sent to the relay.
    #[tokio::test]
    async fn a_peer_that_goes_is_waited_for_on_the_same_connection() {
        let mut t = Tunnel::start();
        let (mut session, conn) = t.up().await;
        for reason in ["peer disconnected", "peer unbound"] {
            session.notify(down(t.id, reason)).await;
            t.becomes(Waiting).await;
            let event = t.event().await;
            assert_eq!(event.severity, EventSeverity::Warning);
            assert_eq!(event.message, format!("Tunnel peer disconnected: {reason}"));
            let details = event.details.unwrap();
            assert_eq!((&details["reason"], &details["relay_addr"]), (&reason.into(), &t.relay.addr[..].into()));
            assert!(session.is_silent(REBIND_MIN + A_MOMENT).await);

            session.notify(ready(t.id)).await;
            t.becomes(Ready).await;
            let event = t.event().await;
            assert_eq!((event.severity, &event.message[..]), (EventSeverity::Info, "Tunnel ready again"));
            assert_eq!(event.details.unwrap()["relay_addr"], t.relay.addr);
        }
        assert!(!t.relay.is_dialled(A_MOMENT).await);
        assert!(conn.close_reason().is_none());
        assert!(session.is_silent(A_MOMENT).await);
        t.is_quiet(A_MOMENT).await;
    }

    /// Our side is removed, for a reason that is known or is not. It is bound
    /// again on the same connection, with the same bind, and not at once.
    #[tokio::test]
    async fn a_side_that_is_removed_is_bound_again() {
        for reason in ["tunnel closed by manager", "tunnel deleted via relay REST API", "some day"] {
            let mut t = Tunnel::start();
            let (mut session, conn) = t.up().await;
            session.notify(down(t.id, reason)).await;
            t.becomes(Down).await;
            let event = t.event().await;
            assert_eq!(event.severity, EventSeverity::Warning);
            assert_eq!(event.message, format!("Tunnel removed on relay: {reason}"));
            let details = event.details.unwrap();
            assert_eq!((&details["reason"], &details["relay_addr"]), (&reason.into(), &t.relay.addr[..].into()));
            assert!(session.is_silent(REBIND_MIN - A_MOMENT).await, "at once");
            assert_eq!(*t.state.borrow(), Down);
            let bind = session.next().await;
            t.assert_binds(&bind);

            session.answer(waiting(t.id)).await;
            t.becomes(Waiting).await;
            t.is_quiet(A_MOMENT).await;
            session.notify(ready(t.id)).await;
            t.becomes(Ready).await;
            assert_eq!(t.told().await, "Tunnel ready again");
            assert!(session.is_silent(REBIND_MIN + A_MOMENT).await);
            assert!(!t.relay.is_dialled(A_MOMENT).await);
            assert!(conn.close_reason().is_none());
        }
    }

    /// Whatever else the relay had to say of a side it has removed changes
    /// nothing: it is bound again, when it would have been, and stays down
    /// until then.
    #[tokio::test]
    async fn what_is_said_of_a_removed_side_is_let_by() {
        let mut t = Tunnel::start();
        let (mut session, _conn) = t.up().await;
        let removed = "tunnel closed by manager";
        session.notify(down(t.id, removed)).await;
        t.becomes(Down).await;
        let since = Instant::now();
        assert_eq!(t.told().await, format!("Tunnel removed on relay: {removed}"));
        for msg in [
            down(t.id, "peer disconnected"),
            ready(t.id),
            down(t.id, "tunnel deleted via relay REST API"),
        ] {
            session.notify(msg).await;
            assert!(session.is_silent(A_MOMENT / 3).await);
            assert_eq!(*t.state.borrow(), Down);
        }
        session.answer(waiting(t.id)).await;
        t.assert_binds(&session.next().await);
        let waited = since.elapsed();
        assert!(waited >= REBIND_MIN - A_MOMENT && waited < REBIND_MIN * 2 - A_MOMENT, "{waited:?}");
        assert_eq!(*t.state.borrow(), Down);
        t.is_quiet(A_MOMENT).await;
        session.answer(ready(t.id)).await;
        t.becomes(Ready).await;
        assert_eq!(t.told().await, "Tunnel ready again");
        assert!(session.is_silent(REBIND_MIN + A_MOMENT).await);

        // The answer is one to our bind, whatever was said before it.
        session.notify(down(t.id, removed)).await;
        t.becomes(Down).await;
        session.notify(down(t.id, "peer disconnected")).await;
        t.assert_binds(&session.next().await);
        session.answer(waiting(t.id)).await;
        t.becomes(Waiting).await;
    }

    /// One removed again and again is bound again later and later. Its answer
    /// is that the tunnel is up, or that it is waiting.
    #[tokio::test(start_paused = true)]
    async fn a_side_that_is_removed_again_waits_longer() {
        let mut t = Tunnel::start();
        let (mut session, _conn) = t.up().await;
        for (wait, answer) in [(1, ready(t.id)), (2, waiting(t.id)), (4, ready(t.id))] {
            let removed = Instant::now();
            session.notify(down(t.id, "tunnel closed by manager")).await;
            t.becomes(Down).await;
            let bind = session.next().await;
            t.assert_binds(&bind);
            let waited = removed.elapsed();
            let wait = Duration::from_secs(wait);
            assert!(waited >= wait && waited < wait + Duration::from_millis(900), "{waited:?} for {wait:?}");
            let up = matches!(answer, RelayMessage::TunnelReady { .. });
            session.answer(answer).await;
            t.becomes(if up { Ready } else { Waiting }).await;
        }
    }

    #[test]
    fn the_wait_doubles_and_starts_over_once_a_bind_has_stood() {
        let mut delay = RebindDelay(REBIND_MIN);
        let just_under = REBIND_MAX - Duration::from_millis(1);
        let waits: Vec<u64> = (0..8).map(|_| delay.next(just_under).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(delay.next(REBIND_MAX).as_secs(), 1);
        assert_eq!(delay.next(Duration::ZERO).as_secs(), 2);
    }

    /// What a relay gives as its reason is not taken at any length.
    #[tokio::test]
    async fn a_reason_is_kept_to_its_first_characters() {
        let long = "\u{e9}".repeat(100_000);
        let kept = "\u{e9}".repeat(MAX_REASON);
        let mut t = Tunnel::start();
        let (mut session, _bind) = t.relay.binding().await;
        session.answer(down(t.id, &long)).await;
        assert_eq!(t.told().await, format!("Tunnel bind rejected by relay: {kept}"));
        let (session, _conn) = t.up().await;
        session.notify(down(t.id, &long)).await;
        assert_eq!(t.told().await, format!("Tunnel removed on relay: {kept}"));
    }

    /// Bound again and refused, the edge leaves the connection.
    #[tokio::test]
    async fn a_bind_that_is_refused_is_not_repeated_on_the_connection() {
        let mut t = Tunnel::start();
        let (mut session, conn) = t.up().await;
        session.notify(down(t.id, "tunnel deleted via relay REST API")).await;
        t.becomes(Down).await;
        t.assert_binds(&session.next().await);
        session.answer(down(t.id, REFUSED)).await;
        session.closed().await;
        assert!(conn.close_reason().is_some(), "the forwarder is not ended");
    }

    /// Bound again and not answered, it leaves the connection too, in time.
    #[tokio::test(start_paused = true)]
    async fn a_bind_that_is_not_answered_is_not_waited_for() {
        let mut t = Tunnel::start();
        let (mut session, conn) = t.up().await;
        session.notify(down(t.id, "tunnel closed by manager")).await;
        t.becomes(Down).await;
        t.assert_binds(&session.next().await);
        let asked = Instant::now();
        assert!(session.is_silent(REBIND_ANSWER_TIMEOUT - Duration::from_secs(1)).await);
        assert!(conn.close_reason().is_none());
        session.closed().await;
        assert!(asked.elapsed() >= REBIND_ANSWER_TIMEOUT - A_MOMENT);
    }

    /// How long a bind has stood is counted from its own answer. A minute after
    /// it was bound again the side is again bound soon, and not when the minute
    /// is only the tunnel's.
    #[tokio::test(start_paused = true)]
    async fn a_bind_that_has_stood_is_bound_again_soon() {
        let mut t = Tunnel::start();
        let (mut session, _conn) = t.up().await;
        tokio::time::sleep(REBIND_MAX).await;
        for (wait, stands) in [(1, 0), (2, 59), (4, 60), (1, 0), (2, 0)] {
            session.notify(down(t.id, "tunnel closed by manager")).await;
            t.becomes(Down).await;
            let removed = Instant::now();
            t.assert_binds(&session.next().await);
            let waited = removed.elapsed().as_secs_f64().round();
            assert_eq!(waited, f64::from(wait));
            session.answer(ready(t.id)).await;
            t.becomes(Ready).await;
            tokio::time::sleep(Duration::from_secs(stands)).await;
        }
    }

    /// The first bind is refused. That is reported once, and it is tried again,
    /// not at once, until it is not refused any more.
    #[tokio::test]
    async fn a_first_bind_that_is_refused_is_tried_again() {
        let mut t = Tunnel::start();
        let mut refused = Instant::now();
        for (attempt, wait) in [(0, 0), (1, 1), (2, 2)] {
            let (mut session, bind) = t.relay.binding().await;
            t.assert_binds(&bind);
            assert!(refused.elapsed() >= Duration::from_secs(wait), "attempt {attempt}");
            assert_eq!(*t.state.borrow(), Connecting);
            session.answer(down(t.id, REFUSED)).await;
            if attempt == 0 {
                let event = t.event().await;
                assert_eq!(event.severity, EventSeverity::Critical);
                assert_eq!(event.message, format!("Tunnel bind rejected by relay: {REFUSED}"));
                assert_eq!(event.details.unwrap()["relay_addr"], t.relay.addr);
            }
            refused = Instant::now();
            session.closed().await;
            t.is_quiet(A_MOMENT).await;
        }
        let _up = t.up().await;
        t.is_quiet(A_MOMENT).await;
    }

    /// Of several relays each is reported once, whichever else is tried in
    /// between.
    #[tokio::test]
    async fn each_relay_that_refuses_is_reported_once() {
        let second = StandIn::start();
        let mut t = Tunnel::start_with(|mut params| {
            params.relay_addrs.push(second.addr.clone());
            params
        });
        for round in 0..2 {
            for second_relay in [false, true] {
                let relay = if second_relay { &second } else { &t.relay };
                let addr = relay.addr.clone();
                let (mut session, _bind) = relay.binding().await;
                session.answer(down(t.id, REFUSED)).await;
                if round == 0 {
                    let event = t.event().await;
                    assert_eq!(event.message, format!("Tunnel bind rejected by relay: {REFUSED}"));
                    assert_eq!(event.details.unwrap()["relay_addr"], addr);
                }
                session.closed().await;
                t.is_quiet(A_MOMENT).await;
            }
        }
        let _up = t.up().await;
    }

    /// The relay answers a bind before it says anything else of the side, but
    /// what it says may be read first. The answer is not taken for the later.
    #[tokio::test]
    async fn a_notice_read_before_the_answer_is_news_after_it() {
        let mut t = Tunnel::start();
        let (mut session, _bind) = t.relay.binding().await;
        session.notify(ready(t.id)).await;
        session.notify(down(t.id, "peer disconnected")).await;
        tokio::time::sleep(A_MOMENT).await;
        assert_eq!(*t.state.borrow(), Connecting);
        session.answer(ready(t.id)).await;
        t.becomes(Waiting).await;
        assert_eq!(t.told().await, "Tunnel connected to relay");
        assert_eq!(t.told().await, "Tunnel peer disconnected: peer disconnected");

        // The same of a bind sent again.
        session.notify(down(t.id, "tunnel closed by manager")).await;
        t.becomes(Down).await;
        assert_eq!(t.told().await, "Tunnel removed on relay: tunnel closed by manager");
        t.assert_binds(&session.next().await);
        session.notify(ready(t.id)).await;
        tokio::time::sleep(A_MOMENT).await;
        assert_eq!(*t.state.borrow(), Down);
        session.answer(waiting(t.id)).await;
        t.becomes(Ready).await;
        assert_eq!(t.told().await, "Tunnel ready again");
    }

    /// Removed while it waited for its peer, the edge connects anew.
    #[tokio::test]
    async fn a_side_removed_before_its_peer_came_is_tried_again() {
        let mut t = Tunnel::start();
        let (mut session, _bind) = t.relay.binding().await;
        session.answer(waiting(t.id)).await;
        t.becomes(Waiting).await;
        // A peer that was there for a moment changes nothing.
        session.notify(down(t.id, "peer unbound")).await;
        assert!(session.is_silent(A_MOMENT).await);
        assert_eq!(*t.state.borrow(), Waiting);
        session.notify(down(t.id, "tunnel closed by manager")).await;
        session.closed().await;
        assert_eq!(
            t.told().await,
            "Tunnel connection to relay failed: tunnel removed on relay: tunnel closed by manager"
        );
        t.up().await;
    }

    /// A peer that never comes: the edge says so and leaves for the next relay.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_never_comes_is_not_waited_for() {
        let mut t = Tunnel::start();
        let (mut session, _bind) = t.relay.binding().await;
        session.answer(waiting(t.id)).await;
        t.becomes(Waiting).await;
        let waiting_since = Instant::now();
        // That it is said again does not start the wait again.
        tokio::time::sleep(WAITING_TIMEOUT / 2).await;
        session.answer(waiting(t.id)).await;
        match session.next().await {
            EdgeMessage::TunnelUnbind { tunnel_id } => assert_eq!(tunnel_id, t.id),
            other => panic!("{other:?}"),
        }
        let waited = waiting_since.elapsed();
        assert!(waited >= WAITING_TIMEOUT - A_MOMENT && waited < WAITING_TIMEOUT + WAIT / 10, "{waited:?}");
        session.closed().await;
        t.up().await;
    }

    /// The relay may have far more to say than we allow it streams at a time,
    /// and what it says is read.
    #[tokio::test]
    async fn every_notice_is_read() {
        let mut t = Tunnel::start();
        let (session, _conn) = t.up().await;
        for _ in 0..400 {
            session.notify(down(t.id, "peer disconnected")).await;
            session.notify(ready(t.id)).await;
        }
        session.notify(down(t.id, "peer unbound")).await;
        t.becomes(Waiting).await;
        session.notify(ready(t.id)).await;
        t.becomes(Ready).await;
        // The cap, and the one that is left over when the peer is there again.
        tokio::time::sleep(A_MOMENT).await;
        let raised = std::iter::from_fn(|| t.events.try_recv().ok()).count();
        assert!(raised <= REPORT_BURST as usize + 1, "{raised} events");
    }

    /// Streams with nothing in them, half a message, and what we do not know,
    /// are passed over. Of those only the half holds the next up, for a while.
    #[tokio::test]
    async fn a_stream_that_says_nothing_is_passed_over() {
        let mut t = Tunnel::start();
        let (session, _conn) = t.up().await;
        drop(session.conn.open_uni().await.unwrap());
        session.conn.open_uni().await.unwrap().reset(0u32.into()).unwrap();
        let mut unknown = session.conn.open_uni().await.unwrap();
        protocol::write_message(&mut unknown, &serde_json::json!({ "type": "some_day" })).await.unwrap();
        session.notify(RelayMessage::Pong).await;
        session.notify(waiting(t.id)).await;
        session.notify(down(t.id, "peer unbound")).await;
        t.becomes(Waiting).await;

        let mut half = session.conn.open_uni().await.unwrap();
        half.write_all(&[0, 0]).await.unwrap();
        let since = Instant::now();
        session.notify(ready(t.id)).await;
        t.becomes(Ready).await;
        let held_up = since.elapsed();
        assert!(held_up < NOTICE_TIMEOUT + Duration::from_secs(2), "{held_up:?}");
    }

    /// The control stream ends and the connection does not: the edge closes
    /// it, which ends the forwarder. It does not wait to bind a removed side
    /// first.
    #[tokio::test]
    async fn the_end_of_the_control_stream_ends_the_connection() {
        for removed in [false, true] {
            let mut t = Tunnel::start();
            let (mut session, conn) = t.up().await;
            if removed {
                session.notify(down(t.id, "tunnel closed by manager")).await;
                t.becomes(Down).await;
            }
            session.send.finish().unwrap();
            session.closed().await;
            assert!(conn.close_reason().is_some());
            while let Ok(msg) = protocol::read_message(&mut session.recv).await {
                assert!(matches!(msg, EdgeMessage::Ping), "{msg:?}");
            }
        }
    }

    /// Cancelled, the edge lets the connection go: at once, and while it waits
    /// to bind again, which it then does not.
    #[tokio::test]
    async fn a_tunnel_that_is_stopped_is_not_bound_again() {
        for removed in [false, true] {
            let mut t = Tunnel::start();
            let (session, conn) = t.up().await;
            if removed {
                session.notify(down(t.id, "tunnel closed by manager")).await;
                t.becomes(Down).await;
            }
            t.cancel.cancel();
            drop(conn);
            session.closed().await;
            assert!(!t.relay.is_dialled(REBIND_MIN + A_MOMENT).await);
        }
    }

    /// The edge pings the relay on the control stream, to begin with at once.
    #[tokio::test]
    async fn the_edge_pings_the_relay() {
        let mut t = Tunnel::start();
        let (mut session, _conn) = t.up().await;
        assert!(matches!(session.raw().await, EdgeMessage::Ping));
    }

    /// Each relay in turn, and a longer and longer wait once they have all been
    /// tried.
    #[test]
    fn the_relays_are_tried_in_turn() {
        for began in [0, 1] {
            let mut walk = Walk::new(2, began);
            assert_eq!(walk.at, began);
            for wait in [1, 2, 4, 8, 16, 32, 60, 60] {
                assert_eq!(walk.step(), None);
                assert_eq!(walk.at, 1 - began);
                assert_eq!(walk.step(), Some(Duration::from_secs(wait)));
                assert_eq!(walk.at, began);
            }
        }
        let mut walk = Walk::new(1, 0);
        for wait in [1, 2, 4, 8, 16, 32, 60, 60] {
            assert_eq!(walk.step(), Some(Duration::from_secs(wait)));
            assert_eq!(walk.at, 0);
        }
    }

    /// When a tunnel tries its other relay, in seconds.
    struct Tries {
        rule: OtherRelay,
        start: Instant,
    }

    impl Tries {
        fn of(on_backup: bool) -> Self {
            let start = Instant::now();
            Self { rule: OtherRelay::new(on_backup, &Timing::default(), start), start }
        }

        fn at(&self, secs: u64) -> Instant {
            self.start + Duration::from_secs(secs)
        }

        fn is(&mut self, state: RelayTunnelState, at: u64) -> Option<u64> {
            self.rule.set(state, self.at(at));
            self.due()
        }

        /// Both sides are on the other relay.
        fn goes(&mut self, at: u64, near: bool) -> bool {
            self.rule.goes(self.at(at), || near)
        }

        /// A try that comes to nothing.
        fn fails(&mut self, at: u64) -> Option<u64> {
            self.rule.failed(self.at(at));
            self.due()
        }

        fn due(&self) -> Option<u64> {
            self.rule.due().map(|at| (at - self.start).as_secs())
        }
    }

    #[test]
    fn a_tunnel_on_its_primary_looks_for_a_peer_that_is_gone() {
        let mut tries = Tries::of(false);
        assert_eq!(tries.is(Connecting, 0), None);
        assert_eq!(tries.is(Ready, 1), None);
        assert_eq!(tries.is(Waiting, 100), Some(110));
        assert_eq!(tries.is(Waiting, 105), Some(110), "since it was first told");
        assert_eq!(tries.is(Ready, 109), None);
        assert_eq!(tries.is(Waiting, 110), Some(120));
        // Its side is removed, and bound again.
        assert_eq!(tries.is(Down, 115), None);
        assert_eq!(tries.is(Waiting, 119), Some(129));
        // Later and later, for as long as the peer is gone.
        let mut at = 129;
        for wait in [1, 2, 4, 8, 16, 32, 60, 60] {
            assert_eq!(tries.fails(at), Some(at + wait));
            at += wait;
        }
        // Soon again after the tunnel has been up, and not for a moment only.
        assert_eq!(tries.is(Ready, at), None);
        assert_eq!(tries.is(Waiting, at + 1), Some(at + 11));
        assert_eq!(tries.fails(at + 11), Some(at + 71));
        assert_eq!(tries.is(Ready, at + 71), None);
        assert_eq!(tries.is(Waiting, at + 73), Some(at + 83));
        assert_eq!(tries.fails(at + 83), Some(at + 84));
        assert_eq!(tries.fails(at + 84), Some(at + 86));
    }

    #[test]
    fn a_tunnel_on_its_backup_looks_for_its_primary() {
        let mut tries = Tries::of(true);
        assert_eq!(tries.is(Connecting, 0), None);
        assert_eq!(tries.is(Ready, 5), Some(60));
        assert_eq!(tries.fails(61), Some(121));
        assert_eq!(tries.fails(122), Some(182));
        // Sooner if its peer is gone, whenever it last tried.
        assert_eq!(tries.is(Waiting, 125), Some(135));
        assert_eq!(tries.is(Down, 130), None);
        assert_eq!(tries.is(Ready, 131), Some(182));
    }

    #[test]
    fn what_a_tunnel_is_to_have_on_its_other_relay() {
        for (on_backup, state, near, wanted, settles) in [
            (false, Connecting, true, true, true),
            (false, Waiting, false, true, true),
            (false, Down, true, true, true),
            (false, Ready, true, false, true),
            (true, Connecting, false, true, false),
            (true, Waiting, false, true, false),
            (true, Down, false, true, false),
            (true, Ready, true, true, true),
            (true, Ready, false, false, true),
        ] {
            let mut tries = Tries::of(on_backup);
            tries.is(Connecting, 0);
            tries.is(state, 5);
            let what = format!("{on_backup} {state} {near}");
            // Not on the spot, on its backup.
            let in_a_moment = tries.at(6) - Duration::from_millis(1);
            assert_eq!(tries.rule.wanted(in_a_moment, || near), wanted || on_backup, "{what}");
            tries.is(state, 6);
            assert_eq!(tries.rule.wanted(tries.at(6), || near), wanted, "{what}");
            assert_eq!(tries.rule.settle(), if settles { SETTLE } else { Duration::ZERO }, "{what}");
        }
    }

    /// A peer that is on a primary that is too far has 16 s to come to the backup.
    /// Then it is followed, if it is there still.
    #[test]
    fn a_peer_on_a_primary_that_is_too_far_has_time_to_come() {
        let mut tries = Tries::of(true);
        tries.is(Ready, 1);
        assert_eq!(tries.is(Waiting, 100), Some(110));
        assert!(!tries.goes(111, false));
        assert_eq!(tries.due(), Some(127));
        // It has only looked, and has the time still.
        tries.is(Ready, 112);
        assert_eq!(tries.is(Waiting, 113), Some(127));
        assert_eq!(tries.fails(127), Some(128), "the primary was not there");
        assert!(tries.goes(128, false));
        // It is not there any more, and comes again later.
        tries.rule.alone();
        assert!(!tries.goes(500, false));
        assert_eq!(tries.due(), Some(516));
        assert!(tries.goes(516, false));
        // It has come to the backup, and goes again.
        assert_eq!(tries.is(Ready, 517), Some(560));
        assert_eq!(tries.is(Waiting, 519), Some(529));
        assert!(!tries.goes(530, false));
    }

    /// Nothing else is too far to go to.
    #[test]
    fn where_a_tunnel_goes() {
        for on_backup in [false, true] {
            for state in [Connecting, Waiting, Ready, Down] {
                for near in [false, true] {
                    let mut tries = Tries::of(on_backup);
                    tries.is(state, 5);
                    let stays = on_backup && state != Ready && !near;
                    assert_eq!(tries.goes(6, near), !stays, "{on_backup} {state} {near}");
                }
            }
        }
    }

    /// What the tests have no time for.
    #[test]
    fn how_long_a_tunnel_waits() {
        let Timing { connect, waiting, settle, failback, refresh } = Timing::default();
        assert_eq!([connect, waiting, settle, failback, refresh].map(|t| t.as_secs()), [6, 10, 1, 60, 60]);
    }

    fn gone(reason: &str) -> Option<Trouble> {
        Some(Trouble::PeerGone(reason.into()))
    }

    /// Each change is reported while there is room, and what there is room for
    /// later is what it has come to.
    #[test]
    fn a_tunnel_that_flaps_is_reported_seldom_and_truly() {
        let start = Instant::now();
        let mut reports = Reports::new(start);
        assert_eq!(reports.due(start), None, "it is up, as reported");
        for n in 0..REPORT_BURST {
            reports.trouble = if n % 2 == 0 { gone("a") } else { None };
            assert_eq!(reports.due(start), Some(reports.trouble.clone()));
            assert_eq!(reports.due(start), None, "once");
        }
        reports.trouble = gone("b");
        assert_eq!(reports.due(start), None);
        reports.trouble = None;
        assert_eq!(reports.due(start), None);
        reports.trouble = gone("c");
        let room = start + REPORT_INTERVAL;
        assert_eq!(reports.due(room - Duration::from_millis(1)), None);
        assert_eq!(reports.due(room), Some(gone("c")));
        reports.trouble = Some(Trouble::Removed("c".into()));
        assert_eq!(reports.due(room), None);
        reports.trouble = gone("c");
        assert_eq!(reports.due(room + REPORT_INTERVAL), None, "as reported already");

        // Room is kept for a burst, and for no more.
        let later = room + REPORT_INTERVAL * 100;
        let mut raised = 0;
        for n in 0..100 {
            reports.trouble = if n % 2 == 0 { None } else { gone("d") };
            raised += u32::from(reports.due(later).is_some());
        }
        assert_eq!(raised, REPORT_BURST);
        // A minute of flapping once a second.
        let mut raised = 0;
        for n in 1..=61 {
            reports.trouble = if n % 2 == 0 { None } else { gone("e") };
            raised += u32::from(reports.due(later + Duration::from_secs(n)).is_some());
        }
        assert_eq!(raised, 2);
    }
}
