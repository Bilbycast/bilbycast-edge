//! A deep intake between the flow's broadcast and one CMAF output.
//!
//! The output's loop does more than read packets: closing a segment awaits the
//! origin's answer to the upload, and publishing the init or a playlist awaits
//! a PUT. While it waits, nothing reads the broadcast, and the flow keeps
//! publishing into a channel sized in packets — 16,384 for a standard flow,
//! about 2.4 s of an SDI feed that broadcasts one 188-byte TS packet per
//! message. A wait longer than that came back as `Lagged`: those packets were
//! gone, and the viewer saw a hole in the picture. On bilby-voyager
//! (2026-10-22) every output of five sessions lagged in the same millisecond,
//! several times a minute, losing 3–4 s each time.
//!
//! The intake is a task of its own that only moves packets from the broadcast
//! into a queue bounded in bytes, so an output that is waiting falls behind
//! rather than losing anything, and catches up when the wait ends. The bound is
//! what a stuck output may cost in memory; past it, packets are dropped and
//! reported exactly as a broadcast lag is, so the loop's handling — the
//! `packets_dropped` stat and the warning — is unchanged.
//!
//! Bounded twice: by the output's own budget, and by one every intake on the
//! node shares ([`NODE_INTAKES`]), because an origin stall reaches every
//! rendition of a flow at once and the outputs on a node are as many as are
//! configured. Past either, the same drop and the same report.
//!
//! It cannot help if the whole runtime stalls: the intake is a task like any
//! other and would stall with it. It turns a slow peer into latency instead of
//! loss, which is the case the output itself can do something about.
//!
//! Latency that the output must not mistake for the time. A segment is dated
//! by the instant it closed, and the flow's wall-clock epoch is steered by that
//! date — re-anchored outright past `EPOCH_REANCHOR_SECS` (10 s) — so an
//! output catching up on a backlog would date each segment it closes as if its
//! packets had just arrived: past ten seconds behind, a re-anchor and an
//! `#EXT-X-DISCONTINUITY` on every rendition of the flow, and a rendition that
//! stayed that far behind its sibling would re-anchor the two against each
//! other on every segment. The broadcast ring never allowed that — it was 2.4 s
//! of an SDI feed — and this queue does. So the intake records when it took
//! each packet off the broadcast, and [`Intake::waited`] says how long the one
//! just returned sat queued, for the output to date by arrival.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::engine::packet::RtpPacket;
use crate::util::byte_budget::{NodeBudget, QueueBudget};

/// Memory an output may hold unread before the intake drops, held only while
/// the output is behind: each packet is charged its payload plus
/// [`ITEM_OVERHEAD_BYTES`]. In time that is ~15 s of a 10 Mbit/s SDI feed
/// (one 188-byte TS packet per message), ~24 s of 1316-byte datagrams at
/// 10 Mbit/s, and more on a frame-per-message feed (RTMP, RTSP). Per output:
/// the renditions of a flow share one origin connection, so a stall that
/// fills one tends to fill them all — which is why the node's intakes are
/// bounded together too, by [`NODE_INTAKE_BUDGET_BYTES`].
pub(super) const INTAKE_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// What every CMAF intake on the node may hold between them: eight outputs'
/// full budgets.
///
/// Without it the bound was the per-output one times the outputs configured —
/// five DVR sessions of two renditions each, 320 MiB, all of it filled at once
/// by one origin stall. Through such a stall those ten outputs still get
/// 25.6 MiB each, about 11 s of a 10 Mbit/s SDI feed, against the 3–4 s
/// stalls bilby-voyager lost packets to (2026-10-22); a node with more
/// outputs, or a longer stall, drops sooner, and that is the bound doing its
/// job. Charged, not resident: a flow's renditions queue clones of the same
/// payload, which is held once.
pub(super) const NODE_INTAKE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// The budget every CMAF intake on the node shares.
pub(super) static NODE_INTAKES: NodeBudget = NodeBudget::new(NODE_INTAKE_BUDGET_BYTES);

/// What a queued packet costs beyond its payload: the queue's own slot for it
/// (the packet header and its arrival instant), the shared header the
/// broadcast's clone gave its `Bytes`, and the allocator's overhead on the
/// payload. A 188-byte SDI packet is no bigger than all of that put
/// together, so counting the payload alone let an SDI feed's queue reach
/// well over one and a half times the budget it was said to be held to.
const ITEM_OVERHEAD_BYTES: usize = std::mem::size_of::<Item>() + 64;

enum Item {
    /// A packet, and when the intake took it off the broadcast.
    Packet(RtpPacket, Instant),
    /// Packets lost ahead of the next one: overrun on the broadcast, or past
    /// the intake's own budget.
    Lost(u64),
}

/// The output's side: reads like the broadcast receiver it replaces.
pub(super) struct Intake {
    rx: mpsc::UnboundedReceiver<Item>,
    budget: Arc<QueueBudget>,
    task: JoinHandle<()>,
    /// How long the packet [`Self::recv`] last returned had been queued.
    waited: Duration,
}

impl Intake {
    /// Start draining `src` into a queue of at most `budget` bytes, and of
    /// what `node` has left.
    pub(super) fn spawn(
        src: broadcast::Receiver<RtpPacket>,
        budget: usize,
        node: &'static NodeBudget,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let budget = Arc::new(QueueBudget::new(budget, node));
        let task = tokio::spawn(forward(src, tx, budget.clone()));
        Self { rx, budget, task, waited: Duration::ZERO }
    }

    /// The next packet, or what the broadcast receiver would have said: a
    /// `Lagged(n)` for packets lost ahead of it, `Closed` when the flow ends.
    pub(super) async fn recv(&mut self) -> Result<RtpPacket, broadcast::error::RecvError> {
        match self.rx.recv().await {
            Some(Item::Packet(p, queued_at)) => {
                self.budget.release(cost(&p));
                self.waited = queued_at.elapsed();
                Ok(p)
            }
            Some(Item::Lost(n)) => Err(broadcast::error::RecvError::Lagged(n)),
            None => Err(broadcast::error::RecvError::Closed),
        }
    }

    /// How long the packet [`Self::recv`] last returned sat in the queue: how
    /// far behind the feed this output is. Near zero while it keeps up.
    pub(super) fn waited(&self) -> Duration {
        self.waited
    }
}

/// What a packet is charged against the budget.
fn cost(p: &RtpPacket) -> usize {
    p.data.len() + ITEM_OVERHEAD_BYTES
}

impl Drop for Intake {
    fn drop(&mut self) {
        // An idle flow sends nothing, so the forwarder could sit in `recv`
        // long after its output has gone. What it had queued is given back to
        // the node when the forwarder's share of the budget goes with it.
        self.task.abort();
    }
}

async fn forward(
    mut src: broadcast::Receiver<RtpPacket>,
    tx: mpsc::UnboundedSender<Item>,
    budget: Arc<QueueBudget>,
) {
    let mut lost: u64 = 0;
    loop {
        match src.recv().await {
            Ok(p) => {
                if !budget.try_charge(cost(&p)) {
                    lost += 1;
                    continue;
                }
                // The loss is reported in order: ahead of the first packet
                // that made it after the gap, as `Lagged` would be.
                if lost > 0 && tx.send(Item::Lost(std::mem::take(&mut lost))).is_err() {
                    return;
                }
                if tx.send(Item::Packet(p, Instant::now())).is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => lost += n,
            Err(broadcast::error::RecvError::Closed) => {
                if lost > 0 {
                    let _ = tx.send(Item::Lost(lost));
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// A node budget that never binds, for the tests of an intake's own.
    static UNBOUNDED: NodeBudget = NodeBudget::new(usize::MAX);

    fn pkt(seq: u16, len: usize) -> RtpPacket {
        RtpPacket {
            data: Bytes::from(vec![0x47; len]),
            sequence_number: seq,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        }
    }

    /// The point of it: an output that is not reading loses nothing while the
    /// broadcast it sits on runs far past its own capacity.
    #[tokio::test]
    async fn a_reader_that_stalls_past_the_broadcast_capacity_loses_nothing() {
        let (tx, rx) = broadcast::channel::<RtpPacket>(16);
        let mut intake = Intake::spawn(rx, 1 << 20, &UNBOUNDED);
        for i in 0..1000u16 {
            tx.send(pkt(i, 188)).unwrap();
            // Let the forwarder keep pace with the publisher, as it would on a
            // runtime with spare workers; the reader below is the one asleep.
            if i % 8 == 0 {
                tokio::task::yield_now().await;
            }
        }
        for i in 0..1000u16 {
            let p = intake.recv().await.expect("nothing lost");
            assert_eq!(p.sequence_number, i);
        }
    }

    /// Past the budget it drops, and says how many, in order — the shape the
    /// output's existing `Lagged` handling counts and warns about.
    #[tokio::test]
    async fn past_its_budget_it_drops_and_reports_the_loss_in_order() {
        let (tx, rx) = broadcast::channel::<RtpPacket>(1024);
        let mut intake = Intake::spawn(rx, cost(&pkt(0, 188)) * 10, &UNBOUNDED);
        for i in 0..15u16 {
            tx.send(pkt(i, 188)).unwrap();
        }
        tokio::task::yield_now().await;
        // Ten fit; five were dropped while the reader was away.
        for i in 0..10u16 {
            assert_eq!(intake.recv().await.unwrap().sequence_number, i);
        }
        // Room again: the next packet arrives after a report of the five.
        tx.send(pkt(15, 188)).unwrap();
        match intake.recv().await {
            Err(broadcast::error::RecvError::Lagged(5)) => {}
            other => panic!("expected Lagged(5), got {:?}", other.map(|p| p.sequence_number)),
        }
        assert_eq!(intake.recv().await.unwrap().sequence_number, 15);
    }

    /// The budget counts what a packet really holds, not just its payload: a
    /// budget of ten payloads takes fewer than ten small packets.
    #[tokio::test]
    async fn the_budget_charges_each_packet_its_overhead_too() {
        let (tx, rx) = broadcast::channel::<RtpPacket>(1024);
        let mut intake = Intake::spawn(rx, 188 * 10, &UNBOUNDED);
        for i in 0..10u16 {
            tx.send(pkt(i, 188)).unwrap();
        }
        tokio::task::yield_now().await;
        let fit = (188 * 10) / cost(&pkt(0, 188));
        assert!(fit < 10);
        for i in 0..fit as u16 {
            assert_eq!(intake.recv().await.unwrap().sequence_number, i);
        }
        tx.send(pkt(10, 188)).unwrap();
        assert!(matches!(intake.recv().await, Err(broadcast::error::RecvError::Lagged(n)) if n == 10 - fit as u64));
    }

    /// The output is told how far behind the feed it is: a packet read after
    /// the reader slept has waited that long, and one read as it arrives has
    /// not — what lets a segment closed from a backlog be dated by arrival.
    #[tokio::test]
    async fn it_says_how_long_each_packet_waited() {
        let (tx, rx) = broadcast::channel::<RtpPacket>(16);
        let mut intake = Intake::spawn(rx, 1 << 20, &UNBOUNDED);
        tx.send(pkt(0, 188)).unwrap();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        intake.recv().await.unwrap();
        assert!(intake.waited() >= Duration::from_millis(300), "waited {:?}", intake.waited());
        tx.send(pkt(1, 188)).unwrap();
        intake.recv().await.unwrap();
        assert!(intake.waited() < Duration::from_millis(300), "waited {:?}", intake.waited());
    }

    /// Past the node's budget an intake drops, and reports the loss in order,
    /// exactly as past its own — while still inside its own. Room another
    /// intake gives back, by catching up or by stopping, is its to take.
    #[tokio::test]
    async fn past_the_nodes_budget_it_drops_and_reports_the_loss_in_order() {
        let per = cost(&pkt(0, 188));
        static NODE: NodeBudget = NodeBudget::new(15 * (188 + ITEM_OVERHEAD_BYTES));
        let (tx_a, rx_a) = broadcast::channel::<RtpPacket>(1024);
        let (tx_b, rx_b) = broadcast::channel::<RtpPacket>(1024);
        let mut a = Intake::spawn(rx_a, per * 10, &NODE);
        let mut b = Intake::spawn(rx_b, per * 10, &NODE);
        for i in 0..10u16 {
            tx_a.send(pkt(i, 188)).unwrap();
        }
        tokio::task::yield_now().await;
        // The node has room for five of B's ten.
        for i in 0..10u16 {
            tx_b.send(pkt(i, 188)).unwrap();
        }
        tokio::task::yield_now().await;
        for i in 0..5u16 {
            assert_eq!(b.recv().await.unwrap().sequence_number, i);
        }
        tx_b.send(pkt(10, 188)).unwrap();
        match b.recv().await {
            Err(broadcast::error::RecvError::Lagged(5)) => {}
            other => panic!("expected Lagged(5), got {:?}", other.map(|p| p.sequence_number)),
        }
        assert_eq!(b.recv().await.unwrap().sequence_number, 10);

        // A reads what it holds: the room is back on the node.
        for i in 0..10u16 {
            assert_eq!(a.recv().await.unwrap().sequence_number, i);
        }
        assert_eq!(NODE.queued(), 0);
        // And an intake stopped with a backlog gives it back too.
        for i in 0..10u16 {
            tx_a.send(pkt(i, 188)).unwrap();
        }
        tokio::task::yield_now().await;
        assert_eq!(NODE.queued(), 10 * per);
        drop(a);
        let deadline = Instant::now() + Duration::from_secs(5);
        while NODE.queued() != 0 {
            assert!(Instant::now() < deadline, "a stopped intake kept {} bytes of the node's budget", NODE.queued());
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn the_flow_ending_reads_as_closed() {
        let (tx, rx) = broadcast::channel::<RtpPacket>(16);
        let mut intake = Intake::spawn(rx, 1 << 20, &UNBOUNDED);
        tx.send(pkt(1, 188)).unwrap();
        drop(tx);
        assert_eq!(intake.recv().await.unwrap().sequence_number, 1);
        assert!(matches!(intake.recv().await, Err(broadcast::error::RecvError::Closed)));
    }
}
