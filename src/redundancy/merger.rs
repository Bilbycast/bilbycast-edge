// Copyright (c) 2026 Reza Rahimi, Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE 2022-7 Hitless Merge — de-duplicates and gap-fills RTP packets
//! arriving from two redundant network legs.
//!
//! The merger tracks a sliding window of recently-emitted RTP sequence
//! numbers via a bitmap. For each incoming packet from either leg:
//!
//! 1. If the seq is **ahead** of the highest seen so far, the gap between
//!    the previous highest and the new seq is marked as "not yet emitted"
//!    in the bitmap, the packet is emitted, and the new seq is marked as
//!    emitted.
//! 2. If the seq is **within the gap-fill window** behind the highest, the
//!    bitmap tells us whether it has already been emitted. If not, it
//!    fills a gap left by the other leg dropping it — emit it. Otherwise
//!    it's a duplicate — drop it.
//! 3. If the seq is **further than the window behind** the highest, it
//!    has either rolled out of the window or is a stream reset — drop it.
//!
//! ## Why a gap-fill window
//!
//! The previous strict-greater-than dedup discarded any seq ≤
//! `last_consumed_seq`, which is wrong for the canonical 2022-7 use case
//! where leg A drops a packet and leg B carries it. Concretely, with
//! out-of-order processing:
//!
//! ```text
//!   leg1: 1000 ____ 1002    (1001 lost in kernel buffer)
//!   leg2: 1000 1001 1002
//!   tokio::select! arbitrary order may dispatch:
//!     leg1 1000 → emit, last=1000
//!     leg2 1000 → drop (dup)
//!     leg1 1002 → emit, last=1002
//!     leg2 1001 → DROPPED as backwards ← bug
//!     leg2 1002 → drop (dup)
//! ```
//!
//! Test 2.5 in the 2026-04-09 testbed was getting 1 H.264 macroblock
//! error per ~3 runs from exactly this race. With the gap-fill window the
//! `leg2 1001` arrival fills the bit at position 1001 and gets emitted in
//! its natural slot, restoring true hitless behaviour.
//!
//! ## Window size
//!
//! 1024 slots ≈ 0.4 s of video at the typical 2400 packets/sec used by
//! 1080p30 H.264 over MPEG-TS — large enough to absorb realistic
//! cross-leg jitter (the dual UDP sockets feeding `tokio::select!` can
//! reorder by ~100 ms when one leg is briefly bursty) without growing the
//! `[u64; N]` bitmap beyond the L1 cache. For 50+ Mbps streams the
//! window is still well above the inter-leg jitter floor.

use std::collections::VecDeque;

/// Identifies which network leg a packet arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveLeg {
    Leg1,
    Leg2,
    None,
}

/// Number of past sequence numbers to track in the gap-fill bitmap.
/// Must be a multiple of 64.
const REORDER_WINDOW: u16 = 1024;

/// Hitless merger state. Tracks emitted RTP sequence numbers in a sliding
/// bitmap window so the merger can dedup duplicates *and* fill gaps left
/// by single-leg loss.
pub struct HitlessMerger {
    /// Highest RTP sequence number we have ever observed across both legs
    /// (wrapping u16). Anchor for the sliding bitmap window.
    highest_seq: u16,
    /// Bitmap of recently-emitted seq numbers, indexed by `seq mod
    /// REORDER_WINDOW`. A `1` bit means "this seq has been emitted at
    /// least once and any further arrival is a duplicate". A `0` bit
    /// in the gap behind `highest_seq` means "this seq has not been
    /// emitted yet — if it arrives, fill the gap".
    ///
    /// `VecDeque<u64>` is sized to hold `REORDER_WINDOW / 64` words. We
    /// rotate it lazily via `clear_gap_words` rather than shifting on
    /// every advance.
    emitted: VecDeque<u64>,
    /// Which leg last provided the forwarded packet.
    active_leg: ActiveLeg,
    /// Whether we've forwarded at least one packet.
    initialized: bool,
}

impl HitlessMerger {
    /// Create a new merger in the un-initialized state.
    pub fn new() -> Self {
        let words = (REORDER_WINDOW as usize) / 64;
        let mut emitted = VecDeque::with_capacity(words);
        for _ in 0..words {
            emitted.push_back(0u64);
        }
        Self {
            highest_seq: 0,
            emitted,
            active_leg: ActiveLeg::None,
            initialized: false,
        }
    }

    /// Test the bitmap for a given seq.
    #[inline]
    fn is_emitted(&self, seq: u16) -> bool {
        let idx = (seq % REORDER_WINDOW) as usize;
        let word = idx / 64;
        let bit = idx % 64;
        (self.emitted[word] >> bit) & 1 == 1
    }

    /// Set the bitmap bit for a given seq.
    #[inline]
    fn mark_emitted(&mut self, seq: u16) {
        let idx = (seq % REORDER_WINDOW) as usize;
        let word = idx / 64;
        let bit = idx % 64;
        self.emitted[word] |= 1u64 << bit;
    }

    /// Clear `count` bitmap slots starting at the wrapping seq `start`.
    /// Used when `highest_seq` advances: the newly-uncovered slots in the
    /// sliding window need to be reset to "not yet emitted" before the
    /// next packet from the other leg can fill them. `count == 0` is a
    /// no-op (the common consecutive-advance case).
    fn clear_slots(&mut self, start: u16, count: u16) {
        let mut s = start;
        for _ in 0..count {
            let idx = (s % REORDER_WINDOW) as usize;
            let word = idx / 64;
            let bit = idx % 64;
            self.emitted[word] &= !(1u64 << bit);
            s = s.wrapping_add(1);
        }
    }

    /// Attempt to merge an incoming packet from the given leg.
    ///
    /// Returns `Some(leg)` if this packet should be forwarded downstream,
    /// or `None` if it's a duplicate / too-old / stream-reset packet that
    /// should be silently dropped.
    pub fn try_merge(&mut self, seq: u16, leg: ActiveLeg) -> Option<ActiveLeg> {
        if !self.initialized {
            // Anchor on the very first packet. Mark only this seq as
            // emitted; the rest of the bitmap stays zero so that the
            // following ~`REORDER_WINDOW` seqs can be filled in any order
            // by either leg.
            self.highest_seq = seq;
            self.mark_emitted(seq);
            self.active_leg = leg;
            self.initialized = true;
            return Some(leg);
        }

        // Wrapping u16 distance from highest_seq.
        // diff in (0, 0x8000)  → forward jump
        // diff == 0            → same as highest_seq (likely duplicate)
        // diff in [0x8000, …)  → backward (older than highest_seq)
        let forward = seq.wrapping_sub(self.highest_seq);
        let backward = self.highest_seq.wrapping_sub(seq);

        if forward > 0 && forward < 0x8000 {
            // ── Forward jump ──
            //
            // If the gap is larger than the reorder window, treat it as a
            // stream reset (e.g. publisher restarted with a new ISN). Reset
            // the bitmap and re-anchor.
            if forward as u16 >= REORDER_WINDOW {
                for w in self.emitted.iter_mut() {
                    *w = 0;
                }
                self.highest_seq = seq;
                self.mark_emitted(seq);
                self.active_leg = leg;
                return Some(leg);
            }

            // Normal forward jump: clear the (forward - 1) just-uncovered
            // slots so the other leg can still fill them, then mark this
            // seq emitted. `forward == 1` (consecutive advance) is a no-op.
            let prev_highest = self.highest_seq;
            self.clear_slots(prev_highest.wrapping_add(1), forward - 1);
            self.highest_seq = seq;
            self.mark_emitted(seq);
            self.active_leg = leg;
            Some(leg)
        } else if backward < REORDER_WINDOW {
            // ── Within the gap-fill window ──
            //
            // Either a duplicate (bit already set — drop) or a missing
            // packet that the other leg has just delivered (bit clear —
            // fill the gap and emit).
            if self.is_emitted(seq) {
                None
            } else {
                self.mark_emitted(seq);
                self.active_leg = leg;
                Some(leg)
            }
        } else {
            // ── Too far behind: rolled out of the window or backward
            //    stream-reset ──
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_packet_accepted() {
        let mut merger = HitlessMerger::new();
        let result = merger.try_merge(0, ActiveLeg::Leg1);
        assert_eq!(result, Some(ActiveLeg::Leg1));
    }

    #[test]
    fn test_ascending_sequence() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(103, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
    }

    #[test]
    fn test_duplicate_rejected() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // Same sequence from other leg should be rejected
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg2), None);
        // Same sequence from same leg also rejected
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1), None);
    }

    #[test]
    fn test_out_of_order_within_window_accepted() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // Packet 101 arrives late from leg2 — within the gap-fill window
        // (the slot at 101 was left clear when leg1 advanced 100 → 102),
        // so it must be emitted to provide hitless coverage. This is the
        // exact behaviour Bug #2 residual was missing.
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
        // A second 101 (e.g. retransmit) is now a duplicate.
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg1), None);
    }

    #[test]
    fn test_gap_accepted() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // Gap: 100 -> 105 (diff=5, valid forward gap)
        assert_eq!(merger.try_merge(105, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
    }

    #[test]
    fn test_sequence_wraparound() {
        let mut merger = HitlessMerger::new();
        // Near end of u16 space
        assert_eq!(merger.try_merge(65534, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(65535, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // Wraparound to 0
        assert_eq!(merger.try_merge(0, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(1, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
    }

    #[test]
    fn test_far_backward_jump_rejected() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(50000, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // A packet far enough behind to fall outside the gap-fill window
        // (REORDER_WINDOW = 1024). 50000 - 2000 = 48000, which is more
        // than 1024 behind — must be dropped.
        assert_eq!(merger.try_merge(48000, ActiveLeg::Leg2), None);
    }

    /// Bug #2 residual (2026-04-09): a single-leg packet loss followed by
    /// the other leg delivering the missing packet must be filled in,
    /// not silently dropped as "backwards". This is the entire point of
    /// SMPTE 2022-7 hitless redundancy.
    #[test]
    fn test_gap_fill_from_other_leg() {
        let mut merger = HitlessMerger::new();
        // leg1 emits 1000, then loses 1001 (kernel buffer overflow), then
        // emits 1002. leg2 delivers all three.
        assert_eq!(merger.try_merge(1000, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(1000, ActiveLeg::Leg2), None); // dup of leg1
        assert_eq!(merger.try_merge(1002, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // Critical: leg2 1001 arrives AFTER we've already advanced past
        // 1001 via leg1. The previous merger dropped it; the gap-fill
        // window must accept it.
        assert_eq!(merger.try_merge(1001, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(1002, ActiveLeg::Leg2), None); // dup of leg1
    }

    /// A duplicate of a previously gap-filled packet is still a duplicate.
    #[test]
    fn test_gap_fill_then_dup_dropped() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(1000, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(1002, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(1001, ActiveLeg::Leg2), Some(ActiveLeg::Leg2)); // gap fill
        // A second arrival of 1001 (e.g. retransmit on leg1) must be a dup.
        assert_eq!(merger.try_merge(1001, ActiveLeg::Leg1), None);
    }

    /// Gap fills must work across the u16 wraparound boundary.
    #[test]
    fn test_gap_fill_across_wraparound() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(65534, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // Skip 65535, jump to 0 (wraparound).
        assert_eq!(merger.try_merge(0, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // leg2 fills 65535 — must be accepted.
        assert_eq!(merger.try_merge(65535, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
        // Duplicate 65535 from leg1 must be dropped.
        assert_eq!(merger.try_merge(65535, ActiveLeg::Leg1), None);
        // Forward progress still works after.
        assert_eq!(merger.try_merge(1, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
    }

    /// Gaps that fall behind the reorder window are unrecoverable. The
    /// merger drops them rather than incorrectly emitting them out of
    /// order, since the receiver has already moved on.
    #[test]
    fn test_gap_outside_window_dropped() {
        let mut merger = HitlessMerger::new();
        merger.try_merge(0, ActiveLeg::Leg1);
        // Advance ~1500 packets ahead (well beyond REORDER_WINDOW = 1024).
        for s in 1..=1500u16 {
            merger.try_merge(s, ActiveLeg::Leg1);
        }
        // A late arrival of seq 100 is now ~1400 behind highest. Must be dropped.
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg2), None);
    }

    /// A forward jump larger than the reorder window resets the merger
    /// (treated as a stream reset / publisher restart with a new ISN).
    #[test]
    fn test_huge_forward_jump_resets() {
        let mut merger = HitlessMerger::new();
        merger.try_merge(100, ActiveLeg::Leg1);
        // Forward jump of 5000 (>> REORDER_WINDOW). This is a stream
        // reset, not a real RTP forward jump.
        assert_eq!(merger.try_merge(5100, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // After reset, the bitmap is fresh — duplicate of the new anchor
        // is still a dup, but a different forward seq works.
        assert_eq!(merger.try_merge(5100, ActiveLeg::Leg2), None);
        assert_eq!(merger.try_merge(5101, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
    }

    #[test]
    fn test_interleaved_legs_dedup() {
        let mut merger = HitlessMerger::new();
        // Simulate two legs sending the same packets with slight timing differences
        // Leg1: 100, 101, 102
        // Leg2: 100, 101, 102 (arrives slightly later)
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg2), None); // dup
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg1), None); // dup
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg2), None); // dup
    }

    #[test]
    fn test_leg1_fails_leg2_continues() {
        let mut merger = HitlessMerger::new();
        // Start on leg1
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg1), Some(ActiveLeg::Leg1));
        // Leg1 stops, leg2 continues
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(103, ActiveLeg::Leg2), Some(ActiveLeg::Leg2));
    }
}
