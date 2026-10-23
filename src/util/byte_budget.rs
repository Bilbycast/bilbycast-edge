// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Byte budgets for the queues that let a consumer fall behind its feed.
//!
//! Two kinds of queue sit between a flow's broadcast and a consumer that can
//! stall: each CMAF output's intake, behind its origin, and each recorder's
//! writer queue, behind its disk. A queue turns a stall into latency rather
//! than lost packets, and each is bounded on its own — 32 MiB apiece. On its
//! own that bounds nothing on the node: the number of queues grows with the
//! outputs and flows configured, and the stalls that fill them are correlated
//! — every recorder writes under one replay root, and a flow's renditions push
//! to one origin — so they fill together. Five DVR sessions with two
//! renditions each could hold 320 MiB in intakes, and five recorders 160 MiB,
//! at the moment the node is already in trouble.
//!
//! So each queue charges two budgets: its own ([`QueueBudget`]) and the one
//! every queue of its kind on the node shares ([`NodeBudget`]). Past either it
//! drops, exactly as it does past its own. The kinds do not share a budget:
//! an origin and a disk fail independently, and a disk stall filling a shared
//! one would make every CMAF output drop on an origin hiccup it would
//! otherwise have ridden out.
//!
//! The first bytes of every queue are its own, though — its floor, charged to
//! it and not to the node. Stalls are correlated, not universal: the outputs
//! on one node can push to different origins, and a node budget that every
//! byte had to pass through let eight outputs stalled on one origin starve a
//! ninth pushing to a healthy one, which then dropped every packet it was
//! handed for as long as the others stayed stuck. A queue that holds no more
//! than its floor never asks the node for anything, and only what a queue
//! holds past its floor competes for the node's budget. The node's queues can
//! therefore hold the node budget plus one floor each.
//!
//! That isolates a queue that keeps up only while what it holds as it keeps
//! up fits its floor, and that is a matter of its feed's rate: a CMAF output
//! that keeps up still waits on its origin at every segment close, holding
//! the feed's rate times the wait. So a floor can move
//! ([`QueueBudget::set_floor`]) — the CMAF intake sizes its own from the rate
//! it measures — and the charge each item carries keeps its release exact
//! however the floor has moved since.
//!
//! What is charged is a bound, not what is resident: two outputs of one flow
//! queue clones of the same payload, which is held once.

use std::sync::atomic::{AtomicUsize, Ordering};

/// What every queue of one kind on the node may hold between them.
#[derive(Debug)]
pub struct NodeBudget {
    queued: AtomicUsize,
    limit: usize,
}

impl NodeBudget {
    pub const fn new(limit: usize) -> Self {
        Self { queued: AtomicUsize::new(0), limit }
    }

    /// Charge `n` bytes if they fit, and nothing if they do not.
    ///
    /// A compare-and-swap rather than a check and an add, because unlike a
    /// queue's own budget this one has a producer per queue charging it at
    /// once, and two that each saw room could take it past the limit.
    fn try_charge(&self, n: usize) -> bool {
        let mut queued = self.queued.load(Ordering::Relaxed);
        loop {
            let Some(total) = queued.checked_add(n).filter(|&total| total <= self.limit) else {
                return false;
            };
            match self.queued.compare_exchange_weak(queued, total, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(now) => queued = now,
            }
        }
    }

    fn release(&self, n: usize) {
        self.queued.fetch_sub(n, Ordering::Relaxed);
    }

    /// What the queues of this kind hold between them now.
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

/// One queue's account: its own limit, the floor of it that is its alone,
/// and its share of its kind's [`NodeBudget`] past that.
///
/// The queue's producer charges each item as it queues it, and its consumer
/// releases it as it takes it off. Whatever is still charged when the last of
/// the two lets go of this — a consumer that stopped with items queued — is
/// given back to the node then, so a stopped output or recorder never leaves
/// its share of the node budget behind.
#[derive(Debug)]
pub struct QueueBudget {
    queued: AtomicUsize,
    /// The part of `queued` charged to the node as well.
    on_node: AtomicUsize,
    limit: usize,
    /// Read on every charge and moved by [`Self::set_floor`].
    floor: AtomicUsize,
    node: &'static NodeBudget,
}

/// What one item was charged, handed back to [`QueueBudget::release`] as it
/// leaves the queue.
///
/// It carries its node share with it rather than having the release work the
/// share out again, because the producer and the consumer move the queue's
/// count at once, and a share each worked out from its own view of it would
/// drift: the node budget leaks, or is given back twice.
#[derive(Debug, Clone, Copy)]
pub struct Charge {
    cost: usize,
    on_node: usize,
}

impl QueueBudget {
    pub fn new(limit: usize, floor: usize, node: &'static NodeBudget) -> Self {
        Self {
            queued: AtomicUsize::new(0),
            on_node: AtomicUsize::new(0),
            limit,
            floor: AtomicUsize::new(floor),
            node,
        }
    }

    /// Move the floor. Only what is charged from now on reads the new one:
    /// each item queued already carries the node share it took, and gives back
    /// exactly that, so a floor moved under a queue that holds items leaves the
    /// node's count as exact as one that stayed put.
    pub fn set_floor(&self, floor: usize) {
        self.floor.store(floor, Ordering::Relaxed);
    }

    /// Charge an item of `cost` bytes against this queue's limit and — for
    /// what of it lands past the queue's floor — the node's, or refuse it,
    /// charging neither.
    ///
    /// The queue's own limit needs no compare-and-swap: only its producer
    /// charges it, so the check and the charge cannot be raced past it — the
    /// consumer only ever brings the count down. A count the consumer brings
    /// down meanwhile makes this charge the node for more than it had to,
    /// never less.
    pub fn try_charge(&self, cost: usize) -> Option<Charge> {
        let queued = self.queued.load(Ordering::Relaxed);
        let total = queued.checked_add(cost).filter(|&total| total <= self.limit)?;
        let on_node = total.saturating_sub(self.floor.load(Ordering::Relaxed)).min(cost);
        if on_node > 0 && !self.node.try_charge(on_node) {
            return None;
        }
        self.queued.fetch_add(cost, Ordering::Relaxed);
        self.on_node.fetch_add(on_node, Ordering::Relaxed);
        Some(Charge { cost, on_node })
    }

    /// Give back an item as it leaves the queue.
    pub fn release(&self, charge: Charge) {
        self.queued.fetch_sub(charge.cost, Ordering::Relaxed);
        self.on_node.fetch_sub(charge.on_node, Ordering::Relaxed);
        self.node.release(charge.on_node);
    }
}

impl Drop for QueueBudget {
    fn drop(&mut self) {
        self.node.release(*self.on_node.get_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queues of one kind cannot hold more between them than the node's
    /// budget past their floors, whatever each may hold on its own, and the
    /// room one gives back is the others' to take.
    #[test]
    fn queues_share_the_node_budget() {
        static NODE: NodeBudget = NodeBudget::new(150);
        let a = QueueBudget::new(100, 0, &NODE);
        let b = QueueBudget::new(100, 0, &NODE);
        let taken: Vec<_> = (0..10).map_while(|_| a.try_charge(10)).collect();
        assert_eq!(taken.len(), 10);
        assert!(a.try_charge(10).is_none(), "past its own limit");
        assert!((0..5).all(|_| b.try_charge(10).is_some()));
        assert!(b.try_charge(10).is_none(), "past the node's, inside its own");
        a.release(taken[0]);
        assert!(b.try_charge(10).is_some(), "the room a gave back was not b's to take");
        assert_eq!(NODE.queued(), 150);
    }

    /// A queue that keeps up never needs the node: with every byte of the
    /// node's budget held by queues that have stalled, one holding less than
    /// its floor still takes everything it is handed. Past its floor it
    /// competes for the node like any other. Without the floor, eight CMAF
    /// outputs stalled on one origin made a ninth, pushing to a healthy one,
    /// drop every packet.
    #[test]
    fn a_queue_within_its_floor_needs_nothing_from_the_node() {
        static NODE: NodeBudget = NodeBudget::new(100);
        let stalled = QueueBudget::new(1_000, 0, &NODE);
        while stalled.try_charge(10).is_some() {}
        assert_eq!(NODE.queued(), 100);

        let healthy = QueueBudget::new(1_000, 50, &NODE);
        for _ in 0..1_000 {
            let charge = healthy.try_charge(10).expect("a queue that keeps up dropped");
            healthy.release(charge);
        }
        let held: Vec<_> = (0..5).map_while(|_| healthy.try_charge(10)).collect();
        assert_eq!(held.len(), 5, "the floor was not the queue's own");
        assert!(healthy.try_charge(10).is_none(), "past its floor it took what the node did not have");
        assert_eq!(NODE.queued(), 100, "a charge inside the floor reached the node");
    }

    /// An item charged partly past the floor gives back exactly the node
    /// share it took, whatever the queue holds when it leaves — so a queue
    /// that drains in order, its early items inside the floor and its later
    /// ones past it, leaves the node as it found it.
    #[test]
    fn each_item_gives_back_the_node_share_it_took() {
        static NODE: NodeBudget = NodeBudget::new(1_000);
        let q = QueueBudget::new(1_000, 25, &NODE);
        let first = q.try_charge(20).unwrap();
        let second = q.try_charge(20).unwrap();
        assert_eq!(NODE.queued(), 15, "only what lands past the floor is the node's");
        q.release(first);
        // Back inside the floor, with the second still charged to the node.
        let third = q.try_charge(5).unwrap();
        assert_eq!(NODE.queued(), 15);
        q.release(second);
        q.release(third);
        assert_eq!(NODE.queued(), 0);
    }

    /// A floor moved under a queue that holds items — raised past what it
    /// holds, lowered under it, and again — leaves the node holding exactly
    /// what the items still queued took from it, and nothing once they have
    /// all left.
    #[test]
    fn a_floor_moved_under_a_full_queue_leaves_the_node_exact() {
        static NODE: NodeBudget = NodeBudget::new(1_000);
        let q = QueueBudget::new(1_000, 20, &NODE);
        let first = q.try_charge(30).unwrap();
        assert_eq!(NODE.queued(), 10);
        q.set_floor(100);
        let second = q.try_charge(30).unwrap();
        assert_eq!(NODE.queued(), 10, "a charge under the raised floor reached the node");
        q.set_floor(0);
        let third = q.try_charge(30).unwrap();
        assert_eq!(NODE.queued(), 40, "a charge over the lowered floor missed the node");
        q.release(first);
        q.set_floor(50);
        q.release(third);
        q.release(second);
        assert_eq!(NODE.queued(), 0, "the node kept bytes no item holds");
    }

    /// A queue dropped with items still charged — a consumer that stopped
    /// with a backlog — gives its node share back to the node.
    #[test]
    fn a_dropped_queue_gives_its_charge_back_to_the_node() {
        static NODE: NodeBudget = NodeBudget::new(1_000);
        let a = QueueBudget::new(1_000, 100, &NODE);
        let _ = a.try_charge(300).unwrap();
        let second = a.try_charge(200).unwrap();
        a.release(second);
        assert_eq!(NODE.queued(), 200);
        drop(a);
        assert_eq!(NODE.queued(), 0, "a stopped queue kept its share of the node budget");
    }

    /// Producers on different queues charging the node at once never take
    /// it past its limit.
    #[test]
    fn concurrent_producers_never_overrun_the_node() {
        static NODE: NodeBudget = NodeBudget::new(10_000);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    let q = QueueBudget::new(usize::MAX, 0, &NODE);
                    let taken = (0..10_000).filter(|_| q.try_charge(7).is_some()).count();
                    // Held until every thread has charged, so none of the
                    // room comes back early.
                    (q, taken)
                })
            })
            .collect();
        let held: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let taken: usize = held.iter().map(|(_, n)| n).sum();
        assert_eq!(taken, 10_000 / 7, "the node was overrun, or room went unused");
        assert!(NODE.queued() <= 10_000);
        drop(held);
        assert_eq!(NODE.queued(), 0);
    }

    /// A producer and a consumer moving one queue's count at once, across
    /// its floor and back, leave nothing charged to the node once the queue
    /// is empty — the share each item took is the share it gives back.
    #[test]
    fn a_racing_producer_and_consumer_leave_the_node_as_they_found_it() {
        static NODE: NodeBudget = NodeBudget::new(usize::MAX);
        let q = std::sync::Arc::new(QueueBudget::new(usize::MAX, 1_000, &NODE));
        let (tx, rx) = std::sync::mpsc::sync_channel::<Charge>(256);
        let consumer = {
            let q = q.clone();
            std::thread::spawn(move || rx.into_iter().for_each(|charge| q.release(charge)))
        };
        let producer = {
            let q = q.clone();
            std::thread::spawn(move || {
                for n in 0..200_000usize {
                    tx.send(q.try_charge(1 + n % 97).unwrap()).unwrap();
                }
            })
        };
        producer.join().unwrap();
        consumer.join().unwrap();
        assert_eq!(NODE.queued(), 0, "the node kept bytes no item holds");
    }
}
