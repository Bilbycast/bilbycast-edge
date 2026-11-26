// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The carrier a native-UDP relay tunnel, or a relayed bond leg, rides for its
//! whole life: the native counterpart of [`super::carrier::Carrier`].
//!
//! It holds up to one socket per relay (a *leg*, indexed by the relay's place
//! in `relay_addrs`), sends on the active one and reads every one, so the media
//! the peer still sends by the relay just left arrives as surely as what comes
//! by the relay moved to. Its supervisor (`udp_relay_client::run_native_relay`)
//! hands it a new [`CarrierSet`] whenever a socket opens or closes or the
//! tunnel moves, and gets back a [`CarrierNote`] for every Ack and refusal the
//! sockets see. The forwarder over it never ends for any of that: not for a
//! move, a new socket, nor a refused datagram.
//!
//! No lock: the sets come by a channel the forwarder drains as it receives,
//! and the per-leg refusal throttle is an atomic.

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use tokio::io::{Interest, Ready};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

use super::protocol::{UdpRelayControl, try_decode_udp_control};
use super::udp_forwarder::DatagramLink;

/// How many notes may wait for the supervisor. Past that a note is dropped:
/// the next Register brings another Ack.
pub(crate) const NOTES_CAPACITY: usize = 32;

/// At most one refusal note per leg in this long. A refused media datagram is
/// a refused send per datagram, and the supervisor needs to hear of it once.
const REFUSED_NOTE_EVERY: Duration = Duration::from_secs(1);

/// The receive buffer: a datagram is at most a TS payload of 1316 bytes plus
/// the AEAD's 28 and the tunnel id's 16, and a control datagram less.
const RECV_BUF: usize = 2048;

/// One relay's socket.
pub(crate) struct Leg {
    /// Which of the relay's sockets this is: a note names it, so that one about
    /// a socket since replaced is not taken for the one there now.
    pub(crate) generation: u64,
    pub(crate) sock: Arc<UdpSocket>,
    /// Milliseconds after the carrier's epoch of the last refusal noted on this
    /// leg, 0 for none.
    refused_ms: AtomicU64,
}

impl Leg {
    pub(crate) fn new(generation: u64, sock: Arc<UdpSocket>) -> Self {
        Self { generation, sock, refused_ms: AtomicU64::new(0) }
    }
}

/// What the carrier sends on and reads: it replaces both wholesale.
pub(crate) struct CarrierSet {
    pub(crate) legs: [Option<Leg>; 2],
    pub(crate) active: Option<usize>,
}

/// What a relay said in an Ack for our tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayAck {
    pub(crate) ready: bool,
    pub(crate) peer_live: Option<bool>,
    pub(crate) live_ms: Option<u64>,
    pub(crate) relay_id: Option<String>,
    pub(crate) seq: Option<u32>,
}

/// What the sockets saw, for the supervisor.
#[derive(Debug)]
pub(crate) enum CarrierNote {
    /// An Ack naming our tunnel came on leg `idx`.
    Ack { idx: usize, generation: u64, at: Instant, ack: RelayAck },
    /// A datagram to leg `idx`'s relay was refused (ICMP port or host
    /// unreachable): nothing listens there.
    Refused { idx: usize, generation: u64, at: Instant },
}

/// Is `e` the error a datagram to a host with nothing listening comes back as?
pub(crate) fn is_refusal(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable
    )
}

/// See the module docs.
pub(crate) struct NativeCarrier {
    tunnel_id: Uuid,
    legs: [Option<Leg>; 2],
    active: Option<usize>,
    sets: mpsc::UnboundedReceiver<CarrierSet>,
    notes: mpsc::Sender<CarrierNote>,
    buf: Vec<u8>,
    epoch: Instant,
    /// Which leg is looked at first on the next receive. It alternates, so a
    /// flood on one leg cannot keep the other's Acks unread.
    next_first: usize,
}

impl NativeCarrier {
    pub(crate) fn new(
        tunnel_id: Uuid,
        sets: mpsc::UnboundedReceiver<CarrierSet>,
        notes: mpsc::Sender<CarrierNote>,
    ) -> Self {
        Self {
            tunnel_id,
            legs: [None, None],
            active: None,
            sets,
            notes,
            buf: vec![0u8; RECV_BUF],
            epoch: Instant::now(),
            next_first: 0,
        }
    }

    /// Note a refusal on leg `idx`, at most once per [`REFUSED_NOTE_EVERY`].
    fn refused(&self, idx: usize, leg: &Leg) {
        let now = self.epoch.elapsed().as_millis() as u64 + 1;
        let last = leg.refused_ms.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < REFUSED_NOTE_EVERY.as_millis() as u64 {
            return;
        }
        leg.refused_ms.store(now, Ordering::Relaxed);
        let _ = self.notes.try_send(CarrierNote::Refused { idx, generation: leg.generation, at: Instant::now() });
    }
}

/// Wait until leg `leg` has something to read, or an error to report — the
/// refusal of a datagram sent on it, which wakes no receive — or for ever if
/// there is none.
async fn readable(leg: Option<&Leg>) -> std::io::Result<Ready> {
    match leg {
        Some(leg) => leg.sock.ready(Interest::READABLE | Interest::ERROR).await,
        None => std::future::pending().await,
    }
}

/// Take the error waiting on `sock`, if any: `WouldBlock` (and its readiness
/// cleared) when there is none.
fn take_error(sock: &UdpSocket) -> std::io::Result<()> {
    sock.try_io(Interest::ERROR, || match sock.take_error()? {
        Some(e) => Err(e),
        None => Err(ErrorKind::WouldBlock.into()),
    })
}

enum Woken {
    Set(Option<CarrierSet>),
    Readable(usize, std::io::Result<Ready>),
}

impl DatagramLink for NativeCarrier {
    fn send_datagram(&self, data: Bytes) -> Result<()> {
        let Some((idx, leg)) = self.active.and_then(|i| Some((i, self.legs[i].as_ref()?))) else {
            anyhow::bail!("udp datagram dropped: no relay socket open");
        };
        match leg.sock.try_send(&data) {
            Ok(_) => Ok(()),
            // Buffer full → the datagram is dropped, as a QUIC datagram is.
            // An error, so the caller counts it in `send_errors`, not sent.
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                Err(anyhow::anyhow!("udp datagram dropped: socket send buffer full"))
            }
            Err(e) => {
                if is_refusal(&e) {
                    self.refused(idx, leg);
                }
                Err(anyhow::anyhow!("udp datagram send: {e}"))
            }
        }
    }

    /// The next media datagram, from whichever leg. Ends only when the
    /// supervisor has gone; with no leg open it waits for one.
    async fn recv_datagram(&mut self) -> Result<Bytes> {
        loop {
            let first = self.next_first;
            self.next_first ^= 1;
            let woken = tokio::select! {
                biased;
                set = self.sets.recv() => Woken::Set(set),
                r = readable(self.legs[first].as_ref()) => Woken::Readable(first, r),
                r = readable(self.legs[first ^ 1].as_ref()) => Woken::Readable(first ^ 1, r),
            };
            let (idx, woke) = match woken {
                Woken::Set(Some(set)) => {
                    (self.legs, self.active) = (set.legs, set.active);
                    continue;
                }
                Woken::Set(None) => anyhow::bail!("native relay carrier: its supervisor has gone"),
                Woken::Readable(idx, Ok(woke)) => (idx, woke),
                Woken::Readable(idx, Err(e)) => {
                    tracing::debug!(tunnel_id = %self.tunnel_id, "native relay leg {idx} not readable: {e}");
                    continue;
                }
            };
            let Some(leg) = self.legs[idx].as_ref() else { continue };
            if woke.is_error() {
                // A Register refused while nothing else is sent or comes on
                // the leg (the receiving side of a one-way flow) shows only
                // here, and is what speeds up the Registers that find a
                // restarted relay.
                match take_error(&leg.sock) {
                    Err(e) if is_refusal(&e) => self.refused(idx, leg),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => tracing::debug!(tunnel_id = %self.tunnel_id, "native relay leg {idx} error: {e}"),
                    Ok(()) => {}
                }
                if !woke.is_readable() {
                    continue;
                }
            }
            let n = match leg.sock.try_recv(&mut self.buf) {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(e) if is_refusal(&e) => {
                    self.refused(idx, leg);
                    continue;
                }
                Err(e) => {
                    tracing::debug!(tunnel_id = %self.tunnel_id, "native relay leg {idx} receive: {e}");
                    continue;
                }
            };
            let data = &self.buf[..n];
            match try_decode_udp_control(data) {
                Some(UdpRelayControl::Ack { tunnel_id, ready, peer_live, live_ms, relay_id, seq })
                    if tunnel_id == self.tunnel_id =>
                {
                    let ack = RelayAck { ready, peer_live, live_ms, relay_id, seq };
                    let note = CarrierNote::Ack { idx, generation: leg.generation, at: Instant::now(), ack };
                    let _ = self.notes.try_send(note);
                }
                // Anything else on the control plane is not for us.
                Some(_) => {}
                None => return Ok(Bytes::copy_from_slice(data)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::protocol::{encode_udp_control, encode_udp_datagram};
    use crate::tunnel::udp_forwarder::{self, UdpForwarderStats};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    const PATIENCE: Duration = Duration::from_secs(2);

    /// A relay's end of a leg: a socket the leg is connected to.
    struct Far {
        sock: UdpSocket,
    }

    impl Far {
        async fn new() -> Self {
            Self { sock: UdpSocket::bind("127.0.0.1:0").await.unwrap() }
        }

        /// A leg connected to it, and the leg's address.
        async fn leg(&self, generation: u64) -> (Leg, std::net::SocketAddr) {
            let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            sock.connect(self.sock.local_addr().unwrap()).await.unwrap();
            let addr = sock.local_addr().unwrap();
            (Leg::new(generation, Arc::new(sock)), addr)
        }
    }

    fn ack(tunnel_id: Uuid, seq: u32) -> Vec<u8> {
        encode_udp_control(&UdpRelayControl::Ack {
            tunnel_id,
            ready: true,
            peer_live: Some(true),
            live_ms: Some(1),
            relay_id: Some("ab".into()),
            seq: Some(seq),
        })
        .unwrap()
    }

    /// A carrier with `legs`, and the ends that steer it.
    fn carrier(
        tunnel_id: Uuid,
        legs: [Option<Leg>; 2],
        active: Option<usize>,
    ) -> (NativeCarrier, mpsc::UnboundedSender<CarrierSet>, mpsc::Receiver<CarrierNote>) {
        let (sets_tx, sets) = mpsc::unbounded_channel();
        let (notes, notes_rx) = mpsc::channel(NOTES_CAPACITY);
        sets_tx.send(CarrierSet { legs, active }).unwrap();
        (NativeCarrier::new(tunnel_id, sets, notes), sets_tx, notes_rx)
    }

    /// After a move the peer may still send by the relay left for a while:
    /// that comes in as surely as what comes by the relay moved to.
    #[tokio::test]
    async fn what_comes_on_the_leg_left_is_taken() {
        let id = Uuid::new_v4();
        let (left, moved_to) = (Far::new().await, Far::new().await);
        let (leg0, addr0) = left.leg(1).await;
        let (leg1, addr1) = moved_to.leg(2).await;
        let (mut c, _sets, _notes) = carrier(id, [Some(leg0), Some(leg1)], Some(1));
        left.sock.send_to(&encode_udp_datagram(&id, b"by the old"), addr0).await.unwrap();
        let got = timeout(PATIENCE, c.recv_datagram()).await.unwrap().unwrap();
        assert_eq!(&got[16..], b"by the old");
        moved_to.sock.send_to(&encode_udp_datagram(&id, b"by the new"), addr1).await.unwrap();
        let got = timeout(PATIENCE, c.recv_datagram()).await.unwrap().unwrap();
        assert_eq!(&got[16..], b"by the new");
        // And what is sent goes by the active leg only.
        c.send_datagram(Bytes::from_static(b"out")).unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = timeout(PATIENCE, moved_to.sock.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!((&buf[..n], from), (&b"out"[..], addr1));
    }

    /// A refused datagram (nothing listening at the relay's port) used to end
    /// the forwarder, which then rotated relays. Now the forwarder goes on,
    /// and delivers as soon as something listens there.
    #[tokio::test]
    async fn a_refused_datagram_does_not_end_the_forwarder() {
        let id = Uuid::new_v4();
        let relay_addr = {
            let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap()
        };
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(relay_addr).await.unwrap();
        let (c, _sets, mut notes) = carrier(id, [Some(Leg::new(7, Arc::new(sock))), None], Some(0));
        let local_addr = {
            let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap()
        };
        let stats = Arc::new(UdpForwarderStats::default());
        let cancel = CancellationToken::new();
        let fwd = tokio::spawn(udp_forwarder::run_egress(id, local_addr, c, stats.clone(), cancel.clone(), None));
        let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let _ = app.send_to(b"media", local_addr).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!fwd.is_finished(), "a refused datagram ended the forwarder");
        assert!(
            matches!(notes.try_recv(), Ok(CarrierNote::Refused { idx: 0, generation: 7, .. })),
            "the refusal is noted for the supervisor"
        );
        let relay = UdpSocket::bind(relay_addr).await.unwrap();
        let mut buf = [0u8; 64];
        let got = timeout(PATIENCE, async {
            loop {
                let _ = app.send_to(b"media", local_addr).await;
                if let Ok(Ok(n)) = timeout(Duration::from_millis(50), relay.recv(&mut buf)).await {
                    return n;
                }
            }
        })
        .await
        .expect("the forwarder delivers once the relay is back");
        assert_eq!(&buf[16..got], b"media");
        cancel.cancel();
        let _ = timeout(PATIENCE, fwd).await;
    }

    /// A flood on one leg must not keep the other leg's Acks unread, or the
    /// relay on it looks silent and the tunnel moves for nothing.
    #[tokio::test]
    async fn neither_leg_starves() {
        let id = Uuid::new_v4();
        let (flood, quiet) = (Far::new().await, Far::new().await);
        let (leg0, addr0) = flood.leg(1).await;
        let (leg1, addr1) = quiet.leg(2).await;
        let (mut c, _sets, mut notes) = carrier(id, [Some(leg0), Some(leg1)], Some(0));
        let reader = tokio::spawn(async move {
            loop {
                if c.recv_datagram().await.is_err() {
                    return;
                }
            }
        });
        let stop = CancellationToken::new();
        let flooding = {
            let stop = stop.clone();
            let datagram = encode_udp_datagram(&id, &[0u8; 1316]);
            tokio::spawn(async move {
                while !stop.is_cancelled() {
                    for _ in 0..64 {
                        let _ = flood.sock.send_to(&datagram, addr0).await;
                    }
                    tokio::task::yield_now().await;
                }
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        quiet.sock.send_to(&ack(id, 5), addr1).await.unwrap();
        let note = timeout(Duration::from_millis(100), async {
            loop {
                if let Some(CarrierNote::Ack { idx: 1, ack, .. }) = notes.recv().await {
                    return ack;
                }
            }
        })
        .await
        .expect("the quiet leg's Ack was not read within 100 ms of a flood on the other");
        assert_eq!(note.seq, Some(5));
        stop.cancel();
        let _ = flooding.await;
        reader.abort();
    }

    /// A refused datagram the supervisor sent (a Register), with nothing to
    /// read on the leg, is noted too. It used to show only once something else
    /// came or went on the leg: the receiving side of a one-way flow heard of
    /// a relay restart at its next keepalive.
    #[tokio::test]
    async fn a_refusal_with_nothing_to_read_is_noted() {
        let id = Uuid::new_v4();
        let closed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        sock.connect(closed).await.unwrap();
        let (mut c, _sets, mut notes) = carrier(id, [Some(Leg::new(4, sock.clone())), None], Some(0));
        let reader = tokio::spawn(async move {
            let _ = c.recv_datagram().await;
        });
        sock.send(b"register").await.unwrap();
        let note = timeout(PATIENCE, notes.recv()).await.expect("the refusal was never noted");
        assert!(matches!(note, Some(CarrierNote::Refused { idx: 0, generation: 4, .. })), "{note:?}");
        reader.abort();
    }

    #[tokio::test]
    async fn an_ack_for_another_tunnel_is_not_noted() {
        let id = Uuid::new_v4();
        let relay = Far::new().await;
        let (leg, addr) = relay.leg(3).await;
        let (mut c, _sets, mut notes) = carrier(id, [Some(leg), None], Some(0));
        relay.sock.send_to(&ack(Uuid::new_v4(), 1), addr).await.unwrap();
        relay.sock.send_to(&ack(id, 2), addr).await.unwrap();
        relay.sock.send_to(&encode_udp_datagram(&id, b"media"), addr).await.unwrap();
        let got = timeout(PATIENCE, c.recv_datagram()).await.unwrap().unwrap();
        assert_eq!(&got[16..], b"media", "control datagrams never surface as media");
        match notes.try_recv() {
            Ok(CarrierNote::Ack { idx: 0, generation: 3, ack, .. }) => assert_eq!(ack.seq, Some(2)),
            other => panic!("expected our Ack alone, got {other:?}"),
        }
        assert!(notes.try_recv().is_err(), "the other tunnel's Ack was noted");
    }
}
