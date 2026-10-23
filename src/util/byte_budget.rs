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

/// One queue's account: its own limit, and its share of its kind's
/// [`NodeBudget`].
///
/// The queue's producer charges each item as it queues it, and its consumer
/// releases it as it takes it off. Whatever is still charged when the last of
/// the two lets go of this — a consumer that stopped with items queued — is
/// given back to the node then, so a stopped output or recorder never leaves
/// its share of the node budget behind.
#[derive(Debug)]
pub struct QueueBudget {
    queued: AtomicUsize,
    limit: usize,
    node: &'static NodeBudget,
}

impl QueueBudget {
    pub fn new(limit: usize, node: &'static NodeBudget) -> Self {
        Self { queued: AtomicUsize::new(0), limit, node }
    }

    /// Charge an item of `cost` bytes against this queue's limit and the
    /// node's, or refuse it, charging neither.
    ///
    /// The queue's own limit needs no compare-and-swap: only its producer
    /// charges it, so the check and the charge cannot be raced past it — the
    /// consumer only ever brings the count down.
    pub fn try_charge(&self, cost: usize) -> bool {
        if self.queued.load(Ordering::Relaxed) + cost > self.limit {
            return false;
        }
        if !self.node.try_charge(cost) {
            return false;
        }
        self.queued.fetch_add(cost, Ordering::Relaxed);
        true
    }

    /// Give back an item of `cost` bytes as it leaves the queue.
    pub fn release(&self, cost: usize) {
        self.queued.fetch_sub(cost, Ordering::Relaxed);
        self.node.release(cost);
    }
}

impl Drop for QueueBudget {
    fn drop(&mut self) {
        self.node.release(*self.queued.get_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queues of one kind cannot hold more between them than the node's
    /// budget, whatever each may hold on its own, and the room one gives back
    /// is the others' to take.
    #[test]
    fn queues_share_the_node_budget() {
        static NODE: NodeBudget = NodeBudget::new(150);
        let a = QueueBudget::new(100, &NODE);
        let b = QueueBudget::new(100, &NODE);
        assert!((0..10).all(|_| a.try_charge(10)));
        assert!(!a.try_charge(10), "past its own limit");
        assert!((0..5).all(|_| b.try_charge(10)));
        assert!(!b.try_charge(10), "past the node's, inside its own");
        a.release(10);
        assert!(b.try_charge(10), "the room a gave back was not b's to take");
        assert_eq!(NODE.queued(), 150);
    }

    /// A queue dropped with items still charged — a consumer that stopped
    /// with a backlog — gives them back to the node.
    #[test]
    fn a_dropped_queue_gives_its_charge_back_to_the_node() {
        static NODE: NodeBudget = NodeBudget::new(1_000);
        let a = QueueBudget::new(1_000, &NODE);
        assert!(a.try_charge(300) && a.try_charge(200));
        a.release(200);
        assert_eq!(NODE.queued(), 300);
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
                    let q = QueueBudget::new(usize::MAX, &NODE);
                    let taken = (0..10_000).filter(|_| q.try_charge(7)).count();
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
}
