// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Relay client — manages the QUIC connection to a bilbycast-relay server.
//!
//! The relay is stateless — no authentication needed. The edge connects and
//! immediately binds tunnels by UUID. All tunnel data is encrypted end-to-end
//! between edge nodes; the relay only sees ciphertext.

use std::net::SocketAddr;
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

/// How long we tolerate a `Waiting` state (peer not yet bound) before stepping
/// to the next relay. This is the convergence mechanism when each edge has
/// landed on a different relay.
const WAITING_TIMEOUT: Duration = Duration::from_secs(10);

/// How long we wait for the QUIC handshake to a relay to complete before
/// stepping to the next relay. Bounded so a dead primary cannot stall failover
/// behind Quinn's default idle timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// Marker substring used to distinguish a waiting-timeout failure from a real
/// connect/bind error. The failover loop checks for this to avoid logging a
/// hard error when we're simply trying to converge on the same relay as the peer.
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

/// How often the failback probe task attempts to measure the primary relay.
pub const FAILBACK_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Timeout for a single RTT probe against a relay address.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// primary. Consumed by the failback probe task.
    pub failback_rtt_gate_ms: u32,
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

/// A connection to a relay, with our side of one tunnel bound on it.
struct RelayLink {
    conn: Connection,
    control: quinn::SendStream,
    tunnel_id: Uuid,
    bind: EdgeMessage,
    received: mpsc::Receiver<Received>,
    /// Read the relay's streams into `received`, and end with the link.
    _readers: JoinSet<()>,
    /// The last bind sent has not been answered yet.
    unanswered: bool,
    /// The last notice read ahead of that answer. The relay sends a notice
    /// about a side it has bound, so the notice is the later of the two.
    early: Option<News>,
}

impl RelayLink {
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
        Ok(Self {
            conn,
            control,
            tunnel_id: params.tunnel_id,
            bind: bind_message(params),
            received,
            _readers: readers,
            unanswered: false,
            early: None,
        })
    }

    /// Bind our side of the tunnel.
    async fn bind(&mut self) -> Result<()> {
        self.unanswered = true;
        protocol::write_message(&mut self.control, &self.bind).await
    }

    /// The next news of the tunnel, or why there will be none. It loses nothing
    /// if it is dropped before it is done.
    async fn news(&mut self) -> Result<News> {
        loop {
            if !self.unanswered
                && let Some(news) = self.early.take()
            {
                return Ok(news);
            }
            match self.received.recv().await {
                Some(Received::Control(msg)) => {
                    if let Some(news) = self.answer(msg) {
                        return Ok(news);
                    }
                }
                Some(Received::Notice(msg)) => match self.notice(msg) {
                    Some(news) if self.unanswered => self.early = Some(news),
                    Some(news) => return Ok(news),
                    None => {}
                },
                Some(Received::Ended(e)) => return Err(e),
                None => anyhow::bail!("relay streams are no longer read"),
            }
        }
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

    /// See to the bind of a tunnel that is up until `cancel` fires: report what
    /// becomes of the tunnel, and bind our side again if it is removed. If that
    /// is refused, or the relay is heard no more, the connection is closed,
    /// which ends the forwarder on it; the caller then connects anew.
    async fn hold(
        mut self,
        relay_addr: String,
        state_tx: watch::Sender<RelayTunnelState>,
        cancel: CancellationToken,
        event_sender: EventSender,
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
                    Ok(news @ (News::Ready | News::Waiting)) => {
                        if due.take().is_some() {
                            bound_at = Instant::now();
                        }
                        if let News::Ready = news {
                            tracing::info!(tunnel_id = %self.tunnel_id, "Tunnel ready");
                            state_tx.send_replace(RelayTunnelState::Ready);
                            reports.trouble = None;
                        } else {
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
                _ = cancel.cancelled() => return,
            }
            if let Some(report) = reports.due(Instant::now()) {
                let (severity, message, details) = match report {
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
                };
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
        self.conn.close(0u32.into(), b"");
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
enum Trouble {
    PeerGone(String),
    Removed(String),
}

/// Which changes of a tunnel's fortune are reported, and when. One that comes
/// too soon after others waits, and what is reported then is what it has come
/// to: the last report is always the one that is true.
struct Reports {
    trouble: Option<Trouble>,
    reported: Option<Trouble>,
    left: u32,
    refilled: Instant,
}

impl Reports {
    fn new(now: Instant) -> Self {
        Self { trouble: None, reported: None, left: REPORT_BURST, refilled: now }
    }

    /// The trouble to report now, or that there is none any more.
    fn due(&mut self, now: Instant) -> Option<Option<Trouble>> {
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

/// Establish a QUIC connection to the specified relay address and bind the
/// tunnel.
///
/// `relay_addr` is the address currently being attempted — chosen by the
/// caller from `params.relay_addrs`. If the peer has bound a different relay
/// and we get stuck in `Waiting` for more than [`WAITING_TIMEOUT`], this
/// function bails with an error containing [`WAITING_TIMEOUT_MSG`] so the
/// failover loop can step to the next relay without treating it as a hard
/// failure.
///
/// Returns the QUIC connection ready for data forwarding once state becomes
/// `Ready`. From then on a task of its own sees to the bind ([`RelayLink::hold`]),
/// until the connection or `cancel` ends it.
///
/// `relay_addr` is a `host:port` string. It is resolved via
/// `tokio::net::lookup_host` inside the [`CONNECT_TIMEOUT`] window so DNS
/// changes are picked up on every retry without a config reload. The
/// first returned `SocketAddr` is used — multi-record round-robin is not
/// implemented at this layer (operators wanting that can list multiple
/// addresses in `relay_addrs`).
pub async fn connect_and_bind(
    params: &RelayTunnelParams,
    relay_addr: &str,
    state_tx: &watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    event_sender: EventSender,
) -> Result<Connection> {
    // Connect to relay
    state_tx.send_replace(RelayTunnelState::Connecting);
    let endpoint = quic::make_client_endpoint(ALPN_RELAY)?;

    // Resolve host:port → SocketAddr (DNS lookup if hostname; instant if
    // already a literal). Bounded by CONNECT_TIMEOUT alongside the QUIC
    // handshake so a slow / unresponsive resolver doesn't park the
    // failover loop.
    let connect_work = async {
        let mut iter = lookup_host(relay_addr).await.map_err(|e| {
            anyhow::anyhow!("resolve relay '{relay_addr}': {e}")
        })?;
        let resolved = iter
            .next()
            .ok_or_else(|| anyhow::anyhow!("relay '{relay_addr}' resolved to no addresses"))?;
        let conn = quic::connect(&endpoint, resolved, "relay").await?;
        Ok::<(SocketAddr, Connection), anyhow::Error>((resolved, conn))
    };

    let (resolved_addr, conn) = tokio::select! {
        result = tokio::time::timeout(CONNECT_TIMEOUT, connect_work) => result
            .map_err(|_| anyhow::anyhow!("connect to relay {relay_addr} timed out after {CONNECT_TIMEOUT:?}"))??,
        _ = cancel.cancelled() => anyhow::bail!("cancelled during connect"),
    };

    tracing::info!(
        tunnel_id = %params.tunnel_id,
        relay = %relay_addr,
        resolved = %resolved_addr,
        "Connected to relay"
    );

    let mut link = RelayLink::open(conn.clone(), params).await?;
    link.bind().await?;

    // Wait for tunnel to become ready. The relay says so in answer to our bind
    // if we are the second edge to bind, and in a notice if the peer binds
    // after us.
    //
    // While in the `Waiting` state (peer not yet bound), we start a deadline
    // timer. If we remain in `Waiting` for longer than `WAITING_TIMEOUT`, we
    // bail so the caller can step to the next relay — this is how two edges
    // that independently landed on different relays converge.
    let mut deadline: Option<Instant> = None;
    loop {
        let waited_out = async {
            match deadline {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            news = link.news() => match news? {
                News::Ready => break,
                News::Waiting => {
                    state_tx.send_replace(RelayTunnelState::Waiting);
                    deadline.get_or_insert(Instant::now() + WAITING_TIMEOUT);
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        "Waiting for peer to bind"
                    );
                }
                // It came and went before we were told that it came.
                News::PeerGone(_) => {}
                News::Removed(reason) => anyhow::bail!("tunnel removed on relay: {reason}"),
                News::Refused(reason) => return Err(BindRejected(reason).into()),
            },
            _ = waited_out => {
                // Peer hasn't bound on this relay within the window. Unbind
                // politely so the relay drops its pending state, then bail.
                let _ = protocol::write_message(
                    &mut link.control,
                    &EdgeMessage::TunnelUnbind { tunnel_id: params.tunnel_id },
                ).await;
                tracing::info!(
                    tunnel_id = %params.tunnel_id,
                    relay = %relay_addr,
                    "Peer hasn't bound on this relay within {WAITING_TIMEOUT:?}, stepping to next relay"
                );
                anyhow::bail!("{WAITING_TIMEOUT_MSG}: peer not bound on {relay_addr}");
            }
            _ = cancel.cancelled() => anyhow::bail!("cancelled waiting for tunnel ready"),
        }
    }

    state_tx.send_replace(RelayTunnelState::Ready);
    tracing::info!(
        tunnel_id = %params.tunnel_id,
        direction = %params.direction,
        "Tunnel ready"
    );
    event_sender.emit_flow_with_details(
        EventSeverity::Info,
        category::TUNNEL,
        "Tunnel connected to relay",
        &params.tunnel_id.to_string(),
        serde_json::json!({ "relay_addr": relay_addr.to_string() }),
    );

    tokio::spawn(link.hold(relay_addr.to_string(), state_tx.clone(), cancel, event_sender));

    Ok(conn)
}

/// Connect to a relay with ordered failover + exponential backoff.
///
/// Walks `params.relay_addrs` starting from `*active_idx`, stepping forward on
/// each failure. After trying every relay in the list once (a full cycle), it
/// sleeps with exponential backoff before retrying.
///
/// `active_idx` is shared mutable state owned by the caller so that reconnect
/// cycles are sticky: when the forwarder drops and we reconnect, we resume on
/// the same relay we were using (and only step forward on sustained failure).
///
/// `active_idx_tx` publishes the currently-active relay index to observers
/// (stats snapshot, failback probe).
pub async fn connect_with_retry(
    params: &RelayTunnelParams,
    active_idx: &mut usize,
    active_idx_tx: &watch::Sender<usize>,
    state_tx: &watch::Sender<RelayTunnelState>,
    cancel: CancellationToken,
    event_sender: EventSender,
) -> Result<Connection> {
    if params.relay_addrs.is_empty() {
        anyhow::bail!("relay_addrs is empty — at least one relay is required");
    }

    let n = params.relay_addrs.len();
    if *active_idx >= n {
        *active_idx = 0;
    }
    let initial_idx = *active_idx;

    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);
    // The relays that have refused our bind since we began. That is an alarm
    // once, and a refusal may go on for as long as nobody sees to it.
    let mut refused_by = vec![false; n];

    loop {
        let relay_addr: &str = &params.relay_addrs[*active_idx];
        match connect_and_bind(params, relay_addr, state_tx, cancel.clone(), event_sender.clone())
            .await
        {
            Ok(conn) => {
                active_idx_tx.send_replace(*active_idx);
                if *active_idx != initial_idx {
                    let from: &str = &params.relay_addrs[initial_idx];
                    tracing::warn!(
                        tunnel_id = %params.tunnel_id,
                        "Tunnel failed over from {from} to {relay_addr} (idx {initial_idx} → {})",
                        *active_idx
                    );
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning,
                        category::TUNNEL,
                        format!("Tunnel failed over to backup relay {relay_addr}"),
                        &params.tunnel_id.to_string(),
                        serde_json::json!({
                            "from_relay_addr": from,
                            "to_relay_addr": relay_addr,
                            "from_idx": initial_idx,
                            "to_idx": *active_idx,
                        }),
                    );
                }
                return Ok(conn);
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    return Err(e);
                }
                let err_msg = e.to_string();
                let is_waiting_timeout = err_msg.contains(WAITING_TIMEOUT_MSG);

                if is_waiting_timeout {
                    // Expected convergence step, not a hard failure.
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        relay = %relay_addr,
                        "Stepping to next relay (waiting timeout)"
                    );
                } else if e.is::<BindRejected>() {
                    tracing::warn!(
                        tunnel_id = %params.tunnel_id,
                        relay = %relay_addr,
                        "Tunnel bind rejected by relay: {e}"
                    );
                    if !std::mem::replace(&mut refused_by[*active_idx], true) {
                        event_sender.emit_flow_with_details(
                            EventSeverity::Critical,
                            category::TUNNEL,
                            format!("Tunnel bind rejected by relay: {e}"),
                            &params.tunnel_id.to_string(),
                            serde_json::json!({ "relay_addr": relay_addr.to_string() }),
                        );
                    }
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
                // to `initial_idx` we've tried every relay once this cycle —
                // sleep with exponential backoff before the next round.
                let next = (*active_idx + 1) % n;
                *active_idx = next;
                active_idx_tx.send_replace(*active_idx);

                if next == initial_idx {
                    tracing::info!(
                        tunnel_id = %params.tunnel_id,
                        "All {n} relays failed this cycle, sleeping {backoff:?} before retry"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => anyhow::bail!("cancelled during retry backoff"),
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }
}

/// Measure the smoothed QUIC RTT to `relay_addr` by opening a short-lived
/// probe connection and performing the Hello / HelloAck handshake.
///
/// `relay_addr` is a `host:port` string (IP literal or DNS name). Resolution
/// happens on every probe so an IP change behind a hostname is observed
/// without restarting the failback task.
///
/// Used by the failback probe to decide whether the primary relay is healthy
/// enough (RTT-wise) to resume traffic on. The connection is dropped at the
/// end of the call.
pub async fn measure_relay_rtt(
    relay_addr: &str,
    cancel: CancellationToken,
) -> Result<Duration> {
    let target = relay_addr.to_string();
    let work = async move {
        let endpoint = quic::make_client_endpoint(ALPN_RELAY)?;
        let resolved = lookup_host(target.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("resolve relay '{target}': {e}"))?
            .next()
            .ok_or_else(|| anyhow::anyhow!("relay '{target}' resolved to no addresses"))?;
        let conn = tokio::select! {
            result = quic::connect(&endpoint, resolved, "relay") => result?,
            _ = cancel.cancelled() => anyhow::bail!("cancelled during probe connect"),
        };
        let (mut send, mut recv) = conn.open_bi().await?;
        protocol::write_message(
            &mut send,
            &EdgeMessage::Hello {
                protocol_version: TUNNEL_PROTOCOL_VERSION,
                software_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await?;
        // Wait for HelloAck so we know the relay is responsive at the
        // application layer (not just the QUIC handshake).
        let _ = protocol::read_message_resilient::<RelayMessage>(&mut recv).await?;
        Ok::<Duration, anyhow::Error>(conn.rtt())
    };

    match tokio::time::timeout(PROBE_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("probe to {relay_addr} timed out after {PROBE_TIMEOUT:?}"),
    }
}

#[cfg(test)]
pub(super) mod stand_in {
    //! A relay that says what the test tells it to, over the real protocol.
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

        /// The next connection to come, with its control stream.
        pub(crate) async fn session(&self) -> Session {
            let incoming = soon("a connection", self.endpoint.accept()).await.unwrap();
            let conn = soon("its handshake", incoming.into_future()).await.unwrap();
            let (send, recv) = soon("its control stream", conn.accept_bi()).await.unwrap();
            Session { conn, send, recv }
        }

        /// [`Self::session`], read up to its bind.
        pub(crate) async fn binding(&self) -> (Session, EdgeMessage) {
            let mut session = self.session().await;
            let bind = session.next().await;
            (session, bind)
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
            });
            let id = params.tunnel_id;
            let (state_tx, state) = watch::channel(Connecting);
            let (event_tx, events) = event_channel();
            let cancel = CancellationToken::new();
            let token = cancel.clone();
            let connect = tokio::spawn(async move {
                let (active_tx, _active) = watch::channel(0);
                connect_with_retry(&params, &mut 0, &active_tx, &state_tx, token, event_tx).await
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
