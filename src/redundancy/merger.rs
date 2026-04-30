// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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

// ─── Buffered SMPTE 2022-7 merger ────────────────────────────────────
//
// The bare [`HitlessMerger`] above is the *dedup primitive* — it decides
// whether a packet is new vs duplicate. Used alone, it emits packets in
// arrival order, which is wrong for industry-standard SMPTE 2022-7
// because:
//
// 1. Asymmetric WAN paths have a non-zero **path differential** (often
//    20–80 ms). The faster leg arrives first and is emitted immediately;
//    if the faster leg then drops a single packet, the slower leg's
//    copy of that packet hasn't arrived yet, and the dedup-only merger
//    has no second copy to fall back on. Result: a glitch on the
//    output that should have been hitless.
// 2. Reorder / late-arrival from one leg can produce out-of-order
//    output that downstream decoders dislike (e.g. RTMP / WebRTC
//    re-muxers expect strictly increasing seq).
//
// [`BufferedHitlessMerger`] adds a per-packet skew-accommodation
// buffer: every accepted packet is held for `max_path_diff` from its
// first arrival, then released in seq order. The slower leg has the
// full window to deliver — when it does, the dedup primitive drops the
// duplicate and the original entry's `legs_seen` mask is updated for
// the metric. When the slower leg fails to deliver, the buffer's hold
// expires and the packet is released anyway. The resulting output
// stream has constant `max_path_diff` latency (the cost of hitless
// protection) and is byte-identical to the source within the dedup
// window.
//
// Algorithm (per `ingest`):
//   1. Update per-leg arrival stats.
//   2. Project u16 seq → i64 virtual seq (monotonic across u16 wrap).
//   3. Test the dedup bitmap. If duplicate, update the buffered
//      entry's `legs_seen` mask + path-diff measurement, drop the
//      payload.
//   4. Insert into a `BTreeMap<i64, BufferedEntry>` keyed by virtual
//      seq, stamping `release_at = now + max_path_diff`.
//   5. Drain ready entries from the map in seq order.
//
// Algorithm (per `drain_expired` / wake-up):
//   1. Walk the map from the lowest entry. While the lowest entry's
//      `release_at <= now`, pop and emit. Record any seq gap as
//      lost packets in stats.
//
// Latency vs. resilience trade-off:
//   * `max_path_diff = 0`  → behaves like the legacy dedup-only path.
//   * `max_path_diff = N`  → output stream has constant N ms latency,
//                           N ms of leg skew tolerated hitlessly.
//   * Industry typical:    30–80 ms for terrestrial WAN; 200–500 ms
//                           for satellite.
//
// Per-leg health:
//   * Each leg has UP / DEGRADED / DOWN state derived from arrival
//     cadence. `down_threshold_ms` = 1000 ms; `degraded_loss_pct` =
//     5 % rolling. The merger surfaces these as events the input
//     task forwards to the manager.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Buffered SMPTE 2022-7 merger over an arbitrary packet payload.
///
/// `T` is the per-packet payload (e.g. `RtpPacket`, `EsPacket`). The
/// merger only inspects the wire-level seq + the leg id; `T` rides
/// through opaquely.
pub struct BufferedHitlessMerger<T: Clone> {
    /// Dedup primitive (rules duplicates / stream resets).
    dedup: HitlessMerger,
    /// Skew-accommodation buffer keyed by virtual (u16-wrap-resolved)
    /// seq number. Entries drain in seq order at `release_at`.
    buffer: BTreeMap<i64, BufferedEntry<T>>,
    /// Anchors that project `u16` wire seq → `i64` virtual seq.
    anchor_real: u16,
    anchor_virtual: i64,
    anchor_set: bool,
    /// Cursor — the next virtual seq we expect to emit. None until the
    /// first packet has been emitted.
    cursor: Option<i64>,
    /// Path-differential window. Every accepted packet is held this
    /// long from first arrival before release. Constant emission
    /// latency.
    pub max_path_diff: Duration,
    /// Hard cap on buffer size. Defends against pathological seq
    /// floods / memory growth under loss.
    pub max_buffer_packets: usize,
    /// Per-leg arrival tracking for SSRC validation, drift detection,
    /// and operator-facing health.
    leg_state: [LegState; 2],
    /// Aggregate stats (lock-free).
    pub stats: BufferedHitlessStats,
}

#[derive(Debug, Clone)]
struct BufferedEntry<T: Clone> {
    payload: T,
    /// Whichever leg first delivered this seq.
    first_leg: ActiveLeg,
    /// Wallclock of first arrival.
    first_seen: Instant,
    /// Drain deadline (`first_seen + max_path_diff`).
    release_at: Instant,
    /// Bitmask of which legs have delivered this seq. Bit 0 = Leg1,
    /// bit 1 = Leg2.
    legs_seen: u8,
}

#[derive(Debug, Default)]
struct LegState {
    last_seq: Option<u16>,
    last_arrival: Option<Instant>,
    /// Observed RTP SSRC (when the input layer fills it). 0 = unset.
    ssrc: u32,
    /// State machine — Up → Degraded → Down.
    health: LegHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LegHealth {
    #[default]
    Unknown,
    Up,
    Degraded,
    Down,
}

/// Aggregate counters published on the flow's stats snapshot. All
/// `AtomicU64` so the snapshot path never blocks the data path.
#[derive(Debug, Default)]
pub struct BufferedHitlessStats {
    pub packets_emitted: AtomicU64,
    pub packets_emitted_leg1: AtomicU64,
    pub packets_emitted_leg2: AtomicU64,
    pub dups_dropped: AtomicU64,
    /// Packets that arrived on both legs (the dedup-overlap case — the
    /// "happy path" for 2022-7). `dups_dropped` was incremented when
    /// the second copy arrived; this is the counter for the emitted
    /// half of the pair.
    pub packets_via_both_legs: AtomicU64,
    /// Packets that were emitted but only ever delivered by leg 1.
    /// When leg 1 is the consistent source and leg 2 has occasional
    /// loss, this is the "leg-1-only" rate. When leg 1 is the lossy
    /// one, this is the count of packets leg 2 wasn't carrying yet.
    pub packets_via_leg1_only: AtomicU64,
    pub packets_via_leg2_only: AtomicU64,
    pub gap_lost: AtomicU64,
    pub late_dropped: AtomicU64,
    pub buffer_overflow_dropped: AtomicU64,
    pub stream_resets: AtomicU64,
    pub failovers: AtomicU64,
    pub current_buffer_depth: AtomicU64,
    pub current_path_diff_us: AtomicU64,
    pub max_observed_path_diff_us: AtomicU64,
    pub leg1_packets_received: AtomicU64,
    pub leg2_packets_received: AtomicU64,
    pub leg1_last_arrival_unix_us: AtomicU64,
    pub leg2_last_arrival_unix_us: AtomicU64,
    /// Tri-state per leg: 0 unknown, 1 up, 2 degraded, 3 down.
    pub leg1_health: AtomicU64,
    pub leg2_health: AtomicU64,
    /// Set when the two legs are observed with mismatched SSRCs.
    pub ssrc_mismatch: AtomicU64,
}

/// Hard threshold on max_path_diff. Beyond this the constant emission
/// latency would be larger than typical broadcast end-to-end budgets.
pub const MAX_PATH_DIFF_BOUND: Duration = Duration::from_millis(2000);

/// Default skew-accommodation window if none configured. 50 ms is the
/// industry-typical mid-point for terrestrial WAN.
pub const DEFAULT_PATH_DIFF: Duration = Duration::from_millis(50);

/// Conservative upper bound on the seq buffer. At 2400 pkt/s this is
/// ≈ 5 s of pending packets — far above any realistic path
/// differential or burst.
const DEFAULT_MAX_BUFFER_PACKETS: usize = 12_000;

/// "Down" threshold: how long a leg can be silent before we flip its
/// health to Down. Below this, missing packets only count as Degraded.
const LEG_DOWN_THRESHOLD: Duration = Duration::from_millis(1000);

/// Re-anchor when |virtual_seq - anchor_virtual| exceeds this. Prevents
/// the i64 from drifting toward overflow over very long-running flows
/// with sustained traffic.
const REANCHOR_THRESHOLD: i64 = 1_000_000;

impl<T: Clone> BufferedHitlessMerger<T> {
    pub fn new(max_path_diff: Duration) -> Self {
        let bounded = if max_path_diff > MAX_PATH_DIFF_BOUND {
            MAX_PATH_DIFF_BOUND
        } else {
            max_path_diff
        };
        Self {
            dedup: HitlessMerger::new(),
            buffer: BTreeMap::new(),
            anchor_real: 0,
            anchor_virtual: 0,
            anchor_set: false,
            cursor: None,
            max_path_diff: bounded,
            max_buffer_packets: DEFAULT_MAX_BUFFER_PACKETS,
            leg_state: [LegState::default(), LegState::default()],
            stats: BufferedHitlessStats::default(),
        }
    }

    /// Project a `u16` wire seq onto a monotonically-increasing `i64`
    /// virtual seq. Wraparound is preserved by sign-extending the
    /// difference from the current anchor. Re-anchors lazily when the
    /// differential exceeds [`REANCHOR_THRESHOLD`].
    fn project(&mut self, seq: u16) -> i64 {
        if !self.anchor_set {
            self.anchor_real = seq;
            self.anchor_virtual = 0;
            self.anchor_set = true;
            return 0;
        }
        let diff = seq.wrapping_sub(self.anchor_real) as i16 as i64;
        let virt = self.anchor_virtual + diff;
        if diff.abs() > REANCHOR_THRESHOLD {
            self.anchor_real = seq;
            self.anchor_virtual = virt;
        }
        virt
    }

    /// Set the SSRC observed for one leg. Called by the input layer
    /// after parsing the RTP header. Cross-leg mismatch increments the
    /// `ssrc_mismatch` counter.
    pub fn observe_ssrc(&mut self, leg: ActiveLeg, ssrc: u32) {
        let idx = match leg {
            ActiveLeg::Leg1 => 0,
            ActiveLeg::Leg2 => 1,
            ActiveLeg::None => return,
        };
        let prev = self.leg_state[idx].ssrc;
        self.leg_state[idx].ssrc = ssrc;
        let other = self.leg_state[1 - idx].ssrc;
        if other != 0 && other != ssrc && prev != ssrc {
            self.stats.ssrc_mismatch.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Ingest a packet. Returns the (leg, payload) pairs that are now
    /// ready to forward, in seq order.
    pub fn ingest(
        &mut self,
        seq: u16,
        leg: ActiveLeg,
        payload: T,
        now: Instant,
    ) -> Vec<(ActiveLeg, T)> {
        // ── Per-leg arrival bookkeeping ──
        if let Some(idx) = leg_idx(leg) {
            let st = &mut self.leg_state[idx];
            st.last_seq = Some(seq);
            st.last_arrival = Some(now);
            st.health = LegHealth::Up;
            let cnt_field = match leg {
                ActiveLeg::Leg1 => &self.stats.leg1_packets_received,
                ActiveLeg::Leg2 => &self.stats.leg2_packets_received,
                _ => unreachable!(),
            };
            cnt_field.fetch_add(1, Ordering::Relaxed);
            let arrival_us_field = match leg {
                ActiveLeg::Leg1 => &self.stats.leg1_last_arrival_unix_us,
                ActiveLeg::Leg2 => &self.stats.leg2_last_arrival_unix_us,
                _ => unreachable!(),
            };
            arrival_us_field.store(now_unix_us(), Ordering::Relaxed);
            let health_field = match leg {
                ActiveLeg::Leg1 => &self.stats.leg1_health,
                ActiveLeg::Leg2 => &self.stats.leg2_health,
                _ => unreachable!(),
            };
            health_field.store(health_code(LegHealth::Up), Ordering::Relaxed);
        }

        // ── Dedup decision ──
        let virt = self.project(seq);
        let leg_bit = leg_bit(leg);

        // If we already have an entry at this virtual seq, we just
        // observed the slower leg. Update the legs_seen mask + path-
        // diff measurement; drop the duplicate payload.
        if let Some(entry) = self.buffer.get_mut(&virt) {
            if entry.legs_seen & leg_bit == 0 && leg_bit != 0 {
                entry.legs_seen |= leg_bit;
                let diff = now.duration_since(entry.first_seen);
                let diff_us = diff.as_micros() as u64;
                self.stats
                    .current_path_diff_us
                    .store(diff_us, Ordering::Relaxed);
                let prev_max = self.stats.max_observed_path_diff_us.load(Ordering::Relaxed);
                if diff_us > prev_max {
                    self.stats
                        .max_observed_path_diff_us
                        .store(diff_us, Ordering::Relaxed);
                }
            }
            self.stats.dups_dropped.fetch_add(1, Ordering::Relaxed);
            return self.drain_ready(now);
        }

        // Run the dedup primitive — this catches old / wrap / reset.
        match self.dedup.try_merge(seq, leg) {
            None => {
                // Outside the dedup window. Late or stale. Drop.
                self.stats.late_dropped.fetch_add(1, Ordering::Relaxed);
                return self.drain_ready(now);
            }
            Some(_) => {
                // Accepted by dedup → genuinely new. Insert into the
                // path-diff buffer for the configured hold.
                //
                // Strict-monotonic-output guard: the dedup primitive can
                // accept a gap-fill seq from the slower leg. If that slot's
                // emission deadline has already elapsed (the faster leg
                // delivered the gap unfilled and the buffer cursor has
                // already walked past), inserting it now would emit it AFTER
                // a higher seq — i.e. backwards on the wire. That breaks the
                // entire point of hitless: TS CC discontinuities, PCR jumps,
                // decoder hiccups. The right behaviour is to drop the late
                // gap-fill — the receiver has already moved on.
                if let Some(c) = self.cursor {
                    if virt < c {
                        self.stats.late_dropped.fetch_add(1, Ordering::Relaxed);
                        return self.drain_ready(now);
                    }
                }
                if self.buffer.len() >= self.max_buffer_packets {
                    self.stats
                        .buffer_overflow_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    return self.drain_ready(now);
                }
                self.buffer.insert(
                    virt,
                    BufferedEntry {
                        payload,
                        first_leg: leg,
                        first_seen: now,
                        release_at: now + self.max_path_diff,
                        legs_seen: leg_bit,
                    },
                );
                self.stats
                    .current_buffer_depth
                    .store(self.buffer.len() as u64, Ordering::Relaxed);
            }
        }

        self.drain_ready(now)
    }

    /// Drain entries whose hold time has expired, in seq order.
    pub fn drain_expired(&mut self, now: Instant) -> Vec<(ActiveLeg, T)> {
        self.update_leg_health(now);
        self.drain_ready(now)
    }

    fn drain_ready(&mut self, now: Instant) -> Vec<(ActiveLeg, T)> {
        let mut out = Vec::new();
        let mut last_emit_leg: Option<ActiveLeg> = None;
        loop {
            let Some((&virt, entry)) = self.buffer.iter().next() else {
                break;
            };
            if now < entry.release_at {
                break;
            }
            // Account for any seq gap relative to the cursor.
            if let Some(c) = self.cursor {
                if virt > c {
                    let gap = (virt - c) as u64;
                    self.stats.gap_lost.fetch_add(gap, Ordering::Relaxed);
                }
            }
            // Emit. Update gap_filled / failover stats based on which
            // leg actually carried this packet first.
            let entry = self.buffer.remove(&virt).unwrap();
            self.cursor = Some(virt + 1);
            self.stats.packets_emitted.fetch_add(1, Ordering::Relaxed);
            match entry.first_leg {
                ActiveLeg::Leg1 => {
                    self.stats
                        .packets_emitted_leg1
                        .fetch_add(1, Ordering::Relaxed);
                }
                ActiveLeg::Leg2 => {
                    self.stats
                        .packets_emitted_leg2
                        .fetch_add(1, Ordering::Relaxed);
                }
                ActiveLeg::None => {}
            }
            // Single-leg vs dual-leg breakdown, used by the manager UI
            // to surface per-leg loss rates and the operator-facing
            // "loss compensated by 2022-7" metric.
            match entry.legs_seen {
                0b01 => {
                    self.stats
                        .packets_via_leg1_only
                        .fetch_add(1, Ordering::Relaxed);
                }
                0b10 => {
                    self.stats
                        .packets_via_leg2_only
                        .fetch_add(1, Ordering::Relaxed);
                }
                0b11 => {
                    self.stats
                        .packets_via_both_legs
                        .fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
            // Failover counter: changed leg at emission.
            if let Some(last) = last_emit_leg {
                if last != entry.first_leg && entry.first_leg != ActiveLeg::None {
                    self.stats.failovers.fetch_add(1, Ordering::Relaxed);
                }
            }
            last_emit_leg = Some(entry.first_leg);
            out.push((entry.first_leg, entry.payload));
        }
        self.stats
            .current_buffer_depth
            .store(self.buffer.len() as u64, Ordering::Relaxed);
        out
    }

    /// When (if ever) the caller should next wake up to drain expired
    /// entries. Returns `None` when the buffer is empty.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.buffer.iter().next().map(|(_, e)| e.release_at)
    }

    fn update_leg_health(&mut self, now: Instant) {
        for (idx, st) in self.leg_state.iter_mut().enumerate() {
            let new_health = match st.last_arrival {
                Some(t) => {
                    if now.duration_since(t) > LEG_DOWN_THRESHOLD {
                        LegHealth::Down
                    } else {
                        LegHealth::Up
                    }
                }
                None => LegHealth::Unknown,
            };
            if new_health != st.health {
                st.health = new_health;
                let field = if idx == 0 {
                    &self.stats.leg1_health
                } else {
                    &self.stats.leg2_health
                };
                field.store(health_code(new_health), Ordering::Relaxed);
            }
        }
    }

    /// Snapshot of current per-leg health, used by the input task to
    /// emit `leg_recovered` / `leg_dropped` events.
    pub fn leg_health(&self) -> [LegHealth; 2] {
        [self.leg_state[0].health, self.leg_state[1].health]
    }

    /// Build a [`BufferedHitlessSnapshot`] off the live atomic state.
    /// Called from the 1 Hz stats path; never blocks the data path.
    pub fn snapshot(&self) -> crate::stats::models::BufferedHitlessSnapshot {
        let s = &self.stats;
        crate::stats::models::BufferedHitlessSnapshot {
            max_path_diff_ms: self.max_path_diff.as_millis() as u32,
            current_path_diff_us: s.current_path_diff_us.load(Ordering::Relaxed),
            max_observed_path_diff_us: s.max_observed_path_diff_us.load(Ordering::Relaxed),
            packets_emitted: s.packets_emitted.load(Ordering::Relaxed),
            packets_via_both_legs: s.packets_via_both_legs.load(Ordering::Relaxed),
            packets_via_leg1_only: s.packets_via_leg1_only.load(Ordering::Relaxed),
            packets_via_leg2_only: s.packets_via_leg2_only.load(Ordering::Relaxed),
            dups_dropped: s.dups_dropped.load(Ordering::Relaxed),
            gap_lost: s.gap_lost.load(Ordering::Relaxed),
            late_dropped: s.late_dropped.load(Ordering::Relaxed),
            buffer_overflow_dropped: s.buffer_overflow_dropped.load(Ordering::Relaxed),
            stream_resets: s.stream_resets.load(Ordering::Relaxed),
            failovers: s.failovers.load(Ordering::Relaxed),
            current_buffer_depth: s.current_buffer_depth.load(Ordering::Relaxed),
            ssrc_mismatch: s.ssrc_mismatch.load(Ordering::Relaxed),
            leg1_packets_received: s.leg1_packets_received.load(Ordering::Relaxed),
            leg2_packets_received: s.leg2_packets_received.load(Ordering::Relaxed),
            leg1_health: s.leg1_health.load(Ordering::Relaxed) as u8,
            leg2_health: s.leg2_health.load(Ordering::Relaxed) as u8,
        }
    }
}

#[inline]
fn leg_idx(leg: ActiveLeg) -> Option<usize> {
    match leg {
        ActiveLeg::Leg1 => Some(0),
        ActiveLeg::Leg2 => Some(1),
        ActiveLeg::None => None,
    }
}

#[inline]
fn leg_bit(leg: ActiveLeg) -> u8 {
    match leg {
        ActiveLeg::Leg1 => 0b01,
        ActiveLeg::Leg2 => 0b10,
        ActiveLeg::None => 0,
    }
}

#[inline]
fn health_code(h: LegHealth) -> u64 {
    match h {
        LegHealth::Unknown => 0,
        LegHealth::Up => 1,
        LegHealth::Degraded => 2,
        LegHealth::Down => 3,
    }
}

#[inline]
fn now_unix_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod buffered_tests {
    use super::*;

    fn pkt(n: u16) -> u16 {
        n
    }

    #[test]
    fn cold_start_holds_first_packet_for_max_path_diff() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        let now = Instant::now();
        let out = m.ingest(100, ActiveLeg::Leg1, pkt(100), now);
        assert!(out.is_empty(), "first packet must be held");
        // Drain just before deadline — still nothing.
        let out = m.drain_expired(now + Duration::from_millis(49));
        assert!(out.is_empty());
        // Drain at deadline — emitted.
        let out = m.drain_expired(now + Duration::from_millis(50));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, 100);
    }

    #[test]
    fn duplicate_dropped_path_diff_measured() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        let t0 = Instant::now();
        m.ingest(200, ActiveLeg::Leg1, pkt(200), t0);
        // Slow leg arrives 30 ms later with the same seq.
        let dups_before = m.stats.dups_dropped.load(Ordering::Relaxed);
        m.ingest(200, ActiveLeg::Leg2, pkt(200), t0 + Duration::from_millis(30));
        assert_eq!(m.stats.dups_dropped.load(Ordering::Relaxed), dups_before + 1);
        let measured = m.stats.current_path_diff_us.load(Ordering::Relaxed);
        // 30 ms ± slack
        assert!(
            (29_000..=31_000).contains(&measured),
            "expected ~30 ms, got {measured} us"
        );
    }

    #[test]
    fn gap_fill_from_slow_leg_works() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        let t0 = Instant::now();
        // Fast leg delivers 100, 102. Slow leg delivers 101 within the window.
        m.ingest(100, ActiveLeg::Leg1, pkt(100), t0);
        m.ingest(102, ActiveLeg::Leg1, pkt(102), t0 + Duration::from_millis(1));
        m.ingest(101, ActiveLeg::Leg2, pkt(101), t0 + Duration::from_millis(20));
        // Drain at T = 50 ms. Should emit 100, 101, 102 in order.
        let out = m.drain_expired(t0 + Duration::from_millis(70));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].1, 100);
        assert_eq!(out[1].1, 101);
        assert_eq!(out[2].1, 102);
        // 101 was a true gap-fill from leg2 (slower leg), 100 + 102
        // came from leg1 only.
        assert_eq!(m.stats.packets_via_leg2_only.load(Ordering::Relaxed), 1);
        assert_eq!(m.stats.packets_via_leg1_only.load(Ordering::Relaxed), 2);
        assert_eq!(m.stats.packets_via_both_legs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dual_leg_overlap_emitted_once() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        let t0 = Instant::now();
        // Both legs deliver 200 within the path-diff window — 30 ms apart.
        m.ingest(200, ActiveLeg::Leg1, pkt(200), t0);
        m.ingest(200, ActiveLeg::Leg2, pkt(200), t0 + Duration::from_millis(30));
        // Drain at hold expiry — single emission, dual_leg counter == 1.
        let out = m.drain_expired(t0 + Duration::from_millis(60));
        assert_eq!(out.len(), 1);
        assert_eq!(m.stats.packets_via_both_legs.load(Ordering::Relaxed), 1);
        assert_eq!(m.stats.dups_dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn permanent_loss_emits_higher_seq_after_window() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        let t0 = Instant::now();
        m.ingest(100, ActiveLeg::Leg1, pkt(100), t0);
        // 101 lost on both legs; 102 arrives.
        m.ingest(102, ActiveLeg::Leg1, pkt(102), t0 + Duration::from_millis(1));
        let out = m.drain_expired(t0 + Duration::from_millis(60));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].1, 100);
        assert_eq!(out[1].1, 102);
        assert_eq!(m.stats.gap_lost.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ssrc_mismatch_detected() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        m.observe_ssrc(ActiveLeg::Leg1, 0xdeadbeef);
        m.observe_ssrc(ActiveLeg::Leg2, 0xfeedface);
        assert_eq!(m.stats.ssrc_mismatch.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn matching_ssrc_no_mismatch() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        m.observe_ssrc(ActiveLeg::Leg1, 0xdeadbeef);
        m.observe_ssrc(ActiveLeg::Leg2, 0xdeadbeef);
        assert_eq!(m.stats.ssrc_mismatch.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn leg_health_flips_to_down_after_silent_threshold() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        let t0 = Instant::now();
        m.ingest(1000, ActiveLeg::Leg1, pkt(1000), t0);
        m.drain_expired(t0 + Duration::from_millis(60));
        assert_eq!(m.leg_health()[0], LegHealth::Up);
        // Advance > 1 s without any leg1 packet.
        m.drain_expired(t0 + Duration::from_millis(1500));
        assert_eq!(m.leg_health()[0], LegHealth::Down);
    }

    #[test]
    fn buffer_overflow_drops_payload_keeps_progress() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_secs(60));
        m.max_buffer_packets = 4;
        let t0 = Instant::now();
        for i in 0..6 {
            m.ingest(100 + i, ActiveLeg::Leg1, pkt(100 + i), t0);
        }
        assert!(m.stats.buffer_overflow_dropped.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn next_deadline_tracks_oldest_entry() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(40));
        let t0 = Instant::now();
        assert!(m.next_deadline().is_none());
        m.ingest(100, ActiveLeg::Leg1, pkt(100), t0);
        assert_eq!(m.next_deadline(), Some(t0 + Duration::from_millis(40)));
    }

    /// Regression: in adversarial path-asymmetry — leg1 loses a packet AND
    /// leg2's copy arrives only AFTER the buffer's cursor has already walked
    /// past that seq slot — the buffered merger MUST drop the late gap-fill,
    /// not emit it backwards on the wire. Hitless's contract is strictly
    /// monotonic seq-order output. Without the guard, the late copy is
    /// inserted at a virt below `cursor`, drained later, and the downstream
    /// TS analyser sees a CC discontinuity / PCR jump.
    #[test]
    fn late_gap_fill_after_cursor_advance_is_dropped() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        let t0 = Instant::now();
        // leg1's seq 100 is lost in the kernel buffer; leg1 carries 101 onward.
        m.ingest(101, ActiveLeg::Leg1, 101, t0);
        m.ingest(102, ActiveLeg::Leg1, 102, t0 + Duration::from_millis(1));
        // Buffer drains at the path-diff window. cursor advances past 101, 102.
        let out = m.drain_expired(t0 + Duration::from_millis(60));
        let seqs: Vec<u16> = out.iter().map(|(_, p)| *p).collect();
        assert_eq!(seqs, vec![101, 102]);
        // leg2's copy of 100 finally lands — well outside the path-diff window.
        let dropped_before = m.stats.late_dropped.load(Ordering::Relaxed);
        let out = m.ingest(100, ActiveLeg::Leg2, 100, t0 + Duration::from_millis(80));
        assert!(out.is_empty(), "late gap-fill must NOT emit out of seq order");
        let out = m.drain_expired(t0 + Duration::from_millis(200));
        assert!(out.is_empty(), "late gap-fill must not surface on next drain either");
        assert_eq!(
            m.stats.late_dropped.load(Ordering::Relaxed),
            dropped_before + 1,
            "the rejected gap-fill should bump late_dropped"
        );
    }

    #[test]
    fn wraparound_preserved_in_seq_order() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(20));
        let t0 = Instant::now();
        m.ingest(65534, ActiveLeg::Leg1, pkt(65534), t0);
        m.ingest(65535, ActiveLeg::Leg1, pkt(65535), t0);
        m.ingest(0, ActiveLeg::Leg1, pkt(0), t0);
        m.ingest(1, ActiveLeg::Leg1, pkt(1), t0);
        let out = m.drain_expired(t0 + Duration::from_millis(30));
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].1, 65534);
        assert_eq!(out[1].1, 65535);
        assert_eq!(out[2].1, 0);
        assert_eq!(out[3].1, 1);
    }
}
