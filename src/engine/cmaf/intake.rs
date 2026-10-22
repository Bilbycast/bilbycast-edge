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
//! It cannot help if the whole runtime stalls: the intake is a task like any
//! other and would stall with it. It turns a slow peer into latency instead of
//! loss, which is the case the output itself can do something about.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::engine::packet::RtpPacket;

/// Bytes an output may hold unread before the intake drops. ~24 s of a
/// 10 Mbit/s feed, held only while the output is behind.
pub(super) const INTAKE_BUDGET_BYTES: usize = 32 * 1024 * 1024;

enum Item {
    Packet(RtpPacket),
    /// Packets lost ahead of the next one: overrun on the broadcast, or past
    /// the intake's own budget.
    Lost(u64),
}

/// The output's side: reads like the broadcast receiver it replaces.
pub(super) struct Intake {
    rx: mpsc::UnboundedReceiver<Item>,
    queued: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Intake {
    /// Start draining `src` into a queue of at most `budget` bytes.
    pub(super) fn spawn(src: broadcast::Receiver<RtpPacket>, budget: usize) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(forward(src, tx, queued.clone(), budget));
        Self { rx, queued, task }
    }

    /// The next packet, or what the broadcast receiver would have said: a
    /// `Lagged(n)` for packets lost ahead of it, `Closed` when the flow ends.
    pub(super) async fn recv(&mut self) -> Result<RtpPacket, broadcast::error::RecvError> {
        match self.rx.recv().await {
            Some(Item::Packet(p)) => {
                self.queued.fetch_sub(p.data.len(), Ordering::Relaxed);
                Ok(p)
            }
            Some(Item::Lost(n)) => Err(broadcast::error::RecvError::Lagged(n)),
            None => Err(broadcast::error::RecvError::Closed),
        }
    }
}

impl Drop for Intake {
    fn drop(&mut self) {
        // An idle flow sends nothing, so the forwarder could sit in `recv`
        // long after its output has gone.
        self.task.abort();
    }
}

async fn forward(
    mut src: broadcast::Receiver<RtpPacket>,
    tx: mpsc::UnboundedSender<Item>,
    queued: Arc<AtomicUsize>,
    budget: usize,
) {
    let mut lost: u64 = 0;
    loop {
        match src.recv().await {
            Ok(p) => {
                let len = p.data.len();
                if queued.load(Ordering::Relaxed) + len > budget {
                    lost += 1;
                    continue;
                }
                // The loss is reported in order: ahead of the first packet
                // that made it after the gap, as `Lagged` would be.
                if lost > 0 && tx.send(Item::Lost(std::mem::take(&mut lost))).is_err() {
                    return;
                }
                queued.fetch_add(len, Ordering::Relaxed);
                if tx.send(Item::Packet(p)).is_err() {
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
        let mut intake = Intake::spawn(rx, 1 << 20);
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
        let mut intake = Intake::spawn(rx, 188 * 10);
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

    #[tokio::test]
    async fn the_flow_ending_reads_as_closed() {
        let (tx, rx) = broadcast::channel::<RtpPacket>(16);
        let mut intake = Intake::spawn(rx, 1 << 20);
        tx.send(pkt(1, 188)).unwrap();
        drop(tx);
        assert_eq!(intake.recv().await.unwrap().sequence_number, 1);
        assert!(matches!(intake.recv().await, Err(broadcast::error::RecvError::Closed)));
    }
}
