// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The QUIC connection that a tunnel's forwarder runs on.

use std::time::Duration;

use bytes::Bytes;
use quinn::{Connection, ConnectionError, RecvStream, SendStream};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};

/// How long what comes on a connection is still taken after the tunnel has
/// moved to another. The peer moves when it is told to, not when we are, and
/// what it sent until then is on its way here.
pub const DRAIN: Duration = Duration::from_secs(2);

/// The connection of a tunnel. It may be replaced while a forwarder runs on it:
/// the local sockets stay as they are, and so do the streams that are open, on
/// the connection they were opened on.
pub struct Carrier {
    conn: Connection,
    moves: mpsc::Receiver<Connection>,
    /// The connection the tunnel has moved from, and until when it is read.
    left: Option<(Connection, Instant)>,
}

impl Carrier {
    /// The tunnel moves to each connection that comes through `moves`.
    pub fn new(conn: Connection, moves: mpsc::Receiver<Connection>) -> Self {
        Self { conn, moves, left: None }
    }

    /// The connection to send on.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The next datagram to come. It loses none if it is dropped before it is
    /// done.
    pub async fn read_datagram(&mut self) -> Result<Bytes, ConnectionError> {
        self.next(async |conn| conn.read_datagram().await).await
    }

    /// The next stream that the other side opens.
    pub async fn accept_bi(&mut self) -> Result<(SendStream, RecvStream), ConnectionError> {
        self.next(async |conn| conn.accept_bi().await).await
    }

    /// Why the tunnel's connection has ended.
    pub async fn closed(&mut self) -> ConnectionError {
        let ended = self.next(async |conn| Err::<std::convert::Infallible, _>(conn.closed().await));
        match ended.await {
            Ok(never) => match never {},
            Err(reason) => reason,
        }
    }

    /// What comes first, on the tunnel's connection or on the one it has left.
    /// That the latter ends is no matter.
    async fn next<T>(
        &mut self,
        from: impl AsyncFn(&Connection) -> Result<T, ConnectionError>,
    ) -> Result<T, ConnectionError> {
        loop {
            let from_left = async {
                match &self.left {
                    Some((conn, until)) => tokio::select! {
                        got = from(conn) => got.ok(),
                        _ = sleep_until(*until) => None,
                    },
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                Some(conn) = self.moves.recv() => {
                    let left = std::mem::replace(&mut self.conn, conn);
                    self.left = Some((left, Instant::now() + DRAIN));
                }
                got = from_left => match got {
                    Some(got) => return Ok(got),
                    None => self.left = None,
                },
                got = from(&self.conn) => return got,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::protocol::ALPN_RELAY;
    use crate::tunnel::quic;
    use crate::tunnel::relay_client::stand_in::{StandIn, soon};
    use tokio::time::timeout;

    const A_MOMENT: Duration = Duration::from_millis(300);

    /// A connection, from both of its ends. Ours is first.
    async fn connection() -> (Connection, Connection) {
        let relay = StandIn::start();
        let endpoint = quic::make_client_endpoint(ALPN_RELAY).unwrap();
        let connect = quic::connect(&endpoint, relay.addr.parse().unwrap(), "relay");
        let (ours, theirs) = tokio::join!(connect, relay.accept());
        (ours.unwrap(), theirs)
    }

    async fn reads(carrier: &mut Carrier, want: &'static [u8]) {
        let got = soon("a datagram", carrier.read_datagram()).await.unwrap();
        assert_eq!(&got[..], want);
    }

    async fn rests(carrier: &mut Carrier) {
        assert!(timeout(A_MOMENT, carrier.read_datagram()).await.is_err());
    }

    fn send(conn: &Connection, what: &'static [u8]) {
        conn.send_datagram(Bytes::from_static(what)).unwrap();
    }

    /// Both connections are read, and the one that is left may end.
    #[tokio::test]
    async fn what_comes_on_the_connection_left_is_taken() {
        let ((first, at_first), (second, at_second)) = (connection().await, connection().await);
        let (moves, moves_rx) = mpsc::channel(1);
        let mut carrier = Carrier::new(first.clone(), moves_rx);
        send(&at_first, b"1");
        reads(&mut carrier, b"1").await;
        assert_eq!(carrier.conn().stable_id(), first.stable_id());

        moves.send(second.clone()).await.unwrap();
        rests(&mut carrier).await;
        assert_eq!(carrier.conn().stable_id(), second.stable_id());
        send(&at_second, b"2");
        reads(&mut carrier, b"2").await;
        send(&at_first, b"3");
        reads(&mut carrier, b"3").await;
        at_first.close(0u32.into(), b"");
        rests(&mut carrier).await;
        send(&at_second, b"4");
        reads(&mut carrier, b"4").await;
        // Nor does it matter that there is nowhere to move any more.
        drop(moves);
        rests(&mut carrier).await;
        at_second.close(0u32.into(), b"");
        soon("its end", carrier.read_datagram()).await.unwrap_err();
    }

    /// For a while, and then it is let go of, whether anything comes on the
    /// other or not.
    #[tokio::test]
    async fn the_connection_left_is_let_go_of() {
        let ((first, at_first), (second, at_second)) = (connection().await, connection().await);
        let (moves, moves_rx) = mpsc::channel(1);
        let mut carrier = Carrier::new(first, moves_rx);
        let since = Instant::now();
        moves.send(second).await.unwrap();
        tokio::select! {
            _ = carrier.read_datagram() => panic!("there is nothing to read"),
            _ = soon("its end", at_first.closed()) => {}
        }
        assert!(since.elapsed() >= DRAIN, "{:?}", since.elapsed());
        send(&at_second, b"1");
        reads(&mut carrier, b"1").await;
    }

    /// A connection that ends as the tunnel moves from it is not its end.
    #[tokio::test]
    async fn a_move_comes_before_an_end() {
        for closes in [false, true].repeat(5) {
            let ((first, at_first), (second, at_second)) = (connection().await, connection().await);
            let (moves, moves_rx) = mpsc::channel(1);
            let mut carrier = Carrier::new(first.clone(), moves_rx);
            at_first.close(0u32.into(), b"");
            soon("its end", first.closed()).await;
            moves.send(second).await.unwrap();
            if closes {
                assert!(timeout(A_MOMENT, carrier.closed()).await.is_err());
                at_second.close(0u32.into(), b"");
                soon("its end", carrier.closed()).await;
            } else {
                send(&at_second, b"1");
                reads(&mut carrier, b"1").await;
            }
        }
    }

    /// Streams are accepted on both. What keeps the one that is left is the
    /// streams that are open on it.
    #[tokio::test]
    async fn streams_are_accepted_on_both() {
        let ((first, at_first), (second, at_second)) = (connection().await, connection().await);
        let (moves, moves_rx) = mpsc::channel(1);
        let mut carrier = Carrier::new(first, moves_rx);
        moves.send(second).await.unwrap();
        let mut open = Vec::new();
        for (conn, n) in [(&at_second, 2), (&at_first, 1)] {
            let (mut send, recv) = conn.open_bi().await.unwrap();
            send.write_all(&[n]).await.unwrap();
            let (back, mut there) = soon("a stream", carrier.accept_bi()).await.unwrap();
            let mut got = [0];
            there.read_exact(&mut got).await.unwrap();
            assert_eq!(got, [n]);
            open.push((send, recv, back, there));
        }
        assert!(timeout(DRAIN + A_MOMENT, carrier.accept_bi()).await.is_err());
        assert!(at_first.close_reason().is_none());
        open.pop();
        soon("its end", at_first.closed()).await;
        assert!(at_second.close_reason().is_none());
    }
}
